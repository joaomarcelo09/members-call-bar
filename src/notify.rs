//! Tells Waybar to re-run `discord-voice slot <n>` for changed slots.
//!
//! Waybar's `signal: N` option makes a custom module refresh on
//! `SIGRTMIN+N`. Slot `n` uses `N = signal_base + n` (setting, default 8),
//! so the sample config sets `signal` to 8..=13 for slots 0..=5 (5 member
//! slots plus overflow). Changes are coalesced for
//! [`COALESCE`] and limited to about [`MAX_SIGNALS_PER_SEC`] signals per
//! second overall; errors are ignored.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::output::MAX_SLOT_FILES;

/// Changes arriving within this window are sent as one batch.
pub const COALESCE: Duration = Duration::from_millis(50);
/// Overall signal budget (token bucket rate and burst size).
pub const MAX_SIGNALS_PER_SEC: u32 = 20;

/// Delivers refresh signals for a batch of slots. Must not block.
pub trait SignalSender: Send + 'static {
    fn send(&self, slots: &[usize]);
}

/// Signals every `waybar` process of this user (best effort).
pub struct WaybarSignaler {
    /// Waybar `signal` value of slot 0.
    pub signal_base: i32,
}

impl SignalSender for WaybarSignaler {
    fn send(&self, slots: &[usize]) {
        let slots = slots.to_vec();
        let base = self.signal_base;
        // Scanning /proc is blocking file I/O: keep it off the event loop.
        tokio::task::spawn_blocking(move || signal_waybar(base, &slots));
    }
}

/// Sends `SIGRTMIN + signal_base + slot` to every process named `waybar`.
/// Blocking but quick; ignores all errors.
pub fn signal_waybar(signal_base: i32, slots: &[usize]) {
    let signals: Vec<i32> = slots
        .iter()
        .filter(|&&s| s < MAX_SLOT_FILES)
        .map(|&s| libc::SIGRTMIN() + signal_base + s as i32)
        .filter(|&sig| sig <= libc::SIGRTMAX())
        .collect();
    if signals.is_empty() {
        return;
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        let is_waybar = std::fs::read_to_string(entry.path().join("comm"))
            .is_ok_and(|comm| comm.trim_end() == "waybar");
        if is_waybar {
            for &sig in &signals {
                // SAFETY: kill(2) has no memory-safety preconditions.
                unsafe { libc::kill(pid, sig) };
            }
            tracing::debug!(pid, ?slots, "signalled waybar");
        }
    }
}

/// Token bucket: `capacity` burst, refilled at `rate` tokens per second.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    rate: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(per_sec: u32, now: Instant) -> Self {
        let n = f64::from(per_sec.max(1));
        Self {
            capacity: n,
            rate: n,
            tokens: n,
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last = now;
    }

    /// Takes one token if available.
    pub fn try_take(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Time until a token is available.
    pub fn wait_time(&mut self, now: Instant) -> Duration {
        self.refill(now);
        Duration::from_secs_f64(((1.0 - self.tokens) / self.rate).max(0.0))
    }
}

/// Handle to the notifier task. Dropping it stops the task after it
/// flushes pending slots.
pub struct Notifier {
    tx: mpsc::UnboundedSender<usize>,
}

impl Notifier {
    /// Spawns the notifier task on the current tokio runtime.
    pub fn spawn(sender: impl SignalSender) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(rx, sender));
        Self { tx }
    }

    /// Marks `slot` as changed. Never blocks.
    pub fn notify(&self, slot: usize) {
        let _ = self.tx.send(slot);
    }
}

async fn run(mut rx: mpsc::UnboundedReceiver<usize>, sender: impl SignalSender) {
    let mut bucket = TokenBucket::new(MAX_SIGNALS_PER_SEC, Instant::now());
    let mut pending = [false; MAX_SLOT_FILES];
    let mut open = true;
    loop {
        if !pending.contains(&true) {
            if !open {
                return;
            }
            match rx.recv().await {
                Some(slot) if slot < MAX_SLOT_FILES => pending[slot] = true,
                Some(_) => continue,
                None => return,
            }
        }
        tokio::time::sleep(COALESCE).await;
        loop {
            match rx.try_recv() {
                Ok(slot) if slot < MAX_SLOT_FILES => pending[slot] = true,
                Ok(_) => {}
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    open = false;
                    break;
                }
            }
        }
        let now = Instant::now();
        let mut batch = Vec::new();
        for (slot, flag) in pending.iter_mut().enumerate() {
            if *flag && bucket.try_take(now) {
                *flag = false;
                batch.push(slot);
            }
        }
        if !batch.is_empty() {
            sender.send(&batch);
        }
        if pending.contains(&true) {
            // Over budget: wait for a token (the coalesce sleep follows).
            tokio::time::sleep(bucket.wait_time(now).saturating_sub(COALESCE)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const SLOTS: usize = 5;

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Vec<usize>>>>);
    impl SignalSender for Recorder {
        fn send(&self, slots: &[usize]) {
            self.0.lock().unwrap().push(slots.to_vec());
        }
    }
    impl Recorder {
        fn batches(&self) -> Vec<Vec<usize>> {
            self.0.lock().unwrap().clone()
        }
    }

    #[test]
    fn bucket_limits_rate() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(20, t0);
        for _ in 0..20 {
            assert!(b.try_take(t0));
        }
        assert!(!b.try_take(t0));
        assert_eq!(b.wait_time(t0), Duration::from_millis(50));
        assert!(!b.try_take(t0 + Duration::from_millis(40)));
        assert!(b.try_take(t0 + Duration::from_millis(51)));
        // Refill is capped at the burst size.
        let later = t0 + Duration::from_secs(10);
        for _ in 0..20 {
            assert!(b.try_take(later));
        }
        assert!(!b.try_take(later));
    }

    #[tokio::test(start_paused = true)]
    async fn coalesces_changes_per_slot() {
        let rec = Recorder::default();
        let n = Notifier::spawn(rec.clone());
        n.notify(1);
        n.notify(0);
        n.notify(1);
        n.notify(MAX_SLOT_FILES); // out of range: ignored
        tokio::time::sleep(Duration::from_millis(40)).await;
        n.notify(0);
        assert!(rec.batches().is_empty());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(rec.batches(), vec![vec![0, 1]]);

        n.notify(2);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(rec.batches(), vec![vec![0, 1], vec![2]]);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_overall() {
        let rec = Recorder::default();
        let n = Notifier::spawn(rec.clone());
        // 10 rounds of all slots in quick succession: 50 signals wanted.
        for _ in 0..10 {
            for s in 0..SLOTS {
                n.notify(s);
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        // ~600 ms elapsed: at most burst (20) + 0.6 s * 20/s = 32 signals.
        let sent: usize = rec.batches().iter().map(Vec::len).sum();
        assert!(sent <= 32, "sent {sent}");
        assert!(sent >= 20, "sent {sent}");
        // Pending slots are eventually delivered.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let last = rec.batches().concat();
        for s in 0..SLOTS {
            assert!(last.contains(&s));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn flushes_on_drop() {
        let rec = Recorder::default();
        let n = Notifier::spawn(rec.clone());
        n.notify(3);
        drop(n);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(rec.batches(), vec![vec![3]]);
    }
}
