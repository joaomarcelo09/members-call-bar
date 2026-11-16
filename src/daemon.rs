//! `run` command renderer: turns call snapshots into Waybar slot files.
//!
//! Avatars are resolved by a separate task, so a speaking-ring change never
//! waits on a download: members whose avatar is not loaded yet are drawn
//! with a placeholder and redrawn when it arrives.
//!
//! The renderer lives as long as the daemon, across reconnects: the `link`
//! channel tells whether Discord is connected (written to the status file).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::avatars::{AvatarCache, Fetcher};
use crate::config::Settings;
use crate::layout;
use crate::notify::{self, Notifier};
use crate::output::{self, SlotContent, SlotView, SlotWriter, Status};
use crate::render::{self, Style};
use crate::state::{MemberState, Snapshot};

struct AvatarRequest {
    user_id: String,
    hash: Option<String>,
    discriminator: Option<String>,
}

struct AvatarReply {
    user_id: String,
    hash: Option<String>,
    /// `None`: neither the avatar nor the default avatar is available.
    bytes: Option<Arc<[u8]>>,
}

/// Avatar state of one user, keyed by the avatar hash it was loaded for.
struct LoadedAvatar {
    hash: Option<String>,
    /// `None` while loading, or if loading failed.
    bytes: Option<Arc<[u8]>>,
    loading: bool,
}

/// Renders snapshots until the snapshot or link sender is dropped, then
/// empties every slot, signals Waybar and removes the status file.
pub async fn run<F>(
    mut rx: watch::Receiver<Snapshot>,
    mut link: watch::Receiver<bool>,
    cache: AvatarCache<F>,
    writer: SlotWriter,
    notifier: Notifier,
    settings: Settings,
) where
    F: Fetcher + Send + Sync + 'static,
{
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
    let resolver = tokio::spawn(resolve_avatars(cache, req_rx, reply_tx));

    let mut renderer = Renderer::new(writer, req_tx, notifier, settings);
    renderer.connected = *link.borrow_and_update();
    renderer.set_snapshot(rx.borrow_and_update().clone());
    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    break;
                }
                renderer.set_snapshot(rx.borrow_and_update().clone());
            }
            changed = link.changed() => {
                if changed.is_err() {
                    break;
                }
                renderer.connected = *link.borrow_and_update();
                renderer.sync();
            }
            Some(reply) = reply_rx.recv() => renderer.avatar_loaded(reply),
        }
    }
    resolver.abort();
    renderer.shutdown();
    tracing::info!("renderer stopped, slots cleared");
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
            bytes: avatar.map(|a| a.bytes),
        };
        if replies.send(reply).is_err() {
            return;
        }
    }
}

struct Renderer {
    writer: SlotWriter,
    requests: mpsc::UnboundedSender<AvatarRequest>,
    notifier: Notifier,
    settings: Settings,
    avatars: HashMap<String, LoadedAvatar>,
    status: Option<Status>,
    connected: bool,
    /// Last snapshot, re-rendered when an avatar arrives.
    snap: Snapshot,
    /// Snapshot counter, the clock of `last_spoke`.
    tick: u64,
    /// Tick of the last snapshot in which each member was speaking.
    last_spoke: HashMap<String, u64>,
}

impl Renderer {
    fn new(
        writer: SlotWriter,
        requests: mpsc::UnboundedSender<AvatarRequest>,
        notifier: Notifier,
        settings: Settings,
    ) -> Self {
        Self {
            writer,
            requests,
            notifier,
            settings,
            avatars: HashMap::new(),
            status: None,
            connected: false,
            snap: Snapshot::default(),
            tick: 0,
            last_spoke: HashMap::new(),
        }
    }

    fn set_snapshot(&mut self, snap: Snapshot) {
        self.tick += 1;
        self.last_spoke
            .retain(|id, _| snap.members.iter().any(|m| &m.user_id == id));
        for m in snap.members.iter().filter(|m| m.speaking) {
            self.last_spoke.insert(m.user_id.clone(), self.tick);
        }
        self.snap = snap;
        self.sync();
    }

    /// Brings status and slot files to the current snapshot.
    fn sync(&mut self) {
        let snap = self.snap.clone();
        self.write_status(&snap);
        if snap.channel.is_none() {
            // Do not keep avatar bytes around outside a call.
            self.avatars.retain(|_, a| a.loading);
        }
        let slots = self.settings.slots;
        let assignment = layout::assign(
            &snap.members,
            snap.own_user_id.as_deref(),
            self.settings.show_self,
            self.settings.sort,
            &self.last_spoke,
            slots,
        );
        let mut contents: Vec<Option<SlotContent>> = Vec::with_capacity(slots + 1);
        for slot in 0..slots {
            let content = assignment
                .shown
                .get(slot)
                .map(|&i| SlotContent::Member(self.view(&snap, &snap.members[i])));
            contents.push(content);
        }
        contents.push((!assignment.hidden.is_empty()).then(|| {
            SlotContent::Overflow {
                hidden: assignment
                    .hidden
                    .iter()
                    .map(|&i| snap.members[i].name.clone())
                    .collect(),
            }
        }));

        let cfg = self.settings.render;
        for (slot, content) in contents.iter().enumerate() {
            let avatars = &self.avatars;
            let result = self.writer.update(slot, content.as_ref(), |c| match c {
                SlotContent::Member(v) => {
                    let bytes = avatars.get(&v.user_id).and_then(|a| a.bytes.as_deref());
                    render::render_slot(
                        bytes,
                        Style {
                            speaking: v.speaking,
                            dimmed: v.muted || v.deafened,
                        },
                        &cfg,
                    )
                }
                SlotContent::Overflow { hidden } => render::render_overflow(hidden.len(), &cfg),
            });
            match result {
                Ok(true) => {
                    tracing::debug!(slot, view = ?content, "slot updated");
                    self.notifier.notify(slot);
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(slot, error = %e, "cannot write slot"),
            }
        }
    }

    /// Builds the slot view, requesting the avatar if it is not loaded.
    fn view(&mut self, snap: &Snapshot, m: &MemberState) -> SlotView {
        let current = self.avatars.get(&m.user_id).filter(|a| a.hash == m.avatar);
        let loaded = match current {
            Some(a) => !a.loading,
            None => {
                self.request(m);
                false
            }
        };
        SlotView {
            user_id: m.user_id.clone(),
            name: m.name.clone(),
            own: snap.own_user_id.as_deref() == Some(m.user_id.as_str()),
            speaking: m.speaking,
            muted: m.muted(),
            deafened: m.deafened(),
            avatar: m.avatar.clone(),
            avatar_loaded: loaded,
        }
    }

    fn request(&mut self, m: &MemberState) {
        self.avatars.insert(
            m.user_id.clone(),
            LoadedAvatar {
                hash: m.avatar.clone(),
                bytes: None,
                loading: true,
            },
        );
        let _ = self.requests.send(AvatarRequest {
            user_id: m.user_id.clone(),
            hash: m.avatar.clone(),
            discriminator: m.discriminator.clone(),
        });
    }

    fn avatar_loaded(&mut self, reply: AvatarReply) {
        // Ignore stale replies (avatar changed meanwhile, or user forgotten).
        let Some(entry) = self.avatars.get_mut(&reply.user_id) else {
            return;
        };
        if entry.hash != reply.hash {
            return;
        }
        entry.bytes = reply.bytes;
        entry.loading = false;
        tracing::debug!(user_id = %reply.user_id, "avatar loaded");
        self.sync();
    }

    fn write_status(&mut self, snap: &Snapshot) {
        let status = Status {
            pid: std::process::id(),
            connected: self.connected,
            channel: snap
                .channel
                .as_ref()
                .map(|c| c.name.clone().unwrap_or_else(|| c.id.clone())),
            members: snap.members.len(),
        };
        if self.status.as_ref() == Some(&status) {
            return;
        }
        match output::write_status(self.writer.dir(), &status) {
            Ok(()) => {
                tracing::info!(
                    connected = status.connected,
                    channel = status.channel.as_deref().unwrap_or("-"),
                    members = status.members,
                    "call state"
                );
                self.status = Some(status);
            }
            Err(e) => tracing::warn!(error = %e, "cannot write status file"),
        }
    }

    fn shutdown(&mut self) {
        let cleared = self.writer.clear_all();
        if !cleared.is_empty() {
            notify::signal_waybar(self.settings.signal_base, &cleared);
        }
        if let Err(e) = output::remove_status(self.writer.dir()) {
            tracing::warn!(error = %e, "cannot remove status file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::path::PathBuf;
    use std::time::Duration;

    use anyhow::Result;
    use reqwest::Url;

    use crate::notify::SignalSender;
    use crate::state::{ChannelInfo, SnapshotData};

    /// Downloads never finish.
    struct StuckFetcher;
    impl Fetcher for StuckFetcher {
        fn fetch(&self, _url: &Url) -> impl Future<Output = Result<Vec<u8>>> + Send {
            std::future::pending()
        }
    }

    struct NoSignals;
    impl SignalSender for NoSignals {
        fn send(&self, _slots: &[usize]) {}
    }

    fn member(id: &str, speaking: bool) -> MemberState {
        MemberState {
            user_id: id.into(),
            name: format!("user{id}"),
            avatar: Some("abc".into()),
            discriminator: None,
            speaking,
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
        }
    }

    fn snapshot(members: Vec<MemberState>) -> Snapshot {
        Arc::new(SnapshotData {
            channel: Some(ChannelInfo {
                id: "9".into(),
                name: Some("General".into()),
                guild_id: None,
            }),
            own_user_id: Some("1".into()),
            members,
        })
    }

    async fn wait_for(path: PathBuf, expected: &str) {
        for _ in 0..200 {
            if std::fs::read_to_string(&path).is_ok_and(|t| t == expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{} never became {expected:?}", path.display());
    }

    #[tokio::test]
    async fn speaking_updates_do_not_wait_for_avatars_and_exit_clears() {
        let base =
            std::env::temp_dir().join(format!("discord-voice-daemon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let out = base.join("out");
        let settings = Settings::default();
        let writer = SlotWriter::open(out.clone(), settings.slot_files()).unwrap();
        let cache = AvatarCache::new(base.join("avatars"), StuckFetcher);
        let (tx, rx) = watch::channel(Snapshot::default());
        let (_link_tx, link_rx) = watch::channel(true);
        let task = tokio::spawn(run(
            rx,
            link_rx,
            cache,
            writer,
            Notifier::spawn(NoSignals),
            settings,
        ));

        tx.send(snapshot(vec![member("1", false), member("2", false)]))
            .unwrap();
        wait_for(output::txt_path(&out, 0), "user1 (you)").await;
        wait_for(output::txt_path(&out, 1), "user2").await;
        assert!(output::png_path(&out, 1).is_file());
        wait_for(
            output::status_path(&out),
            &Status {
                pid: std::process::id(),
                connected: true,
                channel: Some("General".into()),
                members: 2,
            }
            .to_text(),
        )
        .await;

        tx.send(snapshot(vec![member("1", false), member("2", true)]))
            .unwrap();
        wait_for(output::txt_path(&out, 1), "user2 · speaking").await;

        // Leaving the call empties the slots.
        tx.send(Arc::new(SnapshotData::default())).unwrap();
        for _ in 0..200 {
            if !output::txt_path(&out, 0).exists() && !output::txt_path(&out, 1).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!output::png_path(&out, 0).exists());

        tx.send(snapshot(vec![member("1", true)])).unwrap();
        wait_for(output::txt_path(&out, 0), "user1 (you) · speaking").await;

        // Exit clears slots and the status file.
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn overflow_slot_hides_extra_members_and_link_state_is_reported() {
        let base = std::env::temp_dir().join(format!(
            "discord-voice-daemon-overflow-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let out = base.join("out");
        let settings = Settings {
            slots: 2,
            show_self: false,
            ..Settings::default()
        };
        let writer = SlotWriter::open(out.clone(), settings.slot_files()).unwrap();
        let cache = AvatarCache::new(base.join("avatars"), StuckFetcher);
        let (tx, rx) = watch::channel(Snapshot::default());
        let (link_tx, link_rx) = watch::channel(true);
        let task = tokio::spawn(run(
            rx,
            link_rx,
            cache,
            writer,
            Notifier::spawn(NoSignals),
            settings,
        ));

        let ids = ["1", "2", "3", "4", "5"];
        tx.send(snapshot(ids.iter().map(|id| member(id, false)).collect()))
            .unwrap();
        // Own user ("1") is excluded; two member slots, then the badge.
        wait_for(output::txt_path(&out, 0), "user2").await;
        wait_for(output::txt_path(&out, 1), "user3").await;
        wait_for(output::txt_path(&out, 2), "Also in the call: user4, user5").await;
        assert!(output::png_path(&out, 2).is_file());
        assert!(!output::txt_path(&out, 3).exists());

        // Fewer members: the badge goes away.
        tx.send(snapshot(vec![
            member("1", false),
            member("2", false),
            member("3", false),
        ]))
        .unwrap();
        for _ in 0..200 {
            if !output::txt_path(&out, 2).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!output::png_path(&out, 2).exists());

        // Disconnected: status says so.
        link_tx.send_replace(false);
        tx.send(Arc::new(SnapshotData::default())).unwrap();
        wait_for(
            output::status_path(&out),
            &Status {
                pid: std::process::id(),
                connected: false,
                channel: None,
                members: 0,
            }
            .to_text(),
        )
        .await;
        assert!(!output::txt_path(&out, 0).exists());

        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&base);
    }
}
