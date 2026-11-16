//! Local JSON API: the daemon serves the call state on a Unix socket
//! (`$XDG_RUNTIME_DIR/discord-voice/api.sock`, mode 0600).
//!
//! Protocol: newline-delimited JSON, server to client only. A client gets
//! the current state right after connecting, then one line per change
//! (identical states are not repeated, changes are coalesced to at most one
//! line per [`MIN_INTERVAL`]). Input from clients is read and ignored.
//!
//! The API never slows the daemon down: every client has a bounded queue;
//! a client whose queue is full (it does not read) or whose socket write
//! stalls is disconnected.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::avatars::{AvatarCache, Fetcher};
use crate::config::Settings;
use crate::output;
use crate::state::{MemberState, Snapshot, SnapshotData};

pub const SOCKET_NAME: &str = "api.sock";
/// Lines queued per client before it counts as too slow and is dropped.
pub const CLIENT_QUEUE: usize = 16;
/// Connections beyond this are closed right away.
pub const MAX_CLIENTS: usize = 32;
/// At most one line per interval (~20 per second).
pub const MIN_INTERVAL: Duration = Duration::from_millis(50);
/// A client whose socket accepts no data for this long is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// One line of the API.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiState {
    /// Whether the daemon is connected to Discord.
    pub connected: bool,
    /// Current voice channel; `null` outside a call.
    pub channel: Option<ApiChannel>,
    /// Own user first, then the others in join order.
    pub members: Vec<ApiMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiChannel {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiMember {
    pub user_id: String,
    pub name: String,
    pub speaking: bool,
    pub muted: bool,
    pub deafened: bool,
    /// Whether this is your own user.
    #[serde(rename = "self")]
    pub own: bool,
    /// Local PNG file of the avatar; `null` while it loads or if none is
    /// available.
    pub avatar: Option<String>,
}

impl ApiState {
    /// The line sent to clients, newline included.
    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).expect("API state serializes");
        line.push('\n');
        line
    }
}

/// Builds the API state. Outside a Discord connection the call is empty.
pub fn build_state(
    snap: &SnapshotData,
    connected: bool,
    mut avatar: impl FnMut(&MemberState) -> Option<String>,
) -> ApiState {
    if !connected {
        return ApiState::default();
    }
    ApiState {
        connected,
        channel: snap.channel.as_ref().map(|c| ApiChannel {
            id: c.id.clone(),
            name: c.name.clone(),
        }),
        members: snap
            .members
            .iter()
            .map(|m| ApiMember {
                user_id: m.user_id.clone(),
                name: m.name.clone(),
                speaking: m.speaking,
                muted: m.muted(),
                deafened: m.deafened(),
                own: snap.own_user_id.as_deref() == Some(m.user_id.as_str()),
                avatar: avatar(m),
            })
            .collect(),
    }
}

/// `api_socket`, or `$XDG_RUNTIME_DIR/discord-voice/api.sock`.
pub fn socket_path(settings: &Settings) -> Result<PathBuf> {
    match &settings.api_socket {
        Some(p) => Ok(p.clone()),
        None => Ok(output::default_dir()?.join(SOCKET_NAME)),
    }
}

/// Fan-out of state lines to the connected clients.
pub struct Hub {
    clients: Vec<mpsc::Sender<Arc<str>>>,
    current: Arc<str>,
    max_clients: usize,
}

impl Hub {
    pub fn new(initial: &ApiState) -> Self {
        Self {
            clients: Vec::new(),
            current: initial.to_line().into(),
            max_clients: MAX_CLIENTS,
        }
    }

    pub fn clients(&self) -> usize {
        self.clients.iter().filter(|c| !c.is_closed()).count()
    }

    /// Registers a client and queues the current state for it. `None` when
    /// the client limit is reached.
    pub fn subscribe(&mut self) -> Option<mpsc::Receiver<Arc<str>>> {
        self.clients.retain(|c| !c.is_closed());
        if self.clients.len() >= self.max_clients {
            return None;
        }
        let (tx, rx) = mpsc::channel(CLIENT_QUEUE);
        tx.try_send(self.current.clone())
            .expect("a new queue has room");
        self.clients.push(tx);
        Some(rx)
    }

    /// Sends `state` to every client unless it equals the last state sent.
    /// Never waits: clients with a full queue are dropped. Returns whether a
    /// line was sent.
    pub fn publish(&mut self, state: &ApiState) -> bool {
        let line = state.to_line();
        if *line == *self.current {
            return false;
        }
        self.current = line.into();
        let current = &self.current;
        self.clients.retain(|c| match c.try_send(current.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::info!("API client too slow, disconnecting it");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
        true
    }
}

/// Removes the socket file when dropped, unless another process replaced it.
#[derive(Debug)]
pub struct SocketGuard {
    path: PathBuf,
    inode: (u64, u64),
}

impl SocketGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let ours = std::fs::symlink_metadata(&self.path)
            .map(|m| (m.dev(), m.ino()) == self.inode)
            .unwrap_or(false);
        if ours {
            if let Err(e) = std::fs::remove_file(&self.path) {
                tracing::warn!(path = %self.path.display(), error = %e, "cannot remove API socket");
            }
        }
    }
}

/// Binds the API socket: creates the directory (0700) if missing, removes a
/// stale socket, and makes the socket 0600 before it becomes visible (bound
/// under a temporary name, then renamed). Must run inside a Tokio runtime.
pub fn bind(path: &Path) -> Result<(UnixListener, SocketGuard)> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .with_context(|| format!("API socket path {} has no directory", path.display()))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                bail!(
                    "{} is in use: another discord-voice daemon is running",
                    path.display()
                );
            }
            std::fs::remove_file(path)
                .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
        }
        Ok(_) => bail!(
            "{} exists and is not a socket; refusing to replace it",
            path.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", path.display())),
    }

    let name = path
        .file_name()
        .with_context(|| format!("API socket path {} has no file name", path.display()))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let listener = std::os::unix::net::UnixListener::bind(&tmp)
        .with_context(|| format!("cannot bind {}", tmp.display()))?;
    let publish = (|| -> io::Result<(u64, u64)> {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, path)?;
        let meta = std::fs::symlink_metadata(path)?;
        Ok((meta.dev(), meta.ino()))
    })();
    let inode = match publish {
        Ok(inode) => inode,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("cannot create {}", path.display()));
        }
    };
    let guard = SocketGuard {
        path: path.to_owned(),
        inode,
    };
    listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(listener)?;
    Ok((listener, guard))
}

struct AvatarRequest {
    user_id: String,
    hash: Option<String>,
    discriminator: Option<String>,
}

struct AvatarReply {
    user_id: String,
    hash: Option<String>,
    path: Option<String>,
}

/// Avatar file of one user, keyed by the avatar hash it was resolved for.
struct AvatarPath {
    hash: Option<String>,
    path: Option<String>,
    loading: bool,
}

struct Server {
    hub: Hub,
    avatars: HashMap<String, AvatarPath>,
    requests: mpsc::UnboundedSender<AvatarRequest>,
    snap: Snapshot,
    connected: bool,
}

impl Server {
    fn state(&mut self) -> ApiState {
        let snap = self.snap.clone();
        let (avatars, requests) = (&mut self.avatars, &self.requests);
        if self.connected && snap.channel.is_some() {
            avatars.retain(|id, _| snap.members.iter().any(|m| &m.user_id == id));
        }
        build_state(&snap, self.connected, |m| {
            if let Some(a) = avatars.get(&m.user_id).filter(|a| a.hash == m.avatar) {
                return a.path.clone();
            }
            avatars.insert(
                m.user_id.clone(),
                AvatarPath {
                    hash: m.avatar.clone(),
                    path: None,
                    loading: true,
                },
            );
            let _ = requests.send(AvatarRequest {
                user_id: m.user_id.clone(),
                hash: m.avatar.clone(),
                discriminator: m.discriminator.clone(),
            });
            None
        })
    }

    /// Returns whether the reply changes the published state.
    fn avatar_loaded(&mut self, reply: AvatarReply) -> bool {
        match self.avatars.get_mut(&reply.user_id) {
            Some(entry) if entry.hash == reply.hash && entry.loading => {
                entry.loading = false;
                entry.path = reply.path;
                entry.path.is_some()
            }
            _ => false,
        }
    }

    fn add_client(&mut self, stream: UnixStream) {
        match self.hub.subscribe() {
            Some(rx) => {
                tracing::debug!(clients = self.hub.clients(), "API client connected");
                tokio::spawn(client_task(stream, rx));
            }
            None => tracing::warn!("too many API clients, refusing connection"),
        }
    }
}

/// Serves the API until the snapshot or link sender is dropped. Avatars are
/// resolved by the given cache, in a separate task, so a slow download never
/// delays a line (the member shows `"avatar": null` until it is ready).
pub async fn serve<F>(
    listener: UnixListener,
    mut rx: watch::Receiver<Snapshot>,
    mut link: watch::Receiver<bool>,
    cache: AvatarCache<F>,
) where
    F: Fetcher + Send + Sync + 'static,
{
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
    let resolver = tokio::spawn(resolve_avatars(cache, req_rx, reply_tx));

    let mut server = Server {
        hub: Hub::new(&ApiState::default()),
        avatars: HashMap::new(),
        requests: req_tx,
        snap: rx.borrow_and_update().clone(),
        connected: *link.borrow_and_update(),
    };
    let initial = server.state();
    server.hub.publish(&initial);

    let mut dirty = false;
    let mut last_sent = Instant::now();
    loop {
        let flush_at = last_sent + MIN_INTERVAL;
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => server.add_client(stream),
                Err(e) => {
                    // E.g. out of file descriptors: do not spin.
                    tracing::warn!(error = %e, "cannot accept API client");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            changed = rx.changed() => {
                if changed.is_err() {
                    break;
                }
                server.snap = rx.borrow_and_update().clone();
                dirty = true;
            }
            changed = link.changed() => {
                if changed.is_err() {
                    break;
                }
                server.connected = *link.borrow_and_update();
                dirty = true;
            }
            Some(reply) = reply_rx.recv() => {
                dirty |= server.avatar_loaded(reply);
            }
            () = tokio::time::sleep_until(flush_at), if dirty => {
                let state = server.state();
                if server.hub.publish(&state) {
                    tracing::trace!(clients = server.hub.clients(), "API state sent");
                }
                last_sent = Instant::now();
                dirty = false;
            }
        }
    }
    resolver.abort();
    tracing::info!("API stopped");
}

async fn resolve_avatars<F: Fetcher>(
    mut cache: AvatarCache<F>,
    mut requests: mpsc::UnboundedReceiver<AvatarRequest>,
    replies: mpsc::UnboundedSender<AvatarReply>,
) {
    while let Some(req) = requests.recv().await {
        let avatar = cache
            .get(
                &req.user_id,
                req.hash.as_deref(),
                req.discriminator.as_deref(),
            )
            .await;
        let reply = AvatarReply {
            user_id: req.user_id,
            hash: req.hash,
            path: avatar.map(|a| a.path.to_string_lossy().into_owned()),
        };
        if replies.send(reply).is_err() {
            return;
        }
    }
}

/// Writes queued lines to one client until it disconnects, stalls, or is
/// dropped by the hub (its queue then ends).
async fn client_task(stream: UnixStream, mut lines: mpsc::Receiver<Arc<str>>) {
    let (mut read, mut write) = stream.into_split();
    let mut buf = [0u8; 512];
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                match tokio::time::timeout(WRITE_TIMEOUT, write.write_all(line.as_bytes())).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => break,
                    Err(_) => {
                        tracing::info!("API client stalled, disconnecting it");
                        break;
                    }
                }
            }
            n = read.read(&mut buf) => match n {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            },
        }
    }
    tracing::debug!("API client disconnected");
}

/// `watch`: prints the API stream of the running daemon to stdout.
pub fn watch(path: &Path) -> Result<()> {
    let mut stream = std::os::unix::net::UnixStream::connect(path).with_context(|| {
        format!(
            "cannot connect to {}: is the daemon running (`discord-voice run`, with `api = true`)?",
            path.display()
        )
    })?;
    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 8192];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => bail!("the daemon closed the connection"),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("cannot read from the daemon"),
        };
        // A closed stdout (e.g. `| head`) ends the command quietly.
        if stdout
            .write_all(&buf[..n])
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    use reqwest::Url;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, BufReader};

    use crate::state::ChannelInfo;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            // Short path: Unix socket paths are limited to 108 bytes.
            let dir = std::env::temp_dir().join(format!(
                "dv-api-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Serves a fixed body for every avatar URL.
    struct InstantFetcher;
    impl Fetcher for InstantFetcher {
        fn fetch(&self, _url: &Url) -> impl Future<Output = Result<Vec<u8>>> + Send {
            std::future::ready(Ok(b"png".to_vec()))
        }
    }

    fn member(id: &str, name: &str) -> MemberState {
        MemberState {
            user_id: id.into(),
            name: name.into(),
            avatar: None,
            discriminator: None,
            speaking: false,
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
        }
    }

    fn snapshot(members: Vec<MemberState>) -> SnapshotData {
        SnapshotData {
            channel: Some(ChannelInfo {
                id: "42".into(),
                name: Some("General".into()),
                guild_id: None,
            }),
            own_user_id: Some("1".into()),
            members,
        }
    }

    fn state_with(names: &[&str]) -> ApiState {
        let members = names
            .iter()
            .enumerate()
            .map(|(i, n)| member(&i.to_string(), n))
            .collect();
        build_state(&snapshot(members), true, |_| None)
    }

    #[test]
    fn json_shape() {
        let mut me = member("1", "me");
        me.self_mute = true;
        let mut other = member("2", "Mason \"G\"");
        other.speaking = true;
        other.server_deaf = true;
        let state = build_state(&snapshot(vec![me, other]), true, |m| {
            (m.user_id == "1").then(|| "/cache/1_abc.png".to_owned())
        });
        let line = state.to_line();
        assert!(line.ends_with('\n') && !line.trim_end().contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({
                "connected": true,
                "channel": {"id": "42", "name": "General"},
                "members": [
                    {"user_id": "1", "name": "me", "speaking": false, "muted": true,
                     "deafened": false, "self": true, "avatar": "/cache/1_abc.png"},
                    {"user_id": "2", "name": "Mason \"G\"", "speaking": true, "muted": false,
                     "deafened": true, "self": false, "avatar": null},
                ]
            })
        );
        // Round trip, as the popup reads it.
        assert_eq!(serde_json::from_str::<ApiState>(&line).unwrap(), state);
    }

    #[test]
    fn empty_outside_a_call_or_connection() {
        let idle = build_state(&SnapshotData::default(), true, |_| None);
        assert_eq!(
            serde_json::to_value(&idle).unwrap(),
            json!({"connected": true, "channel": null, "members": []})
        );
        let offline = build_state(&snapshot(vec![member("1", "me")]), false, |_| None);
        assert_eq!(
            serde_json::to_value(&offline).unwrap(),
            json!({"connected": false, "channel": null, "members": []})
        );
    }

    #[test]
    fn hub_fans_out_and_skips_duplicates() {
        let mut hub = Hub::new(&state_with(&[]));
        let mut a = hub.subscribe().unwrap();
        let mut b = hub.subscribe().unwrap();
        let initial = state_with(&[]).to_line();
        assert_eq!(&*a.try_recv().unwrap(), initial);
        assert_eq!(&*b.try_recv().unwrap(), initial);

        let next = state_with(&["me"]);
        assert!(hub.publish(&next));
        assert!(!hub.publish(&next), "identical state is not repeated");
        for rx in [&mut a, &mut b] {
            assert_eq!(&*rx.try_recv().unwrap(), next.to_line());
            assert!(rx.try_recv().is_err());
        }
        // A late client starts with the latest state.
        let mut c = hub.subscribe().unwrap();
        assert_eq!(&*c.try_recv().unwrap(), next.to_line());
        assert_eq!(hub.clients(), 3);
        drop(c);
        assert_eq!(hub.clients(), 2);
    }

    #[test]
    fn hub_drops_slow_clients_without_blocking() {
        let mut hub = Hub::new(&state_with(&[]));
        let mut fast = hub.subscribe().unwrap();
        let mut slow = hub.subscribe().unwrap();
        // `slow` never reads; its queue holds the initial line already.
        for i in 0..CLIENT_QUEUE {
            assert!(hub.publish(&state_with(&[&format!("user{i}")])));
            while fast.try_recv().is_ok() {}
        }
        assert_eq!(hub.clients(), 1, "slow client dropped");
        assert!(hub.publish(&state_with(&["last"])));
        assert_eq!(&*fast.try_recv().unwrap(), state_with(&["last"]).to_line());
        // The slow client gets what was queued, then its stream ends.
        let mut queued = 0;
        while slow.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(queued, CLIENT_QUEUE);
        assert!(matches!(
            slow.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn hub_limits_clients() {
        let mut hub = Hub::new(&ApiState::default());
        let receivers: Vec<_> = (0..MAX_CLIENTS).map(|_| hub.subscribe().unwrap()).collect();
        assert!(hub.subscribe().is_none());
        drop(receivers);
        assert!(hub.subscribe().is_some());
    }

    #[tokio::test]
    async fn bind_sets_permissions_and_replaces_stale_socket() {
        let tmp = TempDir::new();
        let path = tmp.0.join("run").join(SOCKET_NAME);
        let (listener, guard) = bind(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        // A live socket is not taken over.
        assert!(format!("{:#}", bind(&path).unwrap_err()).contains("in use"));
        // A stale one (no listener) is replaced.
        drop(listener);
        std::mem::forget(guard);
        assert!(path.exists());
        let (_listener, guard) = bind(&path).unwrap();
        assert_eq!(guard.path(), path);
        drop(guard);
        assert!(!path.exists(), "socket removed on exit");
        // Anything else is left alone.
        std::fs::write(&path, "x").unwrap();
        assert!(format!("{:#}", bind(&path).unwrap_err()).contains("not a socket"));
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, [std::ffi::OsString::from(SOCKET_NAME)]);
    }

    type Lines = tokio::io::Lines<BufReader<UnixStream>>;

    async fn next_state(lines: &mut Lines) -> ApiState {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("line in time")
            .unwrap()
            .expect("stream open");
        serde_json::from_str(&line).unwrap()
    }

    /// Reads lines until one matches.
    async fn wait_for(lines: &mut Lines, pred: impl Fn(&ApiState) -> bool) -> ApiState {
        loop {
            let state = next_state(lines).await;
            if pred(&state) {
                return state;
            }
        }
    }

    async fn connect(path: &Path) -> Lines {
        BufReader::new(UnixStream::connect(path).await.unwrap()).lines()
    }

    #[tokio::test]
    async fn serves_initial_state_and_live_updates() {
        let tmp = TempDir::new();
        let path = tmp.0.join(SOCKET_NAME);
        let (listener, guard) = bind(&path).unwrap();
        let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
        let (link_tx, link_rx) = watch::channel(false);
        let cache = AvatarCache::new(tmp.0.join("avatars"), InstantFetcher);
        let server = tokio::spawn(serve(listener, snap_rx, link_rx, cache));

        let mut a = connect(&path).await;
        assert_eq!(next_state(&mut a).await, ApiState::default());

        link_tx.send_replace(true);
        let mut me = member("1", "me");
        me.avatar = Some("abc".into());
        snap_tx.send_replace(Arc::new(snapshot(vec![me.clone(), member("2", "Mason")])));
        let state = wait_for(&mut a, |s| s.members.len() == 2).await;
        assert!(state.connected);
        assert_eq!(
            state.channel.as_ref().unwrap().name.as_deref(),
            Some("General")
        );
        assert!(state.members[0].own && !state.members[1].own);

        // Avatar paths follow once resolved.
        let state = wait_for(&mut a, |s| s.members.iter().all(|m| m.avatar.is_some())).await;
        let avatar = state.members[0].avatar.clone().unwrap();
        assert!(avatar.ends_with("1_abc.png"), "{avatar}");
        assert!(Path::new(&avatar).exists());
        assert!(state.members[1]
            .avatar
            .as_deref()
            .unwrap()
            .contains("default_"));

        // A second client gets the current state right away; both see the
        // speaking change.
        let mut b = connect(&path).await;
        assert_eq!(next_state(&mut b).await, state);
        let mut speaking = me.clone();
        speaking.speaking = true;
        snap_tx.send_replace(Arc::new(snapshot(vec![speaking, member("2", "Mason")])));
        for lines in [&mut a, &mut b] {
            let s = next_state(lines).await;
            assert!(s.members[0].speaking);
            assert_eq!(s.members[0].avatar.as_deref(), Some(avatar.as_str()));
        }

        // Losing Discord empties the call.
        link_tx.send_replace(false);
        assert_eq!(next_state(&mut a).await, ApiState::default());

        // Stopping the daemon ends the streams.
        drop(snap_tx);
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        drop(guard);
        assert!(!path.exists());
        let end = tokio::time::timeout(Duration::from_secs(5), a.next_line()).await;
        assert!(matches!(end, Ok(Ok(None))));
    }

    #[tokio::test]
    async fn coalesces_bursts() {
        let tmp = TempDir::new();
        let path = tmp.0.join(SOCKET_NAME);
        let (listener, _guard) = bind(&path).unwrap();
        let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
        let (_link_tx, link_rx) = watch::channel(true);
        let cache = AvatarCache::new(tmp.0.join("avatars"), InstantFetcher);
        tokio::spawn(serve(listener, snap_rx, link_rx, cache));

        let mut lines = connect(&path).await;
        next_state(&mut lines).await;
        for i in 0..200 {
            let mut m = member("1", "me");
            m.speaking = i % 2 == 0;
            m.name = format!("me{i}");
            snap_tx.send_replace(Arc::new(snapshot(vec![m])));
            tokio::task::yield_now().await;
        }
        let mut received = 0;
        let last = loop {
            let s = next_state(&mut lines).await;
            received += 1;
            if s.members.first().is_some_and(|m| m.name == "me199") {
                break s;
            }
        };
        assert!(!last.members[0].speaking);
        // Far fewer lines than changes, and none left over.
        assert!(received < 50, "{received} lines for 200 changes");
        let mut extra = 0;
        while let Ok(Ok(Some(_))) =
            tokio::time::timeout(Duration::from_millis(200), lines.next_line()).await
        {
            extra += 1;
        }
        assert!(extra <= 2, "{extra} lines after the final state");
    }
}
