//! OAuth for the Discord RPC: config loading, token storage, code exchange,
//! refresh and AUTHENTICATE. Secrets never reach logs: `Debug` is redacted and
//! HTTP errors only surface the OAuth error code.

use std::fmt;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::rpc::{RpcClient, User};

pub const SCOPES: &[&str] = &["rpc", "rpc.voice.read"];
const TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
const DEFAULT_REDIRECT_URI: &str = "http://localhost";
/// Refresh this many seconds before the token actually expires.
const EXPIRY_MARGIN_SECS: u64 = 60;

#[derive(Deserialize)]
pub struct Config {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_redirect_uri")]
    pub redirect_uri: String,
}

fn default_redirect_uri() -> String {
    DEFAULT_REDIRECT_URI.to_owned()
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Token {
    pub fn is_expired(&self, now: u64) -> bool {
        now + EXPIRY_MARGIN_SECS >= self.expires_at
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
}

/// The token endpoint answered with an error status (e.g. `invalid_grant`
/// for a revoked or expired refresh token), as opposed to a network error.
#[derive(Debug)]
pub struct TokenRejected {
    pub status: reqwest::StatusCode,
    pub code: String,
}

impl fmt::Display for TokenRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "token endpoint returned {} ({})", self.status, self.code)
    }
}

impl std::error::Error for TokenRejected {}

/// Whether a refresh error means the refresh token itself is unusable, so
/// the user must authorize again. Network errors keep the token.
pub fn refresh_token_rejected(err: &anyhow::Error) -> bool {
    err.downcast_ref::<TokenRejected>()
        .is_some_and(|r| r.status.is_client_error())
}

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("cannot determine config directory"))?;
    Ok(base.join("discord-voice"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fails if the file is readable or writable by group/others.
fn check_private(path: &Path) -> Result<()> {
    let mode = std::fs::metadata(path)
        .with_context(|| format!("cannot stat {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} has mode {:o}; it must be private: run `chmod 600 {}`",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    Ok(())
}

pub fn parse_config(text: &str) -> Result<Config> {
    let cfg: Config =
        toml::from_str(text).map_err(|e| anyhow!("invalid config: {}", e.message()))?;
    if cfg.client_id.trim().is_empty() || cfg.client_secret.trim().is_empty() {
        bail!("config needs non-empty client_id and client_secret");
    }
    Ok(cfg)
}

/// Reads the config file, refusing it if missing or not private.
pub fn read_private_config(path: &Path) -> Result<String> {
    if !path.exists() {
        bail!(
            "config file {} not found; see README.md for setup",
            path.display()
        );
    }
    check_private(path)?;
    std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))
}

/// Credentials only; the binary uses `config::load`, which also reads the
/// optional settings from the same file.
#[allow(dead_code)]
pub fn load_config(path: &Path) -> Result<Config> {
    let text = read_private_config(path)?;
    parse_config(&text).with_context(|| format!("in {}", path.display()))
}

/// Deletes a token that Discord no longer accepts, so the next start does
/// not try it again.
pub fn discard_token(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::info!(path = %path.display(), "deleted rejected token"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "cannot delete token"),
    }
}

pub fn load_token(path: &Path) -> Option<Token> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(t) => Some(t),
        Err(_) => {
            tracing::warn!(path = %path.display(), "ignoring unreadable token file");
            None
        }
    }
}

/// Writes the token atomically with mode 0600.
pub fn save_token(path: &Path, token: &Token) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("token path has no parent"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    file.write_all(&serde_json::to_vec_pretty(token)?)?;
    file.sync_all()?;
    drop(file);
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

async fn token_request(form: &[(&str, &str)]) -> Result<Token> {
    let resp = reqwest::Client::new()
        .post(TOKEN_URL)
        .form(form)
        .send()
        .await
        .map_err(|e| anyhow!("token request failed: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        let code = resp
            .json::<OAuthError>()
            .await
            .map(|e| e.error)
            .unwrap_or_else(|_| "unknown".into());
        return Err(TokenRejected { status, code }.into());
    }
    let body: TokenResponse = resp
        .json()
        .await
        .map_err(|_| anyhow!("token endpoint returned an unexpected body"))?;
    Ok(Token {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
        expires_at: now_secs() + body.expires_in,
    })
}

async fn exchange_code(cfg: &Config, code: &str) -> Result<Token> {
    token_request(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", &cfg.client_id),
        ("client_secret", &cfg.client_secret),
        ("redirect_uri", &cfg.redirect_uri),
    ])
    .await
}

async fn refresh(cfg: &Config, token: &Token) -> Result<Token> {
    token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &token.refresh_token),
        ("client_id", &cfg.client_id),
        ("client_secret", &cfg.client_secret),
    ])
    .await
}

async fn authorize(rpc: &mut RpcClient, cfg: &Config) -> Result<Token> {
    tracing::info!("requesting authorization: approve the prompt in the Discord client");
    let code = rpc.authorize(&cfg.client_id, SCOPES).await?;
    exchange_code(cfg, &code).await
}

/// Authenticates the RPC session: stored token, then refresh, then a fresh
/// AUTHORIZE as the last resort. Persists any new token. Returns the
/// authenticated user.
pub async fn authenticate(rpc: &mut RpcClient, cfg: &Config, token_path: &Path) -> Result<User> {
    if let Some(token) = load_token(token_path) {
        if !token.is_expired(now_secs()) {
            match rpc.authenticate(&token.access_token).await {
                Ok(user) => return Ok(user),
                Err(e) => tracing::warn!(error = %e, "stored token rejected; refreshing"),
            }
        }
        match refresh(cfg, &token).await {
            Ok(new) => {
                save_token(token_path, &new)?;
                match rpc.authenticate(&new.access_token).await {
                    Ok(user) => return Ok(user),
                    Err(e) => {
                        tracing::warn!(error = %e, "refreshed token rejected; re-authorizing");
                        discard_token(token_path);
                    }
                }
            }
            Err(e) if refresh_token_rejected(&e) => {
                tracing::warn!(error = %e, "refresh token rejected or expired; re-authorizing");
                discard_token(token_path);
            }
            // Network trouble: keep the token and let the caller retry later.
            Err(e) => return Err(e.context("token refresh failed")),
        }
    }
    let token = authorize(rpc, cfg).await?;
    save_token(token_path, &token)?;
    rpc.authenticate(&token.access_token)
        .await
        .context("AUTHENTICATE failed with a fresh token")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "discord-voice-test-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_config_with_default_redirect() {
        let cfg = parse_config("client_id = \"123\"\nclient_secret = \"placeholder\"\n").unwrap();
        assert_eq!(cfg.client_id, "123");
        assert_eq!(cfg.redirect_uri, DEFAULT_REDIRECT_URI);
        assert!(parse_config("client_id = \"123\"\nclient_secret = \"\"\n").is_err());
    }

    #[test]
    fn debug_redacts_secrets() {
        let cfg =
            parse_config("client_id = \"123\"\nclient_secret = \"placeholder-secret\"\n").unwrap();
        assert!(!format!("{cfg:?}").contains("placeholder-secret"));
        let t = Token {
            access_token: "aaa-x".into(),
            refresh_token: "rrr-x".into(),
            expires_at: 5,
        };
        let s = format!("{t:?}");
        assert!(!s.contains("aaa-x") && !s.contains("rrr-x"));
    }

    #[test]
    fn expiry_uses_margin() {
        let t = Token {
            access_token: String::new(),
            refresh_token: String::new(),
            expires_at: 1000,
        };
        assert!(!t.is_expired(900));
        assert!(t.is_expired(950));
        assert!(t.is_expired(2000));
    }

    #[test]
    fn token_saved_with_0600_and_roundtrips() {
        let dir = tmp_dir("token");
        let path = dir.join("sub").join("token.json");
        let t = Token {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 42,
        };
        save_token(&path, &t).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(load_token(&path), Some(t.clone()));
        save_token(&path, &t).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejected_refresh_is_told_apart_from_network_errors() {
        let rejected: anyhow::Error = TokenRejected {
            status: reqwest::StatusCode::BAD_REQUEST,
            code: "invalid_grant".into(),
        }
        .into();
        assert!(refresh_token_rejected(&rejected));
        assert_eq!(
            rejected.to_string(),
            "token endpoint returned 400 Bad Request (invalid_grant)"
        );
        let server: anyhow::Error = TokenRejected {
            status: reqwest::StatusCode::BAD_GATEWAY,
            code: "unknown".into(),
        }
        .into();
        assert!(!refresh_token_rejected(&server));
        assert!(!refresh_token_rejected(&anyhow!(
            "token request failed: timeout"
        )));
    }

    #[test]
    fn discard_token_deletes_file() {
        let dir = tmp_dir("discard");
        let path = dir.join("token.json");
        std::fs::write(&path, "{}").unwrap();
        discard_token(&path);
        assert!(!path.exists());
        discard_token(&path); // missing file is fine
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_with_open_mode_is_rejected() {
        let dir = tmp_dir("config");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "client_id = \"1\"\nclient_secret = \"placeholder\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_config(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load_config(&path).unwrap().client_id, "1");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
