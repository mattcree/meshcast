use std::process::Stdio;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[derive(Parser)]
#[command(name = "meshcast", about = "P2P screen streaming for Discord via iroh-live")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start streaming your screen (wraps `irl publish --video screen`)
    Stream {
        /// Stream title (used when posting to Discord)
        #[arg(long)]
        title: Option<String>,

        /// Discord webhook URL to auto-post the ticket
        #[arg(long)]
        post_to_discord: Option<String>,

        /// Additional arguments passed through to `irl publish`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        irl_args: Vec<String>,
    },

    /// Watch a stream (wraps `irl play <ticket>`)
    Watch {
        /// Ticket string or meshcast://watch/<ticket> URI
        ticket: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "meshcast=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Stream {
            title,
            post_to_discord,
            irl_args,
        } => cmd_stream(title, post_to_discord, irl_args).await,
        Commands::Watch { ticket } => cmd_watch(ticket).await,
    }
}

async fn cmd_stream(
    _title: Option<String>,
    post_to_discord: Option<String>,
    extra_args: Vec<String>,
) -> anyhow::Result<()> {
    let mut args = vec![
        "publish".to_string(),
        "--video".to_string(),
        "screen".to_string(),
    ];
    args.extend(extra_args);

    tracing::info!("Starting: irl {}", args.join(" "));

    let mut child = Command::new("irl")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to start `irl`. Is iroh-live-cli installed and in PATH?")?;

    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    // Scan stdout for the ticket (lines starting with "iroh-live:")
    let mut ticket_found = false;
    while let Some(line) = lines.next_line().await? {
        println!("{line}");
        if !ticket_found {
            if let Some(ticket) = extract_ticket(&line) {
                ticket_found = true;
                tracing::info!("Ticket: {ticket}");
                println!("\n  meshcast://watch/{ticket}\n");

                if let Some(ref webhook_url) = post_to_discord {
                    if let Err(e) = post_ticket_to_discord(webhook_url, &ticket).await {
                        tracing::warn!("Failed to post to Discord: {e}");
                    }
                }
            }
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        bail!("irl exited with {status}");
    }
    Ok(())
}

async fn cmd_watch(raw: String) -> anyhow::Result<()> {
    let ticket = parse_ticket_uri(&raw);
    tracing::info!("Watching: {ticket}");

    let status = Command::new("irl")
        .args(["play", &ticket])
        .status()
        .await
        .context("Failed to start `irl`. Is iroh-live-cli installed and in PATH?")?;

    if !status.success() {
        bail!("irl exited with {status}");
    }
    Ok(())
}

/// Strips the `meshcast://watch/` prefix if present.
fn parse_ticket_uri(raw: &str) -> &str {
    raw.strip_prefix("meshcast://watch/")
        .or_else(|| raw.strip_prefix("meshcast:///watch/"))
        .unwrap_or(raw)
}

/// Looks for an iroh-live ticket in a line of output.
fn extract_ticket(line: &str) -> Option<&str> {
    // iroh-live tickets look like: iroh-live:<base64>/<name>
    // The CLI may print it bare or in a "Ticket: <ticket>" format.
    let trimmed = line.trim();
    if trimmed.starts_with("iroh-live:") {
        return Some(trimmed);
    }
    // Also handle "Ticket: iroh-live:..." or similar prefixed output
    if let Some(rest) = trimmed.strip_prefix("Ticket:") {
        let rest = rest.trim();
        if rest.starts_with("iroh-live:") {
            return Some(rest);
        }
    }
    None
}

async fn post_ticket_to_discord(webhook_url: &str, ticket: &str) -> anyhow::Result<()> {
    // Simple Discord webhook POST — sends the ticket as a message.
    // The bot can pick this up, or the webhook itself posts an embed.
    let body = serde_json::json!({
        "content": format!("Stream started! Ticket: `{ticket}`\n\nWatch: meshcast://watch/{ticket}")
    });

    // Use a simple reqwest-free approach via curl to avoid adding deps.
    let status = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body.to_string(),
            webhook_url,
        ])
        .status()
        .await
        .context("Failed to run curl")?;

    if !status.success() {
        bail!("curl exited with {status}");
    }
    tracing::info!("Posted ticket to Discord webhook");
    Ok(())
}
