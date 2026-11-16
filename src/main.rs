//! `discord-voice`: prints voice events from the local Discord client.

mod auth;
mod avatars;
mod ipc;
mod rpc;
mod state;

use anyhow::{bail, Result};
use serde_json::json;
use tracing_subscriber::EnvFilter;

use tokio::sync::watch;

use crate::avatars::{AvatarCache, Fetcher, HttpFetcher};
use crate::rpc::{RpcClient, User, VoiceChannel, VoiceEvent, VoiceMember};
use crate::state::{MemberState, Snapshot, StateTracker};

const USAGE: &str = "usage: discord-voice [events|state]

Commands:
  events   connect to Discord and print voice events (default)
  state    track the call and print its state (members, flags, avatar
           files) whenever it changes

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
        Some("state") => run_state().await,
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

/// Connects, handshakes and authenticates; returns the client and own user.
async fn connect() -> Result<(RpcClient, User)> {
    let dir = auth::config_dir()?;
    let cfg = auth::load_config(&dir.join("config.toml"))?;

    let (mut conn, path) = ipc::IpcConnection::connect().await?;
    tracing::info!(socket = %path.display(), "connected");
    conn.handshake(&cfg.client_id).await?;

    let mut rpc = RpcClient::new(conn);
    let user = auth::authenticate(&mut rpc, &cfg, &dir.join("token.json")).await?;
    tracing::info!(user_id = %user.id, "authenticated");
    Ok((rpc, user))
}

async fn run_events() -> Result<()> {
    let (mut rpc, _user) = connect().await?;

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

async fn run_state() -> Result<()> {
    let (mut rpc, user) = connect().await?;
    let (mut tracker, rx) = StateTracker::new();
    tracker.set_own_user_id(user.id);

    let cache = AvatarCache::new(avatars::default_cache_dir()?, HttpFetcher::new()?);
    let printer = tokio::spawn(print_states(rx, cache));

    let result = track(&mut rpc, &mut tracker).await;
    printer.abort();
    result
}

/// RPC event loop feeding the tracker until Ctrl-C.
async fn track(rpc: &mut RpcClient, tracker: &mut StateTracker) -> Result<()> {
    rpc.subscribe("VOICE_CHANNEL_SELECT", json!({})).await?;
    let mut current: Option<String> = None;
    load_selected(rpc, tracker, &mut current).await?;

    loop {
        let event = tokio::select! {
            ev = rpc.next_event() => ev?,
            _ = tokio::signal::ctrl_c() => return Ok(()),
        };
        tracker.apply(&event);
        if let VoiceEvent::ChannelSelect { channel_id, .. } = event {
            if let Some(old) = current.take() {
                if let Err(e) = rpc.unsubscribe_channel(&old).await {
                    tracing::warn!(error = %e, "unsubscribe failed");
                }
            }
            if channel_id.is_some() {
                load_selected(rpc, tracker, &mut current).await?;
            }
        }
    }
}

/// Loads the selected voice channel into the tracker and subscribes to it.
async fn load_selected(
    rpc: &mut RpcClient,
    tracker: &mut StateTracker,
    current: &mut Option<String>,
) -> Result<()> {
    match rpc.get_selected_voice_channel().await? {
        Some(channel) => {
            tracker.load_channel(&channel);
            rpc.subscribe_channel(&channel.id).await?;
            *current = Some(channel.id);
        }
        None => tracker.leave(),
    }
    Ok(())
}

/// Prints every new snapshot, resolving avatars first. Changes that arrive
/// while avatars download are coalesced by the watch channel.
async fn print_states<F: Fetcher>(mut rx: watch::Receiver<Snapshot>, mut cache: AvatarCache<F>) {
    loop {
        let snap = rx.borrow_and_update().clone();
        let mut lines = Vec::with_capacity(snap.members.len());
        for m in &snap.members {
            let avatar = cache
                .get(&m.user_id, m.avatar.as_deref(), m.discriminator.as_deref())
                .await;
            let path = avatar.map_or_else(|| "-".to_owned(), |a| a.path.display().to_string());
            let own = snap.own_user_id.as_deref() == Some(m.user_id.as_str());
            lines.push(member_state_line(m, own, &path));
        }
        match &snap.channel {
            Some(c) => println!(
                "STATE channel={} name={:?} members={}",
                c.id,
                c.name.as_deref().unwrap_or(""),
                snap.members.len()
            ),
            None => println!("STATE not_in_voice"),
        }
        for line in lines {
            println!("  {line}");
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// `no`, or who set the flag: `self`, `server` or `self+server`.
fn flag(any: bool, self_set: bool, server_set: bool) -> &'static str {
    match (any, self_set, server_set) {
        (false, _, _) => "no",
        (true, true, true) => "self+server",
        (true, false, true) => "server",
        (true, _, false) => "self",
    }
}

fn member_state_line(m: &MemberState, own: bool, avatar_path: &str) -> String {
    format!(
        "{}name={:?} user_id={} speaking={} mute={} deaf={} avatar={}",
        if own { "[me] " } else { "" },
        m.name,
        m.user_id,
        if m.speaking { "yes" } else { "no" },
        flag(m.muted(), m.self_mute, m.server_mute),
        flag(m.deafened(), m.self_deaf, m.server_deaf),
        avatar_path
    )
}
