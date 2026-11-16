//! Avatar download and cache.
//!
//! Avatars are fetched only from `https://cdn.discordapp.com`, stored as
//! `$XDG_CACHE_HOME/discord-voice/avatars/<user id>_<hash>.png` (default
//! avatars as `default_<n>.png`) and kept in memory. A cached file is never
//! downloaded again. Failures fall back to the default avatar and are only
//! logged.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Url;

pub const CDN_HOST: &str = "cdn.discordapp.com";
const AVATAR_SIZE: u32 = 64;
const MAX_AVATAR_BYTES: usize = 2 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Returns an error unless `url` is `https://cdn.discordapp.com/...`.
pub fn check_url(url: &Url) -> Result<()> {
    if url.scheme() != "https"
        || url.host_str() != Some(CDN_HOST)
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!(
            "refusing to fetch avatar from non-allowed URL host {:?}",
            url.host_str()
        );
    }
    Ok(())
}

/// HTTP fetch seam, so tests run without network.
pub trait Fetcher {
    fn fetch(&self, url: &Url) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

/// Real fetcher: timeouts, no redirects (they could leave the allowed host),
/// size limit.
pub struct HttpFetcher {
    client: reqwest::Client,
}

impl HttpFetcher {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("cannot build HTTP client")?;
        Ok(Self { client })
    }
}

impl Fetcher for HttpFetcher {
    fn fetch(&self, url: &Url) -> impl Future<Output = Result<Vec<u8>>> + Send {
        let url = url.clone();
        let client = self.client.clone();
        async move {
            check_url(&url)?;
            let mut resp = client.get(url).send().await?.error_for_status()?;
            if resp.content_length().unwrap_or(0) > MAX_AVATAR_BYTES as u64 {
                bail!("avatar too large");
            }
            let mut body = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                if body.len() + chunk.len() > MAX_AVATAR_BYTES {
                    bail!("avatar too large");
                }
                body.extend_from_slice(&chunk);
            }
            if body.is_empty() {
                bail!("empty avatar response");
            }
            Ok(body)
        }
    }
}

/// A resolved avatar: file on disk plus its raw (PNG) bytes in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avatar {
    pub path: PathBuf,
    pub bytes: Arc<[u8]>,
}

/// `$XDG_CACHE_HOME/discord-voice/avatars`.
pub fn default_cache_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir().ok_or_else(|| anyhow!("cannot determine cache directory"))?;
    Ok(base.join("discord-voice").join("avatars"))
}

/// Index of Discord's default avatar: legacy discriminator mod 5, otherwise
/// `(user id >> 22) mod 6`.
pub fn default_avatar_index(user_id: &str, discriminator: Option<&str>) -> u64 {
    match discriminator.and_then(|d| d.parse::<u64>().ok()) {
        Some(d) if d != 0 => d % 5,
        _ => user_id.parse::<u64>().map(|id| (id >> 22) % 6).unwrap_or(0),
    }
}

/// Ids and hashes end up in file names and URLs: allow only safe characters.
fn is_safe_component(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

struct Source {
    key: String,
    url: Url,
}

fn user_source(user_id: &str, hash: &str) -> Option<Source> {
    if !is_safe_component(user_id) || !is_safe_component(hash) {
        return None;
    }
    let url = format!("https://{CDN_HOST}/avatars/{user_id}/{hash}.png?size={AVATAR_SIZE}");
    Some(Source {
        key: format!("{user_id}_{hash}"),
        url: Url::parse(&url).ok()?,
    })
}

fn default_source(index: u64) -> Source {
    Source {
        key: format!("default_{index}"),
        url: Url::parse(&format!("https://{CDN_HOST}/embed/avatars/{index}.png"))
            .expect("static default avatar URL is valid"),
    }
}

pub struct AvatarCache<F> {
    dir: PathBuf,
    fetcher: F,
    memory: HashMap<String, Avatar>,
    /// Keys whose download failed this session; not retried.
    failed: HashSet<String>,
}

impl<F: Fetcher> AvatarCache<F> {
    pub fn new(dir: PathBuf, fetcher: F) -> Self {
        Self {
            dir,
            fetcher,
            memory: HashMap::new(),
            failed: HashSet::new(),
        }
    }

    /// Resolves a user's avatar, falling back to the default avatar when the
    /// user has none or the download fails. `None` only if the default avatar
    /// is unavailable too. Never panics on I/O or network errors.
    pub async fn get(
        &mut self,
        user_id: &str,
        avatar_hash: Option<&str>,
        discriminator: Option<&str>,
    ) -> Option<Avatar> {
        if let Some(src) = avatar_hash.and_then(|h| user_source(user_id, h)) {
            if let Some(a) = self.resolve(&src).await {
                return Some(a);
            }
        }
        let src = default_source(default_avatar_index(user_id, discriminator));
        self.resolve(&src).await
    }

    async fn resolve(&mut self, src: &Source) -> Option<Avatar> {
        if let Some(a) = self.memory.get(&src.key) {
            return Some(a.clone());
        }
        if self.failed.contains(&src.key) {
            return None;
        }
        let path = self.dir.join(format!("{}.png", src.key));
        if let Ok(bytes) = tokio::fs::read(&path).await {
            if !bytes.is_empty() {
                return Some(self.remember(&src.key, path, bytes));
            }
        }
        match self.download(&src.url, &path).await {
            Ok(bytes) => Some(self.remember(&src.key, path, bytes)),
            Err(e) => {
                tracing::warn!(key = %src.key, error = %e, "avatar download failed");
                self.failed.insert(src.key.clone());
                None
            }
        }
    }

    fn remember(&mut self, key: &str, path: PathBuf, bytes: Vec<u8>) -> Avatar {
        let avatar = Avatar {
            path,
            bytes: bytes.into(),
        };
        self.memory.insert(key.to_owned(), avatar.clone());
        avatar
    }

    async fn download(&self, url: &Url, path: &Path) -> Result<Vec<u8>> {
        check_url(url)?;
        let bytes = self.fetcher.fetch(url).await?;
        write_atomic(path, &bytes).await?;
        Ok(bytes)
    }
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("avatar path has no parent"))?;
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    tokio::fs::write(&tmp, bytes)
        .await
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e).with_context(|| format!("cannot write {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Serves canned bodies; any URL not listed fails. Records requests.
    #[derive(Clone, Default)]
    struct MockFetcher {
        bodies: Arc<HashMap<String, Vec<u8>>>,
        calls: Arc<AtomicUsize>,
        urls: Arc<Mutex<Vec<String>>>,
    }

    impl MockFetcher {
        fn with(bodies: &[(&str, &[u8])]) -> Self {
            Self {
                bodies: Arc::new(
                    bodies
                        .iter()
                        .map(|(u, b)| (u.to_string(), b.to_vec()))
                        .collect(),
                ),
                ..Default::default()
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Fetcher for MockFetcher {
        fn fetch(&self, url: &Url) -> impl Future<Output = Result<Vec<u8>>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.urls.lock().unwrap().push(url.to_string());
            let body = self.bodies.get(url.as_str()).cloned();
            async move { body.ok_or_else(|| anyhow!("mock: not found")) }
        }
    }

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "discord-voice-avatars-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            Self(p)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const USER_URL: &str = "https://cdn.discordapp.com/avatars/123/abc.png?size=64";

    #[test]
    fn allow_list_accepts_only_cdn_https() {
        let ok = |s: &str| check_url(&Url::parse(s).unwrap()).is_ok();
        assert!(ok("https://cdn.discordapp.com/avatars/1/a.png"));
        assert!(!ok("http://cdn.discordapp.com/avatars/1/a.png"));
        assert!(!ok("https://media.discordapp.net/avatars/1/a.png"));
        assert!(!ok("https://cdn.discordapp.com.evil.example/a.png"));
        assert!(!ok("https://evil.example/cdn.discordapp.com/a.png"));
        assert!(!ok("https://cdn.discordapp.com:8443/a.png"));
        assert!(!ok("https://user@cdn.discordapp.com/a.png"));
    }

    #[tokio::test]
    async fn http_fetcher_rejects_other_hosts_without_network() {
        let f = HttpFetcher::new().unwrap();
        let err = f
            .fetch(&Url::parse("https://127.0.0.1:1/x.png").unwrap())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing"));
    }

    #[test]
    fn unsafe_ids_or_hashes_build_no_url() {
        assert!(user_source("123", "abc").is_some());
        assert!(user_source("123", "a_abc").is_some());
        assert!(user_source("../x", "abc").is_none());
        assert!(user_source("123", "abc/../../x").is_none());
        assert!(user_source("123", "x?y").is_none());
        assert!(user_source("123", "").is_none());
    }

    #[test]
    fn default_index() {
        assert_eq!(default_avatar_index("1", Some("7479")), 7479 % 5);
        let id = "80351110224678912";
        let expected = (80351110224678912u64 >> 22) % 6;
        assert_eq!(default_avatar_index(id, Some("0")), expected);
        assert_eq!(default_avatar_index(id, None), expected);
        assert_eq!(default_avatar_index("garbage", None), 0);
    }

    #[tokio::test]
    async fn downloads_once_then_hits_memory_and_disk() {
        let tmp = TmpDir::new("hit");
        let fetcher = MockFetcher::with(&[(USER_URL, b"png-a")]);
        let mut cache = AvatarCache::new(tmp.0.clone(), fetcher.clone());
        let a = cache.get("123", Some("abc"), None).await.unwrap();
        assert_eq!(&*a.bytes, b"png-a");
        assert_eq!(a.path, tmp.0.join("123_abc.png"));
        assert_eq!(std::fs::read(&a.path).unwrap(), b"png-a");
        assert_eq!(fetcher.calls(), 1);

        // Memory hit.
        assert_eq!(cache.get("123", Some("abc"), None).await.unwrap(), a);
        assert_eq!(fetcher.calls(), 1);

        // Second run: fresh cache over the same dir downloads nothing.
        let fetcher2 = MockFetcher::default();
        let mut cache2 = AvatarCache::new(tmp.0.clone(), fetcher2.clone());
        let b = cache2.get("123", Some("abc"), None).await.unwrap();
        assert_eq!(b, a);
        assert_eq!(fetcher2.calls(), 0);
    }

    #[tokio::test]
    async fn new_hash_is_a_new_key() {
        let tmp = TmpDir::new("rehash");
        let fetcher = MockFetcher::with(&[
            (USER_URL, b"old"),
            (
                "https://cdn.discordapp.com/avatars/123/def.png?size=64",
                b"new",
            ),
        ]);
        let mut cache = AvatarCache::new(tmp.0.clone(), fetcher.clone());
        cache.get("123", Some("abc"), None).await.unwrap();
        let b = cache.get("123", Some("def"), None).await.unwrap();
        assert_eq!(&*b.bytes, b"new");
        assert_eq!(fetcher.calls(), 2);
    }

    #[tokio::test]
    async fn no_avatar_uses_default() {
        let tmp = TmpDir::new("noavatar");
        let id = "80351110224678912";
        let n = default_avatar_index(id, None);
        let url = format!("https://cdn.discordapp.com/embed/avatars/{n}.png");
        let fetcher = MockFetcher::with(&[(&url, b"default")]);
        let mut cache = AvatarCache::new(tmp.0.clone(), fetcher.clone());
        let a = cache.get(id, None, Some("0")).await.unwrap();
        assert_eq!(&*a.bytes, b"default");
        assert_eq!(a.path, tmp.0.join(format!("default_{n}.png")));
        assert_eq!(*fetcher.urls.lock().unwrap(), [url]);
    }

    #[tokio::test]
    async fn failed_download_falls_back_to_default_and_is_not_retried() {
        let tmp = TmpDir::new("fail");
        let n = default_avatar_index("123", Some("7479"));
        let url = format!("https://cdn.discordapp.com/embed/avatars/{n}.png");
        let fetcher = MockFetcher::with(&[(&url, b"default")]);
        let mut cache = AvatarCache::new(tmp.0.clone(), fetcher.clone());
        let a = cache.get("123", Some("abc"), Some("7479")).await.unwrap();
        assert_eq!(&*a.bytes, b"default");
        assert_eq!(fetcher.calls(), 2);
        assert!(!tmp.0.join("123_abc.png").exists());

        cache.get("123", Some("abc"), Some("7479")).await.unwrap();
        assert_eq!(fetcher.calls(), 2);
    }

    #[tokio::test]
    async fn everything_failing_returns_none() {
        let tmp = TmpDir::new("allfail");
        let mut cache = AvatarCache::new(tmp.0.clone(), MockFetcher::default());
        assert_eq!(cache.get("123", Some("abc"), None).await, None);
    }

    #[tokio::test]
    async fn unsafe_hash_goes_straight_to_default() {
        let tmp = TmpDir::new("unsafe");
        let fetcher = MockFetcher::default();
        let mut cache = AvatarCache::new(tmp.0.clone(), fetcher.clone());
        cache.get("123", Some("../../etc"), None).await;
        let urls = fetcher.urls.lock().unwrap();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("https://cdn.discordapp.com/embed/avatars/"));
    }

    #[tokio::test]
    async fn unwritable_cache_dir_is_a_soft_failure() {
        let tmp = TmpDir::new("unwritable");
        std::fs::write(&tmp.0, b"not a dir").unwrap();
        let fetcher = MockFetcher::with(&[(USER_URL, b"png")]);
        let mut cache = AvatarCache::new(tmp.0.join("avatars"), fetcher);
        assert_eq!(cache.get("123", Some("abc"), None).await, None);
        let _ = std::fs::remove_file(&tmp.0);
    }
}
