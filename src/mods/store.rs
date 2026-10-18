//! A mod's private store (`store.kv` + `store.vector`), backed by SQLite.
//!
//! One file per mod, `~/.local/share/slaq/mods/<id>/store.db` (opened lazily, so
//! a mod that never touches it creates nothing), with a table per store:
//! `_slaq_kv(key TEXT PRIMARY KEY, value TEXT)` and
//! `_slaq_vec(key TEXT PRIMARY KEY, vec BLOB, meta TEXT)`. Vectors are stored as
//! little-endian `f32`; search is a bounded brute-force cosine (bounded by
//! `maxVectors` and [`MAX_SEARCH_WORK`]). Each store's declared quota
//! (`store.kv.maxBytes`, `store.vector.maxBytes`) is enforced in bytes; the two
//! share one file, so `max_page_count` caps it at their sum. The store persists
//! across reloads, which is what lets a mod keep its own state (e.g. what it has
//! already sent) between runs.
//!
//! A vector row stored under a different `dim` (e.g. after the manifest changes
//! it) is skipped by search and — slice 1 having no delete API — still counts
//! toward the caps.
//!
//! See `docs/mods/manifest-v0.md` ("Storage") and `api-v0.md`.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, params};

/// Cap on `rows × dim` elements scanned by one `vec-search`: fuel/epoch do not
/// meter host-side work, so a search must be bounded explicitly.
const MAX_SEARCH_WORK: usize = 32 * 1024 * 1024;

/// The declared `store.kv` config (`docs/mods/manifest.schema.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KvConfig {
    pub max_bytes: usize,
}

/// The declared `store.vector` config (`docs/mods/manifest.schema.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VectorConfig {
    pub dim: usize,
    pub max_vectors: usize,
    pub max_bytes: usize,
}

/// A mod's private store: the DB path, the declared quotas and a lazily-opened
/// connection. A `Connection` is `Send` (not `Sync`), so it lives directly in the
/// host state, used only through `&mut HostState` while the outer host mutex is
/// held.
pub struct ModStore {
    db_path: PathBuf,
    /// An in-memory store (`:memory:`); the one-shot headless runner uses it, so
    /// a `run` never leaves a file behind.
    memory: bool,
    kv: Option<KvConfig>,
    vector: Option<VectorConfig>,
    conn: Option<Connection>,
    /// Cached byte total of `_slaq_kv`, computed once on open: fuel/epoch do not
    /// meter host SQL, so a per-write `SUM` over the table would be O(rows) per
    /// call (and filling it quadratic).
    kv_bytes: Option<i64>,
    /// Cached row count and byte total of `_slaq_vec`, same reason.
    vec_count: Option<i64>,
    vec_bytes: Option<i64>,
}

impl ModStore {
    /// A store at an explicit DB path (the app and the tests compute it).
    pub fn at_path(
        db_path: PathBuf,
        kv: Option<KvConfig>,
        vector: Option<VectorConfig>,
    ) -> ModStore {
        ModStore {
            db_path,
            memory: false,
            kv,
            vector,
            conn: None,
            kv_bytes: None,
            vec_count: None,
            vec_bytes: None,
        }
    }

    /// An in-memory store: for the one-shot headless runner (nothing is kept).
    pub fn in_memory(kv: Option<KvConfig>, vector: Option<VectorConfig>) -> ModStore {
        ModStore {
            db_path: PathBuf::new(),
            memory: true,
            kv,
            vector,
            conn: None,
            kv_bytes: None,
            vec_count: None,
            vec_bytes: None,
        }
    }

    /// Whether the manifest declared `store.kv`.
    pub fn has_kv(&self) -> bool {
        self.kv.is_some()
    }

    /// Whether the manifest declared `store.vector`.
    pub fn has_vector(&self) -> bool {
        self.vector.is_some()
    }

    /// The byte budget of the shared file: the sum of the declared quotas (a
    /// best-effort `max_page_count` backstop; the logical per-store checks are
    /// authoritative).
    fn file_bytes(&self) -> usize {
        self.kv.map(|c| c.max_bytes).unwrap_or(0) + self.vector.map(|c| c.max_bytes).unwrap_or(0)
    }

    /// Opens (creating if needed) the per-mod DB on first use, and the tables of
    /// the declared stores.
    fn open(&mut self) -> Result<()> {
        if self.conn.is_none() {
            let conn = if self.memory {
                Connection::open_in_memory().context("opening the in-memory mod store")?
            } else {
                let dir = self
                    .db_path
                    .parent()
                    .ok_or_else(|| anyhow!("mod store db has no parent directory"))?;
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                Connection::open(&self.db_path)
                    .with_context(|| format!("opening {}", self.db_path.display()))?
            };
            // If two processes ever share a data dir, a writer waits rather than
            // failing immediately with SQLITE_BUSY.
            let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
            // A best-effort backstop on the file, using the real page size, with
            // headroom for the header/b-tree/index pages, so the logical per-store
            // checks normally bind first (many very short rows can still reach the
            // physical cap); the floor keeps a tiny quota's schema creatable.
            let page_size: i64 = conn
                .query_row("PRAGMA page_size", [], |r| r.get(0))
                .unwrap_or(4096)
                .max(1);
            let pages = ((self.file_bytes() as i64) + page_size - 1) / page_size + 16;
            if let Err(e) = conn.pragma_update(None, "max_page_count", pages.max(24)) {
                tracing::warn!("mod store: cannot cap page count: {e}");
            }
            if self.kv.is_some() {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS _slaq_kv(
                         key TEXT PRIMARY KEY,
                         value TEXT NOT NULL
                     );",
                )
                .context("creating _slaq_kv")?;
            }
            if self.vector.is_some() {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS _slaq_vec(
                         key TEXT PRIMARY KEY,
                         vec BLOB NOT NULL,
                         meta TEXT
                     );",
                )
                .context("creating _slaq_vec")?;
            }
            self.conn = Some(conn);
        }
        Ok(())
    }

    /// The open connection (call [`Self::open`] first).
    fn connection(&self) -> &Connection {
        self.conn.as_ref().expect("mod store: connection opened")
    }

    // --- key/value ---------------------------------------------------------

    /// The value stored for `key`, if any.
    pub fn kv_get(&mut self, key: &str) -> Result<Option<String>> {
        self.open()?;
        Ok(self
            .connection()
            .query_row(
                "SELECT value FROM _slaq_kv WHERE key=?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Store `key = value`, enforcing the declared `store.kv.maxBytes` (the byte
    /// total of the table after the write; the baseline is measured in **bytes**,
    /// matching the Rust `len`). The total is cached, so the cost is O(1) per
    /// write, not O(rows).
    pub fn kv_set(&mut self, key: &str, value: &str) -> Result<()> {
        let max = self
            .kv
            .ok_or_else(|| anyhow!("store.kv not declared"))?
            .max_bytes as i64;
        self.open()?;
        if self.kv_bytes.is_none() {
            let sum = self.connection().query_row(
                "SELECT COALESCE(SUM(length(CAST(key AS BLOB))+length(CAST(value AS BLOB))),0) \
                 FROM _slaq_kv",
                [],
                |r| r.get(0),
            )?;
            self.kv_bytes = Some(sum);
        }
        let old: i64 = self
            .connection()
            .query_row(
                "SELECT length(CAST(key AS BLOB))+length(CAST(value AS BLOB)) \
                 FROM _slaq_kv WHERE key=?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let projected = self.kv_bytes.unwrap() - old + (key.len() + value.len()) as i64;
        if projected > max {
            bail!("store.kv: exceeds maxBytes ({max})");
        }
        self.connection().execute(
            "INSERT INTO _slaq_kv(key, value) VALUES(?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        self.kv_bytes = Some(projected);
        Ok(())
    }

    // --- vector ------------------------------------------------------------

    /// Upsert `(key, vector, meta)` items (last-wins for a repeated key).
    /// Validates `dim`, `maxVectors` and `maxBytes` (projecting the total after
    /// the write) before committing.
    pub fn upsert(&mut self, items: &[(String, Vec<f32>, Option<String>)]) -> Result<()> {
        let config = self
            .vector
            .ok_or_else(|| anyhow!("store.vector not declared"))?;
        for (key, vec, meta) in items {
            if key.len() > crate::mods::MAX_KEY_BYTES {
                bail!("key exceeds {} bytes", crate::mods::MAX_KEY_BYTES);
            }
            if vec.len() != config.dim {
                bail!("vector dim {} != declared {}", vec.len(), config.dim);
            }
            if vec.iter().any(|f| !f.is_finite()) {
                bail!("vector has a non-finite value");
            }
            if let Some(m) = meta {
                if m.len() > crate::mods::MAX_VALUE_BYTES {
                    bail!("meta exceeds {} bytes", crate::mods::MAX_VALUE_BYTES);
                }
            }
        }
        // Last-wins per key, so the projection counts each key once.
        let mut dedup: Vec<&(String, Vec<f32>, Option<String>)> = Vec::new();
        for item in items {
            match dedup.iter_mut().find(|e| e.0 == item.0) {
                Some(slot) => *slot = item,
                None => dedup.push(item),
            }
        }
        let dim = config.dim;
        let (max_vectors, max_bytes) = (config.max_vectors as i64, config.max_bytes as i64);
        self.open()?;
        // The row count and byte total are cached (a per-write `COUNT`/`SUM` would
        // be O(rows) unmetered host work, and filling the store quadratic). Fill
        // both together, so neither can be observed as `None` while the other is
        // set.
        if self.vec_count.is_none() || self.vec_bytes.is_none() {
            let (c, b): (i64, i64) = self.connection().query_row(
                "SELECT COUNT(*), COALESCE(SUM(\
                     length(CAST(key AS BLOB))+length(vec)+COALESCE(length(CAST(meta AS BLOB)),0)\
                 ),0) FROM _slaq_vec",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            self.vec_count = Some(c);
            self.vec_bytes = Some(b);
        }
        let mut count = self.vec_count.unwrap();
        let mut bytes = self.vec_bytes.unwrap();
        for (key, _, meta) in &dedup {
            let existing: Option<i64> = self
                .connection()
                .query_row(
                    "SELECT length(CAST(key AS BLOB))+length(vec)+COALESCE(length(CAST(meta AS BLOB)),0) \
                     FROM _slaq_vec WHERE key=?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?;
            match existing {
                Some(old) => bytes -= old,
                None => count += 1,
            }
            bytes += (key.len() + dim * 4 + meta.as_ref().map(|m| m.len()).unwrap_or(0)) as i64;
        }
        if count > max_vectors {
            bail!("store.vector: exceeds maxVectors ({max_vectors})");
        }
        if bytes > max_bytes {
            bail!("store.vector: exceeds maxBytes ({max_bytes})");
        }
        {
            let conn = self.connection();
            let tx = conn.unchecked_transaction()?;
            for (key, vec, meta) in &dedup {
                tx.execute(
                    "INSERT INTO _slaq_vec(key, vec, meta) VALUES(?1, ?2, ?3) \
                     ON CONFLICT(key) DO UPDATE SET vec=excluded.vec, meta=excluded.meta",
                    params![key, f32_bytes(vec), meta],
                )?;
            }
            tx.commit()?;
        }
        self.vec_count = Some(count);
        self.vec_bytes = Some(bytes);
        Ok(())
    }

    /// Removes the rows with these keys (a missing key is ignored) and returns
    /// how many went. Keeps the cached count/bytes in step, so the quotas stay
    /// exact without a rescan.
    pub fn delete(&mut self, keys: &[String]) -> Result<u32> {
        self.vector
            .ok_or_else(|| anyhow!("store.vector not declared"))?;
        self.open()?;
        let mut removed = 0u32;
        let mut freed = 0i64;
        {
            let conn = self.connection();
            let tx = conn.unchecked_transaction()?;
            for key in keys {
                let size: Option<i64> = tx
                    .query_row(
                        "SELECT length(CAST(key AS BLOB))+length(vec)+COALESCE(length(CAST(meta AS BLOB)),0) \
                         FROM _slaq_vec WHERE key=?1",
                        params![key],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(size) = size {
                    tx.execute("DELETE FROM _slaq_vec WHERE key=?1", params![key])?;
                    removed += 1;
                    freed += size;
                }
            }
            tx.commit()?;
        }
        // Adjust both caches together (never one without the other).
        if let (Some(c), Some(b)) = (self.vec_count, self.vec_bytes) {
            self.vec_count = Some(c - removed as i64);
            self.vec_bytes = Some(b - freed);
        }
        Ok(removed)
    }

    /// The `k` nearest rows to `query` (cosine), optionally filtered by key
    /// prefix, as `(key, score, meta)` best-first.
    pub fn search(
        &mut self,
        query: &[f32],
        k: usize,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, f32, Option<String>)>> {
        let config = self
            .vector
            .ok_or_else(|| anyhow!("store.vector not declared"))?;
        if query.len() != config.dim {
            bail!("query dim {} != declared {}", query.len(), config.dim);
        }
        if query.iter().any(|f| !f.is_finite()) {
            bail!("query has a non-finite value");
        }
        let dim = config.dim;
        self.open()?;
        // Read the count for the budget but do NOT cache it here: `upsert` fills
        // `vec_count` and `vec_bytes` **together**, and a lone `vec_count` would
        // make it unwrap a `None` `vec_bytes` (a guest-triggerable panic).
        let rows: i64 = match self.vec_count {
            Some(c) => c,
            None => self
                .connection()
                .query_row("SELECT COUNT(*) FROM _slaq_vec", [], |r| r.get(0))?,
        };
        if (rows as usize).saturating_mul(dim) > MAX_SEARCH_WORK {
            bail!("vec-search: {rows} vectors of dim {dim} exceed the scan budget");
        }
        let mut stmt = self
            .connection()
            .prepare("SELECT key, vec, meta FROM _slaq_vec")?;
        let rows = stmt.query_map([], |r| {
            let key: String = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let meta: Option<String> = r.get(2)?;
            Ok((key, blob, meta))
        })?;
        let mut hits: Vec<(String, f32, Option<String>)> = Vec::new();
        for row in rows {
            let (key, blob, meta) = row?;
            if let Some(p) = prefix {
                if !key.starts_with(p) {
                    continue;
                }
            }
            let vec = bytes_f32(&blob);
            if vec.len() != dim {
                continue; // a differently-dimensioned row cannot be compared
            }
            hits.push((key, cosine(query, &vec), meta));
        }
        // Best first; a stable tie-break by key keeps output deterministic.
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        hits.truncate(k);
        Ok(hits)
    }
}

/// `f32` slice to little-endian bytes (the stored BLOB).
fn f32_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// The inverse of [`f32_bytes`] (trailing partial values are dropped).
fn bytes_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Cosine similarity, accumulated in `f64` (an `f32` accumulator can overflow to
/// `NaN` for large components); `0.0` when either vector is zero.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        (dot / (na.sqrt() * nb.sqrt())) as f32
    }
}

#[cfg(all(test, feature = "mods"))]
mod tests {
    use super::*;

    fn db(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("slaq-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("store.db")
    }

    fn vec_cfg(dim: usize, max_vectors: usize, max_bytes: usize) -> Option<VectorConfig> {
        Some(VectorConfig {
            dim,
            max_vectors,
            max_bytes,
        })
    }

    #[test]
    fn kv_set_get_and_persist() {
        let path = db("kv-persist");
        let kv = Some(KvConfig { max_bytes: 1 << 20 });
        {
            let mut s = ModStore::at_path(path.clone(), kv, None);
            // An unused store creates no file.
            assert!(!path.exists(), "lazy: no file before first use");
            assert_eq!(s.kv_get("a").unwrap(), None);
            s.kv_set("a", "1").unwrap();
            s.kv_set("b", "2").unwrap();
            assert_eq!(s.kv_get("a").unwrap().as_deref(), Some("1"));
        }
        // A fresh store on the same path still sees the keys (persisted).
        let mut s = ModStore::at_path(path, kv, None);
        assert_eq!(s.kv_get("b").unwrap().as_deref(), Some("2"), "persisted");
    }

    #[test]
    fn kv_max_bytes_is_enforced() {
        let path = db("kv-cap");
        let mut s = ModStore::at_path(path, Some(KvConfig { max_bytes: 3 }), None);
        // "k"+"v" = 2 bytes fits under the 3-byte cap.
        s.kv_set("k", "v").unwrap();
        // A second key pushes it past 3 bytes.
        assert!(s.kv_set("k2", "v").is_err(), "maxBytes");
        // Replacing the existing key is free.
        s.kv_set("k", "vv").unwrap();
    }

    /// KV and vector live in one file but their quotas are independent: a large
    /// vector row must not count against `store.kv.maxBytes`.
    #[test]
    fn kv_and_vector_coexist_with_independent_quotas() {
        let path = db("coexist");
        let kv = Some(KvConfig { max_bytes: 8 });
        let vec = vec_cfg(2, 10, 1 << 20);
        let mut s = ModStore::at_path(path, kv, vec);
        s.upsert(&[("v".into(), vec![1.0, 0.0], None)]).unwrap();
        // 7 bytes fit under the 8-byte kv cap, unchanged by the vector row.
        s.kv_set("k", "123456").unwrap();
        assert!(
            s.kv_set("k2", "1").is_err(),
            "the kv cap must ignore the vector rows"
        );
        assert_eq!(s.kv_get("k").unwrap().as_deref(), Some("123456"));
        // The vector side still works on the same store.
        assert_eq!(s.search(&[1.0, 0.0], 1, None).unwrap()[0].0, "v");
    }

    /// A `search` before the first `upsert` must not poison the byte cache: the
    /// next `upsert` used to panic (guest-triggerable, and persistent).
    #[test]
    fn search_before_upsert_does_not_panic() {
        let path = db("search-then-upsert");
        let mut s = ModStore::at_path(path, None, vec_cfg(2, 10, 1 << 20));
        // First op is a search on an empty store.
        assert!(s.search(&[1.0, 0.0], 1, None).unwrap().is_empty());
        // Then an upsert must work (no panic), and a later search sees it.
        s.upsert(&[("a".into(), vec![1.0, 0.0], None)]).unwrap();
        assert_eq!(s.search(&[1.0, 0.0], 1, None).unwrap()[0].0, "a");
    }

    #[test]
    fn upsert_search_and_persist() {
        let path = db("vec-persist");
        let cfg = vec_cfg(3, 10, 1 << 20);
        {
            let mut s = ModStore::at_path(path.clone(), None, cfg);
            assert!(!path.exists(), "lazy: no file before first use");
            s.upsert(&[
                ("a".into(), vec![1.0, 0.0, 0.0], None),
                ("b".into(), vec![0.0, 1.0, 0.0], Some("meta b".into())),
            ])
            .unwrap();
            let hits = s.search(&[1.0, 0.1, 0.0], 1, None).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].0, "a");
            assert!(hits[0].1 > 0.9);
            let hits = s.search(&[0.0, 1.0, 0.0], 5, Some("b")).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].0, "b");
            assert_eq!(hits[0].2.as_deref(), Some("meta b"));
        }
        let mut s = ModStore::at_path(path, None, cfg);
        let hits = s.search(&[1.0, 0.0, 0.0], 5, None).unwrap();
        assert!(hits.iter().any(|(k, _, _)| k == "a"), "persisted: {hits:?}");
    }

    /// Deleting frees quota: a full store accepts a new key once one is
    /// deleted, missing keys are ignored, and the count survives a reopen.
    #[test]
    fn delete_frees_quota() {
        let path = db("vec-delete");
        let cfg = vec_cfg(2, 2, 1 << 20);
        let mut s = ModStore::at_path(path.clone(), None, cfg);
        s.upsert(&[
            ("a".into(), vec![1.0, 0.0], None),
            ("b".into(), vec![0.0, 1.0], None),
        ])
        .unwrap();
        assert!(s.upsert(&[("c".into(), vec![1.0, 1.0], None)]).is_err(), "full");
        assert_eq!(s.delete(&["a".into(), "zzz".into()]).unwrap(), 1);
        s.upsert(&[("c".into(), vec![1.0, 1.0], None)]).unwrap();
        let keys: Vec<String> = s.search(&[1.0, 0.0], 5, None).unwrap().into_iter().map(|h| h.0).collect();
        assert!(!keys.contains(&"a".to_string()) && keys.contains(&"c".to_string()));
        let mut s = ModStore::at_path(path, None, cfg);
        assert!(s.upsert(&[("d".into(), vec![1.0, 1.0], None)]).is_err(), "still full after reopen");
    }

    #[test]
    fn dim_and_caps_are_enforced() {
        let path = db("caps");
        let cfg = vec_cfg(2, 1, 1 << 20);
        let mut s = ModStore::at_path(path.clone(), None, cfg);
        assert!(
            s.upsert(&[("a".into(), vec![1.0], None)]).is_err(),
            "wrong dim"
        );
        s.upsert(&[("a".into(), vec![1.0, 0.0], None)]).unwrap();
        assert!(
            s.upsert(&[("b".into(), vec![0.0, 1.0], None)]).is_err(),
            "maxVectors"
        );
        s.upsert(&[
            ("a".into(), vec![0.0, 1.0], None),
            ("a".into(), vec![0.5, 0.5], None),
        ])
        .unwrap();
        let mut small = ModStore::at_path(path, None, vec_cfg(2, 10, 4));
        assert!(
            small.upsert(&[("k".into(), vec![1.0, 0.0], None)]).is_err(),
            "maxBytes"
        );
    }

    #[test]
    fn a_dim_mismatched_row_is_skipped() {
        let path = db("dimskip");
        let mut s = ModStore::at_path(path, None, vec_cfg(2, 10, 1 << 20));
        s.upsert(&[("a".into(), vec![1.0, 0.0], None)]).unwrap();
        {
            let conn = Connection::open(s.db_path.clone()).unwrap();
            conn.execute(
                "INSERT INTO _slaq_vec(key, vec, meta) VALUES('odd', ?1, NULL)",
                params![f32_bytes(&[1.0, 0.0, 0.0])],
            )
            .unwrap();
        }
        let hits = s.search(&[1.0, 0.0], 5, None).unwrap();
        assert_eq!(hits.len(), 1, "only the dim-2 row is compared: {hits:?}");
        assert_eq!(hits[0].0, "a");
    }
}
