//! Keeps the daemon connected: runs one RPC session after another, with
//! exponential backoff between attempts. Every disconnect empties the call
//! state (so Waybar slots are cleared) and marks the link as down.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;

use crate::state::StateTracker;

/// First retry delay.
pub const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
/// Longest retry delay: keeps recovery after a Discord restart under 10 s.
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Exponential backoff: `initial`, doubled on every call, capped at `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            next: initial.min(max),
        }
    }

    /// Delay before the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }

    /// Back to `initial` (after a successful connection).
    pub fn reset(&mut self) {
        self.next = self.initial.min(self.max);
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(BACKOFF_INITIAL, BACKOFF_MAX)
    }
}

/// One connection attempt and, if it succeeds, the session until it ends.
pub trait Connector {
    /// Connects, authenticates and feeds `tracker` until the connection
    /// ends. Must set `link` to `true` once connected (this resets the
    /// backoff). Returning, with or without an error, means disconnected.
    fn session<'a>(
        &'a mut self,
        tracker: &'a mut StateTracker,
        link: &'a watch::Sender<bool>,
    ) -> impl Future<Output = Result<()>> + 'a;
}

/// Runs sessions forever. The caller stops it by dropping the future
/// (e.g. in a `select!` with the shutdown signals).
pub async fn supervise<C: Connector>(
    connector: &mut C,
    tracker: &mut StateTracker,
    link: &watch::Sender<bool>,
    mut backoff: Backoff,
) {
    // Log the first failure of a streak loudly, the repeats quietly.
    let mut quiet = false;
    loop {
        let result = connector.session(tracker, link).await;
        let was_connected = link.send_replace(false);
        tracker.leave();
        if was_connected {
            backoff.reset();
            quiet = false;
        }
        let delay = backoff.next_delay();
        match &result {
            Ok(()) => tracing::warn!(?delay, "disconnected from Discord; reconnecting"),
            // Authorization failures stay visible: they repeat the prompt.
            Err(e) if !quiet || format!("{e:#}").contains("AUTHORIZE") => {
                tracing::warn!(error = %format!("{e:#}"), ?delay, "Discord connection failed; retrying")
            }
            Err(e) => tracing::debug!(error = %format!("{e:#}"), ?delay, "still not connected"),
        }
        quiet = true;
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use anyhow::bail;
    use tokio::time::Instant;

    use crate::rpc::{User, VoiceChannel, VoiceMember};
    use crate::state::Snapshot;

    #[test]
    fn backoff_doubles_caps_and_resets() {
        let mut b = Backoff::default();
        let secs: Vec<u64> = (0..6).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 5, 5, 5]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
        assert_eq!(b.next_delay(), Duration::from_secs(2));

        let mut tiny = Backoff::new(Duration::from_secs(9), Duration::from_secs(3));
        assert_eq!(tiny.next_delay(), Duration::from_secs(3));
    }

    #[derive(Clone, Copy)]
    enum Step {
        /// Connection attempt fails.
        Fail,
        /// Connects, joins a call, then the connection drops after `secs`.
        Connect { secs: u64 },
    }

    struct FakeConnector {
        steps: VecDeque<Step>,
        start: Instant,
        /// Seconds since start of every attempt.
        attempts: Vec<u64>,
        /// State seen at the start of each attempt: (link up, members).
        seen: Vec<(bool, usize)>,
        snapshots: watch::Receiver<Snapshot>,
    }

    fn channel() -> VoiceChannel {
        let member = |id: &str| VoiceMember {
            user: User {
                id: id.into(),
                username: Some(format!("user{id}")),
                global_name: None,
                avatar: None,
                discriminator: None,
            },
            nick: None,
            voice_state: None,
        };
        VoiceChannel {
            id: "9".into(),
            name: Some("General".into()),
            guild_id: None,
            voice_states: vec![member("1"), member("2")],
        }
    }

    impl Connector for FakeConnector {
        async fn session(
            &mut self,
            tracker: &mut StateTracker,
            link: &watch::Sender<bool>,
        ) -> Result<()> {
            self.attempts.push(self.start.elapsed().as_secs());
            self.seen
                .push((*link.borrow(), self.snapshots.borrow().members.len()));
            match self.steps.pop_front().unwrap_or(Step::Fail) {
                Step::Fail => bail!("no Discord IPC socket found"),
                Step::Connect { secs } => {
                    tracker.set_own_user_id("1");
                    link.send_replace(true);
                    tracker.load_channel(&channel());
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                    bail!("socket closed")
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reconnects_with_backoff_and_clears_state() {
        let (mut tracker, rx) = StateTracker::new();
        let (link, link_rx) = watch::channel(false);
        let mut connector = FakeConnector {
            steps: VecDeque::from([
                Step::Fail,
                Step::Fail,
                Step::Connect { secs: 10 },
                Step::Fail,
                Step::Fail,
                Step::Fail,
                Step::Fail,
                Step::Connect { secs: 1 },
            ]),
            start: Instant::now(),
            attempts: Vec::new(),
            seen: Vec::new(),
            snapshots: rx.clone(),
        };

        // Check the call state while connected.
        let mut observer_rx = rx.clone();
        let mut observer_link = link_rx.clone();
        let observer = tokio::spawn(async move {
            let mut max_members = 0;
            let mut link_ups = 0;
            loop {
                tokio::select! {
                    r = observer_rx.changed() => {
                        if r.is_err() { break; }
                        max_members = max_members.max(observer_rx.borrow_and_update().members.len());
                    }
                    r = observer_link.changed() => {
                        if r.is_err() { break; }
                        if *observer_link.borrow_and_update() { link_ups += 1; }
                    }
                }
            }
            (max_members, link_ups)
        });

        tokio::select! {
            () = supervise(&mut connector, &mut tracker, &link, Backoff::default()) => {
                unreachable!("supervise never returns")
            }
            () = tokio::time::sleep(Duration::from_secs(40)) => {}
        }

        // fail@0 +1s, fail@1 +2s, connect@3 (10 s), drop@13 -> reset +1s,
        // fail@14 +2s, fail@16 +4s, fail@20 +5s, fail@25 +5s, connect@30
        // (1 s), drop@31 -> +1s, fail@32 +2s, fail@34 +4s, fail@38.
        assert_eq!(
            connector.attempts,
            [0, 1, 3, 14, 16, 20, 25, 30, 32, 34, 38]
        );
        // After each drop the next attempt starts disconnected and empty.
        assert!(connector.seen.iter().all(|&s| s == (false, 0)));
        assert!(!*link.borrow());
        assert!(rx.borrow().members.is_empty());
        assert!(rx.borrow().channel.is_none());

        drop(tracker);
        drop(link);
        let (max_members, link_ups) = observer.await.unwrap();
        assert_eq!(max_members, 2);
        assert_eq!(link_ups, 2);
    }
}
