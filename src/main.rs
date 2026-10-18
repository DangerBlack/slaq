//! slaq — lightweight native Slack client with call support.
//!
//! Uses Slack's internal xoxc/xoxd auth protocol against private
//! `/api/*` endpoints and `wss://wss-primary.slack.com` WebSocket.
//!
//! Architecture:
//!   app/      — interface-independent core (state, actions)
//!   auth/     — token extraction, credential storage, cookie decryption
//!   api/      — REST API client (conversations, users, search, files)
//!   ws/       — WebSocket client (real-time events, presence)
//!   calls/    — native huddles (signaling, media, audio)
//!   huddle/   — opening huddles in the browser
//!   notify/   — desktop notifications from Slack events
//!   store/    — local SQLite cache (channels, messages, users)
//!   gui/      — native GUI (egui)
//!   ui/       — Terminal UI

// #![deny(unused_crate_dependencies)]

use anyhow::{Context, Result};

/// The user agent we introduce ourselves with.
///
/// Honest on purpose: an unofficial client shows up in the workspace audit log
/// anyway, and passing ourselves off as Slack Desktop would turn an explainable
/// line into a compromised-credentials alert.
pub const USER_AGENT: &str =
    concat!("slaq/", env!("CARGO_PKG_VERSION"), " (personal TUI/GUI client)");
use clap::{Parser, Subcommand};

mod auth;
mod api;
mod app;
mod calls;
#[cfg(feature = "gui")]
mod gui;
// The GUI design system: colour roles, by name. GUI-only (it speaks egui).
#[cfg(feature = "gui")]
mod theme;
mod huddle;
mod net;
mod notify;
mod store;
mod ui;
mod ws;
#[cfg(feature = "mods")]
mod mods;

#[macro_use]
extern crate rust_i18n;

// Interface strings live in `locales/<lang>.yml`. Italian is the source
// language, so it is also the fallback; the language is chosen at startup.
i18n!("locales", fallback = "it");

/// Chooses the interface language: `SLAQ_LANG` wins, otherwise the system
/// locale, defaulting to Italian (the app's original language).
fn init_locale() {
    let chosen = std::env::var("SLAQ_LANG")
        .ok()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| {
            let sys = std::env::var("LC_ALL")
                .or_else(|_| std::env::var("LC_MESSAGES"))
                .or_else(|_| std::env::var("LANG"))
                .unwrap_or_default()
                .to_lowercase();
            if sys.starts_with("en") {
                "en".to_string()
            } else {
                "it".to_string()
            }
        });
    rust_i18n::set_locale(&chosen);
}

/// Strips session credentials from text before it is printed.
///
/// `listen` dumps raw events for diagnosis, and some carry secrets in their
/// payload: `reconnect_url` embeds the `xoxc` token in its query string. A
/// token leaked into a terminal, a scrollback or a pasted bug report is an
/// account-owning credential, so it is removed here.
fn redact_secrets(text: &str) -> String {
    let mut out = text.to_string();
    for prefix in ["xoxc-", "xoxd-"] {
        while let Some(start) = out.find(prefix) {
            let end = out[start..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .map(|off| start + off)
                .unwrap_or(out.len());
            out.replace_range(start..end, "<redacted>");
        }
    }
    out
}

/// Lightweight native Slack client
#[derive(Parser)]
#[command(name = "slaq", version, about = "Lightweight native Slack client with call support")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the graphical client
    #[cfg(feature = "gui")]
    Gui {
        /// Workspace URL (e.g., myworkspace.slack.com)
        #[arg(short, long)]
        workspace: Option<String>,
        /// Open this channel at startup instead of the first one
        #[arg(short, long)]
        channel: Option<String>,
        /// Open this thread (root message `ts`) at startup
        #[arg(long, requires = "channel")]
        thread: Option<String>,
        /// Start with sample data, no login and no network (for UI work)
        #[arg(long)]
        demo: bool,
    },
    /// Start the interactive TUI client
    Tui {
        /// Workspace URL (e.g., myworkspace.slack.com)
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Authenticate with Slack (set up tokens)
    Login {
        /// xoxc session token
        #[arg(long)]
        xoxc: Option<String>,
        /// xoxd session cookie
        #[arg(long)]
        xoxd: Option<String>,
        /// Extract tokens from Slack Desktop's local storage
        #[arg(long)]
        from_desktop: bool,
        /// Log in through the installed Chromium/Chrome over CDP (Linux)
        #[arg(long)]
        browser: bool,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Send a message to a channel
    Send {
        /// Channel ID (e.g., C0123456789)
        #[arg(short, long)]
        channel: String,
        /// Message text
        #[arg(short, long)]
        text: String,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Run a slash command in a channel (e.g. `/remind me in 1h`)
    Slash {
        /// Channel ID (e.g., C0123456789)
        #[arg(short, long)]
        channel: String,
        /// The command with its arguments, including the leading slash
        #[arg(short, long)]
        text: String,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// List a channel's pinned messages
    Pins {
        /// Channel ID (e.g., C0123456789)
        #[arg(short, long)]
        channel: String,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// List the messages saved for later ("Later")
    Saved {
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// List the threads you are subscribed to (the "Threads" inbox)
    Threads {
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Create a new channel
    NewChannel {
        /// Channel name (e.g. `marketing`)
        name: String,
        /// Make it private
        #[arg(long)]
        private: bool,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Open a group DM with one or more people (ids, or a piece of a name)
    GroupDm {
        /// People, by user id or a piece of their name
        users: Vec<String>,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Rename a channel
    ChannelRename {
        /// Channel ID (e.g., C0123456789)
        #[arg(short, long)]
        channel: String,
        /// New name
        #[arg(short, long)]
        name: String,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Invite people to a channel
    ChannelInvite {
        /// Channel ID (e.g., C0123456789)
        #[arg(short, long)]
        channel: String,
        /// People, by user id or a piece of their name
        users: Vec<String>,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Remove people from a channel
    ChannelKick {
        /// Channel ID (e.g., C0123456789)
        #[arg(short, long)]
        channel: String,
        /// People, by user id or a piece of their name
        users: Vec<String>,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Leave a channel
    ChannelLeave {
        /// Channel ID (e.g., C0123456789)
        #[arg(short, long)]
        channel: String,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Archive a channel
    ChannelArchive {
        /// Channel ID (e.g., C0123456789)
        #[arg(short, long)]
        channel: String,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Listen for real-time events (debug)
    Listen {
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
        /// Event type filter
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// List channels in the workspace (served from the local cache)
    Channels {
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
        /// List private channels too
        #[arg(long)]
        all: bool,
        /// Re-fetch from Slack even if the cache is still fresh
        #[arg(long)]
        refresh: bool,
    },
    /// Probe rooms.join for a channel. Creates a real huddle, then leaves.
    Call {
        /// Channel ID for the call
        #[arg(short, long)]
        channel: String,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
        /// Print tokens and credentials instead of masking them
        #[arg(long)]
        show_secrets: bool,
        /// Also open the Chime signaling socket and JOIN. Makes you a real
        /// participant: the huddle goes live for everyone in the channel.
        #[arg(long)]
        connect: bool,
        /// Go all the way: negotiate audio media with the SFU (implies
        /// --connect). Joins muted.
        #[arg(long)]
        audio: bool,
        /// Stay connected this many seconds before leaving
        #[arg(long, default_value_t = 0)]
        hold: u64,
        /// Write our SDP offer here (and the answer alongside it) for diffing
        /// against the official client's
        #[arg(long)]
        dump_sdp: Option<std::path::PathBuf>,

        /// Declares to the SFU that the microphone is open. The probe transmits
        /// either way: this only aligns the declaration with what it does.
        #[arg(long)]
        unmuted: bool,

        /// Stay in the call until you hang up, with mute from the keyboard.
        #[arg(long, short)]
        interactive: bool,

        /// Microphone to use: a piece of the name is enough. Empty = default.
        #[arg(long)]
        mic: Option<String>,

        /// Speakers to use: a piece of the name is enough.
        #[arg(long)]
        speaker: Option<String>,

        /// Raw audio: no echo cancellation and no noise suppression.
        #[arg(long)]
        raw_audio: bool,
    },
    /// [diagnostic] Real acoustic echo test: plays a WAV through slaq's own
    /// playback path while the microphone is open, then measures how much the
    /// AEC removed. Not part of normal use.
    #[command(hide = true)]
    AecProbe {
        /// WAV file to play (16-bit PCM, mono or stereo)
        #[arg(long)]
        wav: std::path::PathBuf,
        /// Play only the first n seconds (default: the whole file)
        #[arg(long)]
        seconds: Option<f64>,
        /// Microphone to use: part of the name is enough (default: the system one)
        #[arg(long)]
        mic: Option<String>,
        /// Speaker to use: part of the name is enough (default: the system one)
        #[arg(long)]
        speaker: Option<String>,
        /// Raw audio: no echo cancellation (for comparison)
        #[arg(long)]
        raw_audio: bool,
    },
    /// Dump channel history (served from the local cache)
    History {
        /// Channel ID
        #[arg(short, long)]
        channel: String,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
        /// How many messages to show
        #[arg(short, long, default_value_t = 50)]
        limit: u32,
        /// Skip the incremental fetch and read the cache only (offline)
        #[arg(long)]
        offline: bool,
        /// Backfill this many older messages from before the cached window
        #[arg(long)]
        older: Option<u32>,
    },
    /// Show a locally-generated test notification. Sends nothing to Slack.
    NotifyTest {
        /// Pretend this arrived in a DM instead of a channel mention
        #[arg(long)]
        dm: bool,
    },
    /// Upload a file to a channel
    Upload {
        /// Channel ID
        #[arg(short, long)]
        channel: String,
        /// File to send
        file: std::path::PathBuf,
        /// Optional message to go with it
        #[arg(short, long)]
        text: Option<String>,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Download a file shared in a channel into the downloads folder
    Download {
        /// URL of the file (url_private_download)
        url: String,
        /// Name to save it as
        #[arg(short, long)]
        name: String,
        /// Open it afterwards
        #[arg(long)]
        open: bool,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
    },
    /// Open a channel's huddle in the browser
    Huddle {
        /// Channel ID
        #[arg(short, long)]
        channel: String,
        /// Workspace URL
        #[arg(short, long)]
        workspace: Option<String>,
        /// Print the URL instead of opening it
        #[arg(long)]
        print: bool,
    },
    /// Inspect or clear the local cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Run a mod against a synthetic message (headless; slice-1 host)
    #[cfg(feature = "mods")]
    Mod {
        /// Mod folder: manifest.json + the component
        #[arg(long)]
        dir: std::path::PathBuf,
        /// Synthetic event: channel id
        #[arg(long, default_value = "C0EXAMPLE")]
        channel: String,
        /// Synthetic event: message ts
        #[arg(long, default_value = "1.000000")]
        ts: String,
        /// Synthetic event: author id
        #[arg(long, default_value = "U0EXAMPLE")]
        author: String,
        /// Synthetic event: message text
        #[arg(long, default_value = "hello from the host")]
        text: String,
        /// Capabilities to grant for this one-shot run (repeatable). Without any
        /// grant the mod is denied every host call.
        #[arg(long = "grant", value_name = "CAPABILITY")]
        grant: Vec<String>,
        /// Grant every capability this host implements (dev/testing shortcut).
        #[arg(long)]
        grant_all: bool,
        /// Map a declared filesystem slot to a directory (repeatable, NAME=DIR)
        #[arg(long = "fs-slot", value_name = "NAME=DIR")]
        fs_slot: Vec<String>,
    },
    /// Inspect installed mods and manage their consent (grants)
    #[cfg(feature = "mods")]
    Mods {
        #[command(subcommand)]
        action: ModsAction,
    },
}

/// Consent management for installed mods (the install prompt lands later; this
/// is the same store it will write to).
#[cfg(feature = "mods")]
#[derive(Subcommand)]
enum ModsAction {
    /// List installed mods with their requested and granted capabilities
    List,
    /// Grant capabilities to a mod (by manifest id)
    Grant {
        /// Mod id, as in the manifest (reverse-DNS)
        id: String,
        /// Capability names, e.g. `messages.read store.kv`
        #[arg(required = true)]
        capabilities: Vec<String>,
    },
    /// Revoke capabilities from a mod
    Revoke {
        /// Mod id, as in the manifest (reverse-DNS)
        id: String,
        /// Capability names, e.g. `messages.read store.kv`
        #[arg(required = true)]
        capabilities: Vec<String>,
    },
    /// Map a `filesystem` slot to a real directory (needs the `filesystem` grant)
    Map {
        /// Mod id, as in the manifest (reverse-DNS)
        id: String,
        /// Slot name, as the installed manifest declares it
        slot: String,
        /// Directory the mod may access through the slot
        dir: std::path::PathBuf,
    },
    /// Remove a `filesystem` slot mapping
    Unmap {
        /// Mod id, as in the manifest (reverse-DNS)
        id: String,
        /// Slot name
        slot: String,
    },
    /// Choose the channels a mod may read (`messages.read`), beyond what its
    /// manifest suggests
    Scope {
        /// Mod id, as in the manifest (reverse-DNS)
        id: String,
        /// Every channel the client can see
        #[arg(long, conflicts_with_all = ["reset", "channels"])]
        all: bool,
        /// Go back to the scope the manifest declares
        #[arg(long, conflicts_with = "channels")]
        reset: bool,
        /// Channel ids (e.g. `C0123ABCD D0456EFGH`)
        channels: Vec<String>,
    },
    /// Keep a mod's saved secrets for its installed (updated) package
    KeepSecrets {
        /// Mod id, as in the manifest (reverse-DNS)
        id: String,
    },
    /// Forget the secrets saved for an earlier package of a mod
    ForgetSecrets {
        /// Mod id, as in the manifest (reverse-DNS)
        id: String,
    },
    /// Add or remove the hosts a mod may reach (`net.egress`), beyond what its
    /// manifest lists
    Egress {
        /// Mod id, as in the manifest (reverse-DNS)
        id: String,
        #[command(subcommand)]
        op: EgressOp,
    },
}

/// `slaq mods egress <id> add|rm <host>`.
#[cfg(feature = "mods")]
#[derive(Subcommand)]
enum EgressOp {
    /// Allow a host (`host` or `host:port`, e.g. `localhost:11434`)
    Add { host: String },
    /// Stop allowing a host (one the manifest lists, or one you added)
    Rm { host: String },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Show cache location, size and contents
    Status,
    /// Delete everything in the cache
    Clear,
}

/// Parses capability names, refusing unknown or not-yet-implemented ones.
#[cfg(feature = "mods")]
fn parse_capabilities(names: &[String]) -> anyhow::Result<Vec<mods::capability::Capability>> {
    names
        .iter()
        .map(|n| {
            mods::capability::Capability::from_name(n).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown or not-implemented capability: {}",
                    mods::sanitize_log(n)
                )
            })
        })
        .collect()
}

/// The grant set for a one-shot `slaq mod` run.
#[cfg(feature = "mods")]
fn mod_grant_set(
    names: &[String],
    all: bool,
) -> anyhow::Result<std::collections::BTreeSet<mods::capability::Capability>> {
    if all {
        if !names.is_empty() {
            anyhow::bail!("--grant-all cannot be combined with --grant");
        }
        return Ok(mods::capability::Capability::ALL.into_iter().collect());
    }
    Ok(parse_capabilities(names)?.into_iter().collect())
}

/// Parses `--fs-slot NAME=DIR` entries for a one-shot `slaq mod` run into the
/// `filesystem` mapping: slot name -> (canonical directory, declared access).
///
/// `NAME` must be a slot the package declares; the access is the manifest's
/// declared access (the runner builds the sandbox from this mapping). `DIR` must
/// exist, be a directory, and not be inside a reserved slaq directory. A repeated
/// slot is last-wins (a `BTreeMap` insert).
#[cfg(feature = "mods")]
fn parse_fs_slots(
    dir: &std::path::Path,
    entries: &[String],
) -> anyhow::Result<std::collections::BTreeMap<String, (std::path::PathBuf, mods::fs::FsAccess)>> {
    if entries.is_empty() {
        return Ok(std::collections::BTreeMap::new());
    }
    // Load the package to know which slots it declares and at what access.
    let loaded = mods::load(dir).map_err(|e| {
        anyhow::anyhow!(
            "cannot read {}: {}",
            mods::sanitize_log(&dir.display().to_string()),
            mods::sanitize_log(&format!("{e:#}"))
        )
    })?;
    let declared = loaded.requested.fs_slots();
    let mut out = std::collections::BTreeMap::new();
    for entry in entries {
        let Some((name, raw_dir)) = entry.split_once('=') else {
            anyhow::bail!(
                "--fs-slot must be NAME=DIR (missing '='): {}",
                mods::sanitize_log(entry)
            );
        };
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("--fs-slot has an empty slot name: {}", mods::sanitize_log(entry));
        }
        let Some(slot) = declared.iter().find(|s| s.name == name) else {
            anyhow::bail!(
                "{} does not declare a filesystem slot named {}",
                mods::sanitize_log(&dir.display().to_string()),
                mods::sanitize_log(name)
            );
        };
        let raw = std::path::PathBuf::from(raw_dir);
        // The directory must exist; canonicalize so the mapping is absolute and
        // symlink-free (the host resolves against it anyway).
        let canon = std::fs::canonicalize(&raw).map_err(|e| {
            anyhow::anyhow!("cannot use {}: {e}", mods::sanitize_log(&raw.display().to_string()))
        })?;
        if !canon.is_dir() {
            anyhow::bail!(
                "{} is not a directory",
                mods::sanitize_log(&canon.display().to_string())
            );
        }
        if mods::fs::is_reserved_dir(&canon) {
            anyhow::bail!(
                "{} is inside a reserved slaq directory and cannot be mapped",
                mods::sanitize_log(&canon.display().to_string())
            );
        }
        out.insert(name.to_string(), (canon, slot.access));
    }
    Ok(out)
}

/// The read scope to record when granting `messages.read`: the scope the
/// installed package declares (the consent's initial value). Enforcement later
/// uses `min(consent, manifest)`, so a package update can neither widen consent
/// nor escape the manifest. Returns `None` when `messages.read` is not granted.
#[cfg(feature = "mods")]
/// The detail to record when granting a capability that carries one: the
/// installed package's declared read scope (for `messages.read`) and egress
/// allow-list (for `net.egress`). Requires the package to be installed.
fn grant_details(
    id: &str,
    caps: &[mods::capability::Capability],
) -> anyhow::Result<(Option<mods::capability::ReadScope>, Option<Vec<String>>)> {
    use mods::capability::Capability;
    let wants_scope = caps.contains(&Capability::MessagesRead);
    let wants_egress = caps.contains(&Capability::NetEgress);
    if !wants_scope && !wants_egress {
        return Ok((None, None));
    }
    let dir = mods::default_dir()?;
    let entries =
        std::fs::read_dir(&dir).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", dir.display()))?;
    let mut dirs: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join("manifest.json").is_file())
        .collect();
    dirs.sort();
    for d in dirs {
        if let Ok(loaded) = mods::load(&d) {
            if loaded.manifest.id != id {
                continue;
            }
            let scope = if wants_scope {
                if !loaded.requested.caps().contains(&Capability::MessagesRead) {
                    anyhow::bail!(
                        "the installed mod {} does not declare messages.read",
                        mods::sanitize_log(id)
                    );
                }
                Some(loaded.requested.read_scope().clone())
            } else {
                None
            };
            let egress = if wants_egress {
                if !loaded.requested.caps().contains(&Capability::NetEgress) {
                    anyhow::bail!(
                        "the installed mod {} does not declare net.egress",
                        mods::sanitize_log(id)
                    );
                }
                Some(loaded.requested.egress().to_vec())
            } else {
                None
            };
            return Ok((scope, egress));
        }
    }
    anyhow::bail!(
        "mod {} is not installed; install it before granting its capabilities",
        mods::sanitize_log(id)
    )
}

/// What an installed package's manifest requests, by manifest id. `what` names
/// the operation for the not-installed error.
#[cfg(feature = "mods")]
fn installed_request(id: &str, what: &str) -> anyhow::Result<mods::capability::Requested> {
    installed_package(id, what).map(|l| l.requested)
}

/// `slaq mods keep-secrets|forget-secrets`: the secrets saved for an earlier
/// package of `id` are kept for the installed one, or forgotten.
#[cfg(feature = "mods")]
fn mod_secrets(id: &str, keep: bool) -> anyhow::Result<()> {
    if !mods::valid_mod_id(id) {
        anyhow::bail!(
            "invalid mod id (reverse-DNS expected): {}",
            mods::sanitize_log(id)
        );
    }
    let loaded = installed_package(id, "deciding about its secrets")?;
    let fp = mods::package_fingerprint(&loaded)?;
    let keys = mods::settings::stale_secret_keys(id, &loaded.settings, &fp);
    if keys.is_empty() {
        println!(
            "{}: no secrets waiting (none saved, or already kept for this version)",
            mods::sanitize_log(id)
        );
        return Ok(());
    }
    let names = keys.iter().map(|k| mods::sanitize_log(k)).collect::<Vec<_>>().join(", ");
    if keep {
        mods::settings::rebind_secrets(id, &keys, &fp)?;
        println!("kept for this version of {}: {names}", mods::sanitize_log(id));
    } else {
        mods::settings::forget_secrets(id, &keys)?;
        println!("forgot for {}: {names}", mods::sanitize_log(id));
    }
    println!("  restart slaq or reload the mods to apply");
    Ok(())
}

/// An installed package by manifest id. `what` names the operation for the
/// not-installed error.
#[cfg(feature = "mods")]
fn installed_package(id: &str, what: &str) -> anyhow::Result<mods::Loaded> {
    let dir = mods::default_dir()?;
    let entries =
        std::fs::read_dir(&dir).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", dir.display()))?;
    let mut dirs: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join("manifest.json").is_file())
        .collect();
    dirs.sort();
    for d in dirs {
        if let Ok(loaded) = mods::load(&d)
            && loaded.manifest.id == id
        {
            return Ok(loaded);
        }
    }
    anyhow::bail!(
        "mod {} is not installed; install it before {what}",
        mods::sanitize_log(id)
    )
}

/// The `filesystem` slots an installed package declares, by manifest id. Used to
/// validate a `slaq mods map` slot and to display the mappings.
#[cfg(feature = "mods")]
fn declared_fs_slots(id: &str) -> anyhow::Result<Vec<mods::fs::FsSlot>> {
    let dir = mods::default_dir()?;
    let entries =
        std::fs::read_dir(&dir).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", dir.display()))?;
    let mut dirs: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join("manifest.json").is_file())
        .collect();
    dirs.sort();
    for d in dirs {
        if let Ok(loaded) = mods::load(&d)
            && loaded.manifest.id == id
        {
            return Ok(loaded.requested.fs_slots().to_vec());
        }
    }
    anyhow::bail!(
        "mod {} is not installed; install it before mapping its filesystem slots",
        mods::sanitize_log(id)
    )
}

/// Lists installed mods with their requested and granted capabilities.
#[cfg(feature = "mods")]
fn list_mods() -> anyhow::Result<()> {
    let dir = mods::default_dir()?;
    let path = mods::consent::default_path()?;
    let consent = match mods::consent::Consent::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "warning: consent store not loaded ({}): {}",
                mods::sanitize_log(&path.display().to_string()),
                mods::sanitize_log(&format!("{e:#}"))
            );
            mods::consent::Consent::default()
        }
    };
    println!("mods dir: {}", mods::sanitize_log(&dir.display().to_string()));
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("  (none installed)");
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "cannot read {}: {}",
                mods::sanitize_log(&dir.display().to_string()),
                mods::sanitize_log(&e.to_string())
            ));
        }
    };
    let mut dirs: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join("manifest.json").is_file())
        .collect();
    dirs.sort();
    if dirs.is_empty() {
        println!("  (none installed)");
        return Ok(());
    }
    for d in dirs {
        match mods::load(&d) {
            Ok(loaded) => {
                let id = &loaded.manifest.id;
                let requested = loaded.requested.names();
                let granted = consent.granted_names(id);
                println!(
                    "{} ({})",
                    mods::sanitize_log(id),
                    mods::sanitize_log(&loaded.manifest.name)
                );
                println!("  requested: {}", join_or_dash(&requested));
                let entry = consent.entry(id);
                if let Some(scope) =
                    mods::consent::Grants::effective_read_scope(&loaded.requested, &entry)
                {
                    let mark = if entry.user_scope().is_some() {
                        " (your choice)"
                    } else {
                        ""
                    };
                    println!("  read scope: {}{mark}", describe_scope(&scope));
                }
                if loaded.requested.caps().contains(&mods::capability::Capability::NetEgress) {
                    let added = entry.egress_added();
                    // What the user configured, shown even before the grant
                    // (the runtime reaches it only once net.egress is granted).
                    let granted_egress = entry.caps().contains(&mods::capability::Capability::NetEgress);
                    let configured: Vec<String> = if granted_egress {
                        mods::consent::Grants::effective_egress(&loaded.requested, &entry)
                    } else {
                        entry
                            .egress()
                            .iter()
                            .filter(|h| loaded.requested.egress().contains(h))
                            .chain(added.iter())
                            .cloned()
                            .collect()
                    };
                    let reach: Vec<String> = configured
                            .iter()
                            .map(|h| {
                                if added.contains(h) {
                                    format!("{} (added)", mods::sanitize_log(h))
                                } else {
                                    mods::sanitize_log(h)
                                }
                            })
                            .collect();
                    let not_yet = if granted_egress || reach.is_empty() {
                        ""
                    } else {
                        " (not granted)"
                    };
                    println!("  egress:    {}{not_yet}", join_or_dash(&reach));
                }
                println!("  granted:   {}", join_or_dash(&granted));
                if let Ok(fp) = mods::package_fingerprint(&loaded) {
                    let stale = mods::settings::stale_secret_keys(id, &loaded.settings, &fp);
                    if !stale.is_empty() {
                        println!(
                            "  secrets:   {} saved for an earlier version (slaq mods keep-secrets {})",
                            join_or_dash(&stale.iter().map(|k| mods::sanitize_log(k)).collect::<Vec<_>>()),
                            mods::sanitize_log(id)
                        );
                    }
                }
                // The `filesystem` slot mappings the user set (or has yet to).
                for slot in loaded.requested.fs_slots() {
                    match consent.entry(id).fs_slots().get(&slot.name) {
                        Some(m) => println!(
                            "  fs slot {}: {} ({})",
                            mods::sanitize_log(&slot.name),
                            mods::sanitize_log(&m.dir),
                            m.access.as_str()
                        ),
                        None => {
                            println!("  fs slot {}: (unmapped)", mods::sanitize_log(&slot.name))
                        }
                    }
                }
            }
            Err(e) => println!(
                "  {}: invalid package: {}",
                mods::sanitize_log(&d.display().to_string()),
                mods::sanitize_log(&format!("{e:#}"))
            ),
        }
    }
    Ok(())
}

#[cfg(feature = "mods")]
fn join_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

/// A human-readable rendering of an enforced read scope.
#[cfg(feature = "mods")]
fn describe_scope(scope: &mods::capability::ReadScope) -> String {
    match scope {
        mods::capability::ReadScope::All => "all".to_string(),
        mods::capability::ReadScope::Channels { channels } => format!(
            "channels [{}]",
            channels
                .iter()
                .map(|c| mods::sanitize_log(c))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Resolves people given as user ids or as a piece of their name, through the
/// cached user list. Ids pass through untouched.
fn resolve_users(store: &store::Store, queries: &[String]) -> Vec<String> {
    let names = store.user_names().unwrap_or_default();
    queries
        .iter()
        .filter_map(|q| {
            if names.contains_key(q.as_str()) || q.starts_with('U') || q.starts_with('W') {
                return Some(q.clone());
            }
            let needle = q.to_lowercase();
            names
                .iter()
                .find(|(_, n)| n.to_lowercase().contains(&needle))
                .map(|(id, _)| id.clone())
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    // Running as the out-of-process mod compile worker: do the compile and exit
    // before starting the UI or parsing arguments. Keyed on an explicit hidden
    // argv sentinel (plus the env paths), so a stray environment variable cannot
    // turn a normal invocation into a compiler.
    #[cfg(feature = "mods")]
    if std::env::args_os().nth(1).is_some_and(|a| a == mods::WORKER_ARG)
        && std::env::var_os(mods::COMPILE_IN_ENV).is_some()
    {
        return mods::compile_worker();
    }

    init_locale();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Tui { workspace } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let mut app = app::App::new(creds);
            ui::run(&mut app).await?;
        }
        #[cfg(feature = "gui")]
        Command::Gui {
            workspace,
            channel,
            thread,
            demo,
        } => {
            let app = if demo {
                app::App::demo(channel.clone())
            } else {
                let creds = auth::load_credentials(workspace.as_deref()).ok();
                app::App::from_optional_creds(creds, channel.clone())
            };
            // Open the requested thread up front (a debugging aid: the GUI then
            // loads its messages like any other thread). The demo workspace has
            // no credentials, so it is skipped there.
            if !demo {
                if let (Some(c), Some(t)) = (channel, thread) {
                    app.open_thread(c, t);
                }
            }
            gui::run(app)?;
        }
        Command::Login {
            xoxc,
            xoxd,
            from_desktop,
            browser,
            workspace,
        } => {
            if browser {
                auth::login_via_browser(workspace.as_deref()).await?;
                return Ok(());
            }
            if from_desktop {
                auth::extract_from_desktop().await?;
                println!("{}", t!("cli.login_tokens_desktop"));
            } else if let (Some(token), Some(cookie)) = (xoxc, xoxd) {
                let ws = workspace
                    .clone()
                    .unwrap_or_else(|| "app.slack.com".into());
                auth::store_credentials(&ws, &token, &cookie)?;
                println!("{}", t!("cli.login_tokens_stored", workspace = ws));
            } else {
                anyhow::bail!("Provide --xoxc and --xoxd, or use --from-desktop");
            }
        }
        Command::Send {
            channel,
            text,
            workspace,
        } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            let resp = client
                .chat_post_message(&channel, &text)
                .await
                .context("Failed to send message")?;
            println!("{}", t!("cli.send_sent", ts = resp.ts));
        }
        Command::Slash {
            channel,
            text,
            workspace,
        } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            let (command, args) = app::split_slash_command(&text)
                .context("a slash command must start with `/`")?;
            match client.chat_command(&channel, command, args).await? {
                Some(response) => println!("{response}"),
                None => println!("{}", t!("cli.slash_sent")),
            }
        }
        Command::Pins {
            channel,
            workspace,
        } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            let items = client.pins_list(&channel).await?;
            let total = items.len();
            for v in items {
                if let Some(m) = app::parse_message(&v) {
                    println!(
                        "[{}] <{}> {} [reactions:{} replies:{}]",
                        m.ts,
                        m.user,
                        m.text,
                        m.reactions.len(),
                        m.reply_count.unwrap_or(0)
                    );
                }
            }
            println!("{}", t!("cli.pins_total", count = total));
        }
        Command::Saved { workspace } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            let (items, counts) = client.saved_list(50).await?;
            for it in &items {
                let text = client
                    .conversations_replies(&it.item_id, &it.ts)
                    .await
                    .ok()
                    .and_then(|msgs| {
                        msgs.into_iter().find(|m| {
                            m.get("ts").and_then(|v| v.as_str()) == Some(it.ts.as_str())
                        })
                    })
                    .and_then(|v| app::parse_message(&v))
                    .map(|m| m.text)
                    .unwrap_or_default();
                println!(
                    "[{}] {} {} · {}",
                    it.item_id,
                    it.state,
                    crate::app::format_ts(&it.ts),
                    text
                );
            }
            println!(
                "{}",
                t!(
                    "cli.saved_total",
                    total = counts.total_count,
                    open = counts.uncompleted_count
                )
            );
        }
        Command::Threads { workspace } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            let (items, unread) = client.my_threads(20).await?;
            for t in &items {
                let rc = t
                    .root
                    .get("reply_count")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let ts = t.root.get("ts").and_then(|v| v.as_str()).unwrap_or_default();
                println!("[{}] {} replies={}", t.channel, ts, rc);
            }
            println!(
                "{}",
                t!("cli.threads_total", count = items.len(), unread = unread)
            );
        }
        Command::NewChannel {
            name,
            private,
            workspace,
        } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            let (id, normalised) = client
                .create_channel(&name, private)
                .await
                .context("Could not create the channel")?;
            println!(
                "{}",
                t!("cli.channel_created", id = id, name = normalised)
            );
        }
        Command::GroupDm { users, workspace } => {
            let store = store::Store::open()?;
            let ids = resolve_users(&store, &users);
            anyhow::ensure!(!ids.is_empty(), "no user matched");
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            let id = client
                .open_conversation(&ids)
                .await
                .context("Could not open the group DM")?;
            println!("{}", t!("cli.group_opened", id = id));
        }
        Command::ChannelRename {
            channel,
            name,
            workspace,
        } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            client
                .rename_channel(&channel, &name)
                .await
                .context("Could not rename the channel")?;
            println!("{}", t!("cli.channel_renamed", name = name));
        }
        Command::ChannelInvite {
            channel,
            users,
            workspace,
        } => {
            let store = store::Store::open()?;
            let ids = resolve_users(&store, &users);
            anyhow::ensure!(!ids.is_empty(), "no user matched");
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            client
                .invite_to_channel(&channel, &ids)
                .await
                .context("Could not invite people")?;
            println!("{}", t!("cli.invited", n = ids.len()));
        }
        Command::ChannelKick {
            channel,
            users,
            workspace,
        } => {
            let store = store::Store::open()?;
            let ids = resolve_users(&store, &users);
            anyhow::ensure!(!ids.is_empty(), "no user matched");
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            for id in &ids {
                client
                    .kick_from_channel(&channel, id)
                    .await
                    .with_context(|| format!("Could not remove {id}"))?;
            }
            println!("{}", t!("cli.channel_kicked", n = ids.len()));
        }
        Command::ChannelLeave { channel, workspace } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            client
                .leave_channel(&channel)
                .await
                .context("Could not leave the channel")?;
            println!("{}", t!("cli.left_channel"));
        }
        Command::ChannelArchive { channel, workspace } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            client
                .archive_channel(&channel)
                .await
                .context("Could not archive the channel")?;
            println!("{}", t!("cli.channel_archived"));
        }
        Command::Listen {
            workspace,
            filter,
        } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds.clone());
            let ws_url = client.get_websocket_url().await?;
            let _ = ws_url;
            let (tx, mut rx) = tokio::sync::mpsc::channel(256);
            tokio::spawn(ws::listen_forever(creds, filter, tx, None));
            while let Some(update) = rx.recv().await {
                match update {
                    ws::WsUpdate::Connected => println!("{}", t!("cli.ws_connected")),
                    ws::WsUpdate::Disconnected(r) => {
                        println!("{}", t!("cli.ws_disconnected", reason = r))
                    }
                    ws::WsUpdate::Event(env) => {
                        // `listen` is a diagnostic tool: it prints everything
                        // without applying the notification rules, which need
                        // the client's state. Secrets are stripped first.
                        let raw = if env.event.event_type == "reconnect_url" {
                            // The URL carries the session token and reconnect
                            // tickets in its query string.
                            "<url redacted>".to_string()
                        } else {
                            redact_secrets(&env.raw.to_string())
                        };
                        println!("[{}] {}", env.event.event_type, raw);
                    }
                }
            }
        }
        Command::Channels {
            workspace,
            all,
            refresh,
        } => {
            let mut store = store::Store::open()?;

            // The channel list changes rarely: it is downloaded again only if
            // stale or on explicit request.
            if refresh || store.channels_are_stale()? {
                let creds = auth::load_credentials(workspace.as_deref())?;
                let client = api::Client::new(creds);
                let channels = client.list_channels(all).await?;
                let n = store.upsert_channels(&channels)?;
                eprintln!("{}", t!("cli.channels_synced", count = n));
            } else if let Some(age) = store.channels_age_secs()? {
                eprintln!("{}", t!("cli.channels_from_cache", age = humanize_age(age)));
            }

            let channels = store.channels(false)?;
            for ch in &channels {
                let topic = ch.topic.chars().take(80).collect::<String>();
                println!(
                    "  {:>20}  {:25}  {:3} {}  {}",
                    ch.id, ch.name, ch.num_members, t!("cli.channel_row_members"), topic
                );
            }
            println!("{}", t!("cli.channels_total", count = channels.len()));
        }
        Command::Call {
            channel,
            workspace,
            show_secrets,
            connect,
            audio,
            hold,
            dump_sdp,
            unmuted,
            interactive,
            mic,
            speaker,
            raw_audio,
        } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let mut call_system = calls::CallSystem::new(creds, channel);
            call_system
                .probe(
                    show_secrets, connect, audio, hold, dump_sdp, unmuted, interactive,
                    &calls::session::Devices { mic, speaker, raw_audio },
                )
                .await?;
        }
        Command::AecProbe {
            wav,
            seconds,
            mic,
            speaker,
            raw_audio,
        } => {
            run_aec_probe(wav, seconds, mic, speaker, raw_audio).await?;
        }
        Command::History {
            channel,
            workspace,
            limit,
            offline,
            older,
        } => {
            let mut store = store::Store::open()?;

            if !offline {
                let creds = auth::load_credentials(workspace.as_deref())?;
                let client = api::Client::new(creds);

                if let Some(count) = older {
                    // Backfill: everything *before* the oldest one we have.
                    // Without a cache there is no "before", so it is an
                    // ordinary fetch.
                    let latest = store.oldest_ts(&channel)?;
                    let msgs = client
                        .conversations_history_range(
                            &channel,
                            Some(count),
                            None,
                            latest.as_deref(),
                        )
                        .await?;
                    let n = store.upsert_messages(&channel, &msgs)?;
                    eprintln!("{}", t!("cli.history_backfilled", count = n));
                } else {
                    // Incremental sync: only what is new. On the first run
                    // `newest` is None and the initial window is downloaded.
                    let oldest = store.newest_ts(&channel)?;
                    let msgs = client
                        .conversations_history_range(
                            &channel,
                            Some(limit.max(100)),
                            oldest.as_deref(),
                            None,
                        )
                        .await?;
                    let n = store.upsert_messages(&channel, &msgs)?;
                    if n > 0 {
                        eprintln!("{}", t!("cli.history_new", count = n));
                    } else {
                        eprintln!("{}", t!("cli.history_up_to_date"));
                    }
                }
            }

            let names = store.user_names()?;
            let msgs = store.messages(&channel, limit)?;
            for m in &msgs {
                let who = names.get(&m.user).map(String::as_str).unwrap_or(&m.user);
                let reactions = m
                    .raw
                    .get("reactions")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                println!(
                    "{}",
                    t!(
                        "cli.history_message",
                        ts = m.ts,
                        who = who,
                        subtype = m.subtype,
                        text = m.text,
                        reactions = reactions,
                        replies = m.reply_count
                    )
                );
                if let Some(rs) = m.raw.get("reactions").and_then(|r| r.as_array()) {
                    for r in rs {
                        let name = r.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                        let count = r.get("count").and_then(|c| c.as_i64()).unwrap_or(0);
                        println!("  → :{name}: x{count}");
                    }
                }
            }
            println!(
                "{}",
                t!(
                    "cli.history_shown",
                    shown = msgs.len(),
                    total = store.message_count(&channel)?
                )
            );
        }
        Command::NotifyTest { dm } => {
            // An event built in memory: no network call, nothing is sent to
            // Slack. It goes through the real path — same rules, same rendering —
            // so that if it works here and not in the wild, the problem is the
            // delivery of the event, not the notification.
            let event = serde_json::json!({
                "type": "message",
                "channel": if dm { "D0TEST" } else { "C0TEST" },
                "user": "U_FINTO",
                "ts": "1700000000.000100",
                // The text has to be unmistakably fake: a test that looks
                // like a real work message, seen appearing in a notification,
                // gets mistaken for a message actually sent to a colleague.
                "text": if dm {
                    t!("cli.notify_test_text").to_string()
                } else {
                    t!("cli.notify_test_text_mention").to_string()
                },
            });
            let names = std::collections::HashMap::new();
            let test_channel = if dm {
                t!("cli.notify_test_channel")
            } else {
                t!("cli.notify_test_channel_hash")
            };
            let test_author = t!("cli.notify_test_author");
            let ctx = notify::Context {
                my_user_id: "U_ME",
                channel_name: &test_channel,
                is_dm: dm,
                author: &test_author,
                names: &names,
            };
            match notify::from_message(&event, &ctx) {
                Some(n) => {
                    println!("{}", t!("cli.notify_rules_produced"));
                    println!("{}", t!("cli.notify_heading", heading = n.heading()));
                    println!("{}", t!("cli.notify_body", body = n.body()));
                    if notify::enabled() {
                        // Outside the runtime, as in the client: the D-Bus
                        // library starts a runtime of its own and from a tokio
                        // thread it aborts the process. Here we wait for the
                        // outcome, because it is a diagnostic command.
                        let out = std::thread::spawn(move || notify::show(&n))
                            .join();
                        match out {
                            Ok(Ok(())) => println!("{}", t!("cli.notify_desktop_sent")),
                            Ok(Err(e)) => println!(
                                "{}",
                                t!("cli.notify_desktop_failed", error = format!("{e:#}"))
                            ),
                            Err(_) => println!("{}", t!("cli.notify_desktop_panic")),
                        }
                    } else {
                        println!("{}", t!("cli.notify_desktop_disabled"));
                    }
                }
                None => println!("{}", t!("cli.notify_rules_none")),
            }
        }
        Command::Upload {
            channel,
            file,
            text,
            workspace,
        } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let client = api::Client::new(creds);
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .context("path without a file name")?;
            let bytes = std::fs::read(&file)
                .with_context(|| format!("read of {} failed", file.display()))?;
            let size = bytes.len();
            client
                .upload_file(&channel, &name, bytes, text.as_deref(), None)
                .await?;
            println!(
                "{}",
                t!("cli.upload_sent", name = name, channel = channel, size = size)
            );
        }
        Command::Download {
            url,
            name,
            open,
            workspace,
        } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let path = app::download::fetch(&url, &name, &creds.xoxd).await?;
            println!("{}", t!("cli.download_saved", path = path.display()));
            if open {
                app::download::open(&path)?;
            }
        }
        Command::Huddle {
            channel,
            workspace,
            print,
        } => {
            let creds = auth::load_credentials(workspace.as_deref())?;
            let api = api::Client::new(creds.clone());
            let store = store::Store::open_or_memory();
            let cached = store.get_meta("team_id")?;
            let team_id = huddle::resolve_team_id(&creds, &api, cached.clone()).await?;
            if cached.as_deref() != Some(team_id.as_str()) {
                store.set_meta("team_id", &team_id)?;
            }

            if print {
                println!("{}", huddle::huddle_url(&team_id, &channel));
            } else {
                let browser = huddle::Browser::from_env();
                let url = huddle::open(&team_id, &channel, &browser)?;
                println!("{}", t!("cli.huddle_opened", browser = browser.describe()));
                println!("  {url}");
            }
        }
        Command::Cache { action } => match action {
            CacheAction::Status => {
                let path = store::Store::default_path()?;
                let store = store::Store::open()?;
                let channels = store.channels(true)?;
                println!("{}", t!("cli.cache_path", path = path.display()));
                println!(
                    "{}",
                    t!(
                        "cli.cache_size",
                        size = format!("{:.1}", store.size_bytes()? as f64 / 1024.0)
                    )
                );
                println!("{}", t!("cli.cache_channels", count = channels.len()));
                match store.channels_age_secs()? {
                    Some(age) => println!("{}", t!("cli.cache_sync_age", age = humanize_age(age))),
                    None => println!("{}", t!("cli.cache_sync_never")),
                }
                println!("{}", t!("cli.cache_users", count = store.user_names()?.len()));
                let mut total = 0i64;
                let mut with_msgs: Vec<_> = Vec::new();
                for ch in &channels {
                    let n = store.message_count(&ch.id)?;
                    total += n;
                    if n > 0 {
                        with_msgs.push((ch.name.clone(), n));
                    }
                }
                println!("{}", t!("cli.cache_messages", total = total));
                with_msgs.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
                for (name, n) in with_msgs.iter().take(10) {
                    println!("    {name:25} {n}");
                }
            }
            CacheAction::Clear => {
                let store = store::Store::open()?;
                store.clear()?;
                println!("{}", t!("cli.cache_cleared"));
            }
        },
        #[cfg(feature = "mods")]
        Command::Mod { dir, channel, ts, author, text, grant, grant_all, fs_slot } => {
            let granted = mod_grant_set(&grant, grant_all)?;
            let fs_slots = parse_fs_slots(&dir, &fs_slot)?;
            // Guest dispatch (and any `http.fetch`) is synchronous and can block;
            // run it off the async workers, as the app path does, so a request
            // that bridges to the runtime cannot stall an executor thread.
            let outcome = tokio::task::spawn_blocking(move || {
                mods::run_message_with_fs(
                    &dir, &channel, &ts, &author, &text, &granted, &fs_slots,
                )
            })
            .await
            .map_err(|e| anyhow::anyhow!("mod run task failed: {e}"))?;
            match outcome {
                Ok(out) => {
                    println!(
                        "mod {} ({})",
                        mods::sanitize_log(&out.id),
                        mods::sanitize_log(&out.name)
                    );
                    if out.decorations.is_empty() {
                        println!("  (no decorations)");
                    }
                    for (c, t, txt) in out.decorations {
                        println!(
                            "  decoration [{} {}] {}",
                            mods::sanitize_log(&c),
                            mods::sanitize_log(&t),
                            mods::sanitize_log(&txt)
                        );
                    }
                }
                // Sanitize the whole mod error at the CLI boundary: package and
                // guest diagnostics are untrusted text.
                Err(e) => {
                    return Err(anyhow::anyhow!("{}", mods::sanitize_log(&format!("{e:#}"))));
                }
            }
        }
        #[cfg(feature = "mods")]
        Command::Mods { action } => match action {
            ModsAction::List => list_mods()?,
            ModsAction::Grant { id, capabilities } => {
                if !mods::valid_mod_id(&id) {
                    anyhow::bail!(
                        "invalid mod id (reverse-DNS expected): {}",
                        mods::sanitize_log(&id)
                    );
                }
                let caps = parse_capabilities(&capabilities)?;
                // The read scope and egress allow-list are part of the consent:
                // seed them from the installed package. Re-granting never widens
                // (`Consent::grant` keeps them); revoke first to change them.
                let (manifest_scope, manifest_egress) = grant_details(&id, &caps)?;
                let path = mods::consent::default_path()?;
                // Serialize concurrent `slaq mods` transactions so a grant cannot
                // resurrect a capability another command just revoked.
                let _lock = mods::consent::lock(&path)?;
                let mut consent = mods::consent::Consent::load(&path)?;
                let before = consent.granted(&id);
                consent.grant_full(
                    &id,
                    &caps,
                    manifest_scope.clone(),
                    manifest_egress.clone(),
                );
                let after = consent.granted(&id);
                consent.save(&path)?;
                println!(
                    "granted {} capability(ies) to {}",
                    after.difference(&before).count(),
                    mods::sanitize_log(&id)
                );
                if manifest_scope.is_some() {
                    // Show the scope actually enforced (the user's choice, or
                    // min of consent and manifest), as `mods list` does.
                    let effective = installed_request(&id, "granting")
                        .ok()
                        .and_then(|req| {
                            mods::consent::Grants::effective_read_scope(&req, &consent.entry(&id))
                        });
                    match effective {
                        Some(scope) => println!("  read scope: {}", describe_scope(&scope)),
                        None => println!("  read scope: denied (no channels in scope)"),
                    }
                }
                println!("  {}", mods::sanitize_log(&path.display().to_string()));
            }
            ModsAction::Revoke { id, capabilities } => {
                if !mods::valid_mod_id(&id) {
                    anyhow::bail!(
                        "invalid mod id (reverse-DNS expected): {}",
                        mods::sanitize_log(&id)
                    );
                }
                let caps = parse_capabilities(&capabilities)?;
                let path = mods::consent::default_path()?;
                let _lock = mods::consent::lock(&path)?;
                let mut consent = mods::consent::Consent::load(&path)?;
                let before = consent.granted(&id);
                consent.revoke(&id, &caps);
                let after = consent.granted(&id);
                consent.save(&path)?;
                println!(
                    "revoked {} capability(ies) from {}",
                    before.difference(&after).count(),
                    mods::sanitize_log(&id)
                );
                println!("  {}", mods::sanitize_log(&path.display().to_string()));
            }
            ModsAction::Map { id, slot, dir } => {
                if !mods::valid_mod_id(&id) {
                    anyhow::bail!(
                        "invalid mod id (reverse-DNS expected): {}",
                        mods::sanitize_log(&id)
                    );
                }
                // The slot must be one the installed package declares; the access
                // recorded is the one in force now, so a later update cannot widen
                // it (the runner intersects).
                let declared = declared_fs_slots(&id)?;
                let Some(decl) = declared.iter().find(|s| s.name == slot) else {
                    anyhow::bail!(
                        "the installed mod {} does not declare a filesystem slot named {}",
                        mods::sanitize_log(&id),
                        mods::sanitize_log(&slot)
                    );
                };
                let access = decl.access;
                // The directory must exist; canonicalize so the stored path is
                // absolute and symlink-free (the host resolves against it anyway).
                let canon = std::fs::canonicalize(&dir).map_err(|e| {
                    anyhow::anyhow!("cannot use {}: {e}", mods::sanitize_log(&dir.display().to_string()))
                })?;
                if !canon.is_dir() {
                    anyhow::bail!(
                        "{} is not a directory",
                        mods::sanitize_log(&canon.display().to_string())
                    );
                }
                if mods::fs::is_reserved_dir(&canon) {
                    anyhow::bail!(
                        "{} is inside a reserved slaq directory and cannot be mapped",
                        mods::sanitize_log(&canon.display().to_string())
                    );
                }
                let path = mods::consent::default_path()?;
                let _lock = mods::consent::lock(&path)?;
                let mut consent = mods::consent::Consent::load(&path)?;
                consent.map_slot(&id, &slot, canon.clone(), access)?;
                consent.save(&path)?;
                println!(
                    "mapped slot {} of {} to {} ({})",
                    mods::sanitize_log(&slot),
                    mods::sanitize_log(&id),
                    mods::sanitize_log(&canon.display().to_string()),
                    access.as_str()
                );
                println!("  {}", mods::sanitize_log(&path.display().to_string()));
            }
            ModsAction::Unmap { id, slot } => {
                if !mods::valid_mod_id(&id) {
                    anyhow::bail!(
                        "invalid mod id (reverse-DNS expected): {}",
                        mods::sanitize_log(&id)
                    );
                }
                let path = mods::consent::default_path()?;
                let _lock = mods::consent::lock(&path)?;
                let mut consent = mods::consent::Consent::load(&path)?;
                consent.unmap_slot(&id, &slot);
                consent.save(&path)?;
                println!(
                    "unmapped slot {} of {}",
                    mods::sanitize_log(&slot),
                    mods::sanitize_log(&id)
                );
                println!("  {}", mods::sanitize_log(&path.display().to_string()));
            }
            ModsAction::Scope {
                id,
                all,
                reset,
                channels,
            } => {
                use mods::capability::{Capability, ReadScope};
                if !mods::valid_mod_id(&id) {
                    anyhow::bail!(
                        "invalid mod id (reverse-DNS expected): {}",
                        mods::sanitize_log(&id)
                    );
                }
                let request = installed_request(&id, "choosing its channels")?;
                if !request.caps().contains(&Capability::MessagesRead) {
                    anyhow::bail!(
                        "the installed mod {} does not declare messages.read",
                        mods::sanitize_log(&id)
                    );
                }
                let scope = if all {
                    Some(ReadScope::All)
                } else if reset {
                    None
                } else if channels.is_empty() {
                    anyhow::bail!("name the channel ids, or pass --all or --reset");
                } else {
                    Some(ReadScope::Channels {
                        channels: channels.iter().map(|c| c.trim().to_string()).collect(),
                    })
                };
                let path = mods::consent::default_path()?;
                let _lock = mods::consent::lock(&path)?;
                let mut consent = mods::consent::Consent::load(&path)?;
                consent.set_user_scope(&id, scope)?;
                consent.save(&path)?;
                let entry = consent.entry(&id);
                match mods::consent::Grants::effective_read_scope(&request, &entry) {
                    Some(scope) => println!(
                        "{} reads: {}",
                        mods::sanitize_log(&id),
                        describe_scope(&scope)
                    ),
                    None => println!(
                        "{} reads nothing yet: grant messages.read to apply this scope",
                        mods::sanitize_log(&id)
                    ),
                }
                println!("  {}", mods::sanitize_log(&path.display().to_string()));
            }
            ModsAction::KeepSecrets { id } => mod_secrets(&id, true)?,
            ModsAction::ForgetSecrets { id } => mod_secrets(&id, false)?,
            ModsAction::Egress { id, op } => {
                use mods::capability::Capability;
                if !mods::valid_mod_id(&id) {
                    anyhow::bail!(
                        "invalid mod id (reverse-DNS expected): {}",
                        mods::sanitize_log(&id)
                    );
                }
                let request = installed_request(&id, "choosing its hosts")?;
                if !request.caps().contains(&Capability::NetEgress) {
                    anyhow::bail!(
                        "the installed mod {} does not declare net.egress",
                        mods::sanitize_log(&id)
                    );
                }
                let path = mods::consent::default_path()?;
                let _lock = mods::consent::lock(&path)?;
                let mut consent = mods::consent::Consent::load(&path)?;
                match &op {
                    EgressOp::Add { host } => consent.add_host(&id, request.egress(), host)?,
                    EgressOp::Rm { host } => {
                        if !consent.remove_host(&id, request.egress(), host)? {
                            anyhow::bail!(
                                "{} is not among the hosts {} may reach",
                                mods::sanitize_log(host),
                                mods::sanitize_log(&id)
                            );
                        }
                    }
                }
                consent.save(&path)?;
                let entry = consent.entry(&id);
                let reach = mods::consent::Grants::effective_egress(&request, &entry);
                if entry.caps().contains(&Capability::NetEgress) {
                    println!(
                        "{} may reach: {}",
                        mods::sanitize_log(&id),
                        join_or_dash(&reach.iter().map(|h| mods::sanitize_log(h)).collect::<Vec<_>>())
                    );
                } else {
                    println!(
                        "saved; {} reaches nothing until net.egress is granted",
                        mods::sanitize_log(&id)
                    );
                }
                println!("  {}", mods::sanitize_log(&path.display().to_string()));
            }
        },
    }

    Ok(())
}

/// Real acoustic echo test (diagnostic).
///
/// Plays a WAV through the same playback path as the call while the microphone
/// is open: that way the AEC receives the render reference at the moment of
/// playback, exactly as in a call. Then it compares the pre-AEC capture dump
/// with the post-AEC one and derives the ERLE from them.
async fn run_aec_probe(
    wav_path: std::path::PathBuf,
    seconds: Option<f64>,
    mic: Option<String>,
    speaker: Option<String>,
    raw_audio: bool,
) -> Result<()> {
    use calls::AudioManager;

    let mut samples = calls::wav::read_pcm16_mono_48k(&wav_path)
        .with_context(|| format!("WAV read of {} failed", wav_path.display()))?;
    if let Some(s) = seconds {
        samples.truncate((s.max(0.0) * 48_000.0) as usize);
    }
    anyhow::ensure!(!samples.is_empty(), "the WAV contains no samples");

    // Dump paths: environment variables first, otherwise the probe defaults.
    // They are passed to the manager: no `set_var`, which in edition 2024 is
    // `unsafe`, and the files still end up where they are needed.
    let env_mic = std::env::var_os("SLAQ_DUMP_MIC").map(std::path::PathBuf::from);
    let env_tx = std::env::var_os("SLAQ_DUMP_TX").map(std::path::PathBuf::from);
    let mic_path = env_mic
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/slaq-aec-mic.wav"));
    let tx_path = env_tx
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/slaq-aec-tx.wav"));

    let mut mgr = AudioManager::with_processing(!raw_audio)
        .with_dumps(Some(mic_path.clone()), Some(tx_path.clone()));
    let track = mgr.init(mic.as_deref())?;
    let tx = mgr.open_playback(speaker.as_deref())?;

    println!(
        "{}",
        t!(
            "cli.aec_wav",
            path = wav_path.display(),
            samples = samples.len(),
            seconds = format!("{:.2}", samples.len() as f64 / 48_000.0)
        )
    );
    println!("{}", t!("cli.aec_dump_mic", path = mic_path.display()));
    println!("{}", t!("cli.aec_dump_tx", path = tx_path.display()));
    if env_mic.is_none() || env_tx.is_none() {
        println!("{}", t!("cli.aec_note_env"));
    }
    println!("{}", t!("cli.aec_playing"));

    // Real-time playback, 20 ms blocks like the real path.
    const FRAME: usize = 960;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(20));
    let chunks = samples.len().div_ceil(FRAME);
    for i in 0..chunks {
        let lo = i * FRAME;
        let hi = (lo + FRAME).min(samples.len());
        if tx.send(samples[lo..hi].to_vec()).await.is_err() {
            anyhow::bail!("playback channel closed");
        }
        tick.tick().await;
    }
    // ~1 s of trailing silence: lets the last echo out and drains the queue.
    let silence = vec![0i16; FRAME];
    for _ in 0..50 {
        if tx.send(silence.clone()).await.is_err() {
            break;
        }
        tick.tick().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    // Closing the streams stops the callbacks; the capture task exits and the
    // `WavWriter`s close the files with the correct header.
    drop(tx);
    drop(track);
    drop(mgr);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let mic_pcm = read_dump(&mic_path, "mic");
    let tx_pcm = read_dump(&tx_path, "tx");
    println!();
    println!(
        "{}",
        t!("cli.aec_dump_counts", mic = mic_pcm.len(), tx = tx_pcm.len())
    );
    report_erle(&mic_pcm, &tx_pcm);
    Ok(())
}

/// Diagnostic: reads a dump, reporting the error instead of propagating it.
fn read_dump(path: &std::path::Path, which: &str) -> Vec<i16> {
    match calls::wav::read_pcm16_mono_48k(path) {
        Ok(v) => v,
        Err(e) => {
            println!(
                "{}",
                t!(
                    "cli.aec_dump_unreadable",
                    which = which,
                    path = path.display(),
                    error = format!("{e:#}")
                )
            );
            Vec::new()
        }
    }
}

/// Diagnostic: prints the ERLE and RMS levels from the two dumps.
///
/// The ERLE is the ratio between the mic power (echo still present) and the tx
/// power (after the AEC), skipping the first ~1.5 s in which the filter converges.
fn report_erle(mic: &[i16], tx: &[i16]) {
    let n = mic.len().min(tx.len());
    if n == 0 {
        println!("{}", t!("cli.aec_no_samples"));
        return;
    }
    let warmup = (48_000 * 3 / 2).min(n);
    let mut p_mic = 0.0f64;
    let mut p_tx = 0.0f64;
    for i in warmup..n {
        let m = mic[i] as f64;
        let t = tx[i] as f64;
        p_mic += m * m;
        p_tx += t * t;
    }
    let count = (n - warmup).max(1) as f64;
    let erle = 10.0 * (p_mic.max(1.0) / p_tx.max(1.0)).log10();
    let dbfs = |p: f64| 20.0 * ((p / count).sqrt() / 32_768.0).max(1e-9).log10();
    println!(
        "{}",
        t!(
            "cli.aec_measure_window",
            samples = n - warmup,
            from = format!("{:.1}", warmup as f64 / 48_000.0)
        )
    );
    println!("{}", t!("cli.aec_erle", value = format!("{erle:.1}")));
    println!("{}", t!("cli.aec_rms_mic", value = format!("{:.1}", dbfs(p_mic))));
    println!("{}", t!("cli.aec_rms_tx", value = format!("{:.1}", dbfs(p_tx))));
}

/// Renders an age in seconds readably ("3h", "2d").
fn humanize_age(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}g", s / 86_400),
    }
}

/// Property tests for the parsers that eat untrusted bytes.
///
/// The client talks to an undocumented API and renders whatever arrives, and
/// the release build is `panic = "abort"`: a panic in any of these closes the
/// whole client. These tests assert only that they *never panic*, on arbitrary
/// input — they do not pin down what the output should be.
#[cfg(test)]
mod robustness {
    use super::*;
    use proptest::prelude::*;

    /// A recursive arbitrary JSON value, the shape of what Slack sends.
    fn arb_json() -> impl Strategy<Value = serde_json::Value> {
        use serde_json::Value;
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| Value::Number(n.into())),
            any::<f64>().prop_map(|n| serde_json::Number::from_f64(n)
                .map(Value::Number)
                .unwrap_or(Value::Null)),
            ".{0,24}".prop_map(Value::String),
        ];
        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
                prop::collection::hash_map(".{0,8}", inner, 0..8)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #[test]
        fn mrkdwn_never_panics(s in ".{0,512}") {
            let _ = app::mrkdwn::parse(&s);
            let _ = app::mrkdwn::parse_spans(&s);
            let _ = app::mrkdwn::to_plain(&s);
            let _ = app::mrkdwn::to_editable(&s);
        }

        #[test]
        fn json_parsers_never_panic(v in arb_json()) {
            let _ = app::parse_message_as(&v, "U_ME");
            let _ = app::blocks_text(&v);
            let _ = ws::events::cache_op(&v);
        }

        #[test]
        fn text_parsers_never_panic(s in ".{0,200}", cursor in 0usize..200) {
            let _ = app::parse_message_permalink(&s);
            let _ = app::mention_prefix(&s, cursor, '@');
            let _ = app::mention_prefix(&s, cursor, '#');
            let _ = net::is_slack_host(&s);
        }
    }

    /// Every line of the `big-list-of-naughty-strings`
    /// (<https://github.com/minimaxir/big-list-of-naughty-strings>) — the classic
    /// corpus of inputs that break text handling: unicode edge cases, control
    /// characters, RTL, combining marks, huge strings… — through the parsers
    /// that render a message. The point is only that none of them panics.
    #[test]
    fn naughty_strings_never_panic() {
        let blns = include_str!("../tests/data/blns.txt");
        let mut lines = 0;
        for line in blns.lines() {
            let _ = app::mrkdwn::parse(line);
            let _ = app::mrkdwn::parse_spans(line);
            let _ = app::mrkdwn::to_plain(line);
            let _ = app::mrkdwn::to_editable(line);
            let _ = app::parse_message_permalink(line);
            let _ = app::mention_prefix(line, line.len(), '@');
            lines += 1;
        }
        assert!(lines > 100, "the naughty-string corpus looks missing");
    }

    /// The `listen` diagnostic prints raw events; a `reconnect_url` carries the
    /// session token in its query string, so secrets must be stripped.
    #[test]
    fn diagnostic_output_hides_session_secrets() {
        let raw = r#"{"type":"reconnect_url","url":"wss://x/?token=xoxc-111-222-abc&frt=zzz&d=xoxd-333"}"#;
        let red = redact_secrets(raw);
        assert!(!red.contains("xoxc-111-222-abc"));
        assert!(!red.contains("xoxd-333"));
        assert!(red.contains("<redacted>"));
    }
}
