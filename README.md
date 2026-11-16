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

Secrets and tokens are never logged. No telemetry.
