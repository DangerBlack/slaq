//! Downloading shared files into the downloads folder.
//!
//! Slack's files are not public: they are served with the session cookie, like
//! the images. Nothing is cached here, though — a downloaded attachment belongs
//! to the user, not to the client, and goes where they expect to find it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Downloads `url` into the downloads folder and returns the path.
///
/// The name is made unique: overwriting a file the user has already downloaded
/// would be a silent loss.
pub async fn fetch(url: &str, filename: &str, xoxd: &str) -> Result<PathBuf> {
    fetch_with(url, filename, xoxd, |_, _| {}).await
}

/// Like [`fetch`], reporting progress as `(downloaded, total)` while it runs.
///
/// `total` is `None` when Slack sends no `Content-Length`; the downloaded byte
/// count is still reported, so the caller can show something. A file that fails
/// half way is removed: a truncated download that stays on disk looks complete.
pub async fn fetch_with<F>(
    url: &str,
    filename: &str,
    xoxd: &str,
    mut on_progress: F,
) -> Result<PathBuf>
where
    F: FnMut(u64, Option<u64>) + Send,
{
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    let dir = downloads_dir()?;
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("unable to create {}", dir.display()))?;

    // Downloading needs the session cookie; refuse to hand it to a host that
    // is not Slack's, so a crafted download URL cannot exfiltrate it.
    if !crate::net::is_slack_host(url) {
        anyhow::bail!("refusing to send the session cookie to a non-Slack host: {url}");
    }

    let http = reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        // Generous: an attachment can be large, and the API's short timeout
        // would be an arbitrary limit here.
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("HTTP client not created")?;

    let resp = http
        .get(url)
        .header(reqwest::header::COOKIE, format!("d={xoxd}"))
        .send()
        .await
        .context("file request failed")?
        .error_for_status()
        .context("the server rejected the request")?;

    // Without the cookie Slack answers 200 with a login page: the content
    // type is the only way to notice.
    if let Some(ct) = resp.headers().get(reqwest::header::CONTENT_TYPE) {
        let ct = ct.to_str().unwrap_or_default();
        if ct.starts_with("text/html") && !filename.ends_with(".html") {
            anyhow::bail!("Slack returned an HTML page instead of the file");
        }
    }

    let total = resp.content_length();
    let path = unique_path(&dir, filename);
    let mut file = tokio::fs::File::create(&path)
        .await
        .with_context(|| format!("creation of {} failed", path.display()))?;
    let mut downloaded: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => {
                drop(file);
                tokio::fs::remove_file(&path).await.ok();
                return Err(e).context("file read failed");
            }
        };
        if let Err(e) = file.write_all(&bytes).await {
            drop(file);
            tokio::fs::remove_file(&path).await.ok();
            return Err(e).with_context(|| format!("write of {} failed", path.display()));
        }
        downloaded += bytes.len() as u64;
        on_progress(downloaded, total);
    }
    Ok(path)
}

/// Opens the file with the system's default application.
pub fn open(path: &Path) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("xdg-open on {} failed", path.display()))?;
    Ok(())
}

/// `$XDG_DOWNLOAD_DIR` if configured, otherwise `~/Download` or
/// `~/Downloads` if they exist, otherwise `~/Downloads`.
fn downloads_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_DOWNLOAD_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME not set")?);
    // user-dirs.dirs is where Linux desktops record the translated folder.
    if let Ok(conf) = std::fs::read_to_string(home.join(".config/user-dirs.dirs")) {
        for line in conf.lines() {
            if let Some(v) = line.strip_prefix("XDG_DOWNLOAD_DIR=") {
                let v = v
                    .trim_matches('"')
                    .replace("$HOME", &home.to_string_lossy());
                if !v.is_empty() {
                    return Ok(PathBuf::from(v));
                }
            }
        }
    }
    for c in ["Download", "Downloads", "Scaricati"] {
        let p = home.join(c);
        if p.is_dir() {
            return Ok(p);
        }
    }
    Ok(home.join("Downloads"))
}

/// `photo.png` -> `photo.png`, `photo-1.png`, `photo-2.png`…
fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let safe = sanitize(filename);
    let first = dir.join(&safe);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match safe.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (safe.clone(), String::new()),
    };
    for i in 1..1000 {
        let p = dir.join(format!("{stem}-{i}{ext}"));
        if !p.exists() {
            return p;
        }
    }
    dir.join(safe)
}

/// Strips separators and path components: the name comes from Slack and
/// must not be able to write outside the downloads folder.
fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || c == '\0' { '_' } else { c })
        .collect();
    match cleaned.trim() {
        "" | "." | ".." => "file".to_string(),
        s => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_traversal_cannot_escape_the_directory() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("/etc/shadow"), "shadow");
    }

    #[test]
    fn degenerate_names_get_a_fallback() {
        for n in ["", "   ", ".", ".."] {
            assert_eq!(sanitize(n), "file", "{n:?}");
        }
    }

    #[test]
    fn ordinary_names_are_untouched() {
        assert_eq!(sanitize("relazione 2026.pdf"), "relazione 2026.pdf");
    }

    #[test]
    fn collisions_get_a_suffix_keeping_the_extension() {
        let d = std::env::temp_dir().join(format!("slackrs-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let first = unique_path(&d, "nota.pdf");
        assert_eq!(first.file_name().unwrap(), "nota.pdf");
        std::fs::write(&first, b"x").unwrap();
        assert_eq!(
            unique_path(&d, "nota.pdf").file_name().unwrap(),
            "nota-1.pdf"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_name_without_extension_still_gets_a_suffix() {
        let d = std::env::temp_dir().join(format!("slackrs-test2-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("LEGGIMI"), b"x").unwrap();
        assert_eq!(unique_path(&d, "LEGGIMI").file_name().unwrap(), "LEGGIMI-1");
        std::fs::remove_dir_all(&d).ok();
    }
}
