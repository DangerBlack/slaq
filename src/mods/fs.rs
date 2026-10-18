//! The `filesystem` capability (`fs-list`/`fs-read`/`fs-write`/`fs-search`).
//!
//! A mod asks for **named slots**; the **user** maps each slot to a real
//! directory. The mod passes `(slot, relative-path)` and the **host** does every
//! disk access — the guest is wasm with no filesystem, so it never sees a path.
//!
//! Containment is the whole point of this module:
//!
//! - the guest path is **relative**; only `Normal` components are accepted, so
//!   `..` and an absolute path are refused before any I/O;
//! - every resolved path is `canonicalize`d (symlinks resolved) and must still be
//!   under the slot root **and** outside slaq's own config/data dirs — a
//!   canonicalize that escapes the root (a symlink out) is refused, and mapping
//!   `~` cannot hand a mod slaq's credentials or consent store;
//! - symlinks are never listed nor traversed (so they cannot cause an escape or a
//!   loop), and a **write** is `create_new` (no overwrite, no symlink follow);
//! - every operation is bounded (entries, bytes, hits, pattern), failing loudly
//!   rather than silently truncating.
//!
//! `filesystem` combined with `net.egress` is exfiltration; that is a disclosure
//! in `docs/mods/capabilities.md`, not something this module can prevent.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// The most slots one manifest may declare.
pub const MAX_FS_SLOTS: usize = 8;
/// A slot name's byte cap (`manifest.schema.json` mirrors it with `maxLength`).
pub const MAX_FS_SLOT_NAME_BYTES: usize = 32;

/// A guest path's byte cap.
const MAX_FS_PATH_BYTES: usize = 4096;
/// Entries one `fs-list` may return; more is an error (never a silent truncate).
pub const MAX_FS_ENTRIES: u64 = 4096;
/// The most bytes one `fs-read` may return (a larger file is refused).
const MAX_FS_READ_BYTES: u64 = 4 * 1024 * 1024;
/// The most bytes one `fs-write` may create.
const MAX_FS_WRITE_BYTES: usize = 4 * 1024 * 1024;
/// The longest search pattern.
const MAX_FS_PATTERN_BYTES: usize = 256;
/// The largest single file `fs-search` will scan (larger files are skipped).
const MAX_FS_SCAN_FILE_BYTES: u64 = 1024 * 1024;
/// Hits one `fs-search` may return; more is an error.
pub const MAX_FS_HITS: u64 = 512;
/// Aggregate scanned bytes, and files, for one `fs-search`.
const MAX_FS_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FS_SCAN_FILES: u64 = 4096;
/// A returned match line is truncated to this many bytes (char-boundary safe).
const MAX_FS_LINE_BYTES: usize = 512;

/// A slot's declared access, from the manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsAccess {
    Read,
    Write,
}

impl FsAccess {
    /// The manifest spelling (`"read"`/`"write"`), for display and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            FsAccess::Read => "read",
            FsAccess::Write => "write",
        }
    }

    /// The **narrower** of two accesses (`read` wins over `write`). Used so the
    /// enforced access is `min(consent, manifest)`: a later manifest cannot widen
    /// a slot the user approved for reading only.
    pub fn narrow(self, other: FsAccess) -> FsAccess {
        match (self, other) {
            (FsAccess::Write, FsAccess::Write) => FsAccess::Write,
            _ => FsAccess::Read,
        }
    }
}

/// A manifest-declared slot: a name and the access the mod asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsSlot {
    pub name: String,
    pub access: FsAccess,
}

/// What a listed entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    File,
    Dir,
}

/// A directory entry, with a path **relative to the slot root**.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsEntry {
    pub path: String,
    pub kind: FsKind,
    pub size: u64,
}

/// A search match: a path (relative to the slot root), a 1-based line number and
/// the line's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsHit {
    pub path: String,
    pub line: u64,
    pub text: String,
}

/// One resolved slot: a canonical root and the access the guest was granted.
struct SlotRoot {
    root: PathBuf,
    access: FsAccess,
}

/// A walk result (internal): the canonical path, its slot-relative string, kind
/// and size.
struct Walked {
    canon: PathBuf,
    rel: String,
    kind: FsKind,
    size: u64,
}

/// The host-side sandbox for one mod. Built once at load from the **effective**
/// slot mapping (consent ∩ manifest); every operation goes through it.
pub struct FsSandbox {
    slots: BTreeMap<String, SlotRoot>,
    /// Canonical directories a slot root, and any resolved path, must not be
    /// inside (slaq's own config and data dirs).
    denied: Vec<PathBuf>,
}

impl FsSandbox {
    /// Builds the sandbox from `slot -> (absolute dir, access)`. A slot whose root
    /// does not exist, is not a directory, or is inside a denied prefix is
    /// **skipped** (the slot becomes inert) rather than failing the block: a bad
    /// mapping must not take the whole mod down.
    pub fn new(mapping: &BTreeMap<String, (PathBuf, FsAccess)>) -> FsSandbox {
        Self::build(mapping, denied_prefixes())
    }

    /// The testable core of [`FsSandbox::new`], with the denied prefixes injected.
    fn build(mapping: &BTreeMap<String, (PathBuf, FsAccess)>, denied: Vec<PathBuf>) -> FsSandbox {
        let mut slots = BTreeMap::new();
        for (name, (dir, access)) in mapping {
            let root = match dir.canonicalize() {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("filesystem slot {name:?}: cannot resolve: {e}");
                    continue;
                }
            };
            if !root.is_dir() {
                tracing::debug!("filesystem slot {name:?}: not a directory");
                continue;
            }
            if denied.iter().any(|d| root.starts_with(d)) {
                tracing::debug!("filesystem slot {name:?}: inside a reserved directory");
                continue;
            }
            slots.insert(
                name.clone(),
                SlotRoot {
                    root,
                    access: *access,
                },
            );
        }
        FsSandbox { slots, denied }
    }

    /// Whether `path` (canonical) sits inside one of the denied prefixes.
    fn is_denied(&self, path: &Path) -> bool {
        self.denied.iter().any(|d| path.starts_with(d))
    }

    /// The slot, requiring `read`.
    fn slot_read(&self, name: &str) -> Result<&SlotRoot> {
        let s = self
            .slots
            .get(name)
            .ok_or_else(|| anyhow!("unknown slot: {name:?}"))?;
        if s.access != FsAccess::Read {
            bail!("slot is write-only: {name:?}");
        }
        Ok(s)
    }

    /// The slot, requiring `write`.
    fn slot_write(&self, name: &str) -> Result<&SlotRoot> {
        let s = self
            .slots
            .get(name)
            .ok_or_else(|| anyhow!("unknown slot: {name:?}"))?;
        if s.access != FsAccess::Write {
            bail!("slot is read-only: {name:?}");
        }
        Ok(s)
    }

    /// Resolves a guest path that must already exist: joins only `Normal`
    /// components onto the root, canonicalizes the result (resolving symlinks)
    /// and requires it to stay under the root and outside the denied prefixes.
    fn resolve_existing(&self, slot: &str, rel: &str) -> Result<PathBuf> {
        let s = self
            .slots
            .get(slot)
            .ok_or_else(|| anyhow!("unknown slot: {slot:?}"))?;
        let comps = guest_components(rel)?;
        let joined = s.root.join(comps.iter().collect::<PathBuf>());
        let canon = std::fs::canonicalize(&joined)
            .with_context(|| format!("cannot resolve {rel:?} in slot {slot:?}"))?;
        if !canon.starts_with(&s.root) {
            bail!("path escapes the slot");
        }
        if self.is_denied(&canon) {
            bail!("path is inside a reserved directory");
        }
        Ok(canon)
    }

    /// Resolves a path for a **new** file: the parent must already exist, stay
    /// under the root and outside the denied prefixes; the file name is a single
    /// `Normal` component. The caller opens it `create_new`.
    fn resolve_new(&self, slot: &str, rel: &str) -> Result<PathBuf> {
        let s = self.slot_write(slot)?;
        let comps = guest_components(rel)?;
        let name = comps
            .last()
            .ok_or_else(|| anyhow!("a file path is required"))?;
        let parent_rel: PathBuf = comps[..comps.len() - 1].iter().collect();
        let parent = std::fs::canonicalize(s.root.join(parent_rel))
            .with_context(|| format!("cannot resolve the parent of {rel:?} in slot {slot:?}"))?;
        if !parent.starts_with(&s.root) {
            bail!("path escapes the slot");
        }
        if self.is_denied(&parent) {
            bail!("path is inside a reserved directory");
        }
        if !parent.is_dir() {
            bail!("the parent is not a directory");
        }
        let target = parent.join(name);
        // The target itself must also be outside the reserved prefixes: a slot
        // mapped to a parent of a not-yet-existing reserved dir must not be able
        // to create a file at the reserved path.
        if self.is_denied(&target) {
            bail!("path is inside a reserved directory");
        }
        Ok(target)
    }

    /// Lists a slot's directory: one level, or a bounded recursive walk (an
    /// explicit stack, no recursion). Symlinks are skipped (never listed, never
    /// traversed); more than the cap is an error, not a silent truncation.
    pub fn list(
        &self,
        slot: &str,
        rel: &str,
        recursive: bool,
        max_entries: u64,
    ) -> Result<Vec<FsEntry>> {
        let s = self.slot_read(slot)?;
        let base = self.resolve_existing(slot, rel)?;
        if !std::fs::metadata(&base)?.is_dir() {
            bail!("not a directory");
        }
        let cap = max_entries.min(MAX_FS_ENTRIES);
        let walked = self.walk(s, &base, recursive, cap)?;
        Ok(walked
            .into_iter()
            .map(|w| FsEntry {
                path: w.rel,
                kind: w.kind,
                size: w.size,
            })
            .collect())
    }

    /// Reads a regular file in a slot. A file above the cap is refused (never
    /// truncated), and the read itself is bounded so a growing file cannot exceed
    /// it.
    pub fn read(&self, slot: &str, rel: &str) -> Result<Vec<u8>> {
        self.slot_read(slot)?;
        let path = self.resolve_existing(slot, rel)?;
        let meta = std::fs::metadata(&path)?;
        if !meta.is_file() {
            bail!("not a regular file");
        }
        if meta.len() > MAX_FS_READ_BYTES {
            bail!("file too large (max {MAX_FS_READ_BYTES} bytes)");
        }
        match read_capped(&path, MAX_FS_READ_BYTES)? {
            Some(bytes) => Ok(bytes),
            None => bail!("file too large (max {MAX_FS_READ_BYTES} bytes)"),
        }
    }

    /// Creates a **new** file in a slot. The parent must already exist; an
    /// existing file (or a symlink at the leaf) is refused (`create_new`), so a
    /// write never overwrites and never follows a symlink.
    pub fn write(&self, slot: &str, rel: &str, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            bail!("refusing to write an empty file");
        }
        if bytes.len() > MAX_FS_WRITE_BYTES {
            bail!("file too large (max {MAX_FS_WRITE_BYTES} bytes)");
        }
        let target = self.resolve_new(slot, rel)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&target)
            .with_context(|| "cannot create the file")?;
        file.write_all(bytes)
            .with_context(|| "cannot write the file")?;
        Ok(())
    }

    /// Searches a slot for a **case-insensitive literal substring**, as
    /// `(relative-path, line-number, text)` hits. The walk and the scan are
    /// bounded; more hits than the cap is an error.
    pub fn search(
        &self,
        slot: &str,
        rel: &str,
        pattern: &str,
        max_hits: u64,
    ) -> Result<Vec<FsHit>> {
        let s = self.slot_read(slot)?;
        if pattern.is_empty() {
            bail!("empty search pattern");
        }
        if pattern.len() > MAX_FS_PATTERN_BYTES {
            bail!("search pattern too long (max {MAX_FS_PATTERN_BYTES} bytes)");
        }
        let base = self.resolve_existing(slot, rel)?;
        if !std::fs::metadata(&base)?.is_dir() {
            bail!("not a directory");
        }
        let cap = max_hits.min(MAX_FS_HITS);
        let needle = pattern.to_lowercase();
        let mut hits: Vec<FsHit> = Vec::new();
        let mut scanned_bytes: u64 = 0;
        let mut scanned_files: u64 = 0;
        for w in self.walk(s, &base, true, MAX_FS_SCAN_FILES)? {
            if w.kind != FsKind::File || w.size > MAX_FS_SCAN_FILE_BYTES {
                continue;
            }
            scanned_files += 1;
            if scanned_files > MAX_FS_SCAN_FILES {
                bail!("too many files scanned (>{MAX_FS_SCAN_FILES})");
            }
            scanned_bytes = scanned_bytes.saturating_add(w.size);
            if scanned_bytes > MAX_FS_SCAN_BYTES {
                bail!("scan exceeded {MAX_FS_SCAN_BYTES} bytes");
            }
            let Some(bytes) = read_capped(&w.canon, MAX_FS_SCAN_FILE_BYTES)? else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            for (i, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&needle) {
                    if hits.len() as u64 >= cap {
                        bail!("too many hits (more than {cap})");
                    }
                    hits.push(FsHit {
                        path: w.rel.clone(),
                        line: (i as u64) + 1,
                        text: truncate_line(line),
                    });
                }
            }
        }
        Ok(hits)
    }

    /// Walks `base` (already resolved) with an explicit stack. Symlinks and
    /// special files (FIFO/socket/device) are skipped; entries inside a denied
    /// prefix are skipped; more than `cap` **accepted** entries is an error.
    fn walk(&self, s: &SlotRoot, base: &Path, recursive: bool, cap: u64) -> Result<Vec<Walked>> {
        let mut out = Vec::new();
        let mut stack = vec![base.to_path_buf()];
        while let Some(dir) = stack.pop() {
            // Collect up to `cap + 1` **accepted** entries, filtering as we
            // iterate: a directory's raw width must never be materialized (it
            // could be millions of entries), yet the cap check below must see one
            // entry past the limit so an over-cap directory is an error, never a
            // silent truncation. `cap` bounds the accepted count, not the raw one.
            let mut entries: Vec<(std::fs::DirEntry, FsKind, PathBuf)> = Vec::new();
            for e in
                std::fs::read_dir(&dir).with_context(|| "cannot list a directory in the slot")?
            {
                let e = e?;
                let ft = e.file_type()?;
                // Never list nor follow a symlink: it could point out of the root
                // (or loop).
                if ft.is_symlink() {
                    continue;
                }
                // Only regular files and directories are listed; a FIFO/socket/
                // device is skipped (and must never be opened: `File::open` on a
                // FIFO with no writer blocks the host thread forever).
                let kind = if ft.is_dir() {
                    FsKind::Dir
                } else if ft.is_file() {
                    FsKind::File
                } else {
                    continue;
                };
                let canon = match e.path().canonicalize() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if !canon.starts_with(&s.root) || self.is_denied(&canon) {
                    continue;
                }
                entries.push((e, kind, canon));
                if entries.len() as u64 > cap {
                    break;
                }
            }
            // A deterministic order within the accepted window (read_dir's is not).
            entries.sort_by_key(|(e, _, _)| e.file_name());
            for (e, kind, canon) in entries {
                let rel = rel_string(&s.root, &canon)?;
                let size = if kind == FsKind::File {
                    e.metadata().map(|m| m.len()).unwrap_or(0)
                } else {
                    0
                };
                if out.len() as u64 >= cap {
                    bail!("too many entries (more than {cap})");
                }
                out.push(Walked {
                    canon: canon.clone(),
                    rel,
                    kind,
                    size,
                });
                if recursive && kind == FsKind::Dir {
                    stack.push(canon);
                }
            }
        }
        Ok(out)
    }
}

/// The canonical directories a slot root (and any resolved path) must not be
/// inside: slaq's own config and data dirs (the mods dirs' parents) plus the
/// credential file's directory.
fn denied_prefixes() -> Vec<PathBuf> {
    let mut raw: Vec<PathBuf> = Vec::new();
    // The XDG-derived slaq dirs (honoring `XDG_CONFIG_HOME`/`XDG_DATA_HOME`).
    if let Ok(cfg) = crate::mods::default_dir()
        && let Some(p) = cfg.parent()
    {
        raw.push(p.to_path_buf());
    }
    if let Ok(data) = crate::mods::mod_data_base()
        && let Some(p) = data.parent()
    {
        raw.push(p.to_path_buf());
    }
    // The credential file is **hard-coded** to `$HOME/.config/slaq`
    // (`auth::token::config_dir`), ignoring `XDG_CONFIG_HOME`; deny the
    // HOME-based dirs unconditionally too, or a non-default `XDG_CONFIG_HOME`
    // would leave the `xoxc`/`xoxd` tokens reachable through a slot mapped to
    // `~`.
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        let home = PathBuf::from(home);
        raw.push(home.join(".config").join("slaq"));
        raw.push(home.join(".local").join("share").join("slaq"));
    }
    let mut out = Vec::new();
    for p in raw {
        if let Some(c) = canonicalize_lenient(&p)
            && !out.contains(&c)
        {
            out.push(c);
        }
    }
    out
}

/// Whether `dir` is inside one of slaq's reserved directories (a slot mapped
/// there is inert at runtime). Lets the CLI refuse such a mapping up front
/// instead of storing one the runner silently ignores.
pub fn is_reserved_dir(dir: &Path) -> bool {
    let Some(canon) = canonicalize_lenient(dir) else {
        return false;
    };
    denied_prefixes().iter().any(|d| canon.starts_with(d))
}

/// Canonicalizes the longest **existing** prefix of `path` and re-appends the
/// rest, so a directory that does not exist *yet* still yields a canonical
/// prefix to deny against. The sandbox is built once at load, while slaq creates
/// its data dir lazily — a plain `canonicalize` would silently drop a
/// not-yet-existing reserved dir, and a mod could then reach it once slaq made
/// it. Comparing canonical prefixes keeps a symlinked `$HOME` working.
fn canonicalize_lenient(path: &Path) -> Option<PathBuf> {
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(canon) = cur.canonicalize() {
            let mut out = canon;
            for name in rest.iter().rev() {
                out.push(name);
            }
            return Some(out);
        }
        let name = cur.file_name()?.to_os_string();
        rest.push(name);
        if !cur.pop() {
            return None;
        }
    }
}

/// Splits a guest path into its components, refusing anything that could escape:
/// only `Normal` components survive, `CurDir` is skipped, and `ParentDir`,
/// `RootDir` and a `Prefix` are refused.
fn guest_components(rel: &str) -> Result<Vec<String>> {
    if rel.len() > MAX_FS_PATH_BYTES {
        bail!("path too long (max {MAX_FS_PATH_BYTES} bytes)");
    }
    let mut out = Vec::new();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(s) => {
                let s = s
                    .to_str()
                    .ok_or_else(|| anyhow!("path is not valid UTF-8"))?;
                out.push(s.to_string());
            }
            Component::CurDir => {}
            Component::ParentDir => bail!("path escapes the slot"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("absolute paths are not allowed")
            }
        }
    }
    Ok(out)
}

/// A canonical path as a string relative to the slot root, `/`-separated.
fn rel_string(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| anyhow!("path escapes the slot"))?;
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(s) => parts.push(
                s.to_str()
                    .ok_or_else(|| anyhow!("path is not valid UTF-8"))?,
            ),
            Component::CurDir => {}
            _ => bail!("path escapes the slot"),
        }
    }
    Ok(parts.join("/"))
}

/// Reads at most `max` bytes; `None` if the file is (or grew) larger. The error
/// text names no host path: the guest must never learn where its slot lives.
fn read_capped(path: &Path, max: u64) -> Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(path).with_context(|| "cannot open the file")?;
    let mut buf = Vec::new();
    file.take(max.saturating_add(1))
        .read_to_end(&mut buf)
        .with_context(|| "cannot read the file")?;
    if buf.len() as u64 > max {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// Truncates a hit line to a small byte budget at a char boundary.
fn truncate_line(s: &str) -> String {
    if s.len() <= MAX_FS_LINE_BYTES {
        return s.to_string();
    }
    let mut end = MAX_FS_LINE_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("slaq-fs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mapping(slots: &[(&str, &Path, FsAccess)]) -> BTreeMap<String, (PathBuf, FsAccess)> {
        slots
            .iter()
            .map(|(n, d, a)| (n.to_string(), (d.to_path_buf(), *a)))
            .collect()
    }

    #[test]
    fn happy_path_list_read_write_search() {
        let root = tmp("happy");
        let inbox = root.join("inbox");
        let outbox = root.join("outbox");
        std::fs::create_dir_all(inbox.join("sub")).unwrap();
        std::fs::create_dir_all(&outbox).unwrap();
        std::fs::write(inbox.join("a.txt"), "hello world").unwrap();
        std::fs::write(inbox.join("sub/b.txt"), "first\nhello again").unwrap();

        let map = mapping(&[
            ("inbox", &inbox, FsAccess::Read),
            ("outbox", &outbox, FsAccess::Write),
        ]);
        let sb = FsSandbox::build(&map, Vec::new());

        let entries = sb.list("inbox", "", true, 100).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"a.txt"));
        assert!(paths.contains(&"sub"));
        assert!(paths.contains(&"sub/b.txt"));
        assert!(
            entries
                .iter()
                .any(|e| e.path == "sub" && e.kind == FsKind::Dir)
        );
        // One level only.
        let top = sb.list("inbox", "", false, 100).unwrap();
        assert!(top.iter().all(|e| !e.path.contains('/')));

        assert_eq!(sb.read("inbox", "a.txt").unwrap(), b"hello world".to_vec());

        sb.write("outbox", "new.txt", b"data").unwrap();
        assert_eq!(std::fs::read(outbox.join("new.txt")).unwrap(), b"data");

        let hits = sb.search("inbox", "", "hello", 100).unwrap();
        assert!(hits.iter().any(|h| h.path == "a.txt" && h.line == 1));
        assert!(hits.iter().any(|h| h.path == "sub/b.txt" && h.line == 2));
        // Case-insensitive, scoped to a subdirectory.
        let hits = sb.search("inbox", "sub", "HELLO", 100).unwrap();
        assert_eq!(hits.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn escape_paths_are_denied() {
        let root = tmp("escape");
        let slotdir = root.join("slot");
        std::fs::create_dir_all(&slotdir).unwrap();
        std::fs::write(root.join("outside.txt"), "secret").unwrap();
        let map = mapping(&[("s", &slotdir, FsAccess::Read)]);
        let sb = FsSandbox::build(&map, Vec::new());
        assert!(sb.read("s", "../outside.txt").is_err(), "parent traversal");
        assert!(sb.read("s", "a/../../b").is_err(), "nested traversal");
        assert!(sb.read("s", "/etc/hostname").is_err(), "absolute path");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escapes_are_denied() {
        use std::os::unix::fs::symlink;
        let root = tmp("symlink");
        let slotdir = root.join("slot");
        let outside = root.join("outside");
        std::fs::create_dir_all(&slotdir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "top secret").unwrap();
        symlink(outside.join("secret.txt"), slotdir.join("leak.txt")).unwrap();
        symlink(&outside, slotdir.join("dirlink")).unwrap();
        let map = mapping(&[("s", &slotdir, FsAccess::Read)]);
        let sb = FsSandbox::build(&map, Vec::new());
        assert!(
            sb.read("s", "leak.txt").is_err(),
            "a symlink out of the root"
        );
        assert!(
            sb.read("s", "dirlink/secret.txt").is_err(),
            "a symlink to an outside dir"
        );
        // Symlinks are never listed nor traversed.
        let entries = sb.list("s", "", true, 100).unwrap();
        assert!(
            entries
                .iter()
                .all(|e| e.path != "leak.txt" && e.path != "dirlink"),
            "{entries:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn denied_prefixes_are_inert_and_denied() {
        let root = tmp("denied");
        let base = root.join("base");
        let slaqcfg = base.join(".config/slaq");
        std::fs::create_dir_all(&slaqcfg).unwrap();
        std::fs::write(slaqcfg.join("secret.txt"), "consent").unwrap();
        std::fs::write(base.join("ok.txt"), "fine").unwrap();
        let denied = vec![slaqcfg.canonicalize().unwrap()];

        let map = mapping(&[("s", &base, FsAccess::Read)]);
        let sb = FsSandbox::build(&map, denied.clone());
        assert_eq!(sb.read("s", "ok.txt").unwrap(), b"fine".to_vec());
        assert!(
            sb.read("s", ".config/slaq/secret.txt").is_err(),
            "a resolved path inside a denied prefix"
        );
        // The denied subtree is not listed (its parent may be; only the reserved
        // directory itself is hidden).
        let entries = sb.list("s", "", true, 100).unwrap();
        assert!(
            entries
                .iter()
                .all(|e| !e.path.starts_with(".config/slaq") && e.path != ".config/slaq")
        );

        // A root *inside* a denied prefix is inert: the slot is not created.
        let map2 = mapping(&[("s", &slaqcfg, FsAccess::Read)]);
        let sb2 = FsSandbox::build(&map2, denied);
        assert!(
            sb2.list("s", "", false, 10).is_err(),
            "the slot was skipped"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reserved_prefix_that_does_not_exist_yet_is_still_denied() {
        let root = tmp("denied-lazy");
        let base = root.join("base");
        let cfg = base.join(".config");
        std::fs::create_dir_all(&cfg).unwrap();
        // slaq's data dir does not exist when the sandbox is built (it is created
        // lazily); the denied prefix must still cover it.
        let slaq = cfg.join("slaq");
        let denied = vec![canonicalize_lenient(&slaq).unwrap()];
        let map = mapping(&[("s", &base, FsAccess::Read)]);
        let sb = FsSandbox::build(&map, denied);
        // slaq makes the dir *after* the sandbox was built.
        std::fs::create_dir_all(&slaq).unwrap();
        std::fs::write(slaq.join("secret.txt"), "consent").unwrap();
        assert!(
            sb.read("s", ".config/slaq/secret.txt").is_err(),
            "a reserved dir created after the sandbox is built is still denied"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_home_credential_dir_is_denied() {
        // `auth::token` hard-codes `$HOME/.config/slaq`, ignoring
        // `XDG_CONFIG_HOME`; the deny set must cover it either way.
        let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) else {
            return;
        };
        let creds = PathBuf::from(home).join(".config").join("slaq");
        let Some(canon) = canonicalize_lenient(&creds) else {
            return;
        };
        assert!(
            denied_prefixes().iter().any(|d| d == &canon),
            "the HOME credential dir {canon:?} must be denied"
        );
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_skipped() {
        use std::os::unix::ffi::OsStrExt;
        let root = tmp("special");
        let dir = root.join("slot");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
        let fifo = dir.join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path, no aliasing, result checked.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let map = mapping(&[("s", &dir, FsAccess::Read)]);
        let sb = FsSandbox::build(&map, Vec::new());
        let entries = sb.list("s", "", true, 100).unwrap();
        assert!(
            entries.iter().all(|e| e.path != "pipe"),
            "a FIFO is not listed: {entries:?}"
        );
        // `fs-search` must not open the FIFO: `File::open` on it would block the
        // host thread forever.
        let hits = sb.search("s", "", "hi", 10).unwrap();
        assert!(hits.iter().any(|h| h.path == "a.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn an_over_cap_directory_errors_even_when_skipped_entries_fill_the_window() {
        use std::os::unix::fs::symlink;
        let root = tmp("overcap");
        let dir = root.join("slot");
        std::fs::create_dir_all(&dir).unwrap();
        // Symlinks sort ahead of the files and are skipped: the cap must count
        // *accepted* entries, not raw ones (a `take(cap+1)` on raw entries would
        // return `[]` here instead of erroring).
        symlink(root.join("missing-a"), dir.join("a")).unwrap();
        symlink(root.join("missing-b"), dir.join("b")).unwrap();
        std::fs::write(dir.join("c.txt"), "c").unwrap();
        std::fs::write(dir.join("d.txt"), "d").unwrap();
        let map = mapping(&[("s", &dir, FsAccess::Read)]);
        let sb = FsSandbox::build(&map, Vec::new());
        assert!(
            sb.list("s", "", false, 1).is_err(),
            "an over-cap directory is an error, never a silent truncation"
        );
        assert_eq!(sb.list("s", "", false, 10).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_write_cannot_create_a_file_at_a_reserved_path() {
        let root = tmp("reserved-write");
        let base = root.join("base");
        std::fs::create_dir_all(base.join(".config")).unwrap();
        // `.config/slaq` does not exist yet; the deny prefix must still hold.
        let reserved = base.join(".config/slaq");
        let denied = vec![canonicalize_lenient(&reserved).unwrap()];
        let map = mapping(&[("w", &base, FsAccess::Write)]);
        let sb = FsSandbox::build(&map, denied);
        assert!(
            sb.write("w", ".config/slaq", b"x").is_err(),
            "no file may be created at a reserved path"
        );
        assert!(!reserved.exists());
        // A normal write in the same slot still works.
        sb.write("w", "ok.txt", b"x").unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn caps_access_and_missing_slots_are_enforced() {
        let root = tmp("caps");
        let rd = root.join("rd");
        let wr = root.join("wr");
        std::fs::create_dir_all(&rd).unwrap();
        std::fs::create_dir_all(&wr).unwrap();
        std::fs::write(rd.join("one.txt"), "1").unwrap();
        std::fs::write(rd.join("two.txt"), "2").unwrap();
        let map = mapping(&[("rd", &rd, FsAccess::Read), ("wr", &wr, FsAccess::Write)]);
        let sb = FsSandbox::build(&map, Vec::new());

        // Access directions.
        assert!(sb.list("wr", "", false, 10).is_err(), "listing needs read");
        assert!(sb.read("wr", "one.txt").is_err(), "read needs read");
        assert!(sb.write("rd", "x", b"y").is_err(), "write needs write");
        // Missing slots.
        assert!(sb.read("nope", "x").is_err(), "unknown slot");
        assert!(sb.list("nope", "", false, 10).is_err(), "unknown slot");

        // The list cap is an error, not a truncation.
        assert!(
            sb.list("rd", "", false, 1).is_err(),
            "more entries than the cap"
        );
        // Write refuses to overwrite, and refuses an empty body.
        std::fs::write(wr.join("exists"), "old").unwrap();
        assert!(sb.write("wr", "exists", b"new").is_err(), "no overwrite");
        assert_eq!(std::fs::read(wr.join("exists")).unwrap(), b"old");
        assert!(sb.write("wr", "empty", b"").is_err(), "empty body");
        assert!(
            sb.write("wr", "toobig", &vec![0u8; MAX_FS_WRITE_BYTES + 1])
                .is_err(),
            "above the write cap"
        );
        // The read cap refuses (never truncates) a sparse file above it.
        let huge = rd.join("huge.bin");
        let f = std::fs::File::create(&huge).unwrap();
        f.set_len(MAX_FS_READ_BYTES + 1).unwrap();
        drop(f);
        assert!(sb.read("rd", "huge.bin").is_err(), "above the read cap");
        // The search hit cap is an error.
        std::fs::write(rd.join("many.txt"), "hit\nhit\nhit\n").unwrap();
        assert!(
            sb.search("rd", "", "hit", 2).is_err(),
            "more hits than the cap"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
