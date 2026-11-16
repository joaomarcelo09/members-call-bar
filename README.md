# discord-voice

Connects to the official Discord desktop client's local RPC socket, authorizes
once, and prints voice events (call members, joins/leaves, who is speaking) to
stdout. Linux only (native, Flatpak and Snap installs).

## Setup

Discord does not let third-party apps share credentials, so each user registers
their own Discord application:

1. Open <https://discord.com/developers/applications> and click **New Application**.
2. In **OAuth2**, copy the **Client ID**, reset/copy the **Client Secret**, and
   add the redirect URI `http://localhost` (or your own; it must match `redirect_uri`
   in the config).
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

## Usage

```sh
cargo run --release            # same as: discord-voice events
RUST_LOG=debug cargo run       # verbose logs on stderr
```

On first run Discord shows an authorization prompt (scopes `rpc` and
`rpc.voice.read`); approve it. The token is stored with mode 0600 at
`$XDG_CONFIG_HOME/discord-voice/token.json` and refreshed automatically; if the
refresh fails the prompt is shown again. Delete that file to log out.

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

`discord-voice run` is the daemon: it tracks the call and renders up to 5
members (you first) as 64x64 circular avatars with a green ring while
speaking, dimmed while muted or deafened. Files live in
`$XDG_RUNTIME_DIR/discord-voice/` (mode 0700): `slot<n>.png`, `slot<n>.txt`
(tooltip) and `status`. Only changed slots are rewritten, and Waybar is told
to refresh them with `SIGRTMIN+8+n` (changes coalesced for 50 ms, at most ~20
signals per second). Slots are emptied when you leave the call or the daemon
stops.

1. Install the binary: `cargo install --path .` (to `~/.cargo/bin`).
2. Run the daemon once in a terminal to approve the Discord prompt, then
   install the systemd user unit:

   ```sh
   install -Dm644 packaging/discord-voice.service ~/.config/systemd/user/discord-voice.service
   systemctl --user daemon-reload
   systemctl --user enable --now discord-voice.service
   ```

3. Add the five `image#discord-voice-N` modules from
   [`packaging/waybar-config.jsonc`](packaging/waybar-config.jsonc) to your
   Waybar config (optionally the CSS in
   [`packaging/waybar-style.css`](packaging/waybar-style.css)) and reload
   Waybar. Each module runs `discord-voice slot N`, which prints the image
   path and tooltip, or nothing when the slot is empty (the module is hidden).
   Keep `signal` at `8 + N`; no `interval` is needed.

`discord-voice status` tells whether the daemon runs and which call it shows.

Secrets and tokens are never logged. No telemetry.
