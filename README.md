# discord-voice

Shows the members of your current Discord voice call in Waybar: one circular
avatar per member, a green ring while someone speaks, dimmed while muted or
deafened, and a `+K` badge when the call has more members than slots. It talks
to the official Discord desktop client's local RPC socket (native, Flatpak and
Snap installs; Linux only), reconnects by itself when Discord restarts, and
raises the Discord window when you click a slot.

## Install

From the AUR (package `discord-voice-waybar`):

```sh
yay -S discord-voice-waybar      # or: paru -S discord-voice-waybar
```

This installs `/usr/bin/discord-voice`, the systemd user unit
`discord-voice.service`, and sample Waybar config/CSS in
`/usr/share/doc/discord-voice-waybar/`. The PKGBUILD lives in
[`packaging/aur/`](packaging/aur/PKGBUILD).

With cargo (installs to `~/.cargo/bin`):

```sh
cargo install --git https://github.com/joaomarcelo09/members-call-bar
# or, from a checkout:
cargo install --path .
```

The bundled unit runs `/usr/bin/discord-voice`; with a cargo install, copy it
and point `ExecStart` at `%h/.cargo/bin/discord-voice run` (see
[Waybar setup](#waybar-setup)).

## Discord application setup

Discord does not let third-party apps share credentials, so each user registers
their own Discord application (free, takes a minute):

1. Open <https://discord.com/developers/applications> and click **New
   Application**; any name works (it is shown in the approval dialog).
2. In **OAuth2**, copy the **Client ID**, reset/copy the **Client Secret**, and
   under **Redirects** add `http://localhost` (or your own URI; it must match
   `redirect_uri` in the config). Save.
3. Create the config file with private permissions:

   ```sh
   mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/discord-voice"
   cd "${XDG_CONFIG_HOME:-$HOME/.config}/discord-voice"
   install -m 600 /dev/null config.toml
   cat > config.toml <<'EOF'
   client_id = "YOUR_CLIENT_ID"
   client_secret = "YOUR_CLIENT_SECRET"
   # optional, defaults to http://localhost
   redirect_uri = "http://localhost"
   EOF
   ```

   The program refuses to start if `config.toml` is readable by group or others.
4. First run: with the Discord desktop client open, run `discord-voice run` in
   a terminal. Discord shows an approval dialog for your application (scopes
   `rpc` and `rpc.voice.read`); click **Authorize**. The token is stored with
   mode 0600 at `$XDG_CONFIG_HOME/discord-voice/token.json` and refreshed
   automatically. If Discord rejects the refresh token (revoked or expired),
   the file is deleted and the approval dialog is shown again. Delete the file
   to log out. Stop the terminal run (Ctrl-C) and continue with the Waybar
   setup.

## Configuration

Everything lives in `$XDG_CONFIG_HOME/discord-voice/config.toml` (default
`~/.config/discord-voice/config.toml`). Only `client_id` and `client_secret`
are required; every other key is optional. Unknown keys and invalid values are
reported with a clear error at start-up. Restart the daemon after editing
(`systemctl --user restart discord-voice`).

| Key | Default | Description |
| --- | --- | --- |
| `client_id` | (required) | OAuth2 client ID of your Discord application. |
| `client_secret` | (required) | OAuth2 client secret. Never logged. |
| `redirect_uri` | `"http://localhost"` | Must match a redirect of the application. |
| `slots` | `5` | Member slots, 1 to 10. One more slot (`slots`) shows the `+K` overflow badge, so Waybar needs `slots + 1` modules. |
| `size` | `64` | Size of the rendered images in pixels, 16 to 512 (Waybar scales them to the module `size`). |
| `speaking_color` | `"#23a55a"` | Ring color while speaking, `#rrggbb`. |
| `dim` | `0.4` | Opacity of muted or deafened members, 0.0 to 1.0. |
| `signal_base` | `8` | Waybar `signal` of slot 0; slot `n` uses `signal_base + n`. `signal_base + slots` must be at most `SIGRTMAX - SIGRTMIN` (30 on glibc). |
| `socket_path` | (search) | Absolute path of the Discord IPC socket. By default `discord-ipc-0..9` is searched in `$XDG_RUNTIME_DIR`, `$TMPDIR` and `/tmp`, including the Flatpak and Snap sub-directories. |
| `show_self` | `true` | Show your own user (always in the first slot). |
| `sort` | `"join"` | `"join"`: join order. `"speaking"`: most recently speaking first (your own user stays first if shown). |
| `focus_command` | `"hyprctl dispatch focuswindow class:discord"` | Run by `discord-voice focus` (slot click). Split into words with shell-like quoting (`'...'`, `"..."`, `\`), run directly without a shell. E.g. `"swaymsg '[class=\"discord\"] focus'"`. |

Full example:

```toml
client_id = "YOUR_CLIENT_ID"
client_secret = "YOUR_CLIENT_SECRET"
slots = 5
size = 64
speaking_color = "#23a55a"
dim = 0.4
signal_base = 8
show_self = true
sort = "speaking"
focus_command = "hyprctl dispatch focuswindow class:discord"
```

## Debugging commands

```sh
cargo run --release            # same as: discord-voice events
RUST_LOG=debug cargo run       # verbose logs on stderr
```

Example output:

```
JOINED channel=199737254929760256 name="General" members=2
  MEMBER user_id=190320984123768832 name="test" mute=true deaf=false
  MEMBER user_id=53908232506183680 name="Mason" mute=false deaf=false
SPEAKING_START user_id=53908232506183680
SPEAKING_STOP user_id=53908232506183680
LEFT channel=199737254929760256
```

### `state` command

```sh
cargo run --release -- state
```

Tracks the call in memory and prints the whole state whenever it changes: own
user first, then the others in join order, with speaking, mute and deaf flags
(`self`, `server` or `self+server`) and the avatar file:

```
STATE channel=199737254929760256 name="General" members=2
  [me] name="test" user_id=190320984123768832 speaking=no mute=self deaf=no avatar=/home/u/.cache/discord-voice/avatars/190320984123768832_b004ec1740a63ca06ae2e14c5cee11f3.png
  name="Mason G" user_id=53908232506183680 speaking=yes mute=no deaf=no avatar=/home/u/.cache/discord-voice/avatars/default_2.png
```

Avatars are downloaded only from `https://cdn.discordapp.com` (10 s timeout,
no redirects) and cached in `$XDG_CACHE_HOME/discord-voice/avatars/` as
`<user id>_<avatar hash>.png`; cached files are never downloaded again. Users
without an avatar, or whose download fails, get Discord's default avatar.

## Waybar setup

`discord-voice run` is the daemon: it tracks the call and renders up to
`slots` members (you first) as circular avatars, plus the `+K` overflow badge
whose tooltip lists the hidden members. Files live in
`$XDG_RUNTIME_DIR/discord-voice/` (mode 0700): `slot<n>.png`, `slot<n>.txt`
(tooltip) and `status`. Only changed slots are rewritten, and Waybar is told
to refresh them with `SIGRTMIN+signal_base+n` (changes coalesced for 50 ms, at
most ~20 signals per second). Slots are emptied when you leave the call, when
the connection to Discord drops, and when the daemon stops.

If Discord is closed or restarted, the daemon keeps running and reconnects on
its own (retrying after 1 s, 2 s, 4 s, then every 5 s), so the module comes
back within seconds of Discord starting again. `discord-voice status` shows
`not connected` meanwhile.

1. Enable the systemd user unit (after the first-run approval above):

   ```sh
   # AUR install: the unit is already in /usr/lib/systemd/user.
   systemctl --user enable --now discord-voice.service

   # cargo install: install the unit with the cargo binary path.
   sed 's|/usr/bin/discord-voice|%h/.cargo/bin/discord-voice|' packaging/discord-voice.service \
     | install -Dm644 /dev/stdin ~/.config/systemd/user/discord-voice.service
   systemctl --user daemon-reload
   systemctl --user enable --now discord-voice.service
   ```

2. Add the six `image#discord-voice-N` modules from
   [`packaging/waybar-config.jsonc`](packaging/waybar-config.jsonc)
   (`/usr/share/doc/discord-voice-waybar/` for the AUR package) to your Waybar
   config (optionally the CSS in
   [`packaging/waybar-style.css`](packaging/waybar-style.css)) and reload
   Waybar. Each module runs `discord-voice slot N`, which prints the image
   path and tooltip, or nothing when the slot is empty (the module is hidden).
   Keep `signal` at `signal_base + N`; no `interval` is needed. With a
   non-default `slots`, use `slots + 1` modules (the last one is the overflow
   badge).
3. Clicking a slot runs `discord-voice focus`, which starts `focus_command`
   detached and returns immediately (errors are ignored).

`discord-voice status` tells whether the daemon runs, whether it is connected
and which call it shows.

Secrets and tokens are never logged. No telemetry.
