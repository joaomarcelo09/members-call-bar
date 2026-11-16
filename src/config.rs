//! Optional settings read from `$XDG_CONFIG_HOME/discord-voice/config.toml`,
//! the same file that holds the OAuth credentials ([`crate::auth::Config`]).
//! Every key here is optional; unknown keys are rejected so typos are
//! reported instead of silently ignored.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::IgnoredAny;
use serde::Deserialize;

use crate::{auth, focus, render};

pub const DEFAULT_SLOTS: usize = 5;
/// Upper bound for `slots`. The overflow slot comes on top of it.
pub const MAX_SLOTS: usize = 10;
pub const MIN_SIZE: u32 = 16;
pub const MAX_SIZE: u32 = 512;
pub const DEFAULT_SIGNAL_BASE: i32 = 8;
pub const DEFAULT_FOCUS_COMMAND: &str = "hyprctl dispatch focuswindow class:discord";
pub const DEFAULT_POPUP_MARGIN: i32 = 8;
pub const MAX_POPUP_MARGIN: i32 = 4000;

/// Screen corner the popup is placed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupAnchor {
    TopLeft,
    #[default]
    TopRight,
    BottomLeft,
    BottomRight,
}

impl PopupAnchor {
    pub fn is_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopRight)
    }

    pub fn is_left(self) -> bool {
        matches!(self, Self::TopLeft | Self::BottomLeft)
    }
}

/// Popup placement: corner plus the distance to each screen edge (only the
/// two edges of the corner are used).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupSettings {
    pub anchor: PopupAnchor,
    pub margin_top: i32,
    pub margin_right: i32,
    pub margin_bottom: i32,
    pub margin_left: i32,
}

impl Default for PopupSettings {
    fn default() -> Self {
        Self {
            anchor: PopupAnchor::TopRight,
            margin_top: DEFAULT_POPUP_MARGIN,
            margin_right: DEFAULT_POPUP_MARGIN,
            margin_bottom: DEFAULT_POPUP_MARGIN,
            margin_left: DEFAULT_POPUP_MARGIN,
        }
    }
}

/// Order of the members after your own user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    /// Join order.
    #[default]
    Join,
    /// Most recently speaking first; members who never spoke keep join order.
    Speaking,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Member slots; one extra overflow slot (`+K`) follows them.
    pub slots: usize,
    pub render: render::RenderConfig,
    /// Waybar `signal` of slot 0; slot `n` uses `signal_base + n`.
    pub signal_base: i32,
    /// Discord IPC socket; `None`: search the usual locations.
    pub socket_path: Option<PathBuf>,
    pub show_self: bool,
    pub sort: SortOrder,
    /// `focus_command`, already split into program and arguments.
    pub focus_command: Vec<String>,
    /// Serve the local JSON API (`api`).
    pub api: bool,
    /// API socket; `None`: `$XDG_RUNTIME_DIR/discord-voice/api.sock`.
    pub api_socket: Option<PathBuf>,
    pub popup: PopupSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            slots: DEFAULT_SLOTS,
            render: render::RenderConfig::default(),
            signal_base: DEFAULT_SIGNAL_BASE,
            socket_path: None,
            show_self: true,
            sort: SortOrder::Join,
            focus_command: focus::split_command(DEFAULT_FOCUS_COMMAND)
                .expect("default focus command is valid"),
            api: true,
            api_socket: None,
            popup: PopupSettings::default(),
        }
    }
}

impl Settings {
    /// Slot files written by the daemon: the member slots plus overflow.
    pub fn slot_files(&self) -> usize {
        self.slots + 1
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    // Read by `auth::Config`; listed so they are not unknown keys here.
    #[serde(rename = "client_id")]
    _client_id: Option<IgnoredAny>,
    #[serde(rename = "client_secret")]
    _client_secret: Option<IgnoredAny>,
    #[serde(rename = "redirect_uri")]
    _redirect_uri: Option<IgnoredAny>,

    slots: Option<i64>,
    size: Option<i64>,
    speaking_color: Option<String>,
    dim: Option<f64>,
    signal_base: Option<i64>,
    socket_path: Option<String>,
    show_self: Option<bool>,
    sort: Option<String>,
    focus_command: Option<String>,
    api: Option<bool>,
    api_socket: Option<String>,
    popup_anchor: Option<String>,
    popup_margin_top: Option<i64>,
    popup_margin_right: Option<i64>,
    popup_margin_bottom: Option<i64>,
    popup_margin_left: Option<i64>,
}

/// Parses and validates the optional settings of a `config.toml` text.
pub fn parse_settings(text: &str) -> Result<Settings> {
    let raw: Raw = toml::from_str(text).map_err(|e| anyhow!("invalid config: {}", e.message()))?;
    let mut s = Settings::default();

    if let Some(n) = raw.slots {
        if !(1..=MAX_SLOTS as i64).contains(&n) {
            bail!("`slots` must be between 1 and {MAX_SLOTS}, got {n}");
        }
        s.slots = n as usize;
    }
    if let Some(n) = raw.size {
        if !(MIN_SIZE as i64..=MAX_SIZE as i64).contains(&n) {
            bail!("`size` must be between {MIN_SIZE} and {MAX_SIZE} pixels, got {n}");
        }
        s.render.size = n as u32;
    }
    if let Some(c) = &raw.speaking_color {
        s.render.speaking_color = parse_color(c).ok_or_else(|| {
            anyhow!("`speaking_color` must be a hex color like \"#23a55a\", got {c:?}")
        })?;
    }
    if let Some(d) = raw.dim {
        if !(0.0..=1.0).contains(&d) {
            bail!("`dim` must be between 0.0 and 1.0 (opacity of muted members), got {d}");
        }
        s.render.dim = d as f32;
    }
    let max_signal = i64::from(libc::SIGRTMAX() - libc::SIGRTMIN());
    if let Some(b) = raw.signal_base {
        s.signal_base = i32::try_from(b).unwrap_or(-1);
        if s.signal_base < 1 {
            bail!("`signal_base` must be at least 1, got {b}");
        }
    }
    // The overflow slot (index `slots`) uses the highest signal.
    let highest = i64::from(s.signal_base) + s.slots as i64;
    if highest > max_signal {
        bail!(
            "`signal_base` + `slots` must be at most {max_signal} (SIGRTMAX - SIGRTMIN), got {highest}"
        );
    }
    if let Some(p) = raw.socket_path {
        let path = PathBuf::from(&p);
        if !path.is_absolute() {
            bail!("`socket_path` must be an absolute path, got {p:?}");
        }
        s.socket_path = Some(path);
    }
    if let Some(b) = raw.show_self {
        s.show_self = b;
    }
    if let Some(sort) = &raw.sort {
        s.sort = match sort.as_str() {
            "join" => SortOrder::Join,
            "speaking" => SortOrder::Speaking,
            other => bail!("`sort` must be \"join\" or \"speaking\", got {other:?}"),
        };
    }
    if let Some(cmd) = &raw.focus_command {
        let argv = focus::split_command(cmd).context("invalid `focus_command`")?;
        if argv.is_empty() {
            bail!("`focus_command` must not be empty");
        }
        s.focus_command = argv;
    }
    if let Some(b) = raw.api {
        s.api = b;
    }
    if let Some(p) = raw.api_socket {
        let path = PathBuf::from(&p);
        if !path.is_absolute() {
            bail!("`api_socket` must be an absolute path, got {p:?}");
        }
        s.api_socket = Some(path);
    }
    if let Some(a) = &raw.popup_anchor {
        s.popup.anchor = match a.as_str() {
            "top-left" => PopupAnchor::TopLeft,
            "top-right" => PopupAnchor::TopRight,
            "bottom-left" => PopupAnchor::BottomLeft,
            "bottom-right" => PopupAnchor::BottomRight,
            other => bail!(
                "`popup_anchor` must be \"top-left\", \"top-right\", \"bottom-left\" or \"bottom-right\", got {other:?}"
            ),
        };
    }
    let margins = [
        (
            "popup_margin_top",
            raw.popup_margin_top,
            &mut s.popup.margin_top,
        ),
        (
            "popup_margin_right",
            raw.popup_margin_right,
            &mut s.popup.margin_right,
        ),
        (
            "popup_margin_bottom",
            raw.popup_margin_bottom,
            &mut s.popup.margin_bottom,
        ),
        (
            "popup_margin_left",
            raw.popup_margin_left,
            &mut s.popup.margin_left,
        ),
    ];
    for (key, value, field) in margins {
        if let Some(m) = value {
            if !(0..=i64::from(MAX_POPUP_MARGIN)).contains(&m) {
                bail!("`{key}` must be between 0 and {MAX_POPUP_MARGIN} pixels, got {m}");
            }
            *field = m as i32;
        }
    }
    Ok(s)
}

/// `#rrggbb` or `rrggbb`.
fn parse_color(text: &str) -> Option<(u8, u8, u8)> {
    let hex = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(auth::config_dir()?.join("config.toml"))
}

/// Loads credentials and settings from one private config file.
pub fn load(path: &Path) -> Result<(auth::Config, Settings)> {
    let text = auth::read_private_config(path)?;
    let cfg = auth::parse_config(&text).with_context(|| format!("in {}", path.display()))?;
    let settings = parse_settings(&text).with_context(|| format!("in {}", path.display()))?;
    Ok((cfg, settings))
}

/// Settings only, for commands that need no credentials (`focus`): a
/// missing file gives the defaults, an invalid one too, with a warning.
pub fn load_settings_or_default(path: &Path) -> Settings {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    parse_settings(&text).unwrap_or_else(|e| {
        eprintln!(
            "discord-voice: {e:#} (in {}); using defaults",
            path.display()
        );
        Settings::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREDS: &str = "client_id = \"1\"\nclient_secret = \"placeholder\"\n";

    fn parse(extra: &str) -> Result<Settings> {
        parse_settings(&format!("{CREDS}{extra}\n"))
    }

    fn err(extra: &str) -> String {
        format!("{:#}", parse(extra).unwrap_err())
    }

    #[test]
    fn defaults_when_keys_are_missing() {
        let s = parse("").unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!(s.slots, 5);
        assert_eq!(s.slot_files(), 6);
        assert_eq!(s.render.size, 64);
        assert_eq!(s.render.speaking_color, (0x23, 0xa5, 0x5a));
        assert_eq!(s.signal_base, 8);
        assert!(s.show_self);
        assert_eq!(s.sort, SortOrder::Join);
        assert_eq!(s.socket_path, None);
        assert_eq!(
            s.focus_command,
            ["hyprctl", "dispatch", "focuswindow", "class:discord"]
        );
        assert!(s.api);
        assert_eq!(s.api_socket, None);
        assert_eq!(s.popup.anchor, PopupAnchor::TopRight);
        assert_eq!((s.popup.margin_top, s.popup.margin_right), (8, 8));
        // Settings do not need the credentials.
        assert_eq!(parse_settings("").unwrap(), Settings::default());
    }

    #[test]
    fn parses_every_key() {
        let s = parse(
            r##"
slots = 3
size = 32
speaking_color = "#FF0080"
dim = 0.25
signal_base = 20
socket_path = "/run/user/1000/discord-ipc-0"
show_self = false
sort = "speaking"
focus_command = "swaymsg '[class=\"discord\"] focus'"
api = false
api_socket = "/tmp/dv/api.sock"
popup_anchor = "bottom-left"
popup_margin_top = 1
popup_margin_right = 2
popup_margin_bottom = 3
popup_margin_left = 4
"##,
        )
        .unwrap();
        assert_eq!(s.slots, 3);
        assert_eq!(s.render.size, 32);
        assert_eq!(s.render.speaking_color, (0xff, 0x00, 0x80));
        assert_eq!(s.render.dim, 0.25);
        assert_eq!(s.signal_base, 20);
        assert_eq!(
            s.socket_path.as_deref(),
            Some(Path::new("/run/user/1000/discord-ipc-0"))
        );
        assert!(!s.show_self);
        assert_eq!(s.sort, SortOrder::Speaking);
        assert_eq!(s.focus_command, ["swaymsg", "[class=\"discord\"] focus"]);
        assert!(!s.api);
        assert_eq!(s.api_socket.as_deref(), Some(Path::new("/tmp/dv/api.sock")));
        assert_eq!(
            s.popup,
            PopupSettings {
                anchor: PopupAnchor::BottomLeft,
                margin_top: 1,
                margin_right: 2,
                margin_bottom: 3,
                margin_left: 4,
            }
        );
        assert!(!s.popup.anchor.is_top() && s.popup.anchor.is_left());
        assert_eq!(
            parse("speaking_color = \"00ff00\"")
                .unwrap()
                .render
                .speaking_color,
            (0, 255, 0)
        );
    }

    #[test]
    fn rejects_invalid_values_with_clear_errors() {
        assert!(err("slots = 0").contains("`slots` must be between 1 and 10"));
        assert!(err("slots = 11").contains("got 11"));
        assert!(err("size = 8").contains("`size`"));
        assert!(err("speaking_color = \"green\"").contains("hex color"));
        assert!(err("speaking_color = \"#12345\"").contains("hex color"));
        assert!(err("dim = 1.5").contains("`dim`"));
        assert!(err("signal_base = 0").contains("at least 1"));
        assert!(err("signal_base = 25\nslots = 10").contains("at most"));
        assert!(err("socket_path = \"discord-ipc-0\"").contains("absolute"));
        assert!(err("sort = \"name\"").contains("\"join\" or \"speaking\""));
        assert!(err("focus_command = \"\"").contains("must not be empty"));
        assert!(err("focus_command = \"a 'b\"").contains("focus_command"));
        assert!(err("slots = \"5\"").contains("invalid config"));
        assert!(err("api_socket = \"api.sock\"").contains("`api_socket` must be an absolute"));
        assert!(err("popup_anchor = \"center\"").contains("`popup_anchor`"));
        assert!(err("popup_margin_top = -1").contains("`popup_margin_top`"));
        assert!(err("popup_margin_right = 5000").contains("`popup_margin_right`"));
        // Typos are reported.
        assert!(err("slot = 5").contains("unknown field `slot`"));
    }

    #[test]
    fn lenient_load_falls_back_to_defaults() {
        let dir =
            std::env::temp_dir().join(format!("discord-voice-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        assert_eq!(load_settings_or_default(&path), Settings::default());
        std::fs::write(&path, "focus_command = \"wmctrl -a Discord\"\n").unwrap();
        assert_eq!(
            load_settings_or_default(&path).focus_command,
            ["wmctrl", "-a", "Discord"]
        );
        std::fs::write(&path, "slots = 99\n").unwrap();
        assert_eq!(load_settings_or_default(&path), Settings::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
