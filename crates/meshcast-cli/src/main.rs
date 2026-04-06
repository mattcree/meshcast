use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use iroh_live::ticket::LiveTicket;
use iroh_live::Live;
use moq_media::capture::ScreenCapturer;
use moq_media::codec::{AudioCodec, VideoCodec};
use moq_media::format::{AudioPreset, VideoPreset};
use moq_media::publish::LocalBroadcast;
use moq_media::AudioBackend;

#[derive(Parser)]
#[command(name = "meshcast", about = "P2P screen streaming for Discord via iroh-live")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start streaming your screen
    Stream {
        /// Broadcast name (used in the ticket)
        #[arg(long, default_value = "meshcast")]
        name: String,

        /// Disable audio capture
        #[arg(long)]
        no_audio: bool,

        /// Video quality preset: 360p, 720p, 1080p
        #[arg(long, default_value = "720p")]
        quality: String,
    },

    /// Watch a stream
    Watch {
        /// Ticket string or meshcast://watch/<ticket> URI
        ticket: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "meshcast=info,iroh_live=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Stream {
            name,
            no_audio,
            quality,
        } => cmd_stream(name, no_audio, quality).await,
        Commands::Watch { ticket } => cmd_watch(ticket).await,
    }
}

async fn cmd_stream(name: String, no_audio: bool, quality: String) -> Result<()> {
    let live = Live::from_env()
        .await
        .context("Failed to initialize iroh-live")?
        .with_router()
        .spawn();

    let broadcast = LocalBroadcast::new();

    // Screen capture
    let screen = ScreenCapturer::new().context("Failed to initialize screen capture")?;
    let preset = match quality.as_str() {
        "360p" => VideoPreset::P360,
        "720p" => VideoPreset::P720,
        "1080p" => VideoPreset::P1080,
        _ => {
            tracing::warn!("Unknown quality '{quality}', defaulting to 720p");
            VideoPreset::P720
        }
    };
    broadcast
        .video()
        .set_source(screen, VideoCodec::H264, [preset])
        .context("Failed to set video source")?;
    tracing::info!("Screen capture started ({quality})");

    // Audio (optional)
    if !no_audio {
        let audio_backend = AudioBackend::default();
        match audio_backend.default_input().await {
            Ok(mic) => {
                broadcast
                    .audio()
                    .set(mic, AudioCodec::Opus, [AudioPreset::Hq])
                    .context("Failed to set audio source")?;
                tracing::info!("Audio capture started");
            }
            Err(e) => {
                tracing::warn!("No audio input available: {e}");
            }
        }
    }

    // Publish and print ticket
    live.publish(&name, &broadcast)
        .await
        .context("Failed to publish broadcast")?;

    let ticket = LiveTicket::new(live.endpoint().addr(), &name);
    let ticket_str = ticket.to_string();

    println!("\nStreaming! Share this ticket to let others watch:\n");
    println!("  {ticket_str}\n");
    println!("  meshcast://watch/{ticket_str}\n");
    println!("Press Ctrl+C to stop.\n");

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down...");
    live.shutdown().await;

    Ok(())
}

async fn cmd_watch(raw: String) -> Result<()> {
    let ticket_str = parse_ticket_uri(&raw);
    let ticket: LiveTicket = ticket_str
        .parse()
        .context("Invalid ticket string")?;

    tracing::info!("Connecting to stream '{}'...", ticket.broadcast_name);

    let live = Live::from_env()
        .await
        .context("Failed to initialize iroh-live")?
        .spawn();

    let sub = live
        .subscribe(ticket.endpoint, &ticket.broadcast_name)
        .await
        .context("Failed to subscribe to stream")?;

    let audio_backend = AudioBackend::default();
    let tracks = sub
        .media(&audio_backend, Default::default())
        .await
        .context("Failed to initialize media tracks")?;

    tracing::info!("Connected. Rendering stream...");

    if let Some(mut video) = tracks.video {
        while let Some(_frame) = video.next_frame().await {
            // Rendering is handled internally by iroh-live's wgpu integration
        }
    } else {
        tracing::warn!("No video track in this stream");
        tokio::signal::ctrl_c().await?;
    }

    live.shutdown().await;
    Ok(())
}

/// Strips the `meshcast://watch/` prefix if present.
fn parse_ticket_uri(raw: &str) -> &str {
    raw.strip_prefix("meshcast://watch/")
        .or_else(|| raw.strip_prefix("meshcast:///watch/"))
        .unwrap_or(raw)
}
