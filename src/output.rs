//! Files read by Waybar: `$XDG_RUNTIME_DIR/discord-voice/`.
//!
//! For each slot `n` in `0..SLOTS` the daemon writes `slot<n>.png` (the
//! rendered avatar) and `slot<n>.txt` (the one-line tooltip). An empty slot
//! has neither file. A `status` file describes the running daemon. Every
//! write is atomic (temp file + rename), so readers never see partial files.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Number of member slots shown in Waybar.
pub const SLOTS: usize = 5;

/// `$XDG_RUNTIME_DIR/discord-voice`.
pub fn default_dir() -> Result<PathBuf> {
    let base = dirs::runtime_dir().ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    Ok(base.join("discord-voice"))
}

pub fn png_path(dir: &Path, slot: usize) -> PathBuf {
    dir.join(format!("slot{slot}.png"))
}

pub fn txt_path(dir: &Path, slot: usize) -> PathBuf {
    dir.join(format!("slot{slot}.txt"))
}

pub fn status_path(dir: &Path) -> PathBuf {
    dir.join("status")
}

/// Everything that decides how a slot looks. A slot is re-rendered and
/// rewritten only when its view changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotView {
    pub user_id: String,
    pub name: String,
    pub own: bool,
    pub speaking: bool,
    pub muted: bool,
    pub deafened: bool,
    /// Avatar hash (`None`: default avatar).
    pub avatar: Option<String>,
    /// Whether the avatar bytes were available when rendering.
    pub avatar_loaded: bool,
}

/// One-line tooltip, e.g. `Mason (you) · muted · speaking`. Waybar parses
/// tooltips as Pango markup, so the name is escaped.
pub fn tooltip(view: &SlotView) -> String {
    let mut out = String::with_capacity(view.name.len() + 16);
    for c in view.name.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    if view.own {
        out.push_str(" (you)");
    }
    for (on, label) in [
        (view.muted, "muted"),
        (view.deafened, "deafened"),
        (view.speaking, "speaking"),
    ] {
        if on {
            out.push_str(" · ");
            out.push_str(label);
        }
    }
    out
}

/// Writes slot files, skipping slots whose view did not change.
pub struct SlotWriter {
    dir: PathBuf,
    last: [Option<SlotView>; SLOTS],
}

impl SlotWriter {
    /// Creates the directory (mode 0700) and removes stale slot files left
    /// by a previous run.
    pub fn open(dir: PathBuf) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot set permissions on {}", dir.display()))?;
        let mut writer = Self {
            dir,
            last: Default::default(),
        };
        writer.clear_all();
        Ok(writer)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Brings slot `slot` to `view` (`None` empties it). `render` produces
    /// the PNG and is called only if the view changed. Returns whether the
    /// files changed.
    pub fn update(
        &mut self,
        slot: usize,
        view: Option<&SlotView>,
        render: impl FnOnce(&SlotView) -> Vec<u8>,
    ) -> io::Result<bool> {
        if slot >= SLOTS || self.last[slot].as_ref() == view {
            return Ok(false);
        }
        // Forget the old state first: on error the slot is retried next time.
        self.last[slot] = None;
        match view {
            Some(v) => {
                // PNG first: a reader that finds the tooltip finds the image.
                write_atomic(&png_path(&self.dir, slot), &render(v))?;
                write_atomic(&txt_path(&self.dir, slot), tooltip(v).as_bytes())?;
                self.last[slot] = Some(v.clone());
            }
            None => remove_slot(&self.dir, slot)?,
        }
        Ok(true)
    }

    /// Empties every slot; returns the slots that were not empty.
    pub fn clear_all(&mut self) -> Vec<usize> {
        let mut changed = Vec::new();
        for slot in 0..SLOTS {
            let had = self.last[slot].take().is_some();
            let existed = txt_path(&self.dir, slot).exists() || png_path(&self.dir, slot).exists();
            if let Err(e) = remove_slot(&self.dir, slot) {
                tracing::warn!(slot, error = %e, "cannot remove slot files");
            }
            if had || existed {
                changed.push(slot);
            }
        }
        changed
    }
}

fn remove_slot(dir: &Path, slot: usize) -> io::Result<()> {
    // Tooltip first: readers check it to decide whether the slot is shown.
    remove_if_exists(&txt_path(dir, slot))?;
    remove_if_exists(&png_path(dir, slot))
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Writes `bytes` to a temp file in the same directory, then renames it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no file name"))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let result = fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(bytes))
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// What `discord-voice slot <n>` prints: `<png path>\n<tooltip>\n`, or
/// `None` if the slot is empty or anything fails.
pub fn read_slot(dir: &Path, slot: usize) -> Option<String> {
    if slot >= SLOTS {
        return None;
    }
    let tooltip = fs::read_to_string(txt_path(dir, slot)).ok()?;
    let tooltip = tooltip.lines().next().unwrap_or("").trim();
    let png = png_path(dir, slot);
    if tooltip.is_empty() || !png.is_file() {
        return None;
    }
    Some(format!("{}\n{}\n", png.display(), tooltip))
}

/// Daemon status, stored as `key=value` lines in the `status` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub pid: u32,
    pub connected: bool,
    /// Channel name (or id) when in a call.
    pub channel: Option<String>,
    pub members: usize,
}

impl Status {
    pub fn to_text(&self) -> String {
        let channel: String = self
            .channel
            .as_deref()
            .unwrap_or("")
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        format!(
            "pid={}\nconnected={}\nchannel={}\nmembers={}\n",
            self.pid, self.connected, channel, self.members
        )
    }

    /// Parses [`Status::to_text`] output; `None` if `pid` is missing or
    /// invalid. Unknown keys are ignored.
    pub fn parse(text: &str) -> Option<Self> {
        let mut pid = None;
        let mut status = Self {
            pid: 0,
            connected: false,
            channel: None,
            members: 0,
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "pid" => pid = value.trim().parse().ok(),
                "connected" => status.connected = value.trim() == "true",
                "channel" => {
                    status.channel = Some(value.trim().to_owned()).filter(|c| !c.is_empty())
                }
                "members" => status.members = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
        status.pid = pid?;
        Some(status)
    }

    /// Human-readable description, given whether the pid is alive.
    pub fn describe(&self, alive: bool) -> String {
        if !alive {
            return "not running (stale status file)".to_owned();
        }
        let conn = if self.connected {
            "connected"
        } else {
            "not connected"
        };
        match &self.channel {
            Some(c) => format!(
                "running (pid {}), {conn}, in call \"{c}\" with {} member(s)",
                self.pid, self.members
            ),
            None => format!("running (pid {}), {conn}, not in a call", self.pid),
        }
    }
}

pub fn write_status(dir: &Path, status: &Status) -> io::Result<()> {
    write_atomic(&status_path(dir), status.to_text().as_bytes())
}

pub fn remove_status(dir: &Path) -> io::Result<()> {
    remove_if_exists(&status_path(dir))
}

/// Text printed by `discord-voice status`.
pub fn status_report(dir: &Path) -> String {
    let Ok(text) = fs::read_to_string(status_path(dir)) else {
        return "not running".to_owned();
    };
    match Status::parse(&text) {
        Some(s) => s.describe(Path::new(&format!("/proc/{}", s.pid)).exists()),
        None => "not running (invalid status file)".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "discord-voice-output-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&p);
            Self(p)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn view(name: &str) -> SlotView {
        SlotView {
            user_id: "1".into(),
            name: name.into(),
            own: false,
            speaking: false,
            muted: false,
            deafened: false,
            avatar: None,
            avatar_loaded: true,
        }
    }

    #[test]
    fn tooltip_lists_state() {
        assert_eq!(tooltip(&view("Mason")), "Mason");
        let v = SlotView {
            own: true,
            muted: true,
            deafened: true,
            speaking: true,
            ..view("A\nB")
        };
        assert_eq!(tooltip(&v), "A B (you) · muted · deafened · speaking");
        assert_eq!(tooltip(&view("<b>&")), "&lt;b&gt;&amp;");
    }

    #[test]
    fn open_creates_private_dir_and_removes_stale_files() {
        let tmp = TmpDir::new("open");
        let dir = tmp.0.join("discord-voice");
        fs::create_dir_all(&dir).unwrap();
        fs::write(txt_path(&dir, 2), "old").unwrap();
        fs::write(png_path(&dir, 2), "old").unwrap();
        let w = SlotWriter::open(dir.clone()).unwrap();
        let mode = fs::metadata(w.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(!txt_path(&dir, 2).exists());
        assert!(!png_path(&dir, 2).exists());
    }

    #[test]
    fn writes_only_changed_slots_and_empties() {
        let tmp = TmpDir::new("update");
        let mut w = SlotWriter::open(tmp.0.clone()).unwrap();
        let renders = Cell::new(0);
        let render = |_: &SlotView| {
            renders.set(renders.get() + 1);
            b"png".to_vec()
        };

        assert!(w.update(0, Some(&view("A")), render).unwrap());
        assert_eq!(renders.get(), 1);
        assert_eq!(fs::read(png_path(&tmp.0, 0)).unwrap(), b"png");
        assert_eq!(fs::read_to_string(txt_path(&tmp.0, 0)).unwrap(), "A");
        assert_eq!(
            read_slot(&tmp.0, 0).unwrap(),
            format!("{}\nA\n", png_path(&tmp.0, 0).display())
        );

        // Same view: no render, no write.
        assert!(!w.update(0, Some(&view("A")), render).unwrap());
        assert_eq!(renders.get(), 1);

        let speaking = SlotView {
            speaking: true,
            ..view("A")
        };
        assert!(w.update(0, Some(&speaking), render).unwrap());
        assert_eq!(renders.get(), 2);
        assert_eq!(
            fs::read_to_string(txt_path(&tmp.0, 0)).unwrap(),
            "A · speaking"
        );

        assert!(w.update(0, None, render).unwrap());
        assert!(!txt_path(&tmp.0, 0).exists());
        assert!(!png_path(&tmp.0, 0).exists());
        assert_eq!(read_slot(&tmp.0, 0), None);
        assert!(!w.update(0, None, render).unwrap());

        // Out of range is ignored.
        assert!(!w.update(SLOTS, Some(&view("A")), render).unwrap());
        // No temp files left behind.
        assert_eq!(fs::read_dir(&tmp.0).unwrap().count(), 0);
    }

    #[test]
    fn clear_all_reports_non_empty_slots() {
        let tmp = TmpDir::new("clear");
        let mut w = SlotWriter::open(tmp.0.clone()).unwrap();
        w.update(1, Some(&view("A")), |_| vec![1]).unwrap();
        w.update(3, Some(&view("B")), |_| vec![1]).unwrap();
        assert_eq!(w.clear_all(), vec![1, 3]);
        assert_eq!(fs::read_dir(&tmp.0).unwrap().count(), 0);
        assert!(w.clear_all().is_empty());
    }

    #[test]
    fn read_slot_handles_missing_and_bad_input() {
        let tmp = TmpDir::new("read");
        assert_eq!(read_slot(&tmp.0, 0), None); // no dir
        fs::create_dir_all(&tmp.0).unwrap();
        fs::write(txt_path(&tmp.0, 0), "A").unwrap();
        assert_eq!(read_slot(&tmp.0, 0), None); // no png
        fs::write(png_path(&tmp.0, 0), "png").unwrap();
        assert!(read_slot(&tmp.0, 0).is_some());
        assert_eq!(read_slot(&tmp.0, SLOTS), None);
        fs::write(txt_path(&tmp.0, 0), "").unwrap();
        assert_eq!(read_slot(&tmp.0, 0), None); // empty tooltip
    }

    #[test]
    fn status_round_trip_and_parse() {
        let s = Status {
            pid: 42,
            connected: true,
            channel: Some("Gen\neral".into()),
            members: 3,
        };
        let parsed = Status::parse(&s.to_text()).unwrap();
        assert_eq!(parsed.pid, 42);
        assert!(parsed.connected);
        assert_eq!(parsed.channel.as_deref(), Some("Gen eral"));
        assert_eq!(parsed.members, 3);

        let idle = Status::parse("pid=7\nconnected=true\nchannel=\nmembers=0\n").unwrap();
        assert_eq!(idle.channel, None);
        assert_eq!(
            idle.describe(true),
            "running (pid 7), connected, not in a call"
        );
        assert_eq!(idle.describe(false), "not running (stale status file)");
        assert_eq!(
            parsed.describe(true),
            "running (pid 42), connected, in call \"Gen eral\" with 3 member(s)"
        );

        assert_eq!(Status::parse(""), None);
        assert_eq!(Status::parse("pid=x\nconnected=true"), None);
    }

    #[test]
    fn status_report_reads_file() {
        let tmp = TmpDir::new("status");
        assert_eq!(status_report(&tmp.0), "not running");
        fs::create_dir_all(&tmp.0).unwrap();
        let s = Status {
            pid: std::process::id(),
            connected: true,
            channel: None,
            members: 0,
        };
        write_status(&tmp.0, &s).unwrap();
        assert!(status_report(&tmp.0).starts_with("running (pid "));
        remove_status(&tmp.0).unwrap();
        assert_eq!(status_report(&tmp.0), "not running");
    }
}
