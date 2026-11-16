//! In-memory voice call state built from RPC events.
//!
//! [`CallState`] holds the current channel and its members; [`StateTracker`]
//! applies events to it and publishes a cheap-to-clone [`Snapshot`] through a
//! `tokio::sync::watch` channel whenever the visible state changes.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::watch;

use crate::rpc::{VoiceChannel, VoiceEvent, VoiceMember, VoiceStateFlags};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    pub id: String,
    pub name: Option<String>,
    pub guild_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberState {
    pub user_id: String,
    pub name: String,
    /// Avatar hash; `None` means the default avatar.
    pub avatar: Option<String>,
    pub discriminator: Option<String>,
    pub speaking: bool,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub server_mute: bool,
    pub server_deaf: bool,
}

impl MemberState {
    fn from_member(m: &VoiceMember) -> Self {
        let mut s = Self {
            user_id: m.user.id.clone(),
            name: String::new(),
            avatar: None,
            discriminator: None,
            speaking: false,
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
        };
        s.update_from(m);
        s
    }

    /// Updates identity and flags from an event payload. Missing
    /// `voice_state` keeps the current flags.
    fn update_from(&mut self, m: &VoiceMember) {
        self.name = m.display_name().to_owned();
        self.avatar = m.user.avatar.clone();
        self.discriminator = m.user.discriminator.clone();
        if let Some(flags) = &m.voice_state {
            self.set_flags(flags);
        }
    }

    fn set_flags(&mut self, f: &VoiceStateFlags) {
        self.self_mute = f.self_mute;
        self.self_deaf = f.self_deaf;
        self.server_mute = f.mute;
        self.server_deaf = f.deaf;
    }

    /// Muted by self or by the server.
    pub fn muted(&self) -> bool {
        self.self_mute || self.server_mute
    }

    pub fn deafened(&self) -> bool {
        self.self_deaf || self.server_deaf
    }
}

/// Published view of the call. Shared through an `Arc`, so cloning is cheap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotData {
    pub channel: Option<ChannelInfo>,
    pub own_user_id: Option<String>,
    /// Own user first, then the others in join order.
    pub members: Vec<MemberState>,
}

pub type Snapshot = Arc<SnapshotData>;

#[derive(Debug, Default)]
pub struct CallState {
    own_user_id: Option<String>,
    channel: Option<ChannelInfo>,
    members: HashMap<String, MemberState>,
    /// User ids in join order.
    order: Vec<String>,
}

impl CallState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_own_user_id(&mut self, id: impl Into<String>) {
        self.own_user_id = Some(id.into());
    }

    fn clear(&mut self) {
        self.channel = None;
        self.members.clear();
        self.order.clear();
    }

    /// Replaces everything with a freshly loaded channel member list.
    pub fn load_channel(&mut self, channel: &VoiceChannel) {
        self.clear();
        self.channel = Some(ChannelInfo {
            id: channel.id.clone(),
            name: channel.name.clone(),
            guild_id: channel.guild_id.clone(),
        });
        for m in &channel.voice_states {
            self.upsert(m);
        }
    }

    /// Leaves the current channel (if any) and drops all members.
    pub fn leave(&mut self) {
        self.clear();
    }

    fn upsert(&mut self, m: &VoiceMember) {
        match self.members.get_mut(&m.user.id) {
            Some(existing) => existing.update_from(m),
            None => {
                self.order.push(m.user.id.clone());
                self.members
                    .insert(m.user.id.clone(), MemberState::from_member(m));
            }
        }
    }

    fn channel_matches(&self, channel_id: Option<&str>) -> bool {
        match (channel_id, &self.channel) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(id), Some(c)) => c.id == id,
        }
    }

    /// Applies one event. Events for unknown users or for another channel
    /// are ignored.
    pub fn apply(&mut self, event: &VoiceEvent) {
        match event {
            VoiceEvent::ChannelSelect {
                channel_id,
                guild_id,
            } => match channel_id {
                None => self.clear(),
                Some(id) => {
                    if self.channel.as_ref().map(|c| &c.id) != Some(id) {
                        // Switched channel: the member list comes from a
                        // later `load_channel`.
                        self.clear();
                        self.channel = Some(ChannelInfo {
                            id: id.clone(),
                            name: None,
                            guild_id: guild_id.clone(),
                        });
                    }
                }
            },
            VoiceEvent::StateCreate(m) => {
                if self.channel.is_some() {
                    self.upsert(m);
                } else {
                    tracing::debug!("VOICE_STATE_CREATE outside a channel; ignored");
                }
            }
            VoiceEvent::StateUpdate(m) => match self.members.get_mut(&m.user.id) {
                Some(existing) => existing.update_from(m),
                None => tracing::debug!(user_id = %m.user.id, "update for unknown user; ignored"),
            },
            VoiceEvent::StateDelete(m) => {
                if self.members.remove(&m.user.id).is_some() {
                    self.order.retain(|id| id != &m.user.id);
                } else {
                    tracing::debug!(user_id = %m.user.id, "delete for unknown user; ignored");
                }
            }
            VoiceEvent::SpeakingStart {
                user_id,
                channel_id,
            } => self.set_speaking(user_id, channel_id.as_deref(), true),
            VoiceEvent::SpeakingStop {
                user_id,
                channel_id,
            } => self.set_speaking(user_id, channel_id.as_deref(), false),
            VoiceEvent::Other { .. } => {}
        }
    }

    fn set_speaking(&mut self, user_id: &str, channel_id: Option<&str>, speaking: bool) {
        if !self.channel_matches(channel_id) {
            tracing::debug!(user_id, "speaking event for another channel; ignored");
            return;
        }
        match self.members.get_mut(user_id) {
            Some(m) => m.speaking = speaking,
            None => tracing::debug!(user_id, "speaking event for unknown user; ignored"),
        }
    }

    pub fn snapshot(&self) -> SnapshotData {
        let own = self.own_user_id.as_deref();
        let own_member = own.and_then(|id| self.members.get(id));
        let others = self
            .order
            .iter()
            .filter(|id| Some(id.as_str()) != own)
            .filter_map(|id| self.members.get(id));
        SnapshotData {
            channel: self.channel.clone(),
            own_user_id: self.own_user_id.clone(),
            members: own_member.into_iter().chain(others).cloned().collect(),
        }
    }
}

/// Owns the [`CallState`] and publishes snapshots on change.
pub struct StateTracker {
    state: CallState,
    tx: watch::Sender<Snapshot>,
}

impl StateTracker {
    pub fn new() -> (Self, watch::Receiver<Snapshot>) {
        let (tx, rx) = watch::channel(Snapshot::default());
        (
            Self {
                state: CallState::new(),
                tx,
            },
            rx,
        )
    }

    pub fn set_own_user_id(&mut self, id: impl Into<String>) {
        self.state.set_own_user_id(id);
        self.publish();
    }

    pub fn load_channel(&mut self, channel: &VoiceChannel) {
        self.state.load_channel(channel);
        self.publish();
    }

    pub fn leave(&mut self) {
        self.state.leave();
        self.publish();
    }

    pub fn apply(&mut self, event: &VoiceEvent) {
        self.state.apply(event);
        self.publish();
    }

    /// Sends a new snapshot only if it differs from the current one.
    fn publish(&self) {
        let next = self.state.snapshot();
        self.tx.send_if_modified(|current| {
            if **current == next {
                false
            } else {
                *current = Arc::new(next);
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::User;

    fn member(id: &str, name: &str, flags: Option<VoiceStateFlags>) -> VoiceMember {
        VoiceMember {
            user: User {
                id: id.into(),
                username: Some(name.into()),
                global_name: None,
                avatar: None,
                discriminator: Some("0".into()),
            },
            nick: None,
            voice_state: flags.or_else(|| Some(VoiceStateFlags::default())),
        }
    }

    fn channel(id: &str, members: Vec<VoiceMember>) -> VoiceChannel {
        VoiceChannel {
            id: id.into(),
            name: Some(format!("chan-{id}")),
            guild_id: Some("g".into()),
            voice_states: members,
        }
    }

    fn ids(s: &SnapshotData) -> Vec<&str> {
        s.members.iter().map(|m| m.user_id.as_str()).collect()
    }

    fn get<'a>(s: &'a SnapshotData, id: &str) -> &'a MemberState {
        s.members.iter().find(|m| m.user_id == id).unwrap()
    }

    fn speaking(user: &str, start: bool) -> VoiceEvent {
        let user_id = user.to_owned();
        if start {
            VoiceEvent::SpeakingStart {
                user_id,
                channel_id: Some("c1".into()),
            }
        } else {
            VoiceEvent::SpeakingStop {
                user_id,
                channel_id: Some("c1".into()),
            }
        }
    }

    #[test]
    fn load_and_join_keep_join_order_with_own_user_first() {
        let mut st = CallState::new();
        st.set_own_user_id("me");
        st.load_channel(&channel(
            "c1",
            vec![member("a", "A", None), member("me", "Me", None)],
        ));
        st.apply(&VoiceEvent::StateCreate(member("b", "B", None)));
        let s = st.snapshot();
        assert_eq!(ids(&s), ["me", "a", "b"]);
        assert_eq!(s.channel.as_ref().unwrap().name.as_deref(), Some("chan-c1"));
        assert_eq!(get(&s, "b").name, "B");
    }

    #[test]
    fn own_user_joining_late_is_still_first() {
        let mut st = CallState::new();
        st.set_own_user_id("me");
        st.load_channel(&channel("c1", vec![member("a", "A", None)]));
        st.apply(&VoiceEvent::StateCreate(member("me", "Me", None)));
        assert_eq!(ids(&st.snapshot()), ["me", "a"]);
    }

    #[test]
    fn leave_removes_member_and_rejoin_goes_last() {
        let mut st = CallState::new();
        st.load_channel(&channel(
            "c1",
            vec![member("a", "A", None), member("b", "B", None)],
        ));
        st.apply(&VoiceEvent::StateDelete(member("a", "A", None)));
        assert_eq!(ids(&st.snapshot()), ["b"]);
        st.apply(&VoiceEvent::StateCreate(member("a", "A", None)));
        assert_eq!(ids(&st.snapshot()), ["b", "a"]);
    }

    #[test]
    fn mute_and_deafen_track_self_and_server_flags() {
        let mut st = CallState::new();
        st.load_channel(&channel("c1", vec![member("a", "A", None)]));
        st.apply(&VoiceEvent::StateUpdate(member(
            "a",
            "A",
            Some(VoiceStateFlags {
                self_mute: true,
                ..Default::default()
            }),
        )));
        let s = st.snapshot();
        let a = get(&s, "a");
        assert!(a.self_mute && !a.server_mute && a.muted() && !a.deafened());

        st.apply(&VoiceEvent::StateUpdate(member(
            "a",
            "A",
            Some(VoiceStateFlags {
                mute: true,
                deaf: true,
                ..Default::default()
            }),
        )));
        let s = st.snapshot();
        let a = get(&s, "a");
        assert!(!a.self_mute && a.server_mute && a.server_deaf && !a.self_deaf);
        assert!(a.muted() && a.deafened());

        st.apply(&VoiceEvent::StateUpdate(member(
            "a",
            "A",
            Some(VoiceStateFlags {
                self_mute: true,
                self_deaf: true,
                ..Default::default()
            }),
        )));
        let s = st.snapshot();
        let a = get(&s, "a");
        assert!(a.self_deaf && !a.server_deaf && a.deafened());
    }

    #[test]
    fn update_without_voice_state_keeps_flags_and_renames() {
        let mut st = CallState::new();
        st.load_channel(&channel(
            "c1",
            vec![member(
                "a",
                "A",
                Some(VoiceStateFlags {
                    self_mute: true,
                    ..Default::default()
                }),
            )],
        ));
        let mut m = member("a", "A", None);
        m.voice_state = None;
        m.nick = Some("Nick".into());
        st.apply(&VoiceEvent::StateUpdate(m));
        let s = st.snapshot();
        assert!(get(&s, "a").self_mute);
        assert_eq!(get(&s, "a").name, "Nick");
    }

    #[test]
    fn speaking_start_and_stop() {
        let mut st = CallState::new();
        st.load_channel(&channel("c1", vec![member("a", "A", None)]));
        st.apply(&speaking("a", true));
        assert!(get(&st.snapshot(), "a").speaking);
        st.apply(&speaking("a", false));
        assert!(!get(&st.snapshot(), "a").speaking);
        // Speaking survives a flags update.
        st.apply(&speaking("a", true));
        st.apply(&VoiceEvent::StateUpdate(member("a", "A", None)));
        assert!(get(&st.snapshot(), "a").speaking);
    }

    #[test]
    fn speaking_for_another_channel_is_ignored() {
        let mut st = CallState::new();
        st.load_channel(&channel("c1", vec![member("a", "A", None)]));
        st.apply(&VoiceEvent::SpeakingStart {
            user_id: "a".into(),
            channel_id: Some("other".into()),
        });
        assert!(!get(&st.snapshot(), "a").speaking);
    }

    #[test]
    fn unknown_user_events_are_ignored() {
        let mut st = CallState::new();
        st.load_channel(&channel("c1", vec![member("a", "A", None)]));
        let before = st.snapshot();
        st.apply(&speaking("ghost", true));
        st.apply(&speaking("ghost", false));
        st.apply(&VoiceEvent::StateUpdate(member("ghost", "G", None)));
        st.apply(&VoiceEvent::StateDelete(member("ghost", "G", None)));
        st.apply(&VoiceEvent::Other {
            evt: "READY".into(),
        });
        assert_eq!(st.snapshot(), before);
    }

    #[test]
    fn events_outside_a_channel_are_ignored() {
        let mut st = CallState::new();
        st.apply(&VoiceEvent::StateCreate(member("a", "A", None)));
        st.apply(&speaking("a", true));
        assert!(st.snapshot().members.is_empty());
    }

    #[test]
    fn channel_leave_clears_everything_but_own_id() {
        let mut st = CallState::new();
        st.set_own_user_id("me");
        st.load_channel(&channel(
            "c1",
            vec![member("me", "Me", None), member("a", "A", None)],
        ));
        st.apply(&VoiceEvent::ChannelSelect {
            channel_id: None,
            guild_id: None,
        });
        let s = st.snapshot();
        assert!(s.channel.is_none() && s.members.is_empty());
        assert_eq!(s.own_user_id.as_deref(), Some("me"));
    }

    #[test]
    fn channel_switch_clears_old_members() {
        let mut st = CallState::new();
        st.load_channel(&channel("c1", vec![member("a", "A", None)]));
        st.apply(&speaking("a", true));
        st.apply(&VoiceEvent::ChannelSelect {
            channel_id: Some("c2".into()),
            guild_id: Some("g".into()),
        });
        let s = st.snapshot();
        assert_eq!(s.channel.as_ref().unwrap().id, "c2");
        assert!(s.members.is_empty());
        st.load_channel(&channel("c2", vec![member("b", "B", None)]));
        let s = st.snapshot();
        assert_eq!(ids(&s), ["b"]);
        assert!(!get(&s, "b").speaking);
    }

    #[test]
    fn reselecting_same_channel_keeps_members() {
        let mut st = CallState::new();
        st.load_channel(&channel("c1", vec![member("a", "A", None)]));
        st.apply(&VoiceEvent::ChannelSelect {
            channel_id: Some("c1".into()),
            guild_id: None,
        });
        assert_eq!(ids(&st.snapshot()), ["a"]);
    }

    #[test]
    fn tracker_publishes_only_on_change() {
        let (mut tracker, mut rx) = StateTracker::new();
        assert!(!rx.has_changed().unwrap());
        tracker.load_channel(&channel("c1", vec![member("a", "A", None)]));
        assert!(rx.has_changed().unwrap());
        let snap = rx.borrow_and_update().clone();
        assert_eq!(ids(&snap), ["a"]);

        tracker.apply(&speaking("ghost", true));
        assert!(!rx.has_changed().unwrap());

        tracker.apply(&speaking("a", true));
        assert!(rx.has_changed().unwrap());
        assert!(get(&rx.borrow_and_update(), "a").speaking);
        // Old snapshot is untouched.
        assert!(!get(&snap, "a").speaking);

        tracker.leave();
        assert!(rx.borrow_and_update().members.is_empty());
    }
}
