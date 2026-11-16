//! `discord-voice`: prints voice events from the local Discord client.

mod auth;
mod ipc;
mod rpc;

use anyhow::{bail, Result};
use serde_json::json;
use tracing_subscriber::EnvFilter;

use crate::rpc::{RpcClient, VoiceChannel, VoiceEvent, VoiceMember};

const USAGE: &str = "usage: discord-voice [events]

Commands:
  events   connect to Discord and print voice events (default)

Config: $XDG_CONFIG_HOME/discord-voice/config.toml (see README.md).
Logging: set RUST_LOG (e.g. RUST_LOG=debug); logs go to stderr.";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("events") => run_events().await,
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

async fn run_events() -> Result<()> {
    let dir = auth::config_dir()?;
    let cfg = auth::load_config(&dir.join("config.toml"))?;

    let (mut conn, path) = ipc::IpcConnection::connect().await?;
    tracing::info!(socket = %path.display(), "connected");
    conn.handshake(&cfg.client_id).await?;

    let mut rpc = RpcClient::new(conn);
    auth::authenticate(&mut rpc, &cfg, &dir.join("token.json")).await?;
    tracing::info!("authenticated");

    rpc.subscribe("VOICE_CHANNEL_SELECT", json!({})).await?;
    let mut current: Option<String> = None;
    join_selected(&mut rpc, &mut current).await?;

    loop {
        let event = tokio::select! {
            ev = rpc.next_event() => ev?,
            _ = tokio::signal::ctrl_c() => return Ok(()),
        };
        match event {
            VoiceEvent::ChannelSelect { channel_id, .. } => {
                if let Some(old) = current.take() {
                    println!("LEFT channel={old}");
                    if let Err(e) = rpc.unsubscribe_channel(&old).await {
                        tracing::warn!(error = %e, "unsubscribe failed");
                    }
                }
                if channel_id.is_some() {
                    join_selected(&mut rpc, &mut current).await?;
                }
            }
            VoiceEvent::StateCreate(m) => println!("VOICE_STATE_CREATE {}", member_line(&m)),
            VoiceEvent::StateUpdate(m) => println!("VOICE_STATE_UPDATE {}", member_line(&m)),
            VoiceEvent::StateDelete(m) => println!("VOICE_STATE_DELETE {}", member_line(&m)),
            VoiceEvent::SpeakingStart { user_id, .. } => {
                println!("SPEAKING_START user_id={user_id}")
            }
            VoiceEvent::SpeakingStop { user_id, .. } => println!("SPEAKING_STOP user_id={user_id}"),
            VoiceEvent::Other { evt } => tracing::debug!(evt, "unhandled event"),
        }
    }
}

/// Fetches the selected voice channel; if any, prints its members and
/// subscribes to its events.
async fn join_selected(rpc: &mut RpcClient, current: &mut Option<String>) -> Result<()> {
    match rpc.get_selected_voice_channel().await? {
        Some(channel) => {
            print_members(&channel);
            rpc.subscribe_channel(&channel.id).await?;
            *current = Some(channel.id);
        }
        None => println!("NOT_IN_VOICE"),
    }
    Ok(())
}

fn print_members(channel: &VoiceChannel) {
    println!(
        "JOINED channel={} name={:?} members={}",
        channel.id,
        channel.name.as_deref().unwrap_or(""),
        channel.voice_states.len()
    );
    for m in &channel.voice_states {
        println!("  MEMBER {}", member_line(m));
    }
}

fn member_line(m: &VoiceMember) -> String {
    let flags = m.voice_state.clone().unwrap_or_default();
    format!(
        "user_id={} name={:?} mute={} deaf={}",
        m.user.id,
        m.display_name(),
        flags.mute || flags.self_mute,
        flags.deaf || flags.self_deaf
    )
}
