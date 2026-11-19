//! Read Slack's live local storage through a short-lived private RAM snapshot.
use super::TeamEntry;
use anyhow::{bail, Context, Result};
use rusty_leveldb::{LdbIterator, Options, DB};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::Path;
use std::time::SystemTime;

const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;

fn files(dir: &Path) -> Result<BTreeMap<OsString, (u64, SystemTime)>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if n == "CURRENT" || n.starts_with("MANIFEST-") || n.ends_with(".ldb")
            || n.ends_with(".sst") || n.ends_with(".log")
        {
            if !entry.file_type()?.is_file() {
                bail!("Unexpected non-file in Slack local storage");
            }
            let m = entry.metadata()?;
            files.insert(name, (m.len(), m.modified()?));
        }
    }
    if !files.contains_key(std::ffi::OsStr::new("CURRENT")) {
        bail!("Slack local storage CURRENT is missing");
    }
    let size = files.values().try_fold(0u64, |a, (n, _)| a.checked_add(*n))
        .context("Slack local storage size overflow")?;
    if size > MAX_SNAPSHOT_BYTES {
        bail!("Slack local storage exceeds RAM snapshot limit");
    }
    Ok(files)
}

fn require_tmpfs() -> Result<()> {
    let path = std::ffi::CString::new("/dev/shm")?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is a NUL-terminated constant and stat points to writable memory.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot inspect /dev/shm");
    }
    // SAFETY: statfs returned success and initialized the output structure.
    let stat = unsafe { stat.assume_init() };
    if stat.f_type as u64 != 0x0102_1994 {
        bail!("/dev/shm is not tmpfs; refusing to copy session data to disk");
    }
    Ok(())
}

fn decode(value: &[u8]) -> Result<String> {
    match value.split_first() {
        Some((1, bytes)) => Ok(bytes.iter().map(|&b| char::from(b)).collect()),
        Some((0, bytes)) if bytes.len() % 2 == 0 => {
            let words: Vec<u16> = bytes.chunks_exact(2)
                .map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
            String::from_utf16(&words).context("Invalid UTF-16 local storage value")
        }
        _ => bail!("Unsupported local storage string encoding"),
    }
}

#[derive(Deserialize)]
struct Config {
    teams: HashMap<String, Team>,
}

#[derive(Deserialize)]
struct Team {
    #[serde(default)] token: String,
    #[serde(default)] url: String,
    #[serde(default)] user_id: String,
}

fn parse(value: &[u8]) -> Result<Vec<TeamEntry>> {
    let json = decode(value)?;
    let config: Config = serde_json::from_str(&json).context("Invalid Slack local configuration")?;
    let mut result = Vec::new();
    for (team_id, team) in config.teams {
        if !team.token.starts_with("xoxc-") { continue; }
        let url = url::Url::parse(&team.url).context("Invalid workspace URL")?;
        let host = url.host_str().unwrap_or_default();
        if url.scheme() != "https" || !host.ends_with(".slack.com") || url.username() != ""
            || url.password().is_some() || url.port().is_some()
        {
            bail!("Refusing a non-Slack workspace URL");
        }
        result.push(TeamEntry { url: team.url, token: team.token, user_id: team.user_id, team_id });
    }
    result.sort_by(|a, b| a.url.cmp(&b.url));
    Ok(result)
}

pub(super) fn extract(slack_dir: &Path) -> Result<Vec<TeamEntry>> {
    require_tmpfs()?;
    let source = slack_dir.join("Local Storage").join("leveldb");
    let before = files(&source).context("Failed to inspect Slack local storage")?;
    let snapshot = tempfile::Builder::new().prefix("slaq-leveldb-")
        .tempdir_in("/dev/shm").context("Failed to create private RAM snapshot")?;
    for name in before.keys() {
        std::fs::copy(source.join(name), snapshot.path().join(name))
            .context("Failed to copy Slack local storage snapshot")?;
    }
    if before != files(&source).context("Failed to recheck Slack local storage")? {
        bail!("Slack local storage changed during snapshot; retry");
    }
    let mut options = Options::default();
    options.create_if_missing = false;
    let mut db = DB::open(snapshot.path(), options).context("Failed to open RAM snapshot")?;
    let mut iter = db.new_iter().context("Failed to read RAM snapshot")?;
    let mut config = None;
    while let Some((key, value)) = iter.next() {
        if key.ends_with(b"localConfig_v2") {
            if config.is_some() { bail!("Multiple Slack local configuration records found"); }
            config = Some(parse(&value)?);
        }
    }
    config.context("Slack local configuration was not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_latin1_and_rejects_non_slack_hosts() {
        let json = r#"{"teams":{"T01":{"token":"xoxc-test","url":"https://example.slack.com/","user_id":"U01"}}}"#;
        let mut bytes = vec![1]; bytes.extend_from_slice(json.as_bytes());
        let teams = parse(&bytes).unwrap();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].team_id, "T01");
        let bad = json.replace("example.slack.com", "example.invalid");
        let mut bytes = vec![1]; bytes.extend_from_slice(bad.as_bytes());
        assert!(parse(&bytes).is_err());
    }

    #[test]
    fn decodes_utf16_and_fails_closed_on_invalid_encoding() {
        let mut bytes = vec![0];
        for ch in "hello".encode_utf16() { bytes.extend_from_slice(&ch.to_le_bytes()); }
        assert_eq!(decode(&bytes).unwrap(), "hello");
        assert!(decode(&[0, 1]).is_err());
        assert!(decode(&[2]).is_err());
    }
}
