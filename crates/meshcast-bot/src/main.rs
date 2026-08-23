//! `meshcast-bot` — the Discord side of Meshcast.
//!
//! Slash commands: `/link`, `/unlink`, `/stream`. Buttons: Watch, Stop.
//! Talks to each user's desktop daemon over a private iroh-gossip topic.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures_lite::StreamExt;
use meshcast_signal::{
    derive_pairing_topic, normalize_fps, normalize_quality, sanitize_title, validate_ticket,
    BotLink, BotLinkStore, DeliveryScope, EndpointId, Event, GossipSender, PairCode, PairSignal,
    Signal, SignalNode, TopicId, DEFAULT_FPS, DEFAULT_QUALITY, FPS_OPTIONS, QUALITIES,
};
use poise::serenity_prelude as serenity;
use serenity::all::{
    ButtonStyle, ChannelId, ComponentInteraction, ComponentInteractionDataKind, CreateActionRow,
    CreateButton, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, EditInteractionResponse, EditMessage, MessageId, Timestamp, UserId,
};
use tokio::sync::broadcast;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

const BLURPLE: u32 = 0x5865F2;
const GREY: u32 = 0x99AAB5;
const DOWNLOAD_URL: &str = "https://github.com/mattcree/meshcast/releases/latest";
const DOCS_URL: &str = "https://github.com/mattcree/meshcast#readme";
/// Static page that bounces to `meshcast://watch/<ticket>` (ticket in the URL
/// fragment, so it never reaches the server). Discord only allows http(s) links.
const WATCH_PAGE_URL: &str = "https://mattcree.github.io/meshcast/watch/";

/// How long a pairing code stays valid after `/link`.
const PAIR_CODE_TTL: Duration = Duration::from_secs(600);
/// How long we wait for the app to approve and start capture after Start is clicked.
/// Must comfortably exceed the daemon's consent timeout (90s) plus capture start-up
/// (portal picker, encoder init) so its StreamFailed/StreamReady arrives first.
const START_TIMEOUT: Duration = Duration::from_secs(240);
/// How long Stop waits for the app to confirm before forcing the card to "ended".
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an unfinished `/stream` setup card is kept.
const SETUP_TTL: Duration = Duration::from_secs(900);

/// A `/link` pairing code waiting for the app.
struct PendingPin {
    topic: TopicId,
    user_id: UserId,
    created: Instant,
}

/// In-progress `/stream` setup (ephemeral config card).
#[derive(Clone)]
struct StreamSetup {
    title: String,
    quality: String,
    fps: u32,
    created: Instant,
}

/// A live stream posted to Discord.
#[derive(Clone)]
struct ActiveStream {
    channel_id: ChannelId,
    message_id: MessageId,
    ticket: String,
    title: String,
    viewers: HashSet<UserId>,
    streamer: UserId,
    streamer_name: String,
    avatar_url: String,
    quality: String,
    fps: u32,
    /// The streamer's app accepts remote-control requests for this stream.
    control_available: bool,
    /// Who currently has control (user id, display name).
    controller: Option<(UserId, String)>,
    started_at: Timestamp,
}

impl ActiveStream {
    fn embed(&self) -> CreateEmbed {
        let mut desc = format!(
            "🔴 Live at **{} {}fps**\nClick **Watch** to open it in Meshcast.",
            self.quality, self.fps
        );
        if let Some((_, name)) = &self.controller {
            desc.push_str(&format!("\n🎮 **{name}** has control."));
        } else if self.control_available {
            desc.push_str("\n🎮 Remote control available — click **Request control**.");
        }
        CreateEmbed::new()
            .title(&self.title)
            .description(desc)
            .color(BLURPLE)
            .author(CreateEmbedAuthor::new(&self.streamer_name).icon_url(&self.avatar_url))
            .footer(CreateEmbedFooter::new("Started streaming"))
            .timestamp(self.started_at)
    }

    fn components(&self) -> Vec<CreateActionRow> {
        let id = self.streamer;
        let mut buttons = vec![
            CreateButton::new(format!("watch:{id}"))
                .label("Watch")
                .style(ButtonStyle::Primary),
            CreateButton::new(format!("stop:{id}"))
                .label("Stop")
                .style(ButtonStyle::Secondary),
        ];
        if self.controller.is_some() {
            buttons.push(
                CreateButton::new(format!("revoke:{id}"))
                    .label("Revoke control")
                    .style(ButtonStyle::Danger),
            );
        } else if self.control_available {
            buttons.push(
                CreateButton::new(format!("control:{id}"))
                    .label("Request control")
                    .style(ButtonStyle::Secondary),
            );
        }
        // Direct link for people who have the app but haven't linked (or can't).
        let watch_url = format!("{WATCH_PAGE_URL}#{}", self.ticket);
        if watch_url.len() <= 512 {
            buttons.push(CreateButton::new_link(watch_url).label("Open in app"));
        }
        buttons.push(CreateButton::new_link(DOWNLOAD_URL).label("Get Meshcast"));
        vec![CreateActionRow::Buttons(buttons)]
    }
}

/// Re-render a stream's card after its state changed.
async fn refresh_card(http: &serenity::Http, stream: &ActiveStream) {
    let edit = EditMessage::new()
        .embed(stream.embed())
        .components(stream.components());
    if let Err(e) = stream
        .channel_id
        .edit_message(http, stream.message_id, edit)
        .await
    {
        tracing::warn!("Failed to refresh stream card: {e}");
    }
}

struct Data {
    signal_node: SignalNode,
    /// Gossip links per user.
    links: Arc<Mutex<HashMap<UserId, UserLink>>>,
    /// Users whose app is currently connected (NeighborUp seen, no NeighborDown).
    connected: Arc<Mutex<HashSet<UserId>>>,
    pending_pins: Arc<Mutex<HashMap<String, PendingPin>>>,
    setups: Mutex<HashMap<UserId, StreamSetup>>,
    /// Active streams keyed by streamer.
    streams: Arc<Mutex<HashMap<UserId, ActiveStream>>>,
    /// Users whose /stream flow is currently waiting for StreamReady.
    pending_starts: Arc<Mutex<HashSet<UserId>>>,
    signal_tx: broadcast::Sender<(UserId, Signal)>,
    store_path: std::path::PathBuf,
    store: Arc<Mutex<BotLinkStore>>,
}

fn store_path() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("MESHCAST_BOT_STATE_DIR") {
        return std::path::PathBuf::from(dir).join("state.json");
    }
    dirs_next::config_dir()
        .or_else(|| dirs_next::home_dir().map(|h| h.join(".config")))
        .unwrap_or_default()
        .join("meshcast-bot")
        .join("state.json")
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One user's gossip link on the bot side.
struct UserLink {
    sender: GossipSender,
    /// Aborting this ends the receiver task and drops the subscription.
    task: tokio::task::AbortHandle,
}

impl Drop for UserLink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Spawn a gossip receiver task that routes signals from one user's app to the
/// broadcast channel. Only messages delivered directly (neighbours scope) by
/// the paired app are accepted.
fn spawn_receiver(
    user_id: UserId,
    app_id: Option<EndpointId>,
    mut receiver: iroh_gossip::api::GossipReceiver,
    signal_tx: broadcast::Sender<(UserId, Signal)>,
    connected: Arc<Mutex<HashSet<UserId>>>,
) -> tokio::task::AbortHandle {
    tokio::spawn(async move {
        while let Some(event) = receiver.next().await {
            match event {
                Ok(Event::Received(msg)) => {
                    let from_app = app_id.is_none_or(|id| id == msg.delivered_from);
                    if !from_app || !matches!(msg.scope, DeliveryScope::Neighbors) {
                        tracing::warn!(user = %user_id, peer = %msg.delivered_from.fmt_short(),
                            "Ignoring message not from the paired app");
                        continue;
                    }
                    match Signal::decode(&msg.content) {
                        Ok(signal) => {
                            tracing::debug!(signal = signal.name(), user = %user_id, "Signal from app");
                            let _ = signal_tx.send((user_id, signal));
                        }
                        Err(e) => tracing::warn!(user = %user_id, "Undecodable signal: {e}"),
                    }
                }
                Ok(Event::NeighborUp(id)) => {
                    tracing::info!(peer = %id.fmt_short(), user = %user_id, "App connected");
                    lock(&connected).insert(user_id);
                }
                Ok(Event::NeighborDown(id)) => {
                    tracing::info!(peer = %id.fmt_short(), user = %user_id, "App disconnected");
                    lock(&connected).remove(&user_id);
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(user = %user_id, "Gossip error: {e}");
                    break;
                }
            }
        }
        lock(&connected).remove(&user_id);
        tracing::debug!(user = %user_id, "Receiver task ended");
    })
    .abort_handle()
}

/// Subscribe to a user's link topic and register it (replacing and aborting
/// any previous link for that user).
async fn activate_link(
    data_links: &Arc<Mutex<HashMap<UserId, UserLink>>>,
    connected: &Arc<Mutex<HashSet<UserId>>>,
    signal_tx: &broadcast::Sender<(UserId, Signal)>,
    gossip: &iroh_gossip::net::Gossip,
    user_id: UserId,
    topic: TopicId,
    app_id: Option<EndpointId>,
) -> anyhow::Result<()> {
    let sub = gossip
        .subscribe(topic, vec![])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (sender, receiver) = sub.split();
    let task = spawn_receiver(
        user_id,
        app_id,
        receiver,
        signal_tx.clone(),
        connected.clone(),
    );
    // Dropping a previous UserLink aborts its receiver.
    lock(data_links).insert(user_id, UserLink { sender, task });
    lock(connected).remove(&user_id);
    Ok(())
}

fn sender_for(data: &Data, user: UserId) -> Option<GossipSender> {
    lock(&data.links).get(&user).map(|l| l.sender.clone())
}

fn is_connected(data: &Data, user: UserId) -> bool {
    lock(&data.connected).contains(&user)
}

async fn persist_store(store: &Arc<Mutex<BotLinkStore>>, path: &std::path::Path) {
    let snapshot = lock(store).clone();
    if let Err(e) = snapshot.save(path).await {
        tracing::error!("Failed to save bot state: {e:#}");
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Link your Meshcast app to this server (gives you a pairing code).
#[poise::command(slash_command, ephemeral)]
async fn link(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let data = ctx.data();
    let server_name = ctx
        .guild()
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "Discord".into());

    let real_topic = TopicId::from_bytes(rand::random());
    let pin = PairCode::generate_pin();
    let full_code = PairCode::encode_full(data.signal_node.endpoint.id(), &pin);

    {
        let mut pins = lock(&data.pending_pins);
        pins.retain(|_, p| p.created.elapsed() < PAIR_CODE_TTL);
        pins.insert(
            pin.clone(),
            PendingPin {
                topic: real_topic,
                user_id,
                created: Instant::now(),
            },
        );
    }

    // Listen on the PIN-derived pairing topic for the app's PairRequest.
    let pairing_topic = derive_pairing_topic(&pin);
    let gossip = data.signal_node.gossip.clone();
    let pending_pins = data.pending_pins.clone();
    let links = data.links.clone();
    let connected = data.connected.clone();
    let signal_tx = data.signal_tx.clone();
    let store = data.store.clone();
    let store_path = data.store_path.clone();
    let my_pin = pin.clone();

    tokio::spawn(async move {
        let pairing_sub = match gossip.subscribe(pairing_topic, vec![]).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to subscribe to pairing topic: {e}");
                return;
            }
        };
        let (pair_sender, mut pair_receiver) = pairing_sub.split();

        let mut wrong_attempts = 0u32;
        let result = tokio::time::timeout(PAIR_CODE_TTL, async {
            while let Some(event) = pair_receiver.next().await {
                let Ok(Event::Received(msg)) = event else {
                    continue;
                };
                let Ok(PairSignal::PairRequest { pin: received_pin }) =
                    PairSignal::decode(&msg.content)
                else {
                    continue;
                };
                let app_id = msg.delivered_from;

                // This topic only ever accepts *its own* PIN (never an oracle for
                // other users' codes), and only a few attempts.
                let accepted = if received_pin.trim().eq_ignore_ascii_case(&my_pin) {
                    let mut pins = lock(&pending_pins);
                    match pins.remove(&my_pin) {
                        Some(p) if p.created.elapsed() < PAIR_CODE_TTL => {
                            Some((p.topic, p.user_id))
                        }
                        _ => None,
                    }
                } else {
                    wrong_attempts += 1;
                    None
                };

                match accepted {
                    Some((topic, uid)) => {
                        let accept = PairSignal::PairAccepted {
                            topic: *topic.as_bytes(),
                            server_name: server_name.clone(),
                        };
                        if let Ok(bytes) = accept.encode() {
                            let _ = pair_sender.broadcast_neighbors(bytes).await;
                        }
                                                match activate_link(
                            &links,
                            &connected,
                            &signal_tx,
                            &gossip,
                            uid,
                            topic,
                            Some(app_id),
                        )
                        .await
                        {
                            Ok(()) => {
                                lock(&store).links.insert(
                                    uid.to_string(),
                                    BotLink {
                                        topic: *topic.as_bytes(),
                                        app_id: Some(*app_id.as_bytes()),
                                    },
                                );
                                persist_store(&store, &store_path).await;
                                tracing::info!(user = %uid, "Link established");
                            }
                            Err(e) => tracing::error!(user = %uid, "Failed to activate link: {e}"),
                        }
                        return;
                    }
                                        None => {
                        tracing::warn!(peer = %app_id.fmt_short(), "Invalid or expired pairing code ({wrong_attempts} wrong)");
                        let reject = PairSignal::PairRejected {
                            reason: "Invalid or expired code. Run /link again.".into(),
                        };
                        if let Ok(bytes) = reject.encode() {
                            let _ = pair_sender.broadcast_neighbors(bytes).await;
                        }
                        if wrong_attempts >= 3 {
                            tracing::warn!("Too many wrong pairing attempts; closing pairing topic");
                            lock(&pending_pins).remove(&my_pin);
                            return;
                        }
                    }
                }
            }
        })
        .await;

        if result.is_err() {
            tracing::debug!("Pairing code expired unused");
        }
    });

    ctx.say(format!(
        "**Your pairing code** (valid for 10 minutes):\n\
         ```\n{full_code}\n```\n\
         Open the Meshcast app, paste the code and click **Link**.\n\
         Don't have the app yet? [Download it here]({DOWNLOAD_URL})."
    ))
    .await?;
    Ok(())
}

/// Remove the link between your Meshcast app and this bot.
#[poise::command(slash_command, ephemeral)]
async fn unlink(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let data = ctx.data();
    let existed = lock(&data.links).remove(&user_id).is_some(); // Drop aborts the receiver
    lock(&data.connected).remove(&user_id);
    lock(&data.store).links.remove(&user_id.to_string());
    persist_store(&data.store, &data.store_path).await;
    ctx.say(if existed {
        "Unlinked. Run `/link` to pair again."
    } else {
        "You weren't linked."
    })
    .await?;
    Ok(())
}

/// Share your screen — opens a setup card, then signals your Meshcast app.
#[poise::command(slash_command)]
async fn stream(
    ctx: Context<'_>,
    #[description = "Stream title"]
    #[max_length = 80]
    title: Option<String>,
) -> Result<(), Error> {
    let user = ctx.author();
    let user_id = user.id;
    let data = ctx.data();
    let display_name = user.global_name.as_deref().unwrap_or(&user.name);
    let title = title
        .map(|t| sanitize_title(&t))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("{display_name}'s Stream"));

    if !lock(&data.links).contains_key(&user_id) {
        ctx.send(
            poise::CreateReply::default()
                .content(format!(
                    "Your Meshcast app isn't linked yet.\n\
                     1. [Download Meshcast]({DOWNLOAD_URL}) and open it\n\
                     2. Run `/link` here and paste the code into the app"
                ))
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    // Already live? Offer to stop instead.
    let active = lock(&data.streams).get(&user_id).cloned();
    if let Some(active) = active {
        let stop_btn = CreateButton::new(format!("stop:{user_id}"))
            .label("Stop Stream")
            .style(ButtonStyle::Danger);
        ctx.send(
            poise::CreateReply::default()
                .content(format!(
                    "You're already live with **{}** in <#{}>.",
                    active.title, active.channel_id
                ))
                .components(vec![CreateActionRow::Buttons(vec![stop_btn])])
                .ephemeral(true),
        )
        .await?;
        return Ok(());
    }

    {
        let mut setups = lock(&data.setups);
        setups.retain(|_, s| s.created.elapsed() < SETUP_TTL);
        setups.insert(
            user_id,
            StreamSetup {
                title: title.clone(),
                quality: DEFAULT_QUALITY.into(),
                fps: DEFAULT_FPS,
                created: Instant::now(),
            },
        );
    }

    let config_embed = CreateEmbed::new()
        .title("Stream setup")
        .description(format!(
            "**{title}**\nPick quality and frame rate, then click **Start**.\n\
             Your Meshcast app will ask you to confirm."
        ))
        .color(BLURPLE);

    let quality_row = CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            "stream-quality",
            CreateSelectMenuKind::String {
                options: QUALITIES
                    .iter()
                    .map(|q| {
                        CreateSelectMenuOption::new(*q, *q).default_selection(*q == DEFAULT_QUALITY)
                    })
                    .collect(),
            },
        )
        .placeholder("Resolution"),
    );
    let fps_row = CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            "stream-fps",
            CreateSelectMenuKind::String {
                options: FPS_OPTIONS
                    .iter()
                    .map(|f| {
                        CreateSelectMenuOption::new(format!("{f} FPS"), f.to_string())
                            .default_selection(*f == DEFAULT_FPS)
                    })
                    .collect(),
            },
        )
        .placeholder("Frame rate"),
    );
    let btn_row = CreateActionRow::Buttons(vec![
        CreateButton::new("stream-start")
            .label("Start Stream")
            .style(ButtonStyle::Success),
        CreateButton::new("stream-cancel")
            .label("Cancel")
            .style(ButtonStyle::Secondary),
    ]);

    ctx.send(
        poise::CreateReply::default()
            .embed(config_embed)
            .components(vec![quality_row, fps_row, btn_row])
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Component interactions
// ---------------------------------------------------------------------------

async fn handle_event(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    data: &Data,
) -> Result<(), Error> {
    let serenity::FullEvent::InteractionCreate { interaction } = event else {
        return Ok(());
    };
    let Some(component) = interaction.as_message_component() else {
        return Ok(());
    };

    let id = component.data.custom_id.as_str();
    match id {
        "stream-quality" | "stream-fps" => on_setup_select(ctx, component, data).await,
        "stream-cancel" => on_setup_cancel(ctx, component, data).await,
        "stream-start" => on_stream_start(ctx, component, data).await,
        _ if id.starts_with("watch:") => on_watch(ctx, component, data, &id[6..]).await,
        _ if id.starts_with("stop:") => on_stop(ctx, component, data, &id[5..]).await,
        _ if id.starts_with("control:") => on_control_request(ctx, component, data, &id[8..]).await,
        _ if id.starts_with("revoke:") => on_revoke(ctx, component, data, &id[7..]).await,
        _ => Ok(()),
    }
}

async fn ephemeral(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    text: impl Into<String>,
) -> Result<(), Error> {
    c.create_response(
        ctx,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(text)
                .ephemeral(true),
        ),
    )
    .await?;
    Ok(())
}

async fn update_message(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    text: impl Into<String>,
) -> Result<(), Error> {
    c.create_response(
        ctx,
        CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content(text)
                .embeds(vec![])
                .components(vec![]),
        ),
    )
    .await?;
    Ok(())
}

async fn on_setup_select(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    if let ComponentInteractionDataKind::StringSelect { values } = &c.data.kind {
        if let Some(value) = values.first() {
            let mut setups = lock(&data.setups);
            if let Some(setup) = setups.get_mut(&c.user.id) {
                if c.data.custom_id == "stream-quality" {
                    setup.quality = normalize_quality(value).to_string();
                } else {
                    setup.fps = normalize_fps(value.parse().unwrap_or(DEFAULT_FPS));
                }
            }
        }
    }
    c.create_response(ctx, CreateInteractionResponse::Acknowledge)
        .await?;
    Ok(())
}

async fn on_setup_cancel(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    lock(&data.setups).remove(&c.user.id);
    update_message(ctx, c, "Stream cancelled.").await
}

async fn on_stream_start(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    data: &Data,
) -> Result<(), Error> {
    let user_id = c.user.id;
    let setup = lock(&data.setups).remove(&user_id);
    let Some(setup) = setup else {
        return update_message(ctx, c, "This setup card has expired. Run `/stream` again.").await;
    };
    let StreamSetup {
        title,
        quality,
        fps,
        ..
    } = setup;

    let sender = sender_for(data, user_id);
    let Some(sender) = sender else {
        return update_message(ctx, c, "Your app isn't linked any more. Run `/link` again.").await;
    };
    if !is_connected(data, user_id) {
        return update_message(
            ctx,
            c,
            "Meshcast isn't connected on your computer right now. Open the Meshcast window, \
             wait for it to say *Connected*, then run `/stream` again.",
        )
        .await;
    }

    let already_live = lock(&data.streams).contains_key(&user_id);
    if already_live {
        return update_message(ctx, c, "You're already streaming. Stop that stream first.").await;
    }

    update_message(
        ctx,
        c,
        format!("Requesting **{title}** at {quality} {fps}fps…\nApprove the request in your Meshcast app."),
    )
    .await?;

    let guild_name = match c.guild_id {
        Some(gid) => match gid.to_guild_cached(&ctx.cache).map(|g| g.name.clone()) {
            Some(name) => name,
            None => gid
                .to_partial_guild(ctx)
                .await
                .map(|g| g.name)
                .unwrap_or_default(),
        },
        None => String::new(),
    };

    // Subscribe *before* sending so we can't miss a fast reply, and register the
    // wait so the background task knows a StreamReady is expected.
    let mut rx = data.signal_tx.subscribe();
    lock(&data.pending_starts).insert(user_id);
    struct Unregister<'a>(&'a Data, UserId);
    impl Drop for Unregister<'_> {
        fn drop(&mut self) {
            lock(&self.0.pending_starts).remove(&self.1);
        }
    }
    let _unregister = Unregister(data, user_id);

    let signal = Signal::StartStream {
        title: title.clone(),
        quality: quality.clone(),
        fps,
        server: guild_name,
    };
    if let Err(e) = sender.broadcast_neighbors(signal.encode()?).await {
        tracing::warn!(user = %user_id, "Failed to send StartStream: {e}");
        c.edit_response(
            ctx,
            EditInteractionResponse::new()
                .content("Couldn't reach your Meshcast app. Is it running?"),
        )
        .await?;
        return Ok(());
    }

    enum Outcome {
        Ready(String),
        Failed(String),
        Timeout,
    }

    let mut control_available = false;
    let outcome = tokio::time::timeout(START_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Ok((uid, Signal::ControlAvailable { available })) if uid == user_id => {
                    control_available = available;
                }
                Ok((uid, Signal::StreamReady { ticket })) if uid == user_id => {
                    return Outcome::Ready(ticket);
                }
                Ok((uid, Signal::StreamFailed { reason })) if uid == user_id => {
                    return Outcome::Failed(reason);
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Outcome::Timeout,
            }
        }
    })
    .await
    .unwrap_or(Outcome::Timeout);

    match outcome {
        Outcome::Ready(ticket) => {
            let display_name = c.user.global_name.as_deref().unwrap_or(&c.user.name);
            let channel_id = c.channel_id;
            let mut stream = ActiveStream {
                channel_id,
                message_id: MessageId::new(1),
                ticket,
                title: title.clone(),
                viewers: HashSet::new(),
                streamer: user_id,
                streamer_name: display_name.to_string(),
                avatar_url: c.user.face(),
                quality: quality.clone(),
                fps,
                control_available,
                controller: None,
                started_at: Timestamp::now(),
            };
            if let Err(e) = validate_ticket(&stream.ticket) {
                tracing::warn!(user = %user_id, "App sent an invalid ticket: {e}");
                let _ = sender
                    .broadcast_neighbors(Signal::StopStream.encode()?)
                    .await;
                c.edit_response(
                    ctx,
                    EditInteractionResponse::new()
                        .content("Your app sent an invalid stream ticket; stream stopped."),
                )
                .await?;
                return Ok(());
            }
            let msg = match channel_id
                .send_message(
                    ctx,
                    CreateMessage::new()
                        .embed(stream.embed())
                        .components(stream.components()),
                )
                .await
            {
                Ok(m) => m,
                Err(e) => {
                    // Don't leave the desktop capturing with no card and no Stop.
                    tracing::warn!(user = %user_id, "Couldn't post stream card: {e}");
                    let _ = sender
                        .broadcast_neighbors(Signal::StopStream.encode()?)
                        .await;
                    c.edit_response(
                        ctx,
                        EditInteractionResponse::new().content(
                            "I couldn't post the stream card in this channel (missing permission?). \
                             Stream stopped — try in a channel where I can send messages and embeds.",
                        ),
                    )
                    .await?;
                    return Ok(());
                }
            };
            stream.message_id = msg.id;
            lock(&data.streams).insert(user_id, stream);
            tracing::info!(user = %user_id, "Stream live: {title}");

            c.edit_response(
                ctx,
                EditInteractionResponse::new().content(
                    "You're live! Stop any time with the **Stop** button on the post, \
                     from the Meshcast app, or by running `/stream` again.",
                ),
            )
            .await?;
        }
        Outcome::Failed(reason) => {
            tracing::info!(user = %user_id, "Stream not started: {reason}");
            c.edit_response(
                ctx,
                EditInteractionResponse::new().content(format!("Stream not started: {reason}")),
            )
            .await?;
        }
        Outcome::Timeout => {
            // Stop waiting *before* telling the app to stop, so a StreamReady that
            // races in is treated as stray (and stopped) rather than expected.
            drop(_unregister);
            let _ = sender
                .broadcast_neighbors(Signal::StopStream.encode()?)
                .await;
            c.edit_response(
                ctx,
                EditInteractionResponse::new().content(
                    "Your Meshcast app didn't respond in time. Make sure it's open and says *Connected*, then try again.",
                ),
            )
            .await?;
        }
    }
    Ok(())
}

/// A viewer asks the streamer for control.
async fn on_control_request(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    data: &Data,
    streamer: &str,
) -> Result<(), Error> {
    let Ok(streamer_id) = streamer.parse::<u64>().map(UserId::new) else {
        return ephemeral(ctx, c, "This stream post is no longer valid.").await;
    };
    let viewer_id = c.user.id;
    let viewer_name = c
        .user
        .global_name
        .clone()
        .unwrap_or_else(|| c.user.name.clone());

    let stream = lock(&data.streams).get(&streamer_id).cloned();
    let Some(stream) = stream else {
        return ephemeral(ctx, c, "This stream has ended.").await;
    };
    if viewer_id == streamer_id {
        return ephemeral(ctx, c, "You can't request control of your own stream.").await;
    }
    if !stream.control_available {
        return ephemeral(
            ctx,
            c,
            "The streamer hasn't enabled remote control for this stream.",
        )
        .await;
    }
    if let Some((_, name)) = &stream.controller {
        return ephemeral(ctx, c, format!("{name} already has control.")).await;
    }
    let viewer_sender = sender_for(data, viewer_id);
    let Some(viewer_sender) = viewer_sender else {
        return ephemeral(
            ctx,
            c,
            format!("Link your Meshcast app first (`/link`) — control is delivered to your viewer window. [Install]({DOWNLOAD_URL})"),
        )
        .await;
    };
    let streamer_sender = sender_for(data, streamer_id);
    let Some(streamer_sender) = streamer_sender else {
        return ephemeral(ctx, c, "The streamer's app isn't linked any more.").await;
    };
    if !is_connected(data, viewer_id) {
        return ephemeral(
            ctx,
            c,
            "Meshcast isn't connected on your computer — open the Meshcast window first.",
        )
        .await;
    }

    ephemeral(
        ctx,
        c,
        format!(
            "Asked **{}** for control — waiting for them to approve in Meshcast…",
            stream.streamer_name
        ),
    )
    .await?;

    let request_id: u64 = rand::random();
    let mut rx = data.signal_tx.subscribe();
    if let Err(e) = streamer_sender
        .broadcast_neighbors(
            Signal::ControlRequest {
                request_id,
                viewer: viewer_name.clone(),
            }
            .encode()?,
        )
        .await
    {
        tracing::warn!("Failed to send ControlRequest: {e}");
        c.edit_response(
            ctx,
            EditInteractionResponse::new().content("Couldn't reach the streamer's app."),
        )
        .await?;
        return Ok(());
    }

    enum Outcome {
        Granted {
            token: String,
            addr: iroh::EndpointAddr,
        },
        Denied(String),
        Timeout,
    }
    let outcome = tokio::time::timeout(START_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Ok((
                    uid,
                    Signal::ControlGranted {
                        request_id: rid,
                        token,
                        addr,
                    },
                )) if uid == streamer_id && rid == request_id => {
                    return Outcome::Granted { token, addr };
                }
                Ok((
                    uid,
                    Signal::ControlDenied {
                        request_id: rid,
                        reason,
                    },
                )) if uid == streamer_id && (rid == request_id || rid == 0) => {
                    return Outcome::Denied(reason);
                }
                Ok((uid, Signal::StreamStopped)) if uid == streamer_id => {
                    return Outcome::Denied("the stream ended".into());
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Outcome::Timeout,
            }
        }
    })
    .await
    .unwrap_or(Outcome::Timeout);

    match outcome {
        Outcome::Granted { token, addr } => {
            let token_msg = Signal::ControlToken {
                ticket: stream.ticket.clone(),
                token,
                addr,
                streamer: stream.streamer_name.clone(),
            }
            .encode()?;
            if let Err(e) = viewer_sender.broadcast_neighbors(token_msg).await {
                tracing::warn!("Failed to deliver ControlToken: {e}");
                // Don't leave the streamer's daemon armed for a viewer who'll never connect.
                if let Ok(bytes) = Signal::RevokeControl.encode() {
                    let _ = streamer_sender.broadcast_neighbors(bytes).await;
                }
                c.edit_response(
                    ctx,
                    EditInteractionResponse::new().content(
                        "Granted, but your Meshcast app couldn't be reached. Is it running?",
                    ),
                )
                .await?;
                return Ok(());
            }
            let updated = {
                let mut streams = lock(&data.streams);
                match streams.get_mut(&streamer_id) {
                    Some(s) => {
                        s.controller = Some((viewer_id, viewer_name.clone()));
                        Some(s.clone())
                    }
                    None => None,
                }
            };
            if let Some(s) = updated {
                refresh_card(&ctx.http, &s).await;
            }
            tracing::info!(viewer = %viewer_id, streamer = %streamer_id, "Control granted");
            c.edit_response(
                ctx,
                EditInteractionResponse::new().content(
                    "**You have control.** It activates in your Meshcast viewer window for this stream \
                     (open it with **Watch** if you haven't). F8 pauses, Esc Esc releases.",
                ),
            )
            .await?;
        }
        Outcome::Denied(reason) => {
            c.edit_response(
                ctx,
                EditInteractionResponse::new().content(format!("Control not granted: {reason}")),
            )
            .await?;
        }
        Outcome::Timeout => {
            c.edit_response(
                ctx,
                EditInteractionResponse::new()
                    .content("The streamer didn't respond. You can ask again later."),
            )
            .await?;
        }
    }
    Ok(())
}

/// The streamer takes control back from the card.
async fn on_revoke(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    data: &Data,
    streamer: &str,
) -> Result<(), Error> {
    let Ok(streamer_id) = streamer.parse::<u64>().map(UserId::new) else {
        return ephemeral(ctx, c, "This stream post is no longer valid.").await;
    };
    if c.user.id != streamer_id {
        return ephemeral(ctx, c, "Only the streamer can revoke control.").await;
    }
    let has_controller = lock(&data.streams)
        .get(&streamer_id)
        .is_some_and(|s| s.controller.is_some());
    if !has_controller {
        return ephemeral(ctx, c, "Nobody has control right now.").await;
    }
    let sender = sender_for(data, streamer_id);
    ephemeral(ctx, c, "Revoking control…").await?;
    let mut rx = data.signal_tx.subscribe();
    if let Some(s) = sender {
        let _ = s.broadcast_neighbors(Signal::RevokeControl.encode()?).await;
    }
    // The app confirms with ControlRevoked (the background task updates the
    // card); if it doesn't within the timeout, tidy the card anyway.
    let confirmed = tokio::time::timeout(STOP_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Ok((uid, Signal::ControlRevoked)) if uid == streamer_id => return true,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    if !confirmed {
        clear_controller(&ctx.http, &data.streams, streamer_id).await;
    }
    c.edit_response(
        ctx,
        EditInteractionResponse::new().content("Control revoked."),
    )
    .await?;
    Ok(())
}

/// Forget the controller of a stream and refresh its card.
async fn clear_controller(
    http: &serenity::Http,
    streams: &Arc<Mutex<HashMap<UserId, ActiveStream>>>,
    streamer_id: UserId,
) {
    let updated = {
        let mut st = lock(streams);
        match st.get_mut(&streamer_id) {
            Some(s) if s.controller.is_some() => {
                s.controller = None;
                Some(s.clone())
            }
            _ => None,
        }
    };
    if let Some(s) = updated {
        refresh_card(http, &s).await;
    }
}

async fn on_watch(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    data: &Data,
    streamer: &str,
) -> Result<(), Error> {
    let Ok(streamer_id) = streamer.parse::<u64>().map(UserId::new) else {
        return ephemeral(ctx, c, "This stream post is no longer valid.").await;
    };
    let viewer_id = c.user.id;
    tracing::info!(viewer = %viewer_id, streamer = %streamer_id, "Watch clicked");

    let stream = lock(&data.streams).get(&streamer_id).cloned();
    let Some(stream) = stream else {
        return ephemeral(ctx, c, "This stream has ended.").await;
    };

    if viewer_id == streamer_id {
        return ephemeral(ctx, c, "That's your own stream 🙂").await;
    }

    let sender = sender_for(data, viewer_id);
    let Some(sender) = sender else {
        return ephemeral(
            ctx,
            c,
            format!(
                "To watch with one click, [install Meshcast]({DOWNLOAD_URL}) and run `/link`.\n\n\
                 Already have it? Open this stream manually:\n```\nmeshcast watch {}\n```\n\
                 [Setup guide]({DOCS_URL})",
                stream.ticket
            ),
        )
        .await;
    };

    let msg = Signal::WatchStream {
        ticket: stream.ticket.clone(),
    }
    .encode()?;
    let mut sent = false;
    for attempt in 1..=3 {
        match sender.broadcast_neighbors(msg.clone()).await {
            Ok(()) => {
                sent = true;
                break;
            }
            Err(e) => {
                tracing::warn!(viewer = %viewer_id, "WatchStream attempt {attempt} failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    if !sent {
        return ephemeral(
            ctx,
            c,
            "Couldn't reach your Meshcast app. Make sure it's open and try again.",
        )
        .await;
    }

    let count = {
        let mut streams = lock(&data.streams);
        match streams.get_mut(&streamer_id) {
            Some(s) => {
                s.viewers.insert(viewer_id);
                s.viewers.len() as u32
            }
            None => 0,
        }
    };
    let streamer_sender = sender_for(data, streamer_id);
    if let Some(ss) = streamer_sender {
        let _ = ss
            .broadcast_neighbors(Signal::ViewerUpdate { count }.encode()?)
            .await;
    }
    ephemeral(
        ctx,
        c,
        format!(
            "Opening in Meshcast… ({count} viewer{})",
            if count == 1 { "" } else { "s" }
        ),
    )
    .await
}

async fn on_stop(
    ctx: &serenity::Context,
    c: &ComponentInteraction,
    data: &Data,
    streamer: &str,
) -> Result<(), Error> {
    let Ok(streamer_id) = streamer.parse::<u64>().map(UserId::new) else {
        return ephemeral(ctx, c, "This stream post is no longer valid.").await;
    };
    if c.user.id != streamer_id {
        return ephemeral(ctx, c, "Only the streamer can stop this stream.").await;
    }
    let is_live = lock(&data.streams).contains_key(&streamer_id);
    if !is_live {
        return ephemeral(ctx, c, "This stream has already ended.").await;
    }
    let sender = sender_for(data, streamer_id);
    let Some(sender) = sender else {
        // No link any more: at least tidy up the post.
        mark_stream_ended(&ctx.http, &data.streams, streamer_id).await;
        return ephemeral(ctx, c, "Stream marked as ended.").await;
    };

    // Respond within Discord's 3 s window, then wait for the app to confirm.
    ephemeral(ctx, c, "Stopping your stream…").await?;
    let mut rx = data.signal_tx.subscribe();
    let _ = sender
        .broadcast_neighbors(Signal::StopStream.encode()?)
        .await;

    let confirmed = tokio::time::timeout(STOP_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Ok((uid, Signal::StreamStopped)) if uid == streamer_id => return true,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    if confirmed {
        // The background task has already updated the post.
        c.edit_response(
            ctx,
            EditInteractionResponse::new().content("Stream stopped."),
        )
        .await?;
    } else {
        // App unreachable (crashed / offline): don't leave a dead Live card around.
        tracing::warn!(user = %streamer_id, "No StreamStopped from app; forcing end");
        mark_stream_ended(&ctx.http, &data.streams, streamer_id).await;
        c.edit_response(
            ctx,
            EditInteractionResponse::new().content(
                "Your Meshcast app didn't confirm — marked the stream as ended. \
                 If it's still capturing, stop it from the app or tray.",
            ),
        )
        .await?;
    }
    Ok(())
}

/// Replace the live embed with an "ended" one and forget the stream.
async fn mark_stream_ended(
    http: &serenity::Http,
    streams: &Arc<Mutex<HashMap<UserId, ActiveStream>>>,
    user_id: UserId,
) {
    let stream = lock(streams).remove(&user_id);
    let Some(stream) = stream else {
        return;
    };
    tracing::info!(user = %user_id, "Stream ended: {}", stream.title);
    let ended = CreateEmbed::new()
        .title(stream.title)
        .description("This stream has ended.")
        .color(GREY)
        .footer(CreateEmbedFooter::new("Stream ended"))
        .timestamp(Timestamp::now());
    let edit = EditMessage::new().embed(ended).components(vec![]);
    if let Err(e) = stream
        .channel_id
        .edit_message(http, stream.message_id, edit)
        .await
    {
        tracing::warn!("Failed to update ended-stream post: {e}");
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("meshcast-bot {}", meshcast_signal::VERSION);
        return Ok(());
    }
    meshcast_signal::telemetry::init("bot");

    let token = std::env::var("DISCORD_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .context("DISCORD_TOKEN environment variable required")?;

    let path = store_path();
    let mut store = BotLinkStore::load(&path)
        .await
        .with_context(|| format!("Failed to load bot state from {}", path.display()))?;

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
    tracing::info!(
        "Signal node ready ({})",
        signal_node.endpoint.id().fmt_short()
    );

    let (signal_tx, _) = broadcast::channel(1024);
    let links: Arc<Mutex<HashMap<UserId, UserLink>>> = Arc::new(Mutex::new(HashMap::new()));
    let connected: Arc<Mutex<HashSet<UserId>>> = Arc::new(Mutex::new(HashSet::new()));

    // Restore saved links
    for (user_id_str, link) in &store.links {
        let Ok(id) = user_id_str.parse::<u64>() else {
            tracing::warn!("Invalid user ID in store: {user_id_str}");
            continue;
        };
        let user_id = UserId::new(id);
        match activate_link(
            &links,
            &connected,
            &signal_tx,
            &signal_node.gossip,
            user_id,
            link.topic_id(),
            link.app_endpoint_id(),
        )
        .await
        {
            Ok(()) => tracing::info!(user = %user_id, "Restored link"),
            Err(e) => tracing::warn!(user = %user_id, "Failed to restore link: {e}"),
        }
    }
    tracing::info!("Restored {} link(s)", lock(&links).len());

    let streams: Arc<Mutex<HashMap<UserId, ActiveStream>>> = Arc::new(Mutex::new(HashMap::new()));

    let pending_starts: Arc<Mutex<HashSet<UserId>>> = Arc::new(Mutex::new(HashSet::new()));

    // Background: update Discord posts when streams end (e.g. stopped from the app),
    // and keep the app honest if it goes live when nobody is waiting for it.
    {
        let mut rx = signal_tx.subscribe();
        let streams = streams.clone();
        let pending_starts = pending_starts.clone();
        let links = links.clone();
        let links_bg = links.clone();
        let http = serenity::Http::new(&token);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok((user_id, Signal::StreamStopped)) => {
                        mark_stream_ended(&http, &streams, user_id).await;
                    }
                    Ok((user_id, Signal::Hello { version, .. })) => {
                        tracing::info!(user = %user_id, "App version {version}");
                        // Reply so the app can show our version too.
                        let sender = lock(&links_bg).get(&user_id).map(|l| l.sender.clone());
                        if let (Some(s), Ok(bytes)) = (
                            sender,
                            Signal::Hello {
                                version: meshcast_signal::VERSION.to_string(),
                                features: meshcast_signal::CAPABILITIES
                                    .iter()
                                    .map(|x| x.to_string())
                                    .collect(),
                            }
                            .encode(),
                        ) {
                            let _ = s.broadcast_neighbors(bytes).await;
                        }
                    }
                    Ok((user_id, Signal::ControlRevoked)) => {
                        clear_controller(&http, &streams, user_id).await;
                    }
                    Ok((user_id, Signal::ControlAvailable { available })) => {
                        let updated = {
                            let mut st = lock(&streams);
                            match st.get_mut(&user_id) {
                                Some(s) if s.control_available != available => {
                                    s.control_available = available;
                                    Some(s.clone())
                                }
                                _ => None,
                            }
                        };
                        if let Some(s) = updated {
                            refresh_card(&http, &s).await;
                        }
                    }
                    Ok((user_id, Signal::StreamReady { .. })) => {
                        // A StreamReady that arrives after the /stream flow gave up
                        // (timeout) would leave the desktop capturing with no card
                        // and no Stop button. Tell the app to stop instead.
                        let waiting = lock(&pending_starts).contains(&user_id)
                            || lock(&streams).contains_key(&user_id);
                        if !waiting {
                            tracing::warn!(user = %user_id, "Late StreamReady with no pending start; stopping it");
                            let sender = lock(&links).get(&user_id).map(|l| l.sender.clone());
                            if let (Some(s), Ok(bytes)) = (sender, Signal::StopStream.encode()) {
                                let _ = s.broadcast_neighbors(bytes).await;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Signal channel lagged by {n}");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let framework = poise::Framework::builder()
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                tracing::info!(
                    "Bot {} ready as {}",
                    meshcast_signal::VERSION,
                    ready.user.name
                );
                Ok(Data {
                    signal_node,
                    links,
                    connected,
                    pending_pins: Arc::new(Mutex::new(HashMap::new())),
                    setups: Mutex::new(HashMap::new()),
                    streams,
                    pending_starts,
                    signal_tx,
                    store_path: path,
                    store: Arc::new(Mutex::new(store)),
                })
            })
        })
        .options(poise::FrameworkOptions {
            commands: vec![stream(), link(), unlink()],
            event_handler: |ctx, event, _framework_ctx, data| {
                Box::pin(handle_event(ctx, event, data))
            },
            on_error: |error| Box::pin(on_error(error)),
            ..Default::default()
        })
        .build();

    let intents = serenity::GatewayIntents::non_privileged();
    let mut client = serenity::ClientBuilder::new(&token, intents)
        .framework(framework)
        .await
        .context("Failed to create Discord client")?;

    // Graceful shutdown on Ctrl+C / SIGTERM.
    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("Shutting down...");
        shard_manager.shutdown_all().await;
    });

    client.start().await.context("Discord client error")?;
    Ok(())
}

async fn on_error(error: poise::FrameworkError<'_, Data, Error>) {
    match error {
        poise::FrameworkError::Command { error, ctx, .. } => {
            tracing::error!(command = ctx.command().name, "Command failed: {error:?}");
            let _ = ctx
                .send(
                    poise::CreateReply::default()
                        .content("Something went wrong. Please try again.")
                        .ephemeral(true),
                )
                .await;
        }
        poise::FrameworkError::EventHandler { error, event, .. } => {
            tracing::error!(
                event = event.snake_case_name(),
                "Event handler failed: {error:?}"
            );
        }
        other => {
            if let Err(e) = poise::builtins::on_error(other).await {
                tracing::error!("Error handler failed: {e}");
            }
        }
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
