//! The capability vocabulary this host implements, and the typed form of a
//! manifest's `capabilities` request.
//!
//! The vocabulary is normative in `docs/mods/capabilities.md`. This host
//! *implements* a subset — `messages.read`, `channels.list`, `users.list`,
//! `store.kv`, `store.vector`, `store.sql`, `ui.message_decoration`, `ui.settings`,
//! `ui.message_action`, `ui.view`, `ui.composer_action`, `ui.context_menu`,
//! `ui.dialog`, `ui.theme`, `notify`, `commands.slash`, `reactions.write`,
//! `chat.send`, `chat.forward`, `files.download`, `files.upload`, `net.egress`,
//! `filesystem` — and every other
//! name is either *known but not implemented* or *reserved*. A manifest that
//! requests a capability this host does not implement is refused at load (fail
//! closed; see `docs/mods/manifest-v0.md`, design rule 5), so the vocabulary can
//! stay complete while the runtime ships in slices.

use crate::mods::fs::{FsAccess, FsSlot, MAX_FS_SLOT_NAME_BYTES, MAX_FS_SLOTS};
use anyhow::{Result, bail};
use std::collections::BTreeSet;

/// Cap on the number of channel ids in a `messages.read` scope. The schema sets
/// no `maxItems`; this bounds how much an untrusted manifest can make the host
/// iterate (`allows` per message, `intersect` at load).
pub const MAX_SCOPE_CHANNELS: usize = 4096;

/// Cap on `net.egress` entries, and on one entry's length (host + port).
pub const MAX_EGRESS_HOSTS: usize = 64;
const MAX_EGRESS_HOST_BYTES: usize = 260;

/// The host's resource envelope for `store.vector` (the schema sets no maximum):
/// `dim` is structural (refused above it); the aggregate caps are clamped.
const MAX_VECTOR_DIM: usize = 4096;
const MAX_VECTORS_CAP: usize = 65_536;
const MAX_VECTOR_BYTES_CAP: usize = 64 * 1024 * 1024;

/// The host's file envelope for `store.kv` (the schema sets no maximum): the
/// declared `maxBytes` is clamped to it.
const MAX_KV_BYTES_CAP: usize = 64 * 1024 * 1024;

/// The host's envelope for `store.sql`: the file quota is clamped like the other
/// stores, and `maxResultRows` is clamped to a sane maximum.
const MAX_SQL_BYTES_CAP: usize = 64 * 1024 * 1024;
const MAX_SQL_RESULT_ROWS_CAP: u64 = 100_000;

/// A capability this host implements, and can therefore enforce at runtime.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Capability {
    MessagesRead,
    ChannelsList,
    UsersList,
    StoreKv,
    StoreVector,
    StoreSql,
    UiMessageDecoration,
    UiSettings,
    UiMessageAction,
    UiView,
    UiComposerAction,
    UiContextMenu,
    UiDialog,
    UiTheme,
    Notify,
    CommandsSlash,
    ReactionsWrite,
    ChatSend,
    ChatForward,
    FilesDownload,
    FilesUpload,
    NetEgress,
    Filesystem,
    Background,
}

impl Capability {
    /// Every capability this host implements (used by `--grant-all` and tests).
    pub const ALL: [Capability; 24] = [
        Capability::MessagesRead,
        Capability::ChannelsList,
        Capability::UsersList,
        Capability::StoreKv,
        Capability::StoreVector,
        Capability::StoreSql,
        Capability::UiMessageDecoration,
        Capability::UiSettings,
        Capability::UiMessageAction,
        Capability::UiView,
        Capability::UiComposerAction,
        Capability::UiContextMenu,
        Capability::UiDialog,
        Capability::UiTheme,
        Capability::Notify,
        Capability::CommandsSlash,
        Capability::ReactionsWrite,
        Capability::ChatSend,
        Capability::ChatForward,
        Capability::FilesDownload,
        Capability::FilesUpload,
        Capability::NetEgress,
        Capability::Filesystem,
        Capability::Background,
    ];

    /// The capability's normative name (`docs/mods/capabilities.md`).
    pub fn name(self) -> &'static str {
        match self {
            Capability::MessagesRead => "messages.read",
            Capability::ChannelsList => "channels.list",
            Capability::UsersList => "users.list",
            Capability::StoreKv => "store.kv",
            Capability::StoreVector => "store.vector",
            Capability::StoreSql => "store.sql",
            Capability::UiMessageDecoration => "ui.message_decoration",
            Capability::UiSettings => "ui.settings",
            Capability::UiMessageAction => "ui.message_action",
            Capability::UiView => "ui.view",
            Capability::UiComposerAction => "ui.composer_action",
            Capability::UiContextMenu => "ui.context_menu",
            Capability::UiDialog => "ui.dialog",
            Capability::UiTheme => "ui.theme",
            Capability::Notify => "notify",
            Capability::CommandsSlash => "commands.slash",
            Capability::ReactionsWrite => "reactions.write",
            Capability::ChatSend => "chat.send",
            Capability::ChatForward => "chat.forward",
            Capability::FilesDownload => "files.download",
            Capability::FilesUpload => "files.upload",
            Capability::NetEgress => "net.egress",
            Capability::Filesystem => "filesystem",
            Capability::Background => "background",
        }
    }

    /// Parses a normative name, if this host implements it.
    pub fn from_name(name: &str) -> Option<Capability> {
        Capability::ALL.into_iter().find(|c| c.name() == name)
    }

    /// A stable group key for the consent prompt. The UI translates it via the
    /// `gui.mods_group_*` / `tui.mods_group_*` keys (the groups are defined in
    /// `docs/mods/capabilities.md`).
    pub fn group(self) -> &'static str {
        match self {
            Capability::MessagesRead
            | Capability::ChannelsList
            | Capability::UsersList
            | Capability::FilesDownload => "read",
            Capability::StoreKv | Capability::StoreVector | Capability::StoreSql => "storage",
            Capability::UiMessageDecoration
            | Capability::UiSettings
            | Capability::UiMessageAction
            | Capability::UiView
            | Capability::UiComposerAction
            | Capability::UiContextMenu
            | Capability::UiDialog
            | Capability::UiTheme
            | Capability::Notify => "interface",
            Capability::CommandsSlash => "commands",
            Capability::ReactionsWrite
            | Capability::ChatSend
            | Capability::ChatForward
            | Capability::FilesUpload => "write",
            Capability::NetEgress => "network",
            Capability::Filesystem => "filesystem",
            Capability::Background => "background",
        }
    }
}

/// How a manifest capability name relates to this host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Implemented(Capability),
    NotImplemented,
    Reserved,
    Unknown,
}

/// Vocabulary names that exist but this slice-1 host has not implemented yet.
/// Kept in the registry (they are valid requests in principle) so the error is
/// precise: the host says *it* does not implement them, not that they are
/// unknown.
const NOT_IMPLEMENTED: &[&str] = &[
    // Read
    "search.read",
];

/// Reserved names: held by the vocabulary, never requestable (see
/// `docs/mods/capabilities.md`). A manifest that asks for one is refused.
const RESERVED: &[&str] = &[
    "chat.edit",
    "chat.delete",
    "presence.read",
    "pins.read",
    "pins.write",
    "saved.read",
    "saved.write",
    "profile.status.write",
    "scheduled.read",
    "scheduled.write",
    "huddle.read",
    "huddle.control",
    "webhook.receive",
    "messages.sql",
];

fn classify(name: &str) -> Status {
    if let Some(c) = Capability::from_name(name) {
        return Status::Implemented(c);
    }
    if RESERVED.contains(&name) {
        return Status::Reserved;
    }
    if NOT_IMPLEMENTED.contains(&name) {
        return Status::NotImplemented;
    }
    Status::Unknown
}

/// What part of the conversation a `messages.read` grant covers.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum ReadScope {
    /// Every channel the client can see.
    All,
    /// Only these channel ids.
    Channels { channels: BTreeSet<String> },
}

impl Default for ReadScope {
    fn default() -> Self {
        ReadScope::All
    }
}

impl ReadScope {
    /// Whether `channel` is inside the scope. Membership is a set lookup, so a
    /// large scope cannot make this linear (it runs per message, per mod).
    pub fn allows(&self, channel: &str) -> bool {
        match self {
            ReadScope::All => true,
            ReadScope::Channels { channels } => channels.contains(channel),
        }
    }

    /// The least-privilege intersection of two scopes. `None` means empty (no
    /// channel is allowed). Used so the enforced scope is `min(consent, manifest)`:
    /// a package update can neither widen consent nor escape the manifest's own
    /// declared boundary.
    pub fn intersect(&self, other: &ReadScope) -> Option<ReadScope> {
        match (self, other) {
            (ReadScope::All, s) => Some(s.clone()),
            (s, ReadScope::All) => Some(s.clone()),
            (ReadScope::Channels { channels: a }, ReadScope::Channels { channels: b }) => {
                let channels: BTreeSet<String> = a.intersection(b).cloned().collect();
                if channels.is_empty() {
                    None
                } else {
                    Some(ReadScope::Channels { channels })
                }
            }
        }
    }
}

/// A manifest's `capabilities` object, parsed and validated. Only names this
/// host implements survive; anything else is an error.
#[derive(Clone, Debug)]
pub struct Requested {
    caps: BTreeSet<Capability>,
    read_scope: ReadScope,
    /// The `net.egress` allow-list (lowercase `host` or `host:port`).
    egress: Vec<String>,
    /// The declared `store.vector` config, if requested.
    vector: Option<crate::mods::store::VectorConfig>,
    /// The declared `store.kv` config, if requested.
    kv: Option<crate::mods::store::KvConfig>,
    /// The declared `store.sql` config, if requested.
    sql: Option<crate::mods::sql::SqlConfig>,
    /// The declared `filesystem` slots, if requested.
    fs_slots: Vec<FsSlot>,
    /// The declared `background` config (tick interval), if requested.
    background: Option<BackgroundConfig>,
}

impl Requested {
    /// Parses and validates a manifest `capabilities` value. Fails closed: an
    /// unknown, reserved or not-yet-implemented name is an error.
    pub fn parse(v: &serde_json::Value) -> Result<Requested> {
        let obj = v
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("`capabilities` must be an object"))?;
        let mut caps = BTreeSet::new();
        let mut read_scope = ReadScope::All;
        let mut egress = Vec::new();
        let mut vector = None;
        let mut kv = None;
        let mut sql = None;
        let mut fs_slots = Vec::new();
        let mut background = None;
        for (name, detail) in obj {
            match classify(name) {
                Status::Implemented(Capability::MessagesRead) => {
                    read_scope = parse_read_scope(detail)?;
                    caps.insert(Capability::MessagesRead);
                }
                Status::Implemented(Capability::StoreKv) => {
                    kv = Some(parse_store_kv(detail)?);
                    caps.insert(Capability::StoreKv);
                }
                Status::Implemented(Capability::StoreVector) => {
                    vector = Some(parse_store_vector(detail)?);
                    caps.insert(Capability::StoreVector);
                }
                Status::Implemented(Capability::StoreSql) => {
                    sql = Some(parse_store_sql(detail)?);
                    caps.insert(Capability::StoreSql);
                }
                Status::Implemented(Capability::NetEgress) => {
                    egress = parse_egress(detail)?;
                    caps.insert(Capability::NetEgress);
                }
                Status::Implemented(Capability::Filesystem) => {
                    fs_slots = parse_filesystem(detail)?;
                    caps.insert(Capability::Filesystem);
                }
                Status::Implemented(Capability::Background) => {
                    background = Some(parse_background(detail)?);
                    caps.insert(Capability::Background);
                }
                Status::Implemented(c) => {
                    // The only implemented capabilities with a specific `detail`
                    // shape are handled above; the rest are empty.
                    parse_empty(name, detail)?;
                    caps.insert(c);
                }
                Status::NotImplemented => {
                    bail!("capability not implemented by this host: {name}")
                }
                Status::Reserved => bail!("reserved capability cannot be requested: {name}"),
                Status::Unknown => bail!("unknown capability: {name}"),
            }
        }
        Ok(Requested {
            caps,
            read_scope,
            egress,
            vector,
            kv,
            sql,
            fs_slots,
            background,
        })
    }

    /// The implemented capabilities the manifest asked for.
    pub fn caps(&self) -> &BTreeSet<Capability> {
        &self.caps
    }

    /// The declared `store.vector` config (only meaningful if requested).
    pub fn vector(&self) -> Option<crate::mods::store::VectorConfig> {
        self.vector
    }

    /// The declared `store.kv` config (only meaningful if requested).
    pub fn kv(&self) -> Option<crate::mods::store::KvConfig> {
        self.kv
    }

    /// The declared `store.sql` config (only meaningful if requested).
    pub fn sql(&self) -> Option<crate::mods::sql::SqlConfig> {
        self.sql
    }

    /// The declared `background` config, if requested.
    pub fn background(&self) -> Option<BackgroundConfig> {
        self.background
    }

    /// The declared `filesystem` slots (only meaningful if requested).
    pub fn fs_slots(&self) -> &[FsSlot] {
        &self.fs_slots
    }

    /// The declared read scope (only meaningful if `messages.read` is present).
    pub fn read_scope(&self) -> &ReadScope {
        &self.read_scope
    }

    /// The `net.egress` allow-list (only meaningful if `net.egress` is present).
    pub fn egress(&self) -> &[String] {
        &self.egress
    }

    /// The requested capability names, for display.
    pub fn names(&self) -> Vec<String> {
        self.caps.iter().map(|c| c.name().to_string()).collect()
    }
}

/// `{ "scope": "all" }` or `{ "scope": "channels", "channels": ["C…", "D…"] }`,
/// matching `manifest.schema.json` exactly (`additionalProperties: false`,
/// `uniqueItems: true`).
fn parse_read_scope(detail: &serde_json::Value) -> Result<ReadScope> {
    let o = detail
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("messages.read must be an object"))?;
    match o.get("scope").and_then(|s| s.as_str()) {
        Some("all") => {
            if o.len() != 1 {
                bail!("messages.read: `scope: all` takes no other fields");
            }
            Ok(ReadScope::All)
        }
        Some("channels") => {
            if o.len() != 2 {
                bail!("messages.read: `scope: channels` takes only `channels`");
            }
            let list = o
                .get("channels")
                .and_then(|c| c.as_array())
                .ok_or_else(|| {
                    anyhow::anyhow!("messages.read: `channels` is required with scope=channels")
                })?;
            if list.is_empty() {
                bail!("messages.read: `channels` must not be empty");
            }
            if list.len() > MAX_SCOPE_CHANNELS {
                bail!("messages.read: too many channels (>{MAX_SCOPE_CHANNELS})");
            }
            let mut channels = BTreeSet::new();
            for c in list {
                let s = c
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("messages.read: channels must be strings"))?;
                if s.len() > crate::mods::MAX_REF_BYTES {
                    bail!("messages.read: channel id too long");
                }
                // A duplicate is rejected AND deduped for free by the set.
                if !channels.insert(s.to_string()) {
                    bail!("messages.read: duplicate channel {s:?}");
                }
            }
            Ok(ReadScope::Channels { channels })
        }
        Some(other) => bail!("messages.read: unsupported scope {other:?}"),
        None => bail!("messages.read: `scope` is required"),
    }
}

/// `{ "maxBytes": <int> }` (the schema-required shape; no other key). The value
/// is clamped to the host's file envelope (`MAX_KV_BYTES_CAP`).
fn parse_store_kv(detail: &serde_json::Value) -> Result<crate::mods::store::KvConfig> {
    let o = detail
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("store.kv must be an object"))?;
    if o.len() != 1 || !o.contains_key("maxBytes") {
        bail!("store.kv: only `maxBytes` is allowed");
    }
    let max = o.get("maxBytes").and_then(|n| n.as_u64()).ok_or_else(|| {
        anyhow::anyhow!("store.kv: `maxBytes` (a non-negative integer) is required")
    })?;
    Ok(crate::mods::store::KvConfig {
        max_bytes: (max as usize).min(MAX_KV_BYTES_CAP),
    })
}

/// `{ "dim": <int≥1>, "maxVectors": <int≥1>, "maxBytes": <int≥1> }` (the
/// schema-required shape). The aggregate caps are clamped to the host envelope;
/// `dim` is structural and is refused when too large.
fn parse_store_vector(detail: &serde_json::Value) -> Result<crate::mods::store::VectorConfig> {
    let o = detail
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("store.vector must be an object"))?;
    if o.len() != 3
        || !o.contains_key("dim")
        || !o.contains_key("maxVectors")
        || !o.contains_key("maxBytes")
    {
        bail!("store.vector: only `dim`, `maxVectors` and `maxBytes` are allowed");
    }
    let Some(dim) = o.get("dim").and_then(|v| v.as_u64()).filter(|d| *d >= 1) else {
        bail!("store.vector: `dim` (an integer ≥ 1) is required");
    };
    if dim as usize > MAX_VECTOR_DIM {
        bail!("store.vector: dim {dim} exceeds the host maximum ({MAX_VECTOR_DIM})");
    }
    let Some(max_vectors) = o
        .get("maxVectors")
        .and_then(|v| v.as_u64())
        .filter(|d| *d >= 1)
    else {
        bail!("store.vector: `maxVectors` (an integer ≥ 1) is required");
    };
    let Some(max_bytes) = o
        .get("maxBytes")
        .and_then(|v| v.as_u64())
        .filter(|d| *d >= 1)
    else {
        bail!("store.vector: `maxBytes` (an integer ≥ 1) is required");
    };
    Ok(crate::mods::store::VectorConfig {
        dim: dim as usize,
        max_vectors: (max_vectors as usize).min(MAX_VECTORS_CAP),
        max_bytes: (max_bytes as usize).min(MAX_VECTOR_BYTES_CAP),
    })
}

/// `{ "maxBytes": <int≥1>, "maxResultRows": <int≥1> }` (the schema-required
/// shape). The file quota is clamped like the other stores; `maxResultRows` is
/// clamped to the host envelope.
fn parse_store_sql(detail: &serde_json::Value) -> Result<crate::mods::sql::SqlConfig> {
    let o = detail
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("store.sql must be an object"))?;
    if o.len() != 2 || !o.contains_key("maxBytes") || !o.contains_key("maxResultRows") {
        bail!("store.sql: only `maxBytes` and `maxResultRows` are allowed");
    }
    let Some(max_bytes) = o
        .get("maxBytes")
        .and_then(|v| v.as_u64())
        .filter(|d| *d >= 1)
    else {
        bail!("store.sql: `maxBytes` (an integer ≥ 1) is required");
    };
    let Some(max_rows) = o
        .get("maxResultRows")
        .and_then(|v| v.as_u64())
        .filter(|d| *d >= 1)
    else {
        bail!("store.sql: `maxResultRows` (an integer ≥ 1) is required");
    };
    Ok(crate::mods::sql::SqlConfig {
        max_bytes: (max_bytes as usize).min(MAX_SQL_BYTES_CAP),
        max_result_rows: max_rows.min(MAX_SQL_RESULT_ROWS_CAP),
    })
}

/// `net.egress` is an allow-list: `["host", "host:port", …]` (lowercase, no
/// scheme/path/wildcard), matching `manifest.schema.json`.
fn parse_egress(detail: &serde_json::Value) -> Result<Vec<String>> {
    let list = detail
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("net.egress must be an array of hosts"))?;
    if list.len() > MAX_EGRESS_HOSTS {
        bail!("net.egress: too many hosts (>{MAX_EGRESS_HOSTS})");
    }
    let mut out: Vec<String> = Vec::with_capacity(list.len());
    for item in list {
        let s = item
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("net.egress: hosts must be strings"))?;
        if !valid_egress_host(s) {
            bail!("net.egress: invalid host {s:?} (lowercase `host` or `host:port`)");
        }
        if out.iter().any(|h| h == s) {
            bail!("net.egress: duplicate host {s:?}");
        }
        out.push(s.to_string());
    }
    Ok(out)
}

/// `filesystem` is `{ "slots": [ { "name": <label>, "access": "read"|"write" } ] }`,
/// matching `manifest.schema.json` (`additionalProperties: false`): a non-empty
/// array of at most [`MAX_FS_SLOTS`] uniquely-named slots. Slot names are labels
/// the user maps to real directories, so they are constrained (lowercase,
/// `[a-z0-9_-]`, start/end alphanumeric, ≤ 32 bytes) and never paths.
fn parse_filesystem(detail: &serde_json::Value) -> Result<Vec<FsSlot>> {
    let o = detail
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("filesystem must be an object"))?;
    if o.len() != 1 || !o.contains_key("slots") {
        bail!("filesystem: only `slots` is allowed");
    }
    let list = o
        .get("slots")
        .and_then(|s| s.as_array())
        .ok_or_else(|| anyhow::anyhow!("filesystem: `slots` must be an array"))?;
    if list.is_empty() {
        bail!("filesystem: `slots` must not be empty");
    }
    if list.len() > MAX_FS_SLOTS {
        bail!("filesystem: too many slots (>{MAX_FS_SLOTS})");
    }
    let mut out: Vec<FsSlot> = Vec::with_capacity(list.len());
    for item in list {
        let slot = item
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("filesystem: each slot must be an object"))?;
        if slot.len() != 2 || !slot.contains_key("name") || !slot.contains_key("access") {
            bail!("filesystem: each slot takes only `name` and `access`");
        }
        let name = slot
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow::anyhow!("filesystem: slot `name` must be a string"))?;
        if !valid_slot_name(name) {
            bail!("filesystem: invalid slot name {name:?}");
        }
        if out.iter().any(|s| s.name == name) {
            bail!("filesystem: duplicate slot {name:?}");
        }
        let access = match slot.get("access").and_then(|a| a.as_str()) {
            Some("read") => FsAccess::Read,
            Some("write") => FsAccess::Write,
            _ => bail!("filesystem: slot `access` must be `read` or `write`"),
        };
        out.push(FsSlot {
            name: name.to_string(),
            access,
        });
    }
    Ok(out)
}

/// A slot label: 1..=32 bytes matching `^[a-z0-9]([a-z0-9_-]*[a-z0-9])?$`.
fn valid_slot_name(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > MAX_FS_SLOT_NAME_BYTES {
        return false;
    }
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return false;
    }
    b.iter().all(|&c| alnum(c) || c == b'-' || c == b'_')
}

/// Normalizes a `net.egress` host the **user** typed (in the app or with
/// `slaq mods egress`): trimmed and lowercased, then held to the same rule as a
/// manifest entry ([`valid_egress_host`]).
pub fn normalize_user_host(s: &str) -> Result<String> {
    let h = s.trim().to_ascii_lowercase();
    if !valid_egress_host(&h) {
        bail!("invalid host {h:?}: expected `host` or `host:port` (no scheme, path or wildcard)");
    }
    Ok(h)
}

/// Checks a read scope the **user** chose: the same bounds a manifest scope has
/// (a non-empty, bounded set of bounded channel ids).
pub fn check_user_scope(scope: &ReadScope) -> Result<()> {
    if let ReadScope::Channels { channels } = scope {
        if channels.is_empty() {
            bail!("pick at least one channel, or allow all channels");
        }
        if channels.len() > MAX_SCOPE_CHANNELS {
            bail!("too many channels (>{MAX_SCOPE_CHANNELS})");
        }
        if channels
            .iter()
            .any(|c| c.is_empty() || c.len() > crate::mods::MAX_REF_BYTES)
        {
            bail!("invalid channel id");
        }
    }
    Ok(())
}

/// A lowercase `host` or `host:port`: labels of `[a-z0-9-]` starting and ending
/// alphanumeric, and an optional 1–65535 port. No scheme, path, wildcard,
/// userinfo or IPv6 brackets.
pub fn valid_egress_host(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_EGRESS_HOST_BYTES {
        return false;
    }
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (s, None),
    };
    if let Some(p) = port {
        if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        match p.parse::<u32>() {
            Ok(n) if (1..=65535).contains(&n) => {}
            _ => return false,
        }
    }
    if host.is_empty() {
        return false;
    }
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    for label in host.split('.') {
        let b = label.as_bytes();
        if b.is_empty() || !alnum(b[0]) || !alnum(b[b.len() - 1]) {
            return false;
        }
        if !b.iter().all(|&c| alnum(c) || c == b'-') {
            return false;
        }
    }
    true
}

/// `{}` — the schema's `empty` shape.
/// How often a mod with `background` is woken with a `background.tick` event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackgroundConfig {
    pub interval: std::time::Duration,
}

/// Default tick interval when the manifest gives none.
const BACKGROUND_INTERVAL_DEFAULT_SECS: f64 = 60.0;
/// Floor on the tick interval: not a budget, just "not a busy spin" (a tick is a
/// full guest call; a mod wanting continuous work does more per tick).
const BACKGROUND_INTERVAL_MIN_SECS: f64 = 0.1;
/// Ceiling on the tick interval (30 days): anything longer is "practically never",
/// and an unbounded value would overflow `Instant + interval` (a panic, which a
/// `panic = "abort"` release turns into a client crash on every start).
const BACKGROUND_INTERVAL_MAX_SECS: f64 = 30.0 * 24.0 * 3600.0;

/// `{}` or `{ "intervalSecs": <0.1 ≤ number ≤ 30 days> }` (fractions allowed).
fn parse_background(detail: &serde_json::Value) -> Result<BackgroundConfig> {
    let o = detail
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("background must be an object"))?;
    if o.keys().any(|k| k != "intervalSecs") {
        bail!("background: only `intervalSecs` is allowed");
    }
    let secs = match o.get("intervalSecs") {
        None => BACKGROUND_INTERVAL_DEFAULT_SECS,
        Some(v) => v.as_f64().ok_or_else(|| {
            anyhow::anyhow!("background: `intervalSecs` must be a number")
        })?,
    };
    if !secs.is_finite()
        || !(BACKGROUND_INTERVAL_MIN_SECS..=BACKGROUND_INTERVAL_MAX_SECS).contains(&secs)
    {
        bail!(
            "background: `intervalSecs` must be between {BACKGROUND_INTERVAL_MIN_SECS} and \
             {BACKGROUND_INTERVAL_MAX_SECS}"
        );
    }
    let interval = std::time::Duration::try_from_secs_f64(secs)
        .map_err(|_| anyhow::anyhow!("background: `intervalSecs` is out of range"))?;
    Ok(BackgroundConfig { interval })
}

fn parse_empty(name: &str, detail: &serde_json::Value) -> Result<()> {
    if detail.as_object().map(|o| o.is_empty()).unwrap_or(false) {
        Ok(())
    } else {
        bail!("{name} must be an empty object")
    }
}

#[cfg(all(test, feature = "mods"))]
mod tests {
    use super::*;
    use serde_json::json;

    /// `background` defaults to a 60 s tick, accepts fractions, and refuses a
    /// busy-spin interval, unknown keys and non-numbers.
    #[test]
    fn background_config_parses_and_validates() {
        let r = Requested::parse(&json!({"background": {}})).unwrap();
        assert_eq!(r.background().unwrap().interval, std::time::Duration::from_secs(60));
        let r = Requested::parse(&json!({"background": {"intervalSecs": 0.5}})).unwrap();
        assert_eq!(r.background().unwrap().interval, std::time::Duration::from_millis(500));
        assert!(r.caps().contains(&Capability::Background));
        assert!(Requested::parse(&json!({"background": {"intervalSecs": 0.01}})).is_err());
        // Regression (review): a huge interval used to pass and overflow
        // `Instant + d` on the worker — a client-wide abort in release builds.
        assert!(Requested::parse(&json!({"background": {"intervalSecs": 1e19}})).is_err());
        assert!(Requested::parse(&json!({"background": {"intervalSecs": 2592001}})).is_err());
        assert!(Requested::parse(&json!({"background": {"intervalSecs": 2592000}})).is_ok());
        assert!(Requested::parse(&json!({"background": {"intervalSecs": "5"}})).is_err());
        assert!(Requested::parse(&json!({"background": {"every": 5}})).is_err());
        assert!(Requested::parse(&json!({"background": 5})).is_err());
    }

    /// Builds a channel set for literals.
    fn ch(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn accepts_the_implemented_capabilities() {
        let req = Requested::parse(&json!({
            "messages.read": { "scope": "channels", "channels": ["C1", "D2"] },
            "store.kv": { "maxBytes": 1024 },
            "ui.message_decoration": {}
        }))
        .unwrap();
        assert_eq!(req.caps().len(), 3);
        assert_eq!(
            *req.read_scope(),
            ReadScope::Channels {
                channels: ch(&["C1", "D2"])
            }
        );
    }

    #[test]
    fn store_kv_max_bytes_is_kept_and_clamped() {
        let r = Requested::parse(&json!({ "store.kv": { "maxBytes": 1024 } })).unwrap();
        assert_eq!(r.kv().unwrap().max_bytes, 1024);
        // The schema sets no maximum: a huge value is clamped to the host cap.
        let r = Requested::parse(&json!({ "store.kv": { "maxBytes": u64::MAX } })).unwrap();
        assert_eq!(r.kv().unwrap().max_bytes, MAX_KV_BYTES_CAP);
        // The shape is still strict.
        assert!(Requested::parse(&json!({ "store.kv": {} })).is_err());
        assert!(Requested::parse(&json!({ "store.kv": { "maxBytes": 1, "x": 1 } })).is_err());
    }

    #[test]
    fn store_sql_is_kept_and_clamped() {
        let r = Requested::parse(&json!({
            "store.sql": { "maxBytes": 4096, "maxResultRows": 25 }
        }))
        .unwrap();
        let s = r.sql().unwrap();
        assert_eq!((s.max_bytes, s.max_result_rows), (4096, 25));
        // The schema sets no maximum: both are clamped to the host envelope.
        let r = Requested::parse(&json!({
            "store.sql": { "maxBytes": u64::MAX, "maxResultRows": u64::MAX }
        }))
        .unwrap();
        let s = r.sql().unwrap();
        assert_eq!(s.max_bytes, MAX_SQL_BYTES_CAP);
        assert_eq!(s.max_result_rows, MAX_SQL_RESULT_ROWS_CAP);
        // The shape is strict: both keys are required, no extras, both ≥ 1.
        assert!(
            Requested::parse(&json!({ "store.sql": {} })).is_err(),
            "missing keys"
        );
        assert!(
            Requested::parse(&json!({ "store.sql": { "maxBytes": 1 } })).is_err(),
            "missing maxResultRows"
        );
        assert!(
            Requested::parse(
                &json!({ "store.sql": { "maxBytes": 1, "maxResultRows": 1, "x": 1 } })
            )
            .is_err(),
            "extra key"
        );
        assert!(
            Requested::parse(&json!({ "store.sql": { "maxBytes": 0, "maxResultRows": 1 } }))
                .is_err(),
            "maxBytes < 1"
        );
    }

    #[test]
    fn parses_and_validates_store_vector() {
        let r = Requested::parse(&json!({
            "store.vector": { "dim": 4, "maxVectors": 100, "maxBytes": 4096 }
        }))
        .unwrap();
        let v = r.vector().unwrap();
        assert_eq!((v.dim, v.max_vectors, v.max_bytes), (4, 100, 4096));
        assert!(
            Requested::parse(&json!({"store.vector": {"dim":0,"maxVectors":1,"maxBytes":1}}))
                .is_err(),
            "dim < 1"
        );
        assert!(
            Requested::parse(&json!({"store.vector": {"dim":1,"maxVectors":1}})).is_err(),
            "missing maxBytes"
        );
        assert!(
            Requested::parse(&json!({"store.vector": {"dim":1,"maxVectors":1,"maxBytes":1,"x":1}}))
                .is_err(),
            "extra key"
        );
        assert!(
            Requested::parse(&json!({"store.vector": {"dim":999999,"maxVectors":1,"maxBytes":1}}))
                .is_err(),
            "dim too large"
        );
        // Aggregate caps are clamped to the host envelope (accepted, reduced).
        let r = Requested::parse(&json!({
            "store.vector": { "dim": 1, "maxVectors": 99999999, "maxBytes": 99999999 }
        }))
        .unwrap();
        let v = r.vector().unwrap();
        assert!(v.max_vectors <= MAX_VECTORS_CAP && v.max_bytes <= MAX_VECTOR_BYTES_CAP);
    }

    #[test]
    fn parses_and_validates_filesystem() {
        let req = Requested::parse(&json!({
            "filesystem": { "slots": [
                { "name": "inbox", "access": "read" },
                { "name": "out-box_2", "access": "write" }
            ] }
        }))
        .unwrap();
        assert!(req.caps().contains(&Capability::Filesystem));
        assert_eq!(req.caps().len(), 1);
        let slots = req.fs_slots();
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].name, "inbox");
        assert_eq!(slots[0].access, FsAccess::Read);
        assert_eq!(slots[1].name, "out-box_2");
        assert_eq!(slots[1].access, FsAccess::Write);

        let parse = |v: serde_json::Value| Requested::parse(&json!({ "filesystem": v }));
        assert!(parse(json!({})).is_err(), "slots required");
        assert!(parse(json!({ "slots": [] })).is_err(), "non-empty");
        assert!(
            parse(json!({ "slots": [{ "name": "a", "access": "read" }], "x": 1 })).is_err(),
            "extra key"
        );
        assert!(
            parse(json!({ "slots": [{ "name": "a", "access": "read", "x": 1 }] })).is_err(),
            "extra slot key"
        );
        assert!(
            parse(json!({ "slots": [{ "name": "A", "access": "read" }] })).is_err(),
            "uppercase name"
        );
        assert!(
            parse(json!({ "slots": [{ "name": "-a", "access": "read" }] })).is_err(),
            "leading dash"
        );
        assert!(
            parse(json!({ "slots": [{ "name": "a", "access": "own" }] })).is_err(),
            "bad access"
        );
        assert!(
            parse(json!({ "slots": [{ "name": "a" }] })).is_err(),
            "access required"
        );
        assert!(
            parse(json!({ "slots": [{ "access": "read" }] })).is_err(),
            "name required"
        );
        assert!(
            parse(json!({ "slots": [
                { "name": "a", "access": "read" },
                { "name": "a", "access": "write" }
            ] }))
            .is_err(),
            "duplicate name"
        );
        let many: Vec<serde_json::Value> = (0..=MAX_FS_SLOTS)
            .map(|i| json!({ "name": format!("s{i}"), "access": "read" }))
            .collect();
        assert!(parse(json!({ "slots": many })).is_err(), "too many slots");
    }

    #[test]
    fn rejects_unknown_reserved_and_unimplemented() {
        assert!(
            Requested::parse(&json!({ "no.such": {} })).is_err(),
            "unknown"
        );
        assert!(
            Requested::parse(&json!({ "chat.delete": {} })).is_err(),
            "reserved"
        );
        // `store.sql` is implemented now; `search.read` is the one name still not
        // implemented (a request is refused for that reason).
        assert!(
            Requested::parse(&json!({ "search.read": {} })).is_err(),
            "unimplemented"
        );
    }

    #[test]
    fn parses_and_validates_net_egress() {
        let req = Requested::parse(&json!({
            "net.egress": ["example.com", "localhost:11434", "127.0.0.1:8080"]
        }))
        .unwrap();
        assert!(req.caps().contains(&Capability::NetEgress));
        assert_eq!(
            req.egress(),
            &[
                "example.com".to_string(),
                "localhost:11434".to_string(),
                "127.0.0.1:8080".to_string()
            ]
        );
        let parse = |v: serde_json::Value| Requested::parse(&json!({ "net.egress": v }));
        assert!(parse(json!(["Example.com"])).is_err(), "uppercase");
        assert!(parse(json!(["http://x.com"])).is_err(), "scheme");
        assert!(parse(json!(["x.com/path"])).is_err(), "path");
        assert!(parse(json!(["x:0"])).is_err(), "port 0");
        assert!(parse(json!(["x:99999"])).is_err(), "port too big");
        assert!(parse(json!(["x:abc"])).is_err(), "non-numeric port");
        assert!(parse(json!(["a.com", "a.com"])).is_err(), "duplicate");
        assert!(parse(json!("nope")).is_err(), "not an array");
    }

    #[test]
    fn read_scope_is_required_and_well_formed() {
        assert!(
            Requested::parse(&json!({ "messages.read": {} })).is_err(),
            "scope missing"
        );
        assert!(
            Requested::parse(&json!({ "messages.read": { "scope": "none" } })).is_err(),
            "unknown scope"
        );
        assert!(
            Requested::parse(&json!({ "messages.read": { "scope": "channels", "channels": [] } }))
                .is_err(),
            "empty channel scope"
        );
    }

    #[test]
    fn empty_scope_and_scope_checks() {
        assert!(ReadScope::All.allows("anything"));
        let scoped = ReadScope::Channels {
            channels: ch(&["C1"]),
        };
        assert!(scoped.allows("C1"));
        assert!(!scoped.allows("C2"));
    }

    #[test]
    fn rejects_over_specified_or_duplicate_details() {
        // `scope: all` with extra fields (schema: additionalProperties false).
        assert!(
            Requested::parse(&json!({ "messages.read": { "scope": "all", "channels": ["C1"] } }))
                .is_err()
        );
        // Duplicate channels (schema: uniqueItems true).
        assert!(
            Requested::parse(
                &json!({ "messages.read": { "scope": "channels", "channels": ["C1", "C1"] } })
            )
            .is_err()
        );
        // store.kv with an extra key, and store.kv missing maxBytes.
        assert!(Requested::parse(&json!({ "store.kv": { "maxBytes": 1, "extra": 2 } })).is_err());
        assert!(Requested::parse(&json!({ "store.kv": {} })).is_err());
    }

    #[test]
    fn scope_intersection_is_least_privilege() {
        let all = ReadScope::All;
        let c1 = ReadScope::Channels {
            channels: ch(&["C1"]),
        };
        let c12 = ReadScope::Channels {
            channels: ch(&["C1", "C2"]),
        };
        assert_eq!(all.intersect(&c1), Some(c1.clone()));
        assert_eq!(c12.intersect(&c1), Some(c1.clone()));
        assert_eq!(c1.intersect(&c12), Some(c1.clone()));
        assert_eq!(
            c1.intersect(&ReadScope::Channels {
                channels: ch(&["C2"]),
            }),
            None
        );
    }
}
