use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Context as _;
use poise::serenity_prelude as serenity;
use serenity::all::{ChannelId, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, MessageId};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

struct Data {
    /// Tracks the active stream embed per channel so we can edit it on /stream stop.
    active_streams: Mutex<HashMap<ChannelId, MessageId>>,
}

/// Post a stream link to this channel, or stop an active stream.
#[poise::command(slash_command)]
async fn stream(
    ctx: Context<'_>,
    #[description = "Stream title"] title: Option<String>,
    #[description = "iroh-live ticket string"] ticket: Option<String>,
    #[description = "Stop the active stream"] stop: Option<bool>,
) -> Result<(), Error> {
    if stop.unwrap_or(false) {
        return handle_stop(ctx).await;
    }

    let ticket = match ticket {
        Some(t) => t,
        None => {
            ctx.say("Provide a ticket: `/stream ticket:<ticket>`").await?;
            return Ok(());
        }
    };

    let user = ctx.author();
    let display_name = user.global_name.as_deref().unwrap_or(&user.name);
    let title = title.unwrap_or_else(|| format!("{display_name}'s Stream"));
    let avatar_url = user.avatar_url().unwrap_or_default();
    let watch_uri = format!("meshcast://watch/{ticket}");

    let embed = CreateEmbed::new()
        .title(&title)
        .description("Click **Watch** to open the stream in Meshcast.")
        .color(0x5865F2) // Discord blurple
        .author(CreateEmbedAuthor::new(display_name).icon_url(&avatar_url))
        .footer(serenity::all::CreateEmbedFooter::new("Started streaming"))
        .timestamp(serenity::model::Timestamp::now());

    let button = CreateButton::new_link(&watch_uri).label("Watch (Native)");
    let row = CreateActionRow::Buttons(vec![button]);

    let reply = poise::CreateReply::default()
        .embed(embed)
        .components(vec![row]);

    let handle = ctx.send(reply).await?;
    let message = handle.message().await?;

    // Track so /stream stop can edit it later.
    let channel_id = ctx.channel_id();
    ctx.data()
        .active_streams
        .lock()
        .unwrap()
        .insert(channel_id, message.id);

    Ok(())
}

async fn handle_stop(ctx: Context<'_>) -> Result<(), Error> {
    let channel_id = ctx.channel_id();
    let message_id = ctx
        .data()
        .active_streams
        .lock()
        .unwrap()
        .remove(&channel_id);

    let message_id = match message_id {
        Some(id) => id,
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

    let ended_embed = CreateEmbed::new()
        .title("Stream Ended")
        .color(0x99AAB5) // grey
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
                .unwrap_or_else(|_| "meshcast_bot=info".into()),
        )
        .init();

    let token =
        std::env::var("DISCORD_TOKEN").context("DISCORD_TOKEN environment variable required")?;

    let framework = poise::Framework::builder()
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                tracing::info!("Bot is ready");
                Ok(Data {
                    active_streams: Mutex::new(HashMap::new()),
                })
            })
        })
        .options(poise::FrameworkOptions {
            commands: vec![stream()],
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
