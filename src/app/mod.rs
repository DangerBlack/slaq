//! The application core, independent of the interface.
//!
//! State, actions and the reaction to events live here; `ui/` holds only the
//! rendering and the input handling. It is what lets the TUI and the GUI be two
//! views on the same client instead of two distinct clients.
//!
//! The state is an `Arc<RwLock<AppState>>` mutated by background tasks: a front
//! end reads it when it has to redraw, and the core does not know which one it
//! is.

pub mod demo;
pub mod download;
pub mod huddles;
pub mod images;
pub mod mrkdwn;
pub mod sections;

use crate::auth::Credentials;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(PartialEq, Eq, Default, Clone, Copy, Debug)]
pub enum ViewMode {
    #[default]
    ChannelList,
    Chat,
    Thread,
}

/// One capability row in the consent dialog.
#[cfg(feature = "mods")]
#[derive(Clone)]
pub struct ModConsentCap {
    pub cap: crate::mods::capability::Capability,
    /// Whether the user currently wants to grant it.
    pub selected: bool,
    /// Whether it was already granted when the dialog opened. Apply only touches
    /// capabilities whose selection changed, so a concurrent `slaq mods` change
    /// to an untouched capability is not reverted.
    pub initial: bool,
}

/// One `filesystem` slot in the consent dialog: the manifest-declared name and
/// access, plus the directory the user has mapped it to (if any). The mapping is
/// persisted immediately and independently of the capability checkbox (a slot can
/// be mapped before or after `filesystem` is granted; the runtime intersects).
#[cfg(feature = "mods")]
#[derive(Clone)]
pub struct ModConsentSlot {
    pub name: String,
    pub access: crate::mods::fs::FsAccess,
    /// The stored absolute directory, if the user has mapped this slot.
    pub mapped: Option<String>,
}

/// One mod in the consent dialog.
#[cfg(feature = "mods")]
#[derive(Clone)]
pub struct ModConsentEntry {
    pub id: String,
    pub name: String,
    pub caps: Vec<ModConsentCap>,
    /// The manifest's declared read scope, to seed a `messages.read` grant.
    pub read_scope: Option<crate::mods::capability::ReadScope>,
    /// The read scope enforced today (`min(consent, manifest)`), for display of
    /// an already-granted `messages.read`.
    pub effective_scope: Option<crate::mods::capability::ReadScope>,
    /// The manifest's `net.egress` allow-list, shown in the dialog.
    pub egress: Vec<String>,
    /// The manifest's declared `filesystem` slots, each with the user's current
    /// mapping, shown in the dialog.
    pub fs_slots: Vec<ModConsentSlot>,
    /// The channels and hosts the user chooses for this mod (saved on Apply).
    pub access: ModConsentAccess,
}

/// The channels a mod reads and the hosts it reaches, as the user edits them
/// in the consent dialog. Not bounded by the manifest (see
/// `crate::mods::consent`); saved on Apply, only when edited.
#[cfg(feature = "mods")]
#[derive(Clone, Default)]
pub struct ModConsentAccess {
    /// The channels the user chose; `None` follows the manifest's scope.
    pub user_scope: Option<crate::mods::capability::ReadScope>,
    /// The manifest's hosts the user allows.
    pub egress_allowed: Vec<String>,
    /// The hosts the user added beyond the manifest.
    pub egress_added: Vec<String>,
    /// `egress_allowed` and `egress_added` as the dialog opened: Apply writes
    /// only the difference, so a concurrent `slaq mods egress` edit survives.
    pub egress_allowed_initial: Vec<String>,
    pub egress_added_initial: Vec<String>,
    /// The scope was changed in this dialog.
    pub scope_dirty: bool,
    /// The host list was changed in this dialog (or holds manifest hosts the
    /// user has not reviewed yet, which Apply records as allowed).
    pub egress_dirty: bool,
    /// Why the last added host was refused, shown under the list.
    pub error: Option<String>,
}

/// One edit to a mod's [`ModConsentAccess`] from the dialog.
#[cfg(feature = "mods")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessEdit {
    /// Follow the manifest's declared scope.
    ScopeManifest,
    /// Every channel the client can see.
    ScopeAll,
    /// Only the channels picked below (starting from the current ones).
    ScopeSome,
    /// Pick or unpick one channel (switches to "only these").
    ToggleChannel(String),
    /// Allow or stop allowing one of the manifest's hosts.
    ToggleHost(String),
    /// Add a host the user typed (validated; refused with an error otherwise).
    AddHost(String),
    /// Remove a host the user added.
    RemoveHost(String),
}

#[cfg(feature = "mods")]
impl ModConsentEntry {
    /// Applies one access edit to this dialog entry.
    pub fn edit_access(&mut self, edit: AccessEdit) {
        use crate::mods::capability::ReadScope;
        let a = &mut self.access;
        match edit {
            AccessEdit::ScopeManifest => {
                a.user_scope = None;
                a.scope_dirty = true;
            }
            AccessEdit::ScopeAll => {
                a.user_scope = Some(ReadScope::All);
                a.scope_dirty = true;
            }
            AccessEdit::ScopeSome => {
                if !matches!(a.user_scope, Some(ReadScope::Channels { .. })) {
                    // Start from what the mod reads today, when that is a list.
                    let seed = self
                        .effective_scope
                        .as_ref()
                        .or(self.read_scope.as_ref())
                        .and_then(|sc| match sc {
                            ReadScope::Channels { channels } => Some(channels.clone()),
                            ReadScope::All => None,
                        })
                        .unwrap_or_default();
                    a.user_scope = Some(ReadScope::Channels { channels: seed });
                    a.scope_dirty = true;
                }
            }
            AccessEdit::ToggleChannel(id) => {
                let mut channels = match a.user_scope.take() {
                    Some(ReadScope::Channels { channels }) => channels,
                    _ => Default::default(),
                };
                if !channels.remove(&id) {
                    channels.insert(id);
                }
                a.user_scope = Some(ReadScope::Channels { channels });
                a.scope_dirty = true;
            }
            AccessEdit::ToggleHost(h) => {
                if let Some(i) = a.egress_allowed.iter().position(|x| *x == h) {
                    a.egress_allowed.remove(i);
                } else if self.egress.contains(&h) {
                    a.egress_allowed.push(h);
                }
                a.egress_dirty = true;
            }
            AccessEdit::AddHost(raw) => {
                match crate::mods::capability::normalize_user_host(&raw) {
                    Ok(h) if self.egress.contains(&h) => {
                        // A manifest host: allow it rather than duplicate it.
                        if !a.egress_allowed.contains(&h) {
                            a.egress_allowed.push(h);
                        }
                        a.error = None;
                        a.egress_dirty = true;
                    }
                    Ok(h) if a.egress_added.contains(&h) => a.error = None,
                    Ok(_) if a.egress_added.len() >= crate::mods::capability::MAX_EGRESS_HOSTS => {
                        a.error = Some(
                            t!(
                                "gui.mods_access_too_many_hosts",
                                max = crate::mods::capability::MAX_EGRESS_HOSTS
                            )
                            .to_string(),
                        );
                    }
                    Ok(h) => {
                        a.egress_added.push(h);
                        a.error = None;
                        a.egress_dirty = true;
                    }
                    Err(_) => {
                        a.error = Some(
                            t!(
                                "gui.mods_access_bad_host",
                                host = crate::mods::sanitize(raw.trim())
                            )
                            .to_string(),
                        );
                    }
                }
            }
            AccessEdit::RemoveHost(h) => {
                a.egress_added.retain(|x| *x != h);
                a.egress_dirty = true;
            }
        }
    }
}

#[cfg(feature = "mods")]
impl ModConsentEntry {
    /// Whether the current selection is the combination the docs flag as possible
    /// exfiltration: a **content-bearing** read (`messages.read`) **and** network
    /// egress. (Directory caps are rated Low by the registry and are not flagged.)
    pub fn read_and_network(&self) -> bool {
        let selected = |cap| self.caps.iter().any(|c| c.cap == cap && c.selected);
        selected(crate::mods::capability::Capability::MessagesRead)
            && selected(crate::mods::capability::Capability::NetEgress)
    }

    /// The other exfiltration combination the docs flag: `filesystem` **and**
    /// network egress (what the mod reads in its slots could be sent anywhere the
    /// egress list allows).
    pub fn files_and_network(&self) -> bool {
        let selected = |cap| self.caps.iter().any(|c| c.cap == cap && c.selected);
        selected(crate::mods::capability::Capability::Filesystem)
            && selected(crate::mods::capability::Capability::NetEgress)
    }
}

/// The consent dialog ("Mods"): what each installed mod asks for, with the
/// current selection. Rendered by both front ends.
#[cfg(feature = "mods")]
#[derive(Default, Clone)]
pub struct ModConsentDialog {
    pub entries: Vec<ModConsentEntry>,
    /// Shown automatically at startup (something is undecided); the host is not
    /// loaded yet, so Apply *or* Dismiss must load it.
    pub pending: bool,
    /// Keyboard cursor into the flattened capability rows (used by the TUI).
    pub cursor: usize,
}

/// One editable setting in the settings form.
#[cfg(feature = "mods")]
#[derive(Clone)]
pub struct ModSettingField {
    pub key: String,
    pub kind: crate::mods::settings::Kind,
    /// The manifest's `label` (an `i18n:` key or literal), if any.
    pub label: Option<String>,
    /// The manifest's `help` string, if any.
    pub help: Option<String>,
    /// The allowed options for an `enum` (empty otherwise).
    pub options: Vec<String>,
    /// Whether a value is currently set (for a secret: whether the keyring holds
    /// one).
    pub has_value: bool,
    /// The editable buffer. For a `bool` it is `"true"`/`"false"`; for an `enum`
    /// the selected option. For a `secret` it holds **only** a replacement the
    /// user typed — never the loaded plaintext.
    pub text: String,
    /// Whether the user changed this field (only touched fields are written, so
    /// an untouched default-less field is never sent).
    pub dirty: bool,
    /// The user asked to clear a stored secret.
    pub clear: bool,
}

#[cfg(feature = "mods")]
impl ModSettingField {
    /// What to show before the value: the label when present, else the key.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.key)
    }
}

/// One mod's settings in the form.
#[cfg(feature = "mods")]
#[derive(Clone)]
pub struct ModSettingsPane {
    pub id: String,
    pub name: String,
    /// The live handle: reads/writes take effect on the running mod.
    pub handle: crate::mods::SettingsHandle,
    pub fields: Vec<ModSettingField>,
}

#[cfg(feature = "mods")]
impl ModSettingsPane {
    /// Builds a pane from a live handle (the schema from the mod, the values in
    /// effect).
    pub fn from_handle(handle: crate::mods::SettingsHandle) -> ModSettingsPane {
        let fields = {
            let guard = handle.settings.read().unwrap_or_else(|e| e.into_inner());
            guard
                .schema
                .fields
                .iter()
                .map(|f| {
                    let value = guard.get(&f.key);
                    let secret = f.kind.is_secret();
                    ModSettingField {
                        key: f.key.clone(),
                        kind: f.kind,
                        label: f.label.clone(),
                        help: f.help.clone(),
                        options: f.values.clone(),
                        has_value: value.is_some(),
                        // A secret is never loaded into the buffer: the field
                        // holds only a replacement the user types.
                        text: if secret {
                            String::new()
                        } else {
                            value.map(|v| v.as_text()).unwrap_or_default()
                        },
                        dirty: false,
                        clear: false,
                    }
                })
                .collect()
        };
        ModSettingsPane {
            id: handle.id.clone(),
            name: handle.name.clone(),
            handle,
            fields,
        }
    }
}

/// The settings form ("Mod settings"): a tab per mod, a row per field. Rendered
/// by both front ends; edits are persisted on Save.
#[cfg(feature = "mods")]
#[derive(Default, Clone)]
pub struct ModSettingsDialog {
    pub panes: Vec<ModSettingsPane>,
    /// The selected mod tab.
    pub tab: usize,
    /// Keyboard cursor into the current tab's fields.
    pub cursor: usize,
    /// Whether the focused text field is being edited (the TUI's inline input).
    pub editing: bool,
    /// A save error to surface in the form.
    pub error: Option<String>,
    /// A save is in flight: the form is read-only until it completes.
    pub busy: bool,
    /// A generation counter, so a save completion closes only the form it began.
    pub save_gen: u64,
}

/// Clears `mods_consent_busy` when the Apply/Dismiss task ends, including on a
/// panic, so a failed decision cannot wedge the consent UI forever.
#[cfg(feature = "mods")]
struct ModsBusyGuard(std::sync::Arc<RwLock<AppState>>);
#[cfg(feature = "mods")]
impl Drop for ModsBusyGuard {
    fn drop(&mut self) {
        let state = self.0.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                state.write().await.mods_consent_busy = false;
            });
        }
    }
}

/// Toggles a capability in the dialog, if present.
#[cfg(feature = "mods")]
fn toggle_consent_in(s: &mut AppState, id: &str, cap: crate::mods::capability::Capability) {
    if let Some(d) = s.mods_consent.as_mut() {
        for e in &mut d.entries {
            if e.id != id {
                continue;
            }
            for c in &mut e.caps {
                if c.cap == cap {
                    c.selected = !c.selected;
                    return;
                }
            }
        }
    }
}

/// Records the consent dialog's choices in `consent` (the store work of Apply,
/// kept apart so it is testable without the runtime).
#[cfg(feature = "mods")]
fn apply_consent_entries(
    consent: &mut crate::mods::consent::Consent,
    entries: &[ModConsentEntry],
) -> anyhow::Result<()> {
    for e in entries {
        use crate::mods::capability::Capability as Cap;
        // Touch only the capabilities the user actually changed, so a
        // concurrent `slaq mods` edit to an untouched one survives.
        let newly_granted: Vec<_> = e
            .caps
            .iter()
            .filter(|c| c.selected && !c.initial)
            .map(|c| c.cap)
            .collect();
        let newly_rejected: Vec<_> = e
            .caps
            .iter()
            .filter(|c| !c.selected && c.initial)
            .map(|c| c.cap)
            .collect();
        let grant = newly_granted;
        // Clear any stale recorded scope/egress before (re)granting, so
        // the user's choice takes effect even when the old values no
        // longer intersect the manifest.
        if grant.contains(&Cap::MessagesRead) {
            consent.revoke(&e.id, &[Cap::MessagesRead]);
        }
        if grant.contains(&Cap::NetEgress) {
            consent.revoke(&e.id, &[Cap::NetEgress]);
        }
        let scope = grant
            .contains(&Cap::MessagesRead)
            .then(|| e.read_scope.clone())
            .flatten();
        let egress = grant
            .contains(&Cap::NetEgress)
            .then(|| e.access.egress_allowed.clone());
        consent.grant_full(&e.id, &grant, scope, egress);
        consent.revoke(&e.id, &newly_rejected);
        // A capability the store still holds but the dialog shows unticked
        // (it reaches nothing, so `scan` reports it ungranted) is revoked: what
        // Apply leaves must match what was shown, or a host/channel edit below
        // would silently bring it back to life.
        let stored = consent.granted(&e.id);
        let shown_off: Vec<Cap> = e
            .caps
            .iter()
            .filter(|c| !c.selected && matches!(c.cap, Cap::MessagesRead | Cap::NetEgress))
            .map(|c| c.cap)
            .filter(|c| stored.contains(c))
            .collect();
        consent.revoke(&e.id, &shown_off);
        // Record the egress list as seen even when nothing was granted,
        // so a rejection does not re-prompt.
        if !e.egress.is_empty() {
            consent.review_egress(&e.id, e.egress.clone());
        }
        // The user's channels and hosts: written only when edited
        // (or holding unreviewed manifest hosts), so a concurrent
        // `slaq mods` edit to an untouched mod survives.
        if e.access.egress_dirty {
            // Only the user's changes, applied over the store as it is now.
            let a = &e.access;
            let merge = |now: &[String], initial: &[String], wanted: &[String]| -> Vec<String> {
                let mut out: Vec<String> = now
                    .iter()
                    .filter(|h| !(initial.contains(h) && !wanted.contains(h)))
                    .cloned()
                    .collect();
                for h in wanted {
                    if !initial.contains(h) && !out.contains(h) {
                        out.push(h.clone());
                    }
                }
                out
            };
            let entry = consent.entry(&e.id);
            let allowed = merge(entry.egress(), &a.egress_allowed_initial, &a.egress_allowed);
            let added = merge(entry.egress_added(), &a.egress_added_initial, &a.egress_added);
            consent.set_egress(&e.id, &e.egress, &allowed, &added)?;
        }
        if e.access.scope_dirty {
            match &e.access.user_scope {
                // An empty pick is not a scope; the dialog warns, and
                // the previous choice stays.
                Some(crate::mods::capability::ReadScope::Channels { channels })
                    if channels.is_empty() =>
                {
                    tracing::warn!("mod channel pick left empty: not saved");
                }
                scope => consent.set_user_scope(&e.id, scope.clone())?,
            }
        }
        // Record the decision even when nothing changed.
        consent.decide(&e.id);
    }
    Ok(())
}

/// Applies one access edit to the dialog entry of `id`, if present.
#[cfg(feature = "mods")]
fn edit_access_in(s: &mut AppState, id: &str, edit: AccessEdit) {
    if let Some(d) = s.mods_consent.as_mut()
        && let Some(e) = d.entries.iter_mut().find(|e| e.id == id)
    {
        e.edit_access(edit);
    }
}

/// Sets the recorded mapping of one `filesystem` slot in the dialog, if present.
/// Keyed by identity (mod id + slot name), so a stale frame cannot touch the
/// wrong row.
#[cfg(feature = "mods")]
fn set_consent_slot_mapped(s: &mut AppState, id: &str, slot: &str, mapped: Option<String>) {
    if let Some(d) = s.mods_consent.as_mut() {
        for e in &mut d.entries {
            if e.id != id {
                continue;
            }
            for s in &mut e.fs_slots {
                if s.name == slot {
                    s.mapped = mapped;
                    return;
                }
            }
        }
    }
}

/// The value the form's buffer represents, typed by the field.
#[cfg(feature = "mods")]
fn field_value(f: &ModSettingField) -> anyhow::Result<crate::mods::settings::Value> {
    use crate::mods::settings::{Kind, Value};
    Ok(match f.kind {
        Kind::Number => {
            let t = f.text.trim();
            if let Ok(i) = t.parse::<i64>() {
                Value::Integer(i)
            } else if let Ok(x) = t.parse::<f64>() {
                Value::Real(x)
            } else {
                anyhow::bail!("not a number: {:?}", f.text)
            }
        }
        Kind::Bool => Value::Flag(f.text == "true"),
        _ => Value::Text(f.text.clone()),
    })
}

/// A message action a mod declared, as the front ends render it (raw ids are the
/// routing keys passed back to [`App::invoke_mod_action`]).
#[cfg(feature = "mods")]
#[derive(Clone)]
pub struct ModAction {
    pub mod_id: String,
    pub action_id: String,
    pub mod_name: String,
    pub label: String,
    pub icon: Option<String>,
}

/// The TUI's open mod-action menu: the message it targets and the cursor.
#[cfg(feature = "mods")]
#[derive(Clone, Default)]
pub struct ModActionMenu {
    pub channel: String,
    pub ts: String,
    pub cursor: usize,
}

/// A view a mod declared, as the front ends open it.
#[cfg(feature = "mods")]
#[derive(Clone)]
pub struct ModView {
    pub mod_id: String,
    pub view_id: String,
    pub mod_name: String,
    pub label: String,
    pub icon: Option<String>,
}

/// The TUI's view picker (a cursor into `mod_views`).
#[cfg(feature = "mods")]
#[derive(Clone, Default)]
pub struct ModViewMenu {
    pub cursor: usize,
}

/// A slash command a mod declared (`/name`), as the front ends offer it. The
/// extra fields are read by the upcoming command list/completion UI.
#[cfg(feature = "mods")]
#[derive(Clone)]
#[allow(dead_code)]
pub struct ModCommand {
    pub mod_id: String,
    pub mod_name: String,
    pub name: String,
    pub label: String,
    pub description: String,
}

/// A composer action a mod declared, as the front ends offer it.
#[cfg(feature = "mods")]
#[derive(Clone)]
pub struct ModComposerAction {
    pub mod_id: String,
    pub mod_name: String,
    pub action_id: String,
    pub label: String,
    pub icon: Option<String>,
}

/// An open mod-view panel: its title and the `(kind, text)` content blocks (in an
/// `Arc`, so the per-frame snapshot clone is cheap).
#[cfg(feature = "mods")]
#[derive(Clone)]
pub struct ModViewPanel {
    pub mod_id: String,
    pub view_id: String,
    pub title: String,
    pub blocks: Arc<Vec<(String, String)>>,
}

/// A modal dialog a mod showed (`ui.dialog`). The front end renders it as a modal
/// and the user dismisses it; a click on one of its actions is delivered through
/// the mod's `on-dialog-action`.
#[cfg(feature = "mods")]
#[derive(Clone)]
pub struct ModDialog {
    pub mod_id: String,
    pub title: String,
    pub blocks: Arc<Vec<(String, String)>>,
    /// `(id, label)` action buttons, in order.
    pub actions: Arc<Vec<(String, String)>>,
}

#[derive(Default)]
pub struct AppState {
    pub channels: Arc<Vec<ChannelEntry>>,
    pub selected_channel: usize,
    pub channel_scroll: usize,
    pub messages: Vec<MessageEntry>,
    pub message_scroll: usize,
    pub view: ViewMode,
    pub input: String,
    pub search_mode: bool,
    pub search_query: String,
    pub status: String,
    pub user_names: Arc<HashMap<String, String>>,
    /// username -> display name, to resolve the participants of a multi-person
    /// IM (`mpdm-alice--bob-1`), whose name carries usernames, not ids.
    pub usernames: Arc<HashMap<String, String>>,
    pub thread_parent_ts: Option<String>,
    /// The conversation the open thread belongs to. It is kept apart from
    /// `selected_channel`: switching conversations must not retarget the thread
    /// panel or its composer.
    pub thread_channel_id: Option<String>,
    pub thread_messages: Vec<MessageEntry>,
    pub thread_scroll: usize,
    /// The channel whose "info" panel is open, with name, topic, purpose and
    /// member count. `None` closes the panel.
    pub channel_info: Option<crate::api::client::ChannelInfo>,
    /// The members of that channel, as user ids: the interface resolves the
    /// names. Only the first page of `conversations.members` (see the cap in
    /// [`crate::api::Client::conversations_members`]).
    pub channel_members: Vec<String>,
    /// user id -> avatar URL (`profile.image_72`).
    pub avatars: Arc<HashMap<String, String>>,
    /// Users that are a bot (`is_bot`) or an app (`is_app_user`): it feeds the
    /// "Agenti e app" sidebar section.
    pub bot_users: Arc<std::collections::HashSet<String>>,
    /// The sidebar folders, in Slack's order. Empty until they are loaded:
    /// the front ends then show the flat list.
    pub folders: Arc<Vec<sections::Folder>>,
    /// Folders the user has collapsed, by id.
    pub collapsed: Arc<std::collections::HashSet<String>>,
    /// Conversations the user has starred, by channel id. It is persisted in
    /// the store's `meta` table under `"favorites"`, so it survives a restart.
    pub favorites: Arc<std::collections::HashSet<String>>,
    /// emoji name -> URL, for the workspace's custom emoji only.
    pub emoji: Arc<HashMap<String, String>>,
    /// user id -> connected right now. It comes from `presence_change`
    /// events and covers only the subscribed users, not the whole workspace.
    pub presence: Arc<HashMap<String, bool>>,
    /// user id -> the status the person chose (emoji and text).
    pub user_status: Arc<HashMap<String, UserStatus>>,
    /// channel id -> user id -> when their "typing" indicator expires. Slack
    /// sends no "stopped typing", so this is a timestamp that goes stale.
    pub typing: Arc<HashMap<String, HashMap<String, std::time::Instant>>>,
    /// user id -> quiet hours, from `dnd.teamInfo`.
    pub dnd: Arc<HashMap<String, crate::api::client::DndInfo>>,
    /// The call in progress, if there is one.
    pub call: Option<Arc<crate::calls::live::Call>>,
    /// The huddles open right now, by channel.
    ///
    /// It is the state that makes an incoming call visible: without it, a colleague
    /// calling leaves no trace in the client at all.
    pub huddles: HashMap<String, Huddle>,
    /// Bots a name request has already gone out for, so the various entry
    /// points do not fire four of them for the same one.
    pub bots_pending: std::collections::HashSet<String>,
    /// The results of the last message search.
    pub search_results: Arc<Vec<crate::api::client::SearchHit>>,
    pub searching: bool,
    /// Pinned messages of the conversation on screen, newest first. Loaded when
    /// the info panel opens, refreshed on pin/unpin.
    pub pins: Vec<MessageEntry>,
    /// True while the pins of the open conversation are being fetched.
    pub pins_loading: bool,
    /// The "Later" list: saved messages with their content resolved, newest
    /// first. Loaded when that view opens, refreshed on save/unsave.
    pub saved: Vec<SavedRow>,
    /// The counters Slack reports for the saved list.
    pub saved_counts: crate::api::client::SavedCounts,
    /// True while the saved list is being fetched, so the view can show a
    /// spinner instead of a misleading "nothing saved".
    pub saved_loading: bool,
    /// The "Threads" inbox: threads the user is subscribed to, newest first.
    pub threads: Vec<ThreadRow>,
    /// Unread replies across all of them.
    pub threads_unread: i64,
    /// True while they are being fetched.
    pub threads_loading: bool,
    /// Messages that mention the user (`search.messages has:mention`).
    pub mentions: Arc<Vec<crate::api::client::SearchHit>>,
    /// True while they are being fetched.
    pub mentions_loading: bool,
    /// Messages queued to be sent later (client-side).
    pub scheduled: Vec<ScheduledMessage>,
    /// How many messages of the open channel to show. It grows when
    /// scrolling back; it returns to the initial value when changing
    /// conversation.
    pub window: u32,
    /// Interface scale factor (1.0 = native), persisted in the store. The GUI
    /// applies it as a zoom factor; other front ends ignore it.
    pub ui_scale: f32,
    /// Sidebar (channel list) width in points, persisted in the store. The GUI
    /// makes the panel resizable and remembers the dragged width; other front
    /// ends ignore it. `0.0` (the `Default`) means "unset" — the GUI then uses
    /// [`SIDEBAR_WIDTH_DEFAULT`].
    pub sidebar_width: f32,
    /// Desktop notifications on or off, persisted in the store. The settings
    /// panel flips it; the notifier reads the same value.
    pub desktop_notifications: bool,
    /// A backward load is in progress: without this, scrolling to the top
    /// would start one on every frame.
    pub loading_older: bool,
    /// There is nothing more to load: we stop asking.
    pub reached_start: bool,
    /// The channel to open at startup instead of the first in the list.
    pub initial_channel: Option<String>,
    /// Our own user id: needed to recognise mentions.
    pub my_user_id: String,
    /// The team id, for building the huddle URL. The stored credentials do
    /// not always contain it: it is read from the cache and, if missing, the
    /// background task resolves it.
    pub team_id: String,
    /// Wakes the front end when the state changes.
    ///
    /// Without it, a GUI that repaints on a timer shows the data with the delay of
    /// its interval: messages arrived in 250 ms and appeared a second later. The
    /// core does not know what a window is — it just calls.
    #[allow(clippy::type_complexity)]
    pub waker: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Whether the WebSocket is connected. It tells "no new messages" from
    /// "we are not receiving anything any more".
    pub live: bool,
    /// Whether the WebSocket has ever connected. The next `Connected` after a
    /// drop is a reconnection that must reload what the socket missed.
    pub ws_seen: bool,
    /// Consecutive empty backfill pages. One can be a hiccup; two mean the
    /// start of the conversation.
    pub empty_loads: u8,
    /// Whether the stored session is usable. It is false when the app was
    /// started without credentials or the session turned out to be expired:
    /// the GUI then shows the login screen instead of the chat.
    pub authenticated: bool,
    /// The session expired while in use (`invalid_auth`, `token_revoked`…).
    /// Unlike `authenticated`, the chat stays on screen and the GUI asks first:
    /// a modal proposes to sign in again, so the client is not yanked away on
    /// one failed request.
    pub session_expired: bool,
    /// The in-app notification centre: the messages that deserve attention
    /// (DMs, explicit mentions, broadcasts), newest first.
    pub notifications: Vec<NotifEntry>,
    /// Transient messages (errors) the GUI shows in a corner and drops.
    pub toasts: Vec<Toast>,
    /// The `ts` of the message to bring into view, if a notification was
    /// clicked and we still have to jump there. Cleared once done.
    pub jump_to_ts: Option<String>,
    /// The conversation the pending jump belongs to. While it is set the list
    /// only honours the jump in that conversation, so a jump asked before the
    /// conversation has switched cannot page the wrong one.
    pub jump_channel: Option<String>,
    /// The conversation the messages in `messages` belong to. The list uses it
    /// (by id, not by sidebar position) to decide whether a pending jump
    /// applies to what is on screen, so a shifted sidebar index cannot make it
    /// page the wrong conversation.
    pub messages_channel: Option<String>,
    /// The loaded mods, if the `mods` feature is on and any are installed.
    /// The mod host, shared as `Arc<Host>` (not `Arc<Mutex<Host>>`): its dispatch
    /// methods take `&self` and lock only their own small cache, so mods run
    /// concurrently — each on its own worker thread — with no global lock.
    #[cfg(feature = "mods")]
    pub mods: Option<Arc<crate::mods::Host>>,
    /// A mod (re)load is in progress (see `load_mods_and_publish`).
    #[cfg(feature = "mods")]
    pub mods_loading: bool,
    /// Another (re)load was requested while one was in progress.
    #[cfg(feature = "mods")]
    pub mods_reload_again: bool,
    /// The Slack API client shared with the mods host (the `Write` capabilities).
    /// `None` when unset (tests), where a write fails closed.
    #[cfg(feature = "mods")]
    pub api: Option<Arc<crate::api::Client>>,
    /// Channels with more mod work pending than one `run_mods` call covers; the
    /// worker task drains it (deduplicated via `mods_queued`).
    #[cfg(feature = "mods")]
    pub mods_queue: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    #[cfg(feature = "mods")]
    pub mods_queued: std::collections::HashSet<String>,
    /// Channels with a scheduled budget-expiry retry (deduplicated), so an
    /// exhausted budget resumes at the next window instead of waiting forever.
    #[cfg(feature = "mods")]
    pub mods_retry: std::collections::HashSet<String>,
    /// Per-channel follow-up budget: `(claims used, window start)`. Bounds how
    /// much pending work is drained per window; an exhausted budget leaves the
    /// tail of the window undecorated until a new window opens (a tracked,
    /// deliberate limit — see [`MAX_MOD_FOLLOWUPS`]).
    #[cfg(feature = "mods")]
    pub mods_followup: HashMap<String, (u32, std::time::Instant)>,
    /// Mod decorations to render under messages, by channel and then `ts`
    /// (nested so rendering can look up with `&str` without allocating).
    pub decorations: Arc<HashMap<String, HashMap<String, Vec<String>>>>,
    /// The message actions the loaded mods declared (`ui.message_action`),
    /// refreshed on (re)load.
    #[cfg(feature = "mods")]
    pub mod_actions: Arc<Vec<ModAction>>,
    /// The context-menu items the loaded mods declared (`ui.context_menu`),
    /// rendered in a message's context menu.
    #[cfg(feature = "mods")]
    pub mod_context_actions: Arc<Vec<ModAction>>,
    /// The TUI's open mod-action menu, if any.
    #[cfg(feature = "mods")]
    pub mod_action_menu: Option<ModActionMenu>,
    /// The views the loaded mods declared (`ui.view`), refreshed on (re)load.
    #[cfg(feature = "mods")]
    pub mod_views: Arc<Vec<ModView>>,
    /// The TUI's open view picker, if any.
    #[cfg(feature = "mods")]
    pub mod_view_menu: Option<ModViewMenu>,
    /// The open mod-view panel (its content), if any.
    #[cfg(feature = "mods")]
    pub mod_view_panel: Option<ModViewPanel>,
    /// The modal dialog a mod asked to show (`ui.dialog`), if any.
    #[cfg(feature = "mods")]
    pub mod_dialog: Option<ModDialog>,
    /// The TUI's cursor into `mod_dialog`'s actions (one past the last is the
    /// trailing "Dismiss" row).
    #[cfg(feature = "mods")]
    pub mod_dialog_cursor: usize,
    /// The slash commands the loaded mods declared (`commands.slash`), refreshed
    /// on (re)load.
    #[cfg(feature = "mods")]
    pub mod_commands: Arc<Vec<ModCommand>>,
    /// The composer actions the loaded mods declared (`ui.composer_action`),
    /// refreshed on (re)load.
    #[cfg(feature = "mods")]
    pub mod_composer_actions: Arc<Vec<ModComposerAction>>,
    /// The colour-role overrides the loaded mods declared (`ui.theme`), in load
    /// order; the GUI merges them onto the default palette. Refreshed on reload.
    #[cfg(feature = "mods")]
    pub mod_theme: Arc<Vec<(String, String)>>,
    /// `(channel, text)` a composer action asked the composer to hold; the front
    /// end takes it into its input on the next frame.
    #[cfg(feature = "mods")]
    pub composer_prefill: Option<(String, String)>,
    /// The consent dialog ("Mods"), when it should be on screen.
    #[cfg(feature = "mods")]
    pub mods_consent: Option<ModConsentDialog>,
    /// True while an Apply/Dismiss is persisting and reloading; a startup scan or
    /// a manual open must not show a stale dialog over it.
    #[cfg(feature = "mods")]
    pub mods_consent_busy: bool,
    /// Set once the user has answered any consent dialog this session, so the
    /// startup scan never re-shows a prompt after a decision.
    #[cfg(feature = "mods")]
    pub mods_decided_session: bool,
    /// Loaded mods whose package changed since the user saved some of their
    /// secrets ("Mod updated"), waiting for Keep or Forget. Until then the new
    /// package runs without them.
    #[cfg(feature = "mods")]
    pub mods_secrets: Vec<crate::mods::StaleSecrets>,
    /// `(mod id, package)` the user answered "Not now" this session, so a
    /// reload does not ask again for the same package.
    #[cfg(feature = "mods")]
    pub mods_secrets_later: std::collections::HashSet<(String, String)>,
    /// The settings form ("Mod settings"), when it should be on screen.
    #[cfg(feature = "mods")]
    pub mods_settings: Option<ModSettingsDialog>,
}

impl AppState {
    /// The `Msg` (with thread info and file metadata) for a message on screen, if
    /// any: what a mod sees via `get-message` in a message-action callback.
    #[cfg(feature = "mods")]
    pub fn find_mod_msg(&self, channel: &str, ts: &str) -> Option<crate::mods::Msg> {
        // The timeline and the open thread both belong to `channel`, and a thread
        // reply is not in the timeline: check both, in turn (a reply is found only
        // in the thread).
        if self.messages_channel.as_deref() == Some(channel) {
            if let Some(m) = self.messages.iter().find(|m| m.ts == ts) {
                return Some(App::mod_msg(m, &self.my_user_id));
            }
        }
        if self.thread_channel_id.as_deref() == Some(channel) {
            if let Some(m) = self.thread_messages.iter().find(|m| m.ts == ts) {
                return Some(App::mod_msg(m, &self.my_user_id));
            }
        }
        None
    }

    /// Claims a follow-up dispatch slot for `channel`. The budget window starts at
    /// the first claim and resets once [`MODS_FOLLOWUP_WINDOW`] has elapsed (not on
    /// every attempt), so sustained activity cannot lock the channel out forever.
    /// Returns `false` when the budget is exhausted.
    #[cfg(feature = "mods")]
    fn claim_followup(&mut self, channel: &str, now: std::time::Instant) -> bool {
        // Drop windows that have elapsed, so the map does not grow forever.
        self.mods_followup
            .retain(|_, (_, start)| now.duration_since(*start) <= MODS_FOLLOWUP_WINDOW);
        let entry = self
            .mods_followup
            .entry(channel.to_string())
            .or_insert((0, now));
        if now.duration_since(entry.1) > MODS_FOLLOWUP_WINDOW {
            *entry = (0, now);
        }
        if entry.0 < MAX_MOD_FOLLOWUPS {
            entry.0 += 1;
            true
        } else {
            false
        }
    }

    /// Reports a failure: it goes in the status line *and* pops as a toast, so
    /// an error is not just a faint line in the top bar.
    pub fn fail(&mut self, text: String) {
        self.status = text.clone();
        self.push_toast(text, true);
    }

    /// Adds a toast, refreshing an identical recent one instead of stacking it,
    /// and keeping the list small.
    pub fn push_toast(&mut self, text: String, error: bool) {
        const MAX: usize = 4;
        let now = std::time::Instant::now();
        if let Some(t) = self.toasts.iter_mut().find(|t| t.text == text) {
            t.at = now;
            t.error = error;
            return;
        }
        self.toasts.push(Toast {
            text,
            at: now,
            error,
        });
        if self.toasts.len() > MAX {
            let drop = self.toasts.len() - MAX;
            self.toasts.drain(0..drop);
        }
    }
}

/// One entry of the in-app notification centre.
///
/// It is what [`crate::notify::from_message`] decided was worth attention,
/// kept so the bell can list it again without re-deciding.
#[derive(Clone)]
pub struct NotifEntry {
    pub channel_id: String,
    pub channel_name: String,
    pub author: String,
    /// The message text, with the author prefix already stripped.
    pub text: String,
    /// The `ts` of the message, to jump to it.
    pub ts: String,
    /// True for a direct message: it changes how the entry is shown.
    pub is_dm: bool,
    /// The thread root, when the message is a reply (`thread_ts != ts`).
    pub thread_ts: Option<String>,
    /// False until the entry is opened or the centre is cleared.
    pub read: bool,
}

/// A transient message shown in a corner of the GUI.
///
/// Errors are surfaced here, not only in the top-bar status line: a faint line
/// in the bar is easy to miss, while a toast pops and goes away.
#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub at: std::time::Instant,
    /// Errors stay a little longer and are drawn in red.
    pub error: bool,
}

/// Keeps the centre bounded and free of duplicates.
///
/// The same message can arrive twice — over the WebSocket and in an initial
/// sync — and a list that only grows would end up with the same mention ten
/// times. Newest first, at most [`NOTIFICATIONS_MAX`].
pub const NOTIFICATIONS_MAX: usize = 200;

/// How long a "typing" indicator lasts without another `user_typing`: Slack
/// has no "stopped typing" event, so it is a timestamp that goes stale.
pub const TYPING_TTL: std::time::Duration = std::time::Duration::from_secs(6);

pub fn push_notification(state: &mut AppState, entry: NotifEntry) {
    // Slack re-delivers events (a reconnect resends them): a notification the
    // user already read must not come back unread just because it arrived twice.
    let was_read = state
        .notifications
        .iter()
        .any(|n| n.channel_id == entry.channel_id && n.ts == entry.ts && n.read);
    state
        .notifications
        .retain(|n| !(n.channel_id == entry.channel_id && n.ts == entry.ts));
    let mut entry = entry;
    if was_read {
        entry.read = true;
    }
    state.notifications.insert(0, entry);
    state.notifications.truncate(NOTIFICATIONS_MAX);
}

/// Fills the notification centre from the cache for the conversations Slack
/// already marks as unread at startup.
///
/// Without it the bell would stay empty until the next live message, even
/// though the sidebar shows unread DMs and mentions that arrived while the
/// client was closed.
pub async fn seed_notifications(
    state: &Arc<RwLock<AppState>>,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
) {
    const PER_CHANNEL: usize = 5;
    let (targets, names, me) = {
        let s = state.read().await;
        let targets: Vec<(String, String, bool)> = s
            .channels
            .iter()
            .filter(|c| c.unread || c.mentions > 0)
            .map(|c| (c.id.clone(), c.name.clone(), c.is_dm))
            .collect();
        (targets, s.user_names.clone(), s.my_user_id.clone())
    };
    if targets.is_empty() {
        return;
    }

    let mut fresh: Vec<NotifEntry> = Vec::new();
    for (id, channel_name, is_dm) in targets {
        let rows = {
            let st = store.lock().await;
            st.messages(&id, 30).unwrap_or_default()
        };
        let mut found = 0;
        // Newest first: only the most recent few per conversation, or a long
        // unread history would bury everything else.
        for m in rows.iter().rev() {
            if found >= PER_CHANNEL {
                break;
            }
            let author = names
                .get(&m.user)
                .cloned()
                .unwrap_or_else(|| m.user.clone());
            let ctx = crate::notify::Context {
                my_user_id: &me,
                channel_name: &channel_name,
                is_dm,
                author: &author,
                names: &names,
            };
            let Some(n) = crate::notify::from_message(&m.raw, &ctx) else {
                continue;
            };
            let ts = m.ts.clone();
            let thread_ts = m
                .raw
                .get("thread_ts")
                .and_then(|v| v.as_str())
                .filter(|t| *t != ts)
                .map(str::to_string);
            let body = n.body();
            let text = body
                .strip_prefix(&format!("{author}: "))
                .map(str::to_string)
                .unwrap_or(body);
            fresh.push(NotifEntry {
                channel_id: id.clone(),
                channel_name: channel_name.clone(),
                author,
                text,
                ts,
                is_dm,
                thread_ts,
                read: false,
            });
            found += 1;
        }
    }
    if fresh.is_empty() {
        return;
    }

    fresh.sort_by(|a, b| {
        ts_key(&a.ts)
            .partial_cmp(&ts_key(&b.ts))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut s = state.write().await;
    for e in fresh {
        push_notification(&mut s, e);
    }
    wake(&s);
}

/// Sortable numeric form of a Slack `ts` (`"1700000000.000100"`).
fn ts_key(ts: &str) -> f64 {
    ts.parse().unwrap_or(0.0)
}

/// A huddle in progress in a conversation.
pub struct Huddle {
    pub room_id: String,
    /// Who is in it right now.
    pub participants: Vec<String>,
    /// Who called us, if the invite came to us.
    pub caller: Option<String>,
    /// It is ringing: it stops when you answer, when you ignore it, or
    /// when the caller hangs up.
    pub ringing: bool,
}

#[derive(Clone)]
pub struct ChannelEntry {
    pub id: String,
    pub name: String,
    /// True for direct messages: it changes how they are shown (no `#`).
    pub is_dm: bool,
    pub unread: bool,
    /// Unread mentions: shown as a number, the rest as a dot.
    pub mentions: i64,
    /// For a two-party conversation, the other side. `name` is replaced by
    /// the readable name as soon as the users are loaded, so on its own it is no
    /// longer enough to work out who it is — and it is needed to ask for their
    /// presence.
    pub user: Option<String>,
    /// How far Slack has us reading. It is there so we never mark
    /// backwards.
    pub last_read: String,
    /// A space shared with another organisation.
    pub external: bool,
}

/// A file shared in a conversation.
#[derive(Debug, Clone, Default)]
pub struct FileEntry {
    /// Slack's opaque file id (`F…`), the handle a mod downloads by.
    pub id: String,
    pub name: String,
    pub mimetype: String,
    /// A preview, images only. It needs the session cookie.
    pub thumb: String,
    /// The full-resolution original, for the enlarged view.
    pub url: String,
    /// For downloading: Slack serves it as an attachment, not inline.
    pub download_url: String,
    /// The file's page on Slack, to open in the browser.
    pub permalink: String,
    pub size: i64,
}

impl FileEntry {
    pub fn is_image(&self) -> bool {
        self.mimetype.starts_with("image/") && !self.thumb.is_empty()
    }
}

/// An attachment: bots put all the content in it, leaving `text` empty.
#[derive(Debug, Clone, Default)]
pub struct Attachment {
    /// The colour of the bar on the left, in hex without the `#`.
    pub color: String,
    pub title: String,
    pub title_link: String,
    pub text: String,
    /// A link preview's picture: the thumbnail to draw, and the full one to open
    /// (Slack sends both, and often only one of them).
    pub thumb: String,
    pub image: String,
}

pub struct MessageEntry {
    pub user: String,
    pub text: String,
    pub ts: String,
    pub reactions: Vec<Reaction>,
    pub thread_ts: Option<String>,
    pub reply_count: Option<i64>,
    /// The avatar URL when the author is a bot: it is not among the users.
    pub author_icon: String,
    pub attachments: Vec<Attachment>,
    pub files: Vec<FileEntry>,
    /// The interactive buttons an app put in the message's `actions` blocks.
    pub actions: Vec<ActionButton>,
    /// The bot that owns the message: the `service_id` of an action dispatch.
    pub bot_id: Option<String>,
    /// The app that owns the bot, when the message carries a `bot_profile`.
    pub app_id: Option<String>,
}

/// A message saved for later ("Later"), with its content resolved.
pub struct SavedRow {
    /// The conversation it lives in.
    pub channel: String,
    pub ts: String,
    /// `in_progress`, `completed` or `archived`.
    pub state: String,
    pub message: MessageEntry,
}

/// One thread in the "Threads" inbox.
pub struct ThreadRow {
    /// The conversation the thread lives in.
    pub channel: String,
    /// The thread's root message.
    pub root: MessageEntry,
    /// Its most recent replies.
    pub replies: Vec<MessageEntry>,
    /// How many replies it has in total.
    pub reply_count: i64,
}

/// A message queued to be sent later, by the client itself.
///
/// Slack's `chat.scheduleMessage` rejects the session token, so this is a
/// local queue: it is sent at `post_at` as long as slaq is running.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ScheduledMessage {
    pub id: String,
    pub channel: String,
    pub channel_name: String,
    pub text: String,
    /// Unix time (seconds) at which to send it.
    pub post_at: i64,
}

/// A button from an `actions` block: the user can click it to make an app do
/// something. `raw` is the element exactly as Slack sent it, because it is
/// echoed back verbatim in the dispatch payload.
#[derive(Debug, Clone)]
pub struct ActionButton {
    pub action_id: String,
    pub block_id: String,
    pub text: String,
    pub value: String,
    /// `primary`, `danger` or empty, for the colour.
    pub style: String,
    /// A pure link button: it opens this address instead of dispatching.
    pub url: Option<String>,
    pub raw: serde_json::Value,
}

pub struct Reaction {
    /// Whether our own reaction is among these: it decides whether a click
    /// adds or removes.
    pub mine: bool,
    pub name: String,
    pub count: i64,
    /// Who reacted. Slack sends the list on every message, and without it a
    /// reaction says how many agreed but not who, which is usually the
    /// question being asked.
    pub users: Vec<String>,
}

/// Localized day and month names, as translation keys.
///
/// `chrono` would translate them only with an unstable feature; the names live
/// in the locale files like everything else.
const DAY_KEYS: [&str; 7] = [
    "app.day_mon",
    "app.day_tue",
    "app.day_wed",
    "app.day_thu",
    "app.day_fri",
    "app.day_sat",
    "app.day_sun",
];
const MONTH_KEYS: [&str; 12] = [
    "app.month_jan",
    "app.month_feb",
    "app.month_mar",
    "app.month_apr",
    "app.month_may",
    "app.month_jun",
    "app.month_jul",
    "app.month_aug",
    "app.month_sep",
    "app.month_oct",
    "app.month_nov",
    "app.month_dec",
];

/// Localized weekday name (0 = Monday).
pub(crate) fn day_name(i: usize) -> String {
    t!(DAY_KEYS[i]).to_string()
}

/// Localized month name (0 = January).
pub(crate) fn month_name(i: usize) -> String {
    t!(MONTH_KEYS[i]).to_string()
}

fn local(ts: &str) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone;
    let secs: i64 = ts.split('.').next()?.parse().ok()?;
    chrono::Local.timestamp_opt(secs, 0).single()
}

/// The time only, for the message header.
///
/// It uses the local time zone: there used to be a fixed `+2`, which in winter
/// was an hour off and outside central Europe always.
pub fn format_ts(ts: &str) -> String {
    use chrono::Timelike;
    match local(ts) {
        Some(d) => format!("{:02}:{:02}", d.hour(), d.minute()),
        None => String::new(),
    }
}

/// The full date, for the hover tooltip.
///
/// For today's and yesterday's messages it is written this way instead of the
/// date: that is the information you actually want when scrolling through a
/// conversation.
pub fn format_ts_full(ts: &str) -> String {
    use chrono::{Datelike, Timelike};
    let Some(d) = local(ts) else {
        return String::new();
    };
    let ora = format!("{:02}:{:02}", d.hour(), d.minute());
    let oggi = chrono::Local::now().date_naive();
    let giorni_fa = (oggi - d.date_naive()).num_days();

    match giorni_fa {
        0 => t!("app.today_at", time = ora).to_string(),
        1 => t!("app.yesterday_at", time = ora).to_string(),
        _ => {
            let g = day_name(d.weekday().num_days_from_monday() as usize);
            let m = month_name((d.month() - 1) as usize);
            t!(
                "app.date_full",
                weekday = g,
                day = d.day(),
                month = m,
                year = d.year(),
                time = ora
            )
            .to_string()
        }
    }
}

/// The date alone, for a day divider in the conversation: "Today", "Yesterday"
/// or the full weekday/month/day/year. No time — the header marks the day, the
/// per-message time stays on each row. The first character is upper-cased: the
/// localized strings are lowercase (they read mid-sentence elsewhere) but a
/// divider is a header.
pub fn format_day(ts: &str) -> String {
    use chrono::Datelike;
    let Some(d) = local(ts) else {
        return String::new();
    };
    let today = chrono::Local::now().date_naive();
    let s = match (today - d.date_naive()).num_days() {
        0 => t!("app.today").to_string(),
        1 => t!("app.yesterday").to_string(),
        _ => {
            let g = day_name(d.weekday().num_days_from_monday() as usize);
            let m = month_name((d.month() - 1) as usize);
            t!(
                "app.date_day",
                weekday = g,
                day = d.day(),
                month = m,
                year = d.year()
            )
            .to_string()
        }
    };
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => s,
    }
}

#[cfg(test)]
mod format_day_tests {
    use super::format_day;

    #[test]
    fn a_bad_timestamp_has_no_day_label() {
        assert_eq!(format_day(""), "");
        assert_eq!(format_day("not-a-timestamp"), "");
    }

    #[test]
    fn a_day_label_is_non_empty_and_capitalised() {
        // A fixed past date (2023-11-14), well before `now`, so it always takes
        // the full-date branch rather than today/yesterday.
        let s = format_day("1700000000.000100");
        assert!(!s.is_empty(), "expected a date label");
        assert!(
            s.chars().next().unwrap().is_uppercase(),
            "a divider is a header: {s:?} should start upper-case"
        );
    }
}

pub fn parse_message(v: &serde_json::Value) -> Option<MessageEntry> {
    parse_message_as(v, "")
}

/// Resolves the names of the bots that appear in the messages in
/// memory.
///
/// It has to be called **where the messages enter the state**, not where a
/// conversation is opened: the loading is asynchronous, so asking right after
/// starting it still finds the old list — and that is why the id sometimes
/// appeared and sometimes did not. It applies to messages arriving over the
/// WebSocket and to those fetched by scrolling back too.
///
/// It is idempotent and can be called freely: a bot already known or already
/// being resolved produces no traffic.
pub fn spawn_resolve_bots(state: &Arc<RwLock<AppState>>, api: &crate::api::Client) {
    let state = state.clone();
    let api = api.clone();
    tokio::spawn(async move {
        let unknown: Vec<(String, Unresolved)> = {
            let mut s = state.write().await;
            let mut v: Vec<String> = s
                .messages
                .iter()
                .chain(&s.thread_messages)
                .map(|m| m.user.clone())
                .collect();
            v.sort();
            v.dedup();
            let kinds: Vec<(String, Unresolved)> = v
                .into_iter()
                .filter_map(|u| unresolved(&u, &s.user_names).map(|k| (u, k)))
                .collect();
            let mut out = Vec::new();
            for (id, kind) in kinds {
                if s.bots_pending.insert(id.clone()) {
                    out.push((id, kind));
                }
            }
            out
        };

        for (id, kind) in unknown {
            let found = match kind {
                Unresolved::Bot => api.bot_info(&id).await.map(|(n, i)| (n, i, true)),
                // Slack Connect: the person exists, but in another
                // organisation. `users.info` finds them all the same.
                Unresolved::Person => api
                    .users_info(&id)
                    .await
                    .map(|u| (u.full_name(), u.profile.image_72, u.is_bot || u.is_app_user)),
            };
            match found {
                Ok((name, icon, is_bot)) if !name.is_empty() => {
                    let mut s = state.write().await;
                    Arc::make_mut(&mut s.user_names).insert(id.clone(), name);
                    if is_bot {
                        Arc::make_mut(&mut s.bot_users).insert(id.clone());
                    }
                    if !icon.is_empty() {
                        Arc::make_mut(&mut s.avatars).insert(id, icon);
                    }
                    wake(&s);
                }
                Ok(_) => {
                    // An empty name must not keep the id pending forever: allow
                    // a later retry.
                    state.write().await.bots_pending.remove(&id);
                }
                Err(e) => {
                    tracing::debug!("identity {id} not resolved: {e}");
                    // A network failure must not prevent a later attempt.
                    state.write().await.bots_pending.remove(&id);
                }
            }
        }
    });
}

/// Whether `candidate` is newer than `known`, and so can be marked as
/// read.
///
/// Slack's timestamps are strings like `1790779140.488369`, orderable only by
/// comparing their numeric value: `"999.0"` as a string comes after
/// `"1790779140.0"`.
///
/// A guard is needed because marking an **older** message moves the read
/// marker backwards, and the official client reopens the conversation dozens of
/// messages earlier. It really happened: we were marking the last message still
/// in memory, which during a channel switch is the *previous* conversation's.
pub fn is_newer(candidate: &str, known: &str) -> bool {
    let n = |s: &str| s.parse::<f64>().ok();
    match (n(candidate), n(known)) {
        (Some(a), Some(b)) => a > b,
        // With no valid reference we go ahead: it is the first mark.
        (Some(_), None) => true,
        _ => false,
    }
}

/// Adds or removes a reaction in a message's raw JSON.
///
/// Slack never resends the message when a reaction changes: it sends an event
/// about it. Whoever keeps a copy has to apply it themselves, or the copy
/// slowly stops matching what everyone else sees — which is exactly what
/// happened: the reactions were there until you changed channel, because the
/// view came back from a cache that had never heard about them.
///
/// It keeps the shape Slack uses, `{name, count, users}`, because the same
/// JSON is what the rest of the client parses.
pub fn apply_reaction(raw: &mut serde_json::Value, user: &str, name: &str, added: bool) {
    let Some(obj) = raw.as_object_mut() else {
        return;
    };
    let list = obj
        .entry("reactions")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let Some(list) = list.as_array_mut() else {
        return;
    };

    let found = list
        .iter_mut()
        .position(|r| r.get("name").and_then(|n| n.as_str()) == Some(name));

    match found {
        Some(i) => {
            let users = list[i]
                .get("users")
                .and_then(|u| u.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|u| u.as_str())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_else(Vec::new);
            let mut users: Vec<String> = users;
            if added {
                // The same event can arrive twice (on a reconnection): it
                // has to be applied once.
                if !users.iter().any(|u| u == user) {
                    users.push(user.to_string());
                }
            } else {
                users.retain(|u| u != user);
            }
            if users.is_empty() {
                list.remove(i);
            } else {
                list[i] = serde_json::json!({
                    "name": name,
                    "count": users.len(),
                    "users": users,
                });
            }
        }
        None if added => list.push(serde_json::json!({
            "name": name,
            "count": 1,
            "users": [user],
        })),
        None => {}
    }

    if list.is_empty() {
        obj.remove("reactions");
    }
}

/// What kind of identity one we cannot name yet is.
///
/// Slack's ids are prefixed by type: `U`/`W` for people, `B` for bots. It is
/// not a documented rule as such, but it is consistent everywhere in the API,
/// and the alternative would be carrying the field around for every message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unresolved {
    /// `B…`: an integration, to ask `bots.info` about.
    Bot,
    /// `U…` or `W…`: a person absent from `users.list`.
    ///
    /// It happens with Slack Connect: people from other organisations write in
    /// shared spaces, and `users.list` lists only our own team. `users.info`
    /// does find them.
    Person,
}

/// Whether this identity needs resolving, and how.
pub fn unresolved(id: &str, known: &HashMap<String, String>) -> Option<Unresolved> {
    if known.contains_key(id) || id.len() < 4 {
        return None;
    }
    // Slack's ids are prefixed by type and made of capitals and digits.
    if !id[1..]
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return None;
    }
    match id.chars().next() {
        Some('B') => Some(Unresolved::Bot),
        Some('U') | Some('W') => Some(Unresolved::Person),
        _ => None,
    }
}

/// Extracts the readable text from a list of Block Kit blocks.
///
/// Modern bots no longer fill `text`: they put everything in `blocks`, and
/// whoever reads only `text` sees empty rows. The attachment's `fallback`
/// should make up for it, but it is at the bot author's discretion and often
/// contains `[no preview available]`, which helps nobody.
///
/// Only the text is taken: buttons, images and dividers have no sensible
/// textual rendering and are skipped.
pub fn blocks_text(blocks: &serde_json::Value) -> String {
    let Some(arr) = blocks.as_array() else {
        return String::new();
    };
    let mut out: Vec<String> = Vec::new();

    for b in arr {
        match b.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "section" | "header" => {
                if let Some(t) = text_of(b.get("text")) {
                    out.push(t);
                }
                // The columns of a `section` with fields: they count as rows.
                if let Some(fields) = b.get("fields").and_then(|v| v.as_array()) {
                    out.extend(fields.iter().filter_map(|f| text_of(Some(f))));
                }
            }
            "context" => {
                let parts: Vec<String> = b
                    .get("elements")
                    .and_then(|v| v.as_array())
                    .map(|els| els.iter().filter_map(|e| text_of(Some(e))).collect())
                    .unwrap_or_default();
                if !parts.is_empty() {
                    out.push(parts.join(" "));
                }
            }
            // `rich_text` nests the elements of a paragraph: the text is in the
            // leaves, and concatenating them rebuilds the line.
            "rich_text" => {
                let t = rich_text(b);
                if !t.trim().is_empty() {
                    out.push(t);
                }
            }
            _ => {}
        }
    }
    out.join("\n")
}

/// The text to show, choosing between `text` and `blocks`.
///
/// `text` is the fallback Slack uses for notifications and screen readers: it
/// is a **flattened** version, and for app messages it loses the links. The
/// GitHub message announcing a review is the example: in `text` the PR title is
/// plain text, in `blocks` it is a link.
///
/// `rich_text` blocks are the exception: they are the representation of what a
/// person wrote, and the corresponding `text` is the equivalent mrkdwn,
/// complete with markup. There `text` is perfectly good, and it is the
/// better-tested road.
pub fn display_text(text: &str, blocks: Option<&serde_json::Value>) -> Option<String> {
    let blocks = blocks?;
    let from_blocks = blocks_text(blocks);
    if from_blocks.trim().is_empty() {
        return None;
    }
    if text.trim().is_empty() {
        return Some(from_blocks);
    }
    // Only blocks composed by an app can contain more than the `text`.
    let composed = blocks.as_array().is_some_and(|a| {
        a.iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) != Some("rich_text"))
    });
    composed.then_some(from_blocks)
}

/// The text of a `{type: mrkdwn|plain_text, text: …}` object.
fn text_of(v: Option<&serde_json::Value>) -> Option<String> {
    let t = v?.get("text")?.as_str()?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Recursively concatenates the `text` leaves of a `rich_text` block.
fn rich_text(v: &serde_json::Value) -> String {
    let mut out = String::new();

    // A link has to be rewritten into Slack's markup, not flattened into
    // its text: it is the parser downstream that turns it into a clickable link,
    // and discarding the address left `#19974` as dead text.
    if let Some(url) = v
        .get("url")
        .and_then(|t| t.as_str())
        .filter(|u| !u.is_empty())
    {
        let label = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
        // A label containing the separators would break the markup: better the
        // address alone than a crooked line.
        return if label.is_empty() || label.contains(['|', '<', '>']) {
            format!("<{url}>")
        } else {
            format!("<{url}|{label}>")
        };
    }

    if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
        // Inline code: Slack marks the *style*, not the text, so flattening it
        // lost the backticks the parser turns into `Span::Code` — an inline
        // snippet in a bot message came out as ordinary text.
        let code = v
            .get("style")
            .and_then(|s| s.get("code"))
            .and_then(|c| c.as_bool())
            .unwrap_or(false);
        if code && !t.is_empty() {
            out.push('`');
            out.push_str(t);
            out.push('`');
        } else {
            out.push_str(t);
        }
    }
    if let Some(els) = v.get("elements").and_then(|e| e.as_array()) {
        for e in els {
            // A code block: Slack carries it as `rich_text_preformatted`, and
            // flattening it to its text lost the ``` fences — so a block was
            // rendered as ordinary text (a one-line one looked nothing like a
            // multi-line one). Re-emit the fences the parser downstream expects.
            let part = match e.get("type").and_then(|t| t.as_str()) {
                // `elements` are inline pieces composing **one** line (a single
                // `text` with embedded newlines, or `text`+`link`), and
                // `rich_text` already concatenates them with no separator: joining
                // them here would split one line into two.
                Some("rich_text_preformatted") => format!("\n```\n{}\n```\n", rich_text(e)),
                _ => rich_text(e),
            };
            // Every top-level element is a paragraph of its own.
            if v.get("type").and_then(|t| t.as_str()) == Some("rich_text") && !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&part);
        }
    }
    out
}

/// The effective message payload of a WS event: for an edit or a reply the real
/// message is nested under `message` (see `ws::cache_op`), not on the event.
fn event_payload(raw: &serde_json::Value) -> &serde_json::Value {
    raw.get("message")
        .filter(|n| n.get("ts").and_then(|v| v.as_str()).is_some())
        .unwrap_or(raw)
}

/// Whether a raw WS event belongs to the open thread `parent` (whose loaded
/// replies' timestamps are `thread_ts`). Covers a reply (`thread_ts == parent`),
/// a thread-root edit (the effective message `ts == parent`) and a deletion
/// (`deleted_ts` is the root or one of the loaded replies).
fn event_matches_thread(
    raw: &serde_json::Value,
    parent: &str,
    thread_ts: &std::collections::HashSet<String>,
) -> bool {
    let payload = event_payload(raw);
    if payload.get("thread_ts").and_then(|v| v.as_str()) == Some(parent) {
        return true;
    }
    if payload.get("ts").and_then(|v| v.as_str()) == Some(parent) {
        return true;
    }
    if let Some(deleted) = raw.get("deleted_ts").and_then(|v| v.as_str()) {
        return deleted == parent || thread_ts.contains(deleted);
    }
    false
}

/// Like [`parse_message`], knowing who we are: needed to mark our own
/// reactions.
pub fn parse_message_as(v: &serde_json::Value, me: &str) -> Option<MessageEntry> {
    let raw = v.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let from_blocks = display_text(raw, v.get("blocks"));
    let text: &str = from_blocks.as_deref().unwrap_or(raw);
    let ts = v.get("ts").and_then(|v| v.as_str())?;
    // Bot messages have no `user` but a `bot_id`, and the name is in
    // `bot_profile`. Without this they all appeared as "?".
    let bot = v.get("bot_profile");
    let user = v
        .get("user")
        .and_then(|v| v.as_str())
        .or_else(|| bot.and_then(|b| b.get("name")).and_then(|v| v.as_str()))
        .or_else(|| v.get("username").and_then(|v| v.as_str()))
        // Many integrations send only `bot_id`: keeping it as the identity
        // makes it possible to resolve the name later, with `bots.info`, exactly
        // as is done for users.
        .or_else(|| v.get("bot_id").and_then(|v| v.as_str()))
        .unwrap_or("?");
    let author_icon = bot
        .and_then(|b| b.get("icons"))
        .and_then(|i| {
            i.get("image_48")
                .or_else(|| i.get("image_72"))
                .or_else(|| i.get("image_36"))
        })
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    // Attachments carry the content when `text` is empty: without
    // rendering them, an alerts channel is a list of empty rows.
    let actions: Vec<ActionButton> = v
        .get("blocks")
        .and_then(|b| b.as_array())
        .map(|blocks| {
            let mut out = Vec::new();
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) != Some("actions") {
                    continue;
                }
                let block_id = block
                    .get("block_id")
                    .and_then(|b| b.as_str())
                    .unwrap_or_default()
                    .to_string();
                let Some(elements) = block.get("elements").and_then(|e| e.as_array()) else {
                    continue;
                };
                for el in elements {
                    if el.get("type").and_then(|t| t.as_str()) != Some("button") {
                        continue;
                    }
                    let text = el
                        .get("text")
                        .and_then(|t| t.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let url = el
                        .get("url")
                        .and_then(|u| u.as_str())
                        .filter(|u| !u.is_empty())
                        .map(String::from);
                    out.push(ActionButton {
                        action_id: el
                            .get("action_id")
                            .and_then(|a| a.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        block_id: block_id.clone(),
                        text,
                        value: el
                            .get("value")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        style: el
                            .get("style")
                            .and_then(|s| s.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        url,
                        // Slack stores `block_id` on the parent `actions` block,
                        // not the element, but `blocks.actions` requires it on
                        // the submitted action — inject it or Slack answers
                        // `invalid_arguments: missing required field: block_id`.
                        raw: {
                            let mut r = el.clone();
                            if let Some(obj) = r.as_object_mut() {
                                obj.insert(
                                    "block_id".to_string(),
                                    serde_json::Value::String(block_id.clone()),
                                );
                            }
                            r
                        },
                    });
                }
            }
            out
        })
        .unwrap_or_default();
    let bot_id = v
        .get("bot_id")
        .and_then(|b| b.as_str())
        .filter(|b| !b.is_empty())
        .map(String::from);
    let app_id = bot
        .and_then(|b| b.get("app_id"))
        .and_then(|a| a.as_str())
        .filter(|a| !a.is_empty())
        .map(String::from);
    let attachments: Vec<Attachment> = v
        .get("attachments")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .map(|a| {
                    let get = |k: &str| {
                        a.get(k)
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string()
                    };
                    let mut text = get("text");
                    let title = get("title");
                    // Bots put the content in the blocks and leave `text`
                    // empty: they have to be read before the `fallback`, which is at
                    // their discretion and often says nothing.
                    if let Some(t) = display_text(&text, a.get("blocks")) {
                        text = t;
                    }
                    // `fallback` is the textual rendering Slack guarantees:
                    // it counts as a safety net, not as a duplicate.
                    if text.is_empty() && title.is_empty() {
                        text = get("fallback");
                    }
                    // Link previews can be enormous: a GitHub PR's reaches
                    // 7 KB, with commands and code blocks inside. Slack itself
                    // collapses them; rendering them in full means a wall of text
                    // per message.
                    const MAX_ATTACHMENT_TEXT: usize = 600;
                    if text.chars().count() > MAX_ATTACHMENT_TEXT {
                        text =
                            text.chars().take(MAX_ATTACHMENT_TEXT).collect::<String>() + "\u{2026}";
                    }
                    // A link preview's picture (`image_url`, or a smaller
                    // `thumb_url`; Slack often sends only one). Never fetched
                    // before, so previews showed as text only.
                    let image = get("image_url");
                    let thumb = {
                        let t = get("thumb_url");
                        if t.is_empty() { image.clone() } else { t }
                    };
                    Attachment {
                        color: get("color"),
                        title,
                        title_link: get("title_link"),
                        text,
                        thumb,
                        image,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let reactions: Vec<Reaction> = v
        .get("reactions")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let name = r.get("name").and_then(|n| n.as_str())?;
                    let count = r.get("count").and_then(|c| c.as_i64()).unwrap_or(0);
                    // `users` says who reacted: it decides whether the click
                    // adds or removes, and it is what the hover shows.
                    let users: Vec<String> = r
                        .get("users")
                        .and_then(|u| u.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|u| u.as_str())
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(Reaction {
                        name: name.to_string(),
                        count,
                        mine: users.iter().any(|u| u == me),
                        users,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let thread_ts = v
        .get("thread_ts")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let reply_count = v.get("reply_count").and_then(|v| v.as_i64());
    // The shared files: without this, a message containing only an image
    // appears as an empty row.
    let files: Vec<FileEntry> = v
        .get("files")
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .map(|f| {
                    let get = |k: &str| {
                        f.get(k)
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string()
                    };
                    FileEntry {
                        id: get("id"),
                        name: get("name"),
                        mimetype: get("mimetype"),
                        // The smallest useful thumbnail is preferred: the source
                        // can weigh megabytes.
                        thumb: ["thumb_360", "thumb_480", "thumb_720"]
                            .iter()
                            .find_map(|k| {
                                f.get(*k).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                            })
                            .unwrap_or_default()
                            .to_string(),
                        // The large thumbnail is preferred to the original: it is
                        // enough to fill the screen and weighs far less.
                        url: ["thumb_1024", "thumb_960", "thumb_720", "url_private"]
                            .iter()
                            .find_map(|k| {
                                f.get(*k).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                            })
                            .unwrap_or_default()
                            .to_string(),
                        download_url: ["url_private_download", "url_private"]
                            .iter()
                            .find_map(|k| {
                                f.get(*k).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                            })
                            .unwrap_or_default()
                            .to_string(),
                        permalink: get("permalink"),
                        size: f.get("size").and_then(|v| v.as_i64()).unwrap_or(0),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    Some(MessageEntry {
        author_icon,
        attachments,
        files,
        user: user.to_string(),
        text: text.to_string(),
        ts: ts.to_string(),
        reactions,
        thread_ts,
        reply_count,
        actions,
        bot_id,
        app_id,
    })
}

pub fn filtered_channels<'a>(channels: &'a [ChannelEntry], query: &str) -> Vec<&'a ChannelEntry> {
    if query.is_empty() {
        channels.iter().collect()
    } else {
        let q = query.to_lowercase();
        channels
            .iter()
            .filter(|ch| ch.name.to_lowercase().contains(&q))
            .collect()
    }
}

/// The glyph that marks a group conversation: the same `busts_in_silhouette`
/// emoji the client already knows, so a multi-person DM cannot be confused
/// with a one-to-one one.
const MPIM_MARKER: &str = "👥";

/// Extracts the participant usernames from a Slack multi-person IM name.
///
/// Slack names them `mpdm-<user1>--<user2>--…-<n>`, with the usernames joined
/// by `--` and a trailing group index. Anything that does not carry the
/// `mpdm-` prefix is not a multi-person IM and yields an empty list.
pub fn mpim_participants(raw: &str) -> Vec<String> {
    let Some(rest) = raw.strip_prefix("mpdm-") else {
        return Vec::new();
    };
    // The last `-<digits>` is the group index, not part of a username.
    let rest = match rest.rsplit_once('-') {
        Some((head, tail)) if tail.chars().all(|c| c.is_ascii_digit()) && !tail.is_empty() => head,
        _ => rest,
    };
    rest.split("--")
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Renders a multi-person IM name as a readable group label.
///
/// The usernames are mapped to display names when known, and joined with a
/// comma after the group marker. A name that is not a multi-person IM is
/// returned unchanged, so this is safe to call on every conversation.
fn mpim_label(raw: &str, names: &HashMap<String, String>) -> String {
    let parts = mpim_participants(raw);
    if parts.is_empty() {
        return raw.to_string();
    }
    let joined = parts
        .iter()
        .map(|u| names.get(u).cloned().unwrap_or_else(|| u.clone()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{MPIM_MARKER} {joined}")
}

/// The fragment the user is typing after `@` or `#`, if any.
///
/// Returns (position of the sigil, text typed after it). The sigil has to be at
/// the start of a line or after a space: without that constraint an email
/// address would open completion mid-word.
pub fn mention_prefix(text: &str, cursor: usize, sigil: char) -> Option<(usize, String)> {
    let upto = text.get(..cursor)?;
    let at = upto.rfind(sigil)?;
    let before_ok = at == 0
        || upto[..at]
            .chars()
            .next_back()
            .map(|c| c.is_whitespace())
            .unwrap_or(true);
    if !before_ok {
        return None;
    }
    let frag = &upto[at + sigil.len_utf8()..];
    // A space closes completion: something else is being written.
    if frag.contains(char::is_whitespace) {
        return None;
    }
    Some((at, frag.to_string()))
}

/// Wakes the front end, if one has registered.
pub fn wake(state: &AppState) {
    if let Some(w) = &state.waker {
        w();
    }
}

fn bytes_short(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn load_favorites(store: &crate::store::Store) -> std::collections::HashSet<String> {
    store
        .get_meta("favorites")
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<Vec<String>>(&v).ok())
        .map(|ids| ids.into_iter().collect())
        .unwrap_or_default()
}

fn save_favorites(store: &crate::store::Store, favorites: &std::collections::HashSet<String>) {
    let ids: Vec<&String> = favorites.iter().collect();
    let json = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_string());
    if let Err(e) = store.set_meta("favorites", &json) {
        tracing::warn!("favorites not saved: {e}");
    }
}

/// The locally-queued scheduled messages.
fn load_scheduled(store: &crate::store::Store) -> Vec<ScheduledMessage> {
    store
        .get_meta("scheduled")
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<Vec<ScheduledMessage>>(&v).ok())
        .unwrap_or_default()
}

fn save_scheduled(store: &crate::store::Store, scheduled: &[ScheduledMessage]) {
    let json = serde_json::to_string(scheduled).unwrap_or_else(|_| "[]".to_string());
    if let Err(e) = store.set_meta("scheduled", &json) {
        tracing::warn!("scheduled messages not saved: {e}");
    }
}

/// True when the error is Slack refusing the session itself, as opposed to a
/// transient network or server failure: only the former calls for a fresh login.
fn is_auth_error(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}");
    msg.contains("invalid_auth")
        || msg.contains("token_revoked")
        || msg.contains("account_inactive")
}

/// Cloneable: every field is a shared handle, so the copies work on the
/// same state. The tasks that outlive the frame that spawned them need it.
#[derive(Clone)]
pub struct App {
    creds: Credentials,
    api_client: crate::api::Client,
    state: Arc<RwLock<AppState>>,
    /// The local cache. `tokio::Mutex` because `rusqlite::Connection` is
    /// `Send` but not `Sync`, and the background tasks share it.
    store: Arc<tokio::sync::Mutex<crate::store::Store>>,
    /// The users whose presence is followed. The WebSocket holds the read
    /// side and resends `presence_sub` on every change.
    presence_tx: tokio::sync::watch::Sender<Vec<String>>,
    /// True when the app was built with sample data (`gui --demo`): the GUI
    /// then starts no network task.
    demo: bool,
}

/// The status a person chose for themselves: the emoji almost always
/// says more than the text — someone in a meeting sets
/// `:spiral_calendar_pad:` and writes nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserStatus {
    /// The emoji name with the colons, as Slack returns it.
    pub emoji: String,
    pub text: String,
}

impl UserStatus {
    pub fn is_empty(&self) -> bool {
        self.emoji.is_empty() && self.text.is_empty()
    }
}

/// Whether the person is in quiet hours right now.
///
/// `dnd_enabled` on its own is not enough: it says a schedule is
/// **configured**, and for almost everyone it is. What counts is whether now
/// falls in the window between the two timestamps, which Slack moves forward as
/// the days pass.
pub fn in_quiet_hours(now: i64, dnd: &crate::api::client::DndInfo) -> bool {
    if dnd.snooze_enabled {
        // A manual pause: it holds regardless of the recurring schedule.
        return true;
    }
    if !dnd.dnd_enabled {
        return false;
    }
    // A malformed window (end before start, or zeroed) must not make
    // everyone look quiet.
    if dnd.next_dnd_start_ts <= 0 || dnd.next_dnd_end_ts <= dnd.next_dnd_start_ts {
        return false;
    }
    now >= dnd.next_dnd_start_ts && now < dnd.next_dnd_end_ts
}

/// How many messages to keep on screen per channel.
const WINDOW: u32 = 50;

pub const CHANNEL_ROWS: usize = 18;

/// Bounds for the GUI interface scale (`AppState::ui_scale`). The floor keeps
/// the window usable; the ceiling is generous for a very high-DPI screen.
pub const UI_SCALE_MIN: f32 = 0.8;
pub const UI_SCALE_MAX: f32 = 2.0;

/// Bounds and default for the sidebar (channel list) width
/// (`AppState::sidebar_width`). The floor keeps the list readable; the ceiling
/// stops it eating the conversation.
pub const SIDEBAR_WIDTH_MIN: f32 = 180.0;
pub const SIDEBAR_WIDTH_MAX: f32 = 420.0;
pub const SIDEBAR_WIDTH_DEFAULT: f32 = 230.0;

/// How many messages are shown when a conversation is opened.
/// Finds the conversation in the sidebar, or appends it, and returns its index.
/// Appending at the end keeps the folder indexes valid.
fn ensure_channel(
    s: &mut AppState,
    channel_id: String,
    name: String,
    is_dm: bool,
    peer: Option<String>,
) -> usize {
    if let Some(i) = s.channels.iter().position(|c| c.id == channel_id) {
        return i;
    }
    let label = if name.is_empty() {
        channel_id.clone()
    } else {
        name
    };
    Arc::make_mut(&mut s.channels).push(ChannelEntry {
        id: channel_id,
        name: label,
        is_dm,
        unread: false,
        mentions: 0,
        user: peer,
        last_read: String::new(),
        external: false,
    });
    s.channels.len() - 1
}

/// Extracts the conversation and the message timestamp from a Slack message
/// permalink, so a click can open the message here instead of in the browser.
///
/// The shape is `https://<host>/archives/<channel>/p<digits>`: the timestamp
/// is the digits with a dot before the last six (Slack drops the dot when it
/// builds the path). `None` for everything else — external links still go to
/// the browser.
pub fn parse_message_permalink(url: &str) -> Option<(String, String)> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let (host, path) = rest.split_once('/')?;
    if host != "slack.com" && !host.ends_with(".slack.com") {
        return None;
    }
    let path = path.split(|c| c == '?' || c == '#').next().unwrap_or(path);
    let mut seg = path.split('/');
    if seg.next()? != "archives" {
        return None;
    }
    let channel = seg.next().unwrap_or("");
    let digits = seg.next()?.strip_prefix('p')?;
    if channel.is_empty() || digits.len() < 7 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let split = digits.len() - 6;
    Some((
        channel.to_string(),
        format!("{}.{}", &digits[..split], &digits[split..]),
    ))
}

#[cfg(test)]
mod permalink_tests {
    use super::parse_message_permalink;

    #[test]
    fn a_message_permalink_gives_channel_and_ts() {
        assert_eq!(
            parse_message_permalink("https://acme.slack.com/archives/C012AB3/p1700000000123456"),
            Some(("C012AB3".into(), "1700000000.123456".into()))
        );
    }

    #[test]
    fn the_thread_and_cid_query_are_ignored() {
        assert_eq!(
            parse_message_permalink(
                "https://acme.slack.com/archives/D99/p1700000000123456?thread_ts=1700000000.000000&cid=D99"
            ),
            Some(("D99".into(), "1700000000.123456".into()))
        );
    }

    #[test]
    fn external_and_malformed_links_are_not_permalinks() {
        assert_eq!(
            parse_message_permalink("https://example.com/archives/C1/p1700000000123456"),
            None
        );
        assert_eq!(
            parse_message_permalink("https://acme.slack.com/archives/C1/pabc"),
            None
        );
        assert_eq!(
            parse_message_permalink("https://acme.slack.com/archives/C1/short"),
            None
        );
        assert_eq!(
            parse_message_permalink("https://notslack.com.evil/archives/C1/p1700000000123456"),
            None
        );
    }
}

/// Whether `channel_id` is the conversation currently open. Loading another
/// one must not overwrite what is on screen (a belated load of a channel the
/// user already left otherwise clobbered the open conversation).
fn selected_is(s: &AppState, channel_id: &str) -> bool {
    s.channels.get(s.selected_channel).map(|c| c.id.as_str()) == Some(channel_id)
}

pub const INITIAL_WINDOW: u32 = 50;

/// Maximum number of aggregate-budgeted mod dispatch batches per `run_mods`
/// call. Each batch is CPU-bounded by the host, and this caps the total, so a
/// very large window cannot make one call run unbounded work.
#[cfg(feature = "mods")]
const MAX_MOD_BATCHES: u32 = 8;

/// Follow-up dispatch budget per channel within [`MODS_FOLLOWUP_WINDOW`], so a
/// huge window cannot keep a blocking worker busy without bound.
#[cfg(feature = "mods")]
const MAX_MOD_FOLLOWUPS: u32 = 16;
#[cfg(feature = "mods")]
const MODS_FOLLOWUP_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

impl App {
    pub fn new(creds: Credentials) -> Self {
        Self::with_channel(creds, None)
    }

    pub fn with_channel(creds: Credentials, initial_channel: Option<String>) -> Self {
        Self::build(creds, initial_channel, true)
    }

    /// Builds an `App` even when no credentials are stored.
    ///
    /// `Some` behaves exactly like [`App::with_channel`]. `None` opens an
    /// unauthenticated app with empty credentials, so the GUI can start and
    /// show its login screen instead of the process exiting at once.
    pub fn from_optional_creds(
        creds: Option<Credentials>,
        initial_channel: Option<String>,
    ) -> Self {
        match creds {
            Some(creds) => Self::build(creds, initial_channel, true),
            None => Self::build(
                Credentials {
                    xoxc: String::new(),
                    xoxd: String::new(),
                    workspace_url: String::new(),
                    user_id: String::new(),
                    team_id: String::new(),
                },
                initial_channel,
                false,
            ),
        }
    }

    /// Whether the session is usable. False means the login screen.
    pub fn is_authenticated(&self) -> bool {
        self.state
            .try_read()
            .map(|s| s.authenticated)
            .unwrap_or(true)
    }

    fn build(creds: Credentials, initial_channel: Option<String>, authenticated: bool) -> Self {
        let mut app = Self::build_with_store(
            creds,
            initial_channel,
            authenticated,
            crate::store::Store::open_or_memory(),
        );
        // Only the real app loads installed mods: the demo constructor (used by
        // `gui --demo` and the snapshot tests) must stay isolated from whatever
        // the machine has installed.
        #[cfg(feature = "mods")]
        Self::spawn_load_installed_mods(app.state.clone());
        app
    }

    /// Loads and compiles the user's installed mods **off the startup path** and
    /// publishes them, then decorates whatever is already on screen. Never for
    /// the demo. Compiling untrusted components can be slow, so it must not block
    /// the first frame (or a launch could appear to hang for minutes).
    ///
    /// Before loading, it asks for consent to any mod that has never been decided
    /// yet; the host is loaded once the dialog is answered (Apply *or* Dismiss).
    #[cfg(feature = "mods")]
    fn spawn_load_installed_mods(state: Arc<RwLock<AppState>>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return; // no runtime (e.g. a sync context): skip rather than block.
        }
        tokio::spawn(async move {
            // A cheap manifest-only scan decides whether to ask before loading.
            let pending = tokio::task::spawn_blocking(|| -> Option<ModConsentDialog> {
                let root = crate::mods::default_dir().ok()?;
                let consent = crate::mods::consent::Consent::load_default();
                let (dialog, undecided) = Self::build_mods_consent(&root, &consent, true);
                undecided.then_some(dialog)
            })
            .await
            .ok()
            .flatten();
            if let Some(dialog) = pending {
                let mut s = state.write().await;
                // A manual decision may have raced this scan: only show the
                // startup prompt if the user has not decided this session, the
                // host is still unloaded, nothing is busy, and no dialog is up.
                if !s.mods_decided_session
                    && !s.mods_consent_busy
                    && s.mods.is_none()
                    && s.mods_consent.is_none()
                {
                    s.mods_consent = Some(dialog);
                    wake(&s);
                }
                return; // the dialog's Apply/Dismiss loads the mods
            }
            Self::load_mods_and_publish(state).await;
        });
    }

    /// Builds the consent dialog from the installed packages. `only_undecided`
    /// shows just the mods the user has never been asked about (the startup
    /// prompt); `false` shows every mod that asks for something (review/revoke).
    /// Returns the dialog and whether any mod is still undecided.
    #[cfg(feature = "mods")]
    fn build_mods_consent(
        root: &std::path::Path,
        consent: &crate::mods::consent::Consent,
        only_undecided: bool,
    ) -> (ModConsentDialog, bool) {
        let mut entries = Vec::new();
        let mut undecided = false;
        for m in crate::mods::scan(root, consent) {
            if m.requested.is_empty() {
                continue; // nothing to consent to
            }
            let decided = consent.is_decided(&m.id);
            // Re-prompt when the manifest asks for egress hosts the user has not
            // been shown (an update must not silently extend reach); a plain
            // rejection is remembered and does not re-prompt.
            let reviewed = consent.entry(&m.id).egress_reviewed().to_vec();
            let egress_covered = m.egress.iter().all(|h| reviewed.contains(h));
            if !decided || !egress_covered {
                undecided = true;
            }
            if only_undecided && decided && egress_covered {
                continue;
            }
            entries.push(ModConsentEntry {
                id: crate::mods::sanitize(&m.id),
                name: crate::mods::sanitize(&m.name),
                caps: m
                    .requested
                    .iter()
                    .map(|c| {
                        let granted = m.granted.contains(c);
                        ModConsentCap {
                            cap: *c,
                            selected: granted,
                            initial: granted,
                        }
                    })
                    .collect(),
                access: {
                    // A manifest host the user has not reviewed yet defaults to
                    // allowed, as a fresh grant does; so does every host of a
                    // list never recorded at all (not stored, nothing allowed).
                    // A host the user reviewed and unticked stays unticked.
                    let fresh = m.egress_allowed.is_empty()
                        && !m.stored.contains(&crate::mods::capability::Capability::NetEgress);
                    let egress_allowed: Vec<String> = m
                        .egress
                        .iter()
                        .filter(|h| {
                            fresh || m.egress_allowed.contains(h) || !m.egress_reviewed.contains(h)
                        })
                        .cloned()
                        .collect();
                    // Pending only for a live grant (unreviewed hosts to record
                    // as allowed); otherwise the list seeds a future grant.
                    let granted =
                        m.granted.contains(&crate::mods::capability::Capability::NetEgress);
                    let as_set =
                        |v: &[String]| v.iter().cloned().collect::<std::collections::BTreeSet<_>>();
                    ModConsentAccess {
                        egress_dirty: granted && as_set(&egress_allowed) != as_set(&m.egress_allowed),
                        egress_allowed_initial: m.egress_allowed.clone(),
                        egress_added_initial: m.egress_added.clone(),
                        egress_allowed,
                        egress_added: m.egress_added.clone(),
                        user_scope: m.user_scope.clone(),
                        scope_dirty: false,
                        error: None,
                    }
                },
                read_scope: m.read_scope,
                effective_scope: m.effective_scope,
                egress: m.egress,
                fs_slots: m
                    .fs_slots
                    .iter()
                    .map(|s| {
                        let mapped = consent
                            .entry(&m.id)
                            .fs_slots()
                            .get(&s.name)
                            .map(|mm| mm.dir.clone());
                        ModConsentSlot {
                            name: s.name.clone(),
                            access: s.access,
                            mapped,
                        }
                    })
                    .collect(),
            });
        }
        (
            ModConsentDialog {
                entries,
                // The startup prompt leaves the host unloaded until answered.
                pending: only_undecided,
                cursor: 0,
            },
            undecided,
        )
    }

    /// The `Msg` a mod sees for a message: text, thread info and file metadata
    /// (bounded; the bytes come from `file-get`). A file is listed only when
    /// `file-get` can serve it, and the **user's own** messages list none (they are
    /// not delivered, and their files are not in the download map).
    #[cfg(feature = "mods")]
    fn mod_msg(m: &MessageEntry, me: &str) -> crate::mods::Msg {
        let files = if m.user == me {
            Vec::new()
        } else {
            m.files
                .iter()
                .filter(|f| !f.id.is_empty() && crate::net::is_slack_host(&f.download_url))
                .take(crate::mods::MAX_FILES_PER_MESSAGE)
                .map(|f| crate::mods::ModFile {
                    id: f.id.clone(),
                    name: f.name.clone(),
                    mime: f.mimetype.clone(),
                })
                .collect()
        };
        crate::mods::Msg {
            ts: m.ts.clone(),
            author: m.user.clone(),
            text: m.text.clone(),
            thread_ts: m.thread_ts.clone(),
            reply_count: m.reply_count.unwrap_or(0).max(0) as u32,
            files,
        }
    }

    /// The files a mod may learn about and download (`files.download`), as
    /// `id -> FileRef`, from the messages on screen: bounded, and only
    /// Slack-hosted URLs (the host re-checks scope and host before serving one).
    #[cfg(feature = "mods")]
    fn mod_files(s: &AppState) -> std::collections::HashMap<String, crate::mods::FileRef> {
        let mut files = std::collections::HashMap::new();
        let windows = [
            (s.messages_channel.as_deref(), &s.messages),
            (s.thread_channel_id.as_deref(), &s.thread_messages),
        ];
        for (channel, msgs) in windows {
            let Some(channel) = channel else { continue };
            for m in msgs.iter() {
                // The user's own messages are not delivered to mods, so their
                // files must not be in the map either (defense in depth: a guessed
                // id must not resolve).
                if m.user == s.my_user_id {
                    continue;
                }
                for f in &m.files {
                    if files.len() >= crate::mods::MAX_DIRECTORY_ENTRIES {
                        return files;
                    }
                    if !f.id.is_empty() && crate::net::is_slack_host(&f.download_url) {
                        files.insert(
                            f.id.clone(),
                            crate::mods::FileRef {
                                channel: channel.to_string(),
                                ts: m.ts.clone(),
                                url: f.download_url.clone(),
                                name: f.name.clone(),
                                mimetype: f.mimetype.clone(),
                                size: f.size.max(0) as u64,
                            },
                        );
                    }
                }
            }
        }
        files
    }

    /// Loads the installed mods and publishes the new host, coalescing overlaps:
    /// a load requested while one is running (a double-clicked Reload, Reload
    /// racing a consent Apply) runs exactly once more afterwards, instead of two
    /// hosts being built at once on the same mod files.
    #[cfg(feature = "mods")]
    async fn load_mods_and_publish(state: Arc<RwLock<AppState>>) {
        {
            let mut s = state.write().await;
            if s.mods_loading {
                s.mods_reload_again = true;
                return;
            }
            s.mods_loading = true;
        }
        loop {
            Self::load_mods_and_publish_once(state.clone()).await;
            let mut s = state.write().await;
            if std::mem::take(&mut s.mods_reload_again) {
                continue;
            }
            s.mods_loading = false;
            break;
        }
    }

    /// The host load + publish + dispatch, shared by the startup path, the
    /// consent Apply/Dismiss paths and Reload (always via the coalescing
    /// [`Self::load_mods_and_publish`]).
    #[cfg(feature = "mods")]
    async fn load_mods_and_publish_once(state: Arc<RwLock<AppState>>) {
        // The directory is available to the mods from `start`, so build it once
        // here from the current cache (bounded), before the host is created.
        let directory = {
            let s = state.read().await;
            let mut d = crate::mods::Directory {
                channels: s
                    .channels
                    .iter()
                    .take(crate::mods::MAX_DIRECTORY_ENTRIES)
                    .map(|c| (c.id.clone(), c.name.clone()))
                    .collect(),
                users: s
                    .user_names
                    .iter()
                    .take(crate::mods::MAX_DIRECTORY_ENTRIES)
                    .map(|(id, name)| (id.clone(), name.clone()))
                    .collect(),
                files: Self::mod_files(&s),
            };
            d.channels.shrink_to_fit();
            d.users.shrink_to_fit();
            Arc::new(d)
        };
        let loaded = tokio::task::spawn_blocking(move || {
            crate::mods::default_dir()
                .and_then(|dir| crate::mods::Host::from_dir_with(&dir, &directory))
        })
        .await;
        let mut host = match loaded {
            Ok(Ok(host)) => host,
            Ok(Err(e)) => {
                tracing::warn!("mods not loaded: {e}");
                return;
            }
            Err(e) => {
                tracing::warn!("mod load task failed: {e}");
                return;
            }
        };
        if host.is_empty() {
            // No loadable mods: drop any previously published host (and its
            // decorations), so a reload that removes or corrupts packages cannot
            // leave old instances running with their prior grants.
            let mut s = state.write().await;
            s.mods = None;
            s.mods_queue = None;
            // The settings form holds handles to the old store: close it, so an
            // edit cannot persist to a gone mod's settings.
            s.mods_settings = None;
            s.mod_actions = Arc::new(Vec::new());
            s.mod_context_actions = Arc::new(Vec::new());
            s.mod_action_menu = None;
            s.mod_views = Arc::new(Vec::new());
            s.mod_view_menu = None;
            s.mod_view_panel = None;
            s.mod_dialog = None;
            s.mod_commands = Arc::new(Vec::new());
            s.mod_composer_actions = Arc::new(Vec::new());
            s.mod_theme = Arc::new(Vec::new());
            s.composer_prefill = None;
            s.mods_secrets.clear();
            Arc::make_mut(&mut s.decorations).clear();
            wake(&s);
            return;
        }
        tracing::info!("mods: {} loaded", host.len());
        // A worker task drains follow-up dispatch requests (channels with more
        // work than one call covers), so `run_mods` never has to recurse.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let worker_state = state.clone();
        tokio::spawn(async move {
            while let Some(ch) = rx.recv().await {
                {
                    let mut s = worker_state.write().await;
                    s.mods_queued.remove(&ch);
                }
                Self::run_mods(&worker_state, &ch).await;
            }
        });
        // Publish the host and dispatch the on-screen timeline *and* the open
        // thread (which may belong to a different channel), deduplicated.
        let mod_actions: Vec<ModAction> = host
            .message_actions()
            .into_iter()
            .map(|a| ModAction {
                mod_id: a.mod_id,
                action_id: a.action_id,
                mod_name: crate::mods::sanitize(&a.mod_name),
                label: crate::mods::sanitize(&a.label),
                icon: a.icon.map(|ic| crate::mods::sanitize(&ic)),
            })
            .collect();
        let mod_context_actions: Vec<ModAction> = host
            .context_actions()
            .into_iter()
            .map(|a| ModAction {
                mod_id: a.mod_id,
                action_id: a.action_id,
                mod_name: crate::mods::sanitize(&a.mod_name),
                label: crate::mods::sanitize(&a.label),
                icon: a.icon.map(|ic| crate::mods::sanitize(&ic)),
            })
            .collect();
        let mod_views: Vec<ModView> = host
            .views()
            .into_iter()
            .map(|v| ModView {
                mod_id: v.mod_id,
                view_id: v.view_id,
                mod_name: crate::mods::sanitize(&v.mod_name),
                label: crate::mods::sanitize(&v.label),
                icon: v.icon.map(|ic| crate::mods::sanitize(&ic)),
            })
            .collect();
        let mod_commands: Vec<ModCommand> = host
            .commands()
            .into_iter()
            .map(|c| ModCommand {
                mod_id: c.mod_id,
                mod_name: crate::mods::sanitize(&c.mod_name),
                name: crate::mods::sanitize(&c.name),
                label: crate::mods::sanitize(&c.label),
                description: crate::mods::sanitize(&c.description),
            })
            .collect();
        let mod_composer_actions: Vec<ModComposerAction> = host
            .composer_actions()
            .into_iter()
            .map(|a| ModComposerAction {
                mod_id: a.mod_id,
                mod_name: crate::mods::sanitize(&a.mod_name),
                action_id: a.action_id,
                label: crate::mods::sanitize(&a.label),
                icon: a.icon.map(|ic| crate::mods::sanitize(&ic)),
            })
            .collect();
        let mod_theme: Vec<(String, String)> = host.theme();
        // Give the host the API client the `Write` capabilities need.
        if let Some(api) = state.read().await.api.clone() {
            host.set_api(api);
        }
        let mods = Arc::new(host);
        let stale = mods.stale_secrets();
        let (channels, selected): (Vec<String>, Option<String>) = {
            let mut s = state.write().await;
            s.mods = Some(mods.clone());
            s.mods_secrets = stale
                .into_iter()
                .filter(|x| !s.mods_secrets_later.contains(&(x.id.clone(), x.package.clone())))
                .collect();
            s.mods_queue = Some(tx);
            // A (re)load invalidates any open settings form's handles.
            s.mods_settings = None;
            s.mod_actions = Arc::new(mod_actions);
            s.mod_context_actions = Arc::new(mod_context_actions);
            s.mod_action_menu = None;
            s.mod_views = Arc::new(mod_views);
            s.mod_view_menu = None;
            s.mod_view_panel = None;
            s.mod_dialog = None;
            s.mod_commands = Arc::new(mod_commands);
            s.mod_composer_actions = Arc::new(mod_composer_actions);
            s.mod_theme = Arc::new(mod_theme);
            // A reload invalidates any prefill produced by the old host.
            s.composer_prefill = None;
            let mut set = std::collections::HashSet::new();
            if let Some(c) = &s.messages_channel {
                set.insert(c.clone());
            }
            if let Some(c) = &s.thread_channel_id {
                set.insert(c.clone());
            }
            let selected = s.channels.get(s.selected_channel).map(|c| c.id.clone());
            (set.into_iter().collect(), selected)
        };
        // A mod may have shown a dialog in `start` (the only guest entry point
        // with no later drain): surface it now.
        Self::apply_mod_dialogs(&state, &mods).await;
        // A mod that loaded after the conversation was already on screen still
        // learns which one is open.
        if let Some(id) = selected {
            Self::mod_channel_event(state.clone(), crate::mods::events::Event::ChannelOpened, id);
        }
        for ch in channels {
            Self::run_mods(&state, &ch).await;
        }
    }

    /// Reloads the installed mods (from the "Running mods" window): a fresh host
    /// replaces the old one, which restarts stopped mods and picks up rebuilt
    /// packages. No-op without a runtime.
    #[cfg(feature = "mods")]
    pub fn reload_mods(state: Arc<RwLock<AppState>>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(Self::load_mods_and_publish(state));
    }

    /// "Keep" in the Mod updated prompt: the secrets `id` had for an earlier
    /// package are kept for the loaded one, and the mods reload to pick them up.
    #[cfg(feature = "mods")]
    pub fn keep_mod_secrets(state: Arc<RwLock<AppState>>, id: String) {
        Self::decide_mod_secrets(state, id, true);
    }

    /// "Forget" in the Mod updated prompt: the secrets `id` had for an earlier
    /// package are deleted (keyring and binding).
    #[cfg(feature = "mods")]
    pub fn forget_mod_secrets(state: Arc<RwLock<AppState>>, id: String) {
        Self::decide_mod_secrets(state, id, false);
    }

    #[cfg(feature = "mods")]
    fn decide_mod_secrets(state: Arc<RwLock<AppState>>, id: String, keep: bool) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            // Taken out of the prompt first, so a double click acts once. The
            // package is the one shown: if it changed again since, the next
            // load finds the secrets stale again and asks again.
            let entry = {
                let mut s = state.write().await;
                let Some(i) = s.mods_secrets.iter().position(|x| x.id == id) else {
                    return;
                };
                let e = s.mods_secrets.remove(i);
                wake(&s);
                e
            };
            let applied = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                if keep {
                    crate::mods::settings::rebind_secrets(&entry.id, &entry.keys, &entry.package)?;
                } else {
                    crate::mods::settings::forget_secrets(&entry.id, &entry.keys)?;
                }
                Ok(())
            })
            .await;
            match applied {
                Ok(Ok(())) if keep => Self::load_mods_and_publish(state).await,
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    "mod secrets not {}: {}",
                    if keep { "kept" } else { "forgotten" },
                    crate::mods::sanitize_log(&format!("{e:#}"))
                ),
                Err(e) => tracing::warn!("mod secrets task failed: {e}"),
            }
        });
    }

    /// "Not now" in the Mod updated prompt: asked again next session (or for a
    /// newer package); the mods keep running without the secrets.
    #[cfg(feature = "mods")]
    pub fn later_mod_secrets(state: &Arc<RwLock<AppState>>) {
        let apply = |s: &mut AppState| {
            let later: Vec<(String, String)> = s
                .mods_secrets
                .drain(..)
                .map(|x| (x.id, x.package))
                .collect();
            s.mods_secrets_later.extend(later);
            wake(s);
        };
        if let Ok(mut s) = state.try_write() {
            apply(&mut s);
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let state = state.clone();
        tokio::spawn(async move { apply(&mut *state.write().await) });
    }

    /// Opens the consent dialog for review/revoke, showing every mod that asks
    /// for something. No-op when none is installed.
    #[cfg(feature = "mods")]
    pub fn open_mods_consent(state: Arc<RwLock<AppState>>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let dialog = tokio::task::spawn_blocking(|| -> Option<ModConsentDialog> {
                let root = crate::mods::default_dir().ok()?;
                let consent = crate::mods::consent::Consent::load_default();
                let (dialog, _) = Self::build_mods_consent(&root, &consent, false);
                (!dialog.entries.is_empty()).then_some(dialog)
            })
            .await
            .ok()
            .flatten();
            if let Some(dialog) = dialog {
                let mut s = state.write().await;
                // Never clobber a dialog already on screen or an in-flight
                // Apply/Dismiss: the first one owns the load decision.
                if !s.mods_consent_busy && s.mods_consent.is_none() {
                    s.mods_consent = Some(dialog);
                    wake(&s);
                }
            }
        });
    }

    /// Toggles one capability (by mod id + capability) in the open dialog.
    /// Applied in-frame when the lock is free (so it stays ordered with a
    /// same-frame Apply); otherwise retried on the runtime so the click is not
    /// lost. Keyed by identity, so a stale frame cannot toggle the wrong row.
    #[cfg(feature = "mods")]
    pub fn toggle_mod_consent_cap(
        state: &Arc<RwLock<AppState>>,
        id: String,
        cap: crate::mods::capability::Capability,
    ) {
        if let Ok(mut s) = state.try_write() {
            toggle_consent_in(&mut s, &id, cap);
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let state = state.clone();
        tokio::spawn(async move {
            let mut s = state.write().await;
            toggle_consent_in(&mut s, &id, cap);
        });
    }

    /// Edits the channels or hosts of one mod in the open consent dialog (saved
    /// on Apply). Like `toggle_mod_consent_cap`: called from the render thread.
    #[cfg(feature = "mods")]
    pub fn edit_mod_consent_access(state: &Arc<RwLock<AppState>>, id: String, edit: AccessEdit) {
        if let Ok(mut s) = state.try_write() {
            edit_access_in(&mut s, &id, edit);
            wake(&s);
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let state = state.clone();
        tokio::spawn(async move {
            let mut s = state.write().await;
            edit_access_in(&mut s, &id, edit);
            wake(&s);
        });
    }

    /// Maps one `filesystem` slot of an open consent dialog to a directory the
    /// user picked, persisting it immediately. A mapping is independent of the
    /// `filesystem` checkbox — the runtime intersects them — so a slot may be
    /// mapped before or after the capability is granted. The path is
    /// canonicalized and refused when it does not exist, is not a directory, or
    /// is inside one of slaq's reserved dirs (a mapping the sandbox would only
    /// silently skip must never be stored). Called from the render thread, so the
    /// blocking store work runs on the runtime.
    #[cfg(feature = "mods")]
    pub fn map_mod_consent_slot(
        state: &Arc<RwLock<AppState>>,
        id: String,
        slot: String,
        dir: std::path::PathBuf,
        access: crate::mods::fs::FsAccess,
    ) {
        // Canonicalize and validate before touching the store: the picker can
        // hand back any path.
        let canon = match dir.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "filesystem slot mapping refused: {}",
                    crate::mods::sanitize_log(&format!("{e:#}"))
                );
                return;
            }
        };
        if !canon.is_dir() {
            tracing::warn!(
                "filesystem slot mapping refused: {} is not a directory",
                crate::mods::sanitize_log(&canon.display().to_string())
            );
            return;
        }
        if crate::mods::fs::is_reserved_dir(&canon) {
            tracing::warn!(
                "filesystem slot mapping refused: {} is a reserved directory",
                crate::mods::sanitize_log(&canon.display().to_string())
            );
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return; // no runtime: cannot persist off the render thread
        }
        let state = state.clone();
        tokio::spawn(async move {
            let stored = canon.to_string_lossy().into_owned();
            let store_id = id.clone();
            let store_slot = slot.clone();
            let applied = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let path = crate::mods::consent::default_path()?;
                // Serialize with the `slaq mods` CLI (same lock).
                let _lock = crate::mods::consent::lock(&path)?;
                let mut consent = crate::mods::consent::Consent::load(&path)?;
                consent.map_slot(&store_id, &store_slot, canon, access)?;
                consent.save(&path)?;
                Ok(())
            })
            .await;
            match applied {
                // Only reflect the mapping once it is on disk: on a failure the
                // store is unchanged and the dialog must keep showing "unmapped".
                Ok(Ok(())) => {
                    let mut s = state.write().await;
                    set_consent_slot_mapped(&mut s, &id, &slot, Some(stored));
                    wake(&s);
                }
                Ok(Err(e)) => tracing::warn!(
                    "filesystem slot mapping not saved: {}",
                    crate::mods::sanitize_log(&format!("{e:#}"))
                ),
                Err(e) => tracing::warn!("filesystem slot mapping task failed: {e}"),
            }
        });
    }

    /// Removes the mapping of one `filesystem` slot of an open consent dialog,
    /// persisting immediately (see [`App::map_mod_consent_slot`]).
    #[cfg(feature = "mods")]
    pub fn unmap_mod_consent_slot(state: &Arc<RwLock<AppState>>, id: String, slot: String) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let state = state.clone();
        tokio::spawn(async move {
            let store_id = id.clone();
            let store_slot = slot.clone();
            let applied = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let path = crate::mods::consent::default_path()?;
                // Serialize with the `slaq mods` CLI (same lock).
                let _lock = crate::mods::consent::lock(&path)?;
                let mut consent = crate::mods::consent::Consent::load(&path)?;
                consent.unmap_slot(&store_id, &store_slot);
                consent.save(&path)?;
                Ok(())
            })
            .await;
            match applied {
                Ok(Ok(())) => {
                    let mut s = state.write().await;
                    set_consent_slot_mapped(&mut s, &id, &slot, None);
                    wake(&s);
                }
                Ok(Err(e)) => tracing::warn!(
                    "filesystem slot unmap not saved: {}",
                    crate::mods::sanitize_log(&format!("{e:#}"))
                ),
                Err(e) => tracing::warn!("filesystem slot unmap task failed: {e}"),
            }
        });
    }

    /// Applies the consent dialog: persists the selection and reloads the mods.
    #[cfg(feature = "mods")]
    pub fn apply_mods_consent(state: Arc<RwLock<AppState>>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let dialog = {
                let mut s = state.write().await;
                if s.mods_consent_busy {
                    return; // another decision is already in flight
                }
                match s.mods_consent.take() {
                    Some(d) => {
                        s.mods_consent_busy = true;
                        s.mods_decided_session = true;
                        wake(&s);
                        Some(d)
                    }
                    None => None,
                }
            };
            let Some(dialog) = dialog else {
                return;
            };
            let _busy = ModsBusyGuard(state.clone());
            let applied = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let path = crate::mods::consent::default_path()?;
                // Serialize with the `slaq mods` CLI (same lock).
                let _lock = crate::mods::consent::lock(&path)?;
                let mut consent = crate::mods::consent::Consent::load(&path)?;
                apply_consent_entries(&mut consent, &dialog.entries)?;
                consent.save(&path)?;
                Ok(())
            })
            .await;
            match applied {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    "mod consent not saved: {}",
                    crate::mods::sanitize_log(&format!("{e:#}"))
                ),
                Err(e) => tracing::warn!("mod consent task failed: {e}"),
            }
            // Whatever happened, (re)load: on a save failure the store is
            // unchanged, so the mods stay default-denied. The busy flag is cleared
            // by the guard, even on a panic.
            Self::load_mods_and_publish(state.clone()).await;
        });
    }

    /// Dismisses the consent dialog. On the startup prompt the mods still have to
    /// be loaded (with the current grants; undecided ones stay default-denied and
    /// the prompt returns next time). On the review dialog nothing changes.
    #[cfg(feature = "mods")]
    pub fn dismiss_mods_consent(state: Arc<RwLock<AppState>>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            // Dismiss = "not now": never decides anything, so default deny holds.
            // Load the host if it was never loaded (the startup prompt blocked
            // it); if it is already loaded, dismissing the review dialog is a
            // no-op. Inferring from the host, not from a dialog flag, keeps this
            // correct even if the startup scan and a manual open race.
            let needs_load = {
                let mut s = state.write().await;
                if s.mods_consent_busy {
                    return; // another decision is already in flight
                }
                s.mods_consent_busy = true;
                s.mods_decided_session = true;
                s.mods_consent.take();
                wake(&s);
                s.mods.is_none()
            };
            let _busy = ModsBusyGuard(state.clone());
            if needs_load {
                Self::load_mods_and_publish(state.clone()).await;
            }
        });
    }

    /// Opens the settings form for the loaded mods that declare a schema. No-op
    /// when the host is not loaded or no mod declares settings.
    #[cfg(feature = "mods")]
    pub fn open_mods_settings(state: Arc<RwLock<AppState>>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let Some(mods) = ({ state.read().await.mods.clone() }) else {
                return;
            };
            let handles = {
                let guard = &mods;
                guard.settings_handles()
            };
            if handles.is_empty() {
                return;
            }
            let panes = handles
                .into_iter()
                .map(ModSettingsPane::from_handle)
                .collect();
            let mut s = state.write().await;
            // The host may have been (re)loaded since the handles were read; a
            // reload clears the form, so only install against the same host.
            let same_host = s
                .mods
                .as_ref()
                .map(|m| std::sync::Arc::ptr_eq(m, &mods))
                .unwrap_or(false);
            if same_host && s.mods_settings.is_none() {
                s.mods_settings = Some(ModSettingsDialog {
                    panes,
                    ..Default::default()
                });
                wake(&s);
            }
        });
    }

    /// Closes the settings form without saving.
    #[cfg(feature = "mods")]
    pub fn dismiss_mods_settings(state: Arc<RwLock<AppState>>) {
        if let Ok(mut s) = state.try_write() {
            s.mods_settings = None;
            wake(&s);
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let mut s = state.write().await;
            s.mods_settings = None;
            wake(&s);
        });
    }

    /// Saves every field of the form and closes it on success; on failure the
    /// form stays open with the error shown.
    #[cfg(feature = "mods")]
    pub fn apply_mods_settings(state: Arc<RwLock<AppState>>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let (panes, save_gen, mods) = {
                let mut s = state.write().await;
                let Some(mods) = s.mods.clone() else {
                    return;
                };
                match s.mods_settings.as_mut() {
                    Some(d) if !d.busy => {
                        d.busy = true;
                        d.save_gen += 1;
                        (d.panes.clone(), d.save_gen, mods)
                    }
                    // Already saving, or no form.
                    _ => return,
                }
            };
            // Validate + build the per-mod write batches (no persistence yet).
            let batches = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<_>> {
                let mut batches = Vec::new();
                for pane in &panes {
                    let mut items: Vec<(String, crate::mods::settings::Value)> = Vec::new();
                    for f in &pane.fields {
                        if !f.dirty {
                            continue;
                        }
                        let value = if f.clear {
                            crate::mods::settings::Value::Text(String::new())
                        } else {
                            field_value(f)
                                .map_err(|e| anyhow::anyhow!("{}/{}: {e:#}", pane.id, f.key))?
                        };
                        // Schema-validate every field (no write) before writing
                        // any, so a bad value cannot half-apply the form.
                        pane.handle
                            .settings
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .check(&f.key, &value)
                            .map_err(|e| anyhow::anyhow!("{}/{}: {e:#}", pane.id, f.key))?;
                        items.push((f.key.clone(), value));
                    }
                    if !items.is_empty() {
                        batches.push((pane.handle.settings.clone(), pane.id.clone(), items));
                    }
                }
                Ok(batches)
            })
            .await;
            // Unwrap validation: a bad value (or a panic) leaves the form open.
            let batches = match batches {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    let mut s = state.write().await;
                    if matches!(&s.mods_settings, Some(d) if d.save_gen == save_gen) {
                        if let Some(d) = s.mods_settings.as_mut() {
                            d.busy = false;
                            d.error = Some(crate::mods::sanitize_log(&format!("{e:#}")));
                        }
                    }
                    wake(&s);
                    return;
                }
                Err(e) => {
                    let mut s = state.write().await;
                    if let Some(d) = s.mods_settings.as_mut() {
                        d.busy = false;
                    }
                    wake(&s);
                    tracing::warn!("mod settings validate task failed: {e}");
                    return;
                }
            };
            // The host can be reloaded while we validate: never write to a gone
            // host's handles (a secret would be rebound to the old package).
            let host_ok = {
                let s = state.read().await;
                s.mods
                    .as_ref()
                    .map(|m| std::sync::Arc::ptr_eq(m, &mods))
                    .unwrap_or(false)
            };
            if !host_ok {
                let mut s = state.write().await;
                if let Some(d) = s.mods_settings.as_mut() {
                    d.busy = false;
                }
                wake(&s);
                return;
            }
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                for (settings, id, items) in batches {
                    settings
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .set_from_user_batch(&items)
                        .map_err(|e| anyhow::anyhow!("{id}: {e:#}"))?;
                }
                Ok(())
            })
            .await;
            let mut s = state.write().await;
            // Close only the form this save began (a newer save or a reload has a
            // different generation / no form).
            if matches!(&s.mods_settings, Some(d) if d.save_gen == save_gen) {
                match result {
                    Ok(Ok(())) => s.mods_settings = None,
                    Ok(Err(e)) => {
                        if let Some(d) = s.mods_settings.as_mut() {
                            d.busy = false;
                            d.error = Some(crate::mods::sanitize_log(&format!("{e:#}")));
                        }
                    }
                    Err(e) => {
                        if let Some(d) = s.mods_settings.as_mut() {
                            d.busy = false;
                        }
                        tracing::warn!("mod settings task failed: {e}");
                    }
                }
            }
            wake(&s);
        });
    }

    /// Invokes a mod's message action: runs the guest with the message current,
    /// then updates that message's decorations. No-op for an unknown mod/action.
    #[cfg(feature = "mods")]
    pub fn invoke_mod_action(
        state: Arc<RwLock<AppState>>,
        mod_id: String,
        action_id: String,
        channel: String,
        ts: String,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            // No global lock: the guest runs on its mod's worker thread and the
            // Host cache is the source of truth. A same-time `run_mods` could, in
            // the rare overlap, publish a snapshot that briefly overwrites this
            // decoration in `AppState`; it self-heals on the next dispatch.
            let (mods, msg) = {
                let s = state.read().await;
                let Some(mods) = s.mods.clone() else {
                    return;
                };
                let msg = s.find_mod_msg(&channel, &ts).unwrap_or_default();
                (mods, msg)
            };
            // The guest call borrows the ids; keep the originals for the update.
            let (ch, ts2) = (channel.clone(), ts.clone());
            let mods_for_guest = mods.clone();
            // Guest code is synchronous and CPU-bound: keep it off the workers.
            let result = tokio::task::spawn_blocking(move || {
                let host = &mods_for_guest;
                host.invoke_action(&mod_id, &action_id, &ch, &ts2, &msg)
            })
            .await;
            // The handler may also have shown or closed a dialog.
            Self::apply_mod_dialogs(&state, &mods).await;
            let Ok(Some(texts)) = result else {
                return;
            };
            let mut s = state.write().await;
            // A reload while the guest ran replaced the host and cleared the
            // decorations: do not resurrect a removed mod's output.
            if !s
                .mods
                .as_ref()
                .map(|m| std::sync::Arc::ptr_eq(m, &mods))
                .unwrap_or(false)
            {
                return;
            }
            let map = Arc::make_mut(&mut s.decorations);
            match map.get_mut(&channel) {
                Some(by_ts) => {
                    if texts.is_empty() {
                        by_ts.remove(&ts);
                    } else {
                        by_ts.insert(ts, texts);
                    }
                    if by_ts.is_empty() {
                        map.remove(&channel);
                    }
                }
                None if !texts.is_empty() => {
                    map.insert(channel, std::collections::HashMap::from([(ts, texts)]));
                }
                None => {}
            }
            wake(&s);
        });
    }

    /// Delivers a channel event (`channel.opened`/`channel.updated`) to the
    /// subscribed mods. No-op without mods or when there is no runtime.
    #[cfg(feature = "mods")]
    pub fn mod_channel_event(
        state: Arc<RwLock<AppState>>,
        event: crate::mods::events::Event,
        channel: String,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let mods = match state.read().await.mods.clone() {
                Some(mods) => mods,
                None => return,
            };
            let mods_for_guest = mods.clone();
            // Guest code is synchronous and CPU-bound: keep it off the workers.
            let delivered = tokio::task::spawn_blocking(move || {
                let host = &mods_for_guest;
                host.channel_event(event, &channel)
            })
            .await
            .unwrap_or(0);
            // A handler may have shown or closed a dialog (even if it then errored).
            Self::apply_mod_dialogs(&state, &mods).await;
            if delivered == 0 {
                return;
            }
            // A handler may have pushed new view content: refresh the open panel.
            let panel = {
                let s = state.read().await;
                s.mod_view_panel
                    .as_ref()
                    .map(|p| (p.mod_id.clone(), p.view_id.clone()))
            };
            if let Some((mod_id, view_id)) = panel {
                Self::refresh_mod_view(state.clone(), mod_id, view_id);
            }
        });
    }

    /// Runs a composer action a mod declared: the guest sees the current draft
    /// and may replace the composer text; the text it set is published for the
    /// front end to take into the composer. No-op without mods/runtime.
    #[cfg(feature = "mods")]
    pub fn invoke_mod_composer_action(
        state: Arc<RwLock<AppState>>,
        mod_id: String,
        action_id: String,
        channel: String,
        draft: String,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let mods = match state.read().await.mods.clone() {
                Some(mods) => mods,
                None => return,
            };
            let mods_for_guest = mods.clone();
            let (mid, aid, ch, dr) = (mod_id, action_id, channel.clone(), draft);
            // Guest code is synchronous and CPU-bound: keep it off the workers.
            let edit = tokio::task::spawn_blocking(move || {
                let host = &mods_for_guest;
                host.invoke_composer_action(&mid, &aid, &ch, &dr)
            })
            .await
            .unwrap_or(None);
            // The handler may have shown or closed a dialog.
            Self::apply_mod_dialogs(&state, &mods).await;
            let Some(text) = edit else {
                return;
            };
            let mut s = state.write().await;
            // A reload while the guest ran replaced the host: drop the stale text.
            if !s
                .mods
                .as_ref()
                .map(|m| std::sync::Arc::ptr_eq(m, &mods))
                .unwrap_or(false)
            {
                return;
            }
            s.composer_prefill = Some((channel, text));
            wake(&s);
        });
    }

    /// Takes the pending composer prefill for `channel`, if any, so the front end
    /// can put it in its input (and clears it so it applies once).
    #[cfg(feature = "mods")]
    pub fn take_composer_prefill(state: &Arc<RwLock<AppState>>, channel: &str) -> Option<String> {
        let mut s = state.try_write().ok()?;
        match s.composer_prefill.take() {
            Some((ch, text)) if ch == channel => Some(text),
            // Not for this channel: leave it for the one it targets.
            other => {
                if let Some(v) = other {
                    s.composer_prefill = Some(v);
                }
                None
            }
        }
    }

    /// Runs a declared mod command, if any, and reports whether one handled it
    /// (so the caller does not also send it to Slack).
    #[cfg(feature = "mods")]
    async fn try_mod_command(
        state: &Arc<RwLock<AppState>>,
        command: &str,
        args: &str,
        channel: &str,
    ) -> bool {
        // `split_slash_command` keeps the leading `/`; the declared name does not.
        let name = {
            let s = state.read().await;
            mod_command_name(&s.mod_commands, command).map(|n| n.to_string())
        };
        let Some(name) = name else {
            return false;
        };
        // Await the delivery: a command that reaches no mod (e.g. one quarantined
        // after load) must fall through to Slack rather than vanish.
        Self::deliver_mod_command(state.clone(), name, args.to_string(), channel.to_string()).await
            > 0
    }

    /// Delivers a mod command to the host and refreshes an open view panel;
    /// returns how many mods it reached.
    #[cfg(feature = "mods")]
    async fn deliver_mod_command(
        state: Arc<RwLock<AppState>>,
        name: String,
        args: String,
        channel: String,
    ) -> usize {
        let mods = match state.read().await.mods.clone() {
            Some(mods) => mods,
            None => return 0,
        };
        let mods_for_guest = mods.clone();
        // Guest code is synchronous and CPU-bound: keep it off the workers.
        let delivered = tokio::task::spawn_blocking(move || {
            let host = &mods_for_guest;
            host.invoke_command(&name, &args, &channel)
        })
        .await
        .unwrap_or(0);
        // A handler may have shown or closed a dialog (even if it then errored).
        Self::apply_mod_dialogs(&state, &mods).await;
        if delivered == 0 {
            return 0;
        }
        // A handler may have pushed new view content: refresh the open panel.
        let panel = {
            let s = state.read().await;
            s.mod_view_panel
                .as_ref()
                .map(|p| (p.mod_id.clone(), p.view_id.clone()))
        };
        if let Some((mod_id, view_id)) = panel {
            Self::refresh_mod_view(state.clone(), mod_id, view_id);
        }
        delivered
    }

    /// Opens a mod view: reads the content the mod pushed and shows it in a
    /// panel. No-op if the host is gone.
    #[cfg(feature = "mods")]
    pub fn open_mod_view(
        state: Arc<RwLock<AppState>>,
        mod_id: String,
        view_id: String,
        title: String,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let mods = state.read().await.mods.clone();
            let Some(mods) = mods else {
                return;
            };
            let mods_for_read = mods.clone();
            let (mid, vid) = (mod_id.clone(), view_id.clone());
            let blocks = tokio::task::spawn_blocking(move || {
                let host = &mods_for_read;
                host.view_content(&mid, &vid)
            })
            .await
            .unwrap_or_default();
            let blocks: Vec<(String, String)> = blocks
                .into_iter()
                .map(|(k, t)| (k, crate::mods::sanitize(&t)))
                .collect();
            let mut s = state.write().await;
            // A reload while we read replaced the host and cleared the panel: do
            // not resurrect a pre-reload view.
            if !s
                .mods
                .as_ref()
                .map(|m| std::sync::Arc::ptr_eq(m, &mods))
                .unwrap_or(false)
            {
                return;
            }
            s.mod_view_panel = Some(ModViewPanel {
                mod_id,
                view_id,
                title,
                blocks: std::sync::Arc::new(blocks),
            });
            s.mod_view_menu = None;
            wake(&s);
        });
    }

    /// Re-reads an already-open mod-view panel's content (after an event a mod may
    /// have handled). A no-op if the panel is no longer open for that mod/view.
    #[cfg(feature = "mods")]
    fn refresh_mod_view(state: Arc<RwLock<AppState>>, mod_id: String, view_id: String) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let mods = state.read().await.mods.clone();
            let Some(mods) = mods else {
                return;
            };
            let mods_for_read = mods.clone();
            let (mid, vid) = (mod_id.clone(), view_id.clone());
            let blocks = tokio::task::spawn_blocking(move || {
                let host = &mods_for_read;
                host.view_content(&mid, &vid)
            })
            .await
            .unwrap_or_default();
            let blocks: Vec<(String, String)> = blocks
                .into_iter()
                .map(|(k, t)| (k, crate::mods::sanitize(&t)))
                .collect();
            let mut s = state.write().await;
            // A reload while we read replaced the host and cleared the panel.
            if !s
                .mods
                .as_ref()
                .map(|m| std::sync::Arc::ptr_eq(m, &mods))
                .unwrap_or(false)
            {
                return;
            }
            // Only if the same panel is still open: a close (or a switch) while the
            // guest ran must not be undone.
            let still_open = s
                .mod_view_panel
                .as_ref()
                .map(|p| p.mod_id == mod_id && p.view_id == view_id)
                .unwrap_or(false);
            if !still_open {
                return;
            }
            if let Some(panel) = s.mod_view_panel.as_mut() {
                panel.blocks = std::sync::Arc::new(blocks);
            }
            wake(&s);
        });
    }

    /// Closes the open mod-view panel.
    #[cfg(feature = "mods")]
    pub fn close_mod_view(state: Arc<RwLock<AppState>>) {
        if let Ok(mut s) = state.try_write() {
            s.mod_view_panel = None;
            wake(&s);
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let mut s = state.write().await;
            s.mod_view_panel = None;
            wake(&s);
        });
    }

    /// Applies the dialog effects the mods produced during a call: shows or closes
    /// the modal. A no-op when the host changed under us (a reload already reset
    /// the UI). Runs after the guest call (the shared `Host` is free then).
    #[cfg(feature = "mods")]
    async fn apply_mod_dialogs(
        state: &Arc<RwLock<AppState>>,
        mods: &Arc<crate::mods::Host>,
    ) {
        let mods_for_effects = mods.clone();
        // Guest code is synchronous and CPU-bound: keep it off the workers.
        let effects = tokio::task::spawn_blocking(move || {
            let host = &mods_for_effects;
            host.take_dialog_effects()
        })
        .await
        .unwrap_or_default();
        if effects.is_empty() {
            return;
        }
        let mut s = state.write().await;
        // A reload while we read replaced the host: do not act on stale effects.
        if !s
            .mods
            .as_ref()
            .map(|m| std::sync::Arc::ptr_eq(m, mods))
            .unwrap_or(false)
        {
            return;
        }
        for (mod_id, effect) in effects {
            match effect {
                crate::mods::DialogEffect::Show(d) => {
                    let blocks: Vec<(String, String)> = d
                        .blocks
                        .into_iter()
                        .map(|(k, t)| (k, crate::mods::sanitize(&t)))
                        .collect();
                    let actions: Vec<(String, String)> = d
                        .actions
                        .into_iter()
                        .map(|(id, label)| (id, crate::mods::sanitize(&label)))
                        .collect();
                    s.mod_dialog = Some(ModDialog {
                        mod_id,
                        title: crate::mods::sanitize(&d.title),
                        blocks: Arc::new(blocks),
                        actions: Arc::new(actions),
                    });
                    // A fresh dialog starts the TUI cursor on its first action.
                    s.mod_dialog_cursor = 0;
                }
                crate::mods::DialogEffect::Close => {
                    // Only close it if it is still this mod's dialog.
                    if s.mod_dialog
                        .as_ref()
                        .map(|d| d.mod_id == mod_id)
                        .unwrap_or(false)
                    {
                        s.mod_dialog = None;
                    }
                }
            }
        }
        wake(&s);
    }

    /// Delivers a click on an action of the open mod dialog. The dialog is
    /// dismissed **first**, so a handler may immediately show another.
    #[cfg(feature = "mods")]
    pub fn invoke_mod_dialog_action(
        state: Arc<RwLock<AppState>>,
        mod_id: String,
        action_id: String,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let mods = match state.read().await.mods.clone() {
                Some(mods) => mods,
                None => return,
            };
            {
                let mut s = state.write().await;
                if s.mod_dialog
                    .as_ref()
                    .map(|d| d.mod_id == mod_id)
                    .unwrap_or(false)
                {
                    s.mod_dialog = None;
                    wake(&s);
                }
            }
            let mods_for_guest = mods.clone();
            let (mid, aid) = (mod_id, action_id);
            // Guest code is synchronous and CPU-bound: keep it off the workers.
            let _ = tokio::task::spawn_blocking(move || {
                let host = &mods_for_guest;
                host.invoke_dialog_action(&mid, &aid)
            })
            .await;
            // The handler may have shown another dialog (or closed it).
            Self::apply_mod_dialogs(&state, &mods).await;
        });
    }

    /// Closes the open mod dialog (the user dismissed it). The host keeps no
    /// open-state, so there is nothing to notify.
    #[cfg(feature = "mods")]
    pub fn close_mod_dialog(state: Arc<RwLock<AppState>>) {
        if let Ok(mut s) = state.try_write() {
            s.mod_dialog = None;
            wake(&s);
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        tokio::spawn(async move {
            let mut s = state.write().await;
            s.mod_dialog = None;
            wake(&s);
        });
    }

    /// Delivers a reaction to the subscribed mods. The user's **own** reactions
    /// are skipped (a mod reacts as the user, so echoing them back would loop the
    /// mod into itself, exactly like the user's own messages are not delivered).
    /// No-op without mods or a runtime.
    #[cfg(feature = "mods")]
    pub fn mod_reaction_event(
        state: &Arc<RwLock<AppState>>,
        channel: String,
        ts: String,
        name: String,
        user: String,
        added: bool,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let state = state.clone();
        tokio::spawn(async move {
            let (mods, content) = {
                let s = state.read().await;
                if user == s.my_user_id {
                    return;
                }
                let Some(mods) = s.mods.clone() else {
                    return;
                };
                // The reacted message, when it is on screen, so `get-message` can
                // serve it (a reaction carries metadata, not content). Guarded by
                // channel, like every other lookup (`ts` alone can collide across
                // conversations).
                let content = s.find_mod_msg(&channel, &ts);
                (mods, content)
            };
            let event = if added {
                crate::mods::events::Event::ReactionAdded
            } else {
                crate::mods::events::Event::ReactionRemoved
            };
            let mods_for_guest = mods.clone();
            // Guest code is synchronous and CPU-bound: keep it off the workers.
            let delivered = tokio::task::spawn_blocking(move || {
                let host = &mods_for_guest;
                host.reaction_event(event, &channel, &ts, &name, &user, added, content)
            })
            .await
            .unwrap_or(0);
            // A handler may have shown or closed a dialog; do it even on an error
            // (the effect is already queued).
            Self::apply_mod_dialogs(&state, &mods).await;
            if delivered == 0 {
                return;
            }
            // Like a channel event: a handler may have pushed new view content.
            let panel = {
                let s = state.read().await;
                s.mod_view_panel
                    .as_ref()
                    .map(|p| (p.mod_id.clone(), p.view_id.clone()))
            };
            if let Some((mod_id, view_id)) = panel {
                Self::refresh_mod_view(state.clone(), mod_id, view_id);
            }
        });
    }

    /// [`Self::build`] against a caller-provided store.
    ///
    /// `demo` passes an **in-memory** store so the sample-data preview never
    /// reads the machine's real cache. Reading it would mix real conversations
    /// and people into the demo — and, in the UI snapshot tests, leak them into
    /// the rendered images.
    fn build_with_store(
        creds: Credentials,
        initial_channel: Option<String>,
        authenticated: bool,
        store: crate::store::Store,
    ) -> Self {
        let api_client = crate::api::Client::new(creds.clone());

        // We start from what is already in the cache, so the first screen is
        // immediate instead of waiting for two network round trips.
        let mut state = AppState::default();
        state.authenticated = authenticated;
        // The mods host performs the `Write` capabilities (e.g. `reactions.write`)
        // through the same client. Tests construct `AppState` directly (`None`, so
        // a write fails closed); the demo never loads mods, so its client is
        // never used.
        #[cfg(feature = "mods")]
        {
            state.api = Some(std::sync::Arc::new(api_client.clone()));
        }
        state.initial_channel = initial_channel.or_else(|| {
            store
                .get_meta("last_channel")
                .ok()
                .flatten()
                .filter(|s| !s.is_empty())
        });
        state.window = INITIAL_WINDOW;
        state.ui_scale = store
            .get_meta("ui_scale")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|f| f.is_finite())
            .map(|f| f.clamp(UI_SCALE_MIN, UI_SCALE_MAX))
            .unwrap_or(1.0);
        state.sidebar_width = store
            .get_meta("sidebar_width")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|f| f.is_finite())
            .map(|f| f.clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX))
            .unwrap_or(SIDEBAR_WIDTH_DEFAULT);
        // The language and the notification switch are remembered in the store,
        // so they win over the startup environment defaults.
        if let Some(lang) = store.get_meta("ui_lang").ok().flatten() {
            if !lang.is_empty() {
                rust_i18n::set_locale(&lang);
            }
        }
        if store.get_meta("notify").ok().flatten().as_deref() == Some("off") {
            crate::notify::set_enabled(false);
        }
        state.desktop_notifications = crate::notify::enabled();
        state.user_names = Arc::new(store.user_names().unwrap_or_default());
        state.usernames = Arc::new(store.username_names().unwrap_or_default());
        state.team_id = if creds.team_id.is_empty() {
            store.get_meta("team_id").ok().flatten().unwrap_or_default()
        } else {
            creds.team_id.clone()
        };
        state.favorites = Arc::new(load_favorites(&store));
        state.scheduled = load_scheduled(&store);
        match store.channels(false) {
            Ok(cached) => {
                // Everything in the cache comes from `users.conversations`,
                // that is, it is the user's own already: filtering on `is_member`
                // here would throw the channels away, because that endpoint does
                // not return it at all.
                let names = state.user_names.clone();
                let usernames = state.usernames.clone();
                let mut joined: Vec<ChannelEntry> = cached
                    .into_iter()
                    .filter(|ch| !ch.is_archived)
                    .map(|ch| {
                        let is_dm = ch.is_im || ch.is_mpim;
                        let name = if ch.is_im {
                            names.get(&ch.user).cloned().unwrap_or(ch.user.clone())
                        } else if ch.is_mpim {
                            mpim_label(&ch.name, &usernames)
                        } else {
                            ch.name.clone()
                        };
                        let user = ch.is_im.then(|| ch.user.clone());
                        ChannelEntry {
                            id: ch.id,
                            name,
                            is_dm,
                            unread: false,
                            mentions: 0,
                            user,
                            last_read: String::new(),
                            external: ch.is_ext_shared,
                        }
                    })
                    .collect();
                joined.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                state.status = if joined.is_empty() {
                    t!("app.loading").to_string()
                } else {
                    t!("app.channels_cache", n = joined.len()).to_string()
                };
                state.channels = Arc::new(joined);
                state.selected_channel = state
                    .initial_channel
                    .as_ref()
                    .and_then(|id| state.channels.iter().position(|c| &c.id == id))
                    .unwrap_or(0);
            }
            Err(e) => {
                state.fail(t!("app.cache_unreadable", e = e).to_string());
            }
        }

        // The login screen has nothing to load: a stale "loading…" there is
        // noise. Errors from a login attempt still arrive later and show.
        if !authenticated {
            state.status.clear();
        }

        App {
            creds,
            api_client,
            state: Arc::new(RwLock::new(state)),
            store: Arc::new(tokio::sync::Mutex::new(store)),
            presence_tx: tokio::sync::watch::channel(Vec::new()).0,
            demo: false,
        }
    }

    /// Builds an app seeded with sample data, for UI work without a workspace.
    ///
    /// No network, no credentials: the GUI renders the real screens from this
    /// fabricated state (see [`demo`]). Used by `slaq gui --demo`.
    pub fn demo(initial_channel: Option<String>) -> Self {
        let empty = Credentials {
            xoxc: String::new(),
            xoxd: String::new(),
            workspace_url: String::new(),
            user_id: String::new(),
            team_id: String::new(),
        };
        let mut app = Self::build_with_store(
            empty,
            initial_channel,
            true,
            crate::store::Store::open_in_memory().expect("an in-memory SQLite cache cannot fail"),
        );
        app.demo = true;
        // At construction nothing else holds the lock, so this cannot block.
        if let Ok(mut s) = app.state.try_write() {
            demo::seed(&mut s);
        }
        app
    }

    /// Whether the app runs on sample data (the GUI then starts no network task).
    pub fn is_demo(&self) -> bool {
        self.demo
    }

    /// Sets the GUI interface scale (clamped) and persists it. The GUI applies
    /// the new value as a zoom factor on its next frame.
    pub fn set_ui_scale(&self, scale: f32) {
        let scale = scale.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.ui_scale = scale;
                wake(&s);
            }
            store
                .lock()
                .await
                .set_meta("ui_scale", &format!("{scale}"))
                .ok();
        });
    }

    /// Remembers the sidebar width for next time, clamped to the allowed range.
    /// Called by the GUI when the user finishes dragging the panel edge.
    pub fn set_sidebar_width(&self, width: f32) {
        let width = width.clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX);
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.sidebar_width = width;
                wake(&s);
            }
            store
                .lock()
                .await
                .set_meta("sidebar_width", &format!("{width}"))
                .ok();
        });
    }

    /// Switches the interface language at once and remembers it for next time.
    ///
    /// `rust_i18n` keeps the locale in a process global, so the whole UI picks
    /// it up on the next frame; the wake makes that immediate.
    pub fn set_language(&self, lang: String) {
        rust_i18n::set_locale(&lang);
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            {
                let s = state.read().await;
                wake(&s);
            }
            store.lock().await.set_meta("ui_lang", &lang).ok();
        });
    }

    /// Turns desktop notifications on or off at runtime, and remembers it.
    pub fn set_notifications(&self, on: bool) {
        // The notifier reads the flag synchronously; the state feeds the UI.
        crate::notify::set_enabled(on);
        if let Ok(mut s) = self.state.try_write() {
            s.desktop_notifications = on;
            wake(&s);
        }
        let store = self.store.clone();
        tokio::spawn(async move {
            store
                .lock()
                .await
                .set_meta("notify", if on { "on" } else { "off" })
                .ok();
        });
    }

    /// The user put off re-login: close the dialog. It comes back if another
    /// request fails with an expired session.
    pub fn dismiss_session_expired(&self) {
        if let Ok(mut s) = self.state.try_write() {
            s.session_expired = false;
            wake(&s);
        }
    }

    /// Opens the selected channel's huddle in the browser.
    ///
    /// The TUI does not attempt the call: two-way audio, echo and above all
    /// *seeing* someone else's screen do not fit in a terminal. The browser is
    /// open already and authenticated already.
    pub fn open_huddle(&self, state: &mut AppState) {
        let Some(ch) = state.channels.get(state.selected_channel) else {
            state.status = t!("app.no_channel_selected").to_string();
            return;
        };
        if state.team_id.is_empty() {
            state.fail(t!("app.team_id_unavailable").to_string());
            return;
        }
        let browser = crate::huddle::Browser::from_env();
        match crate::huddle::open(&state.team_id, &ch.id, &browser) {
            Ok(_) => {
                state.status = t!(
                    "app.huddle_opened",
                    name = ch.name,
                    browser = browser.describe()
                )
                .to_string()
            }
            Err(e) => state.fail(t!("app.huddle_open_failed", e = e).to_string()),
        }
    }

    /// Stops an incoming call ringing without rejecting it.
    ///
    /// The room stays in the list: ignoring is not hanging up, and a minute later
    /// you can still join.
    pub fn dismiss_ring(&self, channel: String) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut s = state.write().await;
            if let Some(h) = s.huddles.get_mut(&channel) {
                h.ringing = false;
            }
            wake(&s);
        });
    }

    /// The parts of a `reaction_added`/`reaction_removed` a mod cares about:
    /// `(channel, ts, name, user, added)`. `None` for a reaction on a file (only
    /// message reactions concern us) or a malformed payload.
    fn parse_reaction(raw: &serde_json::Value) -> Option<(String, String, String, String, bool)> {
        let added = raw.get("type").and_then(|v| v.as_str()) == Some("reaction_added");
        let item = raw.get("item");
        if item.and_then(|i| i.get("type")).and_then(|v| v.as_str()) != Some("message") {
            return None;
        }
        Some((
            item.and_then(|i| i.get("channel"))
                .and_then(|v| v.as_str())?
                .to_string(),
            item.and_then(|i| i.get("ts"))
                .and_then(|v| v.as_str())?
                .to_string(),
            raw.get("reaction").and_then(|v| v.as_str())?.to_string(),
            raw.get("user").and_then(|v| v.as_str())?.to_string(),
            added,
        ))
    }

    /// Applies a `reaction_added` / `reaction_removed` to the cache and to the
    /// view.
    ///
    /// The cache is what a conversation is redrawn from when you come back to
    /// it, so a reaction that only reaches the view lives until you change
    /// channel and no longer.
    async fn apply_reaction_event(
        raw: &serde_json::Value,
        state: &Arc<RwLock<AppState>>,
        store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    ) {
        let Some((channel, ts, name, user, added)) = Self::parse_reaction(raw) else {
            return;
        };
        let (channel, ts, name, user) =
            (channel.as_str(), ts.as_str(), name.as_str(), user.as_str());

        {
            let mut st = store.lock().await;
            let Ok(Some(mut msg)) = st.raw_message(channel, ts) else {
                // Not in the cache: nothing to keep, and the next load will
                // bring it with its reactions already on it.
                return;
            };
            apply_reaction(&mut msg, user, name, added);
            if let Err(e) = st.upsert_messages(channel, std::slice::from_ref(&msg)) {
                tracing::warn!("reaction not saved: {e}");
            }
        }

        // Redraw only for the open conversation; for the others having it in
        // the cache is enough.
        let mut s = state.write().await;
        let viewing = s.channels.get(s.selected_channel).map(|c| c.id.clone());
        if viewing.as_deref() != Some(channel) {
            return;
        }
        let me = s.my_user_id.clone();
        let AppState {
            messages,
            thread_messages,
            ..
        } = &mut *s;
        for m in messages.iter_mut().chain(thread_messages.iter_mut()) {
            if m.ts != ts {
                continue;
            }
            match m.reactions.iter_mut().find(|r| r.name == name) {
                Some(r) => {
                    if added {
                        if !r.users.iter().any(|u| u == user) {
                            r.users.push(user.to_string());
                        }
                    } else {
                        r.users.retain(|u| u != user);
                    }
                    r.count = r.users.len() as i64;
                    r.mine = r.users.iter().any(|u| *u == me);
                }
                None if added => m.reactions.push(Reaction {
                    name: name.to_string(),
                    count: 1,
                    mine: user == me,
                    users: vec![user.to_string()],
                }),
                None => {}
            }
            m.reactions.retain(|r| r.count > 0);
        }
        wake(&s);
    }

    /// Updates the huddle state and rings, where appropriate.
    ///
    /// Only a direct invite rings: in a channel nobody is calling you, the call is
    /// there and you see it in the list. Making a window appear for every huddle
    /// open in one of the hundred channels you belong to would be the fastest way
    /// to get anyone to switch the whole thing off.
    async fn apply_huddle(state: &Arc<RwLock<AppState>>, ev: huddles::Event) {
        let mut notify: Option<(String, String)> = None;
        {
            let mut s = state.write().await;
            match ev {
                huddles::Event::Room { channel, room } => {
                    if room.ended {
                        s.huddles.remove(&channel);
                    } else {
                        // If we are already in it nothing should ring any more:
                        // it is the room we are talking in.
                        let me = s.my_user_id.clone();
                        let joined = room.participants.contains(&me);
                        let e = s.huddles.entry(channel).or_insert_with(|| Huddle {
                            room_id: room.id.clone(),
                            participants: Vec::new(),
                            caller: None,
                            ringing: false,
                        });
                        e.room_id = room.id;
                        e.participants = room.participants;
                        if joined {
                            e.ringing = false;
                        }
                    }
                }
                huddles::Event::Cancel { channel } => {
                    // A missed call must not be removed from the list: the
                    // room can stay open and you can still join it.
                    if let Some(h) = s.huddles.get_mut(&channel) {
                        h.ringing = false;
                    }
                }
                huddles::Event::Invite { channel, caller } => {
                    let who = s
                        .user_names
                        .get(&caller)
                        .cloned()
                        .unwrap_or_else(|| "qualcuno".to_string());
                    let dove = s
                        .channels
                        .iter()
                        .find(|c| c.id == channel)
                        .map(|c| {
                            if c.is_dm {
                                c.name.clone()
                            } else {
                                format!("#{}", c.name)
                            }
                        })
                        .unwrap_or_else(|| who.clone());
                    let h = s.huddles.entry(channel).or_insert_with(|| Huddle {
                        room_id: String::new(),
                        participants: Vec::new(),
                        caller: None,
                        ringing: false,
                    });
                    h.caller = Some(caller);
                    h.ringing = true;
                    s.status = t!("app.incoming_call", who = who).to_string();
                    notify = Some((dove, t!("app.calling_you", who = who).to_string()));
                }
            }
            wake(&s);
        }

        if let Some((heading, body)) = notify {
            if crate::notify::enabled() {
                crate::notify::show_detached(crate::notify::DesktopNotification {
                    title: Some("slaq".into()),
                    subtitle: Some(heading),
                    content: Some(body),
                    ..Default::default()
                });
            }
        }
    }

    /// Applies an event arriving from the WebSocket: the cache first, then
    /// the view.
    ///
    /// The cache is the source we redraw from, so edits and deletions follow the
    /// same path as new messages instead of needing three different updates of the
    /// in-memory state.
    pub async fn apply_ws_update(
        update: crate::ws::WsUpdate,
        state: &Arc<RwLock<AppState>>,
        store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
        api: &crate::api::Client,
    ) {
        use crate::ws::WsUpdate;

        let env = match update {
            WsUpdate::Connected => {
                let (reconnect, selected) = {
                    let mut s = state.write().await;
                    let reconnect = s.ws_seen;
                    s.ws_seen = true;
                    s.live = true;
                    (
                        reconnect,
                        s.channels.get(s.selected_channel).map(|c| c.id.clone()),
                    )
                };
                // Everything that happened while the socket was down is missing:
                // reloading the open conversation is what makes it catch up.
                if reconnect {
                    if let Some(id) = selected {
                        Self::load_channel_messages(id, state, api, store);
                    }
                }
                return;
            }
            WsUpdate::Disconnected(reason) => {
                let mut s = state.write().await;
                s.live = false;
                tracing::info!("WebSocket dropped: {reason}");
                return;
            }
            WsUpdate::Event(env) => env,
        };

        // Presence arrives batched: one event carries the same state for many
        // users at once (`batch_presence_aware=1` in the URL).
        if env.event.event_type == "presence_change" {
            let active = env.event.presence.as_deref() == Some("active");
            let users: Vec<String> = match env.raw.get("users") {
                Some(v) => v
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|u| u.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                // The unbatched shape, in case the gateway used it anyway.
                None => env.event.user.iter().cloned().collect(),
            };
            if !users.is_empty() {
                let mut st = state.write().await;
                for u in users {
                    // Our **own** verdict, as it happens: the read-back after an
                    // assert shows one moment, this shows the drift (Slack marking
                    // us away on its own, which no call can override).
                    if u == st.my_user_id {
                        tracing::info!(
                            "presence_change(me) -> {}",
                            if active { "active" } else { "away" }
                        );
                    }
                    Arc::make_mut(&mut st.presence).insert(u, active);
                }
                wake(&st);
            }
            return;
        }

        // The status a person chooses changes often during the day: without
        // this it would stay the one from the first load.
        if env.event.event_type == "user_change" {
            if let Some(u) = env.raw.get("user") {
                let id = u.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                if !id.is_empty() {
                    let p = u.get("profile");
                    let get = |k: &str| {
                        p.and_then(|p| p.get(k))
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string()
                    };
                    let mut st = state.write().await;
                    Arc::make_mut(&mut st.user_status).insert(
                        id.to_string(),
                        UserStatus {
                            emoji: get("status_emoji"),
                            text: get("status_text"),
                        },
                    );
                    let is_bot = u.get("is_bot").and_then(|v| v.as_bool()).unwrap_or(false)
                        || u.get("is_app_user")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                    if is_bot {
                        Arc::make_mut(&mut st.bot_users).insert(id.to_string());
                    }
                    wake(&st);
                }
            }
            return;
        }

        // Reactions: Slack does not resend the message, it says what changed.
        if matches!(
            env.event.event_type.as_str(),
            "reaction_added" | "reaction_removed"
        ) {
            Self::apply_reaction_event(&env.raw, state, store).await;
            #[cfg(feature = "mods")]
            if let Some((channel, ts, name, user, added)) = Self::parse_reaction(&env.raw) {
                Self::mod_reaction_event(state, channel, ts, name, user, added);
            }
            return;
        }

        // Huddles: someone is calling, or a call is running somewhere.
        if let Some(ev) = huddles::parse(&env.raw) {
            Self::apply_huddle(&state, ev).await;
            return;
        }

        // Typing: `user_typing` says someone started; there is no "stopped", so
        // the indicator is a timestamp that expires on its own.
        if env.event.event_type == "user_typing" {
            let channel = env.event.channel.clone().unwrap_or_default();
            let user = env.event.user.clone().unwrap_or_default();
            if !channel.is_empty() && !user.is_empty() {
                let mut st = state.write().await;
                if user != st.my_user_id {
                    let expires = std::time::Instant::now() + TYPING_TTL;
                    Arc::make_mut(&mut st.typing)
                        .entry(channel)
                        .or_default()
                        .insert(user, expires);
                    wake(&st);
                }
            }
            return;
        }

        if env.event.event_type != "message" {
            return;
        }

        // Notifications are decided here, from the message: the
        // `desktop_notification` event they were built on no longer arrives.
        // The in-app centre records them as well, whether or not desktop
        // notifications are on: they are two separate switches.
        {
            let (me, chan_id, chan_name, is_dm, author) = {
                let s = state.read().await;
                let ch = env
                    .event
                    .channel
                    .as_deref()
                    .and_then(|id| s.channels.iter().find(|c| c.id == id));
                let author = env
                    .event
                    .user
                    .as_deref()
                    .and_then(|u| s.user_names.get(u).cloned())
                    .unwrap_or_else(|| "qualcuno".to_string());
                (
                    s.my_user_id.clone(),
                    ch.map(|c| c.id.clone()).unwrap_or_default(),
                    ch.map(|c| c.name.clone()).unwrap_or_default(),
                    ch.map(|c| c.is_dm).unwrap_or(false),
                    author,
                )
            };
            let names = state.read().await.user_names.clone();
            let ctx = crate::notify::Context {
                my_user_id: &me,
                channel_name: &chan_name,
                is_dm,
                author: &author,
                names: &names,
            };
            if let Some(n) = crate::notify::from_message(&env.raw, &ctx) {
                if crate::notify::enabled() {
                    crate::notify::show_detached(n.clone());
                }
                let ts = env
                    .raw
                    .get("ts")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let thread_ts = env
                    .raw
                    .get("thread_ts")
                    .and_then(|v| v.as_str())
                    .filter(|t| *t != ts)
                    .map(str::to_string);
                let channel_id = if chan_id.is_empty() {
                    env.event.channel.clone().unwrap_or_default()
                } else {
                    chan_id
                };
                let channel_name = if chan_name.is_empty() {
                    n.heading()
                } else {
                    chan_name
                };
                let body = n.body();
                let text = body
                    .strip_prefix(&format!("{author}: "))
                    .map(str::to_string)
                    .unwrap_or(body);
                let entry = NotifEntry {
                    channel_id,
                    channel_name,
                    author,
                    text,
                    ts,
                    is_dm,
                    thread_ts,
                    read: false,
                };
                let line = t!(
                    "app.notification_line",
                    heading = n.heading(),
                    body = n.body()
                )
                .to_string();
                let mut s = state.write().await;
                push_notification(&mut s, entry);
                s.status = line;
                wake(&s);
            }
        }

        // `channel` is on the event for edits and deletions too.
        let Some(channel_id) = env.event.channel.clone() else {
            return;
        };

        let touched = {
            let mut st = store.lock().await;
            match crate::ws::cache_op(&env.raw) {
                crate::ws::CacheOp::Upsert(m) => st
                    .upsert_messages(&channel_id, std::slice::from_ref(m))
                    .map(|n| n > 0)
                    .unwrap_or(false),
                crate::ws::CacheOp::Delete(ts) => {
                    st.delete_message(&channel_id, ts).unwrap_or(false)
                }
                crate::ws::CacheOp::Ignore => false,
            }
        };

        // A deletion (or edit) of an open-thread reply may not be in the message
        // cache (`conversations_replies` results are not all persisted), so
        // `touched` is false; still process it when it concerns the open thread.
        if !touched {
            let concerns_thread = {
                let s = state.read().await;
                match (s.thread_channel_id.clone(), s.thread_parent_ts.clone()) {
                    (Some(tc), Some(parent)) if tc == channel_id => {
                        let thread_ts: std::collections::HashSet<String> =
                            s.thread_messages.iter().map(|m| m.ts.clone()).collect();
                        event_matches_thread(&env.raw, &parent, &thread_ts)
                    }
                    _ => false,
                }
            };
            if !concerns_thread {
                return;
            }
        }

        // Redraw only if the event concerns the open channel; for the others
        // having written it to the cache is enough.
        let viewing = {
            let s = state.read().await;
            s.channels.get(s.selected_channel).map(|c| c.id.clone())
        };

        // A message in a channel that is not open marks it unread: the
        // badges computed at bootstrap would otherwise stay still for the
        // whole session.
        if viewing.as_deref() != Some(channel_id.as_str()) {
            let mut s = state.write().await;
            if let Some(c) = Arc::make_mut(&mut s.channels)
                .iter_mut()
                .find(|c| c.id == channel_id)
            {
                c.unread = true;
            }
        }
        if viewing.as_deref() != Some(channel_id.as_str()) {
            // A reply can belong to the open thread even when its channel is not
            // the selected conversation (the thread panel may still show it), so
            // reload it: reload_thread then dispatches the mods.
            let (thread_channel, thread_parent, thread_ts) = {
                let s = state.read().await;
                (
                    s.thread_channel_id.clone(),
                    s.thread_parent_ts.clone(),
                    s.thread_messages
                        .iter()
                        .map(|m| m.ts.clone())
                        .collect::<std::collections::HashSet<_>>(),
                )
            };
            if let (Some(tc), Some(parent)) = (thread_channel, thread_parent) {
                if tc == channel_id && event_matches_thread(&env.raw, &parent, &thread_ts) {
                    Self::reload_thread(channel_id, parent, state, api).await;
                }
            }
            return;
        }

        let window = state.read().await.window.max(INITIAL_WINDOW);
        let rows = store
            .lock()
            .await
            .messages(&channel_id, window)
            .unwrap_or_default();
        {
            let mut s = state.write().await;
            let me = s.my_user_id.clone();
            s.messages = rows
                .iter()
                .filter_map(|m| parse_message_as(&m.raw, &me))
                .collect();
            // Tag the conversation these messages belong to: the mod dispatch and
            // the render path both key on it (select_index clears it).
            s.messages_channel = Some(channel_id.clone());
            wake(&s);
        }
        spawn_resolve_bots(state, api);

        // A live message must reach the mods too, not only a full channel load.
        #[cfg(feature = "mods")]
        Self::run_mods(state, &channel_id).await;

        // If the event belongs to the open thread that has to be updated too:
        // the timeline does not show it.
        let (open_thread, thread_channel, thread_ts) = {
            let s = state.read().await;
            (
                s.thread_parent_ts.clone(),
                s.thread_channel_id.clone(),
                s.thread_messages
                    .iter()
                    .map(|m| m.ts.clone())
                    .collect::<std::collections::HashSet<_>>(),
            )
        };
        if let (Some(parent), Some(tc)) = (open_thread, thread_channel) {
            // The open thread must belong to this conversation (as in the other
            // two paths), or we would fetch replies into the wrong channel.
            if tc == channel_id && event_matches_thread(&env.raw, &parent, &thread_ts) {
                Self::reload_thread(channel_id, parent, state, api).await;
            }
        }
    }

    /// Whether a message authored by `user` is delivered to mods. The user's own
    /// messages are **not**: a mod may post as the user (`chat.send`), so its post
    /// would come back authored by us and re-trigger the mod in a loop.
    #[cfg(feature = "mods")]
    fn visible_to_mods(user: &str, me: &str) -> bool {
        user != me
    }

    /// The `ts` of every message of `channel_id` on screen now (timeline and open
    /// thread), for pruning published decorations.
    #[cfg(feature = "mods")]
    fn on_screen_ts(s: &AppState, channel_id: &str) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        if s.messages_channel.as_deref() == Some(channel_id) {
            out.extend(s.messages.iter().map(|m| m.ts.clone()));
        }
        if s.thread_channel_id.as_deref() == Some(channel_id) {
            out.extend(s.thread_messages.iter().map(|m| m.ts.clone()));
        }
        out
    }

    /// Runs the installed mods over the messages now on screen and stores the
    /// decorations to render, keyed by `ts`.
    #[cfg(feature = "mods")]
    async fn run_mods(state: &Arc<RwLock<AppState>>, channel_id: &str) {
        // No global dispatch lock: each mod runs on its own worker thread and the
        // Host cache is the source of truth, so dispatches and gestures run
        // concurrently. A rare snapshot/gesture overlap can briefly overwrite a
        // decoration in `AppState`; it self-heals on the next dispatch.
        let (msgs, mods, directory) = {
            let s = state.read().await;
            let Some(mods) = s.mods.clone() else {
                return;
            };
            // Dispatch the timeline and the open thread only if each belongs to
            // this conversation (they can differ after switching conversations),
            // merged and de-duplicated by `ts`: a thread response repeats its
            // root, which is also in the timeline, and must not run twice. The
            // timeline copy wins as the fresher full-window view. File-only and
            // attachment-only messages are kept (with an empty text).
            //
            // The user's **own** messages are not delivered: a mod may post as the
            // user (`chat.send`), so its post would come back as a `message`
            // authored by us and re-trigger the mod in a loop.
            let me = s.my_user_id.clone();
            let mut msgs: Vec<crate::mods::Msg> = Vec::new();
            let mut seen_ts = std::collections::HashSet::new();
            if s.messages_channel.as_deref() == Some(channel_id) {
                for m in &s.messages {
                    if Self::visible_to_mods(&m.user, &me) && seen_ts.insert(m.ts.clone()) {
                        msgs.push(Self::mod_msg(m, &me));
                    }
                }
            }
            if s.thread_channel_id.as_deref() == Some(channel_id) {
                for m in &s.thread_messages {
                    if Self::visible_to_mods(&m.user, &me) && seen_ts.insert(m.ts.clone()) {
                        msgs.push(Self::mod_msg(m, &me));
                    }
                }
            }
            if msgs.is_empty() {
                return;
            }
            // The directory the mods may enumerate, built only for the halves a
            // loaded mod actually holds and bounded while building (a large
            // workspace's caches are never cloned wholesale, nor per event).
            let (needs_channels, needs_users, needs_files) = mods.directory_needs();
            let mut directory = crate::mods::Directory::default();
            if needs_channels {
                directory.channels = s
                    .channels
                    .iter()
                    .take(crate::mods::MAX_DIRECTORY_ENTRIES)
                    .map(|c| (c.id.clone(), c.name.clone()))
                    .collect();
            }
            if needs_users {
                directory.users = s
                    .user_names
                    .iter()
                    .take(crate::mods::MAX_DIRECTORY_ENTRIES)
                    .map(|(id, name)| (id.clone(), name.clone()))
                    .collect();
            }
            if needs_files {
                directory.files = Self::mod_files(&s);
            }
            let directory = Arc::new(directory);
            (msgs, mods, directory)
        };
        // The messages currently on screen (before the mod-window cap): used to
        // prune only decorations whose message is truly gone, so a message dropped
        // from the mod window keeps its decoration.
        let visible_ts: std::collections::HashSet<String> =
            msgs.iter().map(|m| m.ts.clone()).collect();
        // Bound the mod window to the most recent MAX_CACHE messages (the history
        // bound), so cache pressure cannot replay events.
        let mut msgs = msgs;
        if msgs.len() > crate::mods::MAX_CACHE {
            let drop = msgs.len() - crate::mods::MAX_CACHE;
            msgs.drain(0..drop);
        }
        let channel = channel_id.to_string();
        // The messages this dispatch owns, to retract/replace their old
        // decorations (an edit that now produces none must not leave the old one).
        let keys: Vec<(String, String)> = msgs
            .iter()
            .map(|m| (channel.clone(), m.ts.clone()))
            .collect();
        let msgs = Arc::new(msgs);
        // Dispatch in a bounded number of aggregate-budgeted batches: each call is
        // CPU-bounded and the loop is capped, so the total work per call is finite
        // (and a message dispatched in a batch is never re-run by cache pressure).
        let mut rounds = 0u32;
        let mut still_pending = false;
        loop {
            let (decorations, pending) = {
                let ch = channel.clone();
                let batch = msgs.clone();
                let mods = mods.clone();
                let directory = directory.clone();
                // Guest code is synchronous and CPU-bound: keep it off the async workers.
                match tokio::task::spawn_blocking(move || {
                    mods.dispatch_batch_with(&ch, &batch, &directory)
                })
                .await
                {
                    Ok(pair) => pair,
                    Err(e) => {
                        // A panicked/cancelled dispatch must not wipe what is on screen.
                        tracing::warn!("mod dispatch task failed: {e}");
                        return;
                    }
                }
            };
            still_pending = pending;
            let mut s = state.write().await;
            // A reload replaced the host while this dispatch ran: its output is the
            // old host's (possibly a removed or revoked mod's) — the new host
            // dispatches the conversation itself.
            if !s
                .mods
                .as_ref()
                .is_some_and(|m| std::sync::Arc::ptr_eq(m, &mods))
            {
                return;
            }
            // What is on screen *now*, besides this dispatch's snapshot: an
            // overlapping, newer dispatch may have published a message this one
            // never saw, and pruning to the snapshot alone would erase it.
            let on_screen_now = Self::on_screen_ts(&s, channel_id);
            let d = Arc::make_mut(&mut s.decorations);
            // Retract this dispatch's old entries, then bound by the *projected*
            // size (current minus replaced plus new), so the published map is
            // within the limits, not just the pre-dispatch one.
            for (ch, ts) in &keys {
                if let Some(m) = d.get_mut(ch) {
                    m.remove(ts);
                }
            }
            // Prune the conversation to the messages still on screen (pre-cap), so
            // a deleted message's decoration goes but a capped-out one's survives.
            if let Some(by_ts) = d.get_mut(channel_id) {
                by_ts.retain(|ts, _| visible_ts.contains(ts) || on_screen_now.contains(ts));
            }
            let added_keys = decorations.len();
            let added_bytes: usize = decorations
                .values()
                .map(|v| v.iter().map(String::len).sum::<usize>())
                .sum();
            let keys_count: usize = d.values().map(HashMap::len).sum::<usize>() + added_keys;
            let bytes: usize = d
                .values()
                .flat_map(|m| m.values())
                .map(|v| v.iter().map(String::len).sum::<usize>())
                .sum::<usize>()
                + added_bytes;
            if keys_count > crate::mods::MAX_CACHE || bytes > crate::mods::MAX_CACHED_BYTES {
                // Last resort: clear everything; the current batch (inserted just
                // below) is already bounded by the host, so the result is within
                // the limits, whereas keeping the old channel map could not be.
                d.clear();
            }
            for ((ch, ts), v) in decorations {
                d.entry(ch).or_default().insert(ts, v);
            }
            d.retain(|_, by_ts| !by_ts.is_empty());
            wake(&s);
            drop(s);
            rounds += 1;
            if !pending || rounds >= MAX_MOD_BATCHES {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // A handler may have shown or closed a dialog during the batch.
        Self::apply_mod_dialogs(&state, &mods).await;
        if still_pending {
            // More work than the batch cap covers: enqueue a deduplicated
            // follow-up for the worker task (never a recursive call here) under a
            // rate-limited per-channel budget, so a huge window cannot drain CPU
            // without bound. An exhausted budget leaves the tail undecorated until
            // a new window opens.
            let mut s = state.write().await;
            if let Some(tx) = s.mods_queue.clone() {
                if s.claim_followup(channel_id, std::time::Instant::now()) {
                    if s.mods_queued.insert(channel_id.to_string()) {
                        let _ = tx.send(channel_id.to_string());
                    }
                } else if s.mods_retry.insert(channel_id.to_string()) {
                    // Budget exhausted: wake once at the next window so the tail is
                    // not left undecorated forever (deduplicated by `mods_retry`).
                    let st = state.clone();
                    let ch = channel_id.to_string();
                    tokio::spawn(async move {
                        tokio::time::sleep(MODS_FOLLOWUP_WINDOW).await;
                        let tx = {
                            let mut s = st.write().await;
                            s.mods_retry.remove(&ch);
                            s.mods_queue.clone()
                        };
                        if let Some(tx) = tx {
                            let _ = tx.send(ch);
                        }
                    });
                }
            }
        }
    }

    /// Shows the cached messages at once, then downloads only what is new.
    ///
    /// The visible window comes from the DB in both cases, so the channel opens
    /// instantly and the network is only needed for the increment.
    pub fn load_channel_messages(
        channel_id: String,
        state: &Arc<RwLock<AppState>>,
        api: &crate::api::Client,
        store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    ) {
        let api = api.clone();
        let state_clone = state.clone();
        let store = store.clone();
        tokio::spawn(async move {
            // 1. from the cache, immediate
            let me = state_clone.read().await.my_user_id.clone();
            let window = state_clone.read().await.window.max(INITIAL_WINDOW);
            let cached = store.lock().await.messages(&channel_id, window);
            if let Ok(ref rows) = cached {
                if !rows.is_empty() {
                    let mut s = state_clone.write().await;
                    if selected_is(&s, &channel_id) {
                        s.messages = rows
                            .iter()
                            .filter_map(|m| parse_message_as(&m.raw, &me))
                            .collect();
                        s.messages_channel = Some(channel_id.clone());
                        s.message_scroll = 0;
                        wake(&s);
                        s.status = t!("app.messages_cache", n = s.messages.len()).to_string();
                    }
                    drop(s);
                    spawn_resolve_bots(&state_clone, &api);
                    // Decorate the cached rows now, not only after the network:
                    // the seen cache makes the later dispatch safe.
                    #[cfg(feature = "mods")]
                    Self::run_mods(&state_clone, &channel_id).await;
                }
            }

            // 2. the newest window from the API, not only what is newer than
            // the cache: reactions, edits and reply counts happen on messages
            // already stored, and a forward-only fetch would never refresh
            // them — a reaction added while offline stayed invisible.
            let fetched = api
                .conversations_history_range(&channel_id, Some(WINDOW.max(100)), None, None)
                .await;

            match fetched {
                Ok(msgs) => {
                    let new_count = {
                        let mut st = store.lock().await;
                        let n = st.upsert_messages(&channel_id, &msgs).unwrap_or(0);
                        n
                    };
                    let rows = store
                        .lock()
                        .await
                        .messages(&channel_id, window)
                        .unwrap_or_default();
                    let mut s = state_clone.write().await;
                    if selected_is(&s, &channel_id) {
                        s.messages = rows
                            .iter()
                            .filter_map(|m| parse_message_as(&m.raw, &me))
                            .collect();
                        s.messages_channel = Some(channel_id.clone());
                        s.message_scroll = 0;
                        s.status = if new_count > 0 {
                            t!("app.messages_plus", n = s.messages.len(), new = new_count)
                                .to_string()
                        } else {
                            t!("app.messages", n = s.messages.len()).to_string()
                        };
                    }
                    drop(s);
                    spawn_resolve_bots(&state_clone, &api);
                }
                Err(e) => {
                    let mut s = state_clone.write().await;
                    // The cache is already on screen: a network error does not erase it.
                    s.status = if s.messages.is_empty() {
                        t!("app.status_error", e = e).to_string()
                    } else {
                        t!("app.messages_offline", n = s.messages.len()).to_string()
                    };
                }
            }
            #[cfg(feature = "mods")]
            Self::run_mods(&state_clone, &channel_id).await;
        });
    }

    pub fn adjust_channel_scroll(state: &mut AppState) {
        let count = state.channels.len();
        if count == 0 {
            return;
        }
        if state.selected_channel >= count {
            state.selected_channel = count - 1;
        }
        if state.selected_channel < state.channel_scroll {
            state.channel_scroll = state.selected_channel;
        } else if state.selected_channel >= state.channel_scroll + 18 {
            state.channel_scroll = state.selected_channel.saturating_sub(17);
        }
    }

    pub fn user_name(state: &AppState, user_id: &str) -> String {
        if user_id == "me" {
            return "me".to_string();
        }
        state
            .user_names
            .get(user_id)
            .cloned()
            .unwrap_or_else(|| user_id.to_string())
    }

    /// Opens a thread: marks the parent, clears the old replies and loads.
    ///
    /// Separate from [`App::load_thread`] because the front end must be able to
    /// show the panel at once, before the network answers.
    pub fn open_thread(&self, channel_id: String, thread_ts: String) {
        Self::open_thread_with(channel_id, thread_ts, &self.state, &self.api_client);
    }

    /// The body of [`App::open_thread`], callable from inside another task —
    /// a jump that discovers the thread only once the page has arrived — where
    /// `self` is not available.
    fn open_thread_with(
        channel_id: String,
        thread_ts: String,
        state: &Arc<RwLock<AppState>>,
        api: &crate::api::Client,
    ) {
        let state = state.clone();
        let api = api.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.thread_parent_ts = Some(thread_ts.clone());
                s.thread_channel_id = Some(channel_id.clone());
                s.thread_messages.clear();
                s.thread_scroll = 0;
                s.status = t!("app.loading_thread").to_string();
                wake(&s);
            }
            // A search hit may point at a reply and not carry `thread_ts`;
            // asking conversations.replies for that ts then returns the reply
            // alone (it looked like a root). The reply itself knows its parent,
            // so resolve the real root and load the whole thread.
            let (root, msgs) = match api.conversations_replies(&channel_id, &thread_ts).await {
                Ok(msgs) => {
                    let parent = msgs
                        .iter()
                        .find(|m| m.get("ts").and_then(|v| v.as_str()) == Some(thread_ts.as_str()))
                        .and_then(|m| m.get("thread_ts").and_then(|v| v.as_str()))
                        .filter(|t| *t != thread_ts.as_str())
                        .map(String::from);
                    match parent {
                        Some(p) => match api.conversations_replies(&channel_id, &p).await {
                            Ok(all) => (p, all),
                            Err(_) => (p, msgs),
                        },
                        None => (thread_ts.clone(), msgs),
                    }
                }
                Err(e) => {
                    state
                        .write()
                        .await
                        .fail(t!("app.thread_error", e = e).to_string());
                    return;
                }
            };
            let mut s = state.write().await;
            let me = s.my_user_id.clone();
            s.thread_parent_ts = Some(root);
            s.thread_channel_id = Some(channel_id.clone());
            s.thread_messages = msgs
                .iter()
                .filter_map(|m| parse_message_as(m, &me))
                .collect();
            s.status = t!(
                "app.thread_replies",
                n = s.thread_messages.len().saturating_sub(1)
            )
            .to_string();
            wake(&s);
            drop(s);
            #[cfg(feature = "mods")]
            Self::run_mods(&state, &channel_id).await;
        });
    }

    /// Closes the open thread.
    pub fn close_thread(&self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut s = state.write().await;
            s.thread_parent_ts = None;
            s.thread_channel_id = None;
            s.thread_messages.clear();
        });
    }

    /// Opens the "channel info" panel: details and member list.
    ///
    /// The panel opens empty and fills when the two calls answer, so the
    /// front end can show it before the network replies.
    pub fn open_channel_info(&self, channel_id: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.channel_info = None;
                s.channel_members.clear();
                s.status = t!("app.loading_channel_info").to_string();
                wake(&s);
            }
            let info = api.conversations_info(&channel_id).await;
            let members = api.conversations_members(&channel_id, 200).await;
            let updated = info.is_ok();
            let mut s = state.write().await;
            match info {
                Ok(info) => {
                    s.channel_info = Some(info);
                    match members {
                        Ok(members) => {
                            s.channel_members = members;
                            s.status = t!("app.channel_info").to_string();
                        }
                        Err(e) => {
                            s.channel_members.clear();
                            s.status =
                                t!("app.channel_info_members_unavailable", e = e).to_string();
                        }
                    }
                }
                Err(e) => {
                    s.channel_info = None;
                    s.channel_members.clear();
                    s.fail(t!("app.channel_info_unavailable", e = e).to_string());
                }
            }
            wake(&s);
            drop(s);
            // The (re)fetched channel metadata is a `channel.updated` for the mods.
            if updated {
                #[cfg(feature = "mods")]
                Self::mod_channel_event(
                    state.clone(),
                    crate::mods::events::Event::ChannelUpdated,
                    channel_id.clone(),
                );
            }
        });
    }

    /// Closes the "channel info" panel.
    pub fn close_channel_info(&self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut s = state.write().await;
            s.channel_info = None;
            s.channel_members.clear();
            s.pins.clear();
            wake(&s);
        });
    }

    /// Loads the "Later" list. The API returns references only, so each saved
    /// message is fetched by its ts (one request per item, capped).
    pub fn load_saved(&self) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.saved_loading = true;
                s.status = t!("app.loading_saved").to_string();
                wake(&s);
            }
            match Self::fetch_saved(&state, &api).await {
                Ok(n) => {
                    let mut s = state.write().await;
                    s.saved_loading = false;
                    s.status = t!("app.saved_loaded", n = n).to_string();
                    wake(&s);
                }
                Err(e) => {
                    let mut s = state.write().await;
                    s.saved_loading = false;
                    s.fail(t!("app.saved_unavailable", e = e).to_string());
                    wake(&s);
                }
            }
        });
    }

    /// Loads the "Threads" inbox.
    pub fn load_threads(&self) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.threads_loading = true;
                wake(&s);
            }
            let res = api.my_threads(40).await;
            let mut s = state.write().await;
            s.threads_loading = false;
            match res {
                Ok((items, unread)) => {
                    s.threads = items
                        .into_iter()
                        .filter_map(|t| {
                            let root = parse_message(&t.root)?;
                            let reply_count = t
                                .root
                                .get("reply_count")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            let replies =
                                t.replies.iter().filter_map(|v| parse_message(v)).collect();
                            Some(ThreadRow {
                                channel: t.channel,
                                root,
                                replies,
                                reply_count,
                            })
                        })
                        .collect();
                    s.threads_unread = unread;
                }
                Err(e) => {
                    s.fail(t!("app.threads_unavailable", e = e).to_string());
                }
            }
            wake(&s);
        });
    }

    /// Loads the messages that mention the user, newest first.
    pub fn load_mentions(&self) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.mentions_loading = true;
                wake(&s);
            }
            let res = api.search_messages("has:mention", 40).await;
            let mut s = state.write().await;
            s.mentions_loading = false;
            match res {
                Ok(hits) => s.mentions = Arc::new(hits),
                Err(e) => s.fail(t!("app.mentions_unavailable", e = e).to_string()),
            }
            wake(&s);
        });
    }

    /// Queues a message to be sent at `post_at` (Unix seconds). Slack's own
    /// scheduling needs a token we do not have, so this is kept locally and
    /// persisted.
    pub fn schedule_message(
        &self,
        channel: String,
        channel_name: String,
        text: String,
        post_at: i64,
    ) {
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            let snapshot = {
                let mut s = state.write().await;
                s.scheduled.push(ScheduledMessage {
                    id: uuid::Uuid::new_v4().to_string(),
                    channel,
                    channel_name,
                    text,
                    post_at,
                });
                s.status = t!("app.scheduled_one").to_string();
                wake(&s);
                s.scheduled.clone()
            };
            save_scheduled(&*store.lock().await, &snapshot);
        });
    }

    /// Cancels a queued message.
    pub fn cancel_scheduled(&self, id: String) {
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            let snapshot = {
                let mut s = state.write().await;
                s.scheduled.retain(|m| m.id != id);
                s.status = t!("app.scheduled_cancelled").to_string();
                wake(&s);
                s.scheduled.clone()
            };
            save_scheduled(&*store.lock().await, &snapshot);
        });
    }

    /// Sends queued messages once their time has come, for the life of the
    /// process. One scheduled while the app was closed is sent on the first
    /// tick after it starts again.
    pub fn spawn_scheduler(&self) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                let now = chrono::Utc::now().timestamp();
                let due: Vec<ScheduledMessage> = {
                    let s = state.read().await;
                    s.scheduled
                        .iter()
                        .filter(|m| m.post_at <= now)
                        .cloned()
                        .collect()
                };
                for m in due {
                    if let Err(e) = api.chat_post_message(&m.channel, &m.text).await {
                        tracing::warn!("scheduled message to {} failed: {e}", m.channel);
                    }
                    // Dropped whether it succeeded or not: a permanent failure
                    // (the channel is gone) must not be retried forever.
                    let snapshot = {
                        let mut s = state.write().await;
                        let who = m.channel_name.clone();
                        s.scheduled.retain(|x| x.id != m.id);
                        s.status = t!("app.scheduled_sent", who = who).to_string();
                        wake(&s);
                        s.scheduled.clone()
                    };
                    save_scheduled(&*store.lock().await, &snapshot);
                }
            }
        });
    }

    /// Fetches the saved list and the content of each item. Returns how many
    /// messages could be resolved.
    async fn fetch_saved(
        state: &Arc<RwLock<AppState>>,
        api: &crate::api::Client,
    ) -> anyhow::Result<usize> {
        let (items, counts) = api.saved_list(30).await?;
        let mut rows = Vec::with_capacity(items.len());
        for it in items {
            let message = match api.conversations_replies(&it.item_id, &it.ts).await {
                Ok(msgs) => msgs
                    .into_iter()
                    .find(|m| m.get("ts").and_then(|v| v.as_str()) == Some(it.ts.as_str()))
                    .and_then(|v| parse_message(&v)),
                Err(_) => None,
            };
            if let Some(message) = message {
                rows.push(SavedRow {
                    channel: it.item_id,
                    ts: it.ts,
                    state: it.state,
                    message,
                });
            }
        }
        let n = rows.len();
        let mut s = state.write().await;
        s.saved = rows;
        s.saved_counts = counts;
        Ok(n)
    }

    /// Saves a message for later, then refreshes the list.
    pub fn save_message(&self, channel: String, ts: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            match api.saved_add(&channel, &ts).await {
                Ok(()) => {
                    state.write().await.status = t!("app.saved_one").to_string();
                    let _ = Self::fetch_saved(&state, &api).await;
                }
                Err(e) => state
                    .write()
                    .await
                    .fail(t!("app.save_failed", e = e).to_string()),
            }
        });
    }

    /// Removes a message from the "Later" list.
    pub fn unsave_message(&self, channel: String, ts: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            match api.saved_delete(&channel, &ts).await {
                Ok(()) => {
                    state.write().await.status = t!("app.unsaved_one").to_string();
                    let _ = Self::fetch_saved(&state, &api).await;
                }
                Err(e) => state
                    .write()
                    .await
                    .fail(t!("app.unsave_failed", e = e).to_string()),
            }
        });
    }

    /// Pins a message to its channel, then refreshes the pin list.
    pub fn pin_message(&self, channel: String, ts: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            match api.pins_add(&channel, &ts).await {
                Ok(()) => {
                    state.write().await.status = t!("app.pinned").to_string();
                    Self::reload_pins(&channel, &state, &api).await;
                }
                Err(e) => state
                    .write()
                    .await
                    .fail(t!("app.pin_failed", e = e).to_string()),
            }
        });
    }

    /// Removes a pin.
    pub fn unpin_message(&self, channel: String, ts: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            match api.pins_remove(&channel, &ts).await {
                Ok(()) => {
                    state.write().await.status = t!("app.unpinned").to_string();
                    Self::reload_pins(&channel, &state, &api).await;
                }
                Err(e) => state
                    .write()
                    .await
                    .fail(t!("app.unpin_failed", e = e).to_string()),
            }
        });
    }

    /// Loads the open conversation's pins; they follow the conversation, so a
    /// switch clears the previous list and shows a spinner until the new one
    /// arrives.
    pub fn load_pins(&self, channel: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.pins.clear();
                s.pins_loading = true;
                wake(&s);
            }
            Self::reload_pins(&channel, &state, &api).await;
        });
    }

    async fn reload_pins(channel: &str, state: &Arc<RwLock<AppState>>, api: &crate::api::Client) {
        let items = api.pins_list(channel).await;
        let mut s = state.write().await;
        s.pins_loading = false;
        if let Ok(items) = items {
            s.pins = items
                .into_iter()
                .filter_map(|v| parse_message(&v))
                .collect();
        }
        wake(&s);
    }

    /// Adds or removes one of our reactions, depending on how it is now.
    pub fn toggle_reaction(&self, channel_id: String, ts: String, name: String, mine: bool) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            // Local state changes at once: waiting for the network would
            // make the click feel like it did nothing.
            {
                let mut s = state.write().await;
                let me = s.my_user_id.clone();
                // The message can be in the timeline, in the open thread, or in
                // both: both lists are updated.
                let AppState {
                    messages,
                    thread_messages,
                    ..
                } = &mut *s;
                for m in messages.iter_mut().chain(thread_messages.iter_mut()) {
                    if m.ts != ts {
                        continue;
                    }
                    match m.reactions.iter_mut().find(|r| r.name == name) {
                        Some(r) => {
                            r.mine = !mine;
                            r.count += if mine { -1 } else { 1 };
                            if mine {
                                r.users.retain(|u| *u != me);
                            } else {
                                r.users.push(me.clone());
                            }
                        }
                        None if !mine => m.reactions.push(Reaction {
                            name: name.clone(),
                            count: 1,
                            mine: true,
                            users: vec![me.clone()],
                        }),
                        None => {}
                    }
                    m.reactions.retain(|r| r.count > 0);
                }
                wake(&s);
            }

            let res = if mine {
                api.reactions_remove(&channel_id, &ts, &name).await
            } else {
                api.reactions_add(&channel_id, &ts, &name).await
            };
            if let Err(e) = res {
                state
                    .write()
                    .await
                    .fail(t!("app.reaction_failed", e = e).to_string());
            }
        });
    }

    /// Dispatches a click on an action button: it tells the app that owns the
    /// message which button was pressed, and the app replies over the
    /// WebSocket. The id of the app is the message's `bot_profile.app_id` when
    /// it is there; otherwise it is asked of `bots.info`.
    pub fn trigger_block_action(
        &self,
        channel_id: String,
        message_ts: String,
        action: ActionButton,
        bot_id: Option<String>,
        app_id: Option<String>,
    ) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut app = app_id;
            if app.is_none() {
                if let Some(id) = &bot_id {
                    match api.bot_app_id(id).await {
                        Ok(found) if !found.is_empty() => app = Some(found),
                        _ => {}
                    }
                }
            }
            let service_id = bot_id.clone().unwrap_or_default();
            let team_id = state.read().await.team_id.clone();
            let actions = vec![action.raw.clone()];
            let res = api
                .blocks_actions(
                    &service_id,
                    app.as_deref(),
                    &team_id,
                    &channel_id,
                    &message_ts,
                    &actions,
                )
                .await;
            let mut s = state.write().await;
            let (msg, failed) = match res {
                Ok(()) => (t!("app.action_sent").to_string(), false),
                Err(e) => (t!("app.action_failed", e = e).to_string(), true),
            };
            if failed {
                s.fail(msg);
            } else {
                s.status = msg;
            }
            wake(&s);
        });
    }

    /// Sends a message to a channel, or to a thread if `thread_ts` is there.
    pub fn send_message(&self, channel_id: String, text: String, thread_ts: Option<String>) {
        // A leading `/` is a slash command: Slack runs it server-side instead
        // of posting the text, and may answer with an ephemeral `response`.
        if let Some((command, args)) = split_slash_command(&text) {
            let api = self.api_client.clone();
            let state = self.state.clone();
            let channel = channel_id.clone();
            let (command, args) = (command.to_string(), args.to_string());
            tokio::spawn(async move {
                // A declared mod command takes precedence: it runs locally
                // instead of being sent to Slack.
                #[cfg(feature = "mods")]
                if Self::try_mod_command(&state, &command, &args, &channel).await {
                    return;
                }
                let res = api.chat_command(&channel, &command, &args).await;
                let mut s = state.write().await;
                match res {
                    Ok(Some(response)) if !response.is_empty() => s.status = response,
                    Ok(_) => {}
                    Err(e) => s.fail(t!("app.command_error", e = e).to_string()),
                }
                wake(&s);
            });
            return;
        }
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let sent = match &thread_ts {
                Some(ts) => api
                    .chat_post_message_thread(&channel_id, &text, ts)
                    .await
                    .map(|_| ()),
                None => api.chat_post_message(&channel_id, &text).await.map(|_| ()),
            };
            match sent {
                Err(e) => state
                    .write()
                    .await
                    .fail(t!("app.send_error", e = e).to_string()),
                // A reply in a thread does not appear in the timeline — it is
                // excluded by definition — so without reloading the thread the
                // message just written would look lost.
                Ok(()) => {
                    if let Some(ts) = thread_ts {
                        Self::reload_thread(channel_id, ts, &state, &api).await;
                    }
                }
            }
        });
    }

    async fn reload_thread(
        channel_id: String,
        thread_ts: String,
        state: &Arc<RwLock<AppState>>,
        api: &crate::api::Client,
    ) {
        match api.conversations_replies(&channel_id, &thread_ts).await {
            Ok(msgs) => {
                let mut s = state.write().await;
                // The thread may have been closed in the meantime.
                if s.thread_parent_ts.as_deref() != Some(thread_ts.as_str()) {
                    return;
                }
                s.thread_messages = msgs.iter().filter_map(parse_message).collect();
                s.status = t!(
                    "app.thread_replies",
                    n = s.thread_messages.len().saturating_sub(1)
                )
                .to_string();
                wake(&s);
                drop(s);
                spawn_resolve_bots(&state, &api);
                #[cfg(feature = "mods")]
                Self::run_mods(&state, &channel_id).await;
            }
            Err(e) => tracing::warn!("thread reload failed: {e}"),
        }
    }

    /// Searches the messages and puts the results into the state.
    pub fn search(&self, query: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.searching = true;
                s.search_query = query.clone();
                wake(&s);
            }
            let r = api.search_messages(&query, 40).await;
            let mut s = state.write().await;
            s.searching = false;
            match r {
                Ok(hits) => {
                    s.status = t!("app.search_results_count", n = hits.len()).to_string();
                    s.search_results = Arc::new(hits);
                }
                Err(e) => {
                    s.fail(t!("app.search_failed", e = e).to_string());
                    Arc::make_mut(&mut s.search_results).clear();
                }
            }
            wake(&s);
        });
    }

    /// Closes the results view.
    pub fn clear_search(&self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut s = state.write().await;
            Arc::make_mut(&mut s.search_results).clear();
            s.search_query.clear();
            wake(&s);
        });
    }

    /// The permanent link to a message, built locally by the client.
    pub fn permalink(&self, channel: &str, ts: &str, thread_ts: Option<&str>) -> String {
        self.api_client.permalink(channel, ts, thread_ts)
    }

    /// Forwards a message to another conversation.
    ///
    /// Slack has no dedicated method: you send the permalink, which the recipient
    /// sees expanded with author and text, exactly like the official client's
    /// forward.
    pub fn forward_message(
        &self,
        from_channel: String,
        ts: String,
        to_channel: String,
        comment: String,
    ) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let link = match api.chat_get_permalink(&from_channel, &ts).await {
                Ok(l) => l,
                Err(e) => {
                    let mut s = state.write().await;
                    s.fail(t!("app.forward_failed", e = e).to_string());
                    wake(&s);
                    return;
                }
            };
            let text = if comment.trim().is_empty() {
                link
            } else {
                format!("{comment}\n{link}")
            };
            let sent = api.chat_post_message(&to_channel, &text).await;
            let mut s = state.write().await;
            let (msg, failed) = match sent {
                Ok(_) => (t!("app.message_forwarded").to_string(), false),
                Err(e) => (t!("app.forward_failed", e = e).to_string(), true),
            };
            if failed {
                s.fail(msg);
            } else {
                s.status = msg;
            }
            wake(&s);
        });
    }

    /// Downloads an attachment to the downloads folder and opens it.
    ///
    /// Opening the permalink would send you to Slack's website: an attachment is
    /// something you want on your own disk, with the system application.
    pub fn download_and_open(&self, url: String, filename: String) {
        let state = self.state.clone();
        let xoxd = self.creds.xoxd.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.status = t!("app.downloading", filename = filename).to_string();
                wake(&s);
            }
            let progress = state.clone();
            let name = filename.clone();
            let mut last_tick = i64::MIN;
            let result = download::fetch_with(&url, &filename, &xoxd, move |done, total| {
                let tick = match total.filter(|t| *t > 0) {
                    Some(t) => (done * 100 / t) as i64,
                    None => (done / 262_144) as i64,
                };
                if tick == last_tick {
                    return;
                }
                last_tick = tick;
                if let Ok(mut s) = progress.try_write() {
                    s.status = match total.filter(|t| *t > 0) {
                        Some(t) => t!(
                            "app.downloading_percent",
                            name = name,
                            percent = done * 100 / t,
                            done = bytes_short(done)
                        )
                        .to_string(),
                        None => t!(
                            "app.downloading_bytes",
                            name = name,
                            done = bytes_short(done)
                        )
                        .to_string(),
                    };
                    wake(&s);
                }
            })
            .await;
            match result {
                Ok(path) => {
                    let msg = match download::open(&path) {
                        Ok(()) => t!("app.file_opened", path = path.display()).to_string(),
                        // The file is there anyway: we say where, instead of
                        // reporting only that opening it failed.
                        Err(e) => {
                            t!("app.saved_open_failed", path = path.display(), e = e).to_string()
                        }
                    };
                    let mut s = state.write().await;
                    s.status = msg;
                    wake(&s);
                }
                Err(e) => {
                    let mut s = state.write().await;
                    s.fail(
                        t!(
                            "app.download_failed",
                            filename = filename,
                            e = format!("{e:#}")
                        )
                        .to_string(),
                    );
                    wake(&s);
                }
            }
        });
    }

    /// Uploads a file to the channel, with an optional comment.
    pub fn upload(
        &self,
        channel_id: String,
        path: std::path::PathBuf,
        comment: String,
        thread_ts: Option<String>,
    ) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());

            {
                let mut s = state.write().await;
                s.status = t!("app.uploading", name = name).to_string();
                wake(&s);
            }

            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => {
                    let mut s = state.write().await;
                    s.fail(t!("app.file_read_failed", path = path.display(), e = e).to_string());
                    wake(&s);
                    return;
                }
            };

            match api
                .upload_file(
                    &channel_id,
                    &name,
                    bytes,
                    Some(&comment),
                    thread_ts.as_deref(),
                )
                .await
            {
                Ok(_) => {
                    let mut s = state.write().await;
                    s.status = t!("app.file_sent", name = name).to_string();
                    drop(s);
                    // The file appears as a new message: reload.
                    Self::load_channel_messages(channel_id, &state, &api, &store);
                }
                Err(e) => {
                    let mut s = state.write().await;
                    s.fail(t!("app.upload_failed", name = name, e = format!("{e:#}")).to_string());
                    wake(&s);
                }
            }
        });
    }

    /// Edits one of your own messages.
    ///
    /// The `message_changed` event Slack sends back updates the cache and the view
    /// by itself: here it is enough to send the request.
    pub fn edit_message(&self, channel_id: String, ts: String, text: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            if let Err(e) = api.chat_update(&channel_id, &ts, &text).await {
                let mut s = state.write().await;
                s.fail(t!("app.edit_failed", e = e).to_string());
                wake(&s);
            }
        });
    }

    /// Deletes one of your own messages.
    pub fn delete_message(&self, channel_id: String, ts: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            match api.chat_delete(&channel_id, &ts).await {
                Ok(()) => {
                    // It is removed from the cache and the view at once: the
                    // `message_deleted` event will arrive, but in the meantime the
                    // message would stay on screen.
                    store.lock().await.delete_message(&channel_id, &ts).ok();
                    let mut s = state.write().await;
                    s.messages.retain(|m| m.ts != ts);
                    s.thread_messages.retain(|m| m.ts != ts);
                    wake(&s);
                }
                Err(e) => {
                    let mut s = state.write().await;
                    s.fail(t!("app.delete_failed", e = e).to_string());
                    wake(&s);
                }
            }
        });
    }

    /// Loads messages older than those already in view.
    ///
    /// Callable on every frame: if one is already running, or if the start of the
    /// conversation has been reached, it does nothing.
    pub fn load_older(&self, channel_id: String) {
        // The preview has no history to fetch.
        if self.demo {
            return;
        }
        let api = self.api_client.clone();
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                if s.loading_older || s.reached_start {
                    return;
                }
                s.loading_older = true;
                s.status = t!("app.loading_older").to_string();
                wake(&s);
            }

            // `latest` is exclusive: we start from the oldest message on
            // screen. The cache's global oldest is wrong — the cache is sparse
            // and can hold messages far older than the window, so anchoring
            // there would skip the range in between instead of paging through
            // it, and a jump could never reach its target.
            let oldest = {
                let s = state.read().await;
                s.messages.first().map(|m| m.ts.clone())
            };
            let oldest = match oldest {
                Some(t) => Some(t),
                None => store.lock().await.oldest_ts(&channel_id).ok().flatten(),
            };
            let fetched = api
                .conversations_history_range(
                    &channel_id,
                    Some(INITIAL_WINDOW),
                    None,
                    oldest.as_deref(),
                )
                .await;

            let added = match fetched {
                Ok(msgs) => {
                    let n = msgs.len();
                    store
                        .lock()
                        .await
                        .upsert_messages(&channel_id, &msgs)
                        .unwrap_or(0);
                    n
                }
                Err(e) => {
                    let mut s = state.write().await;
                    s.loading_older = false;
                    s.fail(t!("app.load_failed", e = e).to_string());
                    wake(&s);
                    return;
                }
            };

            {
                let mut s = state.write().await;
                s.loading_older = false;
                // A single empty page can be a hiccup; only two in a row mean
                // the start of the conversation, so a transient empty response
                // does not stop backfilling for good.
                if added == 0 {
                    s.empty_loads = s.empty_loads.saturating_add(1);
                    if s.empty_loads >= 2 {
                        s.reached_start = true;
                        s.status = t!("app.start_of_conversation").to_string();
                    }
                } else {
                    s.empty_loads = 0;
                    s.window += added as u32;
                    s.status = t!("app.messages_count", n = s.window).to_string();
                }
            }
            Self::load_channel_messages(channel_id, &state, &api, &store);
        });
    }

    /// Selects a channel, loads its messages and marks it read.
    pub fn select_channel(&self, index: usize) {
        let state = self.state.clone();
        let api = self.api_client.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            Self::select_index(index, &state, &api, &store).await;
        });
    }

    /// Remembers the conversation to reopen at the next start.
    pub fn remember_channel(&self, channel_id: &str) {
        let store = self.store.clone();
        let id = channel_id.to_string();
        tokio::spawn(async move {
            store.lock().await.set_meta("last_channel", &id).ok();
        });
    }

    /// Like [`App::select_channel`], but by channel id: the notification
    /// centre knows the conversation's id, not its position in the sidebar.
    pub fn select_channel_by_id(&self, channel_id: &str) {
        let state = self.state.clone();
        let api = self.api_client.clone();
        let store = self.store.clone();
        let id = channel_id.to_string();
        tokio::spawn(async move {
            let index = {
                let s = state.read().await;
                match s.channels.iter().position(|c| c.id == id) {
                    Some(i) => i,
                    None => return,
                }
            };
            Self::select_index(index, &state, &api, &store).await;
        });
    }

    async fn select_index(
        index: usize,
        state: &Arc<RwLock<AppState>>,
        api: &crate::api::Client,
        store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    ) {
        let (id, changed) = {
            let mut s = state.write().await;
            if index >= s.channels.len() {
                return;
            }
            let prev = s.channels.get(s.selected_channel).map(|c| c.id.clone());
            s.selected_channel = index;
            // Every conversation starts again from the initial window.
            s.window = INITIAL_WINDOW;
            s.reached_start = false;
            s.empty_loads = 0;
            s.channel_info = None;
            s.channel_members.clear();
            // Do not leave the previous conversation's messages on screen while
            // the new one loads: showing them, then the cache, then the network
            // reply is what made the list flicker on every switch.
            s.messages.clear();
            s.messages_channel = None;
            // The badge disappears at once: waiting for Slack's answer
            // would make the count blink for a second.
            let channels = Arc::make_mut(&mut s.channels);
            channels[index].unread = false;
            channels[index].mentions = 0;
            let id = s.channels[index].id.clone();
            let changed = prev.as_deref() != Some(id.as_str());
            (id, changed)
        };
        #[cfg(not(feature = "mods"))]
        let _ = changed;
        store.lock().await.set_meta("last_channel", &id).ok();
        // Tell the mods a conversation was opened (only on an actual switch).
        #[cfg(feature = "mods")]
        if changed {
            Self::mod_channel_event(
                state.clone(),
                crate::mods::events::Event::ChannelOpened,
                id.clone(),
            );
        }
        Self::load_channel_messages(id.clone(), state, api, store);
        Self::mark_read(id, state, api, store).await;
    }

    /// Opens a notification: selects the conversation, marks its entries read
    /// and asks for the jump to the message, opening the thread when it is a
    /// reply.
    pub fn open_notification(&self, channel_id: String, ts: String, thread_ts: Option<String>) {
        // A thread reply opens its thread: no jump is queued for the main list,
        // or opening the thread would drag the conversation around.
        let is_thread = thread_ts.is_some();
        {
            let state = self.state.clone();
            let channel = channel_id.clone();
            let target = ts.clone();
            tokio::spawn(async move {
                let mut s = state.write().await;
                for n in s.notifications.iter_mut() {
                    if n.channel_id == channel {
                        n.read = true;
                    }
                }
                if !is_thread {
                    s.jump_to_ts = Some(target);
                    s.jump_channel = None;
                }
                wake(&s);
            });
        }
        self.select_channel_by_id(&channel_id);
        if let Some(parent) = thread_ts {
            self.open_thread(channel_id, parent);
        }
    }

    /// Marks every notification as read.
    pub fn mark_all_notifications_read(&self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut s = state.write().await;
            let mut changed = false;
            for n in s.notifications.iter_mut() {
                if !n.read {
                    n.read = true;
                    changed = true;
                }
            }
            if changed {
                wake(&s);
            }
        });
    }

    /// Clears the pending jump to a message, once the list has honoured it.
    pub fn clear_jump(&self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut s = state.write().await;
            if s.jump_to_ts.take().is_some() {
                s.jump_channel = None;
                wake(&s);
            }
        });
    }

    /// Drops a pending jump and the highlight that goes with it: the "abort"
    /// of an in-flight navigation.
    pub fn cancel_jump(&self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut s = state.write().await;
            s.jump_to_ts = None;
            s.jump_channel = None;
            s.status = t!("app.jump_cancelled").to_string();
            wake(&s);
        });
    }

    /// Selects a conversation and jumps to `ts` **in one step**, so the jump
    /// and the selection cannot race and page the previous conversation.
    pub fn jump_in_conversation(
        &self,
        channel_id: String,
        name: String,
        is_dm: bool,
        peer: Option<String>,
        ts: String,
    ) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            // A search hit can be a reply, and replies are not in the
            // conversation timeline; resolve the real root and, for a reply,
            // open its thread so the message is reachable at all.
            let root = match api.conversations_replies(&channel_id, &ts).await {
                Ok(msgs) => msgs
                    .iter()
                    .find(|m| m.get("ts").and_then(|v| v.as_str()) == Some(ts.as_str()))
                    .and_then(|m| m.get("thread_ts").and_then(|v| v.as_str()))
                    .filter(|t| *t != ts.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| ts.clone()),
                Err(_) => ts.clone(),
            };
            {
                let mut s = state.write().await;
                let index = ensure_channel(&mut s, channel_id.clone(), name, is_dm, peer);
                s.selected_channel = index;
                s.window = INITIAL_WINDOW;
                s.reached_start = false;
                s.empty_loads = 0;
                s.channel_info = None;
                s.channel_members.clear();
                // Drop the previous conversation's messages: the jump must not
                // run against them while the switch is still loading, or it
                // pages the wrong list and gives up.
                s.messages.clear();
                s.messages_channel = None;
                let channels = Arc::make_mut(&mut s.channels);
                channels[index].unread = false;
                channels[index].mentions = 0;
                s.jump_to_ts = Some(root.clone());
                s.jump_channel = Some(channel_id.clone());
                s.status = t!("app.jumping_to_message").to_string();
                wake(&s);
            }
            // Step 4: always open the root's thread in the side panel, next to
            // the conversation navigated to the message.
            store
                .lock()
                .await
                .set_meta("last_channel", &channel_id)
                .ok();
            Self::open_thread_with(channel_id.clone(), root, &state, &api);
            Self::load_channel_messages(channel_id, &state, &api, &store);
        });
    }

    /// Tells Slack how far we have read.
    ///
    /// The timestamp is taken from the **cache of that channel**, not from the
    /// messages on screen: during a change of conversation those still belong to
    /// the previous one, and marking them would move this one's read marker
    /// backwards. On top of that we never mark backwards.
    async fn mark_read(
        channel_id: String,
        state: &Arc<RwLock<AppState>>,
        api: &crate::api::Client,
        store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    ) {
        let newest = {
            let rows = store.lock().await.messages(&channel_id, 1);
            match rows {
                Ok(r) => r.last().map(|m| m.ts.clone()),
                Err(e) => {
                    tracing::debug!("cache unreadable for the read marker: {e}");
                    None
                }
            }
        };
        let Some(ts) = newest else { return };

        {
            let s = state.read().await;
            let known = s
                .channels
                .iter()
                .find(|c| c.id == channel_id)
                .map(|c| c.last_read.as_str())
                .unwrap_or("");
            if !is_newer(&ts, known) {
                return;
            }
        }

        if let Err(e) = api.conversations_mark(&channel_id, &ts).await {
            tracing::debug!("conversations.mark failed: {e}");
            return;
        }
        let mut s = state.write().await;
        if let Some(c) = Arc::make_mut(&mut s.channels)
            .iter_mut()
            .find(|c| c.id == channel_id)
        {
            c.last_read = ts;
        }
    }

    /// Starts the initial load: team id, users, channels and the first
    /// channel's messages. It returns at once, the state fills itself in.
    pub fn spawn_bootstrap(&self) {
        if !self.is_authenticated() {
            return;
        }
        // The scheduled-message queue is client-side: the loop that sends it
        // starts with the rest of the background work.
        self.spawn_scheduler();
        let me = self.clone();
        let bg_state = self.state.clone();
        let bg_api = self.api_client.clone();
        let bg_store = self.store.clone();
        let bg_creds = self.creds.clone();
        tokio::spawn(async move {
            // Keep the cache bounded: once, at startup.
            match bg_store.lock().await.prune_messages(3000) {
                Ok(n) if n > 0 => tracing::info!("cache: pruned {n} old messages"),
                Ok(_) => {}
                Err(e) => tracing::warn!("cache pruning failed: {e}"),
            }
            if bg_state.read().await.team_id.is_empty() {
                // The lock on the store is taken and released before the
                // await: the rusqlite connection is not `Sync`.
                let cached = bg_store.lock().await.get_meta("team_id").ok().flatten();
                match crate::huddle::resolve_team_id(&bg_creds, &bg_api, cached).await {
                    Ok(id) => {
                        bg_store.lock().await.set_meta("team_id", &id).ok();
                        bg_state.write().await.team_id = id;
                    }
                    Err(e) => tracing::warn!("team id not resolved: {e}"),
                }
            }

            // Our own id is needed to recognise mentions; like the team id
            // it is not in the stored credentials.
            if bg_state.read().await.my_user_id.is_empty() {
                let cached = bg_store.lock().await.get_meta("my_user_id").ok().flatten();
                let id = match cached.filter(|c| !c.is_empty()) {
                    Some(c) => Some(c),
                    None => match bg_api.auth_test().await {
                        Ok(a) => Some(a.user_id),
                        Err(e) => {
                            if is_auth_error(&e) {
                                let mut s = bg_state.write().await;
                                s.session_expired = true;
                                s.status = t!("app.session_invalid").to_string();
                                wake(&s);
                            }
                            None
                        }
                    },
                };
                if let Some(id) = id {
                    bg_store.lock().await.set_meta("my_user_id", &id).ok();
                    bg_state.write().await.my_user_id = id;
                }
            }

            // The users are loaded separately: `users.list` pages over the whole
            // workspace and can take minutes, while the channels are needed at once.
            // When they arrive, the DM names are corrected.
            {
                let u_api = bg_api.clone();
                let u_state = bg_state.clone();
                let u_store = bg_store.clone();
                tokio::spawn(async move {
                    let users = match u_api.users_list().await {
                        Ok(u) => u,
                        Err(e) => {
                            tracing::warn!("users not loaded: {e}");
                            return;
                        }
                    };
                    if let Err(e) = u_store.lock().await.upsert_users(&users) {
                        tracing::warn!("users not cached: {e}");
                    }
                    let mut s = u_state.write().await;
                    for u in users {
                        if !u.profile.image_72.is_empty() {
                            Arc::make_mut(&mut s.avatars)
                                .insert(u.id.clone(), u.profile.image_72.clone());
                        }
                        let name = u.full_name();
                        if !u.profile.status_emoji.is_empty() || !u.profile.status_text.is_empty() {
                            Arc::make_mut(&mut s.user_status).insert(
                                u.id.clone(),
                                UserStatus {
                                    emoji: u.profile.status_emoji,
                                    text: u.profile.status_text,
                                },
                            );
                        }
                        if u.is_bot || u.is_app_user {
                            Arc::make_mut(&mut s.bot_users).insert(u.id.clone());
                        }
                        Arc::make_mut(&mut s.usernames).insert(u.name.clone(), name.clone());
                        Arc::make_mut(&mut s.user_names).insert(u.id, name);
                    }
                    // DMs loaded before the users show the id: now they
                    // can be resolved.
                    let names = s.user_names.clone();
                    for c in Arc::make_mut(&mut s.channels).iter_mut() {
                        if c.is_dm {
                            if let Some(n) = names.get(&c.name) {
                                c.name = n.clone();
                            }
                        }
                    }
                    tracing::info!("{} users", s.user_names.len());
                });
            }

            match bg_api.list_channels(true).await {
                Ok(channels) => {
                    if let Err(e) = bg_store.lock().await.upsert_channels(&channels) {
                        tracing::warn!("could not cache the channels: {e}");
                    }
                    let mut s = bg_state.write().await;
                    // Slack shows far less in the sidebar than
                    // `users.conversations` returns: the `mpdm-…` groups pile up
                    // for years. `client.counts` says which are alive, and brings
                    // the unread state besides.
                    let counts = bg_api.client_counts().await.unwrap_or_default();
                    let active: std::collections::HashMap<String, (bool, i64)> = counts
                        .all()
                        .map(|c| (c.id.clone(), (c.has_unreads, c.mention_count)))
                        .collect();
                    // How far Slack has us reading: it is the reference that
                    // prevents marking backwards.
                    let reads: std::collections::HashMap<String, String> = counts
                        .all()
                        .map(|c| (c.id.clone(), c.last_read.clone()))
                        .collect();

                    let mut joined: Vec<ChannelEntry> = channels
                        .into_iter()
                        // `users.conversations` returns only the user's
                        // conversations and does not populate `is_member`: using it
                        // as a filter would remove every channel.
                        // If `client.counts` did not answer we keep everything:
                        // a long sidebar is better than an empty one.
                        .filter(|ch| {
                            !ch.is_archived && (active.is_empty() || active.contains_key(&ch.id))
                        })
                        .map(|ch| {
                            let is_dm = ch.is_im || ch.is_mpim;
                            // If the users have not arrived yet we keep the id:
                            // the users task replaces it with the name.
                            let name = if ch.is_im {
                                let uid = ch.user.clone().unwrap_or_default();
                                s.user_names.get(&uid).cloned().unwrap_or(uid)
                            } else if ch.is_mpim {
                                mpim_label(&ch.name, &s.usernames)
                            } else {
                                ch.name.clone()
                            };
                            let (unread, mentions) =
                                active.get(&ch.id).copied().unwrap_or((false, 0));
                            let user = if ch.is_im { ch.user.clone() } else { None };
                            // From `client.counts`: without it the first read mark
                            // would have no reference.
                            let last_read = reads.get(&ch.id).cloned().unwrap_or_default();
                            ChannelEntry {
                                external: ch.is_ext_shared,
                                id: ch.id,
                                name,
                                is_dm,
                                unread,
                                mentions,
                                user,
                                last_read,
                            }
                        })
                        .collect();
                    joined.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                    tracing::info!(
                        "{} active conversations out of {} in cache",
                        joined.len(),
                        active.len().max(joined.len())
                    );
                    // The channel that is open now belongs to the *old* list: keep
                    // its id to tell, after the refresh, whether it changed (the
                    // index is not comparable across lists).
                    let prev_selected = s.channels.get(s.selected_channel).map(|c| c.id.clone());
                    s.channels = Arc::new(joined);
                    s.status = t!("app.conversations_count", n = s.channels.len()).to_string();
                    let ids: Vec<String> = s.channels.iter().map(|c| c.id.clone()).collect();
                    let external: std::collections::HashSet<String> = s
                        .channels
                        .iter()
                        .filter(|c| c.external)
                        .map(|c| c.id.clone())
                        .collect();
                    drop(s);

                    // The unread conversations are already known: the centre
                    // can list what arrived while we were away.
                    seed_notifications(&bg_state, &bg_store).await;

                    // Now we know who appears in the sidebar, and therefore
                    // whose presence and quiet hours are worth following.
                    me.refresh_presence();

                    // The folders index the channel list, so they are loaded
                    // only once the list exists.
                    match bg_api.channel_sections().await {
                        Ok(secs) => {
                            let folders = sections::arrange(&secs, &ids, &external);
                            tracing::info!("{} folders", folders.len());
                            bg_state.write().await.folders = Arc::new(folders);
                        }
                        // Without folders the sidebar stays flat: it is a
                        // degradation, not an error to show.
                        Err(e) => tracing::warn!("folders not loaded: {e}"),
                    }

                    // If a specific channel was asked for we open that one,
                    // otherwise the first in the list.
                    let first = {
                        let s = bg_state.read().await;
                        s.initial_channel
                            .as_ref()
                            .and_then(|id| s.channels.iter().position(|c| &c.id == id))
                            .or(if s.channels.is_empty() { None } else { Some(0) })
                            .map(|i| (i, s.channels[i].id.clone()))
                    };
                    if let Some((idx, cid)) = first {
                        let changed = {
                            let mut s = bg_state.write().await;
                            s.selected_channel = idx;
                            prev_selected.as_deref() != Some(cid.as_str())
                        };
                        // A network refresh can open a different conversation than
                        // the cached one: the mods hear it (once).
                        #[cfg(feature = "mods")]
                        if changed {
                            Self::mod_channel_event(
                                bg_state.clone(),
                                crate::mods::events::Event::ChannelOpened,
                                cid.clone(),
                            );
                        }
                        #[cfg(not(feature = "mods"))]
                        let _ = changed;
                        Self::load_channel_messages(cid, &bg_state, &bg_api, &bg_store);
                    }
                }
                Err(e) => {
                    let mut s = bg_state.write().await;
                    if is_auth_error(&e) {
                        s.session_expired = true;
                        s.status = t!("app.session_invalid").to_string();
                    } else {
                        // The cached channels stay navigable: we only report that
                        // the refresh did not succeed.
                        let (msg, failed) = if s.channels.is_empty() {
                            (t!("app.error_loading_channels", e = e).to_string(), true)
                        } else {
                            (
                                t!("app.channels_offline", n = s.channels.len()).to_string(),
                                false,
                            )
                        };
                        if failed {
                            s.fail(msg);
                        } else {
                            s.status = msg;
                        }
                    }
                    wake(&s);
                }
            }
        });
    }

    /// Opens the DM with a user and selects it.
    ///
    /// If the conversation does not exist yet, `conversations.open` creates it;
    /// the channel is added to the list without waiting for a full reload, so
    /// writing to someone new is immediate.
    pub fn open_dm(&self, user_id: String, display_name: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            let channel_id = match api.conversations_open(&user_id).await {
                Ok(id) => id,
                Err(e) => {
                    state
                        .write()
                        .await
                        .fail(t!("app.dm_open_failed", e = e).to_string());
                    return;
                }
            };

            let index = {
                let mut s = state.write().await;
                match s.channels.iter().position(|c| c.id == channel_id) {
                    Some(i) => i,
                    None => {
                        Arc::make_mut(&mut s.channels).push(ChannelEntry {
                            id: channel_id.clone(),
                            name: display_name,
                            is_dm: true,
                            unread: false,
                            mentions: 0,
                            user: Some(user_id.clone()),
                            last_read: String::new(),
                            external: false,
                        });
                        // The folders index the list: appending at the end leaves
                        // them valid.
                        s.channels.len() - 1
                    }
                }
            };
            {
                // The same thing `select_channel` does: merely selecting would
                // leave the badge lit on a conversation that is open.
                let mut s = state.write().await;
                s.selected_channel = index;
                s.window = INITIAL_WINDOW;
                s.reached_start = false;
                if let Some(c) = Arc::make_mut(&mut s.channels).get_mut(index) {
                    c.unread = false;
                    c.mentions = 0;
                }
                wake(&s);
            }
            Self::load_channel_messages(channel_id.clone(), &state, &api, &store);
            Self::mark_read(channel_id, &state, &api, &store).await;
        });
    }

    /// Opens a `#channel` referenced in a message.
    ///
    /// When we are in the channel it is already in the sidebar: select it.
    /// Otherwise say so — joining on a stray click would be a surprise, and a
    /// private channel we are not in cannot be opened anyway.
    pub fn open_channel(&self, channel_id: String, label: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            let index = {
                let s = state.read().await;
                s.channels.iter().position(|c| c.id == channel_id)
            };
            match index {
                Some(i) => Self::select_index(i, &state, &api, &store).await,
                None => {
                    let mut s = state.write().await;
                    s.fail(t!("app.channel_not_joined", name = label).to_string());
                    wake(&s);
                }
            }
        });
    }

    /// Adds a conversation to the list when it is not there, selects it and
    /// loads it. Shared by "new channel" and "new group DM", which both end up
    /// opening something the list did not have.
    async fn add_and_select(
        id: String,
        name: String,
        is_dm: bool,
        user: Option<String>,
        state: &Arc<RwLock<AppState>>,
        api: &crate::api::Client,
        store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    ) {
        let index = {
            let mut s = state.write().await;
            match s.channels.iter().position(|c| c.id == id) {
                Some(i) => i,
                None => {
                    Arc::make_mut(&mut s.channels).push(ChannelEntry {
                        id: id.clone(),
                        name,
                        is_dm,
                        unread: false,
                        mentions: 0,
                        user,
                        last_read: String::new(),
                        external: false,
                    });
                    s.channels.len() - 1
                }
            }
        };
        {
            let mut s = state.write().await;
            s.selected_channel = index;
            s.window = INITIAL_WINDOW;
            s.reached_start = false;
            if let Some(c) = Arc::make_mut(&mut s.channels).get_mut(index) {
                c.unread = false;
                c.mentions = 0;
            }
            wake(&s);
        }
        Self::load_channel_messages(id.clone(), state, api, store);
        Self::mark_read(id, state, api, store).await;
    }

    /// Creates a channel and opens it.
    pub fn create_channel(&self, name: String, is_private: bool) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            match api.create_channel(&name, is_private).await {
                Ok((id, created)) => {
                    let name = if created.is_empty() { name } else { created };
                    Self::add_and_select(id, name, false, None, &state, &api, &store).await;
                    state.write().await.status = t!("app.channel_created").to_string();
                }
                Err(e) => {
                    state.write().await.status = t!("app.channel_create_failed", e = e).to_string();
                }
            }
        });
    }

    /// Opens a group DM with the chosen people, creating it when it does not
    /// exist yet.
    pub fn create_group_dm(&self, user_ids: Vec<String>) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            let name = {
                let s = state.read().await;
                user_ids
                    .iter()
                    .map(|u| s.user_names.get(u).cloned().unwrap_or_else(|| u.clone()))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            match api.open_conversation(&user_ids).await {
                Ok(id) => {
                    Self::add_and_select(id, name, true, None, &state, &api, &store).await;
                    state.write().await.status = t!("app.group_opened").to_string();
                }
                Err(e) => {
                    state
                        .write()
                        .await
                        .fail(t!("app.group_open_failed", e = e).to_string());
                }
            }
        });
    }

    /// Renames a channel, updating the list and the open info panel.
    pub fn rename_channel(&self, channel_id: String, name: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            match api.rename_channel(&channel_id, &name).await {
                Ok(()) => {
                    let mut s = state.write().await;
                    if let Some(c) = Arc::make_mut(&mut s.channels)
                        .iter_mut()
                        .find(|c| c.id == channel_id)
                    {
                        c.name = name.clone();
                    }
                    if let Some(info) = s.channel_info.as_mut().filter(|i| i.id == channel_id) {
                        info.name = name.clone();
                    }
                    s.status = t!("app.channel_renamed").to_string();
                    wake(&s);
                }
                Err(e) => {
                    state.write().await.status = t!("app.channel_rename_failed", e = e).to_string();
                }
            }
        });
    }

    /// Sets a channel's topic, updating the open info panel.
    pub fn set_channel_topic(&self, channel_id: String, topic: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            match api.set_channel_topic(&channel_id, &topic).await {
                Ok(()) => {
                    let mut s = state.write().await;
                    if let Some(info) = s.channel_info.as_mut().filter(|i| i.id == channel_id) {
                        info.topic.value = Some(topic.clone());
                    }
                    s.status = t!("app.channel_topic_saved").to_string();
                    wake(&s);
                }
                Err(e) => {
                    state.write().await.status = t!("app.channel_topic_failed", e = e).to_string();
                }
            }
        });
    }

    /// Invites people to a channel and reloads its info panel (the member
    /// count changed).
    pub fn invite_to_channel(&self, channel_id: String, user_ids: Vec<String>) {
        let me = self.clone();
        tokio::spawn(async move {
            match me
                .api_client
                .invite_to_channel(&channel_id, &user_ids)
                .await
            {
                Ok(()) => {
                    me.state.write().await.status = t!("app.invited").to_string();
                    me.open_channel_info(channel_id);
                }
                Err(e) => {
                    me.state
                        .write()
                        .await
                        .fail(t!("app.invite_failed", e = e).to_string());
                }
            }
        });
    }

    /// Removes a person from a channel and reloads its info panel (the member
    /// count changed).
    pub fn kick_from_channel(&self, channel_id: String, user_id: String) {
        let me = self.clone();
        tokio::spawn(async move {
            match me.api_client.kick_from_channel(&channel_id, &user_id).await {
                Ok(()) => {
                    me.state.write().await.status = t!("app.member_removed").to_string();
                    me.open_channel_info(channel_id);
                }
                Err(e) => {
                    me.state
                        .write()
                        .await
                        .fail(t!("app.kick_failed", e = e).to_string());
                }
            }
        });
    }

    /// Leaves a channel: it disappears from the list.
    pub fn leave_channel(&self, channel_id: String) {
        let me = self.clone();
        tokio::spawn(async move {
            match me.api_client.leave_channel(&channel_id).await {
                Ok(()) => {
                    {
                        let mut s = me.state.write().await;
                        s.channels = Arc::new(
                            s.channels
                                .iter()
                                .filter(|c| c.id != channel_id)
                                .cloned()
                                .collect(),
                        );
                        if s.channel_info.as_ref().is_some_and(|i| i.id == channel_id) {
                            s.channel_info = None;
                        }
                        if s.selected_channel >= s.channels.len() {
                            s.selected_channel = s.channels.len().saturating_sub(1);
                        }
                        s.status = t!("app.left_channel").to_string();
                        wake(&s);
                    }
                    // The folders index the list: rebuild them around it.
                    me.spawn_sections();
                }
                Err(e) => {
                    me.state
                        .write()
                        .await
                        .fail(t!("app.leave_failed", e = e).to_string());
                }
            }
        });
    }

    /// Reloads the sidebar folders.
    ///
    /// At startup [`App::spawn_bootstrap`] takes care of it, fetching them after
    /// the channels: the folders contain indices into that list, and loading them
    /// first would give an empty sidebar.
    pub fn spawn_sections(&self) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let secs = match api.channel_sections().await {
                Ok(s) => s,
                Err(e) => {
                    // Without sections the sidebar stays flat: it is a
                    // degradation, not an error to show.
                    tracing::warn!("folders not loaded: {e}");
                    return;
                }
            };
            let mut s = state.write().await;
            let ids: Vec<String> = s.channels.iter().map(|c| c.id.clone()).collect();
            let external: std::collections::HashSet<String> = s
                .channels
                .iter()
                .filter(|c| c.external)
                .map(|c| c.id.clone())
                .collect();
            s.folders = Arc::new(sections::arrange(&secs, &ids, &external));
            tracing::info!("{} folders", s.folders.len());
        });
    }

    /// Opens or closes a folder.
    pub fn toggle_folder(&self, id: String) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut s = state.write().await;
            if !Arc::make_mut(&mut s.collapsed).insert(id.clone()) {
                Arc::make_mut(&mut s.collapsed).remove(&id);
            }
        });
    }

    /// Stars or unstars a conversation, saving the new list to the cache.
    pub fn toggle_favorite(&self, channel_id: String) {
        let state = self.state.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            let favorites = {
                let mut s = state.write().await;
                {
                    let favorites = Arc::make_mut(&mut s.favorites);
                    if favorites.contains(&channel_id) {
                        favorites.remove(&channel_id);
                        s.status = t!("app.favorite_removed").to_string();
                    } else {
                        favorites.insert(channel_id.clone());
                        s.status = t!("app.favorite_added").to_string();
                    }
                }
                wake(&s);
                s.favorites.clone()
            };
            let store = store.lock().await;
            save_favorites(&store, &favorites);
        });
    }

    /// Loads the workspace's custom emoji.
    ///
    /// Separate from the bootstrap because it is not needed to draw the first
    /// screen: if it fails you see the textual `:names:`, nothing breaks.
    pub fn spawn_emoji(&self) {
        if !self.is_authenticated() {
            return;
        }
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            match api.emoji_list().await {
                Ok(map) => {
                    tracing::info!("{} custom emoji", map.len());
                    state.write().await.emoji = Arc::new(map);
                }
                Err(e) => tracing::warn!("custom emoji not loaded: {e}"),
            }
        });
    }

    /// Starts listening for real-time events, with reconnection.
    ///
    /// It returns the channel they arrive on: the front end passes them to
    /// [`App::apply_ws_update`] whenever it suits.
    pub fn spawn_ws_listener(&self) -> tokio::sync::mpsc::Receiver<crate::ws::WsUpdate> {
        let (ws_tx, ws_rx) = tokio::sync::mpsc::channel(256);
        let ws_creds = self.creds.clone();
        let presence = self.presence_tx.subscribe();
        tokio::spawn(async move {
            crate::ws::listen_forever(ws_creds, None, ws_tx, Some(presence)).await;
        });
        ws_rx
    }

    /// Starts listening and applies the events to the state itself.
    ///
    /// Convenient for a synchronous front end like the GUI, which has no async
    /// loop to intercept them one by one in.
    pub fn spawn_ws_pump(&self) {
        if !self.is_authenticated() {
            return;
        }
        let mut rx = self.spawn_ws_listener();
        let state = self.state.clone();
        let store = self.store.clone();
        let api = self.api_client.clone();
        tokio::spawn(async move {
            while let Some(update) = rx.recv().await {
                Self::apply_ws_update(update, &state, &store, &api).await;
            }
        });
    }

    /// Joins the call on the open channel, with native audio.
    ///
    /// It replaces opening the browser: the huddle happens in here. The browser
    /// remains the fallback for screen sharing, which we cannot do yet.
    pub fn start_call(&self) {
        self.start_call_in(None);
    }

    /// Joins the call of a specific channel, or of the selected one.
    ///
    /// Answering an incoming call must not force you to move to the right
    /// conversation first: you just answer.
    pub fn start_call_in(&self, channel: Option<String>) {
        let state = self.state.clone();
        let creds = self.creds.clone();
        tokio::spawn(async move {
            let channel = match channel {
                Some(c) => c,
                None => {
                    let s = state.read().await;
                    match s.channels.get(s.selected_channel) {
                        Some(c) => c.id.clone(),
                        None => return,
                    }
                }
            };
            // It must not keep ringing while we join it.
            if let Some(h) = state.write().await.huddles.get_mut(&channel) {
                h.ringing = false;
            }
            if state.read().await.call.is_some() {
                return;
            }
            state.write().await.status = t!("app.call_starting").to_string();
            match crate::calls::live::Call::join(creds, channel, Default::default()).await {
                Ok(call) => {
                    let mut s = state.write().await;
                    s.call = Some(Arc::new(call));
                    s.status = t!("app.in_call").to_string();
                    wake(&s);
                }
                Err(e) => {
                    let mut s = state.write().await;
                    s.fail(t!("app.call_start_failed", e = format!("{e:#}")).to_string());
                    wake(&s);
                }
            }
        });
    }

    /// Hangs up, if there is a call in progress.
    pub fn hang_up(&self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            // We do not throw the call away here: we let its task close it
            // and forget it when it says it has finished. Removing it at once
            // destroyed the command channel just used.
            {
                let s = state.read().await;
                if let Some(c) = s.call.as_ref() {
                    c.hang_up();
                }
            }
            {
                let mut s = state.write().await;
                s.status = t!("app.hanging_up").to_string();
                wake(&s);
            }
            // We wait for the confirmation, without hanging if it never comes.
            for _ in 0..30 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let done = state
                    .read()
                    .await
                    .call
                    .as_ref()
                    .and_then(|c| c.status())
                    .map(|st| st.finished)
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
            let mut s = state.write().await;
            s.call = None;
            s.status = t!("app.call_ended").to_string();
            wake(&s);
        });
    }

    /// Toggles the mute of the call in progress.
    pub fn toggle_call_mute(&self) {
        let state = self.state.clone();
        tokio::spawn(async move {
            if let Some(c) = state.read().await.call.as_ref() {
                c.toggle_mute();
            }
        });
    }

    /// Starts or stops publishing our screen into the call in progress.
    pub fn toggle_call_screen_share(&self, display_id: Option<u32>) {
        let state = self.state.clone();
        tokio::spawn(async move {
            if let Some(c) = state.read().await.call.as_ref() {
                c.toggle_screen_share(display_id);
            }
        });
    }

    /// Follows the presence and quiet hours of the people in the sidebar.
    ///
    /// Only them: the workspace has hundreds of users, but only a few dozen fit on
    /// screen, and asking for the presence of people you cannot see would be
    /// wasted traffic. It has to be called again when the list changes.
    pub fn refresh_presence(&self) {
        let state = self.state.clone();
        let api = self.api_client.clone();
        let tx = self.presence_tx.clone();
        tokio::spawn(async move {
            let (ids, dnd_ids): (Vec<String>, Vec<String>) = {
                let s = state.read().await;
                let mut dnd: Vec<String> = s
                    .channels
                    .iter()
                    .filter(|c| c.is_dm)
                    .filter_map(|c| c.user.clone())
                    .filter(|u| !u.is_empty())
                    .collect();
                dnd.sort();
                dnd.dedup();
                // Ourselves too: the server's verdict on our **own** presence is
                // what the read-back logs. `dnd_team_info` gets the others only —
                // our own quiet-hours schedule would otherwise turn our avatar into
                // "quiet" instead of online.
                let mut ids = dnd.clone();
                if !s.my_user_id.is_empty() {
                    ids.push(s.my_user_id.clone());
                    ids.sort();
                    ids.dedup();
                }
                (ids, dnd)
            };
            if ids.is_empty() {
                return;
            }

            // `send_if_modified` avoids resending `presence_sub` when the
            // sidebar is rebuilt identical to before.
            tx.send_if_modified(|cur| {
                if *cur == ids {
                    false
                } else {
                    *cur = ids.clone();
                    true
                }
            });

            // Quiet hours change rarely: they are read once, not followed in
            // real time.
            match api.dnd_team_info(&dnd_ids).await {
                Ok(dnd) => {
                    let mut s = state.write().await;
                    s.dnd = Arc::new(dnd);
                    wake(&s);
                }
                Err(e) => tracing::debug!("quiet hours not loaded: {e}"),
            }
        });
    }

    /// Sets our status (`users.profile.set`), updating the interface at once
    /// and rolling the message back to the error if Slack rejects it.
    pub fn set_status(&self, emoji: String, text: String) {
        self.apply_status(emoji, text, t!("app.status_updated").to_string());
    }

    /// Clears our status.
    pub fn clear_status(&self) {
        self.apply_status(
            String::new(),
            String::new(),
            t!("app.status_cleared").to_string(),
        );
    }

    fn apply_status(&self, emoji: String, text: String, message: String) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let me = state.read().await.my_user_id.clone();
            if !me.is_empty() {
                let mut s = state.write().await;
                Arc::make_mut(&mut s.user_status).insert(
                    me,
                    UserStatus {
                        emoji: emoji.clone(),
                        text: text.clone(),
                    },
                );
                s.status = message;
                wake(&s);
            }
            if let Err(e) = api.users_profile_set(&emoji, &text).await {
                let mut s = state.write().await;
                s.fail(t!("app.status_update_failed", e = e).to_string());
                wake(&s);
            }
        });
    }

    /// Switches presence: `true` active, `false` away.
    pub fn set_presence(&self, present: bool) {
        self.set_presence_inner(present, false);
    }

    /// The automatic keepalive (see the GUI's `drive_presence`): the same call,
    /// but it leaves the transient status line alone — "Active" flashing every
    /// couple of minutes is just noise.
    pub fn keepalive_presence(&self) {
        self.set_presence_inner(true, true);
    }

    fn set_presence_inner(&self, present: bool, quiet: bool) {
        let api = self.api_client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let me = state.read().await.my_user_id.clone();
            if !me.is_empty() {
                let mut s = state.write().await;
                Arc::make_mut(&mut s.presence).insert(me, present);
                if !quiet {
                    s.status = if present {
                        t!("app.presence_active").to_string()
                    } else {
                        t!("app.presence_away").to_string()
                    };
                }
                wake(&s);
            }
            let value = if present { "auto" } else { "away" };
            match api.users_set_presence(value).await {
                Ok(()) => {
                    // Read it back: `auto` is **not** a force-active (it clears a
                    // manual away and hands the rest to Slack), so what Slack
                    // reports right after is the evidence — log it instead of
                    // assuming the account is now online.
                    match api.users_get_presence().await {
                        Ok(now) => tracing::info!("presence -> {value}, now {now}"),
                        Err(e) => tracing::info!("presence -> {value}, read-back failed: {e}"),
                    }
                }
                Err(e) => {
                    tracing::warn!("presence -> {value} failed: {e}");
                    let mut s = state.write().await;
                    s.fail(t!("app.presence_update_failed", e = e).to_string());
                    wake(&s);
                }
            }
        });
    }

    /// Logs out: removes the stored session and restarts the process, which
    /// re-runs the auth gate and comes back to the login screen.
    pub fn logout(&self) {
        let workspace = self.creds.workspace_url.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            {
                let mut s = state.write().await;
                s.status = t!("app.logging_out").to_string();
                wake(&s);
            }
            if let Err(e) = crate::auth::delete_credentials(&workspace) {
                tracing::warn!("credentials deletion failed: {e}");
            }
            let restart = std::env::current_exe().and_then(|exe| {
                std::process::Command::new(&exe)
                    .args(std::env::args_os().skip(1))
                    .spawn()
            });
            match restart {
                Ok(_) => std::process::exit(0),
                Err(e) => {
                    let mut s = state.write().await;
                    s.fail(t!("app.logout_restart", e = e).to_string());
                    wake(&s);
                }
            }
        });
    }

    /// Registers the function to call when the state changes.
    pub fn set_waker(&self, f: Arc<dyn Fn() + Send + Sync>) {
        let state = self.state.clone();
        tokio::spawn(async move {
            state.write().await.waker = Some(f);
        });
    }

    /// The shared state, which the front ends read in order to draw.
    pub fn state(&self) -> &Arc<RwLock<AppState>> {
        &self.state
    }

    pub fn store(&self) -> &Arc<tokio::sync::Mutex<crate::store::Store>> {
        &self.store
    }

    pub fn api(&self) -> &crate::api::Client {
        &self.api_client
    }

    pub fn creds(&self) -> &Credentials {
        &self.creds
    }
}

/// Translates the mentions chosen from the popup into the form Slack
/// understands.
///
/// In the composer you write `@Name Surname`: readable, and with the cursor
/// ending up where you expect. Only on send does it become `<@U123>`.
///
/// `chosen` holds the *exact* text the popup inserted alongside its Slack
/// form, so the substitution is literal: no attempt is made to guess who a
/// hand-written `@name` refers to. If the user edits or deletes what the popup
/// inserted, the match fails and it stays plain text — preferable to
/// mentioning the wrong person.
///
/// The longer entries have to be substituted first: `@Marco` is a prefix of
/// `@Marco Ferretti`, and in the opposite order the second would break.
pub fn resolve_mentions(text: &str, chosen: &[(String, String)]) -> String {
    let mut subs: Vec<&(String, String)> = chosen.iter().collect();
    subs.sort_by_key(|(visible, _)| std::cmp::Reverse(visible.len()));

    let mut out = text.to_string();
    for (visible, slack) in subs {
        out = out.replace(visible.as_str(), slack);
    }
    out
}

/// Splits a leading slash command into (`/command`, arguments).
///
/// Returns `None` when `text` is not a command, so a bare `/` and a path such
/// as `/usr/bin` are sent as ordinary messages instead of being run.
/// The declared mod command a slash-prefixed `command` (`/name`) refers to, if
/// any. `split_slash_command` keeps the leading `/`; declared names never have
/// one, so it is stripped before matching.
#[cfg(feature = "mods")]
fn mod_command_name<'a>(commands: &'a [ModCommand], command: &str) -> Option<&'a str> {
    let name = command.strip_prefix('/').unwrap_or(command);
    commands
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.name.as_str())
}

pub fn split_slash_command(text: &str) -> Option<(&str, &str)> {
    let t = text.trim_start();
    let after = t.strip_prefix('/')?;
    let (word, args) = match after.find(char::is_whitespace) {
        Some(i) => (&after[..i], after[i..].trim_start()),
        None => (after, ""),
    };
    // The command is one word: another slash means it is a path, not a command.
    if word.is_empty() || word.contains('/') {
        return None;
    }
    Some((&t[..1 + word.len()], args))
}

#[cfg(test)]
mod slash_command_tests {
    use super::split_slash_command;

    #[test]
    fn a_command_is_split_from_its_arguments() {
        assert_eq!(
            split_slash_command("/remind me in 1h"),
            Some(("/remind", "me in 1h"))
        );
        assert_eq!(split_slash_command("/giphy  cat"), Some(("/giphy", "cat")));
        assert_eq!(split_slash_command("  /status"), Some(("/status", "")));
        assert_eq!(split_slash_command("/me waves"), Some(("/me", "waves")));
    }

    #[test]
    fn things_that_are_not_commands_are_left_alone() {
        // A slash in the middle, a bare slash and a path are ordinary text.
        assert_eq!(split_slash_command("hello /remind"), None);
        assert_eq!(split_slash_command("/"), None);
        assert_eq!(split_slash_command("/ "), None);
        assert_eq!(split_slash_command("/usr/bin/env"), None);
        assert_eq!(split_slash_command("https://example.com/a"), None);
    }

    /// A mod command is declared without the leading `/`, while
    /// `split_slash_command` keeps it, so the interception must strip it.
    #[test]
    fn a_mod_command_name_has_no_leading_slash() {
        let (command, args) = split_slash_command("/ping hello").unwrap();
        assert_eq!(command, "/ping");
        assert_eq!(args, "hello");
        assert_eq!(command.strip_prefix('/'), Some("ping"));
    }
}

#[cfg(all(test, feature = "mods"))]
mod mod_visibility_tests {
    use super::App;

    #[test]
    fn own_messages_are_hidden_from_mods() {
        // A mod's own post comes back authored by us: it must be filtered, or a
        // reply-to-everything mod re-triggers itself.
        assert!(!App::visible_to_mods("U_ME", "U_ME"));
        assert!(App::visible_to_mods("U_OTHER", "U_ME"));
        // An unresolved self filters nothing (not reachable once bootstrapped).
        assert!(App::visible_to_mods("U_X", ""));
    }
}

#[cfg(all(test, feature = "mods"))]
mod mod_command_tests {
    use super::{ModCommand, mod_command_name};

    fn declared(name: &str) -> ModCommand {
        ModCommand {
            mod_id: "dev.slaq.x".into(),
            mod_name: "x".into(),
            name: name.into(),
            label: name.into(),
            description: String::new(),
        }
    }

    /// The interception matches a `/name` typed in a composer against the bare
    /// declared name (the leading `/` is stripped).
    #[test]
    fn matching_strips_the_leading_slash() {
        let cmds = vec![declared("ping")];
        assert_eq!(mod_command_name(&cmds, "/ping"), Some("ping"));
        assert_eq!(mod_command_name(&cmds, "ping"), Some("ping"));
        assert_eq!(mod_command_name(&cmds, "/nope"), None);
        assert_eq!(mod_command_name(&[], "/ping"), None);
    }
}

#[cfg(test)]
mod blocks_tests {
    use super::{blocks_text, parse_message};
    use serde_json::json;

    #[test]
    fn bots_and_people_are_told_apart() {
        use super::{Unresolved, unresolved};
        use std::collections::HashMap;
        let known: HashMap<String, String> = HashMap::new();
        assert_eq!(unresolved("B0EXAMPLE01", &known), Some(Unresolved::Bot));
        // People have U or W, and have to be asked of `users.info`, not
        // `bots.info`.
        assert_eq!(unresolved("U01ABCDEF", &known), Some(Unresolved::Person));
        assert_eq!(unresolved("W12345", &known), Some(Unresolved::Person));
    }

    #[test]
    fn a_name_is_not_an_id() {
        use super::unresolved;
        use std::collections::HashMap;
        let known: HashMap<String, String> = HashMap::new();
        // A real name starting with B or U is not to be asked of anyone.
        assert_eq!(unresolved("Bruno Rossi", &known), None);
        assert_eq!(unresolved("Umberto", &known), None);
        assert_eq!(unresolved("?", &known), None);
    }

    #[test]
    fn an_external_person_is_asked_for() {
        use super::{Unresolved, unresolved};
        use std::collections::HashMap;
        // Slack Connect: someone writing from another organisation does not
        // appear in `users.list`, so they are left to resolve like anyone else.
        let known: HashMap<String, String> = HashMap::new();
        assert_eq!(unresolved("U09ESTERNO1", &known), Some(Unresolved::Person));
    }

    #[test]
    fn who_is_already_known_is_not_asked_again() {
        use super::unresolved;
        let known = [("B0EXAMPLE01".to_string(), "example-bot".to_string())]
            .into_iter()
            .collect();
        assert_eq!(unresolved("B0EXAMPLE01", &known), None);
    }

    #[test]
    fn a_composed_block_wins_over_the_flattened_fallback() {
        use super::display_text;
        // The GitHub message: in `text` the PR title is plain text, in the
        // blocks it is a link. The link is the reason for the message.
        let blocks = json!([{"type":"section","text":{"type":"mrkdwn",
            "text":"ha chiesto la tua review su <https://github.com/x/pull/20032|feat: qualcosa>"}}]);
        let flat = "ha chiesto la tua review su feat: qualcosa";
        assert_eq!(
            display_text(flat, Some(&blocks)).as_deref(),
            Some("ha chiesto la tua review su <https://github.com/x/pull/20032|feat: qualcosa>")
        );
    }

    #[test]
    fn what_a_person_typed_is_read_from_text() {
        use super::display_text;
        // `rich_text` is the rendering of what someone wrote, and the
        // corresponding `text` already has all the markup: it is the tested road.
        let blocks = json!([{"type":"rich_text","elements":[
            {"type":"rich_text_section","elements":[{"type":"text","text":"ciao"}]}
        ]}]);
        assert_eq!(display_text("ciao", Some(&blocks)), None);
    }

    #[test]
    fn with_no_text_at_all_the_blocks_are_used_whatever_they_are() {
        use super::display_text;
        let blocks = json!([{"type":"rich_text","elements":[
            {"type":"rich_text_section","elements":[{"type":"text","text":"solo blocchi"}]}
        ]}]);
        assert_eq!(
            display_text("", Some(&blocks)).as_deref(),
            Some("solo blocchi")
        );
    }

    #[test]
    fn empty_or_missing_blocks_change_nothing() {
        use super::display_text;
        assert_eq!(display_text("ciao", None), None);
        assert_eq!(display_text("ciao", Some(&json!([]))), None);
        assert_eq!(
            display_text("ciao", Some(&json!([{"type":"divider"}]))),
            None
        );
    }

    #[test]
    fn section_text_is_taken() {
        // This is the real CI bot case: `text` empty, everything in the blocks.
        let b = json!([
            {"type":"section","text":{"type":"mrkdwn","text":"*Branch*: develop"}},
            {"type":"section","text":{"type":"mrkdwn","text":"*Stato*: failure"}},
        ]);
        assert_eq!(blocks_text(&b), "*Branch*: develop\n*Stato*: failure");
    }

    #[test]
    fn fields_become_lines() {
        let b = json!([{"type":"section","fields":[
            {"type":"mrkdwn","text":"uno"},
            {"type":"mrkdwn","text":"due"}
        ]}]);
        assert_eq!(blocks_text(&b), "uno\ndue");
    }

    #[test]
    fn context_elements_stay_on_one_line() {
        let b = json!([{"type":"context","elements":[
            {"type":"mrkdwn","text":"build 42"},
            {"type":"plain_text","text":"3 min"}
        ]}]);
        assert_eq!(blocks_text(&b), "build 42 3 min");
    }

    #[test]
    fn blocks_without_text_are_skipped() {
        // Dividers, buttons and images have no textual rendering.
        let b = json!([
            {"type":"divider"},
            {"type":"image","image_url":"https://x.test/a.png"},
            {"type":"actions","elements":[{"type":"button","text":{"type":"plain_text","text":"Apri"}}]},
            {"type":"section","text":{"type":"mrkdwn","text":"vero"}}
        ]);
        assert_eq!(blocks_text(&b), "vero");
    }

    #[test]
    fn action_buttons_are_parsed_in_order() {
        let v = json!({
            "ts": "1.0",
            "bot_id": "B0EXAMPLE02",
            "bot_profile": { "app_id": "A0EXAMPLE03" },
            "text": "",
            "blocks": [
                { "type": "section", "text": { "type": "mrkdwn", "text": "ciao" } },
                { "type": "actions", "block_id": "actions", "elements": [
                    { "type": "button", "action_id": "add_car",
                      "text": { "type": "plain_text", "text": "Add :blue_car:", "emoji": true },
                      "value": "add_car_6" },
                    { "type": "button", "action_id": "docs",
                      "text": { "type": "plain_text", "text": "Docs" },
                      "url": "https://example.test/docs", "style": "primary" }
                ] },
                { "type": "actions", "block_id": "more", "elements": [
                    { "type": "button", "action_id": "del",
                      "text": { "type": "plain_text", "text": "Elimina" },
                      "value": "x", "style": "danger" }
                ] }
            ]
        });
        let m = parse_message(&v).unwrap();
        assert_eq!(m.actions.len(), 3);
        assert_eq!(m.actions[0].action_id, "add_car");
        assert_eq!(m.actions[0].block_id, "actions");
        assert_eq!(m.actions[0].text, "Add :blue_car:");
        assert_eq!(m.actions[0].value, "add_car_6");
        assert!(m.actions[0].url.is_none());
        assert_eq!(
            m.actions[1].url.as_deref(),
            Some("https://example.test/docs")
        );
        assert_eq!(m.actions[1].style, "primary");
        assert_eq!(m.actions[2].block_id, "more");
        assert_eq!(m.actions[2].style, "danger");
        assert_eq!(
            m.actions[0].raw.get("action_id").and_then(|v| v.as_str()),
            Some("add_car")
        );
        assert_eq!(m.bot_id.as_deref(), Some("B0EXAMPLE02"));
        assert_eq!(m.app_id.as_deref(), Some("A0EXAMPLE03"));
    }

    #[test]
    fn messages_without_actions_are_unaffected() {
        let m = parse_message(&json!({ "ts": "1.0", "text": "ciao" })).unwrap();
        assert!(m.actions.is_empty());
        assert!(m.bot_id.is_none());
        assert!(m.app_id.is_none());
    }

    /// A link preview's picture: `image_url` (the full one) and `thumb_url` (the
    /// small one). Neither was read before, so a preview rendered as text only.
    #[test]
    fn an_attachment_image_is_parsed() {
        let m = parse_message(&json!({
            "ts": "1.0",
            "attachments": [{
                "title": "A page",
                "title_link": "https://x.test/p",
                "image_url": "https://x.test/big.png",
                "thumb_url": "https://x.test/small.png"
            }]
        }))
        .unwrap();
        let a = &m.attachments[0];
        assert_eq!(a.thumb, "https://x.test/small.png");
        assert_eq!(a.image, "https://x.test/big.png");

        // With only `image_url` the picture doubles as its own thumbnail, so it
        // still shows.
        let m = parse_message(&json!({
            "ts": "1.0",
            "attachments": [{ "image_url": "https://x.test/only.png" }]
        }))
        .unwrap();
        assert_eq!(m.attachments[0].thumb, "https://x.test/only.png");
        assert_eq!(m.attachments[0].image, "https://x.test/only.png");
    }

    /// Inline code in Block Kit is a `style`, not backticks: it must come back as
    /// a code span, or an inline snippet renders as ordinary text.
    #[test]
    fn a_block_inline_code_is_backticked_for_the_renderer() {
        use crate::app::mrkdwn::{Block, Span, parse};
        let b = json!([{"type":"rich_text","elements":[
            {"type":"rich_text_section","elements":[
                {"type":"text","text":"esegui "},
                {"type":"text","text":"cargo build","style":{"code":true}}
            ]}
        ]}]);
        let text = blocks_text(&b);
        let first = parse(&text).into_iter().next();
        assert!(
            matches!(&first, Some(Block::Paragraph(spans))
                if spans.contains(&Span::Code("cargo build".into()))),
            "inline code must reach the parser as code, got {text:?}"
        );
    }

    #[test]
    fn rich_text_leaves_are_joined() {
        let b = json!([{"type":"rich_text","elements":[
            {"type":"rich_text_section","elements":[
                {"type":"text","text":"ciao "},
                {"type":"link","url":"https://x.test"}
            ]}
        ]}]);
        assert_eq!(blocks_text(&b), "ciao <https://x.test>");
    }

    /// A code block carried in Block Kit (`rich_text_preformatted`) must reach the
    /// renderer as a **fenced** block: flattening it to its text dropped the
    /// fences, so a one-line block rendered as ordinary text, nothing like a
    /// multi-line one.
    #[test]
    fn a_block_code_block_is_fenced_for_the_renderer() {
        use crate::app::mrkdwn::{Block, parse};
        // Slack sends `rich_text_preformatted.elements` as **inline** pieces: a
        // `text` (with embedded newlines when there are several lines) and/or
        // `link`s — not one child per line.
        let one = json!([{"type":"rich_text","elements":[
            {"type":"rich_text_preformatted","elements":[
                {"type":"text","text":"cargo build"}
            ]}
        ]}]);
        let text = blocks_text(&one);
        assert!(
            parse(&text)
                .iter()
                .any(|b| matches!(b, Block::CodeBlock(c) if c == "cargo build")),
            "a one-line block must be a fenced code block, got {text:?}"
        );

        // Several lines are newlines **inside** the `text`, not separate elements.
        let many = json!([{"type":"rich_text","elements":[
            {"type":"rich_text_preformatted","elements":[
                {"type":"text","text":"fn main() {\n}"}
            ]}
        ]}]);
        let text = blocks_text(&many);
        assert!(
            parse(&text)
                .iter()
                .any(|b| matches!(b, Block::CodeBlock(c) if c == "fn main() {\n}")),
            "a multi-line block must keep its lines, got {text:?}"
        );

        // Inline pieces compose **one** line: they must not be split apart.
        let mixed = json!([{"type":"rich_text","elements":[
            {"type":"rich_text_preformatted","elements":[
                {"type":"text","text":"curl "},
                {"type":"link","url":"https://x.test","text":"https://x.test"}
            ]}
        ]}]);
        let text = blocks_text(&mixed);
        assert!(
            parse(&text).iter().any(
                |b| matches!(b, Block::CodeBlock(c) if !c.contains('\n') && c.starts_with("curl "))
            ),
            "inline pieces are one line, got {text:?}"
        );
    }

    #[test]
    fn a_link_keeps_its_address_next_to_its_label() {
        // The reported defect: `#19974` was left as non-clickable text.
        let b = json!([{"type":"rich_text","elements":[
            {"type":"rich_text_section","elements":[
                {"type":"link","url":"https://github.com/x/pull/19974","text":"#19974"}
            ]}
        ]}]);
        assert_eq!(blocks_text(&b), "<https://github.com/x/pull/19974|#19974>");
    }

    #[test]
    fn a_label_with_separators_falls_back_to_the_address() {
        let b = json!([{"type":"rich_text","elements":[
            {"type":"rich_text_section","elements":[
                {"type":"link","url":"https://x.test","text":"a|b"}
            ]}
        ]}]);
        assert_eq!(blocks_text(&b), "<https://x.test>");
    }

    #[test]
    fn anything_that_is_not_a_list_gives_nothing() {
        for v in [json!(null), json!("testo"), json!({"type":"section"})] {
            assert_eq!(blocks_text(&v), "", "{v}");
        }
    }

    #[test]
    fn empty_texts_do_not_leave_blank_lines() {
        let b = json!([
            {"type":"section","text":{"type":"mrkdwn","text":"  "}},
            {"type":"section","text":{"type":"mrkdwn","text":"uno"}}
        ]);
        assert_eq!(blocks_text(&b), "uno");
    }
}

#[cfg(test)]
mod lettura_tests {
    use super::is_newer;

    #[test]
    fn a_newer_message_can_be_marked() {
        assert!(is_newer("1790779140.488369", "1790779100.000000"));
    }

    #[test]
    fn an_older_one_is_refused() {
        // This is the case that broke everything: during a channel switch a
        // message from the previous conversation was marked, and the official
        // client reopened dozens of messages back.
        assert!(!is_newer("1790779100.000000", "1790779140.488369"));
    }

    #[test]
    fn the_same_message_is_not_remarked() {
        assert!(!is_newer("1790779140.488369", "1790779140.488369"));
    }

    #[test]
    fn without_a_reference_the_first_mark_goes_through() {
        assert!(is_newer("1790779140.488369", ""));
    }

    #[test]
    fn they_are_compared_as_numbers_not_as_text() {
        // As text "999.0" would come after "1790779140.0", and that is false.
        assert!(!is_newer("999.0", "1790779140.0"));
        assert!(is_newer("1790779140.0", "999.0"));
    }

    #[test]
    fn nonsense_is_never_marked() {
        assert!(!is_newer("abc", "1790779140.0"));
        assert!(!is_newer("", "1790779140.0"));
    }
}

#[cfg(test)]
mod quiet_tests {
    use super::in_quiet_hours;
    use crate::api::client::DndInfo;

    fn dnd(enabled: bool, start: i64, end: i64) -> DndInfo {
        DndInfo {
            dnd_enabled: enabled,
            next_dnd_start_ts: start,
            next_dnd_end_ts: end,
            snooze_enabled: false,
        }
    }

    #[test]
    fn inside_the_window_is_quiet() {
        assert!(in_quiet_hours(150, &dnd(true, 100, 200)));
    }

    #[test]
    fn before_and_after_the_window_are_not() {
        assert!(!in_quiet_hours(99, &dnd(true, 100, 200)));
        assert!(
            !in_quiet_hours(200, &dnd(true, 100, 200)),
            "the end is exclusive"
        );
    }

    #[test]
    fn a_configured_schedule_alone_does_not_mean_quiet_now() {
        // This is the normal case: almost everyone has `dnd_enabled`, but
        // during the day the window is in the future.
        assert!(!in_quiet_hours(50, &dnd(true, 100, 200)));
    }

    #[test]
    fn without_a_schedule_nothing_is_quiet() {
        assert!(!in_quiet_hours(150, &dnd(false, 100, 200)));
    }

    #[test]
    fn a_manual_snooze_wins_over_the_schedule() {
        let mut d = dnd(false, 0, 0);
        d.snooze_enabled = true;
        assert!(in_quiet_hours(150, &d));
    }

    #[test]
    fn a_malformed_window_does_not_silence_everyone() {
        assert!(!in_quiet_hours(150, &dnd(true, 0, 0)));
        assert!(
            !in_quiet_hours(150, &dnd(true, 200, 100)),
            "fine prima dell'inizio"
        );
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::resolve_mentions;

    fn chosen(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn a_chosen_mention_becomes_the_slack_form() {
        let c = chosen(&[("@Marco Ferretti", "<@U1>")]);
        assert_eq!(
            resolve_mentions("ciao @Marco Ferretti tutto bene?", &c),
            "ciao <@U1> tutto bene?"
        );
    }

    #[test]
    fn a_name_typed_by_hand_is_left_alone() {
        // Nobody chose it from the popup: guessing the id would be worse
        // than leaving the text as it is.
        assert_eq!(resolve_mentions("ciao @marco", &[]), "ciao @marco");
    }

    #[test]
    fn a_longer_name_is_not_broken_by_a_shorter_prefix() {
        let c = chosen(&[("@Marco", "<@U1>"), ("@Marco Ferretti", "<@U2>")]);
        assert_eq!(resolve_mentions("@Marco Ferretti", &c), "<@U2>");
    }

    #[test]
    fn an_edited_insertion_no_longer_matches() {
        let c = chosen(&[("@Marco Ferretti", "<@U1>")]);
        assert_eq!(resolve_mentions("@Marco Ferrett", &c), "@Marco Ferrett");
    }

    #[test]
    fn the_same_mention_twice_is_replaced_both_times() {
        let c = chosen(&[("@Ada", "<@U1>")]);
        assert_eq!(resolve_mentions("@Ada e @Ada", &c), "<@U1> e <@U1>");
    }

    #[test]
    fn channels_work_the_same_way() {
        let c = chosen(&[("#generale", "<#C1|generale>")]);
        assert_eq!(
            resolve_mentions("vedi #generale", &c),
            "vedi <#C1|generale>"
        );
    }

    #[test]
    fn text_without_mentions_is_untouched() {
        let c = chosen(&[("@Ada", "<@U1>")]);
        assert_eq!(resolve_mentions("niente da fare", &c), "niente da fare");
    }
}

#[cfg(test)]
mod mention_tests {
    use super::mention_prefix;

    #[test]
    fn finds_the_fragment_being_typed() {
        assert_eq!(mention_prefix("ciao @mar", 9, '@'), Some((5, "mar".into())));
    }

    #[test]
    fn at_the_very_start_works() {
        assert_eq!(mention_prefix("@gi", 3, '@'), Some((0, "gi".into())));
    }

    #[test]
    fn an_email_does_not_open_it() {
        // `@` preceded by a letter: an address is being written.
        assert_eq!(mention_prefix("scrivi a mario@acme", 19, '@'), None);
    }

    #[test]
    fn a_space_closes_it() {
        assert_eq!(mention_prefix("@mario ciao", 11, '@'), None);
    }

    #[test]
    fn an_empty_fragment_is_still_a_match() {
        // As soon as `@` is typed, the full list is shown.
        assert_eq!(mention_prefix("ciao @", 6, '@'), Some((5, String::new())));
    }

    #[test]
    fn only_what_precedes_the_cursor_counts() {
        assert_eq!(
            mention_prefix("@mario e @luigi", 6, '@'),
            Some((0, "mario".into()))
        );
    }

    #[test]
    fn the_hash_works_the_same() {
        assert_eq!(mention_prefix("vedi #gen", 9, '#'), Some((5, "gen".into())));
    }
}

#[cfg(test)]
mod time_tests {
    use super::*;

    #[test]
    fn an_invalid_timestamp_gives_empty_strings_not_a_panic() {
        for bad in ["", "abc", "."] {
            assert_eq!(format_ts(bad), "");
            assert_eq!(format_ts_full(bad), "");
        }
    }

    #[test]
    fn the_time_has_the_expected_shape() {
        let s = format_ts("1700000000.000100");
        assert_eq!(s.len(), 5, "{s}");
        assert_eq!(s.as_bytes()[2], b':');
    }

    #[test]
    fn a_recent_message_says_today() {
        rust_i18n::set_locale("it");
        let now = chrono::Local::now().timestamp();
        assert!(format_ts_full(&now.to_string()).starts_with("oggi alle"));
    }

    #[test]
    fn yesterday_is_named() {
        rust_i18n::set_locale("it");
        let t = chrono::Local::now().timestamp() - 86_400;
        let s = format_ts_full(&t.to_string());
        assert!(s.starts_with("ieri alle"), "{s}");
    }

    #[test]
    fn an_older_message_carries_day_month_and_year() {
        rust_i18n::set_locale("it");
        let s = format_ts_full("1700000000");
        assert!(s.contains("2023"), "{s}");
        assert!(s.contains("novembre"), "{s}");
    }
}

#[cfg(test)]
mod mpim_tests {
    use super::*;

    fn nomi() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("alice".to_string(), "Alice B".to_string());
        m.insert("bob".to_string(), "Bob".to_string());
        m
    }

    #[test]
    fn participants_are_split_on_double_dash() {
        assert_eq!(
            mpim_participants("mpdm-alice--bob--carol-1"),
            vec!["alice", "bob", "carol"]
        );
    }

    #[test]
    fn the_trailing_group_index_is_dropped() {
        assert_eq!(
            mpim_participants("mpdm-alice--bob-12"),
            vec!["alice", "bob"]
        );
    }

    #[test]
    fn a_channel_name_is_not_a_mpim() {
        assert!(mpim_participants("general").is_empty());
    }

    #[test]
    fn label_maps_usernames_to_display_names() {
        assert_eq!(
            mpim_label("mpdm-alice--bob--carol-1", &nomi()),
            "👥 Alice B, Bob, carol"
        );
    }

    #[test]
    fn label_keeps_unknown_usernames() {
        assert_eq!(mpim_label("mpdm-alice--zoe-1", &nomi()), "👥 Alice B, zoe");
    }

    #[test]
    fn label_leaves_a_plain_channel_untouched() {
        assert_eq!(mpim_label("general", &nomi()), "general");
    }
}

#[cfg(test)]
mod huddle_state_tests {
    use super::*;
    use crate::app::huddles::{Event, Room};

    fn stato() -> Arc<RwLock<AppState>> {
        let mut s = AppState::default();
        s.my_user_id = "U_ME".to_string();
        Arc::make_mut(&mut s.user_names).insert("U_CHIAMA".to_string(), "Ada".to_string());
        Arc::new(RwLock::new(s))
    }

    fn room(participants: &[&str]) -> Event {
        Event::Room {
            channel: "D1".to_string(),
            room: Room {
                id: "R1".to_string(),
                participants: participants.iter().map(|s| s.to_string()).collect(),
                ended: participants.is_empty(),
            },
        }
    }

    /// No notifications: a test must not make windows appear on the desktop.
    fn muted() {
        unsafe { std::env::set_var("SLAQ_NO_NOTIFY", "1") };
    }

    #[tokio::test]
    async fn an_invite_makes_it_ring_and_says_who() {
        muted();
        let st = stato();
        App::apply_huddle(
            &st,
            Event::Invite {
                channel: "D1".into(),
                caller: "U_CHIAMA".into(),
            },
        )
        .await;
        let s = st.read().await;
        let h = s.huddles.get("D1").expect("huddle");
        assert!(h.ringing);
        assert_eq!(h.caller.as_deref(), Some("U_CHIAMA"));
        assert!(s.status.contains("Ada"), "{}", s.status);
    }

    #[tokio::test]
    async fn a_huddle_in_a_channel_shows_up_but_does_not_ring() {
        // In a channel nobody is calling you: the call is there and you see it.
        muted();
        let st = stato();
        App::apply_huddle(&st, room(&["U_CHIAMA"])).await;
        let s = st.read().await;
        let h = s.huddles.get("D1").expect("huddle");
        assert!(!h.ringing);
        assert_eq!(h.participants, ["U_CHIAMA"]);
    }

    #[tokio::test]
    async fn joining_stops_the_ringing() {
        muted();
        let st = stato();
        App::apply_huddle(
            &st,
            Event::Invite {
                channel: "D1".into(),
                caller: "U_CHIAMA".into(),
            },
        )
        .await;
        App::apply_huddle(&st, room(&["U_CHIAMA", "U_ME"])).await;
        assert!(!st.read().await.huddles["D1"].ringing);
    }

    #[tokio::test]
    async fn an_empty_room_is_forgotten() {
        muted();
        let st = stato();
        App::apply_huddle(&st, room(&["U_CHIAMA"])).await;
        App::apply_huddle(&st, room(&[])).await;
        assert!(st.read().await.huddles.is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_invite_stops_ringing_but_leaves_the_room() {
        // The caller can hang up leaving the room open: joining it afterwards
        // has to stay possible.
        muted();
        let st = stato();
        App::apply_huddle(&st, room(&["U_CHIAMA"])).await;
        App::apply_huddle(
            &st,
            Event::Invite {
                channel: "D1".into(),
                caller: "U_CHIAMA".into(),
            },
        )
        .await;
        App::apply_huddle(
            &st,
            Event::Cancel {
                channel: "D1".into(),
            },
        )
        .await;
        let s = st.read().await;
        assert!(!s.huddles["D1"].ringing);
        assert_eq!(s.huddles["D1"].participants, ["U_CHIAMA"]);
    }
}

#[cfg(test)]
mod reaction_event_tests {
    use super::*;
    use serde_json::json;

    /// The shape captured from the official client's WebSocket.
    fn event(kind: &str, user: &str) -> serde_json::Value {
        json!({
            "type": kind,
            "event_ts": "1790701470.007700",
            "ts": "1790701470.007700",
            "user": user,
            "item_user": "U_A",
            "reaction": "eyes",
            "item": { "type": "message", "channel": "C1", "ts": "100.1" }
        })
    }

    async fn fixture() -> (
        Arc<RwLock<AppState>>,
        Arc<tokio::sync::Mutex<crate::store::Store>>,
    ) {
        let store = crate::store::Store::open_in_memory().unwrap();
        let mut store = store;
        store
            .upsert_messages(
                "C1",
                &[json!({ "type": "message", "ts": "100.1", "user": "U_A", "text": "ciao" })],
            )
            .unwrap();
        let mut st = AppState::default();
        st.my_user_id = "U_ME".to_string();
        Arc::make_mut(&mut st.channels).push(ChannelEntry {
            id: "C1".into(),
            name: "general".into(),
            is_dm: false,
            unread: false,
            mentions: 0,
            user: None,
            last_read: String::new(),
            external: false,
        });
        st.messages = vec![
            parse_message_as(
                &json!({ "type": "message", "ts": "100.1", "user": "U_A", "text": "ciao" }),
                "U_ME",
            )
            .unwrap(),
        ];
        (
            Arc::new(RwLock::new(st)),
            Arc::new(tokio::sync::Mutex::new(store)),
        )
    }

    /// `find_mod_msg` checks the timeline **and** the open thread: a reply is not
    /// in the timeline, and both belong to the same channel.
    #[cfg(feature = "mods")]
    #[test]
    fn find_mod_msg_falls_through_to_the_thread() {
        let mut st = AppState::default();
        st.my_user_id = "U_ME".to_string();
        st.messages_channel = Some("C1".to_string());
        st.messages = vec![
            parse_message_as(
                &json!({ "type": "message", "ts": "1.0", "user": "U_A", "text": "root" }),
                "U_ME",
            )
            .unwrap(),
        ];
        st.thread_channel_id = Some("C1".to_string());
        st.thread_messages = vec![
            parse_message_as(
                &json!({ "type": "message", "ts": "2.0", "user": "U_B", "text": "reply" }),
                "U_ME",
            )
            .unwrap(),
        ];
        // The reply is only in the thread, and the channel is also the timeline's.
        assert_eq!(
            st.find_mod_msg("C1", "2.0").map(|m| m.text).as_deref(),
            Some("reply"),
            "a thread reply resolves"
        );
        assert_eq!(
            st.find_mod_msg("C1", "1.0").map(|m| m.text).as_deref(),
            Some("root"),
            "the timeline still resolves"
        );
        assert!(st.find_mod_msg("C1", "9.0").is_none(), "not on screen");
    }

    /// The reported bug: the reaction was in the view and nowhere else, so
    /// changing channel and coming back lost it while the official client
    /// still showed it.
    #[tokio::test]
    async fn a_reaction_survives_in_the_cache() {
        let (state, store) = fixture().await;
        App::apply_reaction_event(&event("reaction_added", "U_ME"), &state, &store).await;

        let saved = store
            .lock()
            .await
            .raw_message("C1", "100.1")
            .unwrap()
            .unwrap();
        assert_eq!(saved["reactions"][0]["name"], "eyes");
        assert_eq!(saved["reactions"][0]["users"], json!(["U_ME"]));

        // And what a reopened conversation is redrawn from says the same.
        let rows = store.lock().await.messages("C1", 50).unwrap();
        let m = parse_message_as(&rows[0].raw, "U_ME").unwrap();
        assert_eq!(m.reactions.len(), 1);
        assert!(m.reactions[0].mine);
    }

    #[tokio::test]
    async fn someone_elses_reaction_is_not_ours() {
        let (state, store) = fixture().await;
        App::apply_reaction_event(&event("reaction_added", "U_B"), &state, &store).await;
        let s = state.read().await;
        assert_eq!(s.messages[0].reactions[0].users, ["U_B"]);
        assert!(!s.messages[0].reactions[0].mine);
    }

    #[tokio::test]
    async fn removing_it_removes_it_from_the_cache_too() {
        let (state, store) = fixture().await;
        App::apply_reaction_event(&event("reaction_added", "U_ME"), &state, &store).await;
        App::apply_reaction_event(&event("reaction_removed", "U_ME"), &state, &store).await;
        let saved = store
            .lock()
            .await
            .raw_message("C1", "100.1")
            .unwrap()
            .unwrap();
        assert!(saved.get("reactions").is_none(), "{saved}");
        assert!(state.read().await.messages[0].reactions.is_empty());
    }

    #[tokio::test]
    async fn a_reaction_on_a_file_is_not_our_business() {
        let (state, store) = fixture().await;
        let mut e = event("reaction_added", "U_ME");
        e["item"] = json!({ "type": "file", "file": "F1" });
        App::apply_reaction_event(&e, &state, &store).await;
        let saved = store
            .lock()
            .await
            .raw_message("C1", "100.1")
            .unwrap()
            .unwrap();
        assert!(saved.get("reactions").is_none());
    }
}

#[cfg(test)]
mod reaction_tests {
    use super::apply_reaction;
    use serde_json::json;

    fn msg() -> serde_json::Value {
        json!({ "type": "message", "ts": "100.1", "user": "U_A", "text": "ciao" })
    }

    #[test]
    fn the_first_reaction_creates_the_list() {
        let mut m = msg();
        apply_reaction(&mut m, "U_ME", "thumbsup", true);
        assert_eq!(
            m["reactions"],
            json!([{ "name": "thumbsup", "count": 1, "users": ["U_ME"] }])
        );
    }

    #[test]
    fn a_second_person_is_added_to_the_same_one() {
        let mut m = msg();
        apply_reaction(&mut m, "U_ME", "eyes", true);
        apply_reaction(&mut m, "U_B", "eyes", true);
        assert_eq!(m["reactions"][0]["count"], 2);
        assert_eq!(m["reactions"][0]["users"], json!(["U_ME", "U_B"]));
    }

    #[test]
    fn the_same_event_twice_counts_once() {
        // On a reconnection Slack can resend what we have already applied.
        let mut m = msg();
        apply_reaction(&mut m, "U_ME", "eyes", true);
        apply_reaction(&mut m, "U_ME", "eyes", true);
        assert_eq!(m["reactions"][0]["count"], 1);
    }

    #[test]
    fn removing_the_last_one_removes_the_reaction() {
        let mut m = msg();
        apply_reaction(&mut m, "U_ME", "eyes", true);
        apply_reaction(&mut m, "U_ME", "eyes", false);
        // Not an empty list: an empty `reactions` is not what Slack sends,
        // and the message must stay the shape the rest of the client reads.
        assert!(m.get("reactions").is_none(), "{m}");
    }

    #[test]
    fn removing_one_of_several_leaves_the_others() {
        let mut m = msg();
        apply_reaction(&mut m, "U_ME", "eyes", true);
        apply_reaction(&mut m, "U_B", "eyes", true);
        apply_reaction(&mut m, "U_ME", "eyes", false);
        assert_eq!(m["reactions"][0]["count"], 1);
        assert_eq!(m["reactions"][0]["users"], json!(["U_B"]));
    }

    #[test]
    fn different_reactions_do_not_touch_each_other() {
        let mut m = msg();
        apply_reaction(&mut m, "U_ME", "eyes", true);
        apply_reaction(&mut m, "U_B", "tada", true);
        apply_reaction(&mut m, "U_ME", "eyes", false);
        assert_eq!(m["reactions"].as_array().unwrap().len(), 1);
        assert_eq!(m["reactions"][0]["name"], "tada");
    }

    #[test]
    fn removing_one_that_is_not_there_changes_nothing() {
        let mut m = msg();
        apply_reaction(&mut m, "U_ME", "eyes", false);
        assert!(m.get("reactions").is_none());
    }

    #[test]
    fn what_slack_already_sent_is_preserved() {
        // The shape that arrives from `conversations.history`.
        let mut m = msg();
        m["reactions"] = json!([{ "name": "eyes", "count": 2, "users": ["U_A", "U_B"] }]);
        apply_reaction(&mut m, "U_ME", "eyes", true);
        assert_eq!(m["reactions"][0]["users"], json!(["U_A", "U_B", "U_ME"]));
        assert_eq!(m["reactions"][0]["count"], 3);
    }
}

#[cfg(test)]
mod notification_center_tests {
    use super::*;

    fn entry(channel: &str, ts: &str) -> NotifEntry {
        NotifEntry {
            channel_id: channel.to_string(),
            channel_name: "#prova".to_string(),
            author: "giulio".to_string(),
            text: "ciao".to_string(),
            ts: ts.to_string(),
            is_dm: false,
            thread_ts: None,
            read: false,
        }
    }

    #[test]
    fn the_newest_entry_is_first() {
        let mut s = AppState::default();
        push_notification(&mut s, entry("C1", "100.1"));
        push_notification(&mut s, entry("C1", "200.1"));
        assert_eq!(s.notifications[0].ts, "200.1");
        assert_eq!(s.notifications[1].ts, "100.1");
    }

    #[test]
    fn the_same_message_is_not_listed_twice() {
        let mut s = AppState::default();
        push_notification(&mut s, entry("C1", "100.1"));
        push_notification(&mut s, entry("C1", "100.1"));
        assert_eq!(s.notifications.len(), 1);
    }

    #[test]
    fn the_same_ts_in_another_channel_is_a_different_notification() {
        let mut s = AppState::default();
        push_notification(&mut s, entry("C1", "100.1"));
        push_notification(&mut s, entry("C2", "100.1"));
        assert_eq!(s.notifications.len(), 2);
    }

    #[test]
    fn the_list_is_capped() {
        let mut s = AppState::default();
        for i in 0..(NOTIFICATIONS_MAX + 20) {
            push_notification(&mut s, entry("C1", &format!("{i}.0")));
        }
        assert_eq!(s.notifications.len(), NOTIFICATIONS_MAX);
    }

    #[test]
    fn pushing_the_same_message_again_moves_it_to_the_top() {
        let mut s = AppState::default();
        push_notification(&mut s, entry("C1", "100.1"));
        push_notification(&mut s, entry("C1", "200.1"));
        push_notification(&mut s, entry("C1", "100.1"));
        assert_eq!(s.notifications.len(), 2);
        assert_eq!(s.notifications[0].ts, "100.1");
    }

    #[test]
    fn a_read_notification_stays_read_when_it_arrives_again() {
        let mut s = AppState::default();
        push_notification(&mut s, entry("C1", "100.1"));
        s.notifications[0].read = true;
        push_notification(&mut s, entry("C1", "100.1"));
        assert!(s.notifications[0].read);
    }
}

#[cfg(all(test, feature = "mods"))]
mod mods_followup_tests {
    use super::*;

    /// The follow-up budget is capped, and a later window reopens it (the stored
    /// timestamp is the window *start*, not the last attempt).
    #[test]
    fn followup_budget_is_bounded_and_reopens_after_a_window() {
        let mut s = AppState::default();
        let t0 = std::time::Instant::now();
        for i in 0..MAX_MOD_FOLLOWUPS {
            assert!(s.claim_followup("C1", t0), "claim {i} should succeed");
        }
        assert!(!s.claim_followup("C1", t0), "the budget must be exhausted");
        let later = t0 + MODS_FOLLOWUP_WINDOW + std::time::Duration::from_secs(1);
        assert!(
            s.claim_followup("C1", later),
            "a new window must reopen the budget"
        );
    }
}

/// The consent prompt's load-flow contract: the startup prompt must leave the
/// host unloaded until answered, and a never-decided mod must start unchecked.
#[cfg(all(test, feature = "mods"))]
mod consent_prompt_tests {
    use super::*;
    use std::fs;

    fn fixture_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("slaq-consent-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let d = root.join("hello");
        fs::create_dir_all(&d).unwrap();
        for f in ["manifest.json", "mod.wasm"] {
            fs::copy(
                format!("{}/tests/data/hello-mod/{f}", env!("CARGO_MANIFEST_DIR")),
                d.join(f),
            )
            .unwrap();
        }
        root
    }

    /// A fixture whose manifest asks for a channel scope and two hosts.
    fn net_fixture(tag: &str, hosts: &[&str]) -> std::path::PathBuf {
        let root = fixture_root(tag);
        let path = root.join("hello/manifest.json");
        let mut m: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        m["capabilities"]["messages.read"] =
            serde_json::json!({ "scope": "channels", "channels": ["C1"] });
        m["capabilities"]["net.egress"] = serde_json::json!(hosts);
        fs::write(&path, serde_json::to_vec(&m).unwrap()).unwrap();
        root
    }

    fn granted_net(consent: &mut crate::mods::consent::Consent, hosts: &[&str]) {
        use crate::mods::capability::{Capability, ReadScope};
        consent.grant_full(
            "dev.slaq.hello",
            &[Capability::MessagesRead, Capability::NetEgress],
            Some(ReadScope::Channels {
                channels: ["C1".to_string()].into(),
            }),
            Some(hosts.iter().map(|h| h.to_string()).collect()),
        );
    }

    fn effective(
        root: &std::path::Path,
        consent: &crate::mods::consent::Consent,
    ) -> (Option<crate::mods::capability::ReadScope>, Vec<String>) {
        let loaded = crate::mods::load(&root.join("hello")).unwrap();
        let entry = consent.entry("dev.slaq.hello");
        (
            crate::mods::consent::Grants::effective_read_scope(&loaded.requested, &entry),
            crate::mods::consent::Grants::effective_egress(&loaded.requested, &entry),
        )
    }
    /// The dialog edits the user's channels and hosts beyond the manifest, and
    /// Apply records exactly that; reopening shows it with nothing pending.
    #[test]
    fn access_edits_are_applied_beyond_the_manifest() {
        use crate::mods::capability::ReadScope;
        let root = net_fixture("access", &["api.example.com", "cdn.example.com"]);
        let mut consent = crate::mods::consent::Consent::default();
        granted_net(&mut consent, &["api.example.com", "cdn.example.com"]);
        let (mut dialog, undecided) = App::build_mods_consent(&root, &consent, false);
        assert!(!undecided);
        let e = &mut dialog.entries[0];
        assert!(!e.access.egress_dirty, "nothing pending before an edit");
        e.edit_access(AccessEdit::ToggleHost("cdn.example.com".into()));
        e.edit_access(AccessEdit::AddHost(" LocalHost:11434 ".into()));
        e.edit_access(AccessEdit::AddHost("http://nope".into()));
        assert!(e.access.error.is_some(), "a bad host is refused with a reason");
        e.edit_access(AccessEdit::AddHost("cdn.example.com".into()));
        assert!(e.access.error.is_none());
        assert!(
            !e.access.egress_added.contains(&"cdn.example.com".to_string()),
            "a manifest host is re-allowed, not added"
        );
        e.edit_access(AccessEdit::ToggleHost("cdn.example.com".into()));
        e.edit_access(AccessEdit::ScopeSome);
        assert_eq!(
            e.access.user_scope,
            Some(ReadScope::Channels {
                channels: ["C1".to_string()].into()
            }),
            "the pick starts from what the mod reads today"
        );
        e.edit_access(AccessEdit::ToggleChannel("D9".into()));
        apply_consent_entries(&mut consent, &dialog.entries).unwrap();

        let (scope, egress) = effective(&root, &consent);
        assert_eq!(
            scope,
            Some(ReadScope::Channels {
                channels: ["C1".to_string(), "D9".to_string()].into()
            })
        );
        assert_eq!(egress, vec!["api.example.com", "localhost:11434"]);

        // Reopened: the choice is shown, nothing pending, no prompt.
        let (again, undecided) = App::build_mods_consent(&root, &consent, true);
        assert!(!undecided && again.entries.is_empty());
        let (review, _) = App::build_mods_consent(&root, &consent, false);
        let a = &review.entries[0].access;
        assert!(!a.egress_dirty && !a.scope_dirty);
        assert_eq!(a.egress_allowed, vec!["api.example.com"]);
        assert_eq!(a.egress_added, vec!["localhost:11434"]);

        // An untouched Apply keeps the unticked manifest host unticked (the old
        // dialog re-granted the whole manifest list here).
        apply_consent_entries(&mut consent, &review.entries).unwrap();
        assert_eq!(effective(&root, &consent).1, vec!["api.example.com", "localhost:11434"]);

        // Back to the manifest's scope.
        let (mut review, _) = App::build_mods_consent(&root, &consent, false);
        review.entries[0].edit_access(AccessEdit::ScopeManifest);
        apply_consent_entries(&mut consent, &review.entries).unwrap();
        assert_eq!(
            effective(&root, &consent).0,
            Some(ReadScope::Channels {
                channels: ["C1".to_string()].into()
            })
        );
        let _ = fs::remove_dir_all(&root);
    }

    fn shown(dialog: &ModConsentDialog, cap: crate::mods::capability::Capability) -> bool {
        dialog.entries[0].caps.iter().any(|c| c.cap == cap && c.selected)
    }

    /// Regression (adversarial review): with `net.egress` stored but reaching
    /// nothing (every manifest host unticked), the dialog shows it unticked;
    /// adding a host must not bring the unticked manifest hosts back, nor make
    /// anything reachable while the box stays off.
    #[test]
    fn a_host_edit_with_egress_shown_off_grants_nothing() {
        use crate::mods::capability::Capability;
        let root = net_fixture("shown-off", &["api.example.com", "cdn.example.com"]);
        let manifest = vec!["api.example.com".to_string(), "cdn.example.com".to_string()];
        let mut consent = crate::mods::consent::Consent::default();
        granted_net(&mut consent, &["api.example.com", "cdn.example.com"]);
        consent.set_egress("dev.slaq.hello", &manifest, &[], &[]).unwrap();
        let (mut dialog, _) = App::build_mods_consent(&root, &consent, false);
        assert!(!shown(&dialog, Capability::NetEgress));
        assert!(
            dialog.entries[0].access.egress_allowed.is_empty(),
            "reviewed, unticked hosts stay unticked"
        );
        dialog.entries[0].edit_access(AccessEdit::AddHost("localhost:11434".into()));
        apply_consent_entries(&mut consent, &dialog.entries).unwrap();
        assert!(effective(&root, &consent).1.is_empty(), "the box was off");
        assert_eq!(
            consent.entry("dev.slaq.hello").egress_added(),
            &["localhost:11434".to_string()][..],
            "the host is kept for when the user grants net.egress"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Regression (adversarial review): an update that replaces the consented
    /// host shows `net.egress` unticked; Apply (with or without a host edit)
    /// must not grant the update's new host.
    #[test]
    fn an_updates_new_host_is_not_granted_while_shown_off() {
        use crate::mods::capability::Capability;
        for edit in [false, true] {
            let root = net_fixture("upd-off", &["new.example.com"]);
            let mut consent = crate::mods::consent::Consent::default();
            granted_net(&mut consent, &["old.example.com"]);
            let (mut dialog, undecided) = App::build_mods_consent(&root, &consent, true);
            assert!(undecided && !shown(&dialog, Capability::NetEgress));
            if edit {
                dialog.entries[0].edit_access(AccessEdit::AddHost("localhost:11434".into()));
            }
            apply_consent_entries(&mut consent, &dialog.entries).unwrap();
            assert!(effective(&root, &consent).1.is_empty(), "edit={edit}");
            let _ = fs::remove_dir_all(&root);
        }
    }

    /// Regression (adversarial review): `messages.read` stored with a scope the
    /// manifest no longer meets is shown unticked; picking channels must not
    /// re-activate it behind the unticked box.
    #[test]
    fn a_channel_pick_with_read_shown_off_grants_nothing() {
        use crate::mods::capability::{Capability, ReadScope};
        let root = net_fixture("read-off", &["api.example.com"]);
        let mut consent = crate::mods::consent::Consent::default();
        consent.grant(
            "dev.slaq.hello",
            &[Capability::MessagesRead],
            Some(ReadScope::Channels {
                channels: ["C2".to_string()].into(),
            }),
        );
        assert!(effective(&root, &consent).0.is_none());
        let (mut dialog, _) = App::build_mods_consent(&root, &consent, false);
        assert!(!shown(&dialog, Capability::MessagesRead));
        dialog.entries[0].edit_access(AccessEdit::ScopeAll);
        apply_consent_entries(&mut consent, &dialog.entries).unwrap();
        assert!(effective(&root, &consent).0.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// Regression (adversarial review): Apply must not clobber a host added on
    /// the CLI while the dialog was open — neither an untouched Apply (store
    /// order differing from the manifest's used to mark it dirty) nor an edit
    /// to another host.
    #[test]
    fn apply_keeps_a_concurrent_cli_host() {
        let manifest = vec!["a.example.com".to_string(), "b.example.com".to_string()];
        for edit in [false, true] {
            let root = net_fixture("cli-race", &["a.example.com", "b.example.com"]);
            let mut consent = crate::mods::consent::Consent::default();
            granted_net(&mut consent, &["a.example.com", "b.example.com"]);
            // Store order now differs from the manifest's.
            consent.remove_host("dev.slaq.hello", &manifest, "a.example.com").unwrap();
            consent.add_host("dev.slaq.hello", &manifest, "a.example.com").unwrap();
            let (mut dialog, _) = App::build_mods_consent(&root, &consent, false);
            assert!(!dialog.entries[0].access.egress_dirty, "no edit, nothing pending");
            if edit {
                dialog.entries[0].edit_access(AccessEdit::ToggleHost("b.example.com".into()));
            }
            consent.add_host("dev.slaq.hello", &manifest, "localhost:1").unwrap();
            apply_consent_entries(&mut consent, &dialog.entries).unwrap();
            let eff = effective(&root, &consent).1;
            assert!(eff.contains(&"localhost:1".to_string()), "edit={edit}: {eff:?}");
            assert_eq!(eff.contains(&"b.example.com".to_string()), !edit);
            let _ = fs::remove_dir_all(&root);
        }
    }

    /// An empty channel pick is not saved (the previous choice stays).
    #[test]
    fn an_empty_channel_pick_is_not_saved() {
        use crate::mods::capability::ReadScope;
        let root = net_fixture("empty-pick", &["api.example.com"]);
        let mut consent = crate::mods::consent::Consent::default();
        granted_net(&mut consent, &["api.example.com"]);
        consent
            .set_user_scope("dev.slaq.hello", Some(ReadScope::All))
            .unwrap();
        let (mut dialog, _) = App::build_mods_consent(&root, &consent, false);
        dialog.entries[0].edit_access(AccessEdit::ToggleChannel("C5".into()));
        dialog.entries[0].edit_access(AccessEdit::ToggleChannel("C5".into()));
        apply_consent_entries(&mut consent, &dialog.entries).unwrap();
        assert_eq!(effective(&root, &consent).0, Some(ReadScope::All));
        let _ = fs::remove_dir_all(&root);
    }

    /// A host a manifest update adds, not yet reviewed, defaults to allowed and
    /// is recorded by Apply, as the old re-grant did, without dropping the
    /// user's own hosts.
    #[test]
    fn a_new_manifest_host_is_allowed_on_apply() {
        let root = net_fixture("new-host", &["api.example.com", "new.example.com"]);
        let mut consent = crate::mods::consent::Consent::default();
        granted_net(&mut consent, &["api.example.com"]);
        consent
            .set_egress(
                "dev.slaq.hello",
                &["api.example.com".to_string()],
                &["api.example.com".to_string()],
                &["localhost:11434".to_string()],
            )
            .unwrap();
        let (dialog, undecided) = App::build_mods_consent(&root, &consent, true);
        assert!(undecided, "the new host re-prompts");
        assert!(dialog.entries[0].access.egress_dirty);
        apply_consent_entries(&mut consent, &dialog.entries).unwrap();
        assert_eq!(
            effective(&root, &consent).1,
            vec!["api.example.com", "new.example.com", "localhost:11434"]
        );
        let (_, undecided) = App::build_mods_consent(&root, &consent, true);
        assert!(!undecided);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn startup_prompt_is_pending_and_starts_unchecked() {
        let root = fixture_root("pending");
        let consent = crate::mods::consent::Consent::default();
        let (dialog, undecided) = App::build_mods_consent(&root, &consent, true);
        assert!(undecided, "a never-decided mod must be asked about");
        assert!(
            dialog.pending,
            "the startup prompt must load the host when dismissed"
        );
        assert_eq!(dialog.entries.len(), 1);
        assert!(
            dialog.entries[0]
                .caps
                .iter()
                .all(|c| !c.selected && !c.initial),
            "nothing is granted until the user ticks it"
        );

        // The review dialog is not the startup prompt: dismissing it must not
        // trigger a load.
        let (review, _) = App::build_mods_consent(&root, &consent, false);
        assert!(!review.pending);

        let _ = fs::remove_dir_all(&root);
    }
}
