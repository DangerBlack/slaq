//! The mod's private SQL database (`store.sql`): a per-mod `scratch.db`, opened
//! and owned by the **host**, with the hardening `docs/mods/manifest-v0.md`
//! requires.
//!
//! The mod never sees a path, and the connection is used only through
//! `&mut ModSql`. DDL is allowed (it is the mod's own DB), but `ATTACH`/`DETACH`,
//! non-allow-listed pragmas and the host's reserved `_slaq_` namespace are denied
//! by an authorizer, and the statement length/complexity, the run time, the file
//! size and the returned bytes are bounded. `store.sql` is **not** a door onto
//! Slack messages: those stay behind `messages.read`.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use rusqlite::config::DbConfig;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::{Value, ValueRef};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The declared `store.sql` config (`{ maxBytes, maxResultRows }`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlConfig {
    /// The `scratch.db` file quota (applied as `max_page_count`).
    pub max_bytes: usize,
    /// Rows a single result may return (`maxResultRows`).
    pub max_result_rows: u64,
}

/// One statement's result, ready for the WIT boundary.
#[derive(Debug)]
pub struct SqlResult {
    /// The column names (`[]` for a statement that returns no rows).
    pub columns: Vec<String>,
    /// The rows, each with one value per column.
    pub rows: Vec<Vec<SqlValue>>,
    /// The rows the statement changed (0 for a read).
    pub changes: u64,
}

/// A value crossing the WIT boundary.
#[derive(Debug)]
pub enum SqlValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

// Bounds (`docs/mods/manifest-v0.md`, the `store.sql` hardening table).
const MAX_SQL_BYTES: i32 = 64 * 1024; // SQLITE_LIMIT_SQL_LENGTH
const MAX_VALUE_BYTES: i32 = 2 * 1024 * 1024; // SQLITE_LIMIT_LENGTH (string/blob/row)
const MAX_EXPR_DEPTH: i32 = 64; // SQLITE_LIMIT_EXPR_DEPTH
const MAX_COMPOUND_SELECT: i32 = 8; // SQLITE_LIMIT_COMPOUND_SELECT
const MAX_COLUMNS: i32 = 128; // SQLITE_LIMIT_COLUMN
const SQL_TIMEOUT: Duration = Duration::from_secs(2);
/// How often (in VM ops) the progress handler checks the deadline.
const SQL_PROGRESS_OPS: i32 = 20_000;
/// Aggregate bytes one result may hand to the guest (a computed value cannot be
/// copied across the boundary without limit).
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;
/// Parameters bound to one statement.
const MAX_PARAMS: usize = 1000;
/// Statements in one `sql-tx`.
const MAX_TX_STATEMENTS: usize = 64;

/// The pragmas a mod may run: read-only introspection plus `user_version`/
/// `application_id` (a migration idiom). Everything else is denied, so
/// `journal_mode`/`writable_schema`-style knobs are out of reach (defensive mode
/// blocks the dangerous ones regardless).
const PRAGMA_ALLOWLIST: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_info",
    "index_list",
    "index_xinfo",
    "foreign_key_list",
    "database_list",
    "user_version",
    "application_id",
    "page_count",
    "page_size",
    "freelist_count",
    "encoding",
];

/// Of the allow-listed pragmas, these may only be **read**: writing them would
/// reinterpret `max_page_count` (the file quota), so `PRAGMA page_size = 65536`
/// could 16× the persisted bytes.
const PRAGMA_READ_ONLY_ONLY: &[&str] = &["page_size", "encoding"];

/// The host's reserved namespace. `scratch.db` is a separate file, so these
/// tables cannot exist there; they are denied anyway, so the rule holds whichever
/// way the storage is laid out.
const RESERVED_PREFIX: &str = "_slaq_";

/// A per-mod `scratch.db`, opened lazily on first use. A `rusqlite::Connection`
/// is `Send` (not `Sync`), so it lives directly in the store.
pub struct ModSql {
    conn: Option<Connection>,
    /// `None` for an in-memory DB (tests).
    db_path: Option<PathBuf>,
    cfg: SqlConfig,
    /// The nanosecond deadline the running statement is killed at (0 = disarmed);
    /// read by the progress handler.
    deadline_ns: Arc<AtomicU64>,
}

impl ModSql {
    pub fn at_path(db_path: PathBuf, cfg: SqlConfig) -> ModSql {
        ModSql {
            conn: None,
            db_path: Some(db_path),
            cfg,
            deadline_ns: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn in_memory(cfg: SqlConfig) -> ModSql {
        ModSql {
            conn: None,
            db_path: None,
            cfg,
            deadline_ns: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Opens the connection and applies the hardening, once.
    fn open(&mut self) -> Result<()> {
        if self.conn.is_some() {
            return Ok(());
        }
        let conn = match &self.db_path {
            None => Connection::open_in_memory().context("opening the in-memory scratch DB")?,
            Some(p) => {
                // A `store.sql`-only mod has no KV/vector store to create the
                // directory first, so it is created here too.
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir)
                        .with_context(|| format!("creating {}", dir.display()))?;
                }
                Connection::open(p).with_context(|| format!("opening {}", p.display()))?
            }
        };
        // One running slaq instance owns a mod's data dir; a second waits.
        let _ = conn.busy_timeout(Duration::from_secs(5));
        // No dangerous schema writes (and, since the crate is built without the
        // `load_extension` feature, no native code either).
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
            .context("enabling defensive mode")?;
        // No external files: no database may be attached.
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
        // Complexity and value limits.
        conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, MAX_SQL_BYTES);
        conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_VALUE_BYTES);
        conn.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, MAX_EXPR_DEPTH);
        conn.set_limit(Limit::SQLITE_LIMIT_COMPOUND_SELECT, MAX_COMPOUND_SELECT);
        conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, MAX_COLUMNS);
        // The file quota. `max_page_count` must be set before the authorizer (it
        // is itself a pragma), and it bounds the **in-memory** store too.
        let page_size: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .unwrap_or(4096)
            .max(1);
        let pages = ((self.cfg.max_bytes as i64) / page_size).max(24);
        if let Err(e) = conn.pragma_update(None, "max_page_count", pages) {
            tracing::debug!("mod scratch: max_page_count not set: {e}");
        }
        // Interrupt a statement that runs past its deadline.
        let deadline = self.deadline_ns.clone();
        conn.progress_handler(
            SQL_PROGRESS_OPS,
            Some(move || {
                let d = deadline.load(Ordering::Relaxed);
                d != 0 && now_ns() >= d
            }),
        );
        // The authorizer: the hardening rules SQLite can only express here.
        conn.authorizer(Some(|ctx: AuthContext<'_>| -> Authorization {
            match ctx.action {
                AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
                AuthAction::Pragma {
                    pragma_name,
                    pragma_value,
                } => {
                    let name = pragma_name.to_ascii_lowercase();
                    if !PRAGMA_ALLOWLIST.iter().any(|p| *p == name.as_str()) {
                        // An unknown/other pragma: denied so `journal_mode`,
                        // `max_page_count`, `temp_store_directory` … are out.
                        Authorization::Deny
                    } else if PRAGMA_READ_ONLY_ONLY.contains(&name.as_str())
                        && pragma_value.is_some()
                    {
                        // Setting `page_size` would reinterpret `max_page_count`
                        // (the file quota) — 16× the bytes at a 64 KiB page.
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }
                AuthAction::Function { function_name }
                    if function_name.eq_ignore_ascii_case("load_extension") =>
                {
                    // The per-connection flag is off, so this is already
                    // unauthorized; denying it keeps native-code loading out even
                    // if the flag were ever enabled.
                    Authorization::Deny
                }
                // A temp table would live in a process temp file, outside
                // `scratch.db` and therefore outside `maxBytes`: denied. (A query's
                // own transient spill is bounded by the deadline and freed when the
                // statement ends.)
                AuthAction::CreateTempTable { .. }
                | AuthAction::CreateTempIndex { .. }
                | AuthAction::CreateTempTrigger { .. }
                | AuthAction::CreateTempView { .. } => Authorization::Deny,
                ref action if reserved(action) => Authorization::Deny,
                _ => Authorization::Allow,
            }
        }));
        self.conn = Some(conn);
        Ok(())
    }

    /// Runs one statement (`sql-tx` goes through [`Self::tx`]).
    pub fn query(&mut self, sql: &str, params: &[SqlValue]) -> Result<SqlResult> {
        self.open()?;
        let conn = self.conn.as_ref().expect("opened");
        arm(&self.deadline_ns);
        let out = run_stmt(conn, self.cfg, sql, params);
        disarm(&self.deadline_ns);
        out
    }

    /// Runs several statements in **one** transaction, one result each.
    pub fn tx(&mut self, statements: &[(String, Vec<SqlValue>)]) -> Result<Vec<SqlResult>> {
        if statements.is_empty() {
            bail!("empty transaction");
        }
        if statements.len() > MAX_TX_STATEMENTS {
            bail!("too many statements (>{MAX_TX_STATEMENTS})");
        }
        self.open()?;
        let conn = self.conn.as_mut().expect("opened");
        // **One** deadline for the whole transaction: a per-statement one would
        // let `MAX_TX_STATEMENTS` statements block the dispatch thread (and the
        // serialized host lock) for up to that many times the bound, and the guest
        // is not executing during a host call, so the epoch cannot interrupt it.
        arm(&self.deadline_ns);
        let out = (|| -> Result<Vec<SqlResult>> {
            let tx = conn.transaction().context("cannot begin the transaction")?;
            let mut out = Vec::with_capacity(statements.len());
            for (sql, params) in statements {
                out.push(run_stmt(&tx, self.cfg, sql, params)?);
            }
            tx.commit().context("cannot commit the transaction")?;
            Ok(out)
        })();
        disarm(&self.deadline_ns);
        out
    }
}

/// Runs one statement on `conn`, bounded. Only the **first** statement runs:
/// SQLite stops at the end of it and silently ignores the rest, so pass one
/// statement per call (`sql-tx` takes several).
fn run_stmt(
    conn: &Connection,
    cfg: SqlConfig,
    sql: &str,
    params: &[SqlValue],
) -> Result<SqlResult> {
    if sql.trim().is_empty() {
        bail!("empty statement");
    }
    if sql.len() > MAX_SQL_BYTES as usize {
        bail!("statement too long (>{MAX_SQL_BYTES} bytes)");
    }
    if params.len() > MAX_PARAMS {
        bail!("too many parameters (>{MAX_PARAMS})");
    }
    let before = conn.total_changes();
    let mut result = (|| -> Result<SqlResult> {
        let mut stmt = conn.prepare(sql).context("cannot prepare")?;
        let columns: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let bound: Vec<Value> = params.iter().map(to_rusqlite).collect();
        let mut rows_out: Vec<Vec<SqlValue>> = Vec::new();
        let mut bytes = 0usize;
        let mut rows = stmt.query(rusqlite::params_from_iter(bound.iter()))?;
        while let Some(row) = rows.next()? {
            if rows_out.len() as u64 >= cfg.max_result_rows {
                bail!("result exceeds maxResultRows ({})", cfg.max_result_rows);
            }
            let mut out = Vec::with_capacity(columns.len());
            for i in 0..columns.len() {
                let v = from_rusqlite(row.get_ref(i)?);
                bytes += value_bytes(&v);
                if bytes > MAX_RESULT_BYTES {
                    bail!("result exceeds the byte cap");
                }
                out.push(v);
            }
            rows_out.push(out);
        }
        Ok(SqlResult {
            columns,
            rows: rows_out,
            changes: 0,
        })
    })()?;
    result.changes = conn.total_changes().saturating_sub(before);
    Ok(result)
}

/// Arms the wall-clock deadline the progress handler reads.
fn arm(deadline: &AtomicU64) {
    deadline.store(
        now_ns().saturating_add(SQL_TIMEOUT.as_nanos() as u64),
        Ordering::Relaxed,
    );
}

/// Disarms it (only the call that armed it may be interrupted).
fn disarm(deadline: &AtomicU64) {
    deadline.store(0, Ordering::Relaxed);
}

/// Whether an action touches the host's reserved namespace.
fn reserved(action: &AuthAction<'_>) -> bool {
    use AuthAction::{
        AlterTable, Analyze, CreateIndex, CreateTable, CreateTempIndex, CreateTempTable,
        CreateTempTrigger, CreateTempView, CreateTrigger, CreateView, CreateVtable, Delete,
        DropIndex, DropTable, DropTempIndex, DropTempTable, DropTempTrigger, DropTempView,
        DropTrigger, DropView, DropVtable, Insert, Read, Reindex, Update,
    };
    let name = match action {
        Read { table_name, .. }
        | Insert { table_name }
        | Update { table_name, .. }
        | Delete { table_name }
        | CreateTable { table_name }
        | DropTable { table_name }
        | AlterTable { table_name, .. }
        | Analyze { table_name }
        | CreateIndex { table_name, .. }
        | DropIndex { table_name, .. }
        | CreateTrigger { table_name, .. }
        | DropTrigger { table_name, .. }
        | CreateTempTable { table_name }
        | DropTempTable { table_name }
        | CreateTempIndex { table_name, .. }
        | DropTempIndex { table_name, .. }
        | CreateTempTrigger { table_name, .. }
        | DropTempTrigger { table_name, .. }
        | CreateVtable { table_name, .. }
        | DropVtable { table_name, .. } => table_name,
        CreateView { view_name }
        | DropView { view_name }
        | CreateTempView { view_name }
        | DropTempView { view_name } => view_name,
        Reindex { index_name } => index_name,
        _ => return false,
    };
    name.to_ascii_lowercase().starts_with(RESERVED_PREFIX)
}

fn to_rusqlite(v: &SqlValue) -> Value {
    match v {
        SqlValue::Null => Value::Null,
        SqlValue::Int(i) => Value::Integer(*i),
        SqlValue::Real(f) => Value::Real(*f),
        SqlValue::Text(s) => Value::Text(s.clone()),
        SqlValue::Blob(b) => Value::Blob(b.clone()),
    }
}

fn from_rusqlite(v: ValueRef<'_>) -> SqlValue {
    match v {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(i) => SqlValue::Int(i),
        ValueRef::Real(f) => SqlValue::Real(f),
        ValueRef::Text(t) => SqlValue::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
    }
}

/// The bytes a value contributes to the aggregate result cap.
fn value_bytes(v: &SqlValue) -> usize {
    match v {
        SqlValue::Text(s) => s.len(),
        SqlValue::Blob(b) => b.len(),
        _ => 8,
    }
}

/// A **monotonic** nanosecond clock (a backwards NTP step must not disable the
/// statement timeout).
fn now_ns() -> u64 {
    static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SqlConfig {
        SqlConfig {
            max_bytes: 1024 * 1024,
            max_result_rows: 100,
        }
    }

    fn text(v: &SqlValue) -> &str {
        match v {
            SqlValue::Text(s) => s,
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn ddl_and_dml_round_trip() {
        let mut db = ModSql::in_memory(cfg());
        let r = db
            .query("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", &[])
            .unwrap();
        assert!(r.columns.is_empty());
        let r = db
            .query(
                "INSERT INTO t (name) VALUES (?1)",
                &[SqlValue::Text("a".into())],
            )
            .unwrap();
        assert_eq!(r.changes, 1);
        let r = db.query("SELECT id, name FROM t", &[]).unwrap();
        assert_eq!(r.columns, vec!["id", "name"]);
        assert_eq!(r.rows.len(), 1);
        assert!(matches!(r.rows[0][0], SqlValue::Int(1)));
        assert_eq!(text(&r.rows[0][1]), "a");
    }

    #[test]
    fn attach_and_dangerous_pragmas_are_denied() {
        let mut db = ModSql::in_memory(cfg());
        assert!(
            db.query("ATTACH DATABASE '/etc/passwd' AS x", &[]).is_err(),
            "ATTACH is denied"
        );
        assert!(
            db.query("PRAGMA journal_mode = WAL", &[]).is_err(),
            "a non-allow-listed pragma is denied"
        );
        assert!(
            db.query("PRAGMA writable_schema = 1", &[]).is_err(),
            "writable_schema is denied"
        );
        // An allow-listed pragma still works.
        assert!(db.query("PRAGMA table_info(t)", &[]).is_ok());
    }

    #[test]
    fn the_reserved_namespace_is_denied() {
        let mut db = ModSql::in_memory(cfg());
        assert!(db.query("CREATE TABLE _slaq_kv (k TEXT)", &[]).is_err());
        assert!(db.query("SELECT * FROM _slaq_vector", &[]).is_err());
    }

    #[test]
    fn results_are_bounded() {
        let mut db = ModSql::in_memory(SqlConfig {
            max_bytes: 1024 * 1024,
            max_result_rows: 3,
        });
        assert!(
            db.query(
                "WITH r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r) SELECT n FROM r",
                &[]
            )
            .is_err(),
            "an unbounded recursive SELECT is refused"
        );
        let err = db
            .query(
                "SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4",
                &[],
            )
            .unwrap_err();
        assert!(err.to_string().contains("maxResultRows"), "{err}");
    }

    #[test]
    fn a_too_long_statement_is_refused() {
        let mut db = ModSql::in_memory(cfg());
        let sql = format!("SELECT '{}'", "x".repeat(MAX_SQL_BYTES as usize));
        assert!(db.query(&sql, &[]).is_err());
    }

    #[test]
    fn a_transaction_commits_or_rolls_back_as_a_whole() {
        let mut db = ModSql::in_memory(cfg());
        db.query("CREATE TABLE t (n INTEGER)", &[]).unwrap();
        // The second statement fails: the first must not persist.
        let err = db.tx(&[
            ("INSERT INTO t (n) VALUES (1)".to_string(), vec![]),
            ("SELECT * FROM nope".to_string(), vec![]),
        ]);
        assert!(err.is_err());
        let r = db.query("SELECT count(*) FROM t", &[]).unwrap();
        assert!(matches!(r.rows[0][0], SqlValue::Int(0)), "rolled back");
        // A good transaction commits.
        db.tx(&[
            ("INSERT INTO t (n) VALUES (2)".to_string(), vec![]),
            ("INSERT INTO t (n) VALUES (3)".to_string(), vec![]),
        ])
        .unwrap();
        let r = db.query("SELECT count(*) FROM t", &[]).unwrap();
        assert!(matches!(r.rows[0][0], SqlValue::Int(2)), "committed");
    }

    #[test]
    fn a_recursive_or_expensive_query_is_interrupted() {
        // A query that would run forever is stopped by the progress handler.
        let mut db = ModSql::in_memory(cfg());
        let started = std::time::Instant::now();
        let _ = db.query(
            "WITH r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 1000000000) SELECT count(*) FROM r",
            &[],
        );
        assert!(
            started.elapsed() < SQL_TIMEOUT + Duration::from_secs(3),
            "the statement was interrupted, not run to completion"
        );
    }

    #[test]
    fn page_size_cannot_be_changed() {
        let mut db = ModSql::in_memory(cfg());
        // Reading it is fine; *writing* it would reinterpret the file quota
        // (`max_page_count` is in pages), so it is denied.
        assert!(db.query("PRAGMA page_size", &[]).is_ok());
        assert!(
            db.query("PRAGMA page_size = 65536", &[]).is_err(),
            "setting the page size is denied"
        );
        assert!(
            db.query("PRAGMA encoding = 'UTF-16'", &[]).is_err(),
            "setting the encoding is denied"
        );
    }

    #[test]
    fn external_files_and_native_code_are_denied() {
        let mut db = ModSql::in_memory(cfg());
        assert!(db.query("ATTACH DATABASE '/tmp/x.db' AS x", &[]).is_err());
        // `VACUUM INTO` writes a file: it goes through ATTACH, so it is denied.
        let dir = std::env::temp_dir().join(format!("slaq-sql-vac-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("copy.db");
        assert!(
            db.query(&format!("VACUUM INTO '{}'", out.display()), &[])
                .is_err(),
            "VACUUM INTO is denied"
        );
        assert!(!out.exists(), "no file was written");
        // Native code stays unauthorized (the `load_extension` feature is off).
        assert!(db.query("SELECT load_extension('/tmp/x.so')", &[]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_tables_are_denied() {
        // A temp table lives in a process temp file, outside `scratch.db` and
        // `maxBytes`, so creating one is denied.
        let mut db = ModSql::in_memory(cfg());
        assert!(db.query("CREATE TEMP TABLE tt (b BLOB)", &[]).is_err());
        assert!(db.query("CREATE TEMP VIEW tv AS SELECT 1", &[]).is_err());
    }

    #[test]
    fn at_path_creates_its_own_directory() {
        // A `store.sql`-only mod: nothing else created the directory first.
        let root = std::env::temp_dir().join(format!("slaq-sql-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("dev.slaq.x").join("scratch.db");
        let mut db = ModSql::at_path(path.clone(), cfg());
        db.query("CREATE TABLE t (n INTEGER)", &[]).unwrap();
        assert!(path.exists(), "the DB (and its directory) was created");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_transaction_shares_one_deadline() {
        let mut db = ModSql::in_memory(cfg());
        db.query("CREATE TABLE t (n INTEGER)", &[]).unwrap();
        // A write, then 63 statements each doing real work: a per-statement
        // deadline would let them all finish (and the write commit); one budget for
        // the whole transaction interrupts it. The work per statement is generous
        // so the margin holds even on a host ~10× faster than the one this was
        // written on (63 × a few ms would otherwise fit inside 2 s).
        let work = "WITH r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 10000000) SELECT count(*) FROM r";
        let mut stmts: Vec<(String, Vec<SqlValue>)> =
            vec![("INSERT INTO t (n) VALUES (1)".to_string(), vec![])];
        stmts.extend((1..MAX_TX_STATEMENTS).map(|_| (work.to_string(), vec![])));
        let started = std::time::Instant::now();
        let r = db.tx(&stmts);
        assert!(r.is_err(), "one budget for the whole transaction");
        assert!(
            started.elapsed() < SQL_TIMEOUT * 3,
            "bounded by a single deadline, not one per statement"
        );
        // And the interrupted transaction rolled back.
        let n = db.query("SELECT count(*) FROM t", &[]).unwrap();
        assert!(matches!(n.rows[0][0], SqlValue::Int(0)), "rolled back");
    }

    #[test]
    fn the_file_quota_holds() {
        let root = std::env::temp_dir().join(format!("slaq-sql-quota-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("q").join("scratch.db");
        let mut db = ModSql::at_path(
            path.clone(),
            SqlConfig {
                max_bytes: 64 * 1024,
                max_result_rows: 100,
            },
        );
        db.query("CREATE TABLE t (b BLOB)", &[]).unwrap();
        let big = vec![0u8; 8 * 1024];
        let mut refused = false;
        for _ in 0..64 {
            if db
                .query(
                    "INSERT INTO t (b) VALUES (?1)",
                    &[SqlValue::Blob(big.clone())],
                )
                .is_err()
            {
                refused = true;
                break;
            }
        }
        assert!(refused, "the file quota refused further rows");
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert!(
            size <= 2 * 64 * 1024,
            "the file stayed near the quota, was {size} bytes"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
