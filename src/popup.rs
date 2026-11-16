//! `popup` command: a GTK4 layer-shell panel with the call members (larger
//! avatars, names, live speaking ring), fed by the daemon's local JSON API.
//!
//! It is a separate, short-lived process: it shows the window right away,
//! connects to the API socket and updates the view on every line. Esc or a
//! click outside the panel closes it; `popup toggle` closes an open popup
//! instead of opening a second one (a lock file in the runtime directory
//! holds its pid).
//!
//! The GTK part needs the `popup` cargo feature. Everything else here (view
//! model, placement, CSS, lock) builds without it, so it is unit tested in
//! the default build.
#![cfg_attr(not(feature = "popup"), allow(dead_code))]

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::api::ApiState;
use crate::config::{PopupSettings, Settings};

/// Avatar size in the popup, in pixels.
pub const AVATAR_SIZE: i32 = 96;
pub const LOCK_NAME: &str = "popup.lock";

/// What the popup shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewModel {
    pub title: String,
    pub members: Vec<MemberView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub user_id: String,
    pub name: String,
    pub avatar: Option<PathBuf>,
    pub speaking: bool,
    pub muted: bool,
    pub deafened: bool,
    pub own: bool,
}

impl ViewModel {
    pub fn message(text: &str) -> Self {
        Self {
            title: text.to_owned(),
            members: Vec::new(),
        }
    }
}

impl MemberView {
    /// CSS classes of the member card, toggled live.
    pub fn css_classes(&self) -> Vec<&'static str> {
        let mut classes = vec!["member"];
        if self.speaking {
            classes.push("speaking");
        }
        if self.muted || self.deafened {
            classes.push("dimmed");
        }
        if self.own {
            classes.push("self");
        }
        classes
    }

    /// Text under the name.
    pub fn status_text(&self) -> &'static str {
        match (self.muted, self.deafened) {
            (_, true) => "Deafened",
            (true, false) => "Muted",
            (false, false) => "",
        }
    }
}

pub fn view_model(state: &ApiState) -> ViewModel {
    let title = if !state.connected {
        "Not connected to Discord".to_owned()
    } else {
        match &state.channel {
            None => "Not in a voice channel".to_owned(),
            Some(c) => c
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| "Voice channel".to_owned()),
        }
    };
    ViewModel {
        title,
        members: state
            .members
            .iter()
            .map(|m| MemberView {
                user_id: m.user_id.clone(),
                name: m.name.clone(),
                avatar: m.avatar.as_ref().map(PathBuf::from),
                speaking: m.speaking,
                muted: m.muted,
                deafened: m.deafened,
                own: m.own,
            })
            .collect(),
    }
}

/// Parses one API line into the view model.
pub fn parse_line(line: &str) -> Result<ViewModel> {
    let state: ApiState = serde_json::from_str(line.trim()).context("invalid API line")?;
    Ok(view_model(&state))
}

/// Whether `new` shows the same cards as `old` (same members, names and
/// avatars in the same order), so only CSS classes and texts need updating
/// instead of rebuilding the widgets.
pub fn same_cards(old: &ViewModel, new: &ViewModel) -> bool {
    old.members.len() == new.members.len()
        && old
            .members
            .iter()
            .zip(&new.members)
            .all(|(a, b)| a.user_id == b.user_id && a.name == b.name && a.avatar == b.avatar)
}

/// Where the panel goes inside the full-screen overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub top: bool,
    pub left: bool,
    /// Margins to the top, right, bottom and left screen edges; 0 for the
    /// edges the panel is not anchored to.
    pub margins: [i32; 4],
}

pub fn placement(p: &PopupSettings) -> Placement {
    let top = p.anchor.is_top();
    let left = p.anchor.is_left();
    let pick = |used: bool, m: i32| if used { m } else { 0 };
    Placement {
        top,
        left,
        margins: [
            pick(top, p.margin_top),
            pick(!left, p.margin_right),
            pick(!top, p.margin_bottom),
            pick(left, p.margin_left),
        ],
    }
}

/// The popup style sheet, using the configured speaking color and dimming.
pub fn css(settings: &Settings) -> String {
    let (r, g, b) = settings.render.speaking_color;
    let dim = settings.render.dim;
    format!(
        "window.discord-voice-backdrop {{ background: none; }}
.discord-voice-panel {{
  background-color: rgba(30, 31, 34, 0.96);
  color: #f2f3f5;
  border-radius: 16px;
  padding: 16px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.5);
}}
.discord-voice-panel .title {{ font-weight: bold; font-size: 1.1em; }}
.discord-voice-panel .member .avatar {{
  border-radius: 50%;
  border: 3px solid transparent;
  transition: border-color 120ms ease-out;
}}
.discord-voice-panel .member.speaking .avatar {{
  border-color: rgb({r}, {g}, {b});
  animation: discord-voice-speaking 1s ease-in-out infinite;
}}
@keyframes discord-voice-speaking {{
  0% {{ box-shadow: 0 0 0 0 rgba({r}, {g}, {b}, 0.7); }}
  70% {{ box-shadow: 0 0 0 8px rgba({r}, {g}, {b}, 0); }}
  100% {{ box-shadow: 0 0 0 0 rgba({r}, {g}, {b}, 0); }}
}}
.discord-voice-panel .member.dimmed {{ opacity: {dim}; }}
.discord-voice-panel .member .status {{ font-size: 0.85em; color: #b5bac1; }}
"
    )
}

/// Result of trying to become the (only) popup.
#[derive(Debug)]
pub enum Lock {
    /// This process is the popup; keep the value alive while it runs.
    Acquired(PopupLock),
    /// Another popup runs, with this pid if it already wrote it.
    Running(Option<u32>),
}

/// Held lock file. The kernel releases the lock when the process dies, so a
/// crashed popup never leaves a stale lock.
#[derive(Debug)]
pub struct PopupLock {
    _file: File,
}

pub fn acquire_lock(path: &Path) -> io::Result<Lock> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    // SAFETY: flock on a valid, open descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(err);
        }
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        return Ok(Lock::Running(text.trim().parse().ok()));
    }
    file.set_len(0)?;
    file.rewind()?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(Lock::Acquired(PopupLock { _file: file }))
}

/// Without the `popup` feature, `popup [toggle]` behaves like `focus`, so
/// a Waybar click bound to the popup still raises Discord: it starts
/// `focus_command` detached and returns at once, never failing.
#[cfg(not(feature = "popup"))]
fn focus_fallback(settings: &Settings) {
    tracing::debug!(
        "built without the `popup` feature; running `focus_command` instead \
         (rebuild with `--features popup` for the panel)"
    );
    crate::focus::spawn_detached(&settings.focus_command);
}

/// Stderr logging for the fallback, quiet unless `RUST_LOG` asks for more.
#[cfg(not(feature = "popup"))]
fn init_fallback_logging() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

/// `popup [toggle]`.
pub fn run(settings: &Settings, toggle: bool) -> Result<()> {
    #[cfg(not(feature = "popup"))]
    {
        let _ = toggle;
        init_fallback_logging();
        focus_fallback(settings);
        Ok(())
    }
    #[cfg(feature = "popup")]
    {
        use std::os::unix::fs::DirBuilderExt;

        let dir = crate::output::default_dir()?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let lock_path = dir.join(LOCK_NAME);
        let lock = acquire_lock(&lock_path)
            .with_context(|| format!("cannot lock {}", lock_path.display()))?;
        match lock {
            Lock::Running(pid) => {
                if toggle {
                    if let Some(pid) = pid.and_then(|p| libc::pid_t::try_from(p).ok()) {
                        // SAFETY: plain syscall; the pid belongs to the live
                        // holder of our private lock file.
                        unsafe { libc::kill(pid, libc::SIGTERM) };
                    }
                }
                // Plain `popup` with one open: nothing to do.
                Ok(())
            }
            Lock::Acquired(lock) => ui::run(settings, &crate::api::socket_path(settings)?, lock),
        }
    }
}

#[cfg(feature = "popup")]
mod ui {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use anyhow::{Context, Result};
    use gtk::gio::prelude::*;
    use gtk::prelude::*;
    use gtk::{gdk, gio, glib, pango};
    use gtk4 as gtk;
    use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

    use super::AVATAR_SIZE;
    use super::{css, parse_line, placement, same_cards, MemberView, PopupLock, ViewModel};
    use crate::config::Settings;

    struct Card {
        root: gtk::Box,
        status: gtk::Label,
    }

    struct View {
        title: gtk::Label,
        members: gtk::FlowBox,
        cards: Vec<Card>,
        current: Option<ViewModel>,
    }

    impl View {
        fn update(&mut self, vm: ViewModel) {
            self.title.set_text(&vm.title);
            let rebuild = self
                .current
                .as_ref()
                .is_none_or(|old| !same_cards(old, &vm));
            if rebuild {
                while let Some(child) = self.members.first_child() {
                    self.members.remove(&child);
                }
                self.cards = vm.members.iter().map(build_card).collect();
                for card in &self.cards {
                    self.members.insert(&card.root, -1);
                }
                self.members.set_visible(!self.cards.is_empty());
            }
            for (card, m) in self.cards.iter().zip(&vm.members) {
                card.root.set_css_classes(&m.css_classes());
                card.status.set_text(m.status_text());
            }
            self.current = Some(vm);
        }
    }

    fn build_card(m: &MemberView) -> Card {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let frame = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        frame.add_css_class("avatar");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_halign(gtk::Align::Center);
        let image = match &m.avatar {
            Some(path) => gtk::Image::from_file(path),
            None => gtk::Image::from_icon_name("avatar-default-symbolic"),
        };
        image.set_pixel_size(AVATAR_SIZE);
        frame.append(&image);

        let name = gtk::Label::new(Some(&m.name));
        name.add_css_class("name");
        name.set_ellipsize(pango::EllipsizeMode::End);
        name.set_max_width_chars(12);
        let status = gtk::Label::new(None);
        status.add_css_class("status");

        root.append(&frame);
        root.append(&name);
        root.append(&status);
        Card { root, status }
    }

    pub fn run(settings: &Settings, socket: &Path, _lock: PopupLock) -> Result<()> {
        gtk::init().context("cannot initialize GTK (is a Wayland session running?)")?;
        let display = gdk::Display::default().context("no display")?;
        let provider = gtk::CssProvider::new();
        provider.load_from_data(&css(settings));
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let main_loop = glib::MainLoop::new(None, false);
        let window = gtk::Window::new();
        window.set_title(Some("discord-voice"));
        window.set_decorated(false);
        window.add_css_class("discord-voice-backdrop");

        let panel = gtk::Box::new(gtk::Orientation::Vertical, 12);
        panel.add_css_class("discord-voice-panel");
        let title = gtk::Label::new(Some("Connecting…"));
        title.add_css_class("title");
        title.set_halign(gtk::Align::Start);
        let members = gtk::FlowBox::new();
        members.set_selection_mode(gtk::SelectionMode::None);
        members.set_homogeneous(true);
        members.set_min_children_per_line(1);
        members.set_max_children_per_line(4);
        members.set_column_spacing(12);
        members.set_row_spacing(12);
        members.set_visible(false);
        panel.append(&title);
        panel.append(&members);
        window.set_child(Some(&panel));

        if gtk4_layer_shell::is_supported() {
            // A transparent overlay over the whole output (below exclusive
            // zones such as Waybar), so clicks outside the panel reach us.
            window.init_layer_shell();
            window.set_layer(Layer::Overlay);
            window.set_namespace(Some("discord-voice-popup"));
            window.set_keyboard_mode(KeyboardMode::Exclusive);
            for edge in [Edge::Top, Edge::Right, Edge::Bottom, Edge::Left] {
                window.set_anchor(edge, true);
            }
            window.set_exclusive_zone(0);
            let place = placement(&settings.popup);
            let align = |start: bool| {
                if start {
                    gtk::Align::Start
                } else {
                    gtk::Align::End
                }
            };
            panel.set_valign(align(place.top));
            panel.set_halign(align(place.left));
            let [top, right, bottom, left] = place.margins;
            panel.set_margin_top(top);
            panel.set_margin_end(right);
            panel.set_margin_bottom(bottom);
            panel.set_margin_start(left);
        } else {
            eprintln!(
                "discord-voice: the compositor does not support wlr-layer-shell; \
                 showing a normal window"
            );
        }

        // Esc closes.
        let keys = gtk::EventControllerKey::new();
        let win = window.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                win.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(keys);

        // A click outside the panel closes.
        let click = gtk::GestureClick::new();
        click.set_button(0);
        let (win, pnl) = (window.clone(), panel.clone());
        click.connect_pressed(move |_, _, x, y| {
            let inside = win
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .is_some_and(|w| w == *pnl.upcast_ref::<gtk::Widget>() || w.is_ancestor(&pnl));
            if !inside {
                win.close();
            }
        });
        window.add_controller(click);

        let ml = main_loop.clone();
        window.connect_close_request(move |_| {
            ml.quit();
            glib::Propagation::Proceed
        });

        let view = Rc::new(RefCell::new(View {
            title,
            members,
            cards: Vec::new(),
            current: None,
        }));
        glib::spawn_future_local(stream_states(socket.to_owned(), view));

        window.present();
        main_loop.run();
        Ok(())
    }

    /// Follows the API stream until it ends, then says why.
    async fn stream_states(socket: PathBuf, view: Rc<RefCell<View>>) {
        let client = gio::SocketClient::new();
        let address = gio::UnixSocketAddress::new(&socket);
        let conn = match client.connect_future(&address).await {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("discord-voice: cannot connect to {}: {e}", socket.display());
                view.borrow_mut().update(ViewModel::message(
                    "The discord-voice daemon is not running",
                ));
                return;
            }
        };
        let input = gio::DataInputStream::new(&conn.input_stream());
        loop {
            match input.read_line_utf8_future(glib::Priority::DEFAULT).await {
                Ok(Some(line)) => match parse_line(&line) {
                    Ok(vm) => view.borrow_mut().update(vm),
                    Err(e) => eprintln!("discord-voice: {e:#}"),
                },
                Ok(None) => break,
                Err(e) => {
                    eprintln!("discord-voice: API read failed: {e}");
                    break;
                }
            }
        }
        view.borrow_mut()
            .update(ViewModel::message("The discord-voice daemon stopped"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "popup"))]
    #[test]
    fn popup_without_feature_falls_back_to_focus() {
        let mut settings = Settings {
            focus_command: vec!["/nonexistent/discord-voice-test-program".to_owned()],
            ..Settings::default()
        };
        assert!(run(&settings, false).is_ok());
        assert!(run(&settings, true).is_ok());
        settings.focus_command = vec!["true".to_owned()];
        assert!(run(&settings, true).is_ok());
    }
    use crate::api::{ApiChannel, ApiMember};
    use crate::config::PopupAnchor;

    fn api_member(id: &str, name: &str) -> ApiMember {
        ApiMember {
            user_id: id.into(),
            name: name.into(),
            speaking: false,
            muted: false,
            deafened: false,
            own: false,
            avatar: None,
        }
    }

    #[test]
    fn parses_api_lines() {
        let line = r#"{"connected":true,"channel":{"id":"42","name":"General"},"members":[
            {"user_id":"1","name":"me","speaking":true,"muted":false,"deafened":false,"self":true,"avatar":"/c/1_a.png"},
            {"user_id":"2","name":"Mason","speaking":false,"muted":true,"deafened":true,"self":false,"avatar":null}]}"#
            .replace('\n', "");
        let vm = parse_line(&format!("{line}\n")).unwrap();
        assert_eq!(vm.title, "General");
        assert_eq!(vm.members.len(), 2);
        let me = &vm.members[0];
        assert_eq!(me.avatar.as_deref(), Some(Path::new("/c/1_a.png")));
        assert_eq!(me.css_classes(), ["member", "speaking", "self"]);
        assert_eq!(me.status_text(), "");
        let other = &vm.members[1];
        assert_eq!(other.avatar, None);
        assert_eq!(other.css_classes(), ["member", "dimmed"]);
        assert_eq!(other.status_text(), "Deafened");
        assert!(parse_line("not json").is_err());
    }

    #[test]
    fn titles_for_every_state() {
        let mut state = ApiState::default();
        assert_eq!(view_model(&state).title, "Not connected to Discord");
        state.connected = true;
        assert_eq!(view_model(&state).title, "Not in a voice channel");
        state.channel = Some(ApiChannel {
            id: "42".into(),
            name: None,
        });
        assert_eq!(view_model(&state).title, "Voice channel");
        let mut muted = api_member("1", "a");
        muted.muted = true;
        state.members = vec![muted];
        let vm = view_model(&state);
        assert_eq!(vm.members[0].status_text(), "Muted");
        assert_eq!(vm.members[0].css_classes(), ["member", "dimmed"]);
    }

    #[test]
    fn speaking_changes_keep_the_cards() {
        let state = |speaking: bool, name: &str| {
            let mut m = api_member("1", name);
            m.speaking = speaking;
            view_model(&ApiState {
                connected: true,
                channel: None,
                members: vec![m, api_member("2", "b")],
            })
        };
        assert!(same_cards(&state(false, "a"), &state(true, "a")));
        assert!(!same_cards(&state(false, "a"), &state(false, "renamed")));
        let mut fewer = state(false, "a");
        fewer.members.pop();
        assert!(!same_cards(&state(false, "a"), &fewer));
    }

    #[test]
    fn placement_uses_the_anchor_edges() {
        let mut p = PopupSettings {
            anchor: PopupAnchor::TopRight,
            margin_top: 1,
            margin_right: 2,
            margin_bottom: 3,
            margin_left: 4,
        };
        assert_eq!(
            placement(&p),
            Placement {
                top: true,
                left: false,
                margins: [1, 2, 0, 0]
            }
        );
        p.anchor = PopupAnchor::BottomLeft;
        assert_eq!(
            placement(&p),
            Placement {
                top: false,
                left: true,
                margins: [0, 0, 3, 4]
            }
        );
    }

    #[test]
    fn css_uses_configured_colors() {
        let mut settings = Settings::default();
        settings.render.speaking_color = (1, 2, 3);
        settings.render.dim = 0.5;
        let css = css(&settings);
        assert!(css.contains("border-color: rgb(1, 2, 3);"));
        assert!(css.contains("opacity: 0.5;"));
        assert!(css.contains(".member.speaking"));
    }

    #[test]
    fn lock_allows_one_popup() {
        let dir = std::env::temp_dir().join(format!("dv-popup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LOCK_NAME);
        let first = acquire_lock(&path).unwrap();
        assert!(matches!(first, Lock::Acquired(_)));
        // flock locks belong to the open file, so a second open in the same
        // process conflicts like another process would.
        match acquire_lock(&path).unwrap() {
            Lock::Running(pid) => assert_eq!(pid, Some(std::process::id())),
            Lock::Acquired(_) => panic!("second lock acquired"),
        }
        drop(first);
        assert!(matches!(acquire_lock(&path).unwrap(), Lock::Acquired(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
