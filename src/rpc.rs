//! Discord RPC command/event layer on top of [`crate::ipc`].
//!
//! Commands carry a UUID nonce; replies are matched by nonce. Events that
//! arrive while waiting for a reply are queued and returned later by
//! [`RpcClient::next_event`].

use std::collections::VecDeque;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ipc::IpcConnection;

/// Per-channel events subscribed on join and unsubscribed on leave.
pub const CHANNEL_EVENTS: &[&str] = &[
    "VOICE_STATE_CREATE",
    "VOICE_STATE_UPDATE",
    "VOICE_STATE_DELETE",
    "SPEAKING_START",
    "SPEAKING_STOP",
];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub global_name: Option<String>,
    /// Avatar hash, `None` when the user uses a default avatar.
    #[serde(default)]
    pub avatar: Option<String>,
    /// Legacy discriminator (`"0"` for migrated usernames).
    #[serde(default)]
    pub discriminator: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct VoiceStateFlags {
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub deaf: bool,
    #[serde(default)]
    pub self_mute: bool,
    #[serde(default)]
    pub self_deaf: bool,
}

/// One entry of a channel's `voice_states`, also the payload of VOICE_STATE_*.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VoiceMember {
    pub user: User,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub voice_state: Option<VoiceStateFlags>,
}

impl VoiceMember {
    pub fn display_name(&self) -> &str {
        self.nick
            .as_deref()
            .or(self.user.global_name.as_deref())
            .or(self.user.username.as_deref())
            .unwrap_or("?")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VoiceChannel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub voice_states: Vec<VoiceMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    ChannelSelect {
        channel_id: Option<String>,
        guild_id: Option<String>,
    },
    StateCreate(VoiceMember),
    StateUpdate(VoiceMember),
    StateDelete(VoiceMember),
    SpeakingStart {
        user_id: String,
        channel_id: Option<String>,
    },
    SpeakingStop {
        user_id: String,
        channel_id: Option<String>,
    },
    Other {
        evt: String,
    },
}

#[derive(Deserialize)]
struct ChannelSelectData {
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    guild_id: Option<String>,
}

#[derive(Deserialize)]
struct SpeakingData {
    user_id: String,
    #[serde(default)]
    channel_id: Option<String>,
}

/// Parses a DISPATCH payload into a [`VoiceEvent`].
pub fn parse_event(payload: &Value) -> Result<VoiceEvent> {
    let evt = payload
        .get("evt")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("event payload has no evt"))?;
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let parse_err = || format!("malformed {evt} event");
    Ok(match evt {
        "VOICE_CHANNEL_SELECT" => {
            let d: ChannelSelectData = serde_json::from_value(data).with_context(parse_err)?;
            VoiceEvent::ChannelSelect {
                channel_id: d.channel_id,
                guild_id: d.guild_id,
            }
        }
        "VOICE_STATE_CREATE" => {
            VoiceEvent::StateCreate(serde_json::from_value(data).with_context(parse_err)?)
        }
        "VOICE_STATE_UPDATE" => {
            VoiceEvent::StateUpdate(serde_json::from_value(data).with_context(parse_err)?)
        }
        "VOICE_STATE_DELETE" => {
            VoiceEvent::StateDelete(serde_json::from_value(data).with_context(parse_err)?)
        }
        "SPEAKING_START" => {
            let d: SpeakingData = serde_json::from_value(data).with_context(parse_err)?;
            VoiceEvent::SpeakingStart {
                user_id: d.user_id,
                channel_id: d.channel_id,
            }
        }
        "SPEAKING_STOP" => {
            let d: SpeakingData = serde_json::from_value(data).with_context(parse_err)?;
            VoiceEvent::SpeakingStop {
                user_id: d.user_id,
                channel_id: d.channel_id,
            }
        }
        other => VoiceEvent::Other {
            evt: other.to_owned(),
        },
    })
}

/// Builds a command payload.
pub fn command_payload(cmd: &str, args: Value, evt: Option<&str>, nonce: &str) -> Value {
    let mut payload = json!({ "cmd": cmd, "args": args, "nonce": nonce });
    if let Some(evt) = evt {
        payload["evt"] = json!(evt);
    }
    payload
}

/// What an incoming payload is, relative to a pending command nonce.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Reply to the pending command: its `data` on success.
    Reply(Result<Value, String>),
    /// An event (DISPATCH without a nonce), to be queued.
    Event,
    /// Reply to some other command; ignored.
    Unrelated,
}

pub fn classify(payload: &Value, nonce: &str) -> Incoming {
    match payload.get("nonce").and_then(Value::as_str) {
        Some(n) if n == nonce => {
            let data = payload.get("data").cloned().unwrap_or(Value::Null);
            if payload.get("evt").and_then(Value::as_str) == Some("ERROR") {
                let code = data.get("code").cloned().unwrap_or(Value::Null);
                let message = data.get("message").and_then(Value::as_str).unwrap_or("");
                Incoming::Reply(Err(format!("code {code}: {message}")))
            } else {
                Incoming::Reply(Ok(data))
            }
        }
        Some(_) => Incoming::Unrelated,
        None if payload.get("cmd").and_then(Value::as_str) == Some("DISPATCH") => Incoming::Event,
        None => Incoming::Unrelated,
    }
}

pub struct RpcClient {
    conn: IpcConnection,
    pending_events: VecDeque<Value>,
}

impl RpcClient {
    pub fn new(conn: IpcConnection) -> Self {
        Self {
            conn,
            pending_events: VecDeque::new(),
        }
    }

    /// Sends a command and waits for the reply with the same nonce.
    pub async fn command(&mut self, cmd: &str, args: Value, evt: Option<&str>) -> Result<Value> {
        let nonce = uuid::Uuid::new_v4().to_string();
        self.conn
            .send(&command_payload(cmd, args, evt, &nonce))
            .await?;
        loop {
            let payload = self.conn.recv().await?;
            match classify(&payload, &nonce) {
                Incoming::Reply(Ok(data)) => return Ok(data),
                Incoming::Reply(Err(e)) => bail!("{cmd} failed: {e}"),
                Incoming::Event => self.pending_events.push_back(payload),
                Incoming::Unrelated => {
                    tracing::debug!(cmd = ?payload.get("cmd"), "ignoring unrelated reply")
                }
            }
        }
    }

    /// AUTHORIZE: prompts the user in the Discord client; returns the OAuth code.
    pub async fn authorize(
        &mut self,
        client_id: &str,
        scopes: &[&str],
        redirect_uri: &str,
    ) -> Result<String> {
        let data = self
            .command(
                "AUTHORIZE",
                authorize_args(client_id, scopes, redirect_uri),
                None,
            )
            .await?;
        data.get("code")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("AUTHORIZE reply has no code"))
    }

    /// AUTHENTICATE with an access token. Returns only the authenticated user:
    /// the rest of the reply is dropped because it echoes the token back.
    pub async fn authenticate(&mut self, access_token: &str) -> Result<User> {
        let data = self
            .command(
                "AUTHENTICATE",
                json!({ "access_token": access_token }),
                None,
            )
            .await?;
        parse_authenticated_user(&data)
    }

    pub async fn subscribe(&mut self, evt: &str, args: Value) -> Result<()> {
        self.command("SUBSCRIBE", args, Some(evt)).await.map(drop)
    }

    pub async fn unsubscribe(&mut self, evt: &str, args: Value) -> Result<()> {
        self.command("UNSUBSCRIBE", args, Some(evt)).await.map(drop)
    }

    pub async fn subscribe_channel(&mut self, channel_id: &str) -> Result<()> {
        for evt in CHANNEL_EVENTS {
            self.subscribe(evt, json!({ "channel_id": channel_id }))
                .await?;
        }
        Ok(())
    }

    pub async fn unsubscribe_channel(&mut self, channel_id: &str) -> Result<()> {
        for evt in CHANNEL_EVENTS {
            self.unsubscribe(evt, json!({ "channel_id": channel_id }))
                .await?;
        }
        Ok(())
    }

    /// Returns the voice channel the user is in, or `None`.
    pub async fn get_selected_voice_channel(&mut self) -> Result<Option<VoiceChannel>> {
        let data = self
            .command("GET_SELECTED_VOICE_CHANNEL", json!({}), None)
            .await?;
        parse_selected_channel(data)
    }

    /// Next event, from the queue first, then from the socket.
    pub async fn next_event(&mut self) -> Result<VoiceEvent> {
        loop {
            let payload = match self.pending_events.pop_front() {
                Some(p) => p,
                None => self.conn.recv().await?,
            };
            if classify(&payload, "") == Incoming::Event {
                return parse_event(&payload);
            }
        }
    }
}

/// Arguments of AUTHORIZE. Discord rejects the request with code 5000
/// ("Missing redirect_uri") without `redirect_uri`, which must match the one
/// used for the code exchange.
pub fn authorize_args(client_id: &str, scopes: &[&str], redirect_uri: &str) -> Value {
    json!({ "client_id": client_id, "scopes": scopes, "redirect_uri": redirect_uri })
}

/// Extracts `data.user` from an AUTHENTICATE reply without touching the
/// token fields.
pub fn parse_authenticated_user(data: &Value) -> Result<User> {
    let user = data
        .get("user")
        .cloned()
        .ok_or_else(|| anyhow!("AUTHENTICATE reply has no user"))?;
    serde_json::from_value(user).context("malformed user in AUTHENTICATE reply")
}

pub fn parse_selected_channel(data: Value) -> Result<Option<VoiceChannel>> {
    if data.is_null() {
        return Ok(None);
    }
    serde_json::from_value(data)
        .map(Some)
        .context("malformed GET_SELECTED_VOICE_CHANNEL reply")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELECTED_CHANNEL: &str = r#"{
        "cmd": "GET_SELECTED_VOICE_CHANNEL",
        "data": {
            "id": "199737254929760256",
            "name": "General",
            "type": 2,
            "topic": "",
            "bitrate": 64000,
            "user_limit": 0,
            "guild_id": "199737254929760256",
            "position": 0,
            "messages": [],
            "voice_states": [
                {
                    "nick": "test",
                    "mute": false,
                    "volume": 110,
                    "pan": {"left": 1.0, "right": 1.0},
                    "voice_state": {
                        "mute": false, "deaf": false, "self_mute": true,
                        "self_deaf": false, "suppress": false
                    },
                    "user": {
                        "id": "190320984123768832",
                        "username": "test 2 electric boogaloo",
                        "discriminator": "7479",
                        "avatar": "b004ec1740a63ca06ae2e14c5cee11f3",
                        "bot": false
                    }
                },
                {
                    "nick": null,
                    "mute": false,
                    "volume": 100,
                    "pan": {"left": 1.0, "right": 1.0},
                    "voice_state": {"mute": false, "deaf": false, "self_mute": false, "self_deaf": false, "suppress": false},
                    "user": {"id": "53908232506183680", "username": "Mason", "global_name": "Mason G", "avatar": null, "bot": false}
                }
            ]
        },
        "nonce": "aa0f6b3e-1f0e-4d1b-9f4c-1f1e0a2b3c4d"
    }"#;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn authorize_args_include_redirect_uri() {
        let args = authorize_args("123", &["rpc"], "http://localhost");
        assert_eq!(args["redirect_uri"], "http://localhost");
        assert_eq!(args["client_id"], "123");
        assert_eq!(args["scopes"], json!(["rpc"]));
    }

    #[test]
    fn command_payload_shape() {
        let p = command_payload(
            "SUBSCRIBE",
            json!({"channel_id": "1"}),
            Some("SPEAKING_START"),
            "n1",
        );
        assert_eq!(
            p,
            json!({"cmd": "SUBSCRIBE", "args": {"channel_id": "1"}, "evt": "SPEAKING_START", "nonce": "n1"})
        );
        let p = command_payload("GET_SELECTED_VOICE_CHANNEL", json!({}), None, "n2");
        assert!(p.get("evt").is_none());
    }

    #[test]
    fn classify_matches_nonce_and_errors() {
        let reply = v(SELECTED_CHANNEL);
        assert!(matches!(
            classify(&reply, "aa0f6b3e-1f0e-4d1b-9f4c-1f1e0a2b3c4d"),
            Incoming::Reply(Ok(_))
        ));
        assert_eq!(classify(&reply, "other"), Incoming::Unrelated);

        let err = v(
            r#"{"cmd":"AUTHORIZE","evt":"ERROR","data":{"code":5000,"message":"OAuth2 Error: access_denied"},"nonce":"n"}"#,
        );
        assert_eq!(
            classify(&err, "n"),
            Incoming::Reply(Err("code 5000: OAuth2 Error: access_denied".into()))
        );

        let evt =
            v(r#"{"cmd":"DISPATCH","evt":"SPEAKING_START","data":{"user_id":"1"},"nonce":null}"#);
        assert_eq!(classify(&evt, "n"), Incoming::Event);
    }

    #[test]
    fn parses_selected_channel_members() {
        let data = v(SELECTED_CHANNEL)["data"].clone();
        let ch = parse_selected_channel(data).unwrap().unwrap();
        assert_eq!(ch.id, "199737254929760256");
        assert_eq!(ch.name.as_deref(), Some("General"));
        assert_eq!(ch.voice_states.len(), 2);
        assert_eq!(ch.voice_states[0].user.id, "190320984123768832");
        assert_eq!(ch.voice_states[0].display_name(), "test");
        assert!(ch.voice_states[0].voice_state.as_ref().unwrap().self_mute);
        assert_eq!(ch.voice_states[1].display_name(), "Mason G");
        assert_eq!(
            ch.voice_states[0].user.avatar.as_deref(),
            Some("b004ec1740a63ca06ae2e14c5cee11f3")
        );
        assert_eq!(ch.voice_states[1].user.avatar, None);
    }

    #[test]
    fn parses_authenticated_user() {
        let data = json!({
            "access_token": "placeholder",
            "user": {"id": "42", "username": "me", "discriminator": "0", "avatar": "abc"},
            "scopes": ["rpc"]
        });
        let user = parse_authenticated_user(&data).unwrap();
        assert_eq!(user.id, "42");
        assert_eq!(user.avatar.as_deref(), Some("abc"));
        assert!(parse_authenticated_user(&json!({})).is_err());
    }

    #[test]
    fn parses_null_selected_channel() {
        assert_eq!(parse_selected_channel(Value::Null).unwrap(), None);
    }

    #[test]
    fn parses_voice_channel_select() {
        let joined = v(
            r#"{"cmd":"DISPATCH","evt":"VOICE_CHANNEL_SELECT","data":{"channel_id":"199737254929760256","guild_id":"199737254929760256"},"nonce":null}"#,
        );
        assert_eq!(
            parse_event(&joined).unwrap(),
            VoiceEvent::ChannelSelect {
                channel_id: Some("199737254929760256".into()),
                guild_id: Some("199737254929760256".into())
            }
        );
        let left = v(
            r#"{"cmd":"DISPATCH","evt":"VOICE_CHANNEL_SELECT","data":{"channel_id":null,"guild_id":null},"nonce":null}"#,
        );
        assert_eq!(
            parse_event(&left).unwrap(),
            VoiceEvent::ChannelSelect {
                channel_id: None,
                guild_id: None
            }
        );
    }

    #[test]
    fn parses_voice_state_events() {
        let member = r#"{"voice_state":{"mute":false,"deaf":false,"self_mute":false,"self_deaf":false,"suppress":false},"user":{"id":"190320984123768832","username":"test","discriminator":"0","avatar":null,"bot":false},"nick":"test","volume":100,"mute":false,"pan":{"left":1.0,"right":1.0}}"#;
        for (evt, ctor) in [
            (
                "VOICE_STATE_CREATE",
                VoiceEvent::StateCreate as fn(VoiceMember) -> VoiceEvent,
            ),
            ("VOICE_STATE_UPDATE", VoiceEvent::StateUpdate),
            ("VOICE_STATE_DELETE", VoiceEvent::StateDelete),
        ] {
            let p = v(&format!(
                r#"{{"cmd":"DISPATCH","evt":"{evt}","data":{member},"nonce":null}}"#
            ));
            let parsed = parse_event(&p).unwrap();
            let expected_member: VoiceMember = serde_json::from_str(member).unwrap();
            assert_eq!(parsed, ctor(expected_member));
        }
    }

    #[test]
    fn parses_speaking_events() {
        let start = v(
            r#"{"cmd":"DISPATCH","evt":"SPEAKING_START","data":{"channel_id":"7","user_id":"190320984123768832"},"nonce":null}"#,
        );
        assert_eq!(
            parse_event(&start).unwrap(),
            VoiceEvent::SpeakingStart {
                user_id: "190320984123768832".into(),
                channel_id: Some("7".into())
            }
        );
        let stop = v(
            r#"{"cmd":"DISPATCH","evt":"SPEAKING_STOP","data":{"user_id":"190320984123768832"},"nonce":null}"#,
        );
        assert_eq!(
            parse_event(&stop).unwrap(),
            VoiceEvent::SpeakingStop {
                user_id: "190320984123768832".into(),
                channel_id: None
            }
        );
    }

    #[test]
    fn unknown_event_is_other_and_malformed_is_error() {
        let other = v(r#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1},"nonce":null}"#);
        assert_eq!(
            parse_event(&other).unwrap(),
            VoiceEvent::Other {
                evt: "READY".into()
            }
        );
        let bad = v(r#"{"cmd":"DISPATCH","evt":"SPEAKING_START","data":{},"nonce":null}"#);
        assert!(parse_event(&bad).is_err());
    }
}
