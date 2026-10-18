//! Per-mod settings: the manifest's `settings` schema, the values the user
//! configured, and storage.
//!
//! Non-secret values live in one JSON file (a sibling of the mods dir, like the
//! grants file); **secrets** live in the system keyring, namespaced
//! `slaq:mod:<id>:<key>`, are written by the user in the host-rendered form and
//! are **read-only to the mod** (`settings.get` / `settings.set`).
//!
//! `settings.*` is **ungated** (never `permission-denied`); it is the
//! `ui.settings` capability that gates the *form* that renders a non-empty
//! `settings` array. See `docs/mods/manifest-v0.md` ("Settings and secrets") and
//! `docs/mods/api-v0.md` (Secrets).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};

/// Serializes the read-modify-write of `mods-settings.json`: the per-mod
/// `RwLock` does not order two mods (or the two front ends) touching the file.
static SETTINGS_LOCK: Mutex<()> = Mutex::new(());
/// A per-process counter, so two writers cannot clobber the same temp file.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Most fields a manifest may declare.
const MAX_SETTINGS_FIELDS: usize = 64;
/// A setting key is short and shell-friendly.
const MAX_KEY_BYTES: usize = 64;
/// A display label / enum option.
const MAX_LABEL_BYTES: usize = 200;
/// A text/url/secret value crossing the boundary.
const MAX_TEXT_BYTES: usize = 4096;
const MAX_ENUM_VALUES: usize = 64;
const MAX_ENUM_VALUE_BYTES: usize = 200;
/// The non-secret settings file is read with a hard cap (a stat-based check is
/// a TOCTOU race).
const MAX_SETTINGS_FILE_BYTES: u64 = 1 << 20;

/// Keyring service (shared with the credential entries; the account namespaces
/// a mod setting).
const KEYRING_SERVICE: &str = "slaq";

/// The reserved key (inside a mod's map) recording the package fingerprint the
/// secrets are bound to. Not a valid setting key (`$`), so it cannot collide.
const PKG_KEY: &str = "$pkg";

/// The declared type of a settings field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Number,
    Bool,
    Enum,
    Url,
    Secret,
}

impl Kind {
    fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "string" => Kind::Text,
            "number" => Kind::Number,
            "bool" => Kind::Bool,
            "enum" => Kind::Enum,
            "url" => Kind::Url,
            "secret" => Kind::Secret,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "string",
            Kind::Number => "number",
            Kind::Bool => "bool",
            Kind::Enum => "enum",
            Kind::Url => "url",
            Kind::Secret => "secret",
        }
    }

    /// Secrets are stored in the keyring and are read-only to the mod; they are
    /// the only kind whose value is masked in the form.
    pub fn is_secret(self) -> bool {
        matches!(self, Kind::Secret)
    }
}

/// A configured value. Mirrors the WIT `setting-value` variant; secrets read
/// back as [`Value::Text`].
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Text(String),
    Integer(i64),
    Real(f64),
    Flag(bool),
}

impl Value {
    /// The value as the mod sees it (`setting.get` of a secret is text).
    pub fn as_text(&self) -> String {
        match self {
            Value::Text(s) => s.clone(),
            Value::Integer(n) => n.to_string(),
            Value::Real(f) => f.to_string(),
            Value::Flag(b) => b.to_string(),
        }
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Text(s) => serde_json::Value::String(s.clone()),
            Value::Integer(n) => serde_json::Value::from(*n),
            Value::Real(f) => serde_json::Value::from(*f),
            Value::Flag(b) => serde_json::Value::from(*b),
        }
    }
}

/// One declared setting.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub key: String,
    pub kind: Kind,
    pub default: Option<Value>,
    pub label: Option<String>,
    /// A longer help string (shown by the form).
    pub help: Option<String>,
    /// Allowed options for [`Kind::Enum`] (empty otherwise).
    pub values: Vec<String>,
}

/// The manifest's `settings` array, parsed and validated.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub fields: Vec<Field>,
}

impl Schema {
    /// Parses and validates a manifest `settings` value, matching
    /// `manifest.schema.json` (unknown `type`s, duplicate keys, enum without
    /// `values`, and a default of the wrong shape are all refused).
    pub fn parse(v: &serde_json::Value) -> Result<Schema> {
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow!("`settings` must be an array"))?;
        if arr.len() > MAX_SETTINGS_FIELDS {
            bail!("settings: too many fields (>{MAX_SETTINGS_FIELDS})");
        }
        let mut fields: Vec<Field> = Vec::with_capacity(arr.len());
        for item in arr {
            let o = item
                .as_object()
                .ok_or_else(|| anyhow!("settings: each field must be an object"))?;
            // Reject unknown keys so a typo is not silently ignored.
            for k in o.keys() {
                if !matches!(
                    k.as_str(),
                    "key" | "type" | "default" | "label" | "help" | "values"
                ) {
                    bail!("settings: unknown field key {k:?}");
                }
            }
            let key = o
                .get("key")
                .and_then(|k| k.as_str())
                .ok_or_else(|| anyhow!("settings: `key` (a string) is required"))?;
            if !valid_key(key) {
                bail!("settings: invalid key {key:?}");
            }
            if fields.iter().any(|f| f.key == key) {
                bail!("settings: duplicate key {key:?}");
            }
            let type_str = o
                .get("type")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow!("settings: `type` (a string) is required for {key:?}"))?;
            let kind = Kind::parse(type_str)
                .ok_or_else(|| anyhow!("settings: unknown type {type_str:?} for {key:?}"))?;
            let label = match o.get("label") {
                None => None,
                Some(l) => Some(bounded_string(l, MAX_LABEL_BYTES, key, "label")?),
            };
            let help = match o.get("help") {
                None => None,
                Some(h) => Some(bounded_string(h, MAX_LABEL_BYTES, key, "help")?),
            };
            let values = match (kind, o.get("values")) {
                (Kind::Enum, Some(vs)) => {
                    let list = vs
                        .as_array()
                        .ok_or_else(|| anyhow!("settings: {key:?} `values` must be an array"))?;
                    if list.is_empty() || list.len() > MAX_ENUM_VALUES {
                        bail!("settings: {key:?} `values` must have 1..={MAX_ENUM_VALUES} entries");
                    }
                    let mut out: Vec<String> = Vec::with_capacity(list.len());
                    for x in list {
                        let s = x
                            .as_str()
                            .ok_or_else(|| anyhow!("settings: {key:?} `values` must be strings"))?;
                        if s.len() > MAX_ENUM_VALUE_BYTES {
                            bail!("settings: {key:?} enum value too long");
                        }
                        if out.iter().any(|e| e == s) {
                            bail!("settings: {key:?} duplicate enum value {s:?}");
                        }
                        out.push(s.to_string());
                    }
                    out
                }
                (Kind::Enum, None) => bail!("settings: {key:?} (enum) requires `values`"),
                // The schema only *requires* `values` for an enum; it does not
                // forbid it elsewhere, so a stray `values` is ignored (not an
                // error), keeping every schema-valid manifest loadable.
                (_, _) => Vec::new(),
            };
            let default = match o.get("default") {
                None => None,
                Some(_) if kind.is_secret() => {
                    bail!("settings: {key:?} is a secret and must not carry a default")
                }
                Some(d) => {
                    let v =
                        json_to_value(d).with_context(|| format!("settings: {key:?} default"))?;
                    validate_value(kind, &values, key, &v)?;
                    Some(v)
                }
            };
            fields.push(Field {
                key: key.to_string(),
                kind,
                default,
                label,
                help,
                values,
            });
        }
        Ok(Schema { fields })
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }
}

/// A `settings` key, matching `manifest.schema.json`:
/// `^[A-Za-z_][A-Za-z0-9_]*$`.
fn valid_key(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_KEY_BYTES {
        return false;
    }
    let mut bytes = s.bytes();
    let first = bytes.next().unwrap();
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A bounded optional string (a `label`/`help`).
fn bounded_string(v: &serde_json::Value, max: usize, key: &str, what: &str) -> Result<String> {
    let s = v
        .as_str()
        .ok_or_else(|| anyhow!("settings: {key:?} {what} must be a string"))?;
    if s.len() > max {
        bail!("settings: {key:?} {what} too long");
    }
    Ok(s.to_string())
}

/// A JSON scalar to a [`Value`] (the shape the manifest and the settings file
/// use). Numbers stay `Integer` when integral.
fn json_to_value(j: &serde_json::Value) -> Result<Value> {
    Ok(match j {
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Bool(b) => Value::Flag(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Value::Real(f)
            } else {
                bail!("number out of range")
            }
        }
        _ => bail!("expected a string, number or boolean"),
    })
}

/// Checks a value against its field's declared kind and returns it (unchanged
/// when it already matches). This is the single point that keeps a stored value
/// well-typed.
fn validate_value(kind: Kind, enum_values: &[String], key: &str, value: &Value) -> Result<()> {
    // A non-finite real would serialize to JSON `null` and vanish on reload.
    if let (Kind::Number, Value::Real(f)) = (kind, value) {
        if !f.is_finite() {
            bail!("settings: {key:?} must be a finite number");
        }
    }
    let ok = match (kind, value) {
        (Kind::Text | Kind::Url | Kind::Secret, Value::Text(s)) => {
            if s.len() > MAX_TEXT_BYTES {
                bail!("settings: {key:?} value too long");
            }
            // The schema's `format: uri`: a `url` must parse with a scheme.
            if kind == Kind::Url && url::Url::parse(s).is_err() {
                bail!("settings: {key:?} must be a URL (scheme://…)");
            }
            true
        }
        (Kind::Number, Value::Integer(_) | Value::Real(_)) => true,
        (Kind::Bool, Value::Flag(_)) => true,
        (Kind::Enum, Value::Text(s)) => {
            if !enum_values.iter().any(|v| v == s) {
                bail!("settings: {key:?} must be one of {enum_values:?}");
            }
            true
        }
        _ => false,
    };
    if !ok {
        bail!("settings: {key:?} expects type `{}`", kind.as_str());
    }
    Ok(())
}

/// A mod's live settings: its schema and the values currently in effect,
/// resolved once at load (schema defaults, then the stored non-secret file, then
/// the keyring secrets). Shared with the UI through an `Arc<RwLock<_>>`.
#[derive(Clone, Default)]
pub struct Settings {
    pub id: String,
    pub schema: Schema,
    values: BTreeMap<String, Value>,
    /// The non-secret settings file (overridable in tests).
    path: PathBuf,
    /// The current package's fingerprint; secrets are bound to it.
    fingerprint: String,
    /// Secret keys the user set for **another** package of this id (an update,
    /// or a replacement): still in the keyring, withheld until the user keeps
    /// them for this package ([`rebind_secrets`]).
    stale: Vec<String>,
}

/// Redacting `Debug`: `values` may hold secret plaintext, so never print it.
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("id", &self.id)
            .field("fields", &self.schema.fields.len())
            .field("values", &self.values.len())
            .finish_non_exhaustive()
    }
}

impl Settings {
    /// Loads the settings of `id` for `schema` from the default location, binding
    /// secrets to `fingerprint` (the current package).
    pub fn load(id: &str, schema: Schema, fingerprint: &str) -> Settings {
        let path = settings_file().unwrap_or_default();
        Settings::load_impl(id, schema, path, fingerprint.to_string(), &secret_get)
    }

    /// Like [`Settings::load`], reading the non-secret values from `path`, with
    /// no package fingerprint (so keyring secrets are not read — tests).
    pub fn load_at(id: &str, schema: Schema, path: PathBuf) -> Settings {
        Settings::load_impl(id, schema, path, String::new(), &secret_get)
    }

    /// Like [`Settings::load_at`] but with an injectable secret reader, so the
    /// package-fingerprint gate can be tested without a real keyring.
    #[cfg(test)]
    pub fn load_at_with(
        id: &str,
        schema: Schema,
        path: PathBuf,
        fingerprint: &str,
        reader: &dyn Fn(&str, &str) -> Option<String>,
    ) -> Settings {
        Settings::load_impl(id, schema, path, fingerprint.to_string(), reader)
    }

    fn load_impl(
        id: &str,
        schema: Schema,
        path: PathBuf,
        fingerprint: String,
        secret_reader: &dyn Fn(&str, &str) -> Option<String>,
    ) -> Settings {
        let mut values: BTreeMap<String, Value> = BTreeMap::new();
        let mut stale = Vec::new();
        // Fast path: a mod with no settings never touches disk or keyring.
        if !schema.is_empty() {
            let stored = read_mod(&path, id);
            // Secrets are read only when the recorded fingerprint for THAT key
            // matches THIS package: a replacement reusing the id must not read
            // the old credential, and re-authorizing one key must not re-authorize
            // the others (packages are unsigned, so the id is not an identity).
            let pkgs = stored.get(PKG_KEY).and_then(|v| v.as_object());
            for f in &schema.fields {
                if f.kind.is_secret() {
                    let recorded = pkgs.and_then(|m| m.get(&f.key)).and_then(|v| v.as_str());
                    let bound = !fingerprint.is_empty() && recorded == Some(fingerprint.as_str());
                    if !fingerprint.is_empty() && recorded.is_some() && !bound {
                        stale.push(f.key.clone());
                    }
                    if bound {
                        if let Some(s) = secret_reader(id, &f.key) {
                            values.insert(f.key.clone(), Value::Text(s));
                        }
                    }
                    continue;
                }
                if let Some(v) = &f.default {
                    values.insert(f.key.clone(), v.clone());
                }
                if let Some(j) = stored.get(&f.key) {
                    if let Ok(v) = json_to_value(j) {
                        if validate_value(f.kind, &f.values, &f.key, &v).is_ok() {
                            values.insert(f.key.clone(), v);
                        }
                    }
                }
            }
        }
        Settings {
            id: id.to_string(),
            schema,
            values,
            path,
            fingerprint,
            stale,
        }
    }

    /// The secret keys set for another package of this id (see the field).
    pub fn stale_secrets(&self) -> &[String] {
        &self.stale
    }

    /// The package fingerprint these settings were loaded for.
    pub fn package(&self) -> &str {
        &self.fingerprint
    }

    pub fn is_empty(&self) -> bool {
        self.schema.is_empty()
    }

    /// The value in effect for `key`, if any (`settings.get`).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// All resolved values, for the form.
    pub fn values(&self) -> &BTreeMap<String, Value> {
        &self.values
    }

    /// Validates `value` against `key`'s declared kind **without writing**. The
    /// form checks every field with this before persisting any, so a bad field
    /// cannot leave an earlier one written.
    pub fn check(&self, key: &str, value: &Value) -> Result<()> {
        let field = self
            .schema
            .field(key)
            .ok_or_else(|| anyhow!("unknown setting {key:?}"))?;
        validate_value(field.kind, &field.values, key, value)
    }

    /// A mod writing a **non-secret** setting (`settings.set`): validated against
    /// the schema and persisted. Secrets are read-only to the mod.
    pub fn set(&mut self, key: &str, value: Value) -> Result<()> {
        let field = self
            .schema
            .field(key)
            .ok_or_else(|| anyhow!("unknown setting {key:?}"))?;
        if field.kind.is_secret() {
            bail!("settings: {key:?} is a secret and is read-only to the mod");
        }
        validate_value(field.kind, &field.values, key, &value)?;
        store_value(&self.path, &self.id, key, &value.to_json())?;
        self.values.insert(key.to_string(), value);
        Ok(())
    }

    /// The **user** writing a setting from the form: non-secret to the settings
    /// file, secret to the keyring (an empty secret clears it, and a failed clear
    /// is reported so a stale credential cannot silently reappear).
    pub fn set_from_user(&mut self, key: &str, value: Value) -> Result<()> {
        self.check(key, &value)?;
        let is_secret = self
            .schema
            .field(key)
            .map(|f| f.kind.is_secret())
            .unwrap_or(false);
        if is_secret {
            let text = match value {
                Value::Text(s) => s,
                _ => bail!("settings: {key:?} expects text"),
            };
            if text.is_empty() {
                clear_secret_atomic(&self.path, &self.id, key)?;
                self.values.remove(key);
            } else {
                set_secret_atomic(&self.path, &self.id, key, &self.fingerprint, &text)?;
                self.values.insert(key.to_string(), Value::Text(text));
            }
        } else {
            store_value(&self.path, &self.id, key, &value.to_json())?;
            self.values.insert(key.to_string(), value);
        }
        Ok(())
    }

    /// The **user** writing several settings at once: every value is validated
    /// first, the non-secret ones are persisted in a **single** file write, and
    /// secrets go to the keyring (one binding each). Used by the form, so one
    /// Save does not rewrite the file once per field.
    pub fn set_from_user_batch(&mut self, items: &[(String, Value)]) -> Result<()> {
        for (k, v) in items {
            self.check(k, v)?;
        }
        let mut non_secret: Vec<(String, serde_json::Value)> = Vec::new();
        let mut secrets: Vec<(String, String)> = Vec::new();
        for (k, v) in items {
            let is_secret = self
                .schema
                .field(k)
                .map(|f| f.kind.is_secret())
                .unwrap_or(false);
            if is_secret {
                let text = match v {
                    Value::Text(s) => s.clone(),
                    _ => bail!("settings: {k:?} expects text"),
                };
                secrets.push((k.clone(), text));
            } else {
                non_secret.push((k.clone(), v.to_json()));
            }
        }
        // One atomic write for all the non-secret values of this mod.
        if !non_secret.is_empty() {
            let path = self.path.clone();
            let id = self.id.clone();
            modify(&path, &id, move |m| {
                for (k, v) in non_secret {
                    m.insert(k, v);
                }
            })?;
        }
        for (k, v) in items {
            if !self
                .schema
                .field(k)
                .map(|f| f.kind.is_secret())
                .unwrap_or(false)
            {
                self.values.insert(k.clone(), v.clone());
            }
        }
        for (k, text) in secrets {
            if text.is_empty() {
                clear_secret_atomic(&self.path, &self.id, &k)?;
                self.values.remove(&k);
            } else {
                set_secret_atomic(&self.path, &self.id, &k, &self.fingerprint, &text)?;
                self.values.insert(k.clone(), Value::Text(text));
            }
        }
        Ok(())
    }
}

/// A package fingerprint: SHA-256 of the component bytes. Secrets are bound to
/// it, so a package that reuses an id (packages are unsigned) cannot read the
/// credential the user entered for the original package.
pub fn fingerprint(component: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(component);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The default non-secret settings file: a sibling of the mods dir, like
/// `mods-grants.json` (`~/.config/slaq/mods-settings.json`).
pub fn settings_file() -> Result<PathBuf> {
    Ok(crate::mods::default_dir()?.with_file_name("mods-settings.json"))
}

/// Reads the whole file as a map of `mod id -> { key: value }`. A **missing**
/// file is an empty map; a present-but-unreadable one (oversized, not UTF-8, not
/// a JSON object) is an **error**, so a write can never silently discard other
/// mods' settings. Read through a capped reader (a stat-then-read would be a
/// TOCTOU race on the size).
fn read_stored(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Map::new());
        }
        Err(e) => return Err(anyhow!("reading {}: {e}", path.display())),
        Ok(meta) if meta.len() > MAX_SETTINGS_FILE_BYTES => {
            bail!("{} is too large; refusing to touch it", path.display());
        }
        Ok(_) => {}
    }
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = Vec::new();
    file.take(MAX_SETTINGS_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("reading {}", path.display()))?;
    if buf.len() as u64 > MAX_SETTINGS_FILE_BYTES {
        bail!("{} is too large; refusing to touch it", path.display());
    }
    let text = String::from_utf8(buf).map_err(|_| anyhow!("{} is not UTF-8", path.display()))?;
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => bail!("{} is not a JSON object", path.display()),
        Err(e) => Err(anyhow!("parsing {}: {e}", path.display())),
    }
}

/// The lenient form used at load: any read error is "no stored values".
fn read_stored_lenient(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    match read_stored(path) {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!("mods-settings.json ignored: {e}");
            serde_json::Map::new()
        }
    }
}

/// Reads one mod's stored non-secret values from `path` (lenient).
fn read_mod(path: &Path, id: &str) -> serde_json::Map<String, serde_json::Value> {
    read_stored_lenient(path)
        .remove(id)
        .and_then(|v| match v {
            serde_json::Value::Object(o) => Some(o),
            _ => None,
        })
        .unwrap_or_default()
}

/// Read-modify-write `id`'s map in the settings file, under both a process-wide
/// lock (the per-mod `RwLock` does not order two mods or the two front ends) and
/// the cross-process advisory lock the consent store uses (two running front ends
/// must not clobber each other's rename). A present-but-unreadable file is an
/// **error** (never treated as empty, or one mod's write would wipe the others').
fn modify(
    path: &Path,
    id: &str,
    f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<()> {
    let _thread = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _process = crate::mods::consent::lock(path)?;
    modify_locked(path, id, f)
}

/// [`modify`] assuming the process/cross-process locks are already held.
fn modify_locked(
    path: &Path,
    id: &str,
    f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<()> {
    let mut all = read_stored(path)?;
    let mut mod_map = all
        .remove(id)
        .and_then(|v| match v {
            serde_json::Value::Object(o) => Some(o),
            _ => None,
        })
        .unwrap_or_default();
    f(&mut mod_map);
    all.insert(id.to_string(), serde_json::Value::Object(mod_map));
    write_all(path, &serde_json::Value::Object(all))
}

/// Sets `id`'s non-secret `key` to `value`.
fn store_value(path: &Path, id: &str, key: &str, value: &serde_json::Value) -> Result<()> {
    modify(path, id, |m| {
        m.insert(key.to_string(), value.clone());
    })
}

/// Removes `id`'s binding for secret `key` (assumes the locks are held).
fn revoke_binding_locked(path: &Path, id: &str, key: &str) -> Result<()> {
    modify_locked(path, id, |m| {
        if let Some(obj) = m.get_mut(PKG_KEY).and_then(|v| v.as_object_mut()) {
            obj.remove(key);
        }
    })
}

/// Writes `text` as `id`'s secret `key` and binds it to package `fp`, under the
/// process + cross-process locks. The previous binding is revoked **before** the
/// keyring changes, so a failure leaves no marker that could authorize a
/// credential written by another package.
fn set_secret_atomic(path: &Path, id: &str, key: &str, fp: &str, text: &str) -> Result<()> {
    let _thread = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _process = crate::mods::consent::lock(path)?;
    revoke_binding_locked(path, id, key)?;
    secret_set(id, key, text)?;
    modify_locked(path, id, |m| {
        let mut pkgs = m
            .get(PKG_KEY)
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        pkgs.insert(key.to_string(), serde_json::Value::String(fp.to_string()));
        m.insert(PKG_KEY.to_string(), serde_json::Value::Object(pkgs));
    })
}

/// Clears `id`'s secret `key`: revoke the binding first, then delete the keyring
/// entry, so a failure cannot leave a marker for a stub credential.
fn clear_secret_atomic(path: &Path, id: &str, key: &str) -> Result<()> {
    let _thread = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _process = crate::mods::consent::lock(path)?;
    revoke_binding_locked(path, id, key)?;
    secret_delete(id, key)
}

/// The secret keys of `id` saved for a package other than `fp`, from the
/// settings file alone (the keyring is not read) — for the CLI.
pub fn stale_secret_keys(id: &str, schema: &Schema, fp: &str) -> Vec<String> {
    let path = settings_file().unwrap_or_default();
    Settings::load_impl(id, schema.clone(), path, fp.to_string(), &|_, _| None).stale
}

/// Keeps `id`'s secrets `keys` for package `fp` — the user's "Keep" after an
/// update: the binding moves to the new package and the keyring value is
/// untouched. Only keys bound to some package move (a key the user never set
/// stays unset). Returns how many moved.
pub fn rebind_secrets(id: &str, keys: &[String], fp: &str) -> Result<usize> {
    rebind_secrets_at(&settings_file()?, id, keys, fp)
}

/// [`rebind_secrets`] against an explicit settings file (tests).
pub fn rebind_secrets_at(path: &Path, id: &str, keys: &[String], fp: &str) -> Result<usize> {
    if fp.is_empty() {
        bail!("no package fingerprint to keep the secrets for");
    }
    let mut moved = 0;
    modify(path, id, |m| {
        if let Some(pkgs) = m.get_mut(PKG_KEY).and_then(|v| v.as_object_mut()) {
            for k in keys {
                if let Some(v) = pkgs.get_mut(k) {
                    *v = serde_json::Value::String(fp.to_string());
                    moved += 1;
                }
            }
        }
    })?;
    Ok(moved)
}

/// Forgets `id`'s secrets `keys` — the user's "Forget" after an update: the
/// binding and the keyring value go.
pub fn forget_secrets(id: &str, keys: &[String]) -> Result<()> {
    let path = settings_file()?;
    for k in keys {
        clear_secret_atomic(&path, id, k)?;
    }
    Ok(())
}

/// Atomically writes the settings JSON: a **unique** temp file (a fixed name
/// would be clobbered by a concurrent writer) renamed into place. A serialized
/// map over the byte budget is refused, so the file cannot grow unbounded.
fn write_all(path: &Path, value: &serde_json::Value) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("settings file has no parent directory"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let text = serde_json::to_string_pretty(value)?;
    if text.len() as u64 > MAX_SETTINGS_FILE_BYTES {
        bail!("mods-settings.json would exceed {MAX_SETTINGS_FILE_BYTES} bytes; refusing");
    }
    let tmp = dir.join(format!(
        ".mods-settings-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!("renaming into {}: {e}", path.display()));
    }
    Ok(())
}

/// A keyring entry for a mod secret: service `slaq`, account `mod:<id>:<key>`
/// (the full label `slaq:mod:<id>:<key>` of the spec).
fn secret_entry(id: &str, key: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("mod:{id}:{key}"))
        .map_err(|e| anyhow!("keyring error: {e}"))
}

/// Reads a mod secret (any backend/keyring failure is "not set").
pub fn secret_get(id: &str, key: &str) -> Option<String> {
    secret_entry(id, key).ok()?.get_password().ok()
}

/// Writes a mod secret.
pub fn secret_set(id: &str, key: &str, value: &str) -> Result<()> {
    secret_entry(id, key)?
        .set_password(value)
        .context("storing mod secret in the keyring")
}

/// Deletes a mod secret. A genuinely missing entry is success; any other keyring
/// failure is propagated, so a credential that could not be cleared cannot
/// silently reappear.
pub fn secret_delete(id: &str, key: &str) -> Result<()> {
    let entry = secret_entry(id, key)?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow!("deleting the mod secret: {e}")),
    }
}

#[cfg(all(test, feature = "mods"))]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("slaq-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("mods-settings.json")
    }

    /// Regression: keyring 3 with no backend feature silently uses an
    /// in-memory mock, so every secret vanished when slaq exited (a mod's API
    /// key was lost on each restart). The compiled-in store must outlive the
    /// process.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    fn keyring_store_persists() {
        use keyring::credential::CredentialPersistence;
        let p = keyring::default::default_credential_builder().persistence();
        assert!(
            !matches!(p, CredentialPersistence::ProcessOnly | CredentialPersistence::EntryOnly),
            "the keyring store does not persist (an in-memory mock)"
        );
    }



    #[test]
    fn parses_a_schema_and_rejects_bad_ones() {
        let ok = serde_json::json!([
            { "key": "targetLang", "type": "string", "default": "it", "label": "L" },
            { "key": "mode", "type": "enum", "default": "auto", "values": ["auto", "on_demand"] },
            { "key": "endpoint", "type": "url", "default": "http://x/v1" },
            { "key": "apiKey", "type": "secret" }
        ]);
        let s = Schema::parse(&ok).unwrap();
        assert_eq!(s.fields.len(), 4);
        assert_eq!(s.field("mode").unwrap().values.len(), 2);
        assert!(s.field("apiKey").unwrap().kind.is_secret());

        assert!(Schema::parse(&serde_json::json!("nope")).is_err());
        assert!(Schema::parse(&serde_json::json!({ "key": "a", "type": "string" })).is_err());
        assert!(Schema::parse(&serde_json::json!([{ "type": "string" }])).is_err());
        assert!(
            Schema::parse(&serde_json::json!([{ "key": "a", "type": "nope" }])).is_err(),
            "unknown type"
        );
        assert!(
            Schema::parse(&serde_json::json!([{ "key": "a", "type": "enum" }])).is_err(),
            "enum without values"
        );
        // The schema does not forbid `values` on a non-enum: it is ignored, so a
        // schema-valid manifest still loads.
        assert!(
            Schema::parse(&serde_json::json!([
                { "key": "a", "type": "string", "values": ["x"] }
            ]))
            .is_ok()
        );
        // `help` is a schema property; a schema-valid `_`-prefixed key loads.
        let s = Schema::parse(&serde_json::json!([
            { "key": "_x", "type": "string", "help": "hint" }
        ]))
        .unwrap();
        assert_eq!(s.field("_x").unwrap().help.as_deref(), Some("hint"));
        // A schema-invalid key shape is refused.
        assert!(Schema::parse(&serde_json::json!([{ "key": "a.b", "type": "string" }])).is_err());
        assert!(Schema::parse(&serde_json::json!([{ "key": "1x", "type": "string" }])).is_err());
        // A secret must not carry a manifest default.
        assert!(
            Schema::parse(&serde_json::json!([
                { "key": "k", "type": "secret", "default": "x" }
            ]))
            .is_err()
        );
        // A `url` default must parse with a scheme (the schema's `format: uri`).
        assert!(
            Schema::parse(&serde_json::json!([
                { "key": "u", "type": "url", "default": "not a url" }
            ]))
            .is_err()
        );
        assert!(
            Schema::parse(&serde_json::json!([
                { "key": "u", "type": "url", "default": "http://x/v1" }
            ]))
            .is_ok()
        );
        assert!(
            Schema::parse(&serde_json::json!([
                { "key": "a", "type": "string" }, { "key": "a", "type": "number" }
            ]))
            .is_err(),
            "duplicate key"
        );
        assert!(
            Schema::parse(&serde_json::json!([
                { "key": "a", "type": "enum", "values": ["x"], "default": "y" }
            ]))
            .is_err(),
            "default outside the enum"
        );
        assert!(
            Schema::parse(&serde_json::json!([{ "key": "a", "type": "string", "nope": 1 }]))
                .is_err(),
            "unknown field key"
        );
    }

    #[test]
    fn resolves_defaults_and_stored_values() {
        let path = tmp("resolve");
        let schema = Schema::parse(&serde_json::json!([
            { "key": "lang", "type": "string", "default": "it" },
            { "key": "n", "type": "number", "default": 3 }
        ]))
        .unwrap();
        let mut s = Settings::load_at("dev.slaq.x", schema.clone(), path.clone());
        assert_eq!(s.get("lang"), Some(&Value::Text("it".into())));
        assert_eq!(s.get("n"), Some(&Value::Integer(3)));

        s.set("lang", Value::Text("en".into())).unwrap();
        // Reload: the stored value wins over the default.
        let s2 = Settings::load_at("dev.slaq.x", schema, path);
        assert_eq!(s2.get("lang"), Some(&Value::Text("en".into())));
    }

    #[test]
    fn mod_set_is_validated_and_secrets_are_read_only() {
        let path = tmp("setrules");
        let schema = Schema::parse(&serde_json::json!([
            { "key": "mode", "type": "enum", "values": ["a", "b"] },
            { "key": "key", "type": "secret" }
        ]))
        .unwrap();
        let mut s = Settings::load_at("dev.slaq.y", schema, path);
        assert!(s.set("mode", Value::Text("a".into())).is_ok());
        assert!(
            s.set("mode", Value::Text("z".into())).is_err(),
            "not in enum"
        );
        assert!(s.set("mode", Value::Integer(1)).is_err(), "wrong type");
        assert!(
            s.set("nope", Value::Text("x".into())).is_err(),
            "unknown key"
        );
        assert!(
            s.set("key", Value::Text("secret".into())).is_err(),
            "a secret is read-only to the mod"
        );
    }

    /// A secret is released only when the recorded fingerprint matches the
    /// loading package; a different fingerprint (or none) reads nothing.
    #[test]
    fn a_secret_is_released_only_for_the_matching_package() {
        let path = tmp("fp");
        let schema = Schema::parse(&serde_json::json!([
            { "key": "k", "type": "secret" }
        ]))
        .unwrap();
        std::fs::write(
            &path,
            serde_json::json!({ "dev.slaq.x": { "$pkg": { "k": "fp-A" } } }).to_string(),
        )
        .unwrap();
        let reader = |_id: &str, _key: &str| Some("SEKRET".to_string());
        let matched =
            Settings::load_at_with("dev.slaq.x", schema.clone(), path.clone(), "fp-A", &reader);
        assert_eq!(matched.get("k"), Some(&Value::Text("SEKRET".into())));
        let other =
            Settings::load_at_with("dev.slaq.x", schema.clone(), path.clone(), "fp-B", &reader);
        assert_eq!(other.get("k"), None, "another package must not read it");
        let none = Settings::load_at_with("dev.slaq.x", schema, path, "", &reader);
        assert_eq!(none.get("k"), None, "no fingerprint must not read it");
    }

    /// An update (a new fingerprint) sees its secrets as stale, gets nothing,
    /// and gets them once the user keeps them; the old package then loses them,
    /// and a key the user never set is not bound by a keep.
    #[test]
    fn kept_secrets_follow_the_update() {
        let path = tmp("keep");
        let schema = Schema::parse(&serde_json::json!([
            { "key": "k", "type": "secret" },
            { "key": "unset", "type": "secret" }
        ]))
        .unwrap();
        std::fs::write(
            &path,
            serde_json::json!({ "dev.slaq.x": { "$pkg": { "k": "fp-A" } } }).to_string(),
        )
        .unwrap();
        let reader = |_id: &str, _key: &str| Some("SEKRET".to_string());
        let old = Settings::load_at_with("dev.slaq.x", schema.clone(), path.clone(), "fp-A", &reader);
        assert!(old.stale_secrets().is_empty());
        let new = Settings::load_at_with("dev.slaq.x", schema.clone(), path.clone(), "fp-B", &reader);
        assert_eq!(new.stale_secrets(), &["k".to_string()][..]);
        assert_eq!(new.get("k"), None, "withheld until kept");

        let keys = vec!["k".to_string(), "unset".to_string()];
        assert_eq!(rebind_secrets_at(&path, "dev.slaq.x", &keys, "fp-B").unwrap(), 1);
        let kept = Settings::load_at_with("dev.slaq.x", schema.clone(), path.clone(), "fp-B", &reader);
        assert_eq!(kept.get("k"), Some(&Value::Text("SEKRET".into())));
        assert_eq!(kept.get("unset"), None, "a key never set is not bound");
        assert!(kept.stale_secrets().is_empty());
        let back = Settings::load_at_with("dev.slaq.x", schema, path.clone(), "fp-A", &reader);
        assert_eq!(back.get("k"), None, "the old package no longer reads it");
        assert!(rebind_secrets_at(&path, "dev.slaq.x", &keys, "").is_err());
    }

    /// Two mods share one file: a write preserves the other's values, and a
    /// corrupt file is refused rather than silently wiped.
    #[test]
    fn writing_preserves_other_mods_and_refuses_a_corrupt_file() {
        let path = tmp("preserve");
        let schema = Schema::parse(&serde_json::json!([{ "key": "k", "type": "string" }])).unwrap();
        Settings::load_at("dev.slaq.a", schema.clone(), path.clone())
            .set("k", Value::Text("A".into()))
            .unwrap();
        Settings::load_at("dev.slaq.b", schema.clone(), path.clone())
            .set("k", Value::Text("B".into()))
            .unwrap();
        assert_eq!(
            Settings::load_at("dev.slaq.a", schema.clone(), path.clone()).get("k"),
            Some(&Value::Text("A".into()))
        );
        assert_eq!(
            Settings::load_at("dev.slaq.b", schema.clone(), path.clone()).get("k"),
            Some(&Value::Text("B".into()))
        );
        // A corrupt file must not be read as empty and then overwritten.
        std::fs::write(&path, "{ not json").unwrap();
        let mut a = Settings::load_at("dev.slaq.a", schema, path.clone());
        assert!(a.set("k", Value::Text("C".into())).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
