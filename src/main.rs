//! `discord-voice`: shows the members of the current Discord voice call in
//! Waybar, using the local Discord client's RPC socket.

mod api;
mod auth;
mod avatars;
mod config;
mod daemon;
mod focus;
mod ipc;
mod layout;
mod notify;
mod output;
mod popup;
mod render;
mod rpc;
mod state;
mod supervisor;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::json;
use tracing_subscriber::EnvFilter;

use tokio::sync::watch;

use crate::avatars::{AvatarCache, Fetcher, HttpFetcher};
use crate::rpc::{RpcClient, User, VoiceChannel, VoiceEvent, VoiceMember};
use crate::state::{MemberState, Snapshot, StateTracker};
use crate::supervisor::{Backoff, Connector};

/// Limit for reaching Discord's socket and getting READY (not for
/// AUTHORIZE, which waits for the user).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const USAGE: &str =
    "usage: discord-voice [events|state|run|slot <n>|status|focus|watch|popup [toggle]]

Commands:
  events    connect to Discord and print voice events (default)
  state     track the call and print its state (members, flags, avatar
            files) whenever it changes
  run       daemon: render call members into Waybar slot files
            ($XDG_RUNTIME_DIR/discord-voice/) and signal Waybar;
            reconnects automatically when Discord restarts
  slot <n>  print slot n for a Waybar `image` module (0..slots-1: members,
            slot `slots`: \"+K\" overflow): image path and tooltip, or
            nothing when the slot is empty
  status    tell whether the daemon runs and what call it shows
  focus     run `focus_command` (Waybar on-click) to raise Discord
  watch     print the daemon's local JSON API stream (one line per change)
  popup [toggle]
            show the call in a popup panel (needs the `popup` build
            feature, otherwise it runs `focus`); `toggle` closes it
            instead if it is already open

Config: $XDG_CONFIG_HOME/discord-voice/config.toml (see README.md).
Logging: set RUST_LOG (e.g. RUST_LOG=debug); logs go to stderr.";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Waybar runs `slot` on every refresh: no logging setup, no runtime.
    match args.first().map(String::as_str) {
        Some("slot") => {
            print_slot(args.get(1).map(String::as_str));
            return Ok(());
        }
        Some("status") => {
            let report = match output::default_dir() {
                Ok(dir) => output::status_report(&dir),
                Err(_) => "not running".to_owned(),
            };
            println!("{report}");
            return Ok(());
        }
        Some("focus") => {
            // Never blocks, never fails: the command runs detached.
            let settings = load_settings_lenient();
            focus::spawn_detached(&settings.focus_command);
            return Ok(());
        }
        Some("watch") => {
            let settings = load_settings_lenient();
            return api::watch(&api::socket_path(&settings)?);
        }
        Some("popup") => {
            let toggle = match args.get(1).map(String::as_str) {
                None => false,
                Some("toggle") => true,
                Some(other) => bail!("unknown popup argument `{other}`\n\n{USAGE}"),
            };
            return popup::run(&load_settings_lenient(), toggle);
        }
        _ => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async_main(args))
}

/// Settings for commands that need no credentials.
fn load_settings_lenient() -> config::Settings {
    config::config_path()
        .map(|p| config::load_settings_or_default(&p))
        .unwrap_or_default()
}

async fn async_main(args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        None | Some("events") => run_events().await,
        Some("state") => run_state().await,
        Some("run") => run_daemon().await,
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

/// Credentials, settings and where the token lives.
struct Setup {
    cfg: auth::Config,
    settings: config::Settings,
    token_path: PathBuf,
}

fn load_setup() -> Result<Setup> {
    let path = config::config_path()?;
    let (cfg, settings) = config::load(&path)?;
    Ok(Setup {
        cfg,
        settings,
        token_path: auth::config_dir()?.join("token.json"),
    })
}

/// Connects, handshakes and authenticates; returns the client and own user.
async fn connect(
    cfg: &auth::Config,
    socket_path: Option<&Path>,
    token_path: &Path,
) -> Result<(RpcClient, User)> {
    let conn = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let (mut conn, path) = ipc::IpcConnection::connect(socket_path).await?;
        tracing::info!(socket = %path.display(), "connected");
        conn.handshake(&cfg.client_id).await?;
        anyhow::Ok(conn)
    })
    .await
    .map_err(|_| anyhow::anyhow!("Discord did not answer the handshake in time"))??;

    let mut rpc = RpcClient::new(conn);
    let user = auth::authenticate(&mut rpc, cfg, token_path).await?;
    tracing::info!(user_id = %user.id, "authenticated");
    Ok((rpc, user))
}

async fn connect_setup(setup: &Setup) -> Result<(RpcClient, User)> {
    connect(
        &setup.cfg,
        setup.settings.socket_path.as_deref(),
        &setup.token_path,
    )
    .await
}

async fn run_events() -> Result<()> {
    let setup = load_setup()?;
    let (mut rpc, _user) = connect_setup(&setup).await?;

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
    let setup = load_setup()?;
    let (mut rpc, user) = connect_setup(&setup).await?;
    let (mut tracker, rx) = StateTracker::new();
    tracker.set_own_user_id(user.id);

    let cache = AvatarCache::new(avatars::default_cache_dir()?, HttpFetcher::new()?);
    let printer = tokio::spawn(print_states(rx, cache));

    let result = tokio::select! {
        r = track(&mut rpc, &mut tracker) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    printer.abort();
    result
}

/// `slot <n>`: prints the slot or nothing. Never fails, never blocks on
/// anything but two small local files.
fn print_slot(arg: Option<&str>) {
    let text = arg
        .and_then(|n| n.parse::<usize>().ok())
        .zip(output::default_dir().ok())
        .and_then(|(n, dir)| output::read_slot(&dir, n));
    if let Some(text) = text {
        // Ignore errors such as a closed pipe.
        let _ = std::io::stdout().lock().write_all(text.as_bytes());
    }
}

/// The daemon's real connector: one Discord RPC session per call.
struct RpcConnector {
    setup: Setup,
}

impl Connector for RpcConnector {
    async fn session(
        &mut self,
        tracker: &mut StateTracker,
        link: &watch::Sender<bool>,
    ) -> Result<()> {
        let (mut rpc, user) = connect_setup(&self.setup).await?;
        tracker.set_own_user_id(user.id);
        link.send_replace(true);
        track(&mut rpc, tracker).await
    }
}

/// `run`: tracks the call and renders it into Waybar slot files until
/// Ctrl-C/SIGTERM. Connection errors are retried with backoff (slots are
/// emptied while disconnected); slots are emptied on exit too.
async fn run_daemon() -> Result<()> {
    // Config errors are fatal; connection errors are retried.
    let setup = load_setup()?;
    let settings = setup.settings.clone();
    // Opening the writer also clears slots left by a crashed run.
    let writer = output::SlotWriter::open(output::default_dir()?, settings.slot_files())?;
    tracing::info!(dir = %writer.dir().display(), slots = settings.slots, "writing Waybar slots");
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let (mut tracker, rx) = StateTracker::new();
    let (link_tx, link_rx) = watch::channel(false);
    let cache = AvatarCache::new(avatars::default_cache_dir()?, HttpFetcher::new()?);
    let notifier = notify::Notifier::spawn(notify::WaybarSignaler {
        signal_base: settings.signal_base,
    });
    let api = settings
        .api
        .then(|| start_api(&settings, rx.clone(), link_rx.clone()))
        .flatten();
    let renderer = tokio::spawn(daemon::run(rx, link_rx, cache, writer, notifier, settings));

    let mut connector = RpcConnector { setup };
    tokio::select! {
        () = supervisor::supervise(&mut connector, &mut tracker, &link_tx, Backoff::default()) => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    tracing::info!("shutting down");
    // Dropping the tracker closes the snapshot channel: the renderer clears
    // the slots and exits.
    drop(tracker);
    drop(link_tx);
    if tokio::time::timeout(Duration::from_secs(2), renderer)
        .await
        .is_err()
    {
        tracing::warn!("renderer did not stop in time");
    }
    if let Some((task, guard)) = api {
        if tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .is_err()
        {
            tracing::warn!("API did not stop in time");
        }
        // Removes the socket.
        drop(guard);
    }
    Ok(())
}

/// Binds the local API socket and serves it. Failures only disable the API.
fn start_api(
    settings: &config::Settings,
    rx: watch::Receiver<Snapshot>,
    link: watch::Receiver<bool>,
) -> Option<(tokio::task::JoinHandle<()>, api::SocketGuard)> {
    let started = (|| {
        let path = api::socket_path(settings)?;
        let (listener, guard) = api::bind(&path)?;
        let cache = AvatarCache::new(avatars::default_cache_dir()?, HttpFetcher::new()?);
        anyhow::Ok((listener, guard, cache))
    })();
    match started {
        Ok((listener, guard, cache)) => {
            tracing::info!(socket = %guard.path().display(), "serving local API");
            Some((tokio::spawn(api::serve(listener, rx, link, cache)), guard))
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "local API disabled");
            None
        }
    }
}

/// RPC event loop feeding the tracker until the connection fails.
async fn track(rpc: &mut RpcClient, tracker: &mut StateTracker) -> Result<()> {
    rpc.subscribe("VOICE_CHANNEL_SELECT", json!({})).await?;
    let mut current: Option<String> = None;
    load_selected(rpc, tracker, &mut current).await?;

    loop {
        let event = rpc.next_event().await?;
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
