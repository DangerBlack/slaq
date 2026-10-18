//! The mod host.
//!
//! Loads WASM components (plus a `manifest.json`) from a folder, gives them a
//! few host functions, and dispatches messages. Two entry points:
//!
//! - [`Host`] — long-lived instances the app keeps: it loads the installed mods
//!   once and runs them over the real messages of a conversation.
//! - [`run_message`] — a one-shot run used by `slaq mod …` (headless testing).
//!
//! Trust: a mod is untrusted. It gets **no WASI** (guests are built for
//! `wasm32-unknown-unknown`, so they import no `wasi:*`), only the declared host
//! functions; it is bounded by fuel, a memory/table limit and quotas on the
//! host-side data it can grow.
//!
//! Contract: `wit/slaq-mod.wit` (a slice-1 subset of `docs/mods/api-v0.md`).

use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

wasmtime::component::bindgen!({
    path: "wit/slaq-mod.wit",
    world: "plugin",
});

use slaq::api::host::{
    self, FileRef as HostFileRef, Message, SettingValue, SqlResult as WitSqlResult,
    SqlValue as WitSqlValue,
};
use wasmtime::component::HasSelf;
use wasmtime::{StoreLimits, StoreLimitsBuilder};

// The capability vocabulary (`docs/mods/capabilities.md`) and the consent record.
// Kept in their own modules so `mod.rs` stays about the host/runtime.
pub mod capability;
pub mod consent;
pub mod events;
pub mod fs;
pub mod settings;
pub mod sql;
pub mod store;

use capability::{Capability, ReadScope, Requested};
use consent::{Consent, Grants};

/// Cap on the manifest file: a package-controlled blob read before any wasmtime
/// limit exists.
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// Cap on the component file, read (bounded) before compilation. Kept small: it
/// is the input to the (untrusted) compiler and bounds its work.
const MAX_COMPONENT_BYTES: usize = 8 * 1024 * 1024;
/// Per-value cap for the KV store and for decorations.
const MAX_VALUE_BYTES: usize = 64 * 1024;
/// Max decorations a single event may attach.
const MAX_DECORATIONS: usize = 16;
/// Max non-secret `setting-set` calls a mod may make in one callback (each is a
/// synchronous read-modify-write of the settings file, and fuel does not meter
/// host-side I/O).
const MAX_SETTINGS_WRITES_PER_CALL: usize = 32;
/// Bounds on `ui.messageActions`.
const MAX_MESSAGE_ACTIONS: usize = 32;
const MAX_ACTION_ID_BYTES: usize = 64;
const MAX_ACTION_LABEL_BYTES: usize = 200;
/// Bounds on `ui.theme`: how many roles, and the role-name length.
const MAX_THEME_ENTRIES: usize = 40;
const MAX_ROLE_NAME: usize = 40;
/// Bounds on `ui.views` and the content a view may hold.
const MAX_VIEWS: usize = 16;
const MAX_VIEW_BLOCKS: usize = 256;
const MAX_VIEW_BLOCK_BYTES: usize = 8192;
/// Aggregate bytes of all of one mod's cached view content (view content is host
/// heap, outside the guest memory limit, so the total must be capped).
const MAX_VIEW_BYTES: usize = 1024 * 1024;
/// Bounds on `ui-show-dialog`: the title (in characters) and the block/action
/// counts and byte sizes, mirroring the view-content limits (a dialog is host
/// heap too, so the total bytes are capped).
const MAX_DIALOG_TITLE_CHARS: usize = 200;
const MAX_DIALOG_BLOCKS: usize = MAX_VIEW_BLOCKS;
const MAX_DIALOG_BLOCK_BYTES: usize = MAX_VIEW_BLOCK_BYTES;
const MAX_DIALOG_BYTES: usize = MAX_VIEW_BYTES;
const MAX_DIALOG_ACTIONS: usize = MAX_MESSAGE_ACTIONS;
/// Bounds on `file-get` (`files.download`): the opaque id length, and the maximum
/// file size the host will buffer and hand to the guest (a larger file is refused,
/// not truncated).
const MAX_FILE_ID_BYTES: usize = 128;
const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
/// Bounds on `file-upload` (`files.upload`): the filename length and the maximum
/// bytes the host will send (uploads share `MAX_WRITES_PER_DISPATCH`).
const MAX_FILENAME_BYTES: usize = 255;
/// The same cap as a download, for now: they may diverge.
const MAX_UPLOAD_BYTES: usize = MAX_FILE_BYTES;
/// Files the host keeps (and hands to a mod) for one message.
pub const MAX_FILES_PER_MESSAGE: usize = 32;
/// Bounds on `history`/`thread-replies`: messages per call, the reads one mod
/// may make per minute (they reach Slack with the user's session, which the
/// client itself needs too), and the bytes one call may hand back.
const MAX_HISTORY_LIMIT: u32 = 200;
const HISTORY_READS_PER_MINUTE: usize = 30;
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;
/// Bounds on one `vec-upsert`/`vec-search`/`vec-delete` call.
const MAX_VEC_UPSERT_ITEMS: usize = 1024;
const MAX_SEARCH_K: usize = 512;
/// Bounds on `notify-post`: the text caps (in characters) and an upper bound on
/// how many one `dispatch_batch_with` call may raise (each `show_detached` spawns
/// a thread and fuel does not meter that). The label is the mod id, capped here.
const MAX_NOTIFY_TITLE: usize = 200;
const MAX_NOTIFY_BODY: usize = 2000;
const MAX_NOTIFY_LABEL: usize = 64;
const MAX_NOTIFY_PER_DISPATCH: usize = 8;
/// Bounds on `reactions-add`/`reactions-remove`: the emoji-name cap and how many
/// API writes one dispatch may perform (each is a network round trip on the
/// dispatch thread).
const MAX_REACTION_NAME: usize = 64;
const MAX_WRITES_PER_DISPATCH: usize = 8;
/// Bounds on `chat-send`: the message a mod may post as the user (Slack allows up
/// to 40 000 characters).
const MAX_MESSAGE_CHARS: usize = 40_000;
const MAX_MESSAGE_BYTES: usize = 160 * 1024;
/// Bounds on the declared slash `commands`.
const MAX_COMMANDS: usize = 16;
const MAX_COMMAND_NAME: usize = 32;
const MAX_COMMAND_LABEL: usize = 64;
const MAX_COMMAND_DESC: usize = 200;
/// Aggregate bytes of all decorations the host caches.
pub(crate) const MAX_CACHED_BYTES: usize = 4 * 1024 * 1024;
/// Per-key cap for the KV store.
const MAX_KEY_BYTES: usize = 512;
/// Cap on guest-supplied message references (channel/ts), so host-side lookups
/// stay cheap (Wasmtime fuel bounds guest instructions, not host work).
pub(crate) const MAX_REF_BYTES: usize = 256;
/// Cap on a manifest `id` (it keys the consent record). Mirrored by `maxLength`
/// in `docs/mods/manifest.schema.json`.
const MAX_ID_BYTES: usize = 255;
/// `http.fetch` bounds: header count and size, request/response body, wall clock.
const MAX_HTTP_HEADERS: usize = 32;
const MAX_HTTP_HEADER_BYTES: usize = 4096;
const MAX_HTTP_REQUEST_BYTES: usize = 2 << 20;
const MAX_HTTP_RESPONSE_BYTES: usize = 4 << 20;
/// Whole-request wall clock, and a tighter connect phase (a stalled host must
/// not hold the dispatch thread for long).
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const HTTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Bound on the (blocking) DNS resolution, which is otherwise unbounded.
const DNS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Cap on cached per-target HTTP clients (a bare-host entry permits any port).
const MAX_HTTP_CLIENTS: usize = 64;
/// Cap on how many directory entries cross the boundary to a mod (applied to
/// channels and users separately). Also bounds the app's snapshot build.
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 2000;
/// Per-name byte cap, so the boundary copy is bounded by bytes too.
const MAX_DIRECTORY_NAME_BYTES: usize = 256;

/// A file the host will hand to a mod: where it lives and how to fetch it. Built
/// from the messages on screen; the host checks the channel's read scope before
/// serving it, and only Slack-hosted URLs are kept.
#[derive(Clone, Debug)]
pub struct FileRef {
    /// The channel and `ts` of the message the file is attached to.
    pub channel: String,
    pub ts: String,
    /// The URL Slack served (never supplied by the mod).
    pub url: String,
    pub name: String,
    pub mimetype: String,
    pub size: u64,
}

/// The client's directory — channels and users as `(id, name)` pairs — shared by
/// the mods of one dispatch.
#[derive(Default, Clone)]
pub struct Directory {
    pub channels: Vec<(String, String)>,
    pub users: Vec<(String, String)>,
    /// The files the mod may learn about and download, as `id -> FileRef`, from
    /// the messages on screen. The host refuses an id it was not given, and checks
    /// the file's channel against the mod's read scope before serving it.
    pub files: std::collections::HashMap<String, FileRef>,
}

impl Directory {
    /// An empty directory (headless runs, tests).
    pub fn empty() -> Directory {
        Directory::default()
    }
}

/// A handle the settings form uses to read and write one mod's settings. The
/// inner lock is shared with the mod's running store, so an edit takes effect on
/// the next `settings.get`.
#[derive(Clone)]
pub struct SettingsHandle {
    pub id: String,
    pub name: String,
    pub settings: Arc<std::sync::RwLock<settings::Settings>>,
}

/// A loaded mod whose package changed since the user saved some of its
/// secrets (an update, or a replacement reusing the id): the secrets are
/// withheld until the user keeps them for this package or forgets them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleSecrets {
    /// The raw manifest id (the settings/keyring key).
    pub id: String,
    pub name: String,
    /// The secret setting keys waiting for a decision.
    pub keys: Vec<String>,
    /// The package the user would keep them for.
    pub package: String,
    /// The hosts this package may reach.
    pub egress: Vec<String>,
}

/// The fingerprint secrets bind to: the manifest's and the component's.
fn package_fingerprint_of(manifest_fp: &str, component: &[u8]) -> String {
    format!("{manifest_fp}-{}", settings::fingerprint(component))
}

/// The package fingerprint of an installed mod (no compilation), as the host
/// computes it at load — for `slaq mods keep-secrets`.
pub fn package_fingerprint(loaded: &Loaded) -> Result<String> {
    let bytes = read_bounded(&loaded.wasm, MAX_COMPONENT_BYTES)?;
    Ok(package_fingerprint_of(&loaded.fingerprint, &bytes))
}

/// A message action a front end can render, with its owning mod (the raw ids are
/// the routing keys passed back to [`Host::invoke_action`]).
#[derive(Clone)]
pub struct MessageActionRef {
    pub mod_id: String,
    pub mod_name: String,
    pub action_id: String,
    pub label: String,
    pub icon: Option<String>,
}

/// A view a front end can open, with its owning mod.
#[derive(Clone)]
pub struct ViewRef {
    pub mod_id: String,
    pub mod_name: String,
    pub view_id: String,
    pub label: String,
    pub icon: Option<String>,
}

/// A slash command a front end can offer, with its owning mod.
#[derive(Clone)]
pub struct CommandRef {
    pub mod_id: String,
    pub mod_name: String,
    pub name: String,
    pub label: String,
    pub description: String,
}

/// A composer action a front end can offer, with its owning mod.
#[derive(Clone)]
pub struct ComposerActionRef {
    pub mod_id: String,
    pub mod_name: String,
    pub action_id: String,
    pub label: String,
    pub icon: Option<String>,
}
/// Cap on an untrusted diagnostic before it is logged.
const MAX_LOG_BYTES: usize = 256;
/// Aggregate decoration bytes attached during a single event.
const MAX_DECORATION_BYTES: usize = 256 * 1024;
/// Aggregate decoration bytes returned by a single dispatch.
const MAX_DISPATCH_BYTES: usize = 1024 * 1024;
/// Bound on the process-lifetime dispatch caches (`seen`/`decorated`): the oldest
/// entries are evicted FIFO (see `evict_seen`), applied once at the end of each
/// dispatch batch, so a long session cannot grow without bound. The bound is
/// shared across conversations, so cross-reload exactly-once is not guaranteed —
/// handlers must be idempotent.
pub(crate) const MAX_CACHE: usize = 20_000;
/// Default guest linear-memory ceiling, in MiB. A mod is installed by consent and
/// trusted to use the machine's resources; this is a generous backstop against a
/// genuinely runaway allocation, not a budget. Override with `SLAQ_MOD_MEMORY_MB`.
const GUEST_MEMORY_MB_DEFAULT: usize = 512;
/// Epoch ticker interval, and hence the resolution of the wall-clock kill-switch.
const EPOCH_TICK_MS: u64 = 10;
/// Default wall-clock backstop per guest callback, in seconds. Generous on
/// purpose: honest long work — an LLM round-trip, a large re-embed — must never
/// trip it; it exists only to reap a genuinely wedged guest. Override with
/// `SLAQ_MOD_DEADLINE_SECS` (`0` disables it). Slice 1b replaces this with an
/// on-demand kill-switch.
const EPOCH_DEADLINE_SECS_DEFAULT: u64 = 300;
/// Cap on installed mods, so the aggregate guest memory stays bounded. Loading
/// compiles each mod (up to `COMPILE_TIMEOUT`) on the startup path; caching the
/// artifact is a follow-up.
const MAX_MODS: usize = 32;

/// The guest linear-memory ceiling in bytes (env-overridable; see
/// [`GUEST_MEMORY_MB_DEFAULT`]).
fn guest_memory_limit() -> usize {
    let mb = std::env::var("SLAQ_MOD_MEMORY_MB")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&mb| mb > 0)
        .unwrap_or(GUEST_MEMORY_MB_DEFAULT);
    mb.saturating_mul(1 << 20)
}

/// The per-callback epoch deadline in ticks (env-overridable; see
/// [`EPOCH_DEADLINE_SECS_DEFAULT`]). `u64::MAX` effectively disables the backstop.
fn epoch_deadline_ticks() -> u64 {
    let secs = std::env::var("SLAQ_MOD_DEADLINE_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(EPOCH_DEADLINE_SECS_DEFAULT);
    if secs == 0 {
        u64::MAX
    } else {
        // 1000 / EPOCH_TICK_MS = 100 ticks per second.
        secs.saturating_mul(1000 / EPOCH_TICK_MS)
    }
}
/// Env vars used to run this same binary as an out-of-process compile worker.
pub const COMPILE_IN_ENV: &str = "SLAQ_MOD_COMPILE_IN";
pub const COMPILE_OUT_ENV: &str = "SLAQ_MOD_COMPILE_OUT";
/// Wall-clock limit for the compile worker (unused in test builds, which compile
/// in-process).
#[cfg_attr(test, allow(dead_code))]
const COMPILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Cap on the precompiled artifact read back from the worker.
const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;
/// The only manifest API version this host understands.
const SUPPORTED_API: &str = "0.1";
/// Hidden first argument that turns this binary into the out-of-process compile
/// worker. An explicit argv sentinel (not only the env vars) so a stray
/// environment cannot silently turn a normal invocation into a compiler.
pub const WORKER_ARG: &str = "__slaq-mod-compile-worker";

/// Removes a private directory (and its contents) when dropped, so the compile
/// worker's temp files never leak on an early return.
#[cfg_attr(test, allow(dead_code))]
struct TempDirGuard(std::path::PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Deserializes an optional manifest object/array field, rejecting an explicit
/// `null` (the schema requires a value when the field is present; absent uses the
/// `default`).
fn de_non_null<'de, D>(d: D) -> Result<serde_json::Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let v = <serde_json::Value as serde::Deserialize>::deserialize(d)?;
    if v.is_null() {
        return Err(D::Error::custom("field must not be null"));
    }
    Ok(v)
}

/// A message action a mod declares in `ui.messageActions` (slice-1).
#[derive(Clone, Debug, PartialEq)]
pub struct MessageAction {
    pub id: String,
    pub label: String,
    pub icon: Option<String>,
}

/// A view a mod declares in `ui.views` (slice-1).
#[derive(Clone, Debug, PartialEq)]
pub struct ViewDecl {
    pub id: String,
    pub label: String,
    pub icon: Option<String>,
}

/// A composer action a mod declares in `ui.composerActions` (slice-1).
#[derive(Clone, Debug, PartialEq)]
pub struct ComposerAction {
    pub id: String,
    pub label: String,
    pub icon: Option<String>,
}

/// A context-menu item a mod declares in `ui.contextMenu` (slice-1).
#[derive(Clone, Debug, PartialEq)]
pub struct ContextMenuAction {
    pub id: String,
    pub label: String,
    pub icon: Option<String>,
}

/// A modal dialog a mod asked the host to show (`ui-show-dialog`, slice-1). The
/// host renders it; the user dismisses it and an action click is delivered back
/// through `on-dialog-action`.
#[derive(Clone, Debug, PartialEq)]
pub struct Dialog {
    /// The dialog's heading.
    pub title: String,
    /// `(kind, text)` blocks, the same shape as view content.
    pub blocks: Vec<(String, String)>,
    /// `(id, label)` action buttons, in order.
    pub actions: Vec<(String, String)>,
}

/// A pending dialog effect a mod requested during a callback. The front end
/// drains these after the guest runs and is the sole owner of the modal's
/// lifetime (dismissing it is app-side only).
#[derive(Clone, Debug, PartialEq)]
pub enum DialogEffect {
    /// Show (or replace) the mod's dialog.
    Show(Dialog),
    /// Close the mod's dialog.
    Close,
}

/// A palette a manifest declares under `ui.theme`: role name -> `rrggbb`.
///
/// Strings, not colour types: this layer compiles without `gui` (TUI-only
/// builds) and the front end resolves the names against `theme::Role`. Unknown
/// roles and malformed hex are dropped (lenient): a bad theme degrades to the
/// default rather than refusing the package, and an *unreadable* theme is the
/// user's to remove — the host does not police contrast here.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Theme {
    pub entries: Vec<(String, String)>,
}

/// `#rrggbb` / `rrggbb` -> lowercase `rrggbb`, or `None` when malformed.
fn normalize_hex(s: &str) -> Option<String> {
    let h = s.strip_prefix('#').unwrap_or(s);
    (h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit())).then(|| h.to_ascii_lowercase())
}

/// Parses `ui.theme` (role -> `#rrggbb`). Lenient and bounded.
fn parse_theme(v: Option<&serde_json::Value>) -> Theme {
    let Some(obj) = v.and_then(|v| v.as_object()) else {
        return Theme::default();
    };
    let mut entries = Vec::new();
    for (role, hex) in obj {
        if entries.len() >= MAX_THEME_ENTRIES {
            break;
        }
        if role.is_empty() || role.len() > MAX_ROLE_NAME {
            continue;
        }
        let Some(hex) = hex.as_str().and_then(normalize_hex) else {
            continue;
        };
        entries.push((role.clone(), hex));
    }
    Theme { entries }
}

/// The static UI contributions a manifest declares, validated.
#[derive(Default)]
struct UiContributions {
    actions: Vec<MessageAction>,
    views: Vec<ViewDecl>,
    composer_actions: Vec<ComposerAction>,
    context_actions: Vec<ContextMenuAction>,
    theme: Theme,
}

/// Parses and validates the manifest `ui` object (slice-1: `messageActions`,
/// `views`, `composerActions` and `contextMenu`). Any other `ui.*` key is refused
/// (fail closed).
fn parse_ui(ui: &serde_json::Value) -> Result<UiContributions> {
    if ui.is_null() {
        return Ok(UiContributions::default());
    }
    let obj = ui
        .as_object()
        .ok_or_else(|| anyhow!("`ui` must be an object"))?;
    for k in obj.keys() {
        if !matches!(
            k.as_str(),
            "messageActions" | "views" | "composerActions" | "contextMenu" | "theme"
        ) {
            bail!(
                "ui: `{k}` is not supported by this host (only `messageActions`, `views`, `composerActions`, `contextMenu`, `theme`)"
            );
        }
    }
    let actions = parse_ui_items(
        obj.get("messageActions"),
        "ui.messageActions",
        MAX_MESSAGE_ACTIONS,
    )?
    .into_iter()
    .map(|(id, label, icon)| MessageAction { id, label, icon })
    .collect();
    let views = parse_ui_items(obj.get("views"), "ui.views", MAX_VIEWS)?
        .into_iter()
        .map(|(id, label, icon)| ViewDecl { id, label, icon })
        .collect();
    let composer_actions = parse_ui_items(
        obj.get("composerActions"),
        "ui.composerActions",
        MAX_MESSAGE_ACTIONS,
    )?
    .into_iter()
    .map(|(id, label, icon)| ComposerAction { id, label, icon })
    .collect();
    let context_actions = parse_ui_items(
        obj.get("contextMenu"),
        "ui.contextMenu",
        MAX_MESSAGE_ACTIONS,
    )?
    .into_iter()
    .map(|(id, label, icon)| ContextMenuAction { id, label, icon })
    .collect();
    let theme = parse_theme(obj.get("theme"));
    Ok(UiContributions {
        actions,
        views,
        composer_actions,
        context_actions,
        theme,
    })
}

/// Parses a `ui.messageActions`/`ui.views` array: unique, bounded ids;
/// `id`/`label` required, optional `icon`.
fn parse_ui_items(
    v: Option<&serde_json::Value>,
    what: &str,
    max: usize,
) -> Result<Vec<(String, String, Option<String>)>> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let list = v
        .as_array()
        .ok_or_else(|| anyhow!("{what} must be an array"))?;
    if list.len() > max {
        bail!("{what}: too many (>{max})");
    }
    let mut out: Vec<(String, String, Option<String>)> = Vec::with_capacity(list.len());
    for item in list {
        let o = item
            .as_object()
            .ok_or_else(|| anyhow!("{what}: each entry must be an object"))?;
        for k in o.keys() {
            if !matches!(k.as_str(), "id" | "label" | "icon") {
                bail!("{what}: unknown key {k:?}");
            }
        }
        let id = o
            .get("id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("{what}: `id` (a string) is required"))?;
        if !valid_action_id(id) {
            bail!("{what}: invalid id {id:?}");
        }
        if out.iter().any(|(i, _, _)| i == id) {
            bail!("{what}: duplicate id {id:?}");
        }
        let label = o
            .get("label")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("{what}: `label` (a string) is required"))?;
        if label.is_empty() || label.len() > MAX_ACTION_LABEL_BYTES {
            bail!("{what}: label for {id:?} is empty or too long");
        }
        let icon = match o.get("icon") {
            None => None,
            Some(x) => {
                let s = x
                    .as_str()
                    .ok_or_else(|| anyhow!("{what}: `icon` must be a string"))?;
                if s.len() > MAX_ACTION_LABEL_BYTES {
                    bail!("{what}: icon for {id:?} too long");
                }
                Some(s.to_string())
            }
        };
        out.push((id.to_string(), label.to_string(), icon));
    }
    Ok(out)
}

/// A message-action id: a letter followed by `[A-Za-z0-9_-]` (a routing key).
fn valid_action_id(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_ACTION_ID_BYTES {
        return false;
    }
    let mut b = s.bytes();
    let first = b.next().unwrap();
    first.is_ascii_alphabetic() && b.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// A slash command a mod declares in `commands` (slice-1). `label` defaults to
/// the name, `description` to empty.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandDecl {
    pub name: String,
    pub label: String,
    pub description: String,
}

/// A command name: `[a-z0-9][a-z0-9_-]*` (`docs/mods/manifest.schema.json`).
fn valid_command_name(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_COMMAND_NAME {
        return false;
    }
    let mut b = s.bytes();
    let first = b.next().unwrap();
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

/// A Slack emoji name for `reactions.*`: `[a-z0-9_+-]`, non-empty, bounded
/// (e.g. `thumbsup`, `+1`, `heavy_check_mark`).
fn valid_reaction_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_REACTION_NAME
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'+' | b'-')
        })
}

/// Parses and validates the manifest `commands` array: unique names matching
/// `^[a-z0-9][a-z0-9_-]*$`, optional `label`/`description`.
fn parse_commands(v: &serde_json::Value) -> Result<Vec<CommandDecl>> {
    if v.is_null() {
        return Ok(Vec::new());
    }
    let list = v
        .as_array()
        .ok_or_else(|| anyhow!("`commands` must be an array"))?;
    if list.len() > MAX_COMMANDS {
        bail!("commands: too many (>{MAX_COMMANDS})");
    }
    let mut out: Vec<CommandDecl> = Vec::with_capacity(list.len());
    for item in list {
        let o = item
            .as_object()
            .ok_or_else(|| anyhow!("commands: each entry must be an object"))?;
        for k in o.keys() {
            if !matches!(k.as_str(), "name" | "label" | "description") {
                bail!("commands: unknown key {k:?}");
            }
        }
        let name = o
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("commands: `name` (a string) is required"))?;
        if !valid_command_name(name) {
            bail!("commands: invalid name {name:?}");
        }
        if out.iter().any(|c| c.name == name) {
            bail!("commands: duplicate name {name:?}");
        }
        let label = match o.get("label") {
            None => name.to_string(),
            Some(x) => {
                let s = x
                    .as_str()
                    .ok_or_else(|| anyhow!("commands: `label` must be a string"))?;
                if s.is_empty() || s.len() > MAX_COMMAND_LABEL {
                    bail!("commands: label for {name:?} is empty or too long");
                }
                s.to_string()
            }
        };
        let description = match o.get("description") {
            None => String::new(),
            Some(x) => {
                let s = x
                    .as_str()
                    .ok_or_else(|| anyhow!("commands: `description` must be a string"))?;
                if s.len() > MAX_COMMAND_DESC {
                    bail!("commands: description for {name:?} too long");
                }
                s.to_string()
            }
        };
        out.push(CommandDecl {
            name: name.to_string(),
            label,
            description,
        });
    }
    Ok(out)
}

/// Slice-1 subset of the manifest (`docs/mods/manifest.schema.json`). The
/// version fields are required and checked before any code is loaded.
#[derive(serde::Deserialize)]
pub struct Manifest {
    #[serde(rename = "manifestVersion")]
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// The oldest slaq the mod works with (`minSlaq`, semver). Optional; a newer
    /// requirement than this build refuses the package with a clear message.
    #[serde(rename = "minSlaq", default)]
    pub min_slaq: Option<String>,
    pub entry: Entry,
    pub capabilities: serde_json::Value,
    /// The optional `settings` schema. Absent when the mod declares none; an
    /// explicit `null` is rejected (`de_non_null`).
    #[serde(default, deserialize_with = "de_non_null")]
    pub settings: serde_json::Value,
    /// The static UI contributions (`ui.messageActions`). Absent when none;
    /// `null` rejected.
    #[serde(default, deserialize_with = "de_non_null")]
    pub ui: serde_json::Value,
    /// Passive event subscriptions (`events`). Absent when none; `null` rejected.
    #[serde(default, deserialize_with = "de_non_null")]
    pub events: serde_json::Value,
    /// The declared slash commands (`commands`). Absent when none; `null` rejected.
    #[serde(default, deserialize_with = "de_non_null")]
    pub commands: serde_json::Value,
}

#[derive(serde::Deserialize)]
pub struct Entry {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
}

/// A mod read from disk.
pub struct Loaded {
    pub manifest: Manifest,
    /// The validated capability request. Only names this host implements.
    pub requested: Requested,
    /// The validated `settings` schema (empty when the mod declares none).
    pub settings: settings::Schema,
    /// The declared message actions (empty when none).
    pub actions: Vec<MessageAction>,
    /// The declared views (empty when none).
    pub views: Vec<ViewDecl>,
    /// The declared composer actions (empty when none).
    pub composer_actions: Vec<ComposerAction>,
    /// The declared context-menu items (empty when none).
    pub context_actions: Vec<ContextMenuAction>,
    /// The declared theme overrides (empty when none).
    pub theme: Theme,
    /// The validated event subscriptions (empty when none).
    pub events: Vec<events::Event>,
    /// The declared slash commands (empty when none).
    pub commands: Vec<CommandDecl>,
    /// A fingerprint of the manifest bytes (combined with the component for the
    /// package fingerprint that binds secrets).
    pub fingerprint: String,
    pub wasm: PathBuf,
}

/// What one headless run produced. `id`/`name` are untrusted text: the caller
/// sanitizes before printing.
pub struct RunOutcome {
    pub id: String,
    pub name: String,
    pub decorations: Vec<(String, String, String)>,
}

/// A file attached to a message the mods may see (metadata; the bytes come from
/// `file-get`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModFile {
    pub id: String,
    pub name: String,
    pub mime: String,
}

/// A message handed to the mods: its `ts`, author, text, thread info and files,
/// in `channel`. Also the value of the host's per-message read index (which
/// `get-message` serves and `files.download` resolves against).
#[derive(Clone, Debug, Default)]
pub struct Msg {
    pub ts: String,
    pub author: String,
    pub text: String,
    pub thread_ts: Option<String>,
    pub reply_count: u32,
    pub files: Vec<ModFile>,
}

impl Msg {
    /// A message with just its text (the slice-1 dispatch fixtures and the CLI).
    pub fn text_only(ts: &str, author: &str, text: &str) -> Msg {
        Msg {
            ts: ts.to_string(),
            author: author.to_string(),
            text: text.to_string(),
            thread_ts: None,
            reply_count: 0,
            files: Vec::new(),
        }
    }
}

/// Reads and minimally validates a mod folder: the manifest is parsed (bounded),
/// its versions are checked, and the entry path is constrained to the package
/// folder — no absolute path, no `..`, and after canonicalization (which
/// resolves symlinks) it must still resolve **inside** the folder.
pub fn load(dir: &Path) -> Result<Loaded> {
    let manifest_path = dir.join("manifest.json");
    // Open once and read with a hard cap: a stat-based check is a TOCTOU race
    // (the file can be replaced or grown between the stat and the read).
    let bytes = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
    // Fingerprint the manifest bytes too: a replacement could keep the same wasm
    // while changing the schema/defaults, and the secrets must not survive that.
    let fingerprint = settings::fingerprint(&bytes);
    let text = String::from_utf8(bytes)
        .with_context(|| format!("manifest is not UTF-8: {}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    // The id keys the consent record, so it must be the schema's reverse-DNS
    // shape before anything else trusts it.
    if !valid_mod_id(&manifest.id) {
        return Err(anyhow!(
            "invalid mod id (reverse-DNS expected): {}",
            sanitize(&manifest.id)
        ));
    }
    if manifest.manifest_version != 0 {
        return Err(anyhow!(
            "unsupported manifestVersion: {}",
            manifest.manifest_version
        ));
    }
    if manifest.api_version != SUPPORTED_API {
        return Err(anyhow!(
            "unsupported apiVersion: {} (this host supports {SUPPORTED_API})",
            manifest.api_version
        ));
    }
    if let Some(min) = &manifest.min_slaq {
        let need = parse_semver(min)
            .ok_or_else(|| anyhow!("minSlaq is not a version: {}", sanitize(min)))?;
        let have = parse_semver(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0));
        if need > have {
            return Err(anyhow!(
                "needs slaq {} or newer (this is {})",
                sanitize(min),
                env!("CARGO_PKG_VERSION")
            ));
        }
    }
    if manifest.entry.kind != "wasm" {
        return Err(anyhow!("unsupported entry type: {}", manifest.entry.kind));
    }
    let rel = Path::new(&manifest.entry.path);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(anyhow!(
            "entry path must be package-relative: {}",
            manifest.entry.path
        ));
    }
    let wasm = dir.join(rel);
    if !wasm.is_file() {
        return Err(anyhow!("component not found: {}", wasm.display()));
    }
    let dir_canon = dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))?;
    let wasm_canon = wasm
        .canonicalize()
        .with_context(|| format!("resolving {}", wasm.display()))?;
    if !wasm_canon.starts_with(&dir_canon) {
        return Err(anyhow!(
            "entry escapes the package folder: {}",
            manifest.entry.path
        ));
    }
    // The capability request is validated here, at load: an unknown, reserved or
    // not-yet-implemented name refuses the package (fail closed) before any code
    // is compiled or run.
    let requested = Requested::parse(&manifest.capabilities)
        .with_context(|| format!("invalid capabilities in {}", manifest_path.display()))?;
    // The settings schema is validated here too, and a non-empty one requires
    // `ui.settings` — the host-rendered form is what makes it configurable.
    let settings = if manifest.settings.is_null() {
        settings::Schema::default()
    } else {
        settings::Schema::parse(&manifest.settings)
            .with_context(|| format!("invalid settings in {}", manifest_path.display()))?
    };
    if !settings.is_empty() && !requested.caps().contains(&Capability::UiSettings) {
        return Err(anyhow!(
            "a non-empty `settings` requires the `ui.settings` capability"
        ));
    }
    // The declared UI contributions: validated, and each non-empty list requires
    // its capability.
    let ui = parse_ui(&manifest.ui)
        .with_context(|| format!("invalid ui in {}", manifest_path.display()))?;
    if !ui.actions.is_empty() && !requested.caps().contains(&Capability::UiMessageAction) {
        return Err(anyhow!(
            "a non-empty `ui.messageActions` requires the `ui.message_action` capability"
        ));
    }
    if !ui.views.is_empty() && !requested.caps().contains(&Capability::UiView) {
        return Err(anyhow!(
            "a non-empty `ui.views` requires the `ui.view` capability"
        ));
    }
    if !ui.composer_actions.is_empty() && !requested.caps().contains(&Capability::UiComposerAction)
    {
        return Err(anyhow!(
            "a non-empty `ui.composerActions` requires the `ui.composer_action` capability"
        ));
    }
    // A composer action reads the conversation's draft, which the read scope
    // bounds: without `messages.read` it could never be invoked.
    if !ui.composer_actions.is_empty() && !requested.caps().contains(&Capability::MessagesRead) {
        return Err(anyhow!(
            "a non-empty `ui.composerActions` also requires `messages.read` (the scope it acts within)"
        ));
    }
    if !ui.context_actions.is_empty() && !requested.caps().contains(&Capability::UiContextMenu) {
        return Err(anyhow!(
            "a non-empty `ui.contextMenu` requires the `ui.context_menu` capability"
        ));
    }
    if !ui.theme.entries.is_empty() && !requested.caps().contains(&Capability::UiTheme) {
        return Err(anyhow!(
            "a non-empty `ui.theme` requires the `ui.theme` capability"
        ));
    }
    // The event subscriptions: validated against the vocabulary and the request
    // (an event requires its capability to be requested too).
    let events = events::parse_events(&manifest.events, requested.caps())
        .with_context(|| format!("invalid events in {}", manifest_path.display()))?;
    // The slash commands: validated, and a non-empty list needs `commands.slash`.
    let commands = parse_commands(&manifest.commands)
        .with_context(|| format!("invalid commands in {}", manifest_path.display()))?;
    if !commands.is_empty() && !requested.caps().contains(&Capability::CommandsSlash) {
        return Err(anyhow!(
            "a non-empty `commands` requires the `commands.slash` capability"
        ));
    }
    Ok(Loaded {
        manifest,
        requested,
        settings,
        actions: ui.actions,
        views: ui.views,
        composer_actions: ui.composer_actions,
        context_actions: ui.context_actions,
        theme: ui.theme,
        events,
        commands,
        fingerprint,
        wasm: wasm_canon,
    })
}

/// Host-side state a mod sees through its imports.
struct HostState {
    /// What the user granted this mod. Every host function is gated on it.
    grants: Grants,
    /// The Slack API client, for the `Write` capabilities (`reactions.write`
    /// today). `None` until the app installs it, so a write fails closed.
    api: Option<Arc<crate::api::Client>>,
    /// The manifest's `net.egress` allow-list (lowercase `host[:port]`).
    egress: Vec<String>,
    /// Per-target async HTTP clients for `http.fetch`, each pinned to a vetted
    /// address so a re-resolving name cannot rebind between the check and the
    /// connection.
    http: HashMap<String, reqwest::Client>,
    /// The client's directory for `list-channels`/`list-users`. Installed at load
    /// (available from `start`) and refreshed on every dispatch; `None` only if a
    /// store was somehow built without one.
    directory: Option<Arc<Directory>>,
    /// `(channel, ts) -> Msg`: the messages the mod may read. Shared (`Arc`)
    /// across the mods of one dispatch, so the conversation is indexed once
    /// instead of cloned into every store.
    messages: Arc<HashMap<(String, String), Msg>>,
    /// The mod's private persistent store (`store.kv` + `store.vector`), when
    /// either is declared. A `rusqlite::Connection` is `Send` (not `Sync`); the
    /// store is used only through `&mut HostState`, and the file is opened
    /// lazily, on first use. It survives reloads, so the mod can keep state.
    store: Option<store::ModStore>,
    /// The mod's private SQL DB (`store.sql`), when declared (`scratch.db`); a
    /// **separate** file from `store.db`, opened lazily on first use.
    sql: Option<sql::ModSql>,
    /// The `filesystem` sandbox, when the capability is granted: the map of the
    /// user-mapped slots the host resolves every disk access against. `None`
    /// otherwise, so a call fails closed.
    fs: Option<fs::FsSandbox>,
    /// Decorations attached during the current event.
    decorations: Vec<(String, String, String)>,
    /// Aggregate bytes held in `decorations`, to enforce the quota.
    decoration_bytes: usize,
    /// The message currently being processed: only it may be decorated.
    current: Option<(String, String)>,
    /// The mod's live settings, shared with the settings form (`settings.*` is
    /// ungated). Empty when the mod declares none.
    settings: Arc<std::sync::RwLock<settings::Settings>>,
    /// Per-view content the mod pushed (`set-view-content`), keyed by view id.
    view_content: HashMap<String, Vec<(String, String)>>,
    /// The view ids this mod declared (`ui.views`), so `set-view-content` can be
    /// validated against them.
    declared_views: Vec<String>,
    /// A dialog the mod asked to show or close (`ui-show-dialog` /
    /// `ui-close-dialog`), drained by the app after the guest runs.
    pending_dialog: Option<DialogEffect>,
    /// The action ids of the last dialog the mod showed, so `on-dialog-action`
    /// can be validated against what was actually offered.
    dialog_actions: Vec<String>,
    /// `setting-set` writes in the current callback (see
    /// [`MAX_SETTINGS_WRITES_PER_CALL`]).
    settings_writes: usize,
    /// The mod's id (sanitized), so a `notify-post` can be attributed to it.
    notify_label: String,
    /// `notify-post` calls in the current `dispatch_batch_with` call (see
    /// [`MAX_NOTIFY_PER_DISPATCH`]).
    notify_count: usize,
    /// API writes (`reactions-add`/`-remove`) in the current dispatch (see
    /// [`MAX_WRITES_PER_DISPATCH`]).
    api_writes: usize,
    /// When this mod's recent `history`/`thread-replies` reads started, for the
    /// per-minute pace ([`HISTORY_READS_PER_MINUTE`]). Kept across dispatches:
    /// a backfill runs over many background ticks.
    history_reads: VecDeque<std::time::Instant>,
    /// The composer text a mod set during a composer-action callback
    /// (`ui-set-composer-text`), drained by [`Host::invoke_composer_action`].
    composer_edit: Option<String>,
    /// The channel of the composer action currently running: `ui-set-composer-text`
    /// is honored only then, and only for that channel.
    composer_action_channel: Option<String>,
    /// Resource limits (memory/tables) enforced by wasmtime.
    limits: StoreLimits,
    /// The tokio runtime handle used to drive blocking host I/O (http/dns/api).
    /// Captured at construction — the loader runs inside the runtime — because a
    /// mod runs on a plain OS thread, which has no ambient runtime of its own, so
    /// `Handle::try_current()` there would fail. `None` when built outside a
    /// runtime (the one-shot CLI / unit tests without one), where network I/O then
    /// fails closed.
    rt_handle: Option<tokio::runtime::Handle>,
    /// The mod's kill flag (shared with its [`ModHandle`]): a blocked host call
    /// polls it so stopping a mod also cancels the request it is waiting on,
    /// instead of waiting for the request's own timeout.
    kill: Arc<std::sync::atomic::AtomicBool>,
}

impl HostState {
    fn new(grants: Grants, egress: Vec<String>) -> Self {
        Self {
            grants,
            api: None,
            egress,
            http: HashMap::new(),
            directory: None,
            messages: Arc::new(HashMap::new()),
            store: None,
            sql: None,
            fs: None,
            decorations: Vec::new(),
            decoration_bytes: 0,
            current: None,
            settings: Arc::new(std::sync::RwLock::new(settings::Settings::default())),
            view_content: HashMap::new(),
            declared_views: Vec::new(),
            pending_dialog: None,
            dialog_actions: Vec::new(),
            settings_writes: 0,
            notify_label: String::new(),
            notify_count: 0,
            api_writes: 0,
            history_reads: VecDeque::new(),
            composer_edit: None,
            composer_action_channel: None,
            limits: StoreLimitsBuilder::new()
                .memory_size(guest_memory_limit())
                .memories(1)
                .tables(10)
                .table_elements(65_536)
                .build(),
            rt_handle: tokio::runtime::Handle::try_current().ok(),
            kill: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// The checks shared by `history`/`thread-replies`: the grant and scope,
    /// bounded references, a `limit` in `1..=MAX_HISTORY_LIMIT`, and the
    /// per-minute pace (a read is counted when it is allowed to start).
    fn check_history_read(
        &mut self,
        what: &str,
        channel: &str,
        ts: Option<&str>,
        limit: u32,
    ) -> Result<u32, String> {
        if !self.grants.can_read(channel) {
            return Err(denied(Capability::MessagesRead));
        }
        if channel.len() > MAX_REF_BYTES || ts.is_some_and(|t| t.len() > MAX_REF_BYTES) {
            return Err(format!("{what}: reference too long"));
        }
        if limit == 0 || limit > MAX_HISTORY_LIMIT {
            return Err(format!("{what}: limit must be 1..={MAX_HISTORY_LIMIT}"));
        }
        let now = std::time::Instant::now();
        while self
            .history_reads
            .front()
            .is_some_and(|t| now.duration_since(*t) >= std::time::Duration::from_secs(60))
        {
            self.history_reads.pop_front();
        }
        if self.history_reads.len() >= HISTORY_READS_PER_MINUTE {
            return Err(format!(
                "rate-limited: at most {HISTORY_READS_PER_MINUTE} history reads per minute"
            ));
        }
        self.history_reads.push_back(now);
        Ok(limit)
    }

    /// The API client for a history read (none installed: fail closed).
    fn history_api(&self, what: &str) -> Result<Arc<crate::api::Client>, String> {
        self.api
            .clone()
            .ok_or_else(|| format!("{what}: no API client installed"))
    }

    /// The runtime to start a host request on — refused once the mod is stopped,
    /// so a stopped guest cannot start a new write in the window before the epoch
    /// kill-switch reaps it (`await_io` only cancels requests already started).
    fn io_rt(&self, what: &str) -> Result<tokio::runtime::Handle, String> {
        if self.kill.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(format!("{what}: cancelled (mod stopped)"));
        }
        self.rt().ok_or_else(|| format!("{what}: no async runtime"))
    }

    /// The tokio runtime handle to drive blocking host I/O on: the one captured at
    /// construction, or the ambient one if we happen to be on a runtime thread.
    /// `None` means no runtime is reachable, so network I/O must fail closed.
    fn rt(&self) -> Option<tokio::runtime::Handle> {
        self.rt_handle
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
    }
}

/// The error a guest sees when it calls a host function it was not granted (or
/// outside scope). Logged at **debug**: design rule 1 wants denials recorded, but
/// a mod can call in a loop, so they must not be able to flood the default
/// (info) logs.
fn denied(cap: Capability) -> String {
    tracing::debug!("mod host call denied: {}", cap.name());
    format!("permission-denied: {}", cap.name())
}

impl host::Host for HostState {
    fn get_message(&mut self, channel: String, ts: String) -> Result<Message, String> {
        // Scope is enforced per channel, not once at load.
        if !self.grants.can_read(&channel) {
            return Err(denied(Capability::MessagesRead));
        }
        if channel.len() > MAX_REF_BYTES || ts.len() > MAX_REF_BYTES {
            return Err("get-message: reference too long".to_string());
        }
        let m = self
            .messages
            .get(&(channel, ts))
            .cloned()
            .ok_or_else(|| "not-found".to_string())?;
        Ok(Message {
            author: m.author,
            ts: m.ts,
            text: m.text,
            thread_ts: m.thread_ts,
            reply_count: m.reply_count,
            files: m
                .files
                .into_iter()
                .map(|f| HostFileRef {
                    id: f.id,
                    name: f.name,
                    mime: f.mime,
                })
                .collect(),
        })
    }

    /// `history`: a page of `channel`'s messages older than `before`, newest
    /// first, from Slack. Needs `messages.read` within scope; paced per mod.
    fn history(
        &mut self,
        channel: String,
        before: Option<String>,
        limit: u32,
    ) -> Result<Vec<Message>, String> {
        let limit = self.check_history_read("history", &channel, before.as_deref(), limit)?;
        let api = self.history_api("history")?;
        let handle = self.io_rt("history")?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<serde_json::Value>, String>>(1);
        let task = handle.spawn(async move {
            let res = api
                .conversations_history_range(&channel, Some(limit), None, before.as_deref())
                .await;
            let _ = tx.send(res.map_err(|e| history_error(&e.to_string())));
        });
        let raw = await_io(rx, task, &self.kill, "history")?;
        Ok(history_messages(&raw, limit as usize))
    }

    /// `thread-replies`: a thread's messages, the parent first, from Slack.
    /// Needs `messages.read` within scope; paced like `history`.
    fn thread_replies(
        &mut self,
        channel: String,
        thread_ts: String,
        limit: u32,
    ) -> Result<Vec<Message>, String> {
        let limit = self.check_history_read("thread-replies", &channel, Some(&thread_ts), limit)?;
        let api = self.history_api("thread-replies")?;
        let handle = self.io_rt("thread-replies")?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<serde_json::Value>, String>>(1);
        let task = handle.spawn(async move {
            let res = api.conversations_replies(&channel, &thread_ts).await;
            let _ = tx.send(res.map_err(|e| history_error(&e.to_string())));
        });
        let raw = await_io(rx, task, &self.kill, "thread-replies")?;
        Ok(history_messages(&raw, limit as usize))
    }

    fn kv_get(&mut self, key: String) -> Result<Option<String>, String> {
        if !self.grants.has(Capability::StoreKv) {
            return Err(denied(Capability::StoreKv));
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(format!("key exceeds {MAX_KEY_BYTES} bytes"));
        }
        let store = self
            .store
            .as_mut()
            .filter(|s| s.has_kv())
            .ok_or_else(|| "store.kv not configured".to_string())?;
        store
            .kv_get(&key)
            .map_err(|e| sanitize_log(&format!("{e}")))
    }

    fn kv_set(&mut self, key: String, value: String) -> Result<(), String> {
        if !self.grants.has(Capability::StoreKv) {
            return Err(denied(Capability::StoreKv));
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(format!("key exceeds {MAX_KEY_BYTES} bytes"));
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(format!("value exceeds {MAX_VALUE_BYTES} bytes"));
        }
        let store = self
            .store
            .as_mut()
            .filter(|s| s.has_kv())
            .ok_or_else(|| "store.kv not configured".to_string())?;
        store
            .kv_set(&key, &value)
            .map_err(|e| sanitize_log(&format!("{e}")))
    }

    fn decorate(&mut self, channel: String, ts: String, text: String) -> Result<(), String> {
        if !self.grants.has(Capability::UiMessageDecoration) {
            return Err(denied(Capability::UiMessageDecoration));
        }
        if channel.len() > MAX_REF_BYTES || ts.len() > MAX_REF_BYTES {
            return Err("decorate: reference too long".to_string());
        }
        // v0: a mod may only decorate the message it is currently handling, so
        // every cached decoration has exactly one source event.
        if self.current.as_ref().map(|(c, t)| (c.as_str(), t.as_str()))
            != Some((channel.as_str(), ts.as_str()))
        {
            return Err("decorate: only the current message may be decorated".to_string());
        }
        if !self.messages.contains_key(&(channel.clone(), ts.clone())) {
            return Err("decorate: unknown or out-of-scope message".to_string());
        }
        if text.len() > MAX_VALUE_BYTES {
            return Err(format!("decoration exceeds {MAX_VALUE_BYTES} bytes"));
        }
        if self.decorations.len() >= MAX_DECORATIONS {
            return Err(format!("too many decorations (>{MAX_DECORATIONS})"));
        }
        if self.decoration_bytes + text.len() > MAX_DECORATION_BYTES {
            return Err(format!("decorations exceed {MAX_DECORATION_BYTES} bytes"));
        }
        let text = sanitize_multiline(&text);
        self.decoration_bytes += text.len();
        self.decorations.push((channel, ts, text));
        Ok(())
    }

    /// Ungated (like `settings.*`): the granted set is the host's own record, so
    /// a mod may always ask what it holds and degrade gracefully on partial
    /// consent.
    fn list_grants(&mut self) -> Vec<String> {
        self.grants.names()
    }

    fn http_fetch(
        &mut self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> Result<(u16, Vec<u8>), String> {
        if !self.grants.has(Capability::NetEgress) {
            return Err(denied(Capability::NetEgress));
        }
        let rt = self.io_rt("http-fetch")?;
        egress_fetch(
            &mut self.http,
            &self.egress,
            &method,
            &url,
            &headers,
            body.as_deref(),
            &rt,
            &self.kill,
        )
    }

    fn list_channels(&mut self) -> Result<Vec<(String, String)>, String> {
        if !self.grants.has(Capability::ChannelsList) {
            return Err(denied(Capability::ChannelsList));
        }
        let Some(dir) = self.directory.as_ref() else {
            return Err(
                "unavailable: the directory is only available during a message callback"
                    .to_string(),
            );
        };
        Ok(dir
            .channels
            .iter()
            .take(MAX_DIRECTORY_ENTRIES)
            .map(|(id, name)| (bounded_name(id), bounded_name(name)))
            .collect())
    }

    fn list_users(&mut self) -> Result<Vec<(String, String)>, String> {
        if !self.grants.has(Capability::UsersList) {
            return Err(denied(Capability::UsersList));
        }
        let Some(dir) = self.directory.as_ref() else {
            return Err(
                "unavailable: the directory is only available during a message callback"
                    .to_string(),
            );
        };
        Ok(dir
            .users
            .iter()
            .take(MAX_DIRECTORY_ENTRIES)
            .map(|(id, name)| (bounded_name(id), bounded_name(name)))
            .collect())
    }

    /// `setting-get`: the value the user configured for `key`, or `none`.
    /// `settings.*` is ungated — always available, never `permission-denied`.
    fn setting_get(&mut self, key: String) -> Result<Option<SettingValue>, String> {
        let s = self.settings.read().unwrap_or_else(|e| e.into_inner());
        Ok(s.get(&key).map(to_wit_value))
    }

    /// `setting-set`: writes a **non-secret** setting (secrets are read-only to
    /// the mod and are written only by the user in the host-rendered form).
    fn setting_set(&mut self, key: String, value: SettingValue) -> Result<(), String> {
        if self.settings_writes >= MAX_SETTINGS_WRITES_PER_CALL {
            return Err("too many setting writes in one callback".to_string());
        }
        self.settings_writes += 1;
        let mut s = self.settings.write().unwrap_or_else(|e| e.into_inner());
        s.set(&key, from_wit_value(value))
            .map_err(|e| sanitize_log(&format!("{e}")))
    }

    /// `set-view-content`: the content of a view the mod declared. Needs
    /// `ui.view`; the view id must be one the manifest declared, and each block
    /// kind/size is validated (trusted-ish, but bounded).
    fn set_view_content(
        &mut self,
        view: String,
        blocks: Vec<(String, String)>,
    ) -> Result<(), String> {
        if !self.grants.has(Capability::UiView) {
            return Err(denied(Capability::UiView));
        }
        if !self.declared_views.iter().any(|v| v == &view) {
            return Err("set-view-content: unknown view".to_string());
        }
        if blocks.len() > MAX_VIEW_BLOCKS {
            return Err("set-view-content: too many blocks".to_string());
        }
        let mut out: Vec<(String, String)> = Vec::with_capacity(blocks.len());
        for (kind, text) in blocks {
            if !matches!(kind.as_str(), "section" | "context" | "divider") {
                return Err("set-view-content: unknown block kind".to_string());
            }
            // A divider carries no text.
            let text = if kind == "divider" {
                String::new()
            } else {
                text
            };
            if text.len() > MAX_VIEW_BLOCK_BYTES {
                return Err("set-view-content: block too long".to_string());
            }
            out.push((kind, sanitize_multiline(&text)));
        }
        // Aggregate cap: view content is host heap, outside the guest limit.
        let others: usize = self
            .view_content
            .iter()
            .filter(|(k, _)| k.as_str() != view)
            .map(|(_, v)| v.iter().map(|(_, t)| t.len()).sum::<usize>())
            .sum();
        let new_bytes: usize = out.iter().map(|(_, t)| t.len()).sum();
        if others + new_bytes > MAX_VIEW_BYTES {
            return Err("set-view-content: view content quota exceeded".to_string());
        }
        self.view_content.insert(view, out);
        Ok(())
    }

    /// `ui-set-composer-text`: the text the composer should hold after the action
    /// the user invoked. Needs `ui.composer_action`; honored only during the
    /// `on-composer-action` callback it belongs to (a passive event cannot hijack
    /// the composer).
    fn ui_set_composer_text(&mut self, channel: String, text: String) -> Result<(), String> {
        if !self.grants.has(Capability::UiComposerAction) {
            return Err(denied(Capability::UiComposerAction));
        }
        if channel.len() > MAX_REF_BYTES {
            return Err("ui-set-composer-text: channel reference too long".to_string());
        }
        if self.composer_action_channel.as_deref() != Some(channel.as_str()) {
            return Err(
                "ui-set-composer-text: only during a composer action for that channel".to_string(),
            );
        }
        let text = sanitize(&text);
        if text.chars().count() > MAX_MESSAGE_CHARS {
            return Err(format!(
                "ui-set-composer-text: text exceeds {MAX_MESSAGE_CHARS} characters"
            ));
        }
        self.composer_edit = Some(text);
        Ok(())
    }

    /// `ui-show-dialog`: a modal the host renders over the UI; the user
    /// dismisses it and an action click is delivered through `on-dialog-action`.
    /// Needs `ui.dialog`; the sizes are bounded (blocks share the view limits).
    fn ui_show_dialog(
        &mut self,
        title: String,
        blocks: Vec<(String, String)>,
        actions: Vec<(String, String)>,
    ) -> Result<(), String> {
        if !self.grants.has(Capability::UiDialog) {
            return Err(denied(Capability::UiDialog));
        }
        if title.chars().count() > MAX_DIALOG_TITLE_CHARS {
            return Err(format!(
                "ui-show-dialog: title exceeds {MAX_DIALOG_TITLE_CHARS} characters"
            ));
        }
        // Post-sanitize, so an all-invisible title is refused (it would render as
        // nothing).
        if sanitize(&title).is_empty() {
            return Err("ui-show-dialog: title is empty".to_string());
        }
        if blocks.len() > MAX_DIALOG_BLOCKS {
            return Err("ui-show-dialog: too many blocks".to_string());
        }
        let mut out_blocks: Vec<(String, String)> = Vec::with_capacity(blocks.len());
        let mut bytes = 0usize;
        for (kind, text) in blocks {
            if !matches!(kind.as_str(), "section" | "context" | "divider") {
                return Err("ui-show-dialog: unknown block kind".to_string());
            }
            // A divider carries no text.
            let text = if kind == "divider" {
                String::new()
            } else {
                text
            };
            if text.len() > MAX_DIALOG_BLOCK_BYTES {
                return Err("ui-show-dialog: block too long".to_string());
            }
            let text = sanitize_multiline(&text);
            bytes += text.len();
            out_blocks.push((kind, text));
        }
        if bytes > MAX_DIALOG_BYTES {
            return Err("ui-show-dialog: content quota exceeded".to_string());
        }
        if actions.len() > MAX_DIALOG_ACTIONS {
            return Err("ui-show-dialog: too many actions".to_string());
        }
        let mut out_actions: Vec<(String, String)> = Vec::with_capacity(actions.len());
        for (id, label) in actions {
            if !valid_action_id(&id) {
                return Err("ui-show-dialog: invalid action id".to_string());
            }
            if out_actions.iter().any(|(i, _)| i == &id) {
                return Err("ui-show-dialog: duplicate action id".to_string());
            }
            // Post-sanitize, so a label of only invisible characters (which would
            // render as a blank button) is refused.
            if sanitize(&label).is_empty() || label.len() > MAX_ACTION_LABEL_BYTES {
                return Err("ui-show-dialog: action label is empty or too long".to_string());
            }
            out_actions.push((id, label));
        }
        // Remember what was offered, so `on-dialog-action` can be validated.
        self.dialog_actions = out_actions.iter().map(|(id, _)| id.clone()).collect();
        self.pending_dialog = Some(DialogEffect::Show(Dialog {
            title,
            blocks: out_blocks,
            actions: out_actions,
        }));
        Ok(())
    }

    /// `ui-close-dialog`: close the mod's dialog early. Needs `ui.dialog`; a
    /// no-op (still `Ok`) when the mod has none open.
    fn ui_close_dialog(&mut self) -> Result<(), String> {
        if !self.grants.has(Capability::UiDialog) {
            return Err(denied(Capability::UiDialog));
        }
        self.dialog_actions.clear();
        self.pending_dialog = Some(DialogEffect::Close);
        Ok(())
    }

    /// `file-get`: the bytes of a file shared in a message the mod may read (the
    /// slice-1 form of `files.download`). Needs `files.download`; the id must be
    /// one the host delivered (an opaque `message.files[].id`), the file's channel
    /// must be in the mod's read scope, it must resolve to a Slack-hosted URL, and
    /// the body is size-capped.
    fn file_get(&mut self, id: String) -> Result<Vec<u8>, String> {
        if !self.grants.has(Capability::FilesDownload) {
            return Err(denied(Capability::FilesDownload));
        }
        if id.len() > MAX_FILE_ID_BYTES {
            return Err("file-get: file id too long".to_string());
        }
        // Only an id the host itself delivered, so a mod cannot ask for a file it
        // was never shown.
        let file = self
            .directory
            .as_ref()
            .and_then(|d| d.files.get(&id).cloned())
            .ok_or_else(|| "file-get: unknown file".to_string())?;
        // The map is the on-screen window (which may hold other conversations):
        // serve only a file whose channel is inside the mod's read scope.
        if !self.grants.can_read(&file.channel) {
            return Err(denied(Capability::MessagesRead));
        }
        // The URL is the one Slack served for a delivered message; anything else
        // is refused (the mod never supplies a URL, only an opaque id).
        if !crate::net::is_slack_host(&file.url) {
            return Err("file-get: refusing a non-Slack file host".to_string());
        }
        let api = self
            .api
            .clone()
            .ok_or_else(|| "file-get: no API client".to_string())?;
        // The download is async; run it on the runtime and block this dispatch
        // thread until it finishes (bounded by the client timeout).
        let handle = self.io_rt("file-get")?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(1);
        let url = file.url;
        let task = handle.spawn(async move {
            let _ = tx.send(
                api.download_bytes(&url, MAX_FILE_BYTES)
                    .await
                    .map_err(|e| sanitize_log(&format!("{e}"))),
            );
        });
        await_io(rx, task, &self.kill, "file-get")
    }

    /// `file-upload`: upload a file **as the user** to a channel inside the mod's
    /// `messages.read` scope (the slice-1 form of `files.upload`; ambient — the
    /// capability grant is the authorization, no activation token). Returns the new
    /// file's id. Bounded, and it shares the per-dispatch write budget.
    fn file_upload(
        &mut self,
        channel: String,
        name: String,
        bytes: Vec<u8>,
    ) -> Result<String, String> {
        if !self.grants.has(Capability::FilesUpload) {
            return Err(denied(Capability::FilesUpload));
        }
        // The read scope bounds where a mod may publish (same rule as `chat-send`).
        if !self.grants.can_read(&channel) {
            return Err(denied(Capability::MessagesRead));
        }
        if channel.len() > MAX_REF_BYTES {
            return Err("file-upload: channel reference too long".to_string());
        }
        // The name travels as a filename and as Slack's `title`: strip control and
        // bidi/invisible characters, and refuse path separators.
        let name = sanitize(name.trim());
        if name.is_empty() || name.len() > MAX_FILENAME_BYTES {
            return Err("file-upload: bad filename".to_string());
        }
        if name.contains(['/', '\\']) {
            return Err("file-upload: filename has an illegal character".to_string());
        }
        if bytes.is_empty() {
            return Err("file-upload: empty file".to_string());
        }
        if bytes.len() > MAX_UPLOAD_BYTES {
            return Err(format!(
                "file-upload: file exceeds {MAX_UPLOAD_BYTES} bytes"
            ));
        }
        // Uploads share the per-dispatch write budget with posts/reactions (each is
        // a blocking network round trip on the dispatch thread).
        if self.api_writes >= MAX_WRITES_PER_DISPATCH {
            return Err(format!(
                "too many writes (>{MAX_WRITES_PER_DISPATCH} per dispatch)"
            ));
        }
        let api = self
            .api
            .clone()
            .ok_or_else(|| "file-upload: no API client installed".to_string())?;
        self.api_writes += 1;
        // The call is async: run it on the runtime and block this dispatch thread
        // until it answers (as `chat-send`/`file-get` do).
        let handle = self.io_rt("file-upload")?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<String, String>>(1);
        let task = handle.spawn(async move {
            let _ = tx.send(
                api.upload_file(&channel, &name, bytes, None, None)
                    .await
                    .map_err(|e| sanitize_log(&format!("{e}"))),
            );
        });
        await_io(rx, task, &self.kill, "file-upload")
    }

    /// `fs-list`: list a slot's directory (one level, or a bounded recursive
    /// walk). Needs `filesystem` and a `read` slot; the walk, the entry cap and
    /// the containment are enforced by `mods::fs`.
    fn fs_list(
        &mut self,
        slot: String,
        path: String,
        recursive: bool,
        max_entries: u32,
    ) -> Result<Vec<host::FsEntry>, String> {
        if !self.grants.has(Capability::Filesystem) {
            return Err(denied(Capability::Filesystem));
        }
        let sandbox = self
            .fs
            .as_ref()
            .ok_or_else(|| "filesystem not configured".to_string())?;
        let cap = (max_entries as u64).min(fs::MAX_FS_ENTRIES);
        sandbox
            .list(&slot, &path, recursive, cap)
            .map(|v| v.into_iter().map(to_wit_fs_entry).collect())
            .map_err(|e| sanitize_log(&format!("{e:#}")))
    }

    /// `fs-read`: the bytes of a regular file in a slot. Needs `filesystem` and a
    /// `read` slot; the file is size-capped (refused, never truncated).
    fn fs_read(&mut self, slot: String, path: String) -> Result<Vec<u8>, String> {
        if !self.grants.has(Capability::Filesystem) {
            return Err(denied(Capability::Filesystem));
        }
        let sandbox = self
            .fs
            .as_ref()
            .ok_or_else(|| "filesystem not configured".to_string())?;
        sandbox
            .read(&slot, &path)
            .map_err(|e| sanitize_log(&format!("{e:#}")))
    }

    /// `fs-write`: create a **new** file in a slot. Needs `filesystem` and a
    /// `write` slot; an existing file is refused (no overwrite), the parent must
    /// exist, and the bytes are capped.
    fn fs_write(&mut self, slot: String, path: String, bytes: Vec<u8>) -> Result<(), String> {
        if !self.grants.has(Capability::Filesystem) {
            return Err(denied(Capability::Filesystem));
        }
        let sandbox = self
            .fs
            .as_ref()
            .ok_or_else(|| "filesystem not configured".to_string())?;
        sandbox
            .write(&slot, &path, &bytes)
            .map_err(|e| sanitize_log(&format!("{e:#}")))
    }

    /// `fs-search`: a case-insensitive literal substring search over a slot's
    /// files. Needs `filesystem` and a `read` slot; the scan and the results are
    /// bounded by `mods::fs`.
    fn fs_search(
        &mut self,
        slot: String,
        path: String,
        pattern: String,
        max_hits: u32,
    ) -> Result<Vec<host::FsHit>, String> {
        if !self.grants.has(Capability::Filesystem) {
            return Err(denied(Capability::Filesystem));
        }
        let sandbox = self
            .fs
            .as_ref()
            .ok_or_else(|| "filesystem not configured".to_string())?;
        let cap = (max_hits as u64).min(fs::MAX_FS_HITS);
        sandbox
            .search(&slot, &path, &pattern, cap)
            .map(|v| v.into_iter().map(to_wit_fs_hit).collect())
            .map_err(|e| sanitize_log(&format!("{e:#}")))
    }

    /// `vec-upsert`: store vectors in the mod's private DB. Needs `store.vector`;
    /// each vector must have the declared `dim`; `maxVectors`/`maxBytes` are
    /// enforced by the store.
    fn vec_upsert(&mut self, items: Vec<(String, Vec<f32>, Option<String>)>) -> Result<(), String> {
        if !self.grants.has(Capability::StoreVector) {
            return Err(denied(Capability::StoreVector));
        }
        if items.len() > MAX_VEC_UPSERT_ITEMS {
            return Err(format!(
                "vec-upsert: too many items (>{MAX_VEC_UPSERT_ITEMS})"
            ));
        }
        let store = self
            .store
            .as_mut()
            .filter(|s| s.has_vector())
            .ok_or_else(|| "store.vector not configured".to_string())?;
        store
            .upsert(&items)
            .map_err(|e| sanitize_log(&format!("{e}")))
    }

    /// `vec-search`: the `k` nearest rows (cosine), as `(key, score, meta)`.
    /// Needs `store.vector`; the query must have the declared `dim`.
    fn vec_search(
        &mut self,
        vector: Vec<f32>,
        k: u32,
        prefix: Option<String>,
    ) -> Result<Vec<(String, f32, Option<String>)>, String> {
        if !self.grants.has(Capability::StoreVector) {
            return Err(denied(Capability::StoreVector));
        }
        let k = (k as usize).min(MAX_SEARCH_K);
        let store = self
            .store
            .as_mut()
            .filter(|s| s.has_vector())
            .ok_or_else(|| "store.vector not configured".to_string())?;
        store
            .search(&vector, k, prefix.as_deref())
            .map_err(|e| sanitize_log(&format!("{e}")))
    }

    /// `vec-delete`: remove vectors by key. Needs `store.vector`.
    fn vec_delete(&mut self, keys: Vec<String>) -> Result<u32, String> {
        if !self.grants.has(Capability::StoreVector) {
            return Err(denied(Capability::StoreVector));
        }
        if keys.len() > MAX_VEC_UPSERT_ITEMS {
            return Err(format!("vec-delete: too many keys (>{MAX_VEC_UPSERT_ITEMS})"));
        }
        let store = self
            .store
            .as_mut()
            .filter(|s| s.has_vector())
            .ok_or_else(|| "store.vector not configured".to_string())?;
        store
            .delete(&keys)
            .map_err(|e| sanitize_log(&format!("{e}")))
    }

    /// `sql-query`: run one statement against the mod's private `scratch.db`
    /// (`store.sql`). Needs `store.sql`; the hardening is in `mods::sql`.
    fn sql_query(
        &mut self,
        query: String,
        params: Vec<WitSqlValue>,
    ) -> Result<WitSqlResult, String> {
        if !self.grants.has(Capability::StoreSql) {
            return Err(denied(Capability::StoreSql));
        }
        let sql = self
            .sql
            .as_mut()
            .ok_or_else(|| "store.sql not configured".to_string())?;
        let params: Vec<sql::SqlValue> = params.into_iter().map(from_wit_sql).collect();
        sql.query(&query, &params)
            .map(to_wit_sql_result)
            .map_err(|e| sanitize_log(&format!("{e:#}")))
    }

    /// `sql-tx`: run several statements in one transaction against the same DB,
    /// one result each, in order. Needs `store.sql`.
    fn sql_tx(
        &mut self,
        statements: Vec<(String, Vec<WitSqlValue>)>,
    ) -> Result<Vec<WitSqlResult>, String> {
        if !self.grants.has(Capability::StoreSql) {
            return Err(denied(Capability::StoreSql));
        }
        let sql = self
            .sql
            .as_mut()
            .ok_or_else(|| "store.sql not configured".to_string())?;
        let statements: Vec<(String, Vec<sql::SqlValue>)> = statements
            .into_iter()
            .map(|(q, ps)| (q, ps.into_iter().map(from_wit_sql).collect()))
            .collect();
        sql.tx(&statements)
            .map(|out| out.into_iter().map(to_wit_sql_result).collect())
            .map_err(|e| sanitize_log(&format!("{e:#}")))
    }

    /// `notify-post`: raise a desktop notification attributed to the mod. Needs
    /// `notify`; a no-op when the user has notifications off. The text is
    /// sanitized and length-capped, and one callback may raise only a few.
    fn notify_post(&mut self, title: String, body: String) -> Result<(), String> {
        if !self.grants.has(Capability::Notify) {
            return Err(denied(Capability::Notify));
        }
        if self.notify_count >= MAX_NOTIFY_PER_DISPATCH {
            return Err(format!(
                "too many notifications (>{MAX_NOTIFY_PER_DISPATCH} per dispatch)"
            ));
        }
        let title = sanitize(&title);
        let body = sanitize(&body);
        if title.chars().count() > MAX_NOTIFY_TITLE {
            return Err(format!(
                "notify-post: title exceeds {MAX_NOTIFY_TITLE} characters"
            ));
        }
        if body.chars().count() > MAX_NOTIFY_BODY {
            return Err(format!(
                "notify-post: body exceeds {MAX_NOTIFY_BODY} characters"
            ));
        }
        self.notify_count += 1;
        // Respect the user's switch: the notification is a plain side effect and
        // there is nothing for the mod to observe.
        if !crate::notify::enabled() {
            return Ok(());
        }
        let heading = notify_heading(&self.notify_label, &title);
        crate::notify::show_detached(crate::notify::DesktopNotification {
            title: Some("slaq".to_string()),
            subtitle: Some(heading),
            content: Some(body),
            ..Default::default()
        });
        Ok(())
    }

    /// Add one of the user's reactions to a message the mod may read. Needs
    /// `reactions.write`; the message must be inside the read scope.
    fn reactions_add(&mut self, channel: String, ts: String, name: String) -> Result<(), String> {
        self.reactions(channel, ts, name, true)
    }

    /// Remove one of the user's reactions (`reactions.remove`).
    fn reactions_remove(
        &mut self,
        channel: String,
        ts: String,
        name: String,
    ) -> Result<(), String> {
        self.reactions(channel, ts, name, false)
    }

    /// `chat-send`: post a message **as the user** to a channel inside the mod's
    /// read scope. Needs `chat.send` **and** `messages.read`; the scope bounds
    /// where it may post. Returns the created `(channel, ts)`.
    fn chat_send(
        &mut self,
        channel: String,
        text: String,
        thread: Option<String>,
    ) -> Result<(String, String), String> {
        if !self.grants.has(Capability::ChatSend) {
            return Err(denied(Capability::ChatSend));
        }
        // The read scope bounds where a mod may post; posting is not a separate
        // scope.
        if !self.grants.can_read(&channel) {
            return Err(denied(Capability::MessagesRead));
        }
        if channel.len() > MAX_REF_BYTES {
            return Err("chat-send: channel reference too long".to_string());
        }
        if let Some(t) = &thread {
            if t.len() > MAX_REF_BYTES {
                return Err("chat-send: thread reference too long".to_string());
            }
        }
        if text.is_empty() {
            return Err("chat-send: empty message".to_string());
        }
        if text.len() > MAX_MESSAGE_BYTES || text.chars().count() > MAX_MESSAGE_CHARS {
            return Err(format!(
                "chat-send: message exceeds {MAX_MESSAGE_CHARS} characters"
            ));
        }
        // Posts share the per-dispatch API-write budget with `reactions` (each is a
        // blocking network round trip on the dispatch thread).
        if self.api_writes >= MAX_WRITES_PER_DISPATCH {
            return Err(format!(
                "too many writes (>{MAX_WRITES_PER_DISPATCH} per dispatch)"
            ));
        }
        let api = self
            .api
            .clone()
            .ok_or_else(|| "chat-send: no API client installed".to_string())?;
        self.api_writes += 1;
        // The call is async: run it on the runtime and block this dispatch thread
        // until it answers (as `http.fetch`/`reactions` do).
        let handle = self.io_rt("chat-send")?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(String, String), String>>(1);
        let task = handle.spawn(async move {
            let res = match &thread {
                Some(t) => api.chat_post_message_thread(&channel, &text, t).await,
                None => api.chat_post_message(&channel, &text).await,
            };
            let _ = tx.send(
                res.map(|r| (r.channel, r.ts))
                    .map_err(|e| sanitize_log(&e.to_string())),
            );
        });
        await_io(rx, task, &self.kill, "chat-send")
    }

    /// `chat-forward`: forward the message at `(from_channel, ts)` into
    /// `to_channel` **as the user** (the slice-1 form of `chat.forward`; ambient —
    /// the capability grant is the authorization). A link to the source is posted
    /// in the target; both channels must be inside the mod's `messages.read` scope
    /// (it may only forward what it may read, and only where it may post). Returns
    /// the new `(channel, ts)`.
    fn chat_forward(
        &mut self,
        from_channel: String,
        ts: String,
        to_channel: String,
    ) -> Result<(String, String), String> {
        if !self.grants.has(Capability::ChatForward) {
            return Err(denied(Capability::ChatForward));
        }
        // The read scope bounds both the source and the destination.
        if !self.grants.can_read(&from_channel) || !self.grants.can_read(&to_channel) {
            return Err(denied(Capability::MessagesRead));
        }
        if from_channel.len() > MAX_REF_BYTES
            || to_channel.len() > MAX_REF_BYTES
            || ts.len() > MAX_REF_BYTES
        {
            return Err("chat-forward: reference too long".to_string());
        }
        // Only a message the mod actually **received** may be forwarded (the index
        // is the current window), so it cannot forward arbitrary history — the same
        // rule as `reactions`/`decorate`.
        if !self
            .messages
            .contains_key(&(from_channel.clone(), ts.clone()))
        {
            return Err("chat-forward: unknown or out-of-scope message".to_string());
        }
        // A forward is two round trips (permalink + post): charge both against the
        // shared write budget.
        if self.api_writes + 2 > MAX_WRITES_PER_DISPATCH {
            return Err(format!(
                "too many writes (>{MAX_WRITES_PER_DISPATCH} per dispatch)"
            ));
        }
        let api = self
            .api
            .clone()
            .ok_or_else(|| "chat-forward: no API client installed".to_string())?;
        self.api_writes += 2;
        // The calls are async: run them on the runtime and block this dispatch
        // thread until they answer (as `chat-send` does).
        let handle = self.io_rt("chat-forward")?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(String, String), String>>(1);
        let task = handle.spawn(async move {
            let res = async {
                let link = api.chat_get_permalink(&from_channel, &ts).await?;
                let sent = api.chat_post_message(&to_channel, &link).await?;
                Ok::<_, anyhow::Error>((sent.channel, sent.ts))
            }
            .await;
            let _ = tx.send(res.map_err(|e| sanitize_log(&format!("{e}"))));
        });
        await_io(rx, task, &self.kill, "chat-forward")
    }
}

impl HostState {
    /// Shared body of `reactions-add`/`-remove`: validate, then perform the API
    /// call on the runtime and block this dispatch thread until it answers (as
    /// `http.fetch` does).
    fn reactions(
        &mut self,
        channel: String,
        ts: String,
        name: String,
        add: bool,
    ) -> Result<(), String> {
        if !self.grants.has(Capability::ReactionsWrite) {
            return Err(denied(Capability::ReactionsWrite));
        }
        // Scope is enforced per channel (`get-message`/`invoke-action` do the
        // same): the index alone is not the scope, since it holds every
        // dispatched channel's messages.
        if !self.grants.can_read(&channel) {
            return Err(denied(Capability::MessagesRead));
        }
        if channel.len() > MAX_REF_BYTES || ts.len() > MAX_REF_BYTES {
            return Err("reactions: reference too long".to_string());
        }
        if !valid_reaction_name(&name) {
            return Err("reactions: invalid emoji name".to_string());
        }
        // Only a message the mod can read may be reacted to (the index is the
        // current window), so a mod cannot react to arbitrary history.
        if !self.messages.contains_key(&(channel.clone(), ts.clone())) {
            return Err("reactions: unknown or out-of-scope message".to_string());
        }
        if self.api_writes >= MAX_WRITES_PER_DISPATCH {
            return Err(format!(
                "too many writes (>{MAX_WRITES_PER_DISPATCH} per dispatch)"
            ));
        }
        let api = self
            .api
            .clone()
            .ok_or_else(|| "reactions: no API client installed".to_string())?;
        self.api_writes += 1;
        let handle = self.io_rt("reactions")?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
        let task = handle.spawn(async move {
            let res = if add {
                api.reactions_add(&channel, &ts, &name).await
            } else {
                api.reactions_remove(&channel, &ts, &name).await
            };
            let _ = tx.send(res.map_err(|e| sanitize_log(&e.to_string())));
        });
        await_io(rx, task, &self.kill, "reactions")
    }
}

/// The heading a `notify-post` shows: the mod's id, then the mod's own title, so
/// the user always sees *who* posted it (the id is unique and unforgeable; the
/// title is free text). The label is capped so a long id cannot crowd it out.
fn notify_heading(label: &str, title: &str) -> String {
    let label: String = label.chars().take(MAX_NOTIFY_LABEL).collect();
    if label.is_empty() {
        title.to_string()
    } else {
        format!("{label}: {title}")
    }
}

/// Truncates an untrusted directory string to a small byte budget (char-boundary
/// safe), so the boundary copy is bounded by bytes, not only by entry count.
fn bounded_name(s: &str) -> String {
    if s.len() <= MAX_DIRECTORY_NAME_BYTES {
        return s.to_string();
    }
    let mut end = MAX_DIRECTORY_NAME_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// A resolved setting as the guest sees it.
fn to_wit_value(v: &settings::Value) -> SettingValue {
    match v {
        settings::Value::Text(s) => SettingValue::Text(s.clone()),
        settings::Value::Integer(n) => SettingValue::Integer(*n),
        settings::Value::Real(f) => SettingValue::Real(*f),
        settings::Value::Flag(b) => SettingValue::Flag(*b),
    }
}

/// A guest-supplied setting value.
fn from_wit_value(v: SettingValue) -> settings::Value {
    match v {
        SettingValue::Text(s) => settings::Value::Text(s),
        SettingValue::Integer(n) => settings::Value::Integer(n),
        SettingValue::Real(f) => settings::Value::Real(f),
        SettingValue::Flag(b) => settings::Value::Flag(b),
    }
}

/// A `sql-value` from the guest, in the store's own type.
fn from_wit_sql(v: WitSqlValue) -> sql::SqlValue {
    match v {
        WitSqlValue::Null => sql::SqlValue::Null,
        WitSqlValue::Int(i) => sql::SqlValue::Int(i),
        WitSqlValue::Real(f) => sql::SqlValue::Real(f),
        WitSqlValue::Text(s) => sql::SqlValue::Text(s),
        WitSqlValue::Blob(b) => sql::SqlValue::Blob(b),
    }
}

/// A `sql-value` for the guest.
fn to_wit_sql(v: sql::SqlValue) -> WitSqlValue {
    match v {
        sql::SqlValue::Null => WitSqlValue::Null,
        sql::SqlValue::Int(i) => WitSqlValue::Int(i),
        sql::SqlValue::Real(f) => WitSqlValue::Real(f),
        sql::SqlValue::Text(s) => WitSqlValue::Text(s),
        sql::SqlValue::Blob(b) => WitSqlValue::Blob(b),
    }
}

/// A `sql-result` for the guest.
fn to_wit_sql_result(r: sql::SqlResult) -> WitSqlResult {
    WitSqlResult {
        columns: r.columns,
        rows: r
            .rows
            .into_iter()
            .map(|row| row.into_iter().map(to_wit_sql).collect())
            .collect(),
        changes: r.changes,
    }
}

/// A `fs-entry` for the guest (`kind` is `"file"` or `"dir"`).
fn to_wit_fs_entry(e: fs::FsEntry) -> host::FsEntry {
    host::FsEntry {
        path: e.path,
        kind: match e.kind {
            fs::FsKind::File => "file".to_string(),
            fs::FsKind::Dir => "dir".to_string(),
        },
        size: e.size,
    }
}

/// A `fs-hit` for the guest.
fn to_wit_fs_hit(h: fs::FsHit) -> host::FsHit {
    host::FsHit {
        path: h.path,
        line: h.line,
        text: h.text,
    }
}

/// The `net.egress` allow-list entry for `host` matches either `host:port`
/// exactly or a bare `host` (any port).
fn egress_entry_matches(entry: &str, host: &str, port: u16) -> bool {
    match entry.rsplit_once(':') {
        Some((h, p)) => h == host && p.parse::<u16>().ok() == Some(port),
        None => entry == host,
    }
}

/// Whether `ip` is a globally routable unicast address: loopback, private,
/// link-local, CGNAT, documentation, multicast and friends are **not**. An
/// IPv4-mapped (or the deprecated IPv4-compatible) IPv6 is classified by its
/// embedded IPv4, so `::ffff:127.0.0.1` cannot slip through.
fn addr_is_public(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4_is_public(v4),
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_is_public(v4);
            }
            let s = v6.segments();
            // Deprecated IPv4-compatible `::/96` (excluding `::` and `::1`).
            if s[..6] == [0, 0, 0, 0, 0, 0] && (s[6] != 0 || s[7] > 1) {
                let v4 = std::net::Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    (s[6] & 0xff) as u8,
                    (s[7] >> 8) as u8,
                    (s[7] & 0xff) as u8,
                );
                return v4_is_public(v4);
            }
            // Global unicast is `2000::/3`; everything else (loopback, ULA,
            // link-local, multicast, NAT64 `64:ff9b::/96`, …) is not routable.
            if (s[0] & 0xe000) != 0x2000 {
                return false;
            }
            // …minus the special-purpose sub-blocks inside `2000::/3`.
            !(s[0] == 0x2002 // 6to4 2002::/16
                || (s[0] == 0x2001 && s[1] == 0x0000) // Teredo 2001::/32
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation 2001:db8::/32
                || (s[0] == 0x2001 && (s[1] & 0xfff0) == 0x0010) // ORCHIDv1 2001:10::/28
                || (s[0] == 0x2001 && (s[1] & 0xfff0) == 0x0020) // ORCHIDv2 2001:20::/28
                || (s[0] == 0x2001 && s[1] == 0x0002 && s[2] == 0x0000) // benchmarking 2001:2::/48
                || (s[0] & 0xfff0) == 0x3ff0) // documentation 3fff::/20
        }
    }
}

/// Whether an IPv4 address is globally routable.
fn v4_is_public(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    !(v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || o[0] == 0 // 0.0.0.0/8
        || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 100.64/10
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // 192.88.99.0/24
        || o[0] >= 240 // 240.0.0.0/4 (incl. broadcast)
        || (o[0] == 198 && (18..=19).contains(&o[1]))) // benchmarking 198.18/15
}

/// Resolves `host:port` and returns one address to pin to. A listed *name* must
/// resolve to a public address (otherwise it is refused — DNS rebinding); an IP
/// literal or `localhost` may be private, since the user listed it explicitly.
fn resolve_vetted(host: &str, port: u16, literal_ok: bool) -> Result<std::net::SocketAddr, String> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| "http-fetch: cannot resolve host".to_string())?
        .collect();
    addrs
        .iter()
        .find(|a| literal_ok || addr_is_public(a.ip()))
        .copied()
        .ok_or_else(|| "http-fetch: host resolves to a non-public address (refused)".to_string())
}

/// [`resolve_vetted`] run on the runtime under [`DNS_TIMEOUT`], so a slow
/// resolver cannot hold the dispatch thread without bound.
fn resolve_vetted_bounded(
    host: &str,
    port: u16,
    literal_ok: bool,
    rt: &tokio::runtime::Handle,
    kill: &std::sync::atomic::AtomicBool,
) -> Result<std::net::SocketAddr, String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let host = host.to_string();
    rt.spawn_blocking(move || {
        let _ = tx.send(resolve_vetted(&host, port, literal_ok));
    });
    // A blocking resolver cannot be aborted, but a stopped mod need not wait on it.
    let deadline = std::time::Instant::now() + DNS_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err("http-fetch: DNS resolution timed out".to_string());
        }
        match rx.recv_timeout(left.min(IO_CANCEL_POLL)) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if kill.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err("http-fetch: cancelled (mod stopped)".to_string());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("http-fetch: DNS resolution failed".to_string());
            }
        }
    }
}

/// How often a host call blocked on I/O re-checks its mod's `kill` flag.
const IO_CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Wait for the result of a host request running as `task` on the runtime,
/// giving up promptly — and aborting the task — once the mod is being stopped
/// (its `kill` flag: Drop on reload/exit, or the control panel). The epoch
/// kill-switch only fires while guest code runs, so without this a stop would
/// land only after the request's own timeout (up to tens of seconds).
fn await_io<T>(
    rx: std::sync::mpsc::Receiver<Result<T, String>>,
    task: tokio::task::JoinHandle<()>,
    kill: &std::sync::atomic::AtomicBool,
    what: &str,
) -> Result<T, String> {
    loop {
        match rx.recv_timeout(IO_CANCEL_POLL) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if kill.load(std::sync::atomic::Ordering::Relaxed) {
                    task.abort();
                    return Err(format!("{what}: cancelled (mod stopped)"));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(format!("{what}: request cancelled"));
            }
        }
    }
}

/// A Slack error for a history read, as the guest sees it: rate limiting is
/// reported with the same `rate-limited` prefix as the host's own pace.
fn history_error(e: &str) -> String {
    let e = sanitize_log(e);
    if e.contains("ratelimited") || e.contains("429") {
        format!("rate-limited: {e}")
    } else {
        e
    }
}

/// Raw `conversations.history`/`.replies` messages as the guest's `message`
/// records, at most `limit` and [`MAX_HISTORY_BYTES`] of text. A message with
/// no `ts` is skipped; the author is the user id (or the bot id); `files` stays
/// empty (`file-get` serves only files slaq has shown).
fn history_messages(raw: &[serde_json::Value], limit: usize) -> Vec<Message> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for v in raw.iter().take(limit) {
        let Some(ts) = v.get("ts").and_then(|t| t.as_str()) else {
            continue;
        };
        let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
        bytes += text.len();
        if bytes > MAX_HISTORY_BYTES {
            break;
        }
        let author = v
            .get("user")
            .or_else(|| v.get("bot_id"))
            .and_then(|a| a.as_str())
            .unwrap_or("")
            .to_string();
        out.push(Message {
            author,
            ts: ts.to_string(),
            text,
            thread_ts: v.get("thread_ts").and_then(|t| t.as_str()).map(str::to_string),
            reply_count: v
                .get("reply_count")
                .and_then(|n| n.as_u64())
                .map(|n| n.min(u32::MAX as u64) as u32)
                .unwrap_or(0),
            files: Vec::new(),
        });
    }
    out
}

/// Validates and performs one `http.fetch`, enforcing `net.egress`.
fn egress_fetch(
    http: &mut HashMap<String, reqwest::Client>,
    allowed: &[String],
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    rt: &tokio::runtime::Handle,
    kill: &std::sync::atomic::AtomicBool,
) -> Result<(u16, Vec<u8>), String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "http-fetch: invalid url".to_string())?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err("http-fetch: only http/https".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("http-fetch: userinfo is not allowed".to_string());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "http-fetch: url has no host".to_string())?
        .to_ascii_lowercase();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "http-fetch: url has no port".to_string())?;

    if !allowed.iter().any(|e| egress_entry_matches(e, &host, port)) {
        return Err(format!("permission-denied: net.egress ({host}:{port})"));
    }
    let literal_ok = allowed.iter().any(|e| {
        let h = e.rsplit_once(':').map(|(h, _)| h).unwrap_or(e.as_str());
        h == host && (host == "localhost" || host.parse::<std::net::IpAddr>().is_ok())
    });
    let addr = resolve_vetted_bounded(&host, port, literal_ok, rt, kill)?;

    if headers.len() > MAX_HTTP_HEADERS {
        return Err(format!(
            "http-fetch: too many headers (>{MAX_HTTP_HEADERS})"
        ));
    }
    for (k, v) in headers {
        if k.len() > MAX_HTTP_HEADER_BYTES || v.len() > MAX_HTTP_HEADER_BYTES {
            return Err("http-fetch: header too long".to_string());
        }
        // No header injection, and no overriding the authority the allow-list
        // pinned: a spoofed `Host` would select a different virtual host.
        if k.contains(['\r', '\n', '\0']) || v.contains(['\r', '\n', '\0']) {
            return Err("http-fetch: invalid header".to_string());
        }
        if k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("content-length") {
            return Err("http-fetch: managed header must not be set".to_string());
        }
    }
    if let Some(b) = body {
        if b.len() > MAX_HTTP_REQUEST_BYTES {
            return Err(format!(
                "http-fetch: request body exceeds {MAX_HTTP_REQUEST_BYTES} bytes"
            ));
        }
    }
    let method = reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
        .map_err(|_| "http-fetch: invalid method".to_string())?;

    // One client per target, pinned to the vetted address (no re-resolution).
    let key = format!("{host}:{port}");
    let client = if let Some(existing) = http.get(&key) {
        existing.clone()
    } else {
        let built = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // A system proxy would move DNS/connect off this host, bypassing the
            // pin below; the egress boundary does not use one.
            .no_proxy()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_TIMEOUT)
            .resolve(&host, addr)
            .build()
            .expect("mods: building the http.fetch client");
        // Bound the cache; beyond it, a transient client is used.
        if http.len() < MAX_HTTP_CLIENTS {
            http.insert(key, built.clone());
        }
        built
    };

    // The request is async; run it on the runtime and block this dispatch thread
    // until it finishes (bounded by the client timeout).
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(u16, Vec<u8>), String>>(1);
    let url = url.to_string();
    let headers = headers.to_vec();
    let body = body.map(|b| b.to_vec());
    let task = rt.spawn(async move {
        let _ = tx.send(do_fetch(&client, method, url, headers, body).await);
    });
    await_io(rx, task, kill, "http-fetch")
}

/// The async half of `http.fetch`: bounded, no redirects.
async fn do_fetch(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
) -> Result<(u16, Vec<u8>), String> {
    let mut req = client.request(method, &url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| format!("http-fetch: {}", sanitize_log(&e.to_string())))?;
    let status = resp.status().as_u16();
    // Read with a hard cap so a peer cannot make the host buffer unbounded.
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("http-fetch: {}", sanitize_log(&e.to_string())))?
    {
        if buf.len() + chunk.len() > MAX_HTTP_RESPONSE_BYTES {
            return Err(format!(
                "http-fetch: response exceeds {MAX_HTTP_RESPONSE_BYTES} bytes"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((status, buf))
}

/// Strips control characters from untrusted text before it is printed: a
/// package or a guest must not be able to inject terminal escape sequences.
/// Whether `c` must never reach a UI, log or notification surface: control
/// characters, format characters (bidi overrides, zero-width marks) and
/// line/paragraph separators. They carry no meaning and can reorder or forge the
/// text around them.
fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}'                // soft hyphen
            | '\u{061C}'              // arabic letter mark
            | '\u{180E}'              // mongolian vowel separator
            | '\u{200B}'              // zero-width space
            | '\u{200E}' | '\u{200F}' // left/right-to-left mark
            | '\u{2028}' | '\u{2029}' // line / paragraph separator
            | '\u{202A}'..='\u{202E}' // bidi embedding and override
            | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{206A}'..='\u{206F}' // deprecated format characters
            | '\u{FEFF}'              // zero-width no-break space
            | '\u{1D173}'..='\u{1D17A}' // musical symbol format
            | '\u{E0001}'             // language tag
            | '\u{E0020}'..='\u{E007F}' // tag characters (invisible ASCII)
        )
}

/// Strips the characters in [`is_invisible`] from untrusted single-line text
/// before it is shown (mod names, labels, titles, notifications, logs).
pub(crate) fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !is_invisible(*c)).collect()
}

/// [`sanitize`] for text a mod means to show on several lines — decorations,
/// view and dialog blocks: line breaks and tabs stay (a `\r\n` becomes `\n`),
/// every other invisible character goes, so bidi overrides and zero-width
/// characters cannot make a mod's text read differently from what it is.
pub(crate) fn sanitize_multiline(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !is_invisible(*c))
        .collect()
}

/// `major.minor.patch` of a semver string (a pre-release or build suffix is
/// ignored, so `1.2.3-rc.1` counts as `1.2.3`). `None` when it is not one.
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// A manifest `id` must match the schema's reverse-DNS pattern: at least one
/// dot; lowercase alphanumeric labels that start and end alphanumeric. It keys
/// the consent record, so a malformed or oversized id is refused at load.
pub(crate) fn valid_mod_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return false;
    }
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut labels = 0;
    for label in id.split('.') {
        labels += 1;
        let b = label.as_bytes();
        if b.is_empty() || !alnum(b[0]) || !alnum(b[b.len() - 1]) {
            return false;
        }
        if !b.iter().all(|&c| alnum(c) || c == b'-') {
            return false;
        }
    }
    labels >= 2
}

/// Truncate an untrusted diagnostic to a small byte budget before logging, so a
/// guest cannot flood memory or the logs with a near-memory-sized error string.
pub(crate) fn sanitize_log(s: &str) -> String {
    let mut end = s.len().min(MAX_LOG_BYTES);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    sanitize(&s[..end])
}

/// Hash of the callback inputs (author and text), to detect changes: author is
/// part of `on-message`, so a change in either re-runs the mods.
fn content_hash(author: &str, text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    author.hash(&mut h);
    text.hash(&mut h);
    h.finish()
}

/// Reads a file with a hard cap, so a package blob cannot exhaust host memory.
pub(crate) fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = Vec::new();
    file.take((max as u64).saturating_add(1))
        .read_to_end(&mut buf)?;
    if buf.len() > max {
        return Err(anyhow!("file too large (max {max} bytes)"));
    }
    Ok(buf)
}

/// Where installed mods live: `$SLAQ_MODS_DIR`, else `$XDG_CONFIG_HOME/slaq/mods`
/// (or `~/.config/slaq/mods`).
pub fn default_dir() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("SLAQ_MODS_DIR").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(v);
        return if p.is_absolute() {
            Ok(p)
        } else {
            Err(anyhow!("SLAQ_MODS_DIR must be an absolute path"))
        };
    }
    // An empty XDG value counts as unset: a relative `slaq/mods` from an
    // untrusted working directory must never be scanned for components.
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow!("no XDG_CONFIG_HOME or HOME to locate the mods dir"))?;
            PathBuf::from(home).join(".config")
        }
    };
    if !base.is_absolute() {
        return Err(anyhow!("config home is not absolute: {}", base.display()));
    }
    Ok(base.join("slaq").join("mods"))
}

/// The base directory for per-mod *data* (`~/.local/share/slaq/mods`), honoring
/// `SLAQ_MODS_DATA` (absolute) for tests/headless. A mod's store lives at
/// `<base>/<id>/store.db`; it is created lazily, on first use.
pub fn mod_data_base() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("SLAQ_MODS_DATA").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(v);
        return if p.is_absolute() {
            Ok(p)
        } else {
            Err(anyhow!("SLAQ_MODS_DATA must be an absolute path"))
        };
    }
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow!("no XDG_DATA_HOME or HOME to locate the mods data dir"))?;
            PathBuf::from(home).join(".local").join("share")
        }
    };
    if !base.is_absolute() {
        return Err(anyhow!("data home is not absolute: {}", base.display()));
    }
    Ok(base.join("slaq").join("mods"))
}

/// The engine configuration shared by the runtime engine and the compile worker,
/// so a precompiled artifact is always compatible and epoch instrumentation is
/// identical on both sides.
fn engine_config() -> wasmtime::Config {
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    // Epoch interruption is the wall-clock kill-switch: it lets the host abort a
    // wedged guest (the ticker advances the epoch; a store's deadline traps it).
    // It is not a compute budget — honest long work runs to completion. Host-side
    // work (lifting large canonical-ABI strings, hashing) is bounded instead by
    // the per-value/reference caps and the guest memory ceiling.
    config.epoch_interruption(true);
    config
}

/// A runtime engine with a real-time epoch ticker. Only an engine built here
/// bounds guest execution by wall clock; a bare `Engine::new` does not.
pub(crate) fn engine() -> Result<wasmtime::Engine> {
    let engine = wasmtime::Engine::new(&engine_config())?;
    // Advance the epoch from a real-time ticker, holding the engine *weakly* so the
    // thread exits (rather than pinning the engine) once the last engine is gone.
    let ticker = engine.weak();
    // Fail closed: without the ticker there is no wall-clock bound, so callbacks
    // must not run at all.
    std::thread::Builder::new()
        .name("slaq-mod-epoch".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(EPOCH_TICK_MS));
                match ticker.upgrade() {
                    Some(e) => e.increment_epoch(),
                    None => break,
                }
            }
        })
        .context("cannot start the mod epoch ticker")?;
    Ok(engine)
}

/// A bare engine (same config, no ticker) for the compile worker, which never runs
/// guest code.
fn worker_engine() -> Result<wasmtime::Engine> {
    Ok(wasmtime::Engine::new(&engine_config())?)
}

/// Runs in a child process: precompiles the component named by the env vars and
/// writes the artifact. Compiling untrusted input out of process lets the parent
/// bound the worker's CPU and memory with rlimits and a timeout.
pub fn compile_worker() -> Result<()> {
    let input =
        std::env::var_os(COMPILE_IN_ENV).ok_or_else(|| anyhow!("missing {COMPILE_IN_ENV}"))?;
    let output =
        std::env::var_os(COMPILE_OUT_ENV).ok_or_else(|| anyhow!("missing {COMPILE_OUT_ENV}"))?;
    // Re-read through the bounded reader: the child must not trust its own input.
    let bytes = read_bounded(Path::new(&input), MAX_COMPONENT_BYTES)?;
    let artifact = worker_engine()?
        .precompile_component(&bytes)
        .map_err(|e| anyhow!("precompiling: {}", sanitize_log(&e.to_string())))?;
    // Reject an oversized artifact *before* writing it, so a crafted component
    // cannot fill the disk with the precompiled output.
    if artifact.len() > MAX_ARTIFACT_BYTES {
        return Err(anyhow!(
            "precompiled artifact too large ({})",
            artifact.len()
        ));
    }
    std::fs::write(Path::new(&output), &artifact)
        .with_context(|| format!("writing {}", Path::new(&output).display()))?;
    Ok(())
}

/// Test build: `current_exe` is the test harness, which has no worker hook, so
/// compile in-process. The out-of-process boundary is a production property.
#[cfg(test)]
fn compile_component(
    engine: &wasmtime::Engine,
    bytes: &[u8],
) -> Result<wasmtime::component::Component> {
    wasmtime::component::Component::new(engine, bytes)
        .map_err(|e| anyhow!("compiling: {}", sanitize_log(&e.to_string())))
}

/// Compiles an untrusted component in a resource-limited child process and
/// deserializes the result. The parent never compiles untrusted input itself, so
/// a crafted component cannot hang or exhaust the client by compiling.
#[cfg(not(test))]
fn compile_component(
    engine: &wasmtime::Engine,
    bytes: &[u8],
) -> Result<wasmtime::component::Component> {
    // Fail closed where we cannot cap the worker's memory: on non-Unix the
    // timeout alone does not bound peak memory, so refuse rather than compile
    // untrusted input unbounded.
    #[cfg(not(unix))]
    {
        let _ = (engine, bytes);
        return Err(anyhow!(
            "mod compilation is disabled on this platform (no memory limit available)"
        ));
    }
    // A private, atomically-created directory whose name is not predictable: an
    // attacker cannot pre-create the files, and only the owner can read/write
    // them, so the bytes fed to `Component::deserialize` are ours.
    let nonce = {
        use std::hash::{BuildHasher, Hasher};
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    };
    let dir = std::env::temp_dir().join(format!("slaq-mod-{}-{nonce:016x}", std::process::id()));
    {
        // Create with mode 0700 atomically (no window with umask-derived
        // permissions), and fail if it already exists.
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let in_path = dir.join("in.wasm");
    let out_path = dir.join("out.cwasm");
    // RAII: the private dir (and its files) is removed on every exit path,
    // including the early `?` returns below.
    let _guard = TempDirGuard(dir.clone());
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&in_path).context("creating the compile input")?;
        file.write_all(bytes).context("writing the compile input")?;
    }

    let exe = std::env::current_exe().context("locating the slaq binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(WORKER_ARG)
        // Clear the environment first: a compiler compromised by the untrusted
        // component must not inherit the parent's secrets. The two paths below
        // are all the worker needs.
        .env_clear()
        .env(COMPILE_IN_ENV, &in_path)
        .env(COMPILE_OUT_ENV, &out_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Keep the dynamic-loader path (no secret) so a bundled/distro install can
    // still start the child; `LD_PRELOAD` and everything else stay cleared.
    #[cfg(unix)]
    if let Some(v) = std::env::var_os("LD_LIBRARY_PATH") {
        cmd.env("LD_LIBRARY_PATH", v);
    }
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        // Bound the worker's address space and CPU time. 4 GiB is comfortably
        // above what compiling an 8 MiB component needs (wasmtime reserves a
        // large *virtual* range) while still capping runaway heap growth. On
        // non-Unix only the wall-clock timeout applies.
        let mem = libc::rlimit {
            rlim_cur: 4 << 30,
            rlim_max: 4 << 30,
        };
        let cpu = libc::rlimit {
            rlim_cur: 30,
            rlim_max: 30,
        };
        cmd.pre_exec(move || {
            // Fail closed: if a limit cannot be applied, do not run the worker.
            if libc::setrlimit(libc::RLIMIT_AS, &mem) != 0
                || libc::setrlimit(libc::RLIMIT_CPU, &cpu) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().context("spawning the compile worker")?;
    let deadline = std::time::Instant::now() + COMPILE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!("component compilation timed out"));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => {
                // Never leave the worker running on a wait error.
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).context("waiting for the compile worker");
            }
        }
    };

    if !status.success() {
        return Err(anyhow!(
            "component compilation failed (worker exit {:?})",
            status.code()
        ));
    }
    let artifact = read_bounded(&out_path, MAX_ARTIFACT_BYTES)?;
    // SAFETY: `Component::deserialize` trusts its input, and the artifact comes
    // from a child that just parsed untrusted bytes. We accept this residual
    // risk explicitly: the child is credential-free and resource-limited
    // (rlimits + timeout) and runs this same binary, and the artifact is read
    // from a private 0700 dir we created (not a shipped/world-writable path).
    // A fully safe variant (compile in-process under bounds, or validate the
    // artifact) is future work, tracked with the mod sandbox hardening.
    unsafe { wasmtime::component::Component::deserialize(engine, &artifact) }.map_err(|e| {
        anyhow!(
            "deserializing the component: {}",
            sanitize_log(&e.to_string())
        )
    })
}

/// One instantiated, ready-to-run mod, with its own store (so its KV persists
/// across events).
/// How often (in epoch ticks) the kill-switch callback re-checks the `kill` flag
/// and the wall-clock backstop: 50 × `EPOCH_TICK_MS` (10 ms) ≈ 0.5 s, so a killed
/// or wedged guest is reaped within about half a second of guest execution (host
/// I/O is bounded separately by its own timeouts, and interrupted only when it
/// returns to the guest — on-demand I/O cancellation is a later slice).
const EPOCH_CHECK_TICKS: u64 = 50;

/// A unit of work handed to a mod's worker thread. Every guest-calling entry point
/// of [`Host`] becomes a variant; the coordinator sends it over the mod's channel
/// and blocks on the oneshot `reply`. The guest runs on the worker thread, which
/// owns the `!Sync` `Store`, so a slow or wedged mod blocks only its own thread.
enum Job {
    /// The message itself is read from `index` (shared), so fanning a window
    /// out to every mod does not copy each text once per mod.
    Message {
        channel: String,
        ts: String,
        index: Arc<HashMap<(String, String), Msg>>,
        directory: Arc<Directory>,
        /// The dispatch this message belongs to: the notify/API-write budget is
        /// per dispatch, kept apart even when two dispatches interleave here.
        dispatch: u64,
        reply: std::sync::mpsc::SyncSender<Vec<String>>,
    },
    Event {
        event_name: String,
        channel: String,
        reply: std::sync::mpsc::SyncSender<bool>,
    },
    Reaction {
        channel: String,
        ts: String,
        emoji: String,
        user: String,
        added: bool,
        index: Arc<HashMap<(String, String), Msg>>,
        reply: std::sync::mpsc::SyncSender<bool>,
    },
    Command {
        name: String,
        args: String,
        channel: String,
        reply: std::sync::mpsc::SyncSender<bool>,
    },
    Action {
        action_id: String,
        channel: String,
        ts: String,
        index: Arc<HashMap<(String, String), Msg>>,
        reply: std::sync::mpsc::SyncSender<Option<Vec<String>>>,
    },
    DialogAction {
        action_id: String,
        reply: std::sync::mpsc::SyncSender<bool>,
    },
    ComposerAction {
        action_id: String,
        channel: String,
        draft: String,
        reply: std::sync::mpsc::SyncSender<Option<String>>,
    },
    ViewContent {
        view_id: String,
        reply: std::sync::mpsc::SyncSender<Vec<(String, String)>>,
    },
    TakeDialogEffects {
        reply: std::sync::mpsc::SyncSender<Option<DialogEffect>>,
    },
    SetApi {
        api: Arc<crate::api::Client>,
        reply: std::sync::mpsc::SyncSender<()>,
    },
    /// Test-only: report the keys of the worker's current message index, so a test
    /// can assert the per-dispatch index hand-off without reaching into the Store.
    #[cfg(test)]
    InspectMessageKeys {
        reply: std::sync::mpsc::SyncSender<Vec<(String, String)>>,
    },
    /// Test-only: make the worker thread panic, to exercise crash handling.
    #[cfg(test)]
    Crash,
    Shutdown,
}

/// Live per-mod counters, written by the worker and read lock-free by the
/// coordinator (the control panel polls them every frame).
#[derive(Default)]
struct ModStats {
    /// Guest calls completed (every callback, ticks included).
    calls: std::sync::atomic::AtomicU64,
    /// Background ticks the guest accepted.
    ticks: std::sync::atomic::AtomicU64,
    /// When the current guest call started, in ms since [`stats_origin`] plus one;
    /// `0` means idle.
    busy_since: std::sync::atomic::AtomicU64,
}

/// A fixed origin for [`ModStats::busy_since`].
fn stats_origin() -> std::time::Instant {
    static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *ORIGIN.get_or_init(std::time::Instant::now)
}

fn now_ms() -> u64 {
    stats_origin().elapsed().as_millis() as u64 + 1
}

/// Whether a mod is live or stopped (quarantined: trapped, crashed, or stopped
/// by the user) for the rest of the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModState {
    Running,
    Stopped,
}

/// One mod's live status, for the control panel.
#[derive(Clone, Debug)]
pub struct ModStatus {
    pub id: String,
    pub name: String,
    pub state: ModState,
    /// How long the guest call in progress has been running; `None` when idle.
    pub busy_for: Option<std::time::Duration>,
    /// Guest calls completed so far (ticks included).
    pub calls: u64,
    /// Background ticks delivered so far.
    pub ticks: u64,
    /// The background lane's interval, when it runs.
    pub background: Option<std::time::Duration>,
}

impl Job {
    /// Answers a guest job without running it (the mod is stopped): the reply a
    /// caller would get from a mod that did nothing.
    fn refuse(self) {
        match self {
            Job::Message { reply, .. } => {
                let _ = reply.send(Vec::new());
            }
            Job::Event { reply, .. }
            | Job::Reaction { reply, .. }
            | Job::Command { reply, .. }
            | Job::DialogAction { reply, .. } => {
                let _ = reply.send(false);
            }
            Job::Action { reply, .. } => {
                let _ = reply.send(None);
            }
            Job::ComposerAction { reply, .. } => {
                let _ = reply.send(None);
            }
            _ => {}
        }
    }

    /// Whether handling this job runs guest code (for the busy/calls counters).
    fn runs_guest(&self) -> bool {
        matches!(
            self,
            Job::Message { .. }
                | Job::Event { .. }
                | Job::Reaction { .. }
                | Job::Command { .. }
                | Job::Action { .. }
                | Job::DialogAction { .. }
                | Job::ComposerAction { .. }
        )
    }
}

/// The coordinator-side handle for one mod: the metadata the front ends and the
/// cache logic read without touching the worker, a clone of the (immutable)
/// grants and the shared settings, the kill/trap flags, and the channel to the
/// worker thread that owns the mod's `Store`.
struct ModHandle {
    id: String,
    name: String,
    actions: Vec<MessageAction>,
    views: Vec<ViewDecl>,
    events: Vec<events::Event>,
    commands: Vec<CommandDecl>,
    composer_actions: Vec<ComposerAction>,
    context_actions: Vec<ContextMenuAction>,
    theme: Theme,
    /// A clone of the grants (immutable for the mod's life), so the pure accessors
    /// and dispatch pre-filtering gate without a worker round-trip. The worker's
    /// store holds the authoritative copy that enforces host calls.
    grants: Grants,
    /// The hosts it may reach (shown when the user decides about its secrets).
    egress: Vec<String>,
    /// The mod's live settings, shared (`Arc`) with the worker's store and the
    /// settings form.
    settings: Arc<std::sync::RwLock<settings::Settings>>,
    /// Set by the worker when a call traps: the mod is quarantined.
    trapped: Arc<std::sync::atomic::AtomicBool>,
    /// Set by the coordinator to abort a running guest (read by the worker's epoch
    /// callback).
    kill: Arc<std::sync::atomic::AtomicBool>,
    /// Work channel to the worker thread.
    tx: std::sync::mpsc::Sender<Job>,
    /// The worker thread, joined on drop.
    join: Option<std::thread::JoinHandle<()>>,
    /// Live counters shared with the worker.
    stats: Arc<ModStats>,
    /// The background lane's interval, when it runs.
    tick: Option<std::time::Duration>,
}

impl ModHandle {
    /// Whether the mod subscribed to `event`.
    fn subscribes(&self, event: events::Event) -> bool {
        self.events.contains(&event)
    }

    /// Whether the mod declares the slash command `name`.
    fn declares_command(&self, name: &str) -> bool {
        self.commands.iter().any(|c| c.name == name)
    }

    /// Whether the mod declares the composer action `id`.
    fn declares_composer_action(&self, id: &str) -> bool {
        self.composer_actions.iter().any(|a| a.id == id)
    }

    /// Whether the mod is quarantined.
    fn is_trapped(&self) -> bool {
        self.trapped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Send a job built with a fresh oneshot sender and block on the reply.
    /// `None` if the worker is gone, which a caller treats as "the mod did
    /// nothing" (and which quarantines it, see [`Self::mark_dead`]).
    fn request<T>(&self, make: impl FnOnce(std::sync::mpsc::SyncSender<T>) -> Job) -> Option<T> {
        let rx = self.send_request(make)?;
        self.await_reply(rx)
    }

    /// The first half of [`Self::request`]: queue the job and return its reply
    /// receiver, so a caller can fan a job out to several mods before waiting.
    fn send_request<T>(
        &self,
        make: impl FnOnce(std::sync::mpsc::SyncSender<T>) -> Job,
    ) -> Option<std::sync::mpsc::Receiver<T>> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if self.tx.send(make(tx)).is_err() {
            self.mark_dead();
            return None;
        }
        Some(rx)
    }

    /// The second half of [`Self::request`].
    fn await_reply<T>(&self, rx: std::sync::mpsc::Receiver<T>) -> Option<T> {
        match rx.recv() {
            Ok(v) => Some(v),
            Err(_) => {
                self.mark_dead();
                None
            }
        }
    }

    /// The worker's channel closed without a `Shutdown` (only `Drop` sends one),
    /// i.e. the thread died: quarantine the mod so the front ends stop offering it.
    fn mark_dead(&self) {
        if !self.trapped.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                "mod {} worker stopped unexpectedly (quarantined)",
                sanitize_log(&self.id)
            );
        }
    }
}

impl Drop for Host {
    /// Stops every worker without blocking the dropping thread (often the GUI's,
    /// which holds the last `Arc` after a reload): signal all of them first, so
    /// they wind down in parallel, then join them on a short-lived reaper thread.
    /// Their requests are cancelled (`await_io`) and guests reaped by the
    /// kill-switch, so they release the mod's files within about half a second.
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        let mut joins = Vec::new();
        for m in &mut self.mods {
            m.kill.store(true, Relaxed);
            let _ = m.tx.send(Job::Shutdown);
            if let Some(j) = m.join.take() {
                joins.push(j);
            }
        }
        if joins.is_empty() {
            return;
        }
        let reaper = std::thread::Builder::new()
            .name("slaq-mod-reaper".to_string())
            .spawn(move || {
                for j in joins {
                    let _ = j.join();
                }
            });
        if let Err(e) = reaper {
            tracing::warn!("mods: cannot start the reaper thread: {e}");
        }
    }
}

impl Drop for ModHandle {
    fn drop(&mut self) {
        // Abort any running guest, ask the loop to stop, and join the thread, so a
        // reload never leaves two workers on the same data files.
        self.kill.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.tx.send(Job::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Per-dispatch notify/API-write tallies for one worker. The host imports count
/// against `HostState::{notify_count, api_writes}`; the worker swaps the right
/// dispatch's tally in before each `on-message` and back out after, so two
/// dispatches interleaving on one mod keep separate budgets. Bounded: the oldest
/// slots are dropped beyond [`MAX_BUDGET_SLOTS`].
#[derive(Default)]
struct DispatchBudgets {
    /// `(dispatch, notify_count, api_writes)`, oldest first.
    slots: VecDeque<(u64, usize, usize)>,
}

/// Concurrent dispatches whose budgets a worker remembers.
const MAX_BUDGET_SLOTS: usize = 64;
/// Marks a [`DispatchBudgets`] slot as a background minute window (dispatch ids
/// count up from 1 and never reach this bit).
const BACKGROUND_BUDGET_BIT: u64 = 1 << 63;

impl DispatchBudgets {
    fn load(&self, dispatch: u64) -> (usize, usize) {
        self.slots
            .iter()
            .find(|(d, _, _)| *d == dispatch)
            .map(|&(_, n, a)| (n, a))
            .unwrap_or((0, 0))
    }

    fn save(&mut self, dispatch: u64, notify: usize, api: usize) {
        if let Some(slot) = self.slots.iter_mut().find(|(d, _, _)| *d == dispatch) {
            *slot = (dispatch, notify, api);
            return;
        }
        self.slots.push_back((dispatch, notify, api));
        while self.slots.len() > MAX_BUDGET_SLOTS {
            self.slots.pop_front();
        }
    }
}

/// The worker owning one mod's instance: it runs on a dedicated OS thread and its
/// `Store` never leaves that thread.
struct ModWorker {
    id: String,
    plugin: Plugin,
    store: wasmtime::Store<HostState>,
    trapped: Arc<std::sync::atomic::AtomicBool>,
    /// Remaining kill-switch checks before the wall-clock backstop trips, reset
    /// before each guest call. `u64::MAX` means the backstop is disabled.
    backstop: Arc<std::sync::atomic::AtomicU64>,
    /// The background lane: when the mod subscribed to `background.tick` and
    /// holds `background`, the worker wakes it this often (fixed delay).
    tick: Option<std::time::Duration>,
    /// Per-dispatch notify/API-write budgets for `on-message`.
    budgets: DispatchBudgets,
    /// Live counters shared with the handle.
    stats: Arc<ModStats>,
}

impl ModWorker {
    fn arm(&mut self) {
        arm_deadline(&mut self.store, &self.backstop);
    }

    fn quarantine(&self, e: &wasmtime::Error) {
        self.trapped
            .store(true, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            "mod {} trapped (quarantined): {}",
            sanitize_log(&self.id),
            sanitize_log(&e.to_string())
        );
    }

    fn on_message(
        &mut self,
        channel: &str,
        ts: &str,
        index: Arc<HashMap<(String, String), Msg>>,
        directory: Arc<Directory>,
        dispatch: u64,
    ) -> Vec<String> {
        let (author, text) = index
            .get(&(channel.to_string(), ts.to_string()))
            .map(|m| (m.author.clone(), m.text.clone()))
            .unwrap_or_default();
        let (author, text) = (author.as_str(), text.as_str());
        let (notify, api) = self.budgets.load(dispatch);
        {
            let d = self.store.data_mut();
            d.decorations.clear();
            d.decoration_bytes = 0;
            d.current = Some((channel.to_string(), ts.to_string()));
            d.messages = index;
            d.directory = Some(directory);
            // Per-callback budget; the notify/api budget is this dispatch's.
            d.settings_writes = 0;
            d.notify_count = notify;
            d.api_writes = api;
        }
        self.arm();
        let guest = self.plugin.slaq_api_guest();
        let call = guest.call_on_message(&mut self.store, channel, ts, author, text);
        let (notify, api) = {
            let d = self.store.data();
            (d.notify_count, d.api_writes)
        };
        self.budgets.save(dispatch, notify, api);
        match call {
            Ok(Ok(())) => self
                .store
                .data_mut()
                .decorations
                .drain(..)
                .map(|(_, _, t)| t)
                .collect(),
            Ok(Err(e)) => {
                tracing::warn!("mod {}: {}", sanitize_log(&self.id), sanitize_log(&e));
                Vec::new()
            }
            Err(e) => {
                self.quarantine(&e);
                Vec::new()
            }
        }
    }

    /// Reset the one-shot-gesture budgets (every entry point but `on-message`
    /// resets these itself; `on-message` relies on the per-dispatch reset).
    fn reset_gesture(&mut self, keep_current: Option<(String, String)>) {
        let d = self.store.data_mut();
        d.decorations.clear();
        d.decoration_bytes = 0;
        d.current = keep_current;
        d.settings_writes = 0;
        d.notify_count = 0;
        d.api_writes = 0;
    }

    fn on_event(&mut self, event_name: &str, channel: &str) -> bool {
        self.reset_gesture(None);
        self.arm();
        let guest = self.plugin.slaq_api_guest();
        match guest.call_on_event(&mut self.store, event_name, channel) {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!("mod {}: {}", sanitize_log(&self.id), sanitize_log(&e));
                false
            }
            Err(e) => {
                self.quarantine(&e);
                false
            }
        }
    }

    fn on_reaction(
        &mut self,
        channel: &str,
        ts: &str,
        emoji: &str,
        user: &str,
        added: bool,
        index: Arc<HashMap<(String, String), Msg>>,
    ) -> bool {
        self.reset_gesture(None);
        self.store.data_mut().messages = index;
        self.arm();
        let guest = self.plugin.slaq_api_guest();
        match guest.call_on_reaction(&mut self.store, channel, ts, emoji, user, added) {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!("mod {}: {}", sanitize_log(&self.id), sanitize_log(&e));
                false
            }
            Err(e) => {
                self.quarantine(&e);
                false
            }
        }
    }

    fn on_command(&mut self, name: &str, args: &str, channel: &str) -> bool {
        self.reset_gesture(None);
        self.arm();
        let guest = self.plugin.slaq_api_guest();
        match guest.call_on_command(&mut self.store, name, args, channel) {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!("mod {}: {}", sanitize_log(&self.id), sanitize_log(&e));
                false
            }
            Err(e) => {
                self.quarantine(&e);
                false
            }
        }
    }

    fn on_action(
        &mut self,
        action_id: &str,
        channel: &str,
        ts: &str,
        index: Arc<HashMap<(String, String), Msg>>,
    ) -> Option<Vec<String>> {
        self.reset_gesture(Some((channel.to_string(), ts.to_string())));
        self.store.data_mut().messages = index;
        self.arm();
        let guest = self.plugin.slaq_api_guest();
        match guest.call_on_action(&mut self.store, action_id, channel, ts) {
            Ok(Ok(())) => Some(
                self.store
                    .data_mut()
                    .decorations
                    .drain(..)
                    .map(|(_, _, t)| t)
                    .collect(),
            ),
            Ok(Err(e)) => {
                tracing::warn!("mod {}: {}", sanitize_log(&self.id), sanitize_log(&e));
                None
            }
            Err(e) => {
                self.quarantine(&e);
                None
            }
        }
    }

    fn on_dialog_action(&mut self, action_id: &str) -> bool {
        // Store-side gate (`dialog_actions` is set when the dialog was shown):
        // only an offered action on a mod holding `ui.dialog` is routed.
        {
            let d = self.store.data();
            if !d.grants.has(Capability::UiDialog)
                || !d.dialog_actions.iter().any(|a| a == action_id)
            {
                return false;
            }
        }
        self.reset_gesture(None);
        self.arm();
        let guest = self.plugin.slaq_api_guest();
        match guest.call_on_dialog_action(&mut self.store, action_id) {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!("mod {}: {}", sanitize_log(&self.id), sanitize_log(&e));
                true
            }
            Err(e) => {
                self.quarantine(&e);
                true
            }
        }
    }

    fn on_composer_action(&mut self, action_id: &str, channel: &str, draft: &str) -> Option<String> {
        self.reset_gesture(None);
        {
            let d = self.store.data_mut();
            d.composer_edit = None;
            d.composer_action_channel = Some(channel.to_string());
        }
        self.arm();
        let guest = self.plugin.slaq_api_guest();
        let call = guest.call_on_composer_action(&mut self.store, action_id, channel, draft);
        self.store.data_mut().composer_action_channel = None;
        match call {
            Ok(Ok(())) => self.store.data_mut().composer_edit.take(),
            Ok(Err(e)) => {
                tracing::warn!("mod {}: {}", sanitize_log(&self.id), sanitize_log(&e));
                None
            }
            Err(e) => {
                self.quarantine(&e);
                None
            }
        }
    }

    /// Whether the mod is stopped or quarantined: no more guest code runs.
    fn stopped(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        self.trapped.load(Relaxed) || self.store.data().kill.load(Relaxed)
    }

    /// One background-lane wake-up: `on-event("background.tick", "")`. `None`
    /// when skipped (the mod is stopped), else whether the guest accepted it.
    ///
    /// Ticks share one notify/API-write budget per minute (a dispatch-budget slot
    /// keyed by the minute), instead of a fresh budget per tick: at a 0.1 s
    /// interval a per-tick reset would allow hundreds of desktop notifications and
    /// Slack writes a minute, defeating the per-dispatch bound.
    fn on_background_tick(&mut self) -> Option<bool> {
        if self.stopped() {
            return None;
        }
        let window = BACKGROUND_BUDGET_BIT | (now_ms() / 60_000);
        let (notify, api) = self.budgets.load(window);
        self.reset_gesture(None);
        {
            let d = self.store.data_mut();
            d.notify_count = notify;
            d.api_writes = api;
        }
        self.arm();
        let guest = self.plugin.slaq_api_guest();
        let call =
            guest.call_on_event(&mut self.store, events::Event::BackgroundTick.name(), "");
        let (notify, api) = {
            let d = self.store.data();
            (d.notify_count, d.api_writes)
        };
        self.budgets.save(window, notify, api);
        Some(match call {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::debug!("mod {}: tick: {}", sanitize_log(&self.id), sanitize_log(&e));
                false
            }
            Err(e) => {
                self.quarantine(&e);
                false
            }
        })
    }

    fn view_content(&self, view_id: &str) -> Vec<(String, String)> {
        self.store
            .data()
            .view_content
            .get(view_id)
            .cloned()
            .unwrap_or_default()
    }

    fn take_dialog_effect(&mut self) -> Option<DialogEffect> {
        self.store.data_mut().pending_dialog.take()
    }
}

/// Re-arm the per-call wall-clock backstop and epoch deadline before a guest call.
fn arm_deadline(store: &mut wasmtime::Store<HostState>, backstop: &std::sync::atomic::AtomicU64) {
    let ticks = epoch_deadline_ticks();
    let checks = if ticks == u64::MAX {
        u64::MAX
    } else {
        (ticks / EPOCH_CHECK_TICKS).max(1)
    };
    backstop.store(checks, std::sync::atomic::Ordering::Relaxed);
    store.set_epoch_deadline(EPOCH_CHECK_TICKS);
}

/// Install the kill-switch: an epoch-deadline callback that traps the guest when
/// the coordinator sets `kill`, or when the per-call wall-clock backstop runs out
/// (generous; disabled when `SLAQ_MOD_DEADLINE_SECS=0`, which keeps `backstop` at
/// `u64::MAX`). Otherwise it re-arms for another check interval.
fn install_kill_switch(
    store: &mut wasmtime::Store<HostState>,
    kill: Arc<std::sync::atomic::AtomicBool>,
    backstop: Arc<std::sync::atomic::AtomicU64>,
) {
    use std::sync::atomic::Ordering::Relaxed;
    store.epoch_deadline_callback(move |_ctx| {
        if kill.load(Relaxed) {
            return Err(wasmtime::Error::msg("mod killed"));
        }
        let left = backstop.load(Relaxed);
        if left != u64::MAX {
            if left <= 1 {
                return Err(wasmtime::Error::msg("mod wall-clock backstop exceeded"));
            }
            backstop.store(left - 1, Relaxed);
        }
        Ok(wasmtime::UpdateDeadline::Continue(EPOCH_CHECK_TICKS))
    });
}

/// The worker loop: own the mod's instance and service jobs one at a time until
/// the channel closes or a `Shutdown` arrives. Dropping the `ModWorker` here drops
/// the `Store` (and its SQLite/KV connections) on this thread.
fn run_worker(mut worker: ModWorker, rx: std::sync::mpsc::Receiver<Job>) {
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Instant;
    // The background lane: the next wake-up, if the mod has one. Checked before
    // every receive, so a steady stream of jobs cannot starve the ticks; re-armed
    // *after* each tick (fixed delay), so a long tick never queues a catch-up burst.
    // `checked_add`: an interval too large to add (impossible after manifest
    // validation, but never worth a panic) means "never".
    let arm = |tick: Option<std::time::Duration>| tick.and_then(|d| Instant::now().checked_add(d));
    let mut next_tick = arm(worker.tick);
    let stats = worker.stats.clone();
    let busy = |on: bool| {
        stats
            .busy_since
            .store(if on { now_ms() } else { 0 }, std::sync::atomic::Ordering::Relaxed);
    };
    loop {
        let job = match next_tick {
            None => match rx.recv() {
                Ok(job) => job,
                Err(_) => break,
            },
            Some(at) => {
                let now = Instant::now();
                if now >= at {
                    busy(true);
                    let ran = worker.on_background_tick();
                    busy(false);
                    if let Some(accepted) = ran {
                        stats.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if accepted {
                            stats.ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    next_tick = arm(worker.tick);
                    continue;
                }
                match rx.recv_timeout(at - now) {
                    Ok(job) => job,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        };
        let guest_call = job.runs_guest();
        // A stopped (or quarantined) mod runs no more guest code — including jobs
        // queued before the stop, which would otherwise still post, react, notify.
        let job = if guest_call && worker.stopped() {
            job.refuse();
            continue;
        } else {
            job
        };
        if guest_call {
            busy(true);
        }
        match job {
            Job::Shutdown => break,
            Job::SetApi { api, reply } => {
                worker.store.data_mut().api = Some(api);
                let _ = reply.send(());
            }
            Job::Message {
                channel,
                ts,
                index,
                directory,
                dispatch,
                reply,
            } => {
                let texts = worker.on_message(&channel, &ts, index, directory, dispatch);
                let _ = reply.send(texts);
            }
            Job::Event {
                event_name,
                channel,
                reply,
            } => {
                let ok = worker.on_event(&event_name, &channel);
                let _ = reply.send(ok);
            }
            Job::Reaction {
                channel,
                ts,
                emoji,
                user,
                added,
                index,
                reply,
            } => {
                let ok = worker.on_reaction(&channel, &ts, &emoji, &user, added, index);
                let _ = reply.send(ok);
            }
            Job::Command {
                name,
                args,
                channel,
                reply,
            } => {
                let ok = worker.on_command(&name, &args, &channel);
                let _ = reply.send(ok);
            }
            Job::Action {
                action_id,
                channel,
                ts,
                index,
                reply,
            } => {
                let out = worker.on_action(&action_id, &channel, &ts, index);
                let _ = reply.send(out);
            }
            Job::DialogAction { action_id, reply } => {
                let routed = worker.on_dialog_action(&action_id);
                let _ = reply.send(routed);
            }
            Job::ComposerAction {
                action_id,
                channel,
                draft,
                reply,
            } => {
                let edit = worker.on_composer_action(&action_id, &channel, &draft);
                let _ = reply.send(edit);
            }
            Job::ViewContent { view_id, reply } => {
                let _ = reply.send(worker.view_content(&view_id));
            }
            Job::TakeDialogEffects { reply } => {
                let _ = reply.send(worker.take_dialog_effect());
            }
            #[cfg(test)]
            Job::InspectMessageKeys { reply } => {
                let keys = worker.store.data().messages.keys().cloned().collect();
                let _ = reply.send(keys);
            }
            #[cfg(test)]
            Job::Crash => panic!("test: worker crash"),
        }
        if guest_call {
            busy(false);
            stats.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// The set of installed mods, loaded once and reused.
pub struct Host {
    engine: wasmtime::Engine,
    /// Immutable after load (a reload builds a fresh `Host` and swaps the `Arc`),
    /// so dispatch reads it through `&self` with no lock.
    mods: Vec<ModHandle>,
    /// The dedup/decoration bookkeeping, behind a lock so dispatch can run through
    /// `&self` (the app holds an `Arc<Host>`, not a `Mutex<Host>`): the lock is
    /// taken only for the brief cache updates, never while a guest runs, so a slow
    /// mod does not block another's dispatch.
    cache: std::sync::Mutex<Cache>,
    /// Numbers dispatches, so each mod keeps a separate per-dispatch budget.
    dispatch_seq: std::sync::atomic::AtomicU64,
}

/// The dispatch bookkeeping a [`Host`] guards behind a lock.
struct Cache {
    /// `(channel, ts) -> hash of the text` already processed, so a reload does
    /// not re-run them but an edit does.
    seen: HashMap<(String, String), u64>,
    /// `(key, seq)` in delivery order: a node is live iff `seen_seq[key] == seq`
    /// (a per-delivery sequence, unique even for an A→B→A edit), so a re-delivery
    /// refreshes its position and stale nodes are skipped.
    seen_order: VecDeque<((String, String), u64)>,
    seen_seq: HashMap<(String, String), u64>,
    seen_seq_counter: u64,
    /// Decorations produced so far, per source `(channel, ts)`: kept so a
    /// reload returns the full set instead of an empty one. Each text is tagged
    /// with the mod that produced it, so an action can replace one mod's output
    /// without dropping the others'.
    decorated: HashMap<(String, String), Vec<(String, String)>>,
    /// Aggregate bytes held in `decorated`, to enforce the cache quota.
    decorated_bytes: usize,
    /// `(key, seq)` in insertion order: live iff `decorated_seq[key] == seq`
    /// (lazy tombstones), so eviction is amortized O(1), not a queue rescan.
    decorated_order: VecDeque<((String, String), u64)>,
    decorated_seq: HashMap<(String, String), u64>,
    decorated_seq_counter: u64,
    /// Bound on the `seen`/`decorated` caches (overridable in tests).
    cache_cap: usize,
}

impl Host {
    /// Loads every immediate subfolder of `root` that has a `manifest.json`,
    /// granting each mod only the capabilities `consent` records for it (default
    /// deny). A missing/empty root yields an empty host (not an error).
    /// `directory` is the client's directory, available to the mods from `start`.
    pub fn load_root(
        root: &Path,
        engine: wasmtime::Engine,
        consent: &Consent,
        directory: &Arc<Directory>,
        data_base: &Path,
    ) -> Host {
        let mut host = Host::empty(engine);
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            // A missing directory is just "no mods"; any other error (permissions,
            // I/O, bad configured path) must be visible, not a silent no-mods.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return host,
            Err(e) => {
                tracing::warn!(
                    "mods: cannot read {}: {}",
                    sanitize_log(&root.display().to_string()),
                    sanitize_log(&e.to_string())
                );
                return host;
            }
        };
        // Deterministic order (read_dir is unordered), so multiple mods' decorations
        // appear in a stable order across runs and platforms.
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|d| d.join("manifest.json").is_file())
            .collect();
        dirs.sort();
        // Cap load *attempts* (not only successful loads): an install full of
        // malformed packages must not make startup work unbounded.
        if dirs.len() > MAX_MODS {
            tracing::warn!(
                "mods: {} packages found, loading the first {MAX_MODS}",
                dirs.len()
            );
            dirs.truncate(MAX_MODS);
        }
        for dir in dirs {
            // Path and error are package-controlled: sanitize before logging.
            let shown = sanitize(&dir.display().to_string());
            match host.add(&dir, consent, directory, data_base) {
                Ok(()) => tracing::info!("mod loaded: {shown}"),
                Err(e) => tracing::warn!("mod skipped ({shown}): {}", sanitize_log(&e.to_string())),
            }
        }
        host
    }

    /// Loads a single mod folder with exactly `granted`, bypassing the persistent
    /// consent store (used by the headless runner, which passes an explicit grant).
    pub fn load_one_with(
        dir: &Path,
        engine: wasmtime::Engine,
        granted: &BTreeSet<Capability>,
    ) -> Result<Host> {
        Self::load_one_with_fs(dir, engine, granted, &BTreeMap::new())
    }

    /// Like [`Host::load_one_with`], with an explicit `filesystem` slot mapping
    /// (the one-shot runner's `--fs-slot`); the sandbox is built from it when
    /// `filesystem` is granted. An empty map is the no-mapping case (fail closed).
    pub fn load_one_with_fs(
        dir: &Path,
        engine: wasmtime::Engine,
        granted: &BTreeSet<Capability>,
        fs_slots: &BTreeMap<String, (PathBuf, fs::FsAccess)>,
    ) -> Result<Host> {
        let loaded = load(dir)?;
        let grants = Grants::from_request_with_fs(&loaded.requested, granted, fs_slots.clone());
        let egress = loaded.requested.egress().to_vec();
        let mut host = Host::empty(engine);
        host.push_loaded(
            loaded,
            grants,
            egress,
            &Arc::new(Directory::empty()),
            Path::new(""),
        )?;
        Ok(host)
    }

    /// A host with no mods, the default caches and the given engine.
    fn empty(engine: wasmtime::Engine) -> Host {
        Host {
            engine,
            mods: Vec::new(),
            cache: std::sync::Mutex::new(Cache {
                seen: HashMap::new(),
                seen_order: VecDeque::new(),
                seen_seq: HashMap::new(),
                seen_seq_counter: 0,
                decorated: HashMap::new(),
                decorated_bytes: 0,
                decorated_order: VecDeque::new(),
                decorated_seq: HashMap::new(),
                decorated_seq_counter: 0,
                cache_cap: MAX_CACHE,
            }),
            dispatch_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The cache, recovering from a poisoned lock (a panic elsewhere must not
    /// cascade into every later dispatch; the cache stays internally consistent
    /// because every mutation completes under the lock).
    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Loads one package; its runtime grants come from the persistent consent
    /// record (default deny). The enforced read scope is `min(consent, manifest)`.
    fn add(
        &mut self,
        dir: &Path,
        consent: &Consent,
        directory: &Arc<Directory>,
        data_base: &Path,
    ) -> Result<()> {
        let loaded = load(dir)?;
        let entry = consent.entry(&loaded.manifest.id);
        let grants = Grants::from_consent(&loaded.requested, &entry);
        // The reachable hosts are consent ∩ manifest (an update cannot widen).
        let egress = Grants::effective_egress(&loaded.requested, &entry);
        self.push_loaded(loaded, grants, egress, directory, data_base)
    }

    fn push_loaded(
        &mut self,
        loaded: Loaded,
        grants: Grants,
        egress: Vec<String>,
        directory: &Arc<Directory>,
        data_base: &Path,
    ) -> Result<()> {
        if self.mods.len() >= MAX_MODS {
            return Err(anyhow!("too many mods (max {MAX_MODS})"));
        }
        // Manifest `id` is unique (docs/mods/manifest-v0.md); two folders with the
        // same id would run as separate instances with separate KV, breaking the
        // package identity. Reject the duplicate before compiling anything.
        if self.mods.iter().any(|m| m.id == loaded.manifest.id) {
            return Err(anyhow!(
                "duplicate mod id: {}",
                sanitize(&loaded.manifest.id)
            ));
        }
        tracing::debug!(
            "mod {}: granted {:?}",
            sanitize(&loaded.manifest.id),
            grants.names()
        );
        let bytes = read_bounded(&loaded.wasm, MAX_COMPONENT_BYTES)?;
        // Bind the mod's secrets to this exact package: the component **and** the
        // manifest (a replacement could keep the wasm and change the manifest).
        let fingerprint = package_fingerprint_of(&loaded.fingerprint, &bytes);
        let component = compile_component(&self.engine, &bytes).with_context(|| {
            format!("compiling {}", sanitize(&loaded.wasm.display().to_string()))
        })?;

        // The `filesystem` sandbox is built from the **effective** slot mapping
        // (consent ∩ manifest); computed here because `grants` moves into the host
        // state below. `None` when the capability is absent, so calls fail closed.
        let fs_sandbox = grants
            .has(Capability::Filesystem)
            .then(|| fs::FsSandbox::new(grants.fs_slots()));

        let mut linker = wasmtime::component::Linker::new(&self.engine);
        host::add_to_linker::<_, HasSelf<_>>(&mut linker, |s: &mut HostState| s)?;
        // The handle keeps a clone of the (immutable) grants so the coordinator can
        // gate accessors and dispatch without a worker round-trip; the store holds
        // the authoritative copy that enforces host calls.
        let handle_grants = grants.clone();
        let handle_egress = egress.clone();
        let mut store = wasmtime::Store::new(&self.engine, HostState::new(grants, egress));
        // The directory is available from the start (a directory-only mod uses it
        // in `start`); it is refreshed on every dispatch.
        store.data_mut().directory = Some(directory.clone());
        // Load the mod's settings once, up front (schema defaults, stored
        // non-secret values, keyring secrets), so `start` already sees them.
        store.data_mut().settings = Arc::new(std::sync::RwLock::new(settings::Settings::load(
            &loaded.manifest.id,
            loaded.settings.clone(),
            &fingerprint,
        )));
        // Share the settings `Arc` with the handle (the settings form reads it).
        let handle_settings = store.data().settings.clone();
        // The views this mod declared, for `set-view-content` validation.
        store.data_mut().declared_views = loaded.views.iter().map(|v| v.id.clone()).collect();
        // Attribute any `notify-post` to the mod by its **id** (unique and
        // constrained, unlike the free-text `name`), sanitized.
        store.data_mut().notify_label = sanitize(&loaded.manifest.id);
        // The declared persistent store (`store.kv` + `store.vector`), when either
        // is declared; its file is opened lazily, on first use. An empty data base
        // means the one-shot headless run: an in-memory store.
        let kv = loaded.requested.kv();
        let vector = loaded.requested.vector();
        if kv.is_some() || vector.is_some() {
            let ms = if data_base.as_os_str().is_empty() {
                crate::mods::store::ModStore::in_memory(kv, vector)
            } else {
                crate::mods::store::ModStore::at_path(
                    data_base.join(&loaded.manifest.id).join("store.db"),
                    kv,
                    vector,
                )
            };
            store.data_mut().store = Some(ms);
        }
        // The declared `store.sql`: a **separate** `scratch.db` (its own quota), so
        // SQL growth cannot consume the KV/vector budget. Opened lazily on use.
        if let Some(cfg) = loaded.requested.sql() {
            let ms = if data_base.as_os_str().is_empty() {
                crate::mods::sql::ModSql::in_memory(cfg)
            } else {
                crate::mods::sql::ModSql::at_path(
                    data_base.join(&loaded.manifest.id).join("scratch.db"),
                    cfg,
                )
            };
            store.data_mut().sql = Some(ms);
        }
        // The `filesystem` sandbox: built from the **effective** slot mapping
        // (consent ∩ manifest). A mod without the capability, or whose slots were
        // all dropped, gets `None` and every fs call fails closed.
        if let Some(sandbox) = fs_sandbox {
            store.data_mut().fs = Some(sandbox);
        }
        store.limiter(|s| &mut s.limits);
        // The kill-switch (abort on demand + generous wall-clock backstop) is armed
        // before `start`, which runs guest code and could itself wedge.
        let trapped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let kill = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let backstop = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        install_kill_switch(&mut store, kill.clone(), backstop.clone());
        store.data_mut().kill = kill.clone();
        arm_deadline(&mut store, &backstop);

        let plugin = Plugin::instantiate(&mut store, &component, &linker)?;
        plugin
            .slaq_api_guest()
            .call_start(&mut store)?
            .map_err(|e| anyhow!("mod start: {}", sanitize_log(&e)))?;

        // Move the instance onto its own OS thread; from here the `Store` is owned
        // by the worker and reached only by `Job`s over `tx`.
        // The background lane runs only when the manifest subscribed to
        // `background.tick` (which requires requesting `background`) **and** the
        // user granted `background`.
        let tick = (loaded.events.contains(&events::Event::BackgroundTick)
            && handle_grants.has(Capability::Background))
        .then(|| loaded.requested.background().map(|b| b.interval))
        .flatten();
        let stats = Arc::new(ModStats::default());
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let worker = ModWorker {
            id: loaded.manifest.id.clone(),
            plugin,
            store,
            trapped: trapped.clone(),
            backstop,
            tick,
            budgets: DispatchBudgets::default(),
            stats: stats.clone(),
        };
        let join = std::thread::Builder::new()
            .name(format!("slaq-mod-{}", loaded.manifest.id))
            .spawn(move || run_worker(worker, rx))
            .context("spawning the mod worker thread")?;
        self.mods.push(ModHandle {
            id: loaded.manifest.id,
            name: loaded.manifest.name,
            actions: loaded.actions,
            views: loaded.views,
            events: loaded.events,
            commands: loaded.commands,
            composer_actions: loaded.composer_actions,
            context_actions: loaded.context_actions,
            theme: loaded.theme,
            grants: handle_grants,
            egress: handle_egress,
            settings: handle_settings,
            trapped,
            kill,
            tx,
            join: Some(join),
            stats,
            tick,
        });
        Ok(())
    }

    /// Whether any mod is installed.
    pub fn is_empty(&self) -> bool {
        self.mods.is_empty()
    }

    /// Number of loaded mods.
    pub fn len(&self) -> usize {
        self.mods.len()
    }

    /// Every loaded mod's live status, in load order. Lock-free: it reads the
    /// handles' counters, never the workers, so it is cheap to poll every frame
    /// even while a mod is deep in a long call.
    pub fn statuses(&self) -> Vec<ModStatus> {
        use std::sync::atomic::Ordering::Relaxed;
        self.mods
            .iter()
            .map(|m| {
                let since = m.stats.busy_since.load(Relaxed);
                ModStatus {
                    id: sanitize(&m.id),
                    name: sanitize(&m.name),
                    state: if m.is_trapped() {
                        ModState::Stopped
                    } else {
                        ModState::Running
                    },
                    busy_for: (since != 0).then(|| {
                        std::time::Duration::from_millis(now_ms().saturating_sub(since))
                    }),
                    calls: m.stats.calls.load(Relaxed),
                    ticks: m.stats.ticks.load(Relaxed),
                    background: m.tick,
                }
            })
            .collect()
    }

    /// Stops a mod for the rest of the session: quarantines it (no more
    /// callbacks or ticks), aborts the guest call it may be running (the epoch
    /// kill-switch, within ~0.5 s of guest code) and cancels the host request it
    /// may be blocked on (within ~0.1 s). Its data stays; reloading the mods (or
    /// restarting) starts it again. Returns whether a live mod with `id` existed.
    pub fn stop(&self, id: &str) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(m) = self.mods.iter().find(|m| m.id == id) else {
            return false;
        };
        m.kill.store(true, Relaxed);
        let was_live = !m.trapped.swap(true, Relaxed);
        if was_live {
            tracing::info!("mod {} stopped by the user", sanitize_log(&m.id));
        }
        was_live
    }

    /// Test-only: the keys of a mod's current message index (what `get-message`
    /// would serve), fetched from its worker.
    #[cfg(test)]
    fn mod_message_keys(&self, idx: usize) -> Vec<(String, String)> {
        self.mods[idx]
            .request(|reply| Job::InspectMessageKeys { reply })
            .unwrap_or_default()
    }

    /// Test-only: background ticks the mod's guest accepted so far.
    #[cfg(test)]
    fn mod_ticks(&self, idx: usize) -> u64 {
        self.mods[idx].stats.ticks.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Which directory parts any loaded mod is granted, so the app only builds
    /// what is actually used: `(channels, users, files)`.
    pub fn directory_needs(&self) -> (bool, bool, bool) {
        let mut channels = false;
        let mut users = false;
        let mut files = false;
        for m in &self.mods {
            if m.is_trapped() {
                continue;
            }
            let grants = &m.grants;
            channels |= grants.has(Capability::ChannelsList);
            users |= grants.has(Capability::UsersList);
            files |= grants.has(Capability::FilesDownload);
        }
        (channels, users, files)
    }

    /// The loaded mods with secrets saved for another package of their id,
    /// waiting for the user to keep or forget them (see [`StaleSecrets`]).
    pub fn stale_secrets(&self) -> Vec<StaleSecrets> {
        self.mods
            .iter()
            .filter_map(|m| {
                let s = m.settings.read().unwrap_or_else(|e| e.into_inner());
                (!s.stale_secrets().is_empty()).then(|| StaleSecrets {
                    id: m.id.clone(),
                    name: sanitize(&m.name),
                    keys: s.stale_secrets().to_vec(),
                    package: s.package().to_string(),
                    egress: m.egress.clone(),
                })
            })
            .collect()
    }

    /// The settings handles of the loaded mods that declare a schema **and hold
    /// the `ui.settings` grant**, in load order. `ui.settings` gates the form: a
    /// revoked grant hides it (the mod's own `settings.*` stays ungated).
    pub fn settings_handles(&self) -> Vec<SettingsHandle> {
        self.mods
            .iter()
            .filter(|m| {
                m.grants.has(Capability::UiSettings)
                    && m.settings.read().map(|s| !s.is_empty()).unwrap_or(false)
            })
            .map(|m| SettingsHandle {
                id: sanitize(&m.id),
                name: sanitize(&m.name),
                settings: m.settings.clone(),
            })
            .collect()
    }

    /// The context-menu items of the loaded mods that hold `ui.context_menu` (the
    /// ones the front ends render in a message's context menu), in load order.
    pub fn context_actions(&self) -> Vec<MessageActionRef> {
        let mut out = Vec::new();
        for m in &self.mods {
            if m.is_trapped() || !m.grants.has(Capability::UiContextMenu) {
                continue;
            }
            for a in &m.context_actions {
                out.push(MessageActionRef {
                    mod_id: m.id.clone(),
                    mod_name: m.name.clone(),
                    action_id: a.id.clone(),
                    label: a.label.clone(),
                    icon: a.icon.clone(),
                });
            }
        }
        out
    }

    /// The message actions of the loaded mods that hold `ui.message_action` (the
    /// ones the front ends render), in load order.
    pub fn message_actions(&self) -> Vec<MessageActionRef> {
        let mut out = Vec::new();
        for m in &self.mods {
            if m.is_trapped() || !m.grants.has(Capability::UiMessageAction) {
                continue;
            }
            for a in &m.actions {
                out.push(MessageActionRef {
                    mod_id: m.id.clone(),
                    mod_name: m.name.clone(),
                    action_id: a.id.clone(),
                    label: a.label.clone(),
                    icon: a.icon.clone(),
                });
            }
        }
        out
    }

    /// The theme overrides of the loaded mods that hold `ui.theme`, in load
    /// order (later entries win). Empty when none.
    pub fn theme(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for m in &self.mods {
            if m.is_trapped() || !m.grants.has(Capability::UiTheme) {
                continue;
            }
            out.extend(m.theme.entries.iter().cloned());
        }
        out
    }

    /// Delivers a channel event (`channel.opened`/`channel.updated`) to every
    /// mod that subscribed to it and holds `channels.list`. Returns how many mods
    /// it reached (0 when none are loaded or subscribed).
    pub fn channel_event(&self, event: events::Event, channel: &str) -> usize {
        let mut delivered = 0;
        for m in &self.mods {
            if m.is_trapped() || !m.subscribes(event) || !m.grants.has(event.capability()) {
                continue;
            }
            let ran = m.request(|reply| Job::Event {
                event_name: event.name().to_string(),
                channel: channel.to_string(),
                reply,
            });
            if ran == Some(true) {
                delivered += 1;
            }
        }
        delivered
    }

    /// Delivers a reaction (`reaction.added`/`reaction.removed`) to every mod
    /// that subscribed to it and holds `messages.read`, within scope. A reaction
    /// has no writable target (`decorate` does nothing here); the reacted message
    /// is served by `get-message` when the front end knows it. Returns how many
    /// mods it reached.
    pub fn reaction_event(
        &self,
        event: events::Event,
        channel: &str,
        ts: &str,
        name: &str,
        user: &str,
        added: bool,
        content: Option<Msg>,
    ) -> usize {
        let key = (channel.to_string(), ts.to_string());
        let index: Arc<HashMap<(String, String), Msg>> =
            Arc::new(content.into_iter().map(|m| (key.clone(), m)).collect());
        let mut delivered = 0;
        for m in &self.mods {
            // Like every message-scoped path: only a subscribed, granted mod,
            // inside its read scope.
            if m.is_trapped()
                || !m.subscribes(event)
                || !m.grants.has(event.capability())
                || !m.grants.can_read(channel)
            {
                continue;
            }
            let ran = m.request(|reply| Job::Reaction {
                channel: channel.to_string(),
                ts: ts.to_string(),
                emoji: name.to_string(),
                user: user.to_string(),
                added,
                index: index.clone(),
                reply,
            });
            if ran == Some(true) {
                delivered += 1;
            }
        }
        delivered
    }

    /// The slash commands of the loaded mods that hold `commands.slash`, in load
    /// order (the front ends offer them).
    pub fn commands(&self) -> Vec<CommandRef> {
        let mut out = Vec::new();
        for m in &self.mods {
            if m.is_trapped() || !m.grants.has(Capability::CommandsSlash) {
                continue;
            }
            for c in &m.commands {
                out.push(CommandRef {
                    mod_id: m.id.clone(),
                    mod_name: m.name.clone(),
                    name: c.name.clone(),
                    label: c.label.clone(),
                    description: c.description.clone(),
                });
            }
        }
        out
    }

    /// Invokes the slash command `name` with `args` in `channel`: every mod that
    /// declared it and holds `commands.slash` receives `on-command`. Returns how
    /// many it reached (0 when no loaded mod declares the command).
    pub fn invoke_command(&self, name: &str, args: &str, channel: &str) -> usize {
        let mut delivered = 0;
        for m in &self.mods {
            if m.is_trapped()
                || !m.declares_command(name)
                || !m.grants.has(Capability::CommandsSlash)
            {
                continue;
            }
            let ran = m.request(|reply| Job::Command {
                name: name.to_string(),
                args: args.to_string(),
                channel: channel.to_string(),
                reply,
            });
            if ran == Some(true) {
                delivered += 1;
            }
        }
        delivered
    }

    /// Drains the pending dialogs the loaded mods asked for (`ui-show-dialog` /
    /// `ui-close-dialog`), in load order. The front end applies them and owns the
    /// modal's lifetime.
    pub fn take_dialog_effects(&self) -> Vec<(String, DialogEffect)> {
        let mut out = Vec::new();
        for m in &self.mods {
            if let Some(Some(effect)) = m.request(|reply| Job::TakeDialogEffects { reply }) {
                out.push((m.id.clone(), effect));
            }
        }
        out
    }

    /// Delivers an action click on a dialog a mod showed: calls the owning mod's
    /// `on-dialog-action(id)`. Returns whether the mod was live, held `ui.dialog`
    /// and had offered that action id (so the caller knows the click was routed).
    pub fn invoke_dialog_action(&self, mod_id: &str, action_id: &str) -> bool {
        // The coordinator matches the mod by id and liveness; the worker gates on
        // `ui.dialog` + whether it actually offered this action id (store state),
        // and reports whether the click was routed.
        let Some(m) = self.mods.iter().find(|m| m.id == mod_id && !m.is_trapped()) else {
            return false;
        };
        m.request(|reply| Job::DialogAction {
            action_id: action_id.to_string(),
            reply,
        })
        .unwrap_or(false)
    }

    /// The composer actions of the loaded mods that hold `ui.composer_action`, in
    /// load order (the front ends render them by the composer).
    pub fn composer_actions(&self) -> Vec<ComposerActionRef> {
        let mut out = Vec::new();
        for m in &self.mods {
            if m.is_trapped() || !m.grants.has(Capability::UiComposerAction) {
                continue;
            }
            for a in &m.composer_actions {
                out.push(ComposerActionRef {
                    mod_id: m.id.clone(),
                    mod_name: m.name.clone(),
                    action_id: a.id.clone(),
                    label: a.label.clone(),
                    icon: a.icon.clone(),
                });
            }
        }
        out
    }

    /// Runs one composer action: the guest gets `on-composer-action` with the
    /// composer `draft` and may call `ui-set-composer-text`. Returns the text it
    /// set (if any) for the front end to put back in the composer; `None` for an
    /// unknown/un-granted mod or a trap.
    pub fn invoke_composer_action(
        &self,
        mod_id: &str,
        action_id: &str,
        channel: &str,
        draft: &str,
    ) -> Option<String> {
        let m = self.mods.iter().find(|m| {
            !m.is_trapped()
                && m.id == mod_id
                && m.declares_composer_action(action_id)
                && m.grants.has(Capability::UiComposerAction)
        })?;
        // The draft comes from the conversation the user acted in: bound it to the
        // mod's read scope, like `invoke-action`/`chat-send`.
        if !m.grants.can_read(channel) {
            return None;
        }
        m.request(|reply| Job::ComposerAction {
            action_id: action_id.to_string(),
            channel: channel.to_string(),
            draft: draft.to_string(),
            reply,
        })
        .flatten()
    }

    /// The views of the loaded mods that hold `ui.view`, in load order.
    pub fn views(&self) -> Vec<ViewRef> {
        let mut out = Vec::new();
        for m in &self.mods {
            if m.is_trapped() || !m.grants.has(Capability::UiView) {
                continue;
            }
            for v in &m.views {
                out.push(ViewRef {
                    mod_id: m.id.clone(),
                    mod_name: m.name.clone(),
                    view_id: v.id.clone(),
                    label: v.label.clone(),
                    icon: v.icon.clone(),
                });
            }
        }
        out
    }

    /// The content a mod pushed for one of its views (`(kind, text)` blocks).
    pub fn view_content(&self, mod_id: &str, view_id: &str) -> Vec<(String, String)> {
        self.mods
            .iter()
            .find(|m| m.id == mod_id)
            .and_then(|m| {
                m.request(|reply| Job::ViewContent {
                    view_id: view_id.to_string(),
                    reply,
                })
            })
            .unwrap_or_default()
    }

    /// Invokes a message action **or a context-menu item**: calls the owning mod's
    /// `on-action` with the message current, then returns the message's
    /// decorations from the cache (all mods', with this mod's updated). `None` when
    /// the mod does not hold the matching capability (`ui.message_action` /
    /// `ui.context_menu`), is already quarantined, is outside its `messages.read`
    /// scope for `channel`, or never declared that action. A mod that traps
    /// *during* the call is quarantined and its previous decoration is kept.
    pub fn invoke_action(
        &self,
        mod_id: &str,
        action_id: &str,
        channel: &str,
        ts: &str,
        msg: &Msg,
    ) -> Option<Vec<String>> {
        let key = (channel.to_string(), ts.to_string());
        // Give the handler the target message, so `get-message` can serve it.
        let index: Arc<HashMap<(String, String), Msg>> =
            Arc::new(std::iter::once((key.clone(), msg.clone())).collect());
        let Some(m) = self.mods.iter().find(|m| {
            m.id == mod_id
                && !m.is_trapped()
                // The id may be declared as a message action and/or a context-menu
                // item; honor it when the matching capability covers the list it
                // came from (an id in both needs either).
                && ((m.actions.iter().any(|a| a.id == action_id)
                    && m.grants.has(Capability::UiMessageAction))
                    || (m.context_actions.iter().any(|a| a.id == action_id)
                        && m.grants.has(Capability::UiContextMenu)))
                // Consistent with `run_pass`: a mod may only act on a channel in
                // its read scope (`decorate` has no scope check of its own).
                && m.grants.can_read(channel)
        }) else {
            return None;
        };
        let owner = m.id.clone();
        let outcome: Option<Vec<String>> = m
            .request(|reply| Job::Action {
                action_id: action_id.to_string(),
                channel: channel.to_string(),
                ts: ts.to_string(),
                index,
                reply,
            })
            .flatten();
        // Replace only this mod's decorations for the message (other mods'
        // survive). On a guest error/trap the cache is left unchanged; a
        // too-large output keeps the old decoration.
        let mut cache = self.cache();
        if let Some(texts) = outcome {
            let bytes: usize = texts.iter().map(String::len).sum();
            if texts.is_empty() || bytes <= MAX_CACHED_BYTES {
                cache.retract_decorated_mod(&key, &owner);
                let tagged = texts.into_iter().map(|t| (owner.clone(), t)).collect();
                cache.store_decorations(&key, tagged);
            }
        }
        Some(
            cache
                .decorated
                .get(&key)
                .map(|v| v.iter().map(|(_, t)| t.clone()).collect())
                .unwrap_or_default(),
        )
    }

    /// Whether any of `msgs` still needs (re)running. A single dispatch now runs
    /// the whole window, so this is normally false afterwards; it is retained so a
    /// caller can still confirm a dispatch left nothing behind.
    pub fn has_pending(&self, channel: &str, msgs: &[Msg]) -> bool {
        let cache = self.cache();
        msgs.iter().any(|m| {
            let key = (channel.to_string(), m.ts.clone());
            cache.seen.get(&key) != Some(&content_hash(&m.author, &m.text))
        })
    }

    /// Loads the installed mods from `root`, building the engine, with an empty
    /// directory. Grants come from the persistent consent record (default deny).
    pub fn from_dir(root: &Path) -> Result<Host> {
        Self::from_dir_with(root, &Arc::new(Directory::empty()))
    }

    /// Like [`Host::from_dir`], with the client's `directory` available from load
    /// (so a directory-only mod can use it in `start`).
    pub fn from_dir_with(root: &Path, directory: &Arc<Directory>) -> Result<Host> {
        let consent = Consent::load_default();
        // A missing/unresolvable data home must not disable the mods: fall back to
        // an in-memory base so they still load — but say so, the downgrade is
        // otherwise invisible.
        let data_base = match crate::mods::mod_data_base() {
            Ok(base) => base,
            Err(e) => {
                tracing::warn!(
                    "mods: no data home ({}); the KV/vector and SQL stores will not persist",
                    sanitize_log(&e.to_string())
                );
                PathBuf::new()
            }
        };
        Ok(Self::load_root(
            root,
            engine()?,
            &consent,
            directory,
            &data_base,
        ))
    }

    /// Installs the Slack API client the `Write` host functions need
    /// (`reactions.write` today). Called by the app once at load; when it is
    /// never installed, a write fails closed.
    pub fn set_api(&self, api: Arc<crate::api::Client>) {
        for m in &self.mods {
            let _ = m.request(|reply| Job::SetApi {
                api: api.clone(),
                reply,
            });
        }
    }
}

impl Cache {
    /// Drops a key's decoration (its FIFO node becomes stale and is skipped).
    fn retract_decorated(&mut self, key: &(String, String)) {
        self.decorated_seq.remove(key);
        if let Some(v) = self.decorated.remove(key) {
            self.decorated_bytes = self
                .decorated_bytes
                .saturating_sub(v.iter().map(|(_, t)| t.len()).sum());
        }
    }

    /// Drops only the decorations `mod_id` contributed for `key` (other mods'
    /// output survives); used when a single mod re-runs via a message action.
    fn retract_decorated_mod(&mut self, key: &(String, String), mod_id: &str) {
        let mut removed = 0usize;
        if let Some(v) = self.decorated.get_mut(key) {
            removed = v
                .iter()
                .filter(|(m, _)| m == mod_id)
                .map(|(_, t)| t.len())
                .sum();
            v.retain(|(m, _)| m != mod_id);
        }
        self.decorated_bytes = self.decorated_bytes.saturating_sub(removed);
        if self
            .decorated
            .get(key)
            .map(|v| v.is_empty())
            .unwrap_or(false)
        {
            self.decorated.remove(key);
            self.decorated_seq.remove(key);
        }
    }

    /// Caches a message's decorations (already tagged by mod), applying the same
    /// FIFO eviction and `decorated_order` compaction as `run_pass`. Returns
    /// whether they fit and were stored. The caller frees the previous output
    /// first (see `retract_decorated`/`retract_decorated_mod`).
    fn store_decorations(&mut self, key: &(String, String), tagged: Vec<(String, String)>) -> bool {
        let bytes: usize = tagged.iter().map(|(_, t)| t.len()).sum();
        if tagged.is_empty() {
            return false;
        }
        // Best-effort FIFO eviction (stale nodes are skipped, amortized O(1)).
        while self.decorated_bytes + bytes > MAX_CACHED_BYTES {
            let Some((k, seq)) = self.decorated_order.pop_front() else {
                break;
            };
            // Never evict the key we are writing: it holds other mods' output.
            if k == *key {
                continue;
            }
            if self.decorated_seq.get(&k) == Some(&seq) {
                self.decorated_seq.remove(&k);
                if let Some(v) = self.decorated.remove(&k) {
                    self.decorated_bytes = self
                        .decorated_bytes
                        .saturating_sub(v.iter().map(|(_, t)| t.len()).sum());
                }
            }
        }
        if self.decorated_bytes + bytes > MAX_CACHED_BYTES {
            return false;
        }
        self.decorated_bytes += bytes;
        self.decorated_seq_counter += 1;
        let seq = self.decorated_seq_counter;
        self.decorated_seq.insert(key.clone(), seq);
        self.decorated_order.push_back((key.clone(), seq));
        self.decorated
            .entry(key.clone())
            .or_default()
            .extend(tagged);
        if self.decorated_order.len() > self.decorated.len().saturating_mul(4).saturating_add(64) {
            let seqs = &self.decorated_seq;
            self.decorated_order.retain(|(k, s)| seqs.get(k) == Some(s));
        }
        true
    }

    fn compact_seen_order(&mut self) {
        if self.seen_order.len() > self.seen.len().saturating_mul(4).saturating_add(64) {
            let seq = &self.seen_seq;
            self.seen_order.retain(|(k, s)| seq.get(k) == Some(s));
        }
    }

    /// Keeps delivery history bounded: the oldest entries (and their cached
    /// decorations) are dropped, so a long session does not grow without bound.
    /// Handlers should be idempotent — a forgotten message dispatched again
    /// re-runs `on-message` (there is no cross-eviction exactly-once guarantee).
    fn evict_seen(&mut self) {
        while self.seen.len() > self.cache_cap {
            let Some((k, seq)) = self.seen_order.pop_front() else {
                break;
            };
            if self.seen_seq.get(&k) != Some(&seq) {
                continue; // stale node (superseded by a later delivery)
            }
            self.seen_seq.remove(&k);
            self.seen.remove(&k);
            self.retract_decorated(&k);
        }
    }
}

impl Host {
    /// One dispatch pass: runs every not-yet-seen message of `msgs` (new or
    /// edited) against each subscribed mod. Does not evict history (see
    /// [`Self::dispatch_batch`]). Takes the cache lock only for the brief
    /// bookkeeping, never while a guest runs.
    ///
    /// Concurrency: two passes may overlap (the app has no global dispatch lock).
    /// Each message is **claimed** — marked seen with its content hash, and its
    /// previous output retracted — under the same lock that decides it needs to
    /// run, *before* any guest runs, so an overlapping pass skips it: a message is
    /// never run twice by concurrent dispatches, and its decorations are never
    /// stored twice. Every `(message, mod)` job is then queued at once, so mods
    /// work in parallel (each in message order on its own thread) and a slow mod
    /// blocks only itself; replies are collected in a fixed order (message, then
    /// load order) so the output stays deterministic.
    fn run_pass(
        &self,
        channel: &str,
        msgs: &[Msg],
        directory: &Arc<Directory>,
        dispatch: u64,
    ) {
        // Messages to (re)run: new ones, or whose text changed (an edit).
        struct Pending<'a> {
            msg: &'a Msg,
            key: (String, String),
            is_new: bool,
            /// This pass's claim on `key`: storing is valid only while it is still
            /// the key's current claim (a later edit's pass supersedes it).
            claim: u64,
        }
        let to_run: Vec<Pending> = {
            let mut cache = self.cache();
            let mut claimed = Vec::new();
            for m in msgs {
                let key = (channel.to_string(), m.ts.clone());
                let hash = content_hash(&m.author, &m.text);
                if cache.seen.get(&key) == Some(&hash) {
                    continue;
                }
                // Absent from history is a creation; present with a new hash is
                // an edit — the two carry different `events` subscriptions.
                let is_new = !cache.seen.contains_key(&key);
                // Retract this message's previous output (an edit that now
                // produces nothing must not leave a stale decoration), then claim
                // it: always (re)append with a fresh per-delivery sequence, so the
                // FIFO position refreshes and the old node is uniquely stale (the
                // hash alone is not unique for an A→B→A edit).
                cache.retract_decorated(&key);
                cache.seen_seq_counter += 1;
                let seq = cache.seen_seq_counter;
                cache.seen.insert(key.clone(), hash);
                cache.seen_seq.insert(key.clone(), seq);
                cache.seen_order.push_back((key.clone(), seq));
                claimed.push(Pending {
                    msg: m,
                    key,
                    is_new,
                    claim: seq,
                });
            }
            cache.compact_seen_order();
            claimed
        };
        if to_run.is_empty() {
            return;
        }
        // Build one shared message index and hand it (by `Arc`) to every mod,
        // instead of cloning the conversation into each store.
        let index: Arc<HashMap<(String, String), Msg>> = Arc::new(
            msgs.iter()
                .map(|m| ((channel.to_string(), m.ts.clone()), m.clone()))
                .collect(),
        );
        // Queue every job up front (no lock held): each mod's worker drains its
        // own messages in order while the others do the same in parallel.
        let mut replies: Vec<Vec<(usize, std::sync::mpsc::Receiver<Vec<String>>)>> =
            Vec::with_capacity(to_run.len());
        for p in &to_run {
            let event = if p.is_new {
                events::Event::MessageCreated
            } else {
                events::Event::MessageUpdated
            };
            let mut per_msg = Vec::new();
            for (i, m) in self.mods.iter().enumerate() {
                // A passive event reaches only a subscribed, live mod with
                // `messages.read` in this channel's scope (content is fetched
                // later, where scope is checked again).
                if !m.subscribes(event) || m.is_trapped() || !m.grants.can_read(channel) {
                    continue;
                }
                if let Some(rx) = m.send_request(|reply| Job::Message {
                    channel: channel.to_string(),
                    ts: p.msg.ts.clone(),
                    index: index.clone(),
                    directory: directory.clone(),
                    dispatch,
                    reply,
                }) {
                    per_msg.push((i, rx));
                }
            }
            replies.push(per_msg);
        }
        // Collect in message order, then load order, applying the byte cap.
        let mut dispatch_bytes = 0usize;
        for (p, per_msg) in to_run.iter().zip(replies) {
            let mut tagged_all: Vec<(String, String)> = Vec::new();
            for (i, rx) in per_msg {
                let m = &self.mods[i];
                let texts = m.await_reply(rx).unwrap_or_default();
                let bytes: usize = texts.iter().map(String::len).sum();
                if !texts.is_empty() && dispatch_bytes + bytes <= MAX_DISPATCH_BYTES {
                    dispatch_bytes += bytes;
                    // Tag each text with the mod that produced it, so a later
                    // action can replace only this mod's.
                    tagged_all.extend(texts.into_iter().map(|t| (m.id.clone(), t)));
                }
            }
            let mut cache = self.cache();
            // A newer pass claimed the key (an edit) or history evicted it: this
            // output is stale — drop it rather than append it to the newer one's.
            if cache.seen_seq.get(&p.key) != Some(&p.claim) {
                continue;
            }
            // The claim retracted the key, so any output already there came from
            // a gesture after it (`invoke_action`): newer than this pass's — keep it.
            if let Some(existing) = cache.decorated.get(&p.key) {
                tagged_all.retain(|(mid, _)| !existing.iter().any(|(m, _)| m == mid));
            }
            if !tagged_all.is_empty() {
                cache.store_decorations(&p.key, tagged_all);
            }
        }
    }

    /// The cached decorations for `msgs` (not only what ran in the last pass),
    /// keyed by `(channel, ts)` so conversations cannot collide.
    fn cached_for(&self, channel: &str, msgs: &[Msg]) -> HashMap<(String, String), Vec<String>> {
        let cache = self.cache();
        let mut out: HashMap<(String, String), Vec<String>> = HashMap::new();
        for m in msgs {
            let key = (channel.to_string(), m.ts.clone());
            if let Some(v) = cache.decorated.get(&key).filter(|v| !v.is_empty()) {
                out.insert(key, v.iter().map(|(_, t)| t.clone()).collect());
            }
        }
        out
    }

    /// A fresh dispatch id (see [`DispatchBudgets`]).
    fn next_dispatch(&self) -> u64 {
        self.dispatch_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1)
    }

    /// One pass (used by tests and the single-message CLI runner). Eviction is
    /// deliberately not done here; see [`Self::dispatch_batch`].
    fn decorate(&self, channel: &str, msgs: &[Msg]) -> HashMap<(String, String), Vec<String>> {
        let dispatch = self.next_dispatch();
        self.run_pass(channel, msgs, &Arc::new(Directory::empty()), dispatch);
        self.cached_for(channel, msgs)
    }

    /// [`Self::dispatch_batch`] with an empty directory (tests, the CLI runner).
    pub fn dispatch_batch(
        &self,
        channel: &str,
        msgs: &[Msg],
    ) -> (HashMap<(String, String), Vec<String>>, bool) {
        self.dispatch_batch_with(channel, msgs, &Arc::new(Directory::empty()))
    }

    /// Dispatches the not-yet-seen messages of `msgs` in a single pass, then
    /// applies the history cap once. Returns the cached set and whether some
    /// messages are still pending — now a pass runs the whole window this is
    /// effectively always false, but the flag is kept for the caller that
    /// schedules follow-ups. Eviction happens only after the batch, so a message
    /// dispatched in this batch is never re-run by cache pressure within it.
    /// `directory` is what `list-channels`/`list-users` will return.
    pub fn dispatch_batch_with(
        &self,
        channel: &str,
        msgs: &[Msg],
        directory: &Arc<Directory>,
    ) -> (HashMap<(String, String), Vec<String>>, bool) {
        // A fresh `notify-post`/API-write budget per dispatch (bounded in
        // `notify_post`), so a burst of messages cannot spawn a thread per message.
        // Each worker keys the budget by this id, so concurrent dispatches do not
        // reset each other's tallies.
        let dispatch = self.next_dispatch();
        self.run_pass(channel, msgs, directory, dispatch);
        self.cache().evict_seen();
        let pending = self.has_pending(channel, msgs);
        (self.cached_for(channel, msgs), pending)
    }
}

/// Instantiates a mod and runs it once against a single synthetic message,
/// returning the decorations it attached. `granted` is the capability set the
/// one-shot run uses (the CLI passes an explicit grant).
pub fn run_message(
    dir: &Path,
    channel: &str,
    ts: &str,
    author: &str,
    text: &str,
    granted: &BTreeSet<Capability>,
) -> Result<RunOutcome> {
    run_message_with_fs(dir, channel, ts, author, text, granted, &BTreeMap::new())
}

/// Like [`run_message`], with an explicit `filesystem` slot mapping for the
/// one-shot run (`slaq mod --fs-slot NAME=DIR`). The mapping only takes effect
/// when `filesystem` is granted; an empty map is the no-mapping case.
pub fn run_message_with_fs(
    dir: &Path,
    channel: &str,
    ts: &str,
    author: &str,
    text: &str,
    granted: &BTreeSet<Capability>,
    fs_slots: &BTreeMap<String, (PathBuf, fs::FsAccess)>,
) -> Result<RunOutcome> {
    let mut host = Host::load_one_with_fs(dir, engine()?, granted, fs_slots)?;
    let (decorations, _pending) = host.dispatch_batch(channel, &[Msg::text_only(ts, author, text)]);
    let (id, name) = host
        .mods
        .first()
        .map(|m| (m.id.clone(), m.name.clone()))
        .unwrap_or_default();
    Ok(RunOutcome {
        id,
        name,
        decorations: decorations
            .into_iter()
            .flat_map(|((c, ts), texts)| texts.into_iter().map(move |t| (c.clone(), ts.clone(), t)))
            .collect(),
    })
}

/// One installed package as read for the consent prompt — manifest only, no
/// compilation.
pub struct ScannedMod {
    pub id: String,
    pub name: String,
    /// The implemented capabilities the manifest requests (display order).
    pub requested: Vec<Capability>,
    /// The capabilities currently granted (consent ∩ request).
    pub granted: BTreeSet<Capability>,
    /// The manifest's declared read scope (only when `messages.read` is asked).
    pub read_scope: Option<ReadScope>,
    /// The read scope actually enforced today (`min(consent, manifest)`), if any;
    /// what an already-granted `messages.read` really covers.
    pub effective_scope: Option<ReadScope>,
    /// The manifest's `net.egress` allow-list (empty when not requested).
    pub egress: Vec<String>,
    /// The manifest hosts the user allows (consent ∩ manifest), whether or not
    /// `net.egress` is granted right now.
    pub egress_allowed: Vec<String>,
    /// The `net.egress` hosts the user added beyond the manifest.
    pub egress_added: Vec<String>,
    /// Whether the user has ever reviewed this manifest's egress list (an
    /// unreviewed host defaults to allowed in the dialog, as before).
    pub egress_reviewed: Vec<String>,
    /// The channels the user chose for `messages.read`, if any.
    pub user_scope: Option<ReadScope>,
    /// The capabilities recorded in the store, before the effective checks
    /// (`granted` drops one that reaches nothing; this keeps it).
    pub stored: BTreeSet<Capability>,
    /// The manifest's declared `filesystem` slots (empty when not requested).
    pub fs_slots: Vec<fs::FsSlot>,
}

/// Reads every installed package under `root` (like [`Host::load_root`], but
/// without compiling) and reports what each requests and holds. Invalid packages
/// are logged and skipped. The consent prompt uses this.
pub fn scan(root: &Path, consent: &Consent) -> Vec<ScannedMod> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join("manifest.json").is_file())
        .collect();
    dirs.sort();
    if dirs.len() > MAX_MODS {
        dirs.truncate(MAX_MODS);
    }
    // Reject duplicate ids like the loader does: one dialog entry per id, or an
    // apply would grant/revoke the same id twice.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for dir in dirs {
        match load(&dir) {
            Ok(loaded) => {
                let id = loaded.manifest.id;
                if !seen.insert(id.clone()) {
                    tracing::warn!("mod skipped ({}): duplicate id", sanitize(&id));
                    continue;
                }
                let requested: Vec<Capability> = loaded.requested.caps().iter().copied().collect();
                let entry = consent.entry(&id);
                let effective_scope =
                    Grants::effective_read_scope(&loaded.requested, &consent.entry(&id));
                let mut granted: BTreeSet<Capability> = consent
                    .granted(&id)
                    .intersection(loaded.requested.caps())
                    .copied()
                    .collect();
                // A recorded `messages.read` whose scope no longer intersects the
                // manifest is dropped at runtime; report it the same way.
                if effective_scope.is_none() {
                    granted.remove(&Capability::MessagesRead);
                }
                // The same for `net.egress`: if the consented hosts and the
                // manifest's list are disjoint, nothing is actually reachable.
                let effective_egress =
                    Grants::effective_egress(&loaded.requested, &consent.entry(&id));
                if !loaded.requested.egress().is_empty() && effective_egress.is_empty() {
                    granted.remove(&Capability::NetEgress);
                }
                // The same for `filesystem`: a grant whose slots the user never
                // mapped reaches nothing, so it is dropped at runtime; report it
                // the same way.
                let any_slot_mapped = loaded
                    .requested
                    .fs_slots()
                    .iter()
                    .any(|s| consent.entry(&id).fs_slots().contains_key(&s.name));
                if !loaded.requested.fs_slots().is_empty() && !any_slot_mapped {
                    granted.remove(&Capability::Filesystem);
                }
                let read_scope = loaded
                    .requested
                    .caps()
                    .contains(&Capability::MessagesRead)
                    .then(|| loaded.requested.read_scope().clone());
                out.push(ScannedMod {
                    id,
                    name: loaded.manifest.name,
                    requested,
                    granted,
                    read_scope,
                    effective_scope,
                    egress: loaded.requested.egress().to_vec(),
                    egress_allowed: entry
                        .egress()
                        .iter()
                        .filter(|h| loaded.requested.egress().contains(h))
                        .cloned()
                        .collect(),
                    egress_added: entry.egress_added().to_vec(),
                    egress_reviewed: entry.egress_reviewed().to_vec(),
                    user_scope: entry.user_scope().cloned(),
                    stored: entry.caps(),
                    fs_slots: loaded.requested.fs_slots().to_vec(),
                });
            }
            Err(e) => tracing::warn!(
                "mod skipped ({}): {}",
                sanitize(&dir.display().to_string()),
                sanitize_log(&e.to_string())
            ),
        }
    }
    out
}

#[cfg(all(test, feature = "mods"))]
mod tests {
    use super::load;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;

    /// Every capability this host implements.
    fn all_caps() -> BTreeSet<super::Capability> {
        super::Capability::ALL.into_iter().collect()
    }

    /// A consent record granting every implemented capability (read scope: all)
    /// to `ids`.
    fn consent_all(ids: &[&str]) -> super::Consent {
        let mut c = super::Consent::default();
        for id in ids {
            c.grant(
                id,
                &super::Capability::ALL,
                Some(super::capability::ReadScope::All),
            );
        }
        c
    }

    /// A consent record granting the three implemented capabilities to `id`, with
    /// `messages.read` scoped to `channels`.
    fn consent_scoped(id: &str, channels: &[&str]) -> super::Consent {
        let mut c = super::Consent::default();
        c.grant(
            id,
            &[
                super::Capability::MessagesRead,
                super::Capability::StoreKv,
                super::Capability::UiMessageDecoration,
            ],
            Some(super::capability::ReadScope::Channels {
                channels: channels.iter().map(|s| s.to_string()).collect(),
            }),
        );
        c
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("slaq-mod-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn manifest(dir: &PathBuf, entry: &str) {
        fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"{entry}"}},"capabilities":{{}}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn accepts_a_package_relative_entry() {
        let d = tmp("ok");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        manifest(&d, "mod.wasm");
        assert!(load(&d).is_ok());
    }

    #[test]
    fn rejects_absolute_and_parent_paths() {
        let d = tmp("escape");
        manifest(&d, "/etc/hostname");
        assert!(load(&d).is_err(), "absolute path must be rejected");
        manifest(&d, "../evil.wasm");
        assert!(load(&d).is_err(), "parent traversal must be rejected");
    }

    #[test]
    fn rejects_an_unsupported_api_version() {
        let d = tmp("api");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"9.9","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{}}"#,
        )
        .unwrap();
        assert!(
            load(&d).is_err(),
            "an unsupported apiVersion must be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_that_escapes() {
        use std::os::unix::fs::symlink;
        let d = tmp("symlink-out");
        let outside = tmp("symlink-out-target");
        fs::write(outside.join("evil.wasm"), b"\0asm").unwrap();
        symlink(outside.join("evil.wasm"), d.join("mod.wasm")).unwrap();
        manifest(&d, "mod.wasm");
        assert!(
            load(&d).is_err(),
            "a symlink escaping the package must be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn accepts_a_symlink_inside_the_package() {
        use std::os::unix::fs::symlink;
        let d = tmp("symlink-in");
        fs::write(d.join("real.wasm"), b"\0asm").unwrap();
        symlink(d.join("real.wasm"), d.join("mod.wasm")).unwrap();
        manifest(&d, "mod.wasm");
        assert!(
            load(&d).is_ok(),
            "a symlink resolving inside the package is fine"
        );
    }

    /// End to end: instantiate the checked-in fixture component, call
    /// `start`/`on-message`, and see the host calls it used.
    #[test]
    fn runs_a_real_component_end_to_end() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let out =
            super::run_message(&dir, "C0T", "1.000000", "U0", "hi", &all_caps()).expect("run");
        assert!(
            out.decorations
                .iter()
                .any(|(_, _, t)| t.contains("hello #1")),
            "decorations: {:?}",
            out.decorations
        );
    }

    /// End to end through the `filesystem` capability: the docs fixture searches
    /// a mapped slot for the message text and decorates the first hit; with no
    /// mapping it fails closed and decorates nothing.
    #[test]
    fn the_docs_example_searches_a_mapped_slot() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/docs-mod");
        let caps: BTreeSet<super::Capability> = [
            super::Capability::MessagesRead,
            super::Capability::Filesystem,
            super::Capability::UiMessageDecoration,
        ]
        .into_iter()
        .collect();

        // A hermetic slot directory (no network, no clock, no `$HOME`).
        let slot = std::env::temp_dir().join(format!("slaq-docs-mod-{}", std::process::id()));
        let _ = fs::remove_dir_all(&slot);
        fs::create_dir_all(&slot).unwrap();
        fs::write(slot.join("guide.txt"), "hello world\nneedle here\n").unwrap();

        let mut map: std::collections::BTreeMap<String, (PathBuf, super::fs::FsAccess)> =
            std::collections::BTreeMap::new();
        map.insert(
            "docs".to_string(),
            (slot.clone(), super::fs::FsAccess::Read),
        );

        let out = super::run_message_with_fs(&dir, "C0T", "1.000000", "U0", "needle", &caps, &map)
            .expect("run");
        assert!(
            out.decorations
                .iter()
                .any(|(_, _, t)| t.contains("guide.txt") && t.contains(":2")),
            "a mapped slot must decorate the hit with its line: {:?}",
            out.decorations
        );

        // Fail closed: without a mapping the same run decorates nothing.
        let empty = std::collections::BTreeMap::new();
        let out =
            super::run_message_with_fs(&dir, "C0T", "1.000000", "U0", "needle", &caps, &empty)
                .expect("run");
        assert!(
            out.decorations.is_empty(),
            "an unmapped slot must not decorate: {:?}",
            out.decorations
        );

        let _ = fs::remove_dir_all(&slot);
    }

    /// With two mods, a message runs every mod exactly once and is then cached:
    /// a second dispatch (a reload) must not re-run them (the counter stays #1).
    #[test]
    fn dispatch_runs_each_mod_once_and_caches() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let root = tmp("two-mods");
        for name in ["a", "b"] {
            let d = root.join(name);
            fs::create_dir_all(&d).unwrap();
            for f in ["manifest.json", "mod.wasm"] {
                fs::copy(fixture.join(f), d.join(f)).unwrap();
            }
            // Distinct ids: duplicate ids are rejected at load.
            fs::write(
                d.join("manifest.json"),
                format!(
                    r#"{{"manifestVersion":0,"id":"dev.slaq.hello.{name}","name":"hello {name}","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{{"messages.read":{{"scope":"all"}},"store.kv":{{"maxBytes":1048576}},"ui.message_decoration":{{}}}},"events":["message.created","message.updated"]}}"#
                ),
            )
            .unwrap();
        }
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.hello.a", "dev.slaq.hello.b"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(host.mods.len(), 2, "two mods must load");
        let msgs = vec![super::Msg::text_only("1.000000", "U0", "hi")];

        let _ = host.decorate("C0T", &msgs);
        let reload = host.decorate("C0T", &msgs);

        let texts: Vec<&String> = reload.values().flatten().collect();
        assert_eq!(texts.len(), 2, "one decoration per mod: {reload:?}");
        assert!(
            texts.iter().all(|t| t.contains("#1")),
            "a mod ran twice on a reload: {texts:?}"
        );
    }

    /// A mod that declares a message action is offered it, and invoking it calls
    /// `on-action` (the fixture decorates the target message).
    #[test]
    fn a_message_action_is_rendered_and_invoked() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("action");
        let d = root.join("m");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.action","name":"Action","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"ui.message_action":{},"ui.message_decoration":{}},"ui":{"messageActions":[{"id":"ping","label":"Ping"}]}}"#,
        )
        .unwrap();
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.action"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        // The action is offered (raw ids are the routing keys).
        let actions = host.message_actions();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_id, "ping");
        assert_eq!(actions[0].label, "Ping");
        // Invoking runs `on-action`, which decorates the target message.
        let out = host
            .invoke_action(
                "dev.slaq.action",
                "ping",
                "C0T",
                "1.0",
                &super::Msg::text_only("1.0", "U0", "hi"),
            )
            .expect("a declared action");
        assert!(
            out.iter().any(|t| t.contains("hello action: ping")),
            "on-action decoration missing: {out:?}"
        );
        // An action the mod never declared is refused.
        assert!(
            host.invoke_action(
                "dev.slaq.action",
                "nope",
                "C0T",
                "1.0",
                &super::Msg::text_only("1.0", "U0", "hi")
            )
            .is_none()
        );
    }

    /// `parse_message_actions` refuses duplicate/invalid ids, an empty/oversized
    /// label, unknown action keys, and any unsupported `ui.*` key.
    #[test]
    fn rejects_bad_ui_items() {
        let ok = |ui: serde_json::Value| super::parse_ui(&ui);
        assert!(ok(serde_json::json!({"messageActions":[{"id":"a","label":"A"},{"id":"a","label":"B"}]})).is_err(), "duplicate id");
        assert!(
            ok(serde_json::json!({"messageActions":[{"id":"1bad","label":"A"}]})).is_err(),
            "invalid id"
        );
        assert!(
            ok(serde_json::json!({"messageActions":[{"id":"a","label":""}]})).is_err(),
            "empty label"
        );
        assert!(
            ok(serde_json::json!({"messageActions":[{"id":"a","label":"A","nope":1}]})).is_err(),
            "unknown action key"
        );
        assert!(
            ok(serde_json::json!({"dialog":[{"id":"c","label":"C"}]})).is_err(),
            "unsupported ui key"
        );
        assert!(
            ok(serde_json::json!({"views":[{"id":"v","label":"V"},{"id":"v","label":"W"}]}))
                .is_err(),
            "duplicate view id"
        );
        assert!(
            ok(serde_json::json!({"views":"nope"})).is_err(),
            "not an array"
        );
        let ui = ok(serde_json::json!({
            "messageActions":[{"id":"a","label":"A","icon":"🔤"}],
            "views":[{"id":"v","label":"V"}],
            "composerActions":[{"id":"c","label":"C"}],
            "contextMenu":[{"id":"m","label":"M"}]
        }))
        .unwrap();
        assert_eq!(ui.actions.len(), 1);
        assert_eq!(ui.actions[0].icon.as_deref(), Some("🔤"));
        assert_eq!(ui.views.len(), 1);
        assert_eq!(ui.views[0].id, "v");
        assert_eq!(ui.composer_actions.len(), 1);
        assert_eq!(ui.composer_actions[0].id, "c");
        assert_eq!(ui.context_actions.len(), 1);
        assert_eq!(ui.context_actions[0].id, "m");
    }

    /// `ui-set-composer-text` is denied without the capability and outside a
    /// composer-action callback.
    #[test]
    fn ui_set_composer_text_is_gated_and_scoped_to_the_callback() {
        use super::slaq::api::host::Host as _;
        let mut none = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(
            none.ui_set_composer_text("C0T".into(), "x".into())
                .unwrap_err()
                .contains("permission-denied")
        );
        // Granted, but no composer action is running: refused.
        let mut st = super::HostState::new(
            super::Grants::of(&[super::Capability::UiComposerAction]),
            Vec::new(),
        );
        assert!(
            st.ui_set_composer_text("C0T".into(), "x".into())
                .unwrap_err()
                .contains("only during a composer action")
        );
        // During an action, only its own channel is accepted, and the text is
        // capped.
        st.composer_action_channel = Some("C0T".to_string());
        assert!(st.ui_set_composer_text("C0T".into(), "x".into()).is_ok());
        assert!(
            st.ui_set_composer_text("OTHER".into(), "x".into())
                .unwrap_err()
                .contains("for that channel")
        );
        let long = "x".repeat(super::MAX_MESSAGE_CHARS + 1);
        assert!(
            st.ui_set_composer_text("C0T".into(), long)
                .unwrap_err()
                .contains("exceeds")
        );
    }

    /// A declared composer action runs the guest, which may replace the composer
    /// text (the fixture wraps the draft).
    #[test]
    fn a_composer_action_can_replace_the_composer_text() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("composer");
        let d = root.join("hello");
        fs::create_dir_all(&d).unwrap();
        for f in ["manifest.json", "mod.wasm"] {
            fs::copy(fixture.join(f), d.join(f)).unwrap();
        }
        // With the grant: the action is offered and running it returns the text.
        let mut consent = super::Consent::default();
        consent.grant(
            "dev.slaq.hello",
            &[
                super::Capability::UiComposerAction,
                super::Capability::MessagesRead,
            ],
            Some(super::ReadScope::All),
        );
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent,
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(host.composer_actions().len(), 1, "the fixture declares one");
        let out = host.invoke_composer_action("dev.slaq.hello", "wrap", "C0T", "ciao");
        assert_eq!(
            out.as_deref(),
            Some("[ciao]"),
            "the guest wrapped the draft"
        );
        // Without the grant it is not offered.
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &super::Consent::default(),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert!(host.composer_actions().is_empty());
    }

    /// A declared context-menu item is offered and invoked through the `action`
    /// callback (it reuses `invoke_action`).
    #[test]
    fn a_context_menu_item_runs_the_action_callback() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("ctx-menu");
        let d = root.join("hello");
        fs::create_dir_all(&d).unwrap();
        for f in ["manifest.json", "mod.wasm"] {
            fs::copy(fixture.join(f), d.join(f)).unwrap();
        }
        let mut consent = super::Consent::default();
        consent.grant(
            "dev.slaq.hello",
            &[
                super::Capability::UiContextMenu,
                super::Capability::MessagesRead,
            ],
            Some(super::ReadScope::All),
        );
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent,
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(host.context_actions().len(), 1, "the fixture declares one");
        assert!(
            host.invoke_action(
                "dev.slaq.hello",
                "pin",
                "C0T",
                "1.0",
                &super::Msg::text_only("1.0", "U0", "hi")
            )
            .is_some(),
            "the context item decoded"
        );
        // Without the grant it is neither offered nor delivered.
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &super::Consent::default(),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert!(host.context_actions().is_empty());
        assert!(
            host.invoke_action(
                "dev.slaq.hello",
                "pin",
                "C0T",
                "1.0",
                &super::Msg::text_only("1.0", "U0", "hi")
            )
            .is_none()
        );
        // Granted `ui.message_action` but not `ui.context_menu`: the item is still
        // not offered (and cannot be invoked).
        let mut consent = super::Consent::default();
        consent.grant(
            "dev.slaq.hello",
            &[
                super::Capability::UiMessageAction,
                super::Capability::MessagesRead,
            ],
            Some(super::ReadScope::All),
        );
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent,
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert!(host.context_actions().is_empty(), "wrong capability");
        assert!(
            host.invoke_action(
                "dev.slaq.hello",
                "pin",
                "C0T",
                "1.0",
                &super::Msg::text_only("1.0", "U0", "hi")
            )
            .is_none()
        );
    }

    /// An id declared as **both** a message action and a context item: with only
    /// `ui.context_menu` granted it is offered and invocable (the two gates OR).
    #[test]
    fn an_id_in_both_lists_honors_the_context_grant() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("ctx-both");
        let d = root.join("hello");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.hello","name":"hello","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"ui.message_action":{},"ui.context_menu":{},"ui.message_decoration":{}},"ui":{"messageActions":[{"id":"dup","label":"Dup"}],"contextMenu":[{"id":"dup","label":"Dup"}]}}"#,
        )
        .unwrap();
        let mut consent = super::Consent::default();
        consent.grant(
            "dev.slaq.hello",
            &[
                super::Capability::UiContextMenu,
                super::Capability::MessagesRead,
                super::Capability::UiMessageDecoration,
            ],
            Some(super::ReadScope::All),
        );
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent,
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(
            host.context_actions().len(),
            1,
            "offered under the context grant"
        );
        assert!(
            host.message_actions().is_empty(),
            "the message action is not granted"
        );
        assert!(
            host.invoke_action(
                "dev.slaq.hello",
                "dup",
                "C0T",
                "1.0",
                &super::Msg::text_only("1.0", "U0", "hi")
            )
            .is_some(),
            "the context grant covers the shared id"
        );
    }

    /// `ui-show-dialog`/`ui-close-dialog` require `ui.dialog`, bound their sizes
    /// and record a drainable effect.
    #[test]
    fn show_dialog_is_gated_and_bounded() {
        use super::slaq::api::host::Host as _;
        let mut denied = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(
            denied.ui_show_dialog("t".into(), vec![], vec![]).is_err(),
            "denied without ui.dialog"
        );
        assert!(
            denied.ui_close_dialog().is_err(),
            "close denied without ui.dialog"
        );
        let mut st = super::HostState::new(
            super::Grants::of(&[super::Capability::UiDialog]),
            Vec::new(),
        );
        st.ui_show_dialog(
            "Title".into(),
            vec![
                ("section".into(), "hi".into()),
                ("divider".into(), "ignored".into()),
            ],
            vec![("ok".into(), "OK".into())],
        )
        .unwrap();
        match st.pending_dialog.take().expect("a pending show") {
            super::DialogEffect::Show(d) => {
                assert_eq!(d.title, "Title");
                assert_eq!(
                    d.blocks,
                    vec![
                        ("section".to_string(), "hi".to_string()),
                        ("divider".to_string(), String::new())
                    ]
                );
                assert_eq!(d.actions, vec![("ok".to_string(), "OK".to_string())]);
            }
            other => panic!("expected Show, got {other:?}"),
        }
        assert_eq!(st.dialog_actions, vec!["ok".to_string()]);
        // Bounds: an unknown block kind, a bad/duplicate action id, empty label.
        assert!(
            st.ui_show_dialog("t".into(), vec![("bogus".into(), "x".into())], vec![])
                .is_err(),
            "unknown block kind"
        );
        assert!(
            st.ui_show_dialog("t".into(), vec![], vec![("1bad".into(), "L".into())])
                .is_err(),
            "invalid action id"
        );
        assert!(
            st.ui_show_dialog(
                "t".into(),
                vec![],
                vec![("ok".into(), "A".into()), ("ok".into(), "B".into())]
            )
            .is_err(),
            "duplicate action id"
        );
        assert!(
            st.ui_show_dialog("t".into(), vec![], vec![("ok".into(), String::new())])
                .is_err(),
            "empty label"
        );
        // Post-sanitize emptiness: an all-invisible title or label is refused.
        assert!(
            st.ui_show_dialog(String::new(), vec![], vec![]).is_err(),
            "empty title"
        );
        assert!(
            st.ui_show_dialog("\u{200b}".into(), vec![], vec![])
                .is_err(),
            "invisible title"
        );
        assert!(
            st.ui_show_dialog("t".into(), vec![], vec![("ok".into(), "\u{200b}".into())])
                .is_err(),
            "invisible label"
        );
        // Close records a close effect.
        st.ui_close_dialog().unwrap();
        assert_eq!(st.pending_dialog, Some(super::DialogEffect::Close));
    }

    /// A command can show a dialog (the fixture's `on-command`) and clicking one
    /// of its actions reaches the guest (`on-dialog-action` → a new dialog).
    #[test]
    fn a_command_shows_a_dialog_and_its_action_reaches_the_guest() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("dialog-cmd");
        let d = root.join("hello");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.hello","name":"hello","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"ui.dialog":{},"commands.slash":{}},"commands":[{"name":"hello"}]}"#,
        )
        .unwrap();
        let mut consent = super::Consent::default();
        consent.grant(
            "dev.slaq.hello",
            &[
                super::Capability::UiDialog,
                super::Capability::CommandsSlash,
            ],
            Some(super::ReadScope::All),
        );
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent,
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(
            host.invoke_command("hello", "", "C0T"),
            1,
            "the command ran"
        );
        let effects = host.take_dialog_effects();
        assert_eq!(effects.len(), 1, "one dialog");
        assert_eq!(effects[0].0, "dev.slaq.hello");
        match &effects[0].1 {
            super::DialogEffect::Show(d) => {
                assert_eq!(d.title, "hello dialog");
                assert_eq!(d.actions, vec![("ok".to_string(), "OK".to_string())]);
            }
            other => panic!("expected a dialog, got {other:?}"),
        }
        assert!(host.take_dialog_effects().is_empty(), "drained once");
        // Clicking the offered action reaches the guest, which shows a new dialog.
        assert!(host.invoke_dialog_action("dev.slaq.hello", "ok"));
        match host.take_dialog_effects().first().map(|(_, e)| e) {
            Some(super::DialogEffect::Show(d)) => assert_eq!(d.title, "clicked ok"),
            other => panic!("expected a follow-up dialog, got {other:?}"),
        }
        // An id that was never offered is refused.
        assert!(!host.invoke_dialog_action("dev.slaq.hello", "nope"));
    }

    /// `ui.dialog` withholds the whole path: the guest's `on-command` does not
    /// show a dialog, and `invoke_dialog_action` delivers nothing.
    #[test]
    fn dialog_requires_the_capability() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("dialog-nocap");
        let d = root.join("hello");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.hello","name":"hello","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"ui.dialog":{},"commands.slash":{}},"commands":[{"name":"hello"}]}"#,
        )
        .unwrap();
        let mut consent = super::Consent::default();
        consent.grant(
            "dev.slaq.hello",
            &[super::Capability::CommandsSlash],
            Some(super::ReadScope::All),
        );
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent,
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(
            host.invoke_command("hello", "", "C0T"),
            1,
            "the command ran"
        );
        assert!(
            host.take_dialog_effects().is_empty(),
            "no dialog without the grant"
        );
        assert!(
            !host.invoke_dialog_action("dev.slaq.hello", "ok"),
            "no action without the grant"
        );
    }

    /// A non-empty `ui.messageActions` requires `ui.message_action` (fail closed).
    #[test]
    fn a_message_action_requires_the_capability() {
        let d = tmp("action-cap");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        let write = |caps: &str| {
            fs::write(
                d.join("manifest.json"),
                format!(
                    r#"{{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{caps},"ui":{{"messageActions":[{{"id":"ping","label":"Ping"}}]}}}}"#
                ),
            )
            .unwrap();
        };
        write(r#"{"messages.read":{"scope":"all"}}"#);
        assert!(
            load(&d).is_err(),
            "messageActions require ui.message_action"
        );
        write(r#"{"ui.message_action":{}}"#);
        let loaded = load(&d).unwrap();
        assert_eq!(loaded.actions.len(), 1);
        assert_eq!(loaded.actions[0].id, "ping");
    }

    /// A non-empty `ui.theme` requires `ui.theme` (fail closed); the table itself
    /// is parsed leniently (a bad hex is dropped, an unknown role is kept as a
    /// name the front end ignores).
    #[test]
    fn a_theme_requires_the_capability_and_is_parsed_leniently() {
        let d = tmp("theme-cap");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        let write = |caps: &str| {
            fs::write(
                d.join("manifest.json"),
                format!(
                    r##"{{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{caps},"ui":{{"theme":{{"accent":"#7C8CFF","bg_base":"0b0e14","nope":"zzz","short":"#123"}}}}}}"##
                ),
            )
            .unwrap();
        };
        write(r#"{"messages.read":{"scope":"all"}}"#);
        assert!(load(&d).is_err(), "ui.theme requires ui.theme");
        write(r#"{"ui.theme":{}}"#);
        let loaded = load(&d).unwrap();
        let mut got = loaded.theme.entries.clone();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("accent".to_string(), "7c8cff".to_string()),
                ("bg_base".to_string(), "0b0e14".to_string()),
            ],
            "only the well-formed entries survive"
        );
    }

    /// `ui.theme` is bounded: at most `MAX_THEME_ENTRIES` roles, and a role name
    /// over `MAX_ROLE_NAME` bytes is dropped.
    #[test]
    fn a_theme_is_bounded() {
        let d = tmp("theme-bounds");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        let mut theme = serde_json::Map::new();
        for i in 0..super::MAX_THEME_ENTRIES + 5 {
            theme.insert(format!("r{i:02}"), serde_json::Value::String("112233".into()));
        }
        theme.insert(
            "x".repeat(super::MAX_ROLE_NAME + 1),
            serde_json::Value::String("445566".into()),
        );
        fs::write(
            d.join("manifest.json"),
            serde_json::json!({
                "manifestVersion": 0,
                "id": "dev.slaq.t",
                "name": "t",
                "version": "0.1.0",
                "apiVersion": "0.1",
                "entry": { "type": "wasm", "path": "mod.wasm" },
                "capabilities": { "ui.theme": {} },
                "ui": { "theme": theme },
            })
            .to_string(),
        )
        .unwrap();
        let loaded = load(&d).unwrap();
        assert_eq!(loaded.theme.entries.len(), super::MAX_THEME_ENTRIES);
        assert!(loaded
            .theme
            .entries
            .iter()
            .all(|(n, _)| n.len() <= super::MAX_ROLE_NAME));
    }

    /// `Host::theme()` yields a mod's overrides only when `ui.theme` is granted:
    /// declaring the table is not enough.
    #[test]
    fn theme_requires_the_grant() {
        use super::Capability;
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("theme-grant");
        let d = root.join("m");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r##"{"manifestVersion":0,"id":"dev.slaq.theme","name":"Theme","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"ui.view":{},"ui.theme":{}},"ui":{"views":[{"id":"console","label":"Console"}],"theme":{"accent":"112233"}}}"##,
        )
        .unwrap();
        let load = |caps: &[Capability]| {
            let mut c = super::Consent::default();
            c.grant("dev.slaq.theme", caps, Some(super::capability::ReadScope::All));
            super::Host::load_root(
                &root,
                super::engine().unwrap(),
                &c,
                &std::sync::Arc::new(super::Directory::empty()),
                &root,
            )
        };
        // Everything the package needs EXCEPT `ui.theme`: no colours.
        assert!(
            load(&[Capability::MessagesRead, Capability::UiView]).theme().is_empty(),
            "a declared ui.theme without the grant must not apply"
        );
        // `ui.theme` granted too: the overrides come through.
        assert_eq!(
            load(&[Capability::MessagesRead, Capability::UiView, Capability::UiTheme]).theme(),
            vec![("accent".to_string(), "112233".to_string())]
        );
    }

    /// The light theme shipped as the example (`docs/mods/examples/light-theme`,
    /// published as the download) loads through the real loader with today's
    /// contract: it asks for `ui.theme` only, applies nothing without it, and
    /// applies its whole palette with it. A mod-system change that breaks the
    /// example (an export it lacks, a manifest rule it fails) fails here.
    #[test]
    fn the_light_theme_example_loads_and_applies() {
        use super::Capability;
        let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/mods/examples/light-theme");
        let loaded = super::load(&example).expect("the example manifest is valid");
        assert_eq!(
            loaded.requested.caps().iter().copied().collect::<Vec<_>>(),
            vec![Capability::UiTheme],
            "a theme needs nothing but ui.theme"
        );
        let root = tmp("light-theme-example");
        let d = root.join("light-theme");
        fs::create_dir_all(&d).unwrap();
        for f in ["manifest.json", "mod.wasm"] {
            fs::copy(example.join(f), d.join(f)).unwrap();
        }
        let load = |caps: &[Capability]| {
            let mut c = super::Consent::default();
            c.grant("dev.danger.light-theme", caps, None);
            super::Host::load_root(
                &root,
                super::engine().unwrap(),
                &c,
                &std::sync::Arc::new(super::Directory::empty()),
                &root,
            )
        };
        let off = load(&[]);
        assert_eq!(off.len(), 1, "the component instantiates");
        assert!(off.theme().is_empty(), "no colours without the grant");
        let theme = load(&[Capability::UiTheme]).theme();
        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(example.join("manifest.json")).unwrap()).unwrap();
        let declared = raw["ui"]["theme"].as_object().unwrap().len();
        assert_eq!(theme.len(), declared, "every declared colour is well-formed and applies");
        assert_eq!(theme, loaded.theme.entries);
        assert!(theme.contains(&("bg_base".to_string(), "f4f5f7".to_string())));
    }

    /// A non-empty `ui.composerActions` requires `ui.composer_action` **and**
    /// `messages.read` (without the read scope the action could never run).
    #[test]
    fn composer_actions_require_the_capabilities() {
        let d = tmp("composer-cap");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        let write = |caps: &str| {
            fs::write(
                d.join("manifest.json"),
                format!(
                    r#"{{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{caps},"ui":{{"composerActions":[{{"id":"wrap","label":"Wrap"}}]}}}}"#
                ),
            )
            .unwrap();
        };
        // `messages.read` alone: no `ui.composer_action`.
        write(r#"{"messages.read":{"scope":"all"}}"#);
        assert!(load(&d).is_err(), "composerActions need ui.composer_action");
        // `ui.composer_action` alone: no read scope.
        write(r#"{"ui.composer_action":{}}"#);
        assert!(load(&d).is_err(), "composerActions need messages.read");
        // Both: loads.
        write(r#"{"messages.read":{"scope":"all"},"ui.composer_action":{}}"#);
        let loaded = load(&d).unwrap();
        assert_eq!(loaded.composer_actions.len(), 1);
    }

    /// `set-view-content` is gated, requires a declared view, validates the block
    /// kinds/bounds, and drops a divider's text.
    #[test]
    fn a_view_is_gated_declared_and_bounded() {
        use super::slaq::api::host::Host as _;
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        st.declared_views = vec!["console".to_string()];
        assert!(
            st.set_view_content("console".into(), vec![("section".into(), "hi".into())])
                .unwrap_err()
                .contains("permission-denied")
        );
        st.grants = super::Grants::of(&[super::Capability::UiView]);
        assert!(
            st.set_view_content("nope".into(), vec![]).is_err(),
            "undeclared view"
        );
        assert!(
            st.set_view_content("console".into(), vec![("bogus".into(), "x".into())])
                .is_err(),
            "unknown kind"
        );
        st.set_view_content(
            "console".into(),
            vec![
                ("section".into(), "hello".into()),
                ("divider".into(), "ignored".into()),
            ],
        )
        .unwrap();
        let v = st.view_content.get("console").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[1], ("divider".to_string(), String::new()));
    }

    /// The aggregate view-content quota rejects content beyond `MAX_VIEW_BYTES`
    /// (and replacing a view is free).
    #[test]
    fn view_content_is_quota_bounded() {
        use super::slaq::api::host::Host as _;
        let mut st =
            super::HostState::new(super::Grants::of(&[super::Capability::UiView]), Vec::new());
        st.declared_views = vec!["a".to_string(), "b".to_string()];
        // Exactly the cap fits (MAX_VIEW_BYTES / MAX_VIEW_BLOCK_BYTES blocks).
        let blocks: Vec<(String, String)> = std::iter::repeat((
            "section".to_string(),
            "x".repeat(super::MAX_VIEW_BLOCK_BYTES),
        ))
        .take(super::MAX_VIEW_BYTES / super::MAX_VIEW_BLOCK_BYTES)
        .collect();
        st.set_view_content("a".into(), blocks).unwrap();
        // Another view's content now exceeds the aggregate quota.
        assert!(
            st.set_view_content("b".into(), vec![("section".into(), "x".into())])
                .is_err(),
            "aggregate quota"
        );
        // Replacing the same view is free.
        st.set_view_content("a".into(), vec![("section".into(), "small".into())])
            .unwrap();
    }

    /// `vec-upsert`/`vec-search` are gated and round-trip through the store.
    #[test]
    fn vector_store_is_gated_and_round_trips() {
        use super::slaq::api::host::Host as _;
        let cfg = super::store::VectorConfig {
            dim: 2,
            max_vectors: 8,
            max_bytes: 1 << 20,
        };
        let path = std::env::temp_dir().join(format!("slaq-vec-host-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        st.store = Some(super::store::ModStore::at_path(
            path.clone(),
            None,
            Some(cfg),
        ));
        // Denied without the capability.
        assert!(
            st.vec_upsert(vec![("a".into(), vec![1.0, 0.0], None)])
                .unwrap_err()
                .contains("permission-denied")
        );
        assert!(
            st.vec_search(vec![1.0, 0.0], 1, None)
                .unwrap_err()
                .contains("permission-denied")
        );
        st.grants = super::Grants::of(&[super::Capability::StoreVector]);
        st.vec_upsert(vec![
            ("a".into(), vec![1.0, 0.0], None),
            ("b".into(), vec![0.0, 1.0], None),
        ])
        .unwrap();
        let hits = st.vec_search(vec![0.9, 0.1], 1, None).unwrap();
        assert_eq!(hits[0].0, "a");
        // A wrong-dim query is refused.
        assert!(st.vec_search(vec![1.0], 1, None).is_err());
        // Granted but not declared: the store is not configured.
        let mut no_store = super::HostState::new(
            super::Grants::of(&[super::Capability::StoreVector]),
            Vec::new(),
        );
        assert!(
            no_store
                .vec_upsert(vec![("a".into(), vec![1.0], None)])
                .unwrap_err()
                .contains("not configured")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The KV store is gated and round-trips through the **persistent** store.
    #[test]
    fn kv_is_gated_and_persists() {
        use super::slaq::api::host::Host as _;
        let path = std::env::temp_dir().join(format!("slaq-kv-host-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let kv = Some(super::store::KvConfig { max_bytes: 1 << 20 });
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        st.store = Some(super::store::ModStore::at_path(path.clone(), kv, None));
        // Denied without the capability.
        assert!(
            st.kv_get("a".into())
                .unwrap_err()
                .contains("permission-denied")
        );
        st.grants = super::Grants::of(&[super::Capability::StoreKv]);
        assert_eq!(st.kv_get("a".into()).unwrap(), None);
        st.kv_set("a".into(), "1".into()).unwrap();
        assert_eq!(st.kv_get("a".into()).unwrap().as_deref(), Some("1"));
        // A fresh store on the same file still sees the key (it is on disk).
        let mut st2 =
            super::HostState::new(super::Grants::of(&[super::Capability::StoreKv]), Vec::new());
        st2.store = Some(super::store::ModStore::at_path(path.clone(), kv, None));
        assert_eq!(
            st2.kv_get("a".into()).unwrap().as_deref(),
            Some("1"),
            "the kv survives a reload"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `notify-post` is gated, bounded per dispatch and length-capped.
    #[test]
    fn notify_is_gated_and_bounded() {
        use super::slaq::api::host::Host as _;
        // Keep the real desktop out of the test process, and restore the flag so
        // other tests in this binary are unaffected.
        let was = crate::notify::enabled();
        crate::notify::set_enabled(false);
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(
            st.notify_post("hello".into(), "world".into())
                .unwrap_err()
                .contains("permission-denied")
        );
        st.grants = super::Grants::of(&[super::Capability::Notify]);
        assert!(st.notify_post("hello".into(), "world".into()).is_ok());
        // The per-dispatch cap.
        for _ in 0..super::MAX_NOTIFY_PER_DISPATCH {
            let _ = st.notify_post("t".into(), "b".into());
        }
        assert!(
            st.notify_post("t".into(), "b".into()).is_err(),
            "per-dispatch cap"
        );
        // The text caps.
        st.notify_count = 0;
        let long = "x".repeat(super::MAX_NOTIFY_TITLE + 1);
        assert!(st.notify_post(long, "b".into()).is_err(), "title cap");
        st.notify_count = 0;
        let long = "x".repeat(super::MAX_NOTIFY_BODY + 1);
        assert!(st.notify_post("t".into(), long).is_err(), "body cap");
        crate::notify::set_enabled(was);
    }

    /// `sanitize` strips control, bidi and separator characters.
    #[test]
    fn sanitize_strips_invisible_characters() {
        let out = super::sanitize("a\u{1b}[31m b\u{202E} c\u{200B}d\u{2028}e");
        assert_eq!(out, "a[31m b cde", "got {out:?}");
        // ZWJ/ZWNJ are meaningful (emoji sequences, orthography), so they stay.
        assert_eq!(
            super::sanitize("a\u{200D}b\u{200C}c"),
            "a\u{200D}b\u{200C}c"
        );
    }

    /// The notification heading carries the (capped) mod id, then the title.
    #[test]
    fn notify_heading_attributes_and_caps_the_label() {
        assert_eq!(super::notify_heading("dev.slaq.x", "Hi"), "dev.slaq.x: Hi");
        assert_eq!(super::notify_heading("", "Hi"), "Hi");
        let long = "m".repeat(super::MAX_NOTIFY_LABEL + 5);
        let heading = super::notify_heading(&long, "Hi");
        assert!(heading.ends_with(": Hi"), "keeps the title: {heading}");
        assert!(
            heading.starts_with(&"m".repeat(super::MAX_NOTIFY_LABEL)),
            "label is capped: {heading}"
        );
    }

    /// A manifest declaring `notify` loads, and the capability is requested.
    #[test]
    fn a_manifest_declaring_notify_loads() {
        let d = tmp("notify-load");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"notify":{}}}"#,
        )
        .unwrap();
        let loaded = load(&d).expect("a manifest with notify must load");
        assert!(loaded.requested.caps().contains(&super::Capability::Notify));
    }

    /// `reactions-add`/`-remove` are gated on both capabilities, bounded to the
    /// read scope, validated, and need an installed API client.
    #[test]
    fn reactions_are_gated_and_validated() {
        use super::slaq::api::host::Host as _;
        let indexed = || {
            std::sync::Arc::new(std::collections::HashMap::from([(
                ("C0T".to_string(), "1.0".to_string()),
                super::Msg {
                    author: "U0".to_string(),
                    text: "hi".to_string(),
                    ..Default::default()
                },
            )]))
        };
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(
            st.reactions_add("C0T".into(), "1.0".into(), "thumbsup".into())
                .unwrap_err()
                .contains("permission-denied")
        );
        // `reactions.write` alone is not enough: the message must be **readable**
        // (`messages.read`, checked before the index).
        st.grants = super::Grants::of(&[super::Capability::ReactionsWrite]);
        st.messages = indexed();
        assert!(
            st.reactions_add("C0T".into(), "1.0".into(), "thumbsup".into())
                .unwrap_err()
                .contains("permission-denied"),
            "reactions.write without messages.read must be refused"
        );
        // With both grants: a bad emoji is refused, then (no client) it fails
        // closed.
        st.grants = super::Grants::of(&[
            super::Capability::ReactionsWrite,
            super::Capability::MessagesRead,
        ]);
        assert!(
            st.reactions_add("C0T".into(), "1.0".into(), "Bad Name".into())
                .unwrap_err()
                .contains("invalid emoji")
        );
        assert!(
            st.reactions_remove("C0T".into(), "1.0".into(), "thumbsup".into())
                .unwrap_err()
                .contains("no API client")
        );
    }

    /// Emoji names are `[a-z0-9_+-]`, non-empty and bounded.
    #[test]
    fn reaction_names_are_validated() {
        for ok in ["thumbsup", "+1", "-1", "heavy_check_mark", "a", "eyes2"] {
            assert!(super::valid_reaction_name(ok), "{ok} must be valid");
        }
        for bad in ["", "Thumbsup", "has space", "a:b", "a.b", "ünï"] {
            assert!(!super::valid_reaction_name(bad), "{bad:?} must be invalid");
        }
        assert!(!super::valid_reaction_name(
            &"a".repeat(super::MAX_REACTION_NAME + 1)
        ));
    }

    /// The API-write budget bounds one dispatch.
    #[test]
    fn reaction_writes_are_bounded_per_dispatch() {
        use super::slaq::api::host::Host as _;
        let mut st = super::HostState::new(
            super::Grants::of(&[
                super::Capability::ReactionsWrite,
                super::Capability::MessagesRead,
            ]),
            Vec::new(),
        );
        st.messages = std::sync::Arc::new(std::collections::HashMap::from([(
            ("C0T".to_string(), "1.0".to_string()),
            super::Msg {
                author: "U0".to_string(),
                text: "hi".to_string(),
                ..Default::default()
            },
        )]));
        st.api_writes = super::MAX_WRITES_PER_DISPATCH;
        assert!(
            st.reactions_add("C0T".into(), "1.0".into(), "thumbsup".into())
                .unwrap_err()
                .contains("too many writes")
        );
    }

    /// A reaction is bounded to the mod's **read scope**, not the index: a mod
    /// scoped to `C1` cannot react on an indexed `C2` message (the security hole
    /// this guards against).
    #[test]
    fn reactions_are_bounded_to_the_read_scope() {
        use super::slaq::api::host::Host as _;
        let mut consent = super::Consent::default();
        consent.grant(
            "m",
            &[
                super::Capability::MessagesRead,
                super::Capability::ReactionsWrite,
            ],
            Some(super::capability::ReadScope::Channels {
                channels: ["C1".to_string()].into_iter().collect(),
            }),
        );
        // The manifest declares the wider "all"; consent still wins.
        let req = super::Requested::parse(&serde_json::json!({
            "messages.read": { "scope": "all" },
            "reactions.write": {}
        }))
        .unwrap();
        let grants = super::Grants::from_consent(&req, &consent.entry("m"));
        let mut st = super::HostState::new(grants, Vec::new());
        st.messages = std::sync::Arc::new(std::collections::HashMap::from([
            (
                ("C1".to_string(), "1.0".to_string()),
                super::Msg {
                    author: "U0".to_string(),
                    text: "hi".to_string(),
                    ..Default::default()
                },
            ),
            (
                ("C2".to_string(), "2.0".to_string()),
                super::Msg {
                    author: "U0".to_string(),
                    text: "hi".to_string(),
                    ..Default::default()
                },
            ),
        ]));
        // In scope: the scope check passes and it reaches the (missing) client.
        assert!(
            st.reactions_add("C1".into(), "1.0".into(), "thumbsup".into())
                .unwrap_err()
                .contains("no API client")
        );
        // Out of scope but indexed: refused.
        let err = st
            .reactions_add("C2".into(), "2.0".into(), "thumbsup".into())
            .unwrap_err();
        assert!(err.contains("permission-denied"), "out of scope: {err}");
    }

    /// `chat-send` posts **as the user**, bounded to the read scope, and needs the
    /// API client (so it fails closed in tests).
    /// `history`/`thread-replies` are gated on `messages.read` and its scope,
    /// bound their references and `limit`, and are paced per mod: the reads in a
    /// minute are capped (counted when allowed to start, so a call that then
    /// fails — here, no API client — still counts).
    #[test]
    fn history_is_gated_scoped_bounded_and_paced() {
        use super::slaq::api::host::Host as _;
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(st.history("C1".into(), None, 10).unwrap_err().contains("permission-denied"));
        let mut consent = super::Consent::default();
        consent.grant(
            "m",
            &[super::Capability::MessagesRead],
            Some(super::capability::ReadScope::Channels {
                channels: ["C1".to_string()].into_iter().collect(),
            }),
        );
        let req =
            super::Requested::parse(&serde_json::json!({ "messages.read": { "scope": "all" } }))
                .unwrap();
        st.grants = super::Grants::from_consent(&req, &consent.entry("m"));
        assert!(st.history("C2".into(), None, 10).unwrap_err().contains("permission-denied"));
        assert!(
            st.thread_replies("C2".into(), "1.0".into(), 10)
                .unwrap_err()
                .contains("permission-denied")
        );
        assert!(st.history("C1".into(), None, 0).unwrap_err().contains("limit"));
        assert!(st.history("C1".into(), None, 201).unwrap_err().contains("limit"));
        let long = "9".repeat(super::MAX_REF_BYTES + 1);
        assert!(st.history("C1".into(), Some(long), 10).unwrap_err().contains("too long"));
        for _ in 0..super::HISTORY_READS_PER_MINUTE {
            assert!(st.history("C1".into(), None, 10).unwrap_err().contains("no API client"));
        }
        assert!(
            st.thread_replies("C1".into(), "1.0".into(), 10)
                .unwrap_err()
                .starts_with("rate-limited")
        );
    }

    /// Raw Slack messages become guest records: the author falls back to the
    /// bot id, a message without `ts` is skipped, files stay empty, and the
    /// count and bytes are bounded.
    #[test]
    fn history_messages_are_converted_and_bounded() {
        let raw = vec![
            serde_json::json!({ "ts": "2.0", "user": "U1", "text": "hi", "thread_ts": "1.0", "reply_count": 3 }),
            serde_json::json!({ "text": "no ts" }),
            serde_json::json!({ "ts": "3.0", "bot_id": "B1", "text": "bot" }),
            serde_json::json!({ "ts": "4.0", "user": "U2", "text": "over the limit" }),
        ];
        let out = super::history_messages(&raw, 3);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].author.as_str(), out[0].thread_ts.as_deref(), out[0].reply_count), ("U1", Some("1.0"), 3));
        assert_eq!(out[1].author, "B1");
        assert!(out.iter().all(|m| m.files.is_empty()));
        let big = vec![serde_json::json!({ "ts": "1.0", "text": "x".repeat(super::MAX_HISTORY_BYTES + 1) })];
        assert!(super::history_messages(&big, 10).is_empty());
        assert!(super::history_error("slack error: ratelimited").starts_with("rate-limited"));
    }

    /// Text a mod shows on several lines keeps its line breaks and tabs, and
    /// loses what could disguise it (bidi overrides, zero-width characters,
    /// other control characters).
    #[test]
    fn multiline_mod_text_is_sanitized() {
        assert_eq!(
            super::sanitize_multiline("a\r\nb\tc\u{202E}evil\u{200B}\u{0007}"),
            "a\nb\tcevil"
        );
        assert_eq!(super::sanitize("a\nb"), "ab", "single-line text stays single-line");
    }

    /// `minSlaq` is enforced: a package needing a newer slaq is refused with a
    /// clear reason, an older or equal requirement loads, a malformed one is
    /// refused.
    #[test]
    fn min_slaq_is_enforced() {
        assert_eq!(super::parse_semver("0.9.0"), Some((0, 9, 0)));
        assert_eq!(super::parse_semver("1.2.3-rc.1+b"), Some((1, 2, 3)));
        assert_eq!(super::parse_semver("1.2"), None);
        assert_eq!(super::parse_semver("1.2.3.4"), None);
        let d = tmp("min-slaq");
        fs::copy("tests/data/hello-mod/mod.wasm", d.join("mod.wasm")).unwrap();
        let write = |min: &str| {
            fs::write(
                d.join("manifest.json"),
                format!(
                    r#"{{"manifestVersion":0,"id":"dev.slaq.min","name":"m","version":"0.1.0","apiVersion":"0.1","minSlaq":"{min}","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{{}}}}"#
                ),
            )
            .unwrap();
        };
        write(env!("CARGO_PKG_VERSION"));
        assert!(super::load(&d).is_ok(), "this very version loads");
        write("0.0.1");
        assert!(super::load(&d).is_ok());
        write("999.0.0");
        let e = super::load(&d).err().unwrap().to_string();
        assert!(e.contains("needs slaq 999.0.0 or newer"), "{e}");
        write("soon");
        assert!(super::load(&d).is_err());
    }

    #[test]
    fn vec_delete_is_gated() {
        use super::slaq::api::host::Host as _;
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(st.vec_delete(vec!["a".into()]).unwrap_err().contains("permission-denied"));
        st.grants = super::Grants::of(&[super::Capability::StoreVector]);
        assert!(st.vec_delete(vec!["a".into()]).unwrap_err().contains("not configured"));
        let many = vec![String::new(); super::MAX_VEC_UPSERT_ITEMS + 1];
        assert!(st.vec_delete(many).unwrap_err().contains("too many"));
    }

    #[test]
    fn chat_send_is_gated_scoped_and_bounded() {
        use super::slaq::api::host::Host as _;
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(
            st.chat_send("C0T".into(), "hi".into(), None)
                .unwrap_err()
                .contains("permission-denied")
        );
        // `chat.send` without `messages.read`: the scope is undefined → refused.
        st.grants = super::Grants::of(&[super::Capability::ChatSend]);
        assert!(
            st.chat_send("C0T".into(), "hi".into(), None)
                .unwrap_err()
                .contains("permission-denied")
        );
        // Both grants, scope C1: another channel is refused, C1 passes the scope.
        let mut consent = super::Consent::default();
        consent.grant(
            "m",
            &[super::Capability::MessagesRead, super::Capability::ChatSend],
            Some(super::capability::ReadScope::Channels {
                channels: ["C1".to_string()].into_iter().collect(),
            }),
        );
        let req = super::Requested::parse(&serde_json::json!({
            "messages.read": { "scope": "all" },
            "chat.send": {}
        }))
        .unwrap();
        let grants = super::Grants::from_consent(&req, &consent.entry("m"));
        let mut st = super::HostState::new(grants, Vec::new());
        assert!(
            st.chat_send("C2".into(), "hi".into(), None)
                .unwrap_err()
                .contains("permission-denied"),
            "out of scope"
        );
        assert!(
            st.chat_send("C1".into(), String::new(), None)
                .unwrap_err()
                .contains("empty")
        );
        let long = "x".repeat(super::MAX_MESSAGE_CHARS + 1);
        assert!(
            st.chat_send("C1".into(), long, None)
                .unwrap_err()
                .contains("exceeds")
        );
        // In scope, valid: the (missing) client makes it fail closed.
        assert!(
            st.chat_send("C1".into(), "hi".into(), None)
                .unwrap_err()
                .contains("no API client")
        );
        // Posts share the per-dispatch write budget with reactions.
        st.api_writes = super::MAX_WRITES_PER_DISPATCH;
        assert!(
            st.chat_send("C1".into(), "hi".into(), None)
                .unwrap_err()
                .contains("too many writes")
        );
    }

    /// `commands` parsing: defaults, invalid names, duplicates, unknown keys.
    #[test]
    fn parses_and_validates_commands() {
        assert!(
            super::parse_commands(&serde_json::json!(null))
                .unwrap()
                .is_empty()
        );
        let c = super::parse_commands(&serde_json::json!([{ "name": "translate" }])).unwrap();
        assert_eq!(
            (
                c[0].name.as_str(),
                c[0].label.as_str(),
                c[0].description.as_str()
            ),
            ("translate", "translate", "")
        );
        assert!(super::parse_commands(&serde_json::json!([{ "name": "Bad" }])).is_err());
        assert!(
            super::parse_commands(&serde_json::json!([{ "name": "a" }, { "name": "a" }])).is_err(),
            "duplicate"
        );
        assert!(super::parse_commands(&serde_json::json!([{ "name": "a", "x": 1 }])).is_err());
        assert!(super::parse_commands(&serde_json::json!([{}])).is_err());
    }

    /// A manifest declaring `commands` without `commands.slash` is refused; with
    /// it, the command is loaded.
    #[test]
    fn commands_require_the_capability() {
        let d = tmp("cmd-cap");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        let manifest = |caps: &str| {
            format!(
                r#"{{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{caps},"commands":[{{"name":"ping"}}]}}"#
            )
        };
        fs::write(d.join("manifest.json"), manifest("{}")).unwrap();
        assert!(load(&d).is_err(), "a command requires commands.slash");
        fs::write(
            d.join("manifest.json"),
            manifest(r#"{"commands.slash":{}}"#),
        )
        .unwrap();
        assert_eq!(load(&d).unwrap().commands.len(), 1);
    }

    /// A command reaches the declaring mod that holds `commands.slash` (and
    /// nobody otherwise).
    #[test]
    fn commands_go_to_the_declaring_mod_with_the_capability() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("cmd-invoke");
        let d = root.join("a");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.a","name":"a","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"commands.slash":{}},"commands":[{"name":"ping"}]}"#,
        )
        .unwrap();
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.a"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(host.commands().len(), 1, "the registry lists it");
        assert_eq!(host.invoke_command("ping", "hello", "C0T"), 1);
        // An undeclared command reaches nobody.
        assert_eq!(host.invoke_command("nope", "", "C0T"), 0);
        // Without the grant it is neither offered nor delivered.
        let mut denied = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &super::Consent::default(),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert!(denied.commands().is_empty());
        assert_eq!(denied.invoke_command("ping", "", "C0T"), 0);
    }

    /// `events` parsing: only implemented events, each with its capability.
    #[test]
    fn parses_and_validates_events() {
        use std::collections::BTreeSet;
        let caps = |c: &[super::Capability]| c.iter().copied().collect::<BTreeSet<_>>();
        let read = caps(&[super::Capability::MessagesRead]);
        let ok = |v: serde_json::Value, c: &BTreeSet<super::Capability>| {
            super::events::parse_events(&v, c)
        };
        assert_eq!(
            ok(
                serde_json::json!(["message.created", "message.updated"]),
                &read
            )
            .unwrap()
            .len(),
            2
        );
        assert_eq!(
            ok(serde_json::json!(null), &read).unwrap().len(),
            0,
            "absent"
        );
        // A message event requires `messages.read`.
        assert!(
            ok(serde_json::json!(["message.created"]), &caps(&[])).is_err(),
            "capability required"
        );
        // A channel event requires `channels.list`.
        let chan = caps(&[super::Capability::ChannelsList]);
        assert_eq!(
            ok(
                serde_json::json!(["channel.opened", "channel.updated"]),
                &chan
            )
            .unwrap()
            .len(),
            2
        );
        assert!(
            ok(serde_json::json!(["channel.opened"]), &read).is_err(),
            "a channel event requires channels.list"
        );
        // Reaction events are implemented and require `messages.read`.
        assert_eq!(
            ok(
                serde_json::json!(["reaction.added", "reaction.removed"]),
                &read
            )
            .unwrap()
            .len(),
            2
        );
        assert!(
            ok(serde_json::json!(["reaction.added"]), &chan).is_err(),
            "a reaction event requires messages.read"
        );
        // `message.deleted` is still not implemented.
        assert!(
            ok(serde_json::json!(["message.deleted"]), &read).is_err(),
            "not implemented"
        );
        assert!(
            ok(serde_json::json!(["channel.member_joined"]), &read).is_err(),
            "reserved"
        );
        assert!(
            ok(serde_json::json!(["nope.event"]), &read).is_err(),
            "unknown"
        );
        assert!(
            ok(serde_json::json!("message.created"), &read).is_err(),
            "not an array"
        );
        assert!(
            ok(serde_json::json!(["message.created", 42]), &read).is_err(),
            "non-string"
        );
        assert!(
            ok(
                serde_json::json!(["message.created", "message.created"]),
                &read
            )
            .is_err(),
            "duplicate"
        );
        let many: Vec<&str> = std::iter::repeat("message.created").take(17).collect();
        assert!(ok(serde_json::json!(many), &read).is_err(), "too many");
    }

    /// A manifest subscribing to an event without its capability is refused.
    #[test]
    fn a_subscription_without_the_capability_is_refused() {
        let d = tmp("events-cap");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{},"events":["message.created"]}"#,
        )
        .unwrap();
        assert!(load(&d).is_err(), "an event requires its capability");
    }

    /// A channel event is delivered to a subscribed mod that holds
    /// `channels.list` (and to nobody otherwise).
    #[test]
    fn channel_events_go_to_subscribers_with_the_capability() {
        use super::events::Event;
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("chan-ev");
        let d = root.join("a");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.a","name":"a","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"channels.list":{}},"events":["channel.opened","channel.updated"]}"#,
        )
        .unwrap();
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.a"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(host.channel_event(Event::ChannelOpened, "C0T"), 1);
        assert_eq!(host.channel_event(Event::ChannelUpdated, "C0T"), 1);
        // Without the grant, it is not delivered.
        let mut denied = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &super::Consent::default(),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(denied.channel_event(Event::ChannelOpened, "C0T"), 0);

        // Granted `channels.list` but not subscribed to the event: still nothing.
        let root_b = tmp("chan-ev-b");
        let db = root_b.join("b");
        fs::create_dir_all(&db).unwrap();
        fs::copy(fixture.join("mod.wasm"), db.join("mod.wasm")).unwrap();
        fs::write(
            db.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.b","name":"b","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"channels.list":{}}}"#,
        )
        .unwrap();
        let mut unsub = super::Host::load_root(
            &root_b,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.b"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root_b,
        );
        assert_eq!(unsub.channel_event(Event::ChannelOpened, "C0T"), 0);
    }

    /// A reaction event is delivered to a subscribed mod that holds
    /// `messages.read`, only inside its read scope.
    #[test]
    fn reaction_events_go_to_subscribers_within_scope() {
        use super::events::Event;
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("react-ev");
        let d = root.join("a");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.a","name":"a","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"}},"events":["reaction.added","reaction.removed"]}"#,
        )
        .unwrap();
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.a"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        // The reacted message is served by `get-message` (the fixture reads it).
        assert_eq!(
            host.reaction_event(
                Event::ReactionAdded,
                "C0T",
                "1.0",
                "thumbsup",
                "U0",
                true,
                Some(super::Msg {
                    author: "U1".to_string(),
                    text: "hi".to_string(),
                    ..Default::default()
                }),
            ),
            1
        );
        // `get-message` reads this per-mod index: it holds the reacted message.
        assert!(
            host.mod_message_keys(0)
                .contains(&("C0T".to_string(), "1.0".to_string())),
            "the reacted message is in the index get-message reads"
        );
        assert_eq!(
            host.reaction_event(
                Event::ReactionRemoved,
                "C0T",
                "1.0",
                "thumbsup",
                "U0",
                false,
                None
            ),
            1
        );
        // With no content from the front end, the index is empty.
        assert!(
            host.mod_message_keys(0).is_empty(),
            "no content => nothing to serve"
        );
        // Not subscribed: nothing (a fresh host without the `events` entry).
        let root_u = tmp("react-ev-unsub");
        let du = root_u.join("u");
        fs::create_dir_all(&du).unwrap();
        fs::copy(fixture.join("mod.wasm"), du.join("mod.wasm")).unwrap();
        fs::write(
            du.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.u","name":"u","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"}}}"#,
        )
        .unwrap();
        let mut unsub = super::Host::load_root(
            &root_u,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.u"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root_u,
        );
        assert_eq!(
            unsub.reaction_event(Event::ReactionAdded, "C0T", "1.0", "x", "U0", true, None),
            0
        );
        // Scoped to C_OK: a reaction on another channel is not delivered.
        let root_s = tmp("react-ev-scoped");
        let ds = root_s.join("s");
        fs::create_dir_all(&ds).unwrap();
        fs::copy(fixture.join("mod.wasm"), ds.join("mod.wasm")).unwrap();
        fs::write(
            ds.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.s","name":"s","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"channels","channels":["C_OK"]}},"events":["reaction.added"]}"#,
        )
        .unwrap();
        let mut scoped = super::Host::load_root(
            &root_s,
            super::engine().unwrap(),
            &consent_scoped("dev.slaq.s", &["C_OK"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root_s,
        );
        assert_eq!(
            scoped.reaction_event(
                Event::ReactionAdded,
                "C_OTHER",
                "1.0",
                "x",
                "U0",
                true,
                None
            ),
            0,
            "outside the read scope"
        );
        assert_eq!(
            scoped.reaction_event(Event::ReactionAdded, "C_OK", "1.0", "x", "U0", true, None),
            1,
            "inside the read scope"
        );
    }

    /// A message is delivered only to a mod that subscribed to a message event.
    #[test]
    fn a_mod_must_subscribe_to_receive_messages() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("events-sub");
        let d = root.join("a");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        // Everything on-message needs, except an `events` subscription.
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.a","name":"a","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"store.kv":{"maxBytes":1048576},"ui.message_decoration":{}}}"#,
        )
        .unwrap();
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.a"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        let msgs = vec![super::Msg::text_only("1.000000", "U0", "hi")];
        let out = host.decorate("C0T", &msgs);
        assert!(
            out.values().flatten().next().is_none(),
            "an unsubscribed mod got a message: {out:?}"
        );
    }

    /// `message.created` and `message.updated` are gated independently: a mod
    /// subscribed to updates only gets edits, not creations.
    #[test]
    fn message_created_and_updated_are_gated_independently() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("events-kinds");
        let d = root.join("a");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.a","name":"a","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"store.kv":{"maxBytes":1048576},"ui.message_decoration":{}},"events":["message.updated"]}"#,
        )
        .unwrap();
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.a"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        // A creation is not delivered (the mod wants updates only).
        let created = host.decorate("C0T", &[super::Msg::text_only("1.0", "U0", "hi")]);
        assert!(
            created.values().flatten().next().is_none(),
            "an updates-only mod got a creation: {created:?}"
        );
        // An edit (changed text) is delivered.
        let edited = host.decorate("C0T", &[super::Msg::text_only("1.0", "U0", "hi there")]);
        assert!(
            edited.values().flatten().next().is_some(),
            "an updates-only mod missed the edit: {edited:?}"
        );
    }

    /// A declared view is offered (gated by the grant), and its pushed content is
    /// returned.
    #[test]
    fn a_declared_view_is_offered() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("view");
        let d = root.join("m");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.view","name":"v","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"ui.view":{}},"ui":{"views":[{"id":"console","label":"Console"}]}}"#,
        )
        .unwrap();
        let host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.view"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        let views = host.views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].view_id, "console");
        assert_eq!(views[0].label, "Console");
        // `start` pushed the view content (a real WIT round trip): a section and
        // a divider (whose text is dropped).
        assert_eq!(
            host.view_content("dev.slaq.view", "console"),
            vec![
                ("section".to_string(), "hello view".to_string()),
                ("divider".to_string(), String::new()),
            ]
        );
        // Without the grant, no view is offered.
        let denied = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &super::Consent::default(),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert!(denied.views().is_empty());
    }

    /// A non-empty `ui.views` requires `ui.view` (fail closed).
    #[test]
    fn a_view_requires_the_capability() {
        let d = tmp("view-cap");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{},"ui":{"views":[{"id":"v","label":"V"}]}}"#,
        )
        .unwrap();
        assert!(load(&d).is_err(), "views require ui.view");
    }

    /// An action replaces only its own mod's decoration (another mod's survives),
    /// and a mod without the grant offers none and cannot be invoked.
    #[test]
    fn an_action_replaces_only_its_own_mods_decoration() {
        let fixture = std::path::Path::new("tests/data/hello-mod");
        let root = tmp("action-prov");
        // A: declares a message action. B: only decorations (no action).
        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::copy(fixture.join("mod.wasm"), a.join("mod.wasm")).unwrap();
        fs::write(
            a.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.a","name":"a","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"ui.message_action":{},"ui.message_decoration":{}},"ui":{"messageActions":[{"id":"ping","label":"Ping"}]},"events":["message.created","message.updated"]}"#,
        )
        .unwrap();
        let b = root.join("b");
        fs::create_dir_all(&b).unwrap();
        fs::copy(fixture.join("mod.wasm"), b.join("mod.wasm")).unwrap();
        fs::write(
            b.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.b","name":"b","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"store.kv":{"maxBytes":1048576},"ui.message_decoration":{}},"events":["message.created","message.updated"]}"#,
        )
        .unwrap();
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.a", "dev.slaq.b"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        let msgs = vec![super::Msg::text_only("1.000000", "U0", "hi")];
        let key = ("C0T".to_string(), "1.000000".to_string());
        // Only B decorates on-message (A has no `store.kv`, so its `on-message`
        // fails); A's decoration comes only from the action — making the
        // after-count discriminating (a whole-key replace would leave 1).
        let before = host
            .decorate("C0T", &msgs)
            .get(&key)
            .cloned()
            .unwrap_or_default();
        assert_eq!(before.len(), 1, "only B decorates on-message: {before:?}");
        // Invoke A's action: A adds its decoration, B's survives.
        let after = host
            .invoke_action(
                "dev.slaq.a",
                "ping",
                "C0T",
                "1.000000",
                &super::Msg::text_only("1.000000", "U0", "hi"),
            )
            .unwrap();
        assert!(
            after.iter().any(|t| t.contains("hello action: ping")),
            "the action decoration: {after:?}"
        );
        assert!(
            after.iter().any(|t| t.contains("hello #1")),
            "B's on-message decoration survived: {after:?}"
        );
        assert_eq!(
            after.len(),
            2,
            "A's action + B's on-message (not 1: provenance broken): {after:?}"
        );

        // Without the grant, the action is neither offered nor invocable.
        let mut denied = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_scoped("dev.slaq.a", &["C0T"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert!(denied.message_actions().is_empty(), "no grant, no action");
        assert!(
            denied
                .invoke_action(
                    "dev.slaq.a",
                    "ping",
                    "C0T",
                    "1.0",
                    &super::Msg::text_only("1.0", "U0", "hi")
                )
                .is_none()
        );
    }

    /// An edit (changed content hash) re-runs the mod and replaces the stale
    /// decoration (the counter advances), rather than keeping the old one.
    #[test]
    fn edit_reruns_and_replaces_decoration() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let mut host =
            super::Host::load_one_with(&dir, super::engine().unwrap(), &all_caps()).unwrap();
        let first = host.decorate("C0T", &[super::Msg::text_only("1.000000", "U0", "hi")]);
        assert!(
            first.values().flatten().any(|t| t.contains("#1")),
            "first run: {first:?}"
        );
        // Same ts, changed text: the mod must run again (counter is now #2).
        let edited = host.decorate("C0T", &[super::Msg::text_only("1.000000", "U0", "hello")]);
        let texts: Vec<&String> = edited.values().flatten().collect();
        assert!(
            texts.iter().any(|t| t.contains("#2")),
            "the edit did not re-run the mod: {texts:?}"
        );
    }

    /// A cache-cap of one (applied once, at the end of a batch) evicts the older
    /// message from both `seen` and `decorated`, so the structures stay bounded
    /// and consistent.
    #[test]
    fn cache_cap_evicts_seen_and_decorated_together() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let mut host =
            super::Host::load_one_with(&dir, super::engine().unwrap(), &all_caps()).unwrap();
        host.cache.lock().unwrap().cache_cap = 1;
        let both = vec![
            super::Msg::text_only("1.000000", "U0", "one"),
            super::Msg::text_only("2.000000", "U0", "two"),
        ];
        let _ = host.dispatch_batch("C0T", &both);
        let cache = host.cache.lock().unwrap();
        assert!(cache.seen.len() <= 1, "seen not evicted: {}", cache.seen.len());
        assert!(
            cache.decorated.len() <= 1,
            "decorated not evicted: {}",
            cache.decorated.len()
        );
        // The FIFO queue may hold lazy tombstones (stale nodes) but stays bounded
        // by the compaction threshold, not by the number of evictions.
        assert!(
            cache.decorated_order.len() <= 8,
            "the FIFO queue grew without compaction: {}",
            cache.decorated_order.len()
        );
    }

    /// Regression (adversarial review of 1b): two dispatches of the same channel
    /// racing on the same new messages must run each message's `on-message` once
    /// and store its decoration once. Before claiming, both passes saw the message
    /// as unseen, ran the guest twice (the fixture's global counter skipped
    /// numbers) and appended the decoration twice to the Host cache.
    #[test]
    fn concurrent_dispatches_run_each_message_once() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let root = tmp("race");
        let d = root.join("m");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.race","name":"race","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"store.kv":{"maxBytes":1048576},"ui.message_decoration":{}},"events":["message.created","message.updated"]}"#,
        )
        .unwrap();
        let host = std::sync::Arc::new(super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.race"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        ));
        const ROUNDS: usize = 25;
        for i in 1..=ROUNDS {
            let msgs = vec![super::Msg::text_only(&format!("{i}.000000"), "U0", "hi")];
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let racers: Vec<_> = (0..2)
                .map(|_| {
                    let (host, barrier, msgs) = (host.clone(), barrier.clone(), msgs.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        host.dispatch_batch("C0T", &msgs)
                    })
                })
                .collect();
            for r in racers {
                r.join().unwrap();
            }
        }
        let all: Vec<super::Msg> = (1..=ROUNDS)
            .map(|i| super::Msg::text_only(&format!("{i}.000000"), "U0", "hi"))
            .collect();
        let out = host.cached_for("C0T", &all);
        let mut numbers = Vec::new();
        for m in &all {
            let texts = out
                .get(&("C0T".to_string(), m.ts.clone()))
                .unwrap_or_else(|| panic!("message {} was not decorated", m.ts));
            assert_eq!(texts.len(), 1, "duplicated decoration on {}: {texts:?}", m.ts);
            let n: usize = texts[0]
                .split('#')
                .nth(1)
                .and_then(|t| t.split(':').next())
                .and_then(|t| t.parse().ok())
                .unwrap_or_else(|| panic!("no counter in {:?}", texts[0]));
            numbers.push(n);
        }
        numbers.sort_unstable();
        assert_eq!(
            numbers,
            (1..=ROUNDS).collect::<Vec<_>>(),
            "on-message ran more than once for some message"
        );
    }

    /// Regression (second review): Stop must also stop jobs that were already
    /// queued for the mod — they used to run to completion, side effects included.
    #[test]
    fn a_stopped_worker_refuses_queued_jobs() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let host = super::Host::load_one_with(&dir, super::engine().unwrap(), &all_caps()).unwrap();
        let id = host.mods[0].id.clone();
        let index: std::sync::Arc<std::collections::HashMap<(String, String), super::Msg>> = std::sync::Arc::new(
            [(
                ("C0T".to_string(), "1.000000".to_string()),
                super::Msg::text_only("1.000000", "U0", "hi"),
            )]
            .into_iter()
            .collect(),
        );
        let job = |index: std::sync::Arc<std::collections::HashMap<(String, String), super::Msg>>| {
            move |reply| super::Job::Message {
                channel: "C0T".into(),
                ts: "1.000000".into(),
                index,
                directory: std::sync::Arc::new(super::Directory::empty()),
                dispatch: 1,
                reply,
            }
        };
        // A live mod runs it (the fixture decorates with its counter).
        let out = host.mods[0].request(job(index.clone())).unwrap();
        assert_eq!(out.len(), 1, "a live mod runs the job: {out:?}");
        let calls = host.statuses()[0].calls;
        assert!(host.stop(&id));
        // Queued straight to the worker, past the coordinator's own trapped check.
        let out = host.mods[0].request(job(index)).unwrap();
        assert!(out.is_empty(), "a stopped mod must not run queued jobs: {out:?}");
        assert_eq!(host.statuses()[0].calls, calls, "a refused job is not a call");
    }

    /// Regression (second review): two passes racing an *edit* of the same
    /// message used to append both texts' output to the cache. A pass now stores
    /// only while its claim on the key is current, so each key ends with exactly
    /// one decoration.
    #[test]
    fn racing_edits_leave_one_decoration_per_message() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let root = tmp("edit-race");
        let d = root.join("m");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.edits","name":"edits","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"store.kv":{"maxBytes":1048576},"ui.message_decoration":{}},"events":["message.created","message.updated"]}"#,
        )
        .unwrap();
        let host = std::sync::Arc::new(super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&["dev.slaq.edits"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        ));
        const ROUNDS: usize = 30;
        for i in 1..=ROUNDS {
            let ts = format!("{i}.000000");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let racers: Vec<_> = ["first text", "edited text"]
                .into_iter()
                .map(|text| {
                    let (host, barrier) = (host.clone(), barrier.clone());
                    let msg = super::Msg::text_only(&ts, "U0", text);
                    std::thread::spawn(move || {
                        barrier.wait();
                        host.dispatch_batch("C0T", &[msg])
                    })
                })
                .collect();
            for r in racers {
                r.join().unwrap();
            }
            let texts = host
                .cached_for("C0T", &[super::Msg::text_only(&ts, "U0", "")])
                .remove(&("C0T".to_string(), ts.clone()))
                .unwrap_or_default();
            assert_eq!(texts.len(), 1, "round {i}: stale/duplicate output: {texts:?}");
        }
    }

    /// Per-dispatch budgets stay separate when dispatches interleave on one
    /// worker, and the remembered set is bounded.
    #[test]
    fn dispatch_budgets_are_kept_per_dispatch() {
        let mut b = super::DispatchBudgets::default();
        assert_eq!(b.load(1), (0, 0), "a new dispatch starts with a fresh budget");
        b.save(1, 3, 2);
        b.save(2, 1, 0);
        // Dispatch 2 interleaving does not reset dispatch 1's tally.
        assert_eq!(b.load(1), (3, 2));
        assert_eq!(b.load(2), (1, 0));
        b.save(1, 4, 2);
        assert_eq!(b.load(1), (4, 2));
        for d in 10..10 + super::MAX_BUDGET_SLOTS as u64 {
            b.save(d, 1, 1);
        }
        assert_eq!(b.slots.len(), super::MAX_BUDGET_SLOTS, "bounded");
        assert_eq!(b.load(1), (0, 0), "the oldest slots are dropped");
    }

    /// A worker thread that dies is quarantined (instead of silently no-oping
    /// forever while the front ends still offer the mod).
    #[test]
    fn a_crashed_worker_is_quarantined() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let host = super::Host::load_one_with(&dir, super::engine().unwrap(), &all_caps()).unwrap();
        assert!(!host.mods[0].is_trapped());
        let _ = host.mods[0].tx.send(super::Job::Crash);
        // The next request finds the channel closed (or its reply dropped).
        assert!(host.mods[0].request(|reply| super::Job::TakeDialogEffects { reply }).is_none());
        assert!(host.mods[0].is_trapped(), "a dead worker must be quarantined");
        // And the dispatch path skips it without hanging.
        let (out, _) = host.dispatch_batch("C0T", &[super::Msg::text_only("9.000000", "U0", "x")]);
        assert!(out.is_empty());
    }

    /// A host call blocked on I/O returns promptly once its mod is stopped, and
    /// aborts the in-flight request, instead of waiting out the request's own
    /// timeout (tens of seconds for a Slack API call).
    #[test]
    fn blocked_host_io_is_cancelled_when_the_mod_is_stopped() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = finished.clone();
        let task = rt.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            f.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = tx.send(Ok(()));
        });
        let kill = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let k = kill.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            k.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let start = std::time::Instant::now();
        let err = super::await_io(rx, task, &kill, "test-io").unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
        // The aborted request never completes.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!finished.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// The control-panel API: statuses report a live mod and its activity;
    /// `stop` quarantines it, halts its background lane and its dispatch.
    #[test]
    fn stop_quarantines_a_mod_and_halts_its_lane() {
        let root = tmp("stop");
        let id = background_mod(&root, "a", 0.1);
        let host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&[&id]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        let _ = host.dispatch_batch("C0T", &[super::Msg::text_only("1.000000", "U0", "hi")]);
        std::thread::sleep(std::time::Duration::from_millis(350));
        let st = &host.statuses()[0];
        assert_eq!(st.state, super::ModState::Running);
        assert_eq!(st.background, Some(std::time::Duration::from_millis(100)));
        assert!(st.ticks >= 1, "the lane ran: {st:?}");
        assert!(st.calls > st.ticks, "the message call is counted too: {st:?}");

        assert!(host.stop(&id), "a live mod stops");
        assert!(!host.stop(&id), "stopping twice reports it was already stopped");
        assert!(!host.stop("dev.slaq.nope"), "unknown id");
        assert_eq!(host.statuses()[0].state, super::ModState::Stopped);
        // Let a tick in flight finish, then the lane must stay silent — and a
        // skipped tick must not count as a call either.
        std::thread::sleep(std::time::Duration::from_millis(250));
        let (ticks, calls) = (host.statuses()[0].ticks, host.statuses()[0].calls);
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(host.statuses()[0].ticks, ticks, "a stopped mod is not ticked");
        assert_eq!(host.statuses()[0].calls, calls, "skipped ticks are not calls");
        let (out, _) =
            host.dispatch_batch("C0T", &[super::Msg::text_only("2.000000", "U0", "hi")]);
        assert!(
            !out.contains_key(&("C0T".to_string(), "2.000000".to_string())),
            "a stopped mod receives no new messages"
        );
        assert!(host.statuses()[0].busy_for.is_none(), "idle once stopped");
    }

    /// Writes a hello-mod package under `root/name` with a background lane every
    /// `interval` seconds (plus what `on-message` needs, for the starvation test).
    fn background_mod(root: &std::path::Path, name: &str, interval: f64) -> String {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let d = root.join(name);
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        let id = format!("dev.slaq.bg.{name}");
        fs::write(
            d.join("manifest.json"),
            format!(
                r#"{{"manifestVersion":0,"id":"{id}","name":"bg","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{{"background":{{"intervalSecs":{interval}}},"messages.read":{{"scope":"all"}},"store.kv":{{"maxBytes":1048576}},"ui.message_decoration":{{}}}},"events":["background.tick","message.created"]}}"#
            ),
        )
        .unwrap();
        id
    }

    /// A mod that subscribed to `background.tick` and was granted `background` is
    /// woken on its own thread at about the configured interval.
    #[test]
    fn background_lane_ticks_at_the_interval() {
        let root = tmp("bg-ticks");
        let id = background_mod(&root, "a", 0.1);
        let host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&[&id]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(host.mods.len(), 1, "the mod must load");
        std::thread::sleep(std::time::Duration::from_millis(750));
        let ticks = host.mod_ticks(0);
        // ~7 at 100 ms; generous bounds keep this robust on a loaded machine while
        // still proving it ticks repeatedly and does not busy-spin.
        assert!((3..=9).contains(&ticks), "ticks in 750ms at 100ms: {ticks}");
    }

    /// Without the user's `background` grant the lane never runs, even though the
    /// manifest asked for it.
    #[test]
    fn background_lane_needs_the_grant() {
        let root = tmp("bg-nogrant");
        let id = background_mod(&root, "a", 0.1);
        let mut consent = super::Consent::default();
        let without: Vec<super::Capability> = super::Capability::ALL
            .into_iter()
            .filter(|c| *c != super::Capability::Background)
            .collect();
        consent.grant(&id, &without, Some(super::capability::ReadScope::All));
        let host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent,
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(host.mods.len(), 1, "the mod must load");
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(host.mod_ticks(0), 0, "no grant, no background lane");
    }

    /// A steady stream of message dispatches cannot starve the background lane: the
    /// deadline is checked before every receive.
    #[test]
    fn background_ticks_are_not_starved_by_jobs() {
        let root = tmp("bg-starve");
        let id = background_mod(&root, "a", 0.1);
        let host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_all(&[&id]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        let start = std::time::Instant::now();
        let mut n = 0u64;
        while start.elapsed() < std::time::Duration::from_millis(600) {
            n += 1;
            let msg = super::Msg::text_only(&format!("{n}.000000"), "U0", "hi");
            let _ = host.dispatch_batch("C0T", &[msg]);
        }
        assert!(n > 10, "the loop must have kept the worker busy: {n} dispatches");
        assert!(host.mod_ticks(0) >= 2, "ticks starved: {}", host.mod_ticks(0));
    }

    /// `background.tick` is only subscribable alongside the `background` request.
    #[test]
    fn background_tick_requires_the_background_capability() {
        let caps: BTreeSet<super::Capability> = [super::Capability::MessagesRead].into();
        let err = super::events::parse_events(&serde_json::json!(["background.tick"]), &caps)
            .unwrap_err()
            .to_string();
        assert!(err.contains("background"), "{err}");
        let caps: BTreeSet<super::Capability> = [super::Capability::Background].into();
        assert_eq!(
            super::events::parse_events(&serde_json::json!(["background.tick"]), &caps).unwrap(),
            vec![super::events::Event::BackgroundTick]
        );
    }

    /// A single batch processes the whole window, each message exactly once, and
    /// leaves nothing pending.
    #[test]
    fn dispatch_batch_processes_the_whole_window() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let mut host =
            super::Host::load_one_with(&dir, super::engine().unwrap(), &all_caps()).unwrap();
        let msgs: Vec<super::Msg> = (1..=5)
            .map(|i| super::Msg::text_only(&format!("{i}.000000"), "U0", &format!("m{i}")))
            .collect();
        let (out, pending) = host.dispatch_batch("C0T", &msgs);
        assert_eq!(out.len(), 5, "every message must be decorated: {out:?}");
        assert!(!pending, "a single pass must leave nothing pending");
        assert!(
            !host.has_pending("C0T", &msgs),
            "the batch left work pending"
        );
    }

    #[test]
    fn sanitize_log_truncates_and_strips_control() {
        let s = format!("a\u{1b}[31m{}", "x".repeat(1000));
        let out = super::sanitize_log(&s);
        assert!(
            out.len() <= super::MAX_LOG_BYTES,
            "not truncated: {}",
            out.len()
        );
        assert!(!out.chars().any(|c| c.is_control()), "control char kept");
        // A multi-byte char straddling the cut must not panic / split.
        let multi = "é".repeat(1000);
        let _ = super::sanitize_log(&multi);
    }

    #[test]
    fn read_bounded_rejects_oversize() {
        let d = tmp("bounded");
        let f = d.join("big");
        fs::write(&f, vec![0u8; 64]).unwrap();
        assert!(super::read_bounded(&f, 16).is_err());
        assert!(super::read_bounded(&f, 64).is_ok());
    }

    #[test]
    fn content_hash_includes_author_and_text() {
        assert_ne!(
            super::content_hash("U1", "x"),
            super::content_hash("U2", "x")
        );
        assert_ne!(
            super::content_hash("U1", "x"),
            super::content_hash("U1", "y")
        );
        assert_eq!(
            super::content_hash("U1", "x"),
            super::content_hash("U1", "x")
        );
    }

    /// The host rejects decorating a message other than the one being handled.
    #[test]
    fn decorate_only_allows_the_current_message() {
        use super::slaq::api::host::Host as _;
        let mut st = super::HostState::new(super::Grants::of(&super::Capability::ALL), Vec::new());
        st.messages = std::sync::Arc::new(std::collections::HashMap::from([(
            ("C0T".to_string(), "1.000000".to_string()),
            super::Msg {
                author: "U0".to_string(),
                text: "hi".to_string(),
                ..Default::default()
            },
        )]));
        st.current = Some(("C0T".to_string(), "1.000000".to_string()));
        assert!(
            st.decorate("C0T".to_string(), "1.000000".to_string(), "ok".into())
                .is_ok()
        );
        assert!(
            st.decorate("C0T".to_string(), "2.000000".to_string(), "no".into())
                .is_err(),
            "a non-current message must be rejected"
        );
    }

    /// Two folders with the same manifest `id` must not both load.
    #[test]
    fn rejects_duplicate_mod_ids() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let root = tmp("dup-ids");
        for name in ["a", "b"] {
            let d = root.join(name);
            fs::create_dir_all(&d).unwrap();
            for f in ["manifest.json", "mod.wasm"] {
                fs::copy(fixture.join(f), d.join(f)).unwrap();
            }
        }
        // Both copies keep the fixture's id ("dev.slaq.hello").
        let host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &super::Consent::default(),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        assert_eq!(host.mods.len(), 1, "a duplicate id must be rejected");
    }

    /// [`scan`] reports each installed mod's request and its current grant.
    #[test]
    fn scan_reports_requested_and_granted() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let root = tmp("scan");
        let d = root.join("hello");
        fs::create_dir_all(&d).unwrap();
        for f in ["manifest.json", "mod.wasm"] {
            fs::copy(fixture.join(f), d.join(f)).unwrap();
        }
        let mut consent = super::Consent::default();
        consent.grant(
            "dev.slaq.hello",
            &[super::Capability::MessagesRead],
            Some(super::ReadScope::All),
        );
        let scanned = super::scan(&root, &consent);
        assert_eq!(scanned.len(), 1, "the fixture must be scanned");
        let m = &scanned[0];
        assert_eq!(m.id, "dev.slaq.hello");
        assert_eq!(m.requested.len(), 7, "the fixture asks for seven");
        assert!(m.granted.contains(&super::Capability::MessagesRead));
        assert!(!m.granted.contains(&super::Capability::StoreKv));
        assert!(m.read_scope.is_some(), "messages.read asks for a scope");
    }

    /// Two installed folders with the same manifest id yield one entry (the
    /// loader rejects the duplicate; the dialog must not double it).
    #[test]
    fn scan_skips_duplicate_ids() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let root = tmp("scan-dup");
        for name in ["a", "b"] {
            let d = root.join(name);
            fs::create_dir_all(&d).unwrap();
            for f in ["manifest.json", "mod.wasm"] {
                fs::copy(fixture.join(f), d.join(f)).unwrap();
            }
        }
        let scanned = super::scan(&root, &super::Consent::default());
        assert_eq!(scanned.len(), 1, "a duplicate id must yield one entry");
    }

    #[test]
    fn egress_entry_matching() {
        assert!(super::egress_entry_matches(
            "example.com",
            "example.com",
            443
        ));
        assert!(super::egress_entry_matches(
            "example.com",
            "example.com",
            80
        ));
        assert!(super::egress_entry_matches(
            "example.com:443",
            "example.com",
            443
        ));
        assert!(!super::egress_entry_matches(
            "example.com:443",
            "example.com",
            80
        ));
        assert!(!super::egress_entry_matches(
            "example.com",
            "other.com",
            443
        ));
    }

    #[test]
    fn address_classification() {
        use std::net::IpAddr;
        let public = |s: &str| super::addr_is_public(s.parse::<IpAddr>().unwrap());
        assert!(public("93.184.216.34"), "a public IPv4");
        assert!(public("2606:2800:220:1::1"), "a public IPv6");
        for private in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "192.0.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            // IPv4-mapped/compatible must not slip through as "public".
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            // 6to4 wrapping a private v4.
            "2002:7f00:1::",
            // Special-purpose IPv6 blocks.
            "2001:db8::1",
            "2001::1",
            "2001:1f::1", // ORCHIDv1 within the /28
            "2001:21::1", // ORCHIDv2 within the /28
            "64:ff9b::1",
            "3fff::1",
        ] {
            assert!(!public(private), "{private} must be non-public");
        }
        // An IPv4-mapped *public* address is still public.
        assert!(public("::ffff:93.184.216.34"));
    }

    /// `http.fetch` end to end against a loopback server: the allow-list and the
    /// capability gate are enforced, and the response is returned.
    #[test]
    fn http_fetch_enforces_egress() {
        use super::slaq::api::host::Host as _;
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
                );
            }
        });

        // The request runs on the runtime; enter one (multi-threaded so the
        // spawned task can progress while this thread blocks on the result).
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();

        let allowed = vec![format!("127.0.0.1:{}", addr.port())];
        let mut st =
            super::HostState::new(super::Grants::of(&[super::Capability::NetEgress]), allowed);
        let url = format!("http://127.0.0.1:{}/", addr.port());
        let (status, body) = st
            .http_fetch("GET".to_string(), url, Vec::new(), None)
            .expect("an allowed fetch");
        assert_eq!(status, 200);
        assert_eq!(body, b"hi");
        let _ = server.join();

        // A host off the allow-list is denied.
        let denied = st
            .http_fetch(
                "GET".to_string(),
                "http://127.0.0.1:1/".to_string(),
                Vec::new(),
                None,
            )
            .unwrap_err();
        assert!(denied.contains("permission-denied"), "{denied}");
        // Without the capability it is denied, whatever the allow-list.
        let mut none =
            super::HostState::new(super::Grants::default(), vec!["127.0.0.1".to_string()]);
        assert!(
            none.http_fetch(
                "GET".to_string(),
                "http://127.0.0.1:1/".to_string(),
                Vec::new(),
                None
            )
            .unwrap_err()
            .contains("permission-denied")
        );
    }

    /// `file-get` is gated on `files.download`, only resolves ids the host was
    /// given, and refuses a non-Slack file host before any fetch. (The transport
    /// is the shared `download_bytes`, which is size-capped.)
    #[test]
    fn file_get_is_gated_and_scoped_to_delivered_ids() {
        use super::slaq::api::host::Host as _;
        let mut denied = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(
            denied.file_get("F1".to_string()).is_err(),
            "denied without files.download"
        );
        let mut st = super::HostState::new(
            super::Grants::of(&[
                super::Capability::FilesDownload,
                super::Capability::MessagesRead,
            ]),
            Vec::new(),
        );
        // No directory: the id was never delivered.
        assert!(
            st.file_get("F1".to_string())
                .unwrap_err()
                .contains("unknown file"),
            "an id the host never delivered"
        );
        // An over-long id is refused up front.
        assert!(
            st.file_get("F".repeat(super::MAX_FILE_ID_BYTES + 1))
                .is_err(),
            "an over-long id"
        );
        // A delivered file: the id the host was given, for a message in C1.
        let dir = |url: &str| {
            let mut files = std::collections::HashMap::new();
            files.insert(
                "F1".to_string(),
                super::FileRef {
                    channel: "C1".to_string(),
                    ts: "1.0".to_string(),
                    url: url.to_string(),
                    name: "f.bin".to_string(),
                    mimetype: "application/octet-stream".to_string(),
                    size: 3,
                },
            );
            Some(std::sync::Arc::new(super::Directory {
                files,
                ..Default::default()
            }))
        };
        st.directory = dir("https://evil.example/x");
        assert!(
            st.file_get("F1".to_string())
                .unwrap_err()
                .contains("non-Slack"),
            "a non-Slack host"
        );
        // A Slack-hosted id passes the checks and reaches the download (which
        // fails here: no API client is installed), so it is not a validation error.
        st.directory = dir("https://files.slack.com/x");
        assert!(
            st.file_get("F1".to_string())
                .unwrap_err()
                .contains("no API client"),
            "reaches the download"
        );

        // Read scope C2: a file on C1 is out of scope for both calls.
        let req = super::Requested::parse(&serde_json::json!({
            "messages.read": { "scope": "all" },
            "files.download": {}
        }))
        .unwrap();
        let mut consent = super::Consent::default();
        consent.grant(
            "m",
            &[
                super::Capability::FilesDownload,
                super::Capability::MessagesRead,
            ],
            Some(super::capability::ReadScope::Channels {
                channels: ["C2".to_string()].into_iter().collect(),
            }),
        );
        let mut out = super::HostState::new(
            super::Grants::from_consent(&req, &consent.entry("m")),
            Vec::new(),
        );
        out.directory = dir("https://files.slack.com/x");
        assert!(
            out.file_get("F1".to_string())
                .unwrap_err()
                .contains("permission-denied"),
            "a file outside the read scope"
        );
        // `get-message` carries a message's files (the ids `file-get` takes).
        st.messages = std::sync::Arc::new(std::collections::HashMap::from([(
            ("C1".to_string(), "1.0".to_string()),
            super::Msg {
                author: "U1".to_string(),
                text: "hi".to_string(),
                files: vec![super::ModFile {
                    id: "F1".to_string(),
                    name: "f.bin".to_string(),
                    mime: "application/octet-stream".to_string(),
                }],
                ..Default::default()
            },
        )]));
        let m = st.get_message("C1".to_string(), "1.0".to_string()).unwrap();
        assert_eq!(m.files.len(), 1, "the message carries its file");
        assert_eq!(m.files[0].id, "F1");
    }

    /// `fs-*` are gated on `filesystem`, need a configured sandbox, and serve a
    /// mapped slot through the host functions (deny path + happy path).
    #[test]
    fn fs_calls_are_gated_and_serve_a_mapped_slot() {
        use super::slaq::api::host::Host as _;
        // Denied without `filesystem`.
        let mut denied = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(
            denied
                .fs_list("inbox".into(), "".into(), false, 10)
                .unwrap_err()
                .contains("permission-denied"),
            "denied without filesystem"
        );
        assert!(
            denied
                .fs_read("inbox".into(), "a".into())
                .unwrap_err()
                .contains("permission-denied")
        );
        // Granted `filesystem` but no sandbox configured (e.g. the dev path).
        let mut unconfigured = super::HostState::new(
            super::Grants::of(&[super::Capability::Filesystem]),
            Vec::new(),
        );
        assert!(
            unconfigured
                .fs_read("inbox".into(), "a".into())
                .unwrap_err()
                .contains("not configured"),
            "no sandbox"
        );

        // A mapped slot, served through the host functions.
        let root = tmp("fs-host");
        let dir = root.join("inbox");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "hello").unwrap();
        let map = std::collections::BTreeMap::from([(
            "inbox".to_string(),
            (dir.clone(), super::fs::FsAccess::Read),
        )]);
        let mut st = super::HostState::new(
            super::Grants::of(&[super::Capability::Filesystem]),
            Vec::new(),
        );
        st.fs = Some(super::fs::FsSandbox::new(&map));

        assert_eq!(
            st.fs_read("inbox".into(), "a.txt".into()).unwrap(),
            b"hello".to_vec()
        );
        let entries = st.fs_list("inbox".into(), "".into(), false, 10).unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.path == "a.txt" && e.kind == "file")
        );
        let hits = st
            .fs_search("inbox".into(), "".into(), "HELLO".into(), 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a.txt");
        assert!(
            st.fs_read("inbox".into(), "../escape".into()).is_err(),
            "containment still holds through the host call"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `file-upload` is gated on `files.upload`, bounded to the read scope, and
    /// refuses a bad filename / an empty or over-large body / a spent write budget.
    #[test]
    fn file_upload_is_gated_scoped_and_bounded() {
        use super::slaq::api::host::Host as _;
        let mut denied = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(
            denied
                .file_upload("C1".into(), "a.txt".into(), b"x".to_vec())
                .unwrap_err()
                .contains("permission-denied"),
            "denied without files.upload"
        );
        // `files.upload` without `messages.read`: the scope is undefined → refused.
        let mut st = super::HostState::new(
            super::Grants::of(&[super::Capability::FilesUpload]),
            Vec::new(),
        );
        assert!(
            st.file_upload("C1".into(), "a.txt".into(), b"x".to_vec())
                .unwrap_err()
                .contains("permission-denied")
        );
        // With the read scope: the bounds are what refuse (before any network).
        st.grants = super::Grants::of(&[
            super::Capability::FilesUpload,
            super::Capability::MessagesRead,
        ]);
        assert!(
            st.file_upload("C1".into(), "  ".into(), b"x".to_vec())
                .is_err(),
            "an empty name"
        );
        assert!(
            st.file_upload("C1".into(), "../a".into(), b"x".to_vec())
                .is_err(),
            "a path in the name"
        );
        assert!(
            st.file_upload("C1".into(), "a.txt".into(), Vec::new())
                .is_err(),
            "an empty file"
        );
        assert!(
            st.file_upload(
                "C1".into(),
                "a.txt".into(),
                vec![0u8; super::MAX_UPLOAD_BYTES + 1]
            )
            .is_err(),
            "over the cap"
        );
        // A spent write budget refuses before the network.
        st.api_writes = super::MAX_WRITES_PER_DISPATCH;
        assert!(
            st.file_upload("C1".into(), "a.txt".into(), b"x".to_vec())
                .unwrap_err()
                .contains("too many writes")
        );
        // With the budget left it reaches the API client (none is installed here).
        st.api_writes = 0;
        assert!(
            st.file_upload("C1".into(), "a.txt".into(), b"x".to_vec())
                .unwrap_err()
                .contains("no API client")
        );
        // A control/bidi filename is sanitized (not rejected), so the upload still
        // reaches the client.
        assert!(
            st.file_upload("C1".into(), "a\u{202e}b.txt".into(), b"x".to_vec())
                .unwrap_err()
                .contains("no API client")
        );
        // Out of the read scope: refused.
        let req = super::Requested::parse(&serde_json::json!({
            "messages.read": { "scope": "all" },
            "files.upload": {}
        }))
        .unwrap();
        let mut consent = super::Consent::default();
        consent.grant(
            "m",
            &[
                super::Capability::FilesUpload,
                super::Capability::MessagesRead,
            ],
            Some(super::capability::ReadScope::Channels {
                channels: ["C_OK".to_string()].into_iter().collect(),
            }),
        );
        let mut out = super::HostState::new(
            super::Grants::from_consent(&req, &consent.entry("m")),
            Vec::new(),
        );
        assert!(
            out.file_upload("C_OTHER".into(), "a.txt".into(), b"x".to_vec())
                .unwrap_err()
                .contains("permission-denied")
        );
    }

    /// `chat-forward` is gated on `chat.forward`, bounded to the read scope for
    /// **both** the source and the destination, and shares the write budget.
    #[test]
    fn chat_forward_is_gated_scoped_and_bounded() {
        use super::slaq::api::host::Host as _;
        let mut denied = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(
            denied
                .chat_forward("C1".into(), "1.0".into(), "C2".into())
                .unwrap_err()
                .contains("permission-denied"),
            "denied without chat.forward"
        );
        // With the capability but no `messages.read`: refused (no scope).
        let mut st = super::HostState::new(
            super::Grants::of(&[super::Capability::ChatForward]),
            Vec::new(),
        );
        assert!(
            st.chat_forward("C1".into(), "1.0".into(), "C2".into())
                .unwrap_err()
                .contains("permission-denied")
        );
        st.grants = super::Grants::of(&[
            super::Capability::ChatForward,
            super::Capability::MessagesRead,
        ]);
        // A too-long reference is refused.
        assert!(
            st.chat_forward(
                "x".repeat(super::MAX_REF_BYTES + 1),
                "1.0".into(),
                "C2".into()
            )
            .is_err()
        );
        // Only a message the mod actually received may be forwarded.
        assert!(
            st.chat_forward("C1".into(), "9.9".into(), "C2".into())
                .unwrap_err()
                .contains("unknown or out-of-scope message")
        );
        // The delivered window holds the source.
        st.messages = std::sync::Arc::new(std::collections::HashMap::from([(
            ("C1".to_string(), "1.0".to_string()),
            super::Msg::text_only("1.0", "U0", "hi"),
        )]));
        // A forward costs two round trips, so one slot is not enough.
        st.api_writes = super::MAX_WRITES_PER_DISPATCH - 1;
        assert!(
            st.chat_forward("C1".into(), "1.0".into(), "C2".into())
                .unwrap_err()
                .contains("too many writes")
        );
        // With the budget it reaches the API client (none is installed here).
        st.api_writes = 0;
        assert!(
            st.chat_forward("C1".into(), "1.0".into(), "C2".into())
                .unwrap_err()
                .contains("no API client")
        );
        // Scoped to C_OK: the source **and** the destination must be inside.
        let req = super::Requested::parse(&serde_json::json!({
            "messages.read": { "scope": "all" },
            "chat.forward": {}
        }))
        .unwrap();
        let mut consent = super::Consent::default();
        consent.grant(
            "m",
            &[
                super::Capability::ChatForward,
                super::Capability::MessagesRead,
            ],
            Some(super::capability::ReadScope::Channels {
                channels: ["C_OK".to_string()].into_iter().collect(),
            }),
        );
        let mut out = super::HostState::new(
            super::Grants::from_consent(&req, &consent.entry("m")),
            Vec::new(),
        );
        assert!(
            out.chat_forward("C_OTHER".into(), "1.0".into(), "C_OK".into())
                .unwrap_err()
                .contains("permission-denied"),
            "the source is outside the scope"
        );
        assert!(
            out.chat_forward("C_OK".into(), "1.0".into(), "C_OTHER".into())
                .unwrap_err()
                .contains("permission-denied"),
            "the destination is outside the scope"
        );
    }

    /// `sql-query`/`sql-tx` need `store.sql`; a mod without the config is refused,
    /// and with it the statements run (the hardening itself is in `mods::sql`).
    #[test]
    fn sql_is_gated_and_is_configured() {
        use super::slaq::api::host::Host as _;
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        assert!(
            st.sql_query("SELECT 1".into(), vec![])
                .unwrap_err()
                .contains("permission-denied")
        );
        // Granted but not configured (the manifest declared no `store.sql`).
        st.grants = super::Grants::of(&[super::Capability::StoreSql]);
        assert!(
            st.sql_query("SELECT 1".into(), vec![])
                .unwrap_err()
                .contains("not configured")
        );
        // Configured: it runs, with parameters and a result.
        st.sql = Some(super::sql::ModSql::in_memory(super::sql::SqlConfig {
            max_bytes: 1024 * 1024,
            max_result_rows: 10,
        }));
        let r = st
            .sql_query("SELECT 1 + ?1".into(), vec![super::WitSqlValue::Int(2)])
            .unwrap();
        assert_eq!(r.columns, vec!["1 + ?1"]);
        assert!(matches!(r.rows[0][0], super::WitSqlValue::Int(3)));
        // A transaction reports one result per statement.
        st.sql_query("CREATE TABLE t (n INTEGER)".into(), vec![])
            .unwrap();
        let out = st
            .sql_tx(vec![
                ("INSERT INTO t (n) VALUES (1)".into(), vec![]),
                ("INSERT INTO t (n) VALUES (2)".into(), vec![]),
            ])
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].changes, 1);
    }

    /// Every host function is denied when its capability was not granted, and
    /// `grants.list` reports nothing.
    #[test]
    fn host_functions_are_denied_without_grants() {
        use super::slaq::api::host::Host as _;
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        for err in [
            st.get_message("C0T".into(), "1.0".into()).unwrap_err(),
            st.kv_get("k".into()).unwrap_err(),
            st.kv_set("k".into(), "v".into()).unwrap_err(),
            st.decorate("C0T".into(), "1.0".into(), "x".into())
                .unwrap_err(),
        ] {
            assert!(
                err.contains("permission-denied"),
                "expected denial, got: {err}"
            );
        }
        assert!(st.list_grants().is_empty(), "nothing granted");
    }

    /// A grant is visible through `grants.list` and unlocks only its own
    /// operation: `store.kv` does not open reads or decorations.
    #[test]
    fn grants_unlock_only_their_own_operation() {
        use super::slaq::api::host::Host as _;
        let mut st =
            super::HostState::new(super::Grants::of(&[super::Capability::StoreKv]), Vec::new());
        st.store = Some(super::store::ModStore::in_memory(
            Some(super::store::KvConfig { max_bytes: 1 << 20 }),
            None,
        ));
        assert_eq!(st.list_grants(), vec!["store.kv".to_string()]);
        assert!(st.kv_set("k".into(), "v".into()).is_ok());
        assert_eq!(st.kv_get("k".into()).unwrap(), Some("v".to_string()));
        assert!(st.get_message("C0T".into(), "1.0".into()).is_err());
        assert!(st.decorate("C0T".into(), "1.0".into(), "x".into()).is_err());
    }

    /// `list-channels`/`list-users` are gated and return the dispatch's directory.
    #[test]
    fn directory_is_gated_and_returned() {
        use super::slaq::api::host::Host as _;
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        st.directory = Some(std::sync::Arc::new(super::Directory {
            channels: vec![("C1".to_string(), "general".to_string())],
            users: vec![("U1".to_string(), "Ada".to_string())],
            files: Default::default(),
        }));
        // Denied without the capability.
        assert!(
            st.list_channels()
                .unwrap_err()
                .contains("permission-denied")
        );
        assert!(st.list_users().unwrap_err().contains("permission-denied"));
        // Granted: the directory comes back.
        st.grants = super::Grants::of(&[
            super::Capability::ChannelsList,
            super::Capability::UsersList,
        ]);
        assert_eq!(
            st.list_channels().unwrap(),
            vec![("C1".to_string(), "general".to_string())]
        );
        assert_eq!(
            st.list_users().unwrap(),
            vec![("U1".to_string(), "Ada".to_string())]
        );
        // Cross-capability isolation: only `channels.list` does not open users.
        st.grants = super::Grants::of(&[super::Capability::ChannelsList]);
        assert!(st.list_users().unwrap_err().contains("permission-denied"));
        // Outside a dispatch (no directory set) it is `unavailable`, not empty.
        let mut fresh = super::HostState::new(
            super::Grants::of(&[super::Capability::ChannelsList]),
            Vec::new(),
        );
        assert!(fresh.list_channels().unwrap_err().contains("unavailable"));
    }

    /// The entry count is capped, and an over-long name is truncated.
    #[test]
    fn directory_is_bounded() {
        use super::slaq::api::host::Host as _;
        let mut st = super::HostState::new(
            super::Grants::of(&[super::Capability::ChannelsList]),
            Vec::new(),
        );
        let many: Vec<(String, String)> = (0..super::MAX_DIRECTORY_ENTRIES + 5)
            .map(|i| (format!("C{i}"), format!("chan{i}")))
            .collect();
        st.directory = Some(std::sync::Arc::new(super::Directory {
            channels: many,
            users: vec![(
                "U1".to_string(),
                "x".repeat(super::MAX_DIRECTORY_NAME_BYTES + 50),
            )],
            files: Default::default(),
        }));
        assert_eq!(
            st.list_channels().unwrap().len(),
            super::MAX_DIRECTORY_ENTRIES
        );
        // The long name is truncated to the byte cap (users still needs its cap).
        st.grants = super::Grants::of(&[super::Capability::UsersList]);
        let users = st.list_users().unwrap();
        assert!(users[0].1.len() <= super::MAX_DIRECTORY_NAME_BYTES);
    }

    /// `settings.*` is ungated: a mod with no capability can read/write a
    /// non-secret setting but cannot write a secret.
    #[test]
    fn settings_are_ungated_and_a_mod_cannot_write_a_secret() {
        use super::slaq::api::host::Host as _;
        let schema = super::settings::Schema::parse(&serde_json::json!([
            { "key": "mode", "type": "enum", "values": ["a", "b"], "default": "a" },
            { "key": "key", "type": "secret" }
        ]))
        .unwrap();
        let path =
            std::env::temp_dir().join(format!("slaq-host-settings-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut st = super::HostState::new(super::Grants::default(), Vec::new());
        st.settings = std::sync::Arc::new(std::sync::RwLock::new(
            super::settings::Settings::load_at("dev.slaq.t", schema, path.clone()),
        ));
        // No capability is granted, yet a read works and returns the default.
        assert!(matches!(
            st.setting_get("mode".into()).unwrap(),
            Some(super::SettingValue::Text(s)) if s == "a"
        ));
        st.setting_set("mode".into(), super::SettingValue::Text("b".into()))
            .unwrap();
        assert!(matches!(
            st.setting_get("mode".into()).unwrap(),
            Some(super::SettingValue::Text(s)) if s == "b"
        ));
        // A secret is read-only to the mod; an unknown key is refused.
        assert!(
            st.setting_set("key".into(), super::SettingValue::Text("x".into()))
                .is_err()
        );
        assert!(st.setting_get("nope".into()).unwrap().is_none());
        assert!(
            st.setting_set("nope".into(), super::SettingValue::Text("x".into()))
                .is_err()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A non-empty `settings` array requires `ui.settings`; a bad schema is
    /// refused at load.
    #[test]
    fn a_non_empty_settings_requires_ui_settings() {
        let d = tmp("settings-cap");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        let write = |caps: &str, settings: &str| {
            fs::write(
                d.join("manifest.json"),
                format!(
                    r#"{{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{caps},"settings":{settings}}}"#
                ),
            )
            .unwrap();
        };
        write(
            r#"{"messages.read":{"scope":"all"}}"#,
            r#"[{"key":"lang","type":"string"}]"#,
        );
        assert!(load(&d).is_err(), "settings require ui.settings");
        write(
            r#"{"ui.settings":{}}"#,
            r#"[{"key":"lang","type":"string","default":"it"}]"#,
        );
        let loaded = load(&d).unwrap();
        assert_eq!(loaded.settings.fields.len(), 1);
        write(r#"{"ui.settings":{}}"#, r#"[{"key":"lang","type":"nope"}]"#);
        assert!(load(&d).is_err(), "invalid settings schema");
    }

    /// Load refuses a manifest that requests a capability this host does not
    /// implement, and one that requests a reserved name (fail closed).
    #[test]
    fn load_rejects_unavailable_capabilities() {
        let d = tmp("unavailable");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        let base = |caps: &str| {
            format!(
                r#"{{"manifestVersion":0,"id":"dev.slaq.t","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{caps}}}"#
            )
        };
        // `store.sql` is implemented now; `search.read` is the remaining
        // not-implemented name (the refusal is for that reason).
        fs::write(d.join("manifest.json"), base(r#"{"search.read":{}}"#)).unwrap();
        assert!(
            load(&d).is_err(),
            "an unimplemented capability must be refused"
        );
        fs::write(d.join("manifest.json"), base(r#"{"chat.delete":{}}"#)).unwrap();
        assert!(load(&d).is_err(), "a reserved capability must be refused");
        fs::write(d.join("manifest.json"), base(r#"{"no.such.thing":{}}"#)).unwrap();
        assert!(load(&d).is_err(), "an unknown capability must be refused");
    }

    /// The id keys the consent record, so it must be reverse-DNS (schema pattern).
    #[test]
    fn rejects_an_invalid_id() {
        let d = tmp("bad-id");
        fs::write(d.join("mod.wasm"), b"\0asm").unwrap();
        let write_id = |id: &str| {
            fs::write(
                d.join("manifest.json"),
                format!(
                    r#"{{"manifestVersion":0,"id":"{id}","name":"t","version":"0.1.0","apiVersion":"0.1","entry":{{"type":"wasm","path":"mod.wasm"}},"capabilities":{{}}}}"#
                ),
            )
            .unwrap();
        };
        for bad in [
            "t",
            "No.Dot",
            ".lead",
            "trail.",
            "a..b",
            "UPPER.case",
            "has space.x",
            // A path separator (or traversal) must never reach the data dir.
            "a/b",
            "../esc",
            "a\\b",
        ] {
            write_id(bad);
            assert!(load(&d).is_err(), "id {bad:?} must be refused");
        }
        write_id("dev.slaq.ok");
        assert!(load(&d).is_ok(), "a reverse-DNS id must be accepted");
    }

    /// The **consent** read scope is enforced per channel: even with the manifest
    /// declaring `scope:"all"`, a grant recorded for `[C0T]` suppresses the event
    /// on any other channel.
    #[test]
    fn read_scope_is_enforced_per_channel() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hello-mod");
        let root = tmp("scope");
        let d = root.join("m");
        fs::create_dir_all(&d).unwrap();
        fs::copy(fixture.join("mod.wasm"), d.join("mod.wasm")).unwrap();
        fs::write(
            d.join("manifest.json"),
            r#"{"manifestVersion":0,"id":"dev.slaq.scoped","name":"scoped","version":"0.1.0","apiVersion":"0.1","entry":{"type":"wasm","path":"mod.wasm"},"capabilities":{"messages.read":{"scope":"all"},"store.kv":{"maxBytes":1048576},"ui.message_decoration":{}},"events":["message.created","message.updated"]}"#,
        )
        .unwrap();
        let mut host = super::Host::load_root(
            &root,
            super::engine().unwrap(),
            &consent_scoped("dev.slaq.scoped", &["C0T"]),
            &std::sync::Arc::new(super::Directory::empty()),
            &root,
        );
        // In scope: the mod runs and decorates.
        let inside = host.decorate("C0T", &[super::Msg::text_only("1.000000", "U0", "hi")]);
        assert!(
            inside.values().flatten().any(|t| t.contains("hello #1")),
            "in scope: {inside:?}"
        );
        // Out of scope: no event, no decoration.
        let outside = host.decorate("COTHER", &[super::Msg::text_only("2.000000", "U0", "hi")]);
        assert!(
            outside.is_empty(),
            "out of scope must be suppressed: {outside:?}"
        );
    }

    /// `get_message` enforces the scope too, not only event delivery.
    #[test]
    fn get_message_enforces_the_scope() {
        use super::slaq::api::host::Host as _;
        let mut consent = super::Consent::default();
        consent.grant(
            "m",
            &[super::Capability::MessagesRead],
            Some(super::capability::ReadScope::Channels {
                channels: ["C1".to_string()].into_iter().collect(),
            }),
        );
        // The manifest declares the wider "all"; consent still wins.
        let req =
            super::Requested::parse(&serde_json::json!({ "messages.read": { "scope": "all" } }))
                .unwrap();
        let grants = super::Grants::from_consent(&req, &consent.entry("m"));
        let mut st = super::HostState::new(grants, Vec::new());
        st.messages = std::sync::Arc::new(std::collections::HashMap::from([
            (
                ("C1".to_string(), "1.0".to_string()),
                super::Msg {
                    author: "U0".to_string(),
                    text: "hi".to_string(),
                    ..Default::default()
                },
            ),
            (
                ("C2".to_string(), "2.0".to_string()),
                super::Msg {
                    author: "U0".to_string(),
                    text: "hi".to_string(),
                    ..Default::default()
                },
            ),
        ]));
        assert!(st.get_message("C1".into(), "1.0".into()).is_ok());
        let err = st.get_message("C2".into(), "2.0".into()).unwrap_err();
        assert!(err.contains("permission-denied"), "out of scope: {err}");
    }
}
