use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context as _;
use futures_lite::StreamExt;
use meshcast_signal::{BotLinkStore, Event, LinkState, PairCode, PairSignal, Signal, SignalNode, derive_pairing_topic};
use poise::serenity_prelude as serenity;
use serenity::all::{
    ChannelId, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, MessageId, UserId,
};
use tokio::sync::broadcast;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

struct Data {
    active_streams: std::sync::Arc<Mutex<HashMap<ChannelId, (MessageId, String, UserId, String)>>>,
    viewers: Mutex<HashMap<ChannelId, std::collections::HashSet<UserId>>>,
    signal_node: SignalNode,
    links: std::sync::Arc<Mutex<HashMap<UserId, iroh_gossip::api::GossipSender>>>,
    pending_pins: std::sync::Arc<Mutex<HashMap<String, (iroh_gossip::proto::TopicId, UserId, std::time::Instant)>>>,
    stream_configs: Mutex<HashMap<UserId, (String, u32)>>,
    signal_tx: broadcast::Sender<(UserId, Signal)>,
    store_path: std::path::PathBuf,
    store: std::sync::Arc<Mutex<BotLinkStore>>,
}

fn store_path() -> std::path::PathBuf {
    dirs_next::home_dir()
        .unwrap_or_default()
        .join(".config/meshcast-bot/state.json")
}

/// Spawn a gossip receiver task that routes signals to the broadcast channel.
fn spawn_receiver(
    user_id: UserId,
    mut receiver: iroh_gossip::api::GossipReceiver,
    signal_tx: broadcast::Sender<(UserId, Signal)>,
) {
    tokio::spawn(async move {
        while let Some(event) = receiver.next().await {
            match event {
                Ok(Event::Received(msg)) => {
                    if let Ok(signal) = Signal::decode(&msg.content) {
                        tracing::info!(?signal, user = %user_id, "Signal from app");
                        let _ = signal_tx.send((user_id, signal));
                    }
                }
                Ok(Event::NeighborUp(id)) => {
                    tracing::info!(peer = %id.fmt_short(), user = %user_id, "App connected");
                }
                Ok(Event::NeighborDown(id)) => {
                    tracing::warn!(peer = %id.fmt_short(), user = %user_id, "App disconnected");
                }
                Err(e) => {
                    tracing::error!(user = %user_id, "Gossip error: {e}");
                    break;
                }
                _ => {}
            }
        }
    });
}

/// Link your desktop app to this bot for remote stream control.
#[poise::command(slash_command, ephemeral)]
async fn link(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let data = ctx.data();
    let server_name = ctx.guild().map(|g| g.name.clone()).unwrap_or_else(|| "Unknown Server".into());

    let real_topic = iroh_gossip::proto::TopicId::from_bytes(rand::random());
    let pin = PairCode::generate_pin();
    let full_code = PairCode::encode_full(data.signal_node.endpoint.id(), &pin);

    // Store PIN → real topic mapping (expire old PINs first)
    {
        let mut pins = data.pending_pins.lock().expect("poisoned");
        pins.retain(|_, (_, _, created)| created.elapsed() < Duration::from_secs(600));
        pins.insert(pin.clone(), (real_topic, user_id, std::time::Instant::now()));
    }

    // Subscribe to the pairing topic (derived from PIN) to handle PairRequest
    let pairing_topic = derive_pairing_topic(&pin);
    let gossip = data.signal_node.gossip.clone();
    let pending_pins = data.pending_pins.clone();
    let links = data.links.clone();
    let signal_tx = data.signal_tx.clone();
    let store = data.store.clone();
    let store_path = data.store_path.clone();
    let endpoint_id = data.signal_node.endpoint.id();

    // Spawn pairing listener on the derived topic
    tokio::spawn(async move {
        let pairing_sub = match gossip.subscribe(pairing_topic, vec![]).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to subscribe to pairing topic: {e}");
                return;
            }
        };
        let (pair_sender, mut pair_receiver) = pairing_sub.split();

        // Wait for PairRequest or timeout
        let result = tokio::time::timeout(Duration::from_secs(600), async {
            while let Some(event) = pair_receiver.next().await {
                if let Ok(Event::Received(msg)) = event {
                    if let Ok(PairSignal::PairRequest { pin: received_pin }) = PairSignal::decode(&msg.content) {
                        // Validate PIN
                        let valid = {
                            let mut pins = pending_pins.lock().expect("poisoned");
                            if let Some((topic, uid, created)) = pins.get(&received_pin) {
                                if created.elapsed() < Duration::from_secs(600) {
                                    let t = *topic;
                                    let u = *uid;
                                    pins.remove(&received_pin); // one-time use
                                    Some((t, u))
                                } else {
                                    pins.remove(&received_pin);
                                    None
                                }
                            } else {
                                None
                            }
                        };

                        match valid {
                            Some((topic, uid)) => {
                                tracing::info!(user = %uid, "PIN accepted, establishing link");

                                // Send PairAccepted with the real topic
                                let accept = PairSignal::PairAccepted { topic: *topic.as_bytes(), server_name: server_name.clone() };
                                let _ = pair_sender.broadcast_neighbors(accept.encode().unwrap()).await;

                                // Now subscribe to the real gossip topic
                                if let Ok(real_sub) = gossip.subscribe(topic, vec![]).await {
                                    let (sender, receiver) = real_sub.split();
                                    links.lock().expect("poisoned").insert(uid, sender);
                                    spawn_receiver(uid, receiver, signal_tx.clone());

                                    // Persist the link
                                    let link_state = LinkState::new(
                                        topic,
                                        &iroh::SecretKey::from_bytes(&[0u8; 32]),
                                        endpoint_id,
                                    );
                                    {
                                        let mut s = store.lock().expect("poisoned");
                                        s.links.insert(uid.to_string(), link_state);
                                    }
                                    let snapshot = store.lock().expect("poisoned").clone();
                                    let _ = snapshot.save(&store_path).await;
                                    tracing::info!(user = %uid, "Link established via PairCode");
                                }
                                return; // done
                            }
                            None => {
                                tracing::warn!("Invalid or expired PIN");
                                let reject = PairSignal::PairRejected { reason: "Invalid or expired code".into() };
                                let _ = pair_sender.broadcast_neighbors(reject.encode().unwrap()).await;
                            }
                        }
                    }
                }
            }
        }).await;

        if result.is_err() {
            tracing::debug!("Pairing topic timed out");
        }
    });

    ctx.say(format!(
        "**Your pairing code:**\n\
         ```\n{full_code}\n```\n\
         Paste this into the Meshcast app to connect."
    ))
    .await?;
    Ok(())
}

/// Start a stream — shows config card first, then signals your app.
#[poise::command(slash_command)]
async fn stream(
    ctx: Context<'_>,
    #[description = "Stream title"] title: Option<String>,
) -> Result<(), Error> {
    let user = ctx.author();
    let user_id = user.id;
    let display_name = user.global_name.as_deref().unwrap_or(&user.name);
    let title = title.unwrap_or_else(|| format!("{display_name}'s Stream"));

    let has_link = ctx.data().links.lock().expect("poisoned").contains_key(&user_id);
    if !has_link {
        ctx.say("Connect your app first with `/link`.").await?;
        return Ok(());
    }

    // Show ephemeral config card with quality/fps selectors + Start/Cancel
    let config_embed = CreateEmbed::new()
        .title("Stream Setup")
        .description(format!("**{}**\nChoose your settings and click Start.", title))
        .color(0x5865F2);

    let quality_select = serenity::all::CreateSelectMenu::new(
        "stream-quality",
        serenity::all::CreateSelectMenuKind::String {
            options: vec![
                serenity::all::CreateSelectMenuOption::new("360p", "360p"),
                serenity::all::CreateSelectMenuOption::new("720p", "720p").default_selection(true),
                serenity::all::CreateSelectMenuOption::new("1080p", "1080p"),
            ],
        },
    )
    .placeholder("Resolution");
    let quality_row = CreateActionRow::SelectMenu(quality_select);

    let fps_select = serenity::all::CreateSelectMenu::new(
        "stream-fps",
        serenity::all::CreateSelectMenuKind::String {
            options: vec![
                serenity::all::CreateSelectMenuOption::new("30 FPS", "30").default_selection(true),
                serenity::all::CreateSelectMenuOption::new("60 FPS", "60"),
            ],
        },
    )
    .placeholder("FPS");
    let fps_row = CreateActionRow::SelectMenu(fps_select);

    let start_btn = CreateButton::new(format!("stream-start:{}", title))
        .label("Start Stream")
        .style(serenity::all::ButtonStyle::Success);
    let cancel_btn = CreateButton::new("stream-cancel")
        .label("Cancel")
        .style(serenity::all::ButtonStyle::Secondary);
    let btn_row = CreateActionRow::Buttons(vec![start_btn, cancel_btn]);

    let reply = poise::CreateReply::default()
        .embed(config_embed)
        .components(vec![quality_row, fps_row, btn_row])
        .ephemeral(true);

    ctx.send(reply).await?;
    Ok(())
}


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "meshcast_bot=info,iroh=warn".into()),
        )
        .init();

    let token =
        std::env::var("DISCORD_TOKEN").context("DISCORD_TOKEN environment variable required")?;

    // Load persisted state
    let path = store_path();
    let mut store = BotLinkStore::load(&path)
        .await
        .context("Failed to load link store")?;

    // Use persisted secret key or generate + save a new one
    let secret_key = match store.bot_secret_key() {
        Some(key) => {
            tracing::info!("Loaded persisted bot identity");
            key
        }
        None => {
            let key = iroh::SecretKey::from_bytes(&rand::random::<[u8; 32]>());
            store.bot_secret_key = Some(key.to_bytes());
            store.save(&path).await?;
            tracing::info!("Generated new bot identity");
            key
        }
    };

    tracing::info!("Starting signal node...");
    let signal_node = SignalNode::new(Some(secret_key))
        .await
        .context("Failed to start signal node")?;
    tracing::info!("Signal node ready");

    let (signal_tx, _) = broadcast::channel(64);
    let mut links: HashMap<UserId, iroh_gossip::api::GossipSender> = HashMap::new();

    // Restore saved links
    for (user_id_str, link_state) in &store.links {
        let user_id: UserId = match user_id_str.parse() {
            Ok(id) => UserId::new(id),
            Err(_) => {
                tracing::warn!("Invalid user ID in store: {user_id_str}");
                continue;
            }
        };

        let topic = link_state.topic_id();
        match signal_node.gossip.subscribe(topic, vec![]).await {
            Ok(gossip_topic) => {
                let (sender, receiver) = gossip_topic.split();
                links.insert(user_id, sender);
                spawn_receiver(user_id, receiver, signal_tx.clone());
                tracing::info!(user = %user_id, "Restored link");
            }
            Err(e) => {
                tracing::warn!(user = %user_id, "Failed to restore link: {e}");
            }
        }
    }

    let restored_count = links.len();
    tracing::info!("Restored {restored_count} link(s)");

    // Subscribe to signals for background stream-end handling
    let mut stream_end_rx = signal_tx.subscribe();

    let framework = poise::Framework::builder()
        .setup(move |ctx, _ready, framework| {
            let http = ctx.http.clone();

            // Spawn background task to update Discord embeds when streams end
            let active_streams = std::sync::Arc::new(Mutex::new(HashMap::<ChannelId, (MessageId, String, UserId, String)>::new()));
            let active_streams_bg = active_streams.clone();
            let http_bg = http.clone();
            tokio::spawn(async move {
                while let Ok((user_id, signal)) = stream_end_rx.recv().await {
                    if let Signal::StreamStopped = signal {
                        tracing::info!(user = %user_id, "Stream stopped — updating embed");
                        // Find and update the embed for this user's stream
                        let entry: Option<(ChannelId, MessageId, String)> = {
                            let mut streams = active_streams_bg.lock().expect("poisoned");
                            let mut found = None;
                            streams.retain(|channel_id, (msg_id, _, streamer, title)| {
                                if *streamer == user_id {
                                    found = Some((*channel_id, *msg_id, title.clone()));
                                    false // remove
                                } else {
                                    true
                                }
                            });
                            found
                        };
                        if let Some((channel_id, message_id, title)) = entry {
                            let ended_embed = CreateEmbed::new()
                                .title(title)
                                .description("This stream has ended.")
                                .color(0x99AAB5)
                                .footer(serenity::all::CreateEmbedFooter::new("Stream ended"))
                                .timestamp(serenity::model::Timestamp::now());
                            let edit = serenity::all::EditMessage::new()
                                .embed(ended_embed)
                                .components(vec![]);
                            if let Err(e) = http_bg.get_message(channel_id, message_id).await
                                .and_then(|_| Ok(()))
                            {
                                tracing::warn!("Failed to fetch message for embed update: {e}");
                            } else {
                                let _ = channel_id.edit_message(&http_bg, message_id, edit).await;
                            }
                        }
                    }
                }
            });

            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                tracing::info!("Bot is ready");
                Ok(Data {
                    active_streams,
                    viewers: Mutex::new(HashMap::new()),
                    signal_node,
                    signal_tx,
                    links: std::sync::Arc::new(Mutex::new(links)),
                    pending_pins: std::sync::Arc::new(Mutex::new(HashMap::new())),
                    stream_configs: Mutex::new(HashMap::new()),
                    store_path: path,
                    store: std::sync::Arc::new(Mutex::new(store)),
                })
            })
        })
        .options(poise::FrameworkOptions {
            commands: vec![stream(), link()],
            event_handler: |ctx, event, _framework_ctx, data| {
                Box::pin(handle_event(ctx, event, data))
            },
            ..Default::default()
        })
        .build();

    let intents = serenity::GatewayIntents::non_privileged();
    let mut client = serenity::ClientBuilder::new(&token, intents)
        .framework(framework)
        .await
        .context("Failed to create Discord client")?;

    client.start().await.context("Client error")?;
    Ok(())
}



/// Handle component interactions (Watch button clicks)
async fn handle_event(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    data: &Data,
) -> Result<(), Error> {
    if let serenity::FullEvent::InteractionCreate { interaction } = event {
        if let Some(component) = interaction.as_message_component() {
            let id = component.data.custom_id.as_str();
            let user_id = component.user.id;

            // Handle quality/fps select menus
            if id == "stream-quality" || id == "stream-fps" {
                if let serenity::all::ComponentInteractionDataKind::StringSelect { values } = &component.data.kind {
                    if let Some(value) = values.first() {
                        let mut configs = data.stream_configs.lock().expect("poisoned");
                        let entry = configs.entry(user_id).or_insert(("720p".into(), 30));
                        if id == "stream-quality" {
                            entry.0 = value.clone();
                        } else {
                            entry.1 = value.parse().unwrap_or(30);
                        }
                    }
                }
                // Acknowledge without changing the message
                component
                    .create_response(ctx, serenity::all::CreateInteractionResponse::Acknowledge)
                    .await?;
                return Ok(());
            }

            // Handle stream cancel
            if id == "stream-cancel" {
                data.stream_configs.lock().expect("poisoned").remove(&user_id);
                component
                    .create_response(
                        ctx,
                        serenity::all::CreateInteractionResponse::UpdateMessage(
                            serenity::all::CreateInteractionResponseMessage::new()
                                .content("Stream cancelled.")
                                .embeds(vec![])
                                .components(vec![]),
                        ),
                    )
                    .await?;
                return Ok(());
            }

            // Handle stream start
            if id.starts_with("stream-start:") {
                let title = id.strip_prefix("stream-start:").unwrap_or("Stream").to_string();
                let (quality, fps) = data
                    .stream_configs
                    .lock()
                    .expect("poisoned")
                    .remove(&user_id)
                    .unwrap_or(("720p".into(), 30));

                let sender = data.links.lock().expect("poisoned").get(&user_id).cloned();
                let sender = match sender {
                    Some(s) => s,
                    None => {
                        component
                            .create_response(
                                ctx,
                                serenity::all::CreateInteractionResponse::UpdateMessage(
                                    serenity::all::CreateInteractionResponseMessage::new()
                                        .content("Link lost. Run `/link` again.")
                                        .embeds(vec![])
                                        .components(vec![]),
                                ),
                            )
                            .await?;
                        return Ok(());
                    }
                };

                // Update the ephemeral message to show "Starting..."
                component
                    .create_response(
                        ctx,
                        serenity::all::CreateInteractionResponse::UpdateMessage(
                            serenity::all::CreateInteractionResponseMessage::new()
                                .content(format!("Starting stream at {quality} {fps}fps..."))
                                .embeds(vec![])
                                .components(vec![]),
                        ),
                    )
                    .await?;

                // Signal the app
                let guild_name = if let Some(gid) = component.guild_id {
                    gid.to_partial_guild(ctx).await.ok().map(|g| g.name).unwrap_or_default()
                } else {
                    String::new()
                };
                let signal = Signal::StartStream {
                    title: title.clone(),
                    quality: quality.clone(),
                    fps,
                    server: guild_name,
                };
                sender
                    .broadcast_neighbors(signal.encode()?)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;

                // Wait for StreamReady
                let mut rx = data.signal_tx.subscribe();
                let ticket: Option<String> =
                    tokio::time::timeout(Duration::from_secs(30), async move {
                        loop {
                            match rx.recv().await {
                                Ok((uid, Signal::StreamReady { ticket })) if uid == user_id => {
                                    return ticket;
                                }
                                Err(_) => return String::new(),
                                _ => continue,
                            }
                        }
                    })
                    .await
                    .ok()
                    .filter(|t| !t.is_empty());

                match ticket {
                    Some(ticket) => {
                        let display_name = component.user.global_name.as_deref().unwrap_or(&component.user.name);
                        let avatar_url = component.user.avatar_url().unwrap_or_default();

                        let embed = CreateEmbed::new()
                            .title(&title)
                            .description(format!(
                                "Streaming at **{quality} {fps}fps**\nClick **Watch** to view."
                            ))
                            .color(0x5865F2)
                            .author(CreateEmbedAuthor::new(display_name).icon_url(&avatar_url))
                            .footer(serenity::all::CreateEmbedFooter::new("Started streaming"))
                            .timestamp(serenity::model::Timestamp::now());

                        let watch_btn = CreateButton::new("watch")
                            .label("Watch")
                            .style(serenity::all::ButtonStyle::Primary);
                        let download_btn = CreateButton::new_link(
                            "https://github.com/mattcree/meshcast/releases/latest",
                        )
                        .label("Download App");
                        let row = CreateActionRow::Buttons(vec![watch_btn, download_btn]);

                        // Post the public stream embed
                        let channel_id = component.channel_id;
                        let msg = channel_id
                            .send_message(
                                ctx,
                                serenity::all::CreateMessage::new()
                                    .embed(embed)
                                    .components(vec![row]),
                            )
                            .await?;

                        data.active_streams
                            .lock()
                            .expect("poisoned")
                            .insert(channel_id, (msg.id, ticket, user_id, title));
                        data.viewers
                            .lock()
                            .expect("poisoned")
                            .insert(channel_id, std::collections::HashSet::new());
                    }
                    None => {
                        // Follow up since we already responded
                        component
                            .create_followup(
                                ctx,
                                serenity::all::CreateInteractionResponseFollowup::new()
                                    .content("Your Meshcast app doesn't seem to be running. Open it and try again.")
                                    .ephemeral(true),
                            )
                            .await?;
                    }
                }
                return Ok(());
            }

            if id == "watch" {
                tracing::info!(user = %component.user.name, "Watch button clicked");

                let user_id = component.user.id;
                let channel_id = component.channel_id;

                let stream_info = data
                    .active_streams
                    .lock()
                    .expect("poisoned")
                    .get(&channel_id)
                    .map(|(_, t, streamer, _)| (t.clone(), *streamer));

                let reply = match stream_info {
                    None => "No active stream in this channel.".to_string(),
                    Some((ticket, streamer_id)) => {
                        let sender = data.links.lock().expect("poisoned").get(&user_id).cloned();
                        match sender {
                            Some(sender) => {
                                let signal = Signal::WatchStream { ticket };
                                let msg = signal.encode()?;
                                let mut sent = false;
                                for attempt in 0..3 {
                                    match sender.broadcast_neighbors(msg.clone()).await {
                                        Ok(_) => {
                                            tracing::info!(
                                                "Sent WatchStream (attempt {})",
                                                attempt + 1
                                            );
                                            sent = true;
                                            break;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "Send attempt {} failed: {e}",
                                                attempt + 1
                                            );
                                            tokio::time::sleep(Duration::from_millis(200)).await;
                                        }
                                    }
                                }
                                if sent {
                                    // Track viewer and notify streamer
                                    let count = {
                                        let mut viewers = data.viewers.lock().expect("poisoned");
                                        let set = viewers.entry(channel_id).or_default();
                                        set.insert(user_id);
                                        set.len() as u32
                                    };
                                    let streamer_sender = data.links.lock().expect("poisoned").get(&streamer_id).cloned();
                                    if let Some(ss) = streamer_sender {
                                        let update = Signal::ViewerUpdate { count };
                                        let _ = ss.broadcast_neighbors(update.encode()?).await;
                                    }
                                    format!("Opening stream... ({count} viewer{})", if count == 1 { "" } else { "s" })
                                } else {
                                    "Couldn't reach your app. Make sure Meshcast is open and try again.".to_string()
                                }
                            }
                            None => {
                                "You haven't set up Meshcast yet. Click **Download App** to get started, then use `/link` to connect.".to_string()
                            }
                        }
                    }
                };

                component
                    .create_response(
                        ctx,
                        serenity::all::CreateInteractionResponse::Message(
                            serenity::all::CreateInteractionResponseMessage::new()
                                .content(reply)
                                .ephemeral(true),
                        ),
                    )
                    .await?;
            }
        }
    }
    Ok(())
}
