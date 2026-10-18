//! Fetching and caching avatars and custom emoji.
//!
//! Images are downloaded once and stay on disk: avatars change rarely and
//! workspace emoji practically never, while downloading them again on every
//! start would cost hundreds of requests.
//!
//! The module knows nothing about graphics: it hands over bytes. Turning them
//! into textures is the front end's job, as the only one that knows what a
//! texture is.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::debug;

/// Images already downloaded, ready for the front end.
///
/// A synchronous mutex: the reads and writes are tiny and never held across an
/// await, and the front end can take it without blocking or failing on
/// contention (which used to skip a texture for a frame).
pub type Loaded = Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>>;

/// Upper bound on in-flight bytes. Past it the map is cleared — the disk cache
/// is untouched, so anything on screen reloads — which stops images requested
/// but never drawn (scrolled away before they arrived) from leaking.
const LOADED_MAX: usize = 4000;

/// Downloads images in the background and keeps them cached on disk.
#[derive(Clone)]
pub struct Images {
    http: reqwest::Client,
    /// Session cookie: the files on `files.slack.com` are not public, and
    /// without this they answer with a redirect to an HTML page.
    cookie: Option<String>,
    dir: PathBuf,
    loaded: Loaded,
    /// URLs already queued or completed, so as not to download the same
    /// image twice when it appears in twenty messages in a row.
    ///
    /// A synchronous mutex on purpose: the check has to happen **before** the task
    /// is created. With an async lock the check ends up inside the task, and a
    /// front end calling `request` on every frame for every avatar generates dozens
    /// of tasks a second that queue up — enough to starve the runtime and block the
    /// rest of the client.
    seen: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl Images {
    pub fn new() -> Self {
        Self::with_cookie(None)
    }

    /// With the `d` cookie, so workspace files can be downloaded too.
    pub fn with_cookie(xoxd: Option<String>) -> Self {
        let dir = cache_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            debug!("image cache not created ({}): {e}", dir.display());
        }
        Images {
            http: reqwest::Client::builder()
                .user_agent(crate::USER_AGENT)
                // Same reasoning as the API client: without a timeout a
                // stalled image host leaves the request pending forever.
                .timeout(std::time::Duration::from_secs(30))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            dir,
            cookie: xoxd.map(|d| format!("d={d}")),
            loaded: Default::default(),
            seen: Default::default(),
        }
    }

    pub fn loaded(&self) -> &Loaded {
        &self.loaded
    }

    /// Asks for the image. Returns immediately; when it is ready it appears
    /// in [`Images::loaded`].
    ///
    /// Calling it on every frame is expected: after the first time it does nothing.
    pub fn request(&self, url: &str) {
        if url.is_empty() {
            return;
        }
        // Filtered before spawning: it is the difference between one task per
        // image and one task per image per frame.
        match self.seen.lock() {
            Ok(mut seen) => {
                if !seen.insert(url.to_string()) {
                    return;
                }
            }
            Err(_) => return,
        }

        let this = self.clone();
        let url = url.to_string();
        tokio::spawn(async move {
            match this.fetch(&url).await {
                Ok(bytes) => {
                    let overflow = {
                        let mut loaded = match this.loaded.lock() {
                            Ok(l) => l,
                            Err(_) => return,
                        };
                        loaded.insert(url.clone(), bytes);
                        if loaded.len() > LOADED_MAX {
                            let keys: Vec<String> = loaded.keys().cloned().collect();
                            loaded.clear();
                            Some(keys)
                        } else {
                            None
                        }
                    };
                    if let Some(keys) = overflow {
                        for k in keys {
                            this.unsee(&k);
                        }
                    }
                }
                Err(e) => debug!("image not downloaded {url}: {e}"),
            }
        });
    }

    /// Frees the bytes of an image already turned into a texture.
    ///
    /// Without this every image stays in memory twice: here as compressed PNG and
    /// in the front end as a texture. With a few hundred emoji that is no longer
    /// negligible.
    ///
    /// It can be done because the caller keeps the texture for the whole session:
    /// the URL stays among those already seen, so it will not be downloaded again,
    /// and in any case the copy on disk is left alone.
    pub fn forget(&self, url: &str) {
        if let Ok(mut loaded) = self.loaded.lock() {
            loaded.remove(url);
        }
    }

    /// Lets a URL be requested again after its texture was evicted.
    ///
    /// `seen` otherwise keeps it out forever: `request` returns at once, the
    /// bytes are gone, and the image can never come back. That is why avatars
    /// and workspace emoji vanished once the texture cache overflowed, and fell
    /// back to the base glyphs.
    pub fn unsee(&self, url: &str) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.remove(url);
        }
        self.forget(url);
    }

    async fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        let path = self.dir.join(file_name(url));

        // A huge (or hostile) image must not be read into memory whole: the URL
        // comes from message metadata, and the response size is not bounded.
        const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if meta.len() > 0 && meta.len() <= MAX_IMAGE_BYTES as u64 {
                if let Ok(bytes) = tokio::fs::read(&path).await {
                    return Ok(bytes);
                }
            }
        }

        let mut req = self.http.get(url);
        if let Some(c) = &self.cookie {
            // Only Slack's own hosts: the URL comes from message metadata, and
            // a third-party host must not receive the session cookie.
            if crate::net::is_slack_host(url) {
                req = req.header(reqwest::header::COOKIE, c);
            }
        }
        let mut resp = req
            .send()
            .await
            .context("image request failed")?
            .error_for_status()
            .context("invalid response")?;
        // Stream it and stop at the cap, so an unbounded body cannot exhaust
        // memory before it is rejected.
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.context("image read failed")? {
            if bytes.len() + chunk.len() > MAX_IMAGE_BYTES {
                anyhow::bail!("image too large (over {MAX_IMAGE_BYTES} bytes)");
            }
            bytes.extend_from_slice(&chunk);
        }

        // Saving is best-effort: a cache that cannot be written must not stop
        // the image from being shown now.
        if let Err(e) = tokio::fs::write(&path, &bytes).await {
            debug!("image not cached: {e}");
        }
        Ok(bytes)
    }
}

impl Default for Images {
    fn default() -> Self {
        Self::new()
    }
}

/// File name derived from the URL, to avoid collisions and illegal characters.
fn file_name(url: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    let ext = url
        .rsplit('.')
        .next()
        .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("img");
    format!("{:016x}.{ext}", h.finish())
}

fn cache_dir() -> PathBuf {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"),
    };
    base.join("slaq").join("images")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_stable_and_keep_the_extension() {
        let a = file_name("https://x.test/avatar.png");
        assert_eq!(a, file_name("https://x.test/avatar.png"));
        assert!(a.ends_with(".png"), "{a}");
    }

    #[test]
    fn different_urls_get_different_names() {
        assert_ne!(
            file_name("https://x.test/a.png"),
            file_name("https://x.test/b.png")
        );
    }

    #[test]
    fn a_url_without_a_sane_extension_falls_back() {
        let n = file_name("https://x.test/avatar?size=72");
        assert!(n.ends_with(".img"), "{n}");
    }

    #[test]
    fn no_path_separators_end_up_in_the_name() {
        let n = file_name("https://x.test/a/b/c.png");
        assert!(!n.contains('/'), "{n}");
    }
}
