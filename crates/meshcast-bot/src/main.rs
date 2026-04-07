use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context as _;
use futures_lite::StreamExt;
use meshcast_signal::{BotLinkStore, Event, LinkState, PairCode, PairToken, Signal, SignalNode};
use poise::serenity_prelude as serenity;
use serenity::all::{
    ChannelId, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, MessageId, UserId,
};
use tokio::sync::broadcast;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

struct Data {
    active_streams: Mutex<HashMap<ChannelId, (MessageId, String, UserId)>>,
    viewers: Mutex<HashMap<ChannelId, std::collections::HashSet<UserId>>>,
    signal_node: SignalNode,
    links: Mutex<HashMap<UserId, iroh_gossip::api::GossipSender>>,
    /// Pending pairing PINs: PIN → (topic, user_id, created_at)
    pending_pins: Mutex<HashMap<String, (iroh_gossip::proto::TopicId, UserId, std::time::Instant)>>,
    signal_tx: broadcast::Sender<(UserId, Signal)>,
    store_path: std::path::PathBuf,
    store: Mutex<BotLinkStore>,
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

    let topic = iroh_gossip::proto::TopicId::from_bytes(rand::random());
    let addr = data.signal_node.addr();
    let pin = PairCode::generate_pin();
    let full_code = PairCode::encode_full(data.signal_node.endpoint.id(), &pin);
    // Keep legacy token as fallback
    let _legacy_token = PairToken::new(topic, vec![addr]).to_string()?;

    let gossip_topic = data
        .signal_node
        .gossip
        .subscribe(topic, vec![])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (sender, receiver) = gossip_topic.split();

    data.links.lock().expect("poisoned").insert(user_id, sender);
    spawn_receiver(user_id, receiver, data.signal_tx.clone());

    // Store PIN for pairing exchange (expire old PINs first)
    {
        let mut pins = data.pending_pins.lock().expect("poisoned");
        pins.retain(|_, (_, _, created)| created.elapsed() < Duration::from_secs(600));
        pins.insert(pin.clone(), (topic, user_id, std::time::Instant::now()));
    }

    // Persist the link (don't store bot's own secret key — it's in BotLinkStore)
    let link_state = LinkState::new(
        topic,
        &iroh::SecretKey::from_bytes(&[0u8; 32]), // placeholder — bot key is in BotLinkStore
        data.signal_node.endpoint.id(),
    );
    {
        let mut store = data.store.lock().expect("poisoned");
        store.links.insert(user_id.to_string(), link_state);
    }
    let store_snapshot = data.store.lock().expect("poisoned").clone();
    if let Err(e) = store_snapshot.save(&data.store_path).await {
        tracing::error!("Failed to save link store: {e}");
    }

    ctx.say(format!(
        "**Your pairing code:**\n\
         ```\n{full_code}\n```\n\
         Enter this in the Meshcast app to connect.\n\n\
         *Already paired before?* Just use the PIN: **{pin}**"
    ))
    .await?;
    Ok(())
}

/// Start or stop a stream. If linked, signals your desktop app automatically.
#[poise::command(slash_command)]
async fn stream(
    ctx: Context<'_>,
    #[description = "Stream title"] title: Option<String>,
    #[description = "iroh-live ticket (not needed if linked)"] ticket: Option<String>,
    #[description = "Stop the active stream"] stop: Option<bool>,
) -> Result<(), Error> {
    if stop.unwrap_or(false) {
        return handle_stop(ctx).await;
    }

    let user = ctx.author();
    let user_id = user.id;
    let display_name = user.global_name.as_deref().unwrap_or(&user.name);
    let title = title.unwrap_or_else(|| format!("{display_name}'s Stream"));

    let has_link = ctx.data().links.lock().expect("poisoned").contains_key(&user_id);

    let ticket = if let Some(t) = ticket {
        t
    } else if has_link {
        let sender = ctx.data().links.lock().expect("poisoned").get(&user_id).cloned();
        let sender = match sender {
            Some(s) => s,
            None => {
                ctx.say("Link lost. Run `/link` again.").await?;
                return Ok(());
            }
        };

        ctx.defer().await?;

        let signal = Signal::StartStream { title: title.clone() };
        sender
            .broadcast_neighbors(signal.encode()?)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut rx = ctx.data().signal_tx.subscribe();
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
            Some(t) => t,
            None => {
                ctx.say("Your Meshcast app doesn't seem to be running. Open it and try again.")
                    .await?;
                return Ok(());
            }
        }
    } else {
        ctx.say("Connect your app first with `/link`, or pass a stream code: `/stream ticket:<code>`")
            .await?;
        return Ok(());
    };

    let avatar_url = user.avatar_url().unwrap_or_default();

    let embed = CreateEmbed::new()
        .title(&title)
        .description(
            "Click **Watch** to view this stream.\n\
             Need the app? Click your platform to download."
        )
        .color(0x5865F2)
        .author(CreateEmbedAuthor::new(display_name).icon_url(&avatar_url))
        .footer(serenity::all::CreateEmbedFooter::new("Started streaming"))
        .timestamp(serenity::model::Timestamp::now());

    let watch_btn =
        CreateButton::new("watch").label("Watch").style(serenity::all::ButtonStyle::Primary);
    let download_btn = CreateButton::new_link(
        "https://github.com/mattcree/meshcast/releases/latest",
    )
    .label("Download App");

    let row = CreateActionRow::Buttons(vec![watch_btn, download_btn]);

    let reply = poise::CreateReply::default()
        .embed(embed)
        .components(vec![row]);

    let handle = ctx.send(reply).await?;
    let message = handle.message().await?;

    ctx.data()
        .active_streams
        .lock()
        .expect("poisoned")
        .insert(ctx.channel_id(), (message.id, ticket, user_id));
    ctx.data()
        .viewers
        .lock()
        .expect("poisoned")
        .insert(ctx.channel_id(), std::collections::HashSet::new());

    Ok(())
}

async fn handle_stop(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let channel_id = ctx.channel_id();

    let sender = ctx.data().links.lock().expect("poisoned").get(&user_id).cloned();
    if let Some(sender) = sender {
        let _ = sender
            .broadcast_neighbors(Signal::StopStream.encode()?)
            .await;
    }

    let entry = ctx
        .data()
        .active_streams
        .lock()
        .expect("poisoned")
        .remove(&channel_id);
    ctx.data().viewers.lock().expect("poisoned").remove(&channel_id);

    let (message_id, _, _) = match entry {
        Some(e) => e,
        None => {
            ctx.say("No active stream in this channel.").await?;
            return Ok(());
        }
    };

    let mut message = ctx
        .http()
        .get_message(channel_id, message_id)
        .await
        .context("Failed to fetch stream message")?;

    let display_name = ctx.author().global_name.as_deref().unwrap_or(&ctx.author().name);
    let avatar_url = ctx.author().avatar_url().unwrap_or_default();

    let ended_embed = CreateEmbed::new()
        .title(format!("{display_name}'s Stream"))
        .description("This stream has ended.")
        .color(0x99AAB5)
        .author(CreateEmbedAuthor::new(display_name).icon_url(&avatar_url))
        .footer(serenity::all::CreateEmbedFooter::new("Stream ended"))
        .timestamp(serenity::model::Timestamp::now());

    message
        .edit(
            ctx.http(),
            serenity::all::EditMessage::new()
                .embed(ended_embed)
                .components(vec![]),
        )
        .await
        .context("Failed to edit stream message")?;

    ctx.say("Stream stopped.").await?;
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

    let framework = poise::Framework::builder()
        .setup(move |ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                tracing::info!("Bot is ready");
                Ok(Data {
                    active_streams: Mutex::new(HashMap::new()),
                    viewers: Mutex::new(HashMap::new()),
                    signal_node,
                    signal_tx,
                    links: Mutex::new(links),
                    pending_pins: Mutex::new(HashMap::new()),
                    store_path: path,
                    store: Mutex::new(store),
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
            if component.data.custom_id == "watch" {
                tracing::info!(user = %component.user.name, "Watch button clicked");

                let user_id = component.user.id;
                let channel_id = component.channel_id;

                let stream_info = data
                    .active_streams
                    .lock()
                    .expect("poisoned")
                    .get(&channel_id)
                    .map(|(_, t, streamer)| (t.clone(), *streamer));

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
