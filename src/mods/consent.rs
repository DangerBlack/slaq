//! Consent: the user's per-capability grants.
//!
//! Two shapes, deliberately separate:
//!
//! - [`Grants`] — the **runtime** form for one mod: the intersection of what the
//!   manifest requested and what the user granted, plus the read scope the host
//!   enforces.
//! - [`Consent`] — the **persistent** form: what the user granted to each mod id,
//!   stored as JSON so it survives restarts and is manageable from `slaq mods`.
//!
//! Per-capability consent is normative in `docs/mods/capabilities.md`. The
//! interactive install prompt (the in-app "Mods" dialog, GUI + TUI) writes this
//! store; `slaq mods` does the same from the CLI.
//!
//! The **read scope is part of the consent**: enforcement uses the intersection
//! of the recorded consent scope and the manifest's current request
//! (`min(consent, manifest)`), so a package update can neither widen an existing
//! grant nor escape the boundary its own manifest declares. The manifest's scope
//! seeds the grant at install; the consent may be narrower.
//!
//! **What the user configures is not bounded by the manifest.** The user may set
//! the channels a mod reads ([`Entry::user_scope`]) and add `net.egress` hosts
//! ([`Entry::egress_added`]) in the app or with `slaq mods scope|egress`. Those
//! are enforced as chosen: the client is the user's, so they decide what a mod
//! may touch. The `min(consent, manifest)` rule still governs what the
//! **package** asks for, so an update cannot widen a grant on its own. The
//! capability itself must still be declared and granted: a mod that never asked
//! for `net.egress` is not built to use it.

use crate::mods::capability::{Capability, ReadScope, Requested};
use crate::mods::fs::FsAccess;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Cap on the consent file, read before any parsing (the store is small; a
/// symlinked or hand-edited giant file must not be read unbounded).
const MAX_CONSENT_BYTES: usize = 1024 * 1024;

/// One `filesystem` slot as the user approved it: the directory it was mapped to
/// and the access in force when it was mapped. Enforcement intersects the stored
/// access with the manifest's current request (`min`), so a package update that
/// widens a slot from `read` to `write` cannot take effect without re-mapping.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FsMapping {
    /// The **absolute** directory the user mapped the slot to.
    pub dir: String,
    /// The access the user approved.
    pub access: FsAccess,
}

/// What the user granted to one mod.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    #[serde(default)]
    capabilities: BTreeSet<String>,
    /// The scope `messages.read` was granted at. Stored here (the consent), not
    /// taken from the manifest at load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read_scope: Option<ReadScope>,
    /// The `net.egress` allow-list the user consented to. Enforcement uses
    /// `min(consent, manifest)` so a package update cannot silently widen it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    egress: Vec<String>,
    /// The `net.egress` hosts the user has been **shown** (even when they granted
    /// none). A later manifest that asks for new hosts re-prompts; a rejection
    /// does not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    egress_reviewed: Vec<String>,
    /// The channels the **user** chose for `messages.read`, replacing the
    /// manifest's scope (see the module docs). `None` follows the manifest.
    /// Kept across a revoke: it is configuration, inert without the grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_scope: Option<ReadScope>,
    /// `net.egress` hosts the **user** added beyond the manifest's list,
    /// reachable while `net.egress` is granted. Kept across a revoke, like
    /// `user_scope`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    egress_added: Vec<String>,
    /// The user's mapping of each `filesystem` slot to a real directory (slot
    /// name -> mapping). The mod never sees these; the host resolves every access
    /// against them. The **access the user approved** is recorded here so an
    /// update cannot silently widen a mapped slot from `read` to `write`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    fs_slots: BTreeMap<String, FsMapping>,
    /// Whether the user has been shown this mod's request and made a decision
    /// (even "grant nothing"). Keeps the install prompt from reappearing on every
    /// start after a deliberate partial or empty grant.
    #[serde(default)]
    decided: bool,
}

impl Entry {
    /// The granted capabilities, ignoring unknown names (fail closed).
    pub fn caps(&self) -> BTreeSet<Capability> {
        self.capabilities
            .iter()
            .filter_map(|n| Capability::from_name(n))
            .collect()
    }

    /// The recorded read scope, if any.
    pub fn read_scope(&self) -> Option<&ReadScope> {
        self.read_scope.as_ref()
    }

    /// The `net.egress` allow-list the user consented to.
    pub fn egress(&self) -> &[String] {
        &self.egress
    }

    /// The `net.egress` hosts the user has been shown (consented or rejected).
    pub fn egress_reviewed(&self) -> &[String] {
        &self.egress_reviewed
    }

    /// The channels the user chose for `messages.read`, if any (see the field).
    pub fn user_scope(&self) -> Option<&ReadScope> {
        self.user_scope.as_ref()
    }

    /// The `net.egress` hosts the user added (see the field).
    pub fn egress_added(&self) -> &[String] {
        &self.egress_added
    }

    /// The user's slot mapping (slot name -> the mapped directory and access).
    pub fn fs_slots(&self) -> &BTreeMap<String, FsMapping> {
        &self.fs_slots
    }

    /// Whether the user has decided about this mod (see the field).
    pub fn decided(&self) -> bool {
        self.decided
    }

    /// Whether anything must be kept: a decision always is, and so is a slot
    /// mapping or user-chosen access, so an entry is only dropped when it has
    /// none of them.
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
            && self.fs_slots.is_empty()
            && self.user_scope.is_none()
            && self.egress_added.is_empty()
            && !self.decided
    }
}

/// The runtime grants of a single mod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grants {
    caps: BTreeSet<Capability>,
    read_scope: ReadScope,
    /// The **effective** `filesystem` mapping (slot name -> (absolute dir,
    /// access)): the intersection of the manifest-declared slots and the user's
    /// mapping. Empty unless `filesystem` is granted.
    fs_slots: BTreeMap<String, (PathBuf, FsAccess)>,
}

impl Default for Grants {
    fn default() -> Self {
        // Default deny: no capabilities, no scope.
        Grants {
            caps: BTreeSet::new(),
            read_scope: ReadScope::All,
            fs_slots: BTreeMap::new(),
        }
    }
}

impl Grants {
    /// Runtime grants from the **persisted consent** entry. The enforced read
    /// scope is `min(consent, manifest)` (the intersection): a package update can
    /// neither widen an existing grant nor escape the boundary its own manifest
    /// declares. If the intersection is empty — or the entry recorded no scope —
    /// `messages.read` is dropped (fail closed).
    pub fn from_consent(request: &Requested, entry: &Entry) -> Grants {
        let mut caps: BTreeSet<Capability> = request
            .caps()
            .intersection(&entry.caps())
            .copied()
            .collect();
        let read_scope = if caps.contains(&Capability::MessagesRead) {
            match Self::effective_read_scope(request, entry) {
                Some(scope) => scope,
                None => {
                    caps.remove(&Capability::MessagesRead);
                    ReadScope::All
                }
            }
        } else {
            ReadScope::All
        };
        // Likewise: a nonempty `net.egress` request with no effective hosts has
        // nothing reachable, so it is not granted (keeps `list-grants` honest).
        if caps.contains(&Capability::NetEgress)
            && !request.egress().is_empty()
            && Self::effective_egress(request, entry).is_empty()
        {
            caps.remove(&Capability::NetEgress);
        }
        // Likewise: a `filesystem` grant with no **effective** slot (the user
        // mapped nothing the manifest still declares) has nothing reachable, so
        // it is not granted (fail closed, keeping `list-grants` honest).
        let mut fs_slots = BTreeMap::new();
        if caps.contains(&Capability::Filesystem) {
            for slot in request.fs_slots() {
                if let Some(m) = entry.fs_slots().get(&slot.name) {
                    // Intersect the access: an update that widens a mapped slot
                    // from `read` to `write` is downgraded to the access the user
                    // actually approved (re-mapping is how the user widens it).
                    fs_slots.insert(
                        slot.name.clone(),
                        (PathBuf::from(&m.dir), slot.access.narrow(m.access)),
                    );
                }
            }
            if fs_slots.is_empty() {
                caps.remove(&Capability::Filesystem);
            }
        }
        Grants {
            caps,
            read_scope,
            fs_slots,
        }
    }

    /// The read scope actually **enforced** for a `(request, entry)` pair: the
    /// scope the user chose, when they chose one; otherwise the intersection of
    /// what the user consented to and what the manifest still requests. `None`
    /// when `messages.read` is not effectively granted (either side lacks it, no
    /// scope was recorded, or the intersection is empty). This is what
    /// `slaq mods list` displays, so it must match enforcement.
    pub fn effective_read_scope(request: &Requested, entry: &Entry) -> Option<ReadScope> {
        if !request.caps().contains(&Capability::MessagesRead)
            || !entry.caps().contains(&Capability::MessagesRead)
        {
            return None;
        }
        if let Some(chosen) = entry.user_scope() {
            return Some(chosen.clone());
        }
        entry
            .read_scope()
            .and_then(|consented| consented.intersect(request.read_scope()))
    }

    /// The `net.egress` hosts actually allowed: the intersection of the consented
    /// allow-list and the manifest's current request (so a package update can
    /// neither widen consent nor reach hosts the manifest no longer asks for),
    /// plus the hosts the user added themselves.
    pub fn effective_egress(request: &Requested, entry: &Entry) -> Vec<String> {
        if !request.caps().contains(&Capability::NetEgress)
            || !entry.caps().contains(&Capability::NetEgress)
        {
            return Vec::new();
        }
        let mut out: Vec<String> = entry
            .egress()
            .iter()
            .filter(|h| request.egress().iter().any(|r| r == *h))
            .cloned()
            .collect();
        for h in entry.egress_added() {
            if !out.contains(h) {
                out.push(h.clone());
            }
        }
        out
    }

    /// Runtime grants for a one-shot/headless run with explicit capabilities and
    /// the manifest's own declared scope. This is the **dev path** (no persisted
    /// consent): it trusts the manifest, which is acceptable only because the
    /// caller opted in explicitly (`--grant`/`--grant-all`).
    pub fn from_request(request: &Requested, granted: &BTreeSet<Capability>) -> Grants {
        // The dev path has no persisted mapping, so `filesystem` (if granted) has
        // no slots: the sandbox is inert and every call fails closed.
        Self::from_request_with_fs(request, granted, BTreeMap::new())
    }

    /// Like [`Grants::from_request`], but with an explicit `filesystem` slot
    /// mapping (the one-shot runner's `--fs-slot`). The capability set is still
    /// `request ∩ granted`; the mapping is used verbatim, and only takes effect
    /// when `filesystem` is among the granted capabilities (the sandbox is built
    /// from `fs_slots()` only when the grant holds `filesystem`).
    pub fn from_request_with_fs(
        request: &Requested,
        granted: &BTreeSet<Capability>,
        fs_slots: BTreeMap<String, (PathBuf, FsAccess)>,
    ) -> Grants {
        let caps: BTreeSet<Capability> = request.caps().intersection(granted).copied().collect();
        let read_scope = if caps.contains(&Capability::MessagesRead) {
            request.read_scope().clone()
        } else {
            ReadScope::All
        };
        Grants {
            caps,
            read_scope,
            fs_slots,
        }
    }

    /// Whether the capability was granted.
    pub fn has(&self, cap: Capability) -> bool {
        self.caps.contains(&cap)
    }

    /// The effective `filesystem` mapping (slot name -> (absolute dir, access)).
    pub fn fs_slots(&self) -> &BTreeMap<String, (PathBuf, FsAccess)> {
        &self.fs_slots
    }

    /// Whether the mod may read `channel`: it holds `messages.read` **and** the
    /// channel is inside the granted scope.
    pub fn can_read(&self, channel: &str) -> bool {
        self.has(Capability::MessagesRead) && self.read_scope.allows(channel)
    }

    /// The granted capability names, in a stable order (`grants.list`).
    pub fn names(&self) -> Vec<String> {
        self.caps.iter().map(|c| c.name().to_string()).collect()
    }

    /// Builds a grant set from explicit capabilities (tests, one-shot runs).
    #[cfg(test)]
    pub fn of(caps: &[Capability]) -> Grants {
        Grants {
            caps: caps.iter().copied().collect(),
            read_scope: ReadScope::All,
            fs_slots: BTreeMap::new(),
        }
    }

    /// Like [`Grants::of`], with an explicit `filesystem` mapping (tests).
    #[cfg(test)]
    pub fn of_with_fs(
        caps: &[Capability],
        fs_slots: BTreeMap<String, (PathBuf, FsAccess)>,
    ) -> Grants {
        Grants {
            caps: caps.iter().copied().collect(),
            read_scope: ReadScope::All,
            fs_slots,
        }
    }
}

/// The persistent consent record: mod id -> [`Entry`].
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Consent {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    mods: BTreeMap<String, Entry>,
}

/// Where the consent record lives by default: `$SLAQ_MODS_GRANTS`, otherwise a
/// `mods-grants.json` **sibling** of the mods dir (`~/.config/slaq/mods-grants.json`,
/// or next to `$SLAQ_MODS_DIR`). Kept out of the mods dir itself so scanning for
/// packages never has to special-case it.
pub fn default_path() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("SLAQ_MODS_GRANTS").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(v);
        if !p.is_absolute() {
            anyhow::bail!("SLAQ_MODS_GRANTS must be an absolute path");
        }
        return Ok(p);
    }
    Ok(crate::mods::default_dir()?.with_file_name("mods-grants.json"))
}

impl Consent {
    /// Reads the record. A missing file is an empty record (default deny).
    pub fn load(path: &Path) -> Result<Consent> {
        match crate::mods::read_bounded(path, MAX_CONSENT_BYTES) {
            Ok(bytes) => {
                let mut consent: Consent = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                // Fail closed on a future/hand-edited format: an unknown version
                // must not be read as v0 and activate its grants.
                if consent.version != 0 {
                    anyhow::bail!(
                        "unsupported consent version {} in {}",
                        consent.version,
                        path.display()
                    );
                }
                // Backward compatibility: a pre-`decided` entry that recorded a
                // grant (or a scope) was an explicit choice, so treat it as
                // decided rather than re-prompting on every start.
                for e in consent.mods.values_mut() {
                    if !e.decided
                        && (!e.capabilities.is_empty()
                            || e.read_scope.is_some()
                            || !e.egress.is_empty()
                            || !e.egress_reviewed.is_empty())
                    {
                        e.decided = true;
                    }
                }
                Ok(consent)
            }
            Err(e) if is_not_found(&e) => Ok(Consent::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Best-effort load from [`default_path`]. A broken store must not stop the
    /// client from starting; it just means no grants (fail closed).
    pub fn load_default() -> Consent {
        match default_path().and_then(|p| Consent::load(&p)) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("mod consent not loaded: {e}");
                Consent::default()
            }
        }
    }

    /// Writes the record atomically (fsynced temp file + rename) so a crash
    /// mid-write cannot leave a truncated store that silently drops every grant.
    /// The temp name is unpredictable and opened with `create_new`, so a
    /// pre-created symlink cannot redirect the write and two writers cannot
    /// collide. Written `0600` on Unix.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut bytes = serde_json::to_vec_pretty(self).context("serializing consent")?;
        bytes.push(b'\n');
        // Never write a store the next `load` would reject: an unreadable store
        // silently denies every mod.
        if bytes.len() > MAX_CONSENT_BYTES {
            anyhow::bail!(
                "consent store would be {} bytes, above the {MAX_CONSENT_BYTES}-byte load cap",
                bytes.len()
            );
        }
        let nonce = {
            use std::hash::{BuildHasher, Hasher};
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        };
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("consent");
        let tmp = path.with_file_name(format!(".{stem}.{nonce:016x}.tmp"));
        let write = || -> Result<()> {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts
                .open(&tmp)
                .with_context(|| format!("writing {}", tmp.display()))?;
            file.write_all(&bytes)
                .with_context(|| format!("writing {}", tmp.display()))?;
            // Durable before it replaces the store.
            file.sync_all()
                .with_context(|| format!("syncing {}", tmp.display()))?;
            Ok(())
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("replacing {}", path.display()));
        }
        // The rename is atomic but not durable until the directory entry itself
        // is synced; without this a crash could restore the pre-revoke grants.
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }

    /// The recorded entry for `id` (unknown names already filtered by [`Entry::caps`]).
    pub fn entry(&self, id: &str) -> Entry {
        self.mods.get(id).cloned().unwrap_or_default()
    }

    /// The capabilities effectively granted to `id`.
    pub fn granted(&self, id: &str) -> BTreeSet<Capability> {
        self.entry(id).caps()
    }

    /// The effective granted capability names, for display.
    pub fn granted_names(&self, id: &str) -> Vec<String> {
        self.granted(id)
            .iter()
            .map(|c| c.name().to_string())
            .collect()
    }

    /// Grants capabilities to `id`. When `messages.read` is among `caps`, the
    /// read scope is recorded — but only when none is recorded yet: re-granting
    /// must not silently *change* (in particular, must not widen) an existing
    /// scope. To change the scope, `revoke` `messages.read` first (which clears
    /// the recorded scope), then grant again.
    pub fn grant(&mut self, id: &str, caps: &[Capability], read_scope: Option<ReadScope>) {
        self.grant_full(id, caps, read_scope, None);
    }

    /// Like [`Consent::grant`], but also records the `net.egress` allow-list the
    /// user consented to (only when `net.egress` is among `caps` and none is
    /// recorded yet — re-granting never widens it).
    pub fn grant_full(
        &mut self,
        id: &str,
        caps: &[Capability],
        read_scope: Option<ReadScope>,
        egress: Option<Vec<String>>,
    ) {
        let e = self.mods.entry(id.to_string()).or_default();
        e.decided = true;
        for c in caps {
            e.capabilities.insert(c.name().to_string());
        }
        if caps.contains(&Capability::MessagesRead) && e.read_scope.is_none() {
            e.read_scope = read_scope;
        }
        if caps.contains(&Capability::NetEgress) {
            if e.egress.is_empty() {
                if let Some(list) = egress {
                    e.egress = list;
                }
            }
            // Granting implies the list has been seen. Added to, never
            // replaced: a manifest host the user unticked stays reviewed.
            for h in &e.egress {
                if !e.egress_reviewed.contains(h) {
                    e.egress_reviewed.push(h.clone());
                }
            }
        }
    }

    /// Revokes capabilities from `id`; the entry itself persists (the decision
    /// stands) even when nothing is left.
    pub fn revoke(&mut self, id: &str, caps: &[Capability]) {
        if let Some(e) = self.mods.get_mut(id) {
            e.decided = true;
            for c in caps {
                e.capabilities.remove(c.name());
            }
            if caps.contains(&Capability::MessagesRead) {
                e.read_scope = None;
            }
            if caps.contains(&Capability::NetEgress) {
                e.egress.clear();
            }
            if caps.contains(&Capability::Filesystem) {
                // A revoke drops the user's mapping too: re-granting must not
                // silently revive a directory the user unmapped.
                e.fs_slots.clear();
            }
            if e.is_empty() {
                self.mods.remove(id);
            }
        }
    }

    /// Whether the user has already decided about `id` (see [`Entry::decided`]):
    /// false means the install prompt still has to ask.
    pub fn is_decided(&self, id: &str) -> bool {
        self.mods.get(id).map(|e| e.decided).unwrap_or(false)
    }

    /// Records that the user has decided about `id` (even granting nothing), so
    /// the install prompt does not reappear.
    pub fn decide(&mut self, id: &str) {
        self.mods.entry(id.to_string()).or_default().decided = true;
    }

    /// Records that the user has seen this mod's `net.egress` list (even when
    /// they granted none), so a rejection does not re-prompt every start.
    pub fn review_egress(&mut self, id: &str, hosts: Vec<String>) {
        let e = self.mods.entry(id.to_string()).or_default();
        e.decided = true;
        e.egress_reviewed = hosts;
    }

    /// Sets the channels the user chose for `id`'s `messages.read` (`None` goes
    /// back to the manifest's scope). Validated like a manifest scope.
    pub fn set_user_scope(&mut self, id: &str, scope: Option<ReadScope>) -> Result<()> {
        if let Some(sc) = &scope {
            crate::mods::capability::check_user_scope(sc)?;
        }
        let e = self.mods.entry(id.to_string()).or_default();
        e.user_scope = scope;
        if e.is_empty() {
            self.mods.remove(id);
        }
        Ok(())
    }

    /// Sets `id`'s whole `net.egress` configuration as the user edited it: the
    /// manifest hosts they allow (`allowed`, kept only when the manifest lists
    /// them — enforcement intersects anyway) and the hosts they `added`. The
    /// manifest's list is recorded as reviewed, so unticking one of its hosts
    /// does not bring the prompt back. Hosts are validated and deduplicated.
    ///
    /// For a mod the user has **not decided** about yet only `added` is
    /// recorded: the manifest's hosts are still for the install prompt to show
    /// (recording them as reviewed — or as consented, which the legacy-store
    /// upgrade in `load` reads as a decision — would suppress that prompt).
    pub fn set_egress(
        &mut self,
        id: &str,
        manifest: &[String],
        allowed: &[String],
        added: &[String],
    ) -> Result<()> {
        let mut keep: Vec<String> = Vec::new();
        for h in allowed {
            if manifest.contains(h) && !keep.contains(h) {
                keep.push(h.clone());
            }
        }
        let mut extra: Vec<String> = Vec::new();
        for h in added {
            let h = crate::mods::capability::normalize_user_host(h)?;
            if !keep.contains(&h) && !extra.contains(&h) {
                extra.push(h);
            }
        }
        if extra.len() > crate::mods::capability::MAX_EGRESS_HOSTS {
            anyhow::bail!(
                "too many added hosts (>{})",
                crate::mods::capability::MAX_EGRESS_HOSTS
            );
        }
        let e = self.mods.entry(id.to_string()).or_default();
        e.egress_added = extra;
        if e.decided {
            e.egress = keep;
            if !manifest.is_empty() {
                e.egress_reviewed = manifest.to_vec();
            }
        }
        if e.is_empty() {
            self.mods.remove(id);
        }
        Ok(())
    }

    /// Adds one host the user wants `id` to reach (`slaq mods egress add`).
    /// A host the manifest lists is allowed again rather than duplicated.
    pub fn add_host(&mut self, id: &str, manifest: &[String], host: &str) -> Result<()> {
        let h = crate::mods::capability::normalize_user_host(host)?;
        if manifest.contains(&h) && !self.is_decided(id) {
            anyhow::bail!("{h} is one of the mod's own hosts: decide about the mod first");
        }
        let e = self.entry(id);
        let mut allowed = e.egress().to_vec();
        let mut added = e.egress_added().to_vec();
        if manifest.contains(&h) {
            allowed.push(h);
        } else {
            added.push(h);
        }
        self.set_egress(id, manifest, &allowed, &added)
    }

    /// Removes one host from what `id` may reach, whether the manifest listed
    /// it or the user added it (`slaq mods egress rm`). Returns whether it was
    /// there.
    pub fn remove_host(&mut self, id: &str, manifest: &[String], host: &str) -> Result<bool> {
        let h = host.trim().to_ascii_lowercase();
        if manifest.contains(&h) && !self.is_decided(id) {
            anyhow::bail!("{h} is one of the mod's own hosts: decide about the mod first");
        }
        let e = self.entry(id);
        let allowed: Vec<String> = e.egress().iter().filter(|x| **x != h).cloned().collect();
        let added: Vec<String> = e.egress_added().iter().filter(|x| **x != h).cloned().collect();
        let found = allowed.len() != e.egress().len() || added.len() != e.egress_added().len();
        self.set_egress(id, manifest, &allowed, &added)?;
        Ok(found)
    }

    /// Maps a `filesystem` slot of `id` to `dir` (the user's choice) with the
    /// `access` the user approves. Refuses an empty slot name. The path is stored
    /// as given — the caller canonicalizes it. Mapping is **not** a capability
    /// decision: it does not by itself suppress the install prompt (granting
    /// `filesystem` does).
    pub fn map_slot(&mut self, id: &str, slot: &str, dir: PathBuf, access: FsAccess) -> Result<()> {
        if slot.is_empty() {
            anyhow::bail!("slot name must not be empty");
        }
        let dir = dir
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("directory path is not valid UTF-8"))?
            .to_string();
        let e = self.mods.entry(id.to_string()).or_default();
        e.fs_slots
            .insert(slot.to_string(), FsMapping { dir, access });
        Ok(())
    }

    /// Removes a `filesystem` slot mapping from `id`; an entry left with nothing
    /// (no caps, no mapping, no decision) is dropped, like `revoke`.
    pub fn unmap_slot(&mut self, id: &str, slot: &str) {
        if let Some(e) = self.mods.get_mut(id) {
            e.fs_slots.remove(slot);
            if e.is_empty() {
                self.mods.remove(id);
            }
        }
    }

    /// Whether `id` has any recorded entry (used to tell "no consent yet" from
    /// "consented to nothing").
    #[cfg(test)]
    pub fn is_recorded(&self, id: &str) -> bool {
        self.mods.contains_key(id)
    }
}

/// `anyhow` wraps the `std::io::Error`; walk the chain to spot a missing file.
fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// An exclusive, cross-process advisory lock held across a `slaq mods`
/// load/mutate/save, so two concurrent commands cannot lose a grant or revive a
/// revocation (last-rename-wins).
pub struct Lock {
    #[cfg(unix)]
    file: std::fs::File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

/// Locks the consent store for a read-modify-write. Best-effort on non-Unix (no
/// `flock`); the store is written atomically regardless.
pub fn lock(path: &Path) -> Result<Lock> {
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("consent");
    let lock_path = path.with_file_name(format!(".{stem}.lock"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            // State truncate explicitly (clippy `suspicious_open_options`); the
            // lock file is empty and never rewritten.
            .truncate(false)
            .write(true)
            // Owner-only, like the store.
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("locking {}", lock_path.display()))?;
        // Retry on EINTR; any other error means we could not take the lock.
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e).with_context(|| format!("locking {}", lock_path.display()));
            }
        }
        Ok(Lock { file })
    }
    #[cfg(not(unix))]
    {
        let _ = lock_path;
        Ok(Lock {})
    }
}

#[cfg(all(test, feature = "mods"))]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("slaq-consent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn request() -> Requested {
        Requested::parse(&json!({
            "messages.read": { "scope": "channels", "channels": ["C1"] },
            "store.kv": { "maxBytes": 1024 }
        }))
        .unwrap()
    }

    /// Builds a channel set for literals.
    fn ch(ids: &[&str]) -> std::collections::BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn net_request() -> Requested {
        Requested::parse(&json!({
            "messages.read": { "scope": "channels", "channels": ["C1"] },
            "net.egress": ["api.example.com", "cdn.example.com"]
        }))
        .unwrap()
    }

    fn hosts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The user's own channel choice is enforced as-is, beyond the manifest's
    /// scope, and survives a revoke/re-grant and a round-trip through disk.
    #[test]
    fn user_scope_goes_beyond_the_manifest() {
        let req = net_request();
        let mut c = Consent::default();
        c.grant(
            "x",
            &[Capability::MessagesRead],
            Some(req.read_scope().clone()),
        );
        c.set_user_scope(
            "x",
            Some(ReadScope::Channels {
                channels: ch(&["C1", "C9"]),
            }),
        )
        .unwrap();
        let g = Grants::from_consent(&req, &c.entry("x"));
        assert!(g.can_read("C9"), "a channel the manifest never named");
        assert!(!g.can_read("C2"));
        c.set_user_scope("x", Some(ReadScope::All)).unwrap();
        assert!(Grants::from_consent(&req, &c.entry("x")).can_read("C2"));

        // Configuration, not consent: kept across a revoke, inert without it.
        c.revoke("x", &[Capability::MessagesRead]);
        assert!(!Grants::from_consent(&req, &c.entry("x")).can_read("C1"));
        assert_eq!(c.entry("x").user_scope(), Some(&ReadScope::All));

        // Back to the manifest's scope.
        c.grant("x", &[Capability::MessagesRead], Some(req.read_scope().clone()));
        c.set_user_scope("x", None).unwrap();
        let g = Grants::from_consent(&req, &c.entry("x"));
        assert!(g.can_read("C1") && !g.can_read("C2"));

        let path = tmp("user-scope").join("g.json");
        c.set_user_scope(
            "x",
            Some(ReadScope::Channels {
                channels: ch(&["D7"]),
            }),
        )
        .unwrap();
        c.save(&path).unwrap();
        let back = Consent::load(&path).unwrap();
        assert!(Grants::from_consent(&req, &back.entry("x")).can_read("D7"));
    }

    #[test]
    fn user_scope_is_validated() {
        let mut c = Consent::default();
        assert!(
            c.set_user_scope(
                "x",
                Some(ReadScope::Channels {
                    channels: BTreeSet::new()
                })
            )
            .is_err(),
            "an empty pick is refused (choose all, or some)"
        );
        let long = "C".repeat(crate::mods::MAX_REF_BYTES + 1);
        assert!(
            c.set_user_scope(
                "x",
                Some(ReadScope::Channels {
                    channels: ch(&[&long])
                })
            )
            .is_err()
        );
        assert!(!c.is_recorded("x"), "a refused edit records nothing");
    }

    /// Added hosts are reachable beyond the manifest's list; unticked manifest
    /// hosts are not, and stay reviewed (no re-prompt).
    #[test]
    fn user_egress_adds_and_removes_hosts() {
        let req = net_request();
        let manifest = req.egress().to_vec();
        let mut c = Consent::default();
        c.grant_full("x", &[Capability::NetEgress], None, Some(manifest.clone()));
        c.set_egress(
            "x",
            &manifest,
            &hosts(&["api.example.com"]),
            &hosts(&["LocalHost:11434", " 127.0.0.1:8080 ", "localhost:11434"]),
        )
        .unwrap();
        let eff = Grants::effective_egress(&req, &c.entry("x"));
        assert_eq!(
            eff,
            hosts(&["api.example.com", "localhost:11434", "127.0.0.1:8080"]),
            "normalized, deduplicated, manifest host unticked"
        );
        assert_eq!(c.entry("x").egress_reviewed(), &manifest[..]);
        assert!(Grants::from_consent(&req, &c.entry("x")).has(Capability::NetEgress));

        // Unticking every manifest host still leaves the added ones reachable.
        c.set_egress("x", &manifest, &[], &hosts(&["localhost:11434"]))
            .unwrap();
        let g = Grants::from_consent(&req, &c.entry("x"));
        assert!(g.has(Capability::NetEgress));
        assert_eq!(
            Grants::effective_egress(&req, &c.entry("x")),
            hosts(&["localhost:11434"])
        );

        // CLI-style edits.
        c.add_host("x", &manifest, "cdn.example.com").unwrap();
        assert!(c.entry("x").egress().contains(&"cdn.example.com".to_string()));
        assert!(
            !c.entry("x").egress_added().contains(&"cdn.example.com".to_string()),
            "a manifest host is re-allowed, not added"
        );
        assert!(c.remove_host("x", &manifest, "localhost:11434").unwrap());
        assert!(!c.remove_host("x", &manifest, "nope.example").unwrap());
        assert_eq!(
            Grants::effective_egress(&req, &c.entry("x")),
            hosts(&["cdn.example.com"])
        );

        // Nothing is reachable without the grant, added hosts included.
        c.set_egress("x", &manifest, &[], &hosts(&["localhost:1"])).unwrap();
        c.revoke("x", &[Capability::NetEgress]);
        assert!(Grants::effective_egress(&req, &c.entry("x")).is_empty());
        assert_eq!(c.entry("x").egress_added(), &hosts(&["localhost:1"])[..]);
    }

    /// Configuring hosts for a mod not yet decided about must not count as a
    /// decision, not even after the store is saved and reloaded (the legacy
    /// upgrade reads a consented/reviewed list as one).
    #[test]
    fn user_hosts_on_an_undecided_mod_keep_the_prompt() {
        let req = net_request();
        let manifest = req.egress().to_vec();
        let path = tmp("undecided-hosts").join("g.json");
        let mut c = Consent::default();
        c.add_host("x", &manifest, "localhost:11434").unwrap();
        assert!(c.add_host("x", &manifest, "api.example.com").is_err());
        assert!(c.remove_host("x", &manifest, "api.example.com").is_err());
        c.set_egress("x", &manifest, &manifest, &hosts(&["localhost:1"]))
            .unwrap();
        c.save(&path).unwrap();
        let back = Consent::load(&path).unwrap();
        assert!(!back.is_decided("x"), "the install prompt still shows");
        assert!(back.entry("x").egress_reviewed().is_empty());
        assert_eq!(back.entry("x").egress_added(), &hosts(&["localhost:1"])[..]);
    }

    #[test]
    fn user_egress_is_validated() {
        let mut c = Consent::default();
        for bad in ["http://x.com", "*.x.com", "x.com/path", "x.com:0", "", "a b"] {
            assert!(c.add_host("x", &[], bad).is_err(), "{bad:?} accepted");
        }
        let many: Vec<String> = (0..=crate::mods::capability::MAX_EGRESS_HOSTS)
            .map(|i| format!("h{i}.example"))
            .collect();
        assert!(c.set_egress("x", &[], &[], &many).is_err());
        assert!(!c.is_recorded("x"));
    }

    /// A mod that declares `net.egress` with an empty list ("the user picks")
    /// reaches exactly the hosts the user adds.
    #[test]
    fn empty_manifest_egress_reaches_only_user_hosts() {
        let req = Requested::parse(&json!({ "net.egress": [] })).unwrap();
        let mut c = Consent::default();
        c.grant_full("x", &[Capability::NetEgress], None, Some(vec![]));
        assert!(Grants::effective_egress(&req, &c.entry("x")).is_empty());
        c.add_host("x", req.egress(), "localhost:11434").unwrap();
        assert_eq!(
            Grants::effective_egress(&req, &c.entry("x")),
            hosts(&["localhost:11434"])
        );
    }

    #[test]
    fn round_trips_through_disk() {
        let path = tmp("rt").join("mods-grants.json");
        let mut c = Consent::default();
        c.grant(
            "dev.slaq.hello",
            &[Capability::MessagesRead, Capability::StoreKv],
            Some(ReadScope::Channels {
                channels: ch(&["C1"]),
            }),
        );
        c.save(&path).unwrap();
        let back = Consent::load(&path).unwrap();
        let g = back.granted("dev.slaq.hello");
        assert!(g.contains(&Capability::MessagesRead));
        assert!(g.contains(&Capability::StoreKv));
        assert!(!g.contains(&Capability::UiMessageDecoration));
        assert_eq!(
            back.entry("dev.slaq.hello").read_scope(),
            Some(&ReadScope::Channels {
                channels: ch(&["C1"]),
            })
        );
    }

    #[test]
    fn missing_file_is_empty_and_revoke_keeps_the_decision() {
        let path = tmp("missing").join("nope.json");
        assert!(Consent::load(&path).unwrap().granted("x").is_empty());
        assert!(
            !Consent::load(&path).unwrap().is_decided("x"),
            "never asked"
        );

        let mut c = Consent::default();
        c.grant("x", &[Capability::StoreKv], None);
        c.revoke("x", &[Capability::StoreKv]);
        assert!(c.granted("x").is_empty());
        // The entry persists so the install prompt does not reappear.
        assert!(c.is_recorded("x"), "the decision is remembered");
        assert!(c.is_decided("x"));
    }

    #[test]
    fn unknown_names_in_the_file_are_ignored() {
        let mut c = Consent::default();
        // Simulate a hand-edited / future file.
        let mut src = Entry::default();
        src.capabilities = ["store.kv".to_string(), "made.up".to_string()]
            .into_iter()
            .collect();
        c.mods.insert("x".into(), src);
        let g = c.granted("x");
        assert_eq!(g.len(), 1);
        assert!(g.contains(&Capability::StoreKv));
    }

    /// The consent scope is the authority: the manifest's declared scope cannot
    /// widen it.
    #[test]
    fn consent_scope_wins_over_the_manifest_scope() {
        // Manifest requests "all" but consent recorded [C1].
        let manifest_all = Requested::parse(&json!({
            "messages.read": { "scope": "all" },
            "store.kv": { "maxBytes": 1024 }
        }))
        .unwrap();
        let mut consent = Consent::default();
        consent.grant(
            "x",
            &[Capability::MessagesRead],
            Some(ReadScope::Channels {
                channels: ch(&["C1"]),
            }),
        );
        let g = Grants::from_consent(&manifest_all, &consent.entry("x"));
        assert!(g.can_read("C1"));
        assert!(!g.can_read("C2"), "the consent scope must not widen");
    }

    /// The other direction: a manifest that narrows its request tightens the
    /// effective scope, and the display helper agrees with enforcement.
    #[test]
    fn manifest_narrows_the_consent_scope() {
        let manifest = Requested::parse(&json!({
            "messages.read": { "scope": "channels", "channels": ["C1"] },
            "store.kv": { "maxBytes": 1024 }
        }))
        .unwrap();
        let mut consent = Consent::default();
        consent.grant("x", &[Capability::MessagesRead], Some(ReadScope::All));
        let g = Grants::from_consent(&manifest, &consent.entry("x"));
        assert!(g.can_read("C1"));
        assert!(
            !g.can_read("C2"),
            "the manifest narrows the effective scope"
        );
        assert_eq!(
            Grants::effective_read_scope(&manifest, &consent.entry("x")),
            Some(ReadScope::Channels {
                channels: ch(&["C1"])
            })
        );
    }

    #[test]
    fn regrant_does_not_widen_the_scope() {
        let mut c = Consent::default();
        let narrow = ReadScope::Channels {
            channels: ch(&["C1"]),
        };
        c.grant("x", &[Capability::MessagesRead], Some(narrow.clone()));
        // A re-grant with a wider scope is ignored (revoke first to change it).
        c.grant("x", &[Capability::MessagesRead], Some(ReadScope::All));
        assert_eq!(c.entry("x").read_scope(), Some(&narrow));
        // Revoking clears the scope, so a fresh grant can set a new one.
        c.revoke("x", &[Capability::MessagesRead]);
        c.grant("x", &[Capability::MessagesRead], Some(ReadScope::All));
        assert_eq!(c.entry("x").read_scope(), Some(&ReadScope::All));
    }

    #[test]
    fn unknown_consent_version_is_rejected() {
        let path = tmp("ver").join("c.json");
        std::fs::write(&path, br#"{"version":1,"mods":{}}"#).unwrap();
        assert!(
            Consent::load(&path).is_err(),
            "a future format must not be read as v0"
        );
        // A v0 file (version present or absent) loads.
        std::fs::write(&path, br#"{"version":0,"mods":{}}"#).unwrap();
        assert!(Consent::load(&path).is_ok());
        std::fs::write(&path, br#"{"mods":{}}"#).unwrap();
        assert!(Consent::load(&path).is_ok());
    }

    #[test]
    fn legacy_entries_with_grants_are_decided() {
        let path = tmp("legacy").join("c.json");
        // A pre-`decided` entry that recorded a capability was an explicit
        // choice: it must load as decided, not re-prompt.
        std::fs::write(
            &path,
            br#"{"version":0,"mods":{"x":{"capabilities":["store.kv"]}}}"#,
        )
        .unwrap();
        let c = Consent::load(&path).unwrap();
        assert!(c.is_decided("x"), "a legacy grant is a decision");
        assert!(c.granted("x").contains(&Capability::StoreKv));
        // An empty, undecided entry stays undecided.
        std::fs::write(&path, br#"{"version":0,"mods":{"y":{"capabilities":[]}}}"#).unwrap();
        assert!(!Consent::load(&path).unwrap().is_decided("y"));
    }

    #[test]
    fn egress_is_intersected_with_the_manifest() {
        // Consent recorded a.com + b.com; the manifest now asks for a.com + c.com.
        let request = Requested::parse(&json!({ "net.egress": ["a.com", "c.com"] })).unwrap();
        let mut consent = Consent::default();
        consent.grant_full(
            "x",
            &[Capability::NetEgress],
            None,
            Some(vec!["a.com".to_string(), "b.com".to_string()]),
        );
        // Only the consented host that is still requested is reachable: an
        // update cannot add c.com, and b.com is no longer asked for.
        assert_eq!(
            Grants::effective_egress(&request, &consent.entry("x")),
            vec!["a.com".to_string()]
        );
    }

    #[test]
    fn filesystem_slots_are_mapped_and_intersected() {
        let req = Requested::parse(&json!({
            "filesystem": { "slots": [
                { "name": "inbox", "access": "read" },
                { "name": "legacy", "access": "write" }
            ] }
        }))
        .unwrap();
        let mut consent = Consent::default();
        consent.grant("x", &[Capability::Filesystem], None);
        consent
            .map_slot(
                "x",
                "inbox",
                PathBuf::from("/tmp/slaq-inbox"),
                FsAccess::Read,
            )
            .unwrap();
        consent
            .map_slot(
                "x",
                "legacy",
                PathBuf::from("/tmp/slaq-legacy"),
                FsAccess::Write,
            )
            .unwrap();
        // An empty slot name is refused.
        assert!(
            consent
                .map_slot("x", "", PathBuf::from("/tmp/x"), FsAccess::Read)
                .is_err()
        );

        // Only the manifest-declared, user-mapped slots are effective.
        let g = Grants::from_consent(&req, &consent.entry("x"));
        assert!(g.has(Capability::Filesystem));
        assert_eq!(g.fs_slots().len(), 2);
        assert_eq!(
            g.fs_slots().get("inbox"),
            Some(&(PathBuf::from("/tmp/slaq-inbox"), FsAccess::Read))
        );
        assert_eq!(
            g.fs_slots().get("legacy"),
            Some(&(PathBuf::from("/tmp/slaq-legacy"), FsAccess::Write))
        );

        // A manifest that drops a slot drops it from the mapping too.
        let req2 = Requested::parse(&json!({
            "filesystem": { "slots": [{ "name": "inbox", "access": "read" }] }
        }))
        .unwrap();
        let g2 = Grants::from_consent(&req2, &consent.entry("x"));
        assert_eq!(g2.fs_slots().len(), 1);
        assert!(g2.fs_slots().contains_key("inbox"));

        // No user mapping => the capability is dropped (fail closed).
        let mut c2 = Consent::default();
        c2.grant("y", &[Capability::Filesystem], None);
        let g3 = Grants::from_consent(&req, &c2.entry("y"));
        assert!(!g3.has(Capability::Filesystem));
        assert!(g3.fs_slots().is_empty());

        // Revoke clears the recorded mapping.
        consent.revoke("x", &[Capability::Filesystem]);
        assert!(consent.entry("x").fs_slots().is_empty());
    }

    #[test]
    fn an_update_cannot_widen_a_mapped_slots_access() {
        // The user mapped `inbox` while it was declared `read`.
        let mut consent = Consent::default();
        consent.grant("x", &[Capability::Filesystem], None);
        consent
            .map_slot("x", "inbox", PathBuf::from("/tmp/drop"), FsAccess::Read)
            .unwrap();
        // A later manifest declares the same slot `write`.
        let widened = Requested::parse(&json!({
            "filesystem": { "slots": [{ "name": "inbox", "access": "write" }] }
        }))
        .unwrap();
        let g = Grants::from_consent(&widened, &consent.entry("x"));
        assert_eq!(
            g.fs_slots().get("inbox"),
            Some(&(PathBuf::from("/tmp/drop"), FsAccess::Read)),
            "the approved access wins; the update cannot escalate to write"
        );
        // Re-mapping is how the user widens it.
        consent
            .map_slot("x", "inbox", PathBuf::from("/tmp/drop"), FsAccess::Write)
            .unwrap();
        let g = Grants::from_consent(&widened, &consent.entry("x"));
        assert_eq!(
            g.fs_slots().get("inbox"),
            Some(&(PathBuf::from("/tmp/drop"), FsAccess::Write)),
            "a fresh map records the wider access"
        );
    }

    #[test]
    fn mapping_a_slot_does_not_mark_the_mod_decided() {
        // Mapping is not a capability decision, so the install prompt still asks.
        let mut consent = Consent::default();
        consent
            .map_slot("x", "inbox", PathBuf::from("/tmp/x"), FsAccess::Read)
            .unwrap();
        assert!(!consent.is_decided("x"), "mapping alone is not a decision");
        // The entry survives a revoke (the mapping is kept).
        consent.revoke("x", &[Capability::StoreKv]);
        assert!(consent.is_recorded("x"), "the mapping is not dropped");
    }

    #[test]
    fn unmap_drops_an_entry_left_empty() {
        let mut consent = Consent::default();
        consent
            .map_slot("x", "inbox", PathBuf::from("/tmp/x"), FsAccess::Read)
            .unwrap();
        assert!(consent.is_recorded("x"));
        consent.unmap_slot("x", "inbox");
        assert!(!consent.is_recorded("x"), "an empty entry is dropped");
    }

    #[test]
    fn grants_of_with_fs_builds_a_mapping() {
        let mut slots = BTreeMap::new();
        slots.insert(
            "inbox".to_string(),
            (PathBuf::from("/tmp/x"), FsAccess::Read),
        );
        let g = Grants::of_with_fs(&[Capability::Filesystem], slots);
        assert!(g.has(Capability::Filesystem));
        assert_eq!(g.fs_slots().len(), 1);
    }

    #[test]
    fn save_refuses_to_exceed_the_load_cap() {
        let path = tmp("too-big").join("c.json");
        let mut c = Consent::default();
        for i in 0..20_000 {
            c.grant(&format!("dev.slaq.m{i}"), &[Capability::StoreKv], None);
        }
        assert!(
            c.save(&path).is_err(),
            "save must not write a store it cannot load back"
        );
        assert!(!path.exists(), "nothing must be written when over the cap");
    }

    #[test]
    fn messages_read_without_a_recorded_scope_is_denied() {
        let req = request();
        let mut e = Entry::default();
        e.capabilities.insert("messages.read".to_string());
        // No scope recorded: fail closed.
        let g = Grants::from_consent(&req, &e);
        assert!(!g.has(Capability::MessagesRead));
        assert!(!g.can_read("C1"));
    }

    #[test]
    fn grants_intersect_request_and_default_deny() {
        let req = request();
        let none = Grants::from_request(&req, &BTreeSet::new());
        assert!(!none.has(Capability::StoreKv));
        assert!(!none.can_read("C1"));
        let only_kv = Grants::from_request(&req, &[Capability::StoreKv].into_iter().collect());
        assert!(only_kv.has(Capability::StoreKv));
        assert!(!only_kv.can_read("C1"), "no read without messages.read");
        let all = Grants::from_request(&req, &Capability::ALL.into_iter().collect());
        assert!(all.can_read("C1"));
        assert!(!all.can_read("C2"), "out of scope");
    }
}
