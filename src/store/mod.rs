//! Local cache on SQLite.
//!
//! It avoids downloading the channel list and the history again on every
//! start. The DB is a *cache*, not the source of truth: it can be deleted at
//! any time and the client starts again by rebuilding it from Slack.
//!
//! Messages are kept with their raw JSON, besides the fields extracted into
//! columns. Slack adds fields constantly and the cost of keeping the blob is
//! negligible against having to download them again.

use crate::api::client::{ChannelInfo, UserInfo};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How long before the channel list counts as stale.
pub const CHANNELS_TTL_SECS: i64 = 24 * 60 * 60;

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct CachedChannel {
    pub id: String,
    pub name: String,
    /// For DMs: the user on the other side, to get the name from.
    pub user: String,
    pub topic: String,
    pub num_members: i64,
    pub is_im: bool,
    pub is_mpim: bool,
    /// A space shared with another organisation (Slack Connect).
    pub is_ext_shared: bool,
    pub is_archived: bool,
    pub is_member: bool,
}

#[derive(Debug, Clone)]
pub struct CachedMessage {
    pub ts: String,
    pub user: String,
    pub text: String,
    pub subtype: String,
    pub thread_ts: Option<String>,
    pub reply_count: i64,
    pub raw: serde_json::Value,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `1712345678.123456` -> `1712345678.123456_f64`, to sort reliably
/// instead of lexicographically.
fn ts_to_num(ts: &str) -> f64 {
    ts.parse::<f64>().unwrap_or(0.0)
}

impl Store {
    /// Opens the cache at the standard path, creating it if needed.
    ///
    /// `$XDG_DATA_HOME/slaq/cache.db`, otherwise
    /// `~/.local/share/slaq/cache.db`.
    pub fn open() -> Result<Self> {
        Self::open_at(&Self::default_path()?)
    }

    pub fn default_path() -> Result<PathBuf> {
        let base = match std::env::var_os("XDG_DATA_HOME") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => {
                let home = std::env::var_os("HOME")
                    .context("neither XDG_DATA_HOME nor HOME is set")?;
                PathBuf::from(home).join(".local").join("share")
            }
        };
        Ok(base.join("slaq").join("cache.db"))
    }

    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("unable to create {}", dir.display()))?;
            // The cache holds private messages. Keep the directory owner-only so
            // the db, its -wal and -shm siblings are unreachable by other local
            // users, whatever mode SQLite creates them with.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        // Create the database owner-only on first use: SQLite would otherwise
        // use the process umask (often 0644). The 0700 directory above is the
        // real barrier; this is defence in depth.
        if !path.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o600)
                    .open(path);
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("unable to open the cache {}", path.display()))?;

        // WAL: reads not blocked by the TUI's background writes.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Opens the cache on disk; if that is not possible (permissions, disk
    /// full) it falls back to an in-memory cache and says so.
    ///
    /// A broken cache must not stop you reading Slack: all that is lost is the
    /// advantage from one start to the next.
    pub fn open_or_memory() -> Self {
        match Self::open() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("disk cache unavailable ({e}); using memory");
                Self::open_in_memory()
                    .expect("an in-memory SQLite cache cannot fail")
            }
        }
    }

    /// In-memory cache: used by the tests and as a fallback.
    pub fn open_in_memory() -> Result<Self> {
        let store = Store {
            conn: Connection::open_in_memory()?,
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 =
            self.conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))?;

        if version < 1 {
            self.conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS channels (
                    id           TEXT PRIMARY KEY,
                    name         TEXT NOT NULL DEFAULT '',
                    topic        TEXT NOT NULL DEFAULT '',
                    num_members  INTEGER NOT NULL DEFAULT 0,
                    is_im        INTEGER NOT NULL DEFAULT 0,
                    is_mpim      INTEGER NOT NULL DEFAULT 0,
                    is_archived  INTEGER NOT NULL DEFAULT 0,
                    is_member    INTEGER NOT NULL DEFAULT 0,
                    updated_at   INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS messages (
                    channel_id  TEXT NOT NULL,
                    ts          TEXT NOT NULL,
                    ts_num      REAL NOT NULL,
                    user        TEXT NOT NULL DEFAULT '',
                    text        TEXT NOT NULL DEFAULT '',
                    subtype     TEXT NOT NULL DEFAULT '',
                    thread_ts   TEXT,
                    reply_count INTEGER NOT NULL DEFAULT 0,
                    raw         TEXT NOT NULL,
                    PRIMARY KEY (channel_id, ts)
                );

                CREATE INDEX IF NOT EXISTS idx_messages_chan_ts
                    ON messages (channel_id, ts_num DESC);

                CREATE TABLE IF NOT EXISTS users (
                    id           TEXT PRIMARY KEY,
                    name         TEXT NOT NULL DEFAULT '',
                    real_name    TEXT NOT NULL DEFAULT '',
                    display_name TEXT NOT NULL DEFAULT '',
                    updated_at   INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                PRAGMA user_version = 1;
                "#,
            )?;
        }

        if version < 2 {
            // DMs have no name: their title is the user on the other
            // side. Without keeping their id, direct conversations came
            // back from the cache as empty rows after a restart.
            self.conn.execute_batch(
                r#"
                ALTER TABLE channels ADD COLUMN user TEXT NOT NULL DEFAULT '';
                PRAGMA user_version = 2;
                "#,
            )?;
        }
        if version < 3 {
            // Spaces shared with other organisations: Slack groups them
            // separately, and to do the same we have to know at restart, not
            // only when the list arrives from the network.
            self.conn.execute_batch(
                r#"
                ALTER TABLE channels ADD COLUMN is_ext_shared INTEGER NOT NULL DEFAULT 0;
                PRAGMA user_version = 3;
                "#,
            )?;
        }
        Ok(())
    }

    // ---- meta -------------------------------------------------------------

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- channels ---------------------------------------------------------

    pub fn upsert_channels(&mut self, channels: &[ChannelInfo]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let now = now_secs();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO channels
                   (id, name, topic, num_members, is_im, is_mpim, is_archived, is_member, updated_at, user, is_ext_shared)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(id) DO UPDATE SET
                   name        = excluded.name,
                   user        = excluded.user,
                   topic       = excluded.topic,
                   num_members = excluded.num_members,
                   is_im       = excluded.is_im,
                   is_mpim     = excluded.is_mpim,
                   is_archived = excluded.is_archived,
                   is_member   = excluded.is_member,
                   is_ext_shared = excluded.is_ext_shared,
                   updated_at  = excluded.updated_at",
            )?;
            for ch in channels {
                stmt.execute(params![
                    ch.id,
                    ch.name,
                    ch.topic.value.as_deref().unwrap_or(""),
                    ch.num_members,
                    ch.is_im as i64,
                    ch.is_mpim as i64,
                    ch.is_archived as i64,
                    ch.is_member as i64,
                    now,
                    ch.user.clone().unwrap_or_default(),
                    ch.is_ext_shared as i64,
                ])?;
            }
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('channels_synced_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![now.to_string()],
        )?;
        tx.commit()?;
        Ok(channels.len())
    }

    pub fn channels(&self, include_archived: bool) -> Result<Vec<CachedChannel>> {
        let sql = if include_archived {
            "SELECT id, name, topic, num_members, is_im, is_mpim, is_archived, is_member, user,
                    is_ext_shared
             FROM channels ORDER BY name"
        } else {
            "SELECT id, name, topic, num_members, is_im, is_mpim, is_archived, is_member, user,
                    is_ext_shared
             FROM channels WHERE is_archived = 0 ORDER BY name"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(CachedChannel {
                id: r.get(0)?,
                name: r.get(1)?,
                topic: r.get(2)?,
                num_members: r.get(3)?,
                is_im: r.get::<_, i64>(4)? != 0,
                is_mpim: r.get::<_, i64>(5)? != 0,
                is_archived: r.get::<_, i64>(6)? != 0,
                is_member: r.get::<_, i64>(7)? != 0,
                user: r.get(8)?,
                is_ext_shared: r.get::<_, i64>(9)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Seconds since the last full sync of the channel list, `None` if it
    /// has never happened.
    pub fn channels_age_secs(&self) -> Result<Option<i64>> {
        Ok(self
            .get_meta("channels_synced_at")?
            .and_then(|v| v.parse::<i64>().ok())
            .map(|t| now_secs().saturating_sub(t)))
    }

    pub fn channels_are_stale(&self) -> Result<bool> {
        Ok(match self.channels_age_secs()? {
            None => true,
            Some(age) => age > CHANNELS_TTL_SECS,
        })
    }

    // ---- users ------------------------------------------------------------

    pub fn upsert_users(&mut self, users: &[UserInfo]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let now = now_secs();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO users (id, name, real_name, display_name, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET
                   name         = excluded.name,
                   real_name    = excluded.real_name,
                   display_name = excluded.display_name,
                   updated_at   = excluded.updated_at",
            )?;
            for u in users {
                stmt.execute(params![
                    u.id,
                    u.name,
                    u.real_name,
                    u.display_name,
                    now
                ])?;
            }
        }
        tx.commit()?;
        Ok(users.len())
    }

    /// Map of user id -> displayable name, to resolve authors without a
    /// `users.info` call per message.
    pub fn user_names(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, real_name, display_name FROM users")?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let name: String = r.get(1)?;
            let real: String = r.get(2)?;
            let display: String = r.get(3)?;
            // The full name wins over the nickname, as the real client shows.
            let best = if !real.is_empty() {
                real
            } else if !display.is_empty() {
                display
            } else {
                name
            };
            Ok((id, best))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Map of username -> displayable name. Slack's MPIM names carry the
    /// participant usernames (`mpdm-alice--bob-1`): this is what turns them
    /// into readable people without a `users.info` call per conversation.
    pub fn username_names(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, real_name, display_name FROM users")?;
        let rows = stmt.query_map([], |r| {
            let name: String = r.get(0)?;
            let real: String = r.get(1)?;
            let display: String = r.get(2)?;
            // The full name wins over the nickname, as the real client shows.
            let best = if !real.is_empty() {
                real
            } else if !display.is_empty() {
                display
            } else {
                name.clone()
            };
            Ok((name, best))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- messages ---------------------------------------------------------

    /// Inserts the messages returned by `conversations.history`.
    ///
    /// Idempotent: running it again with messages already present updates them,
    /// which is what we want for edits and reaction/reply counts.
    pub fn upsert_messages(
        &mut self,
        channel_id: &str,
        messages: &[serde_json::Value],
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO messages
                   (channel_id, ts, ts_num, user, text, subtype, thread_ts, reply_count, raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(channel_id, ts) DO UPDATE SET
                   user        = excluded.user,
                   text        = excluded.text,
                   subtype     = excluded.subtype,
                   thread_ts   = excluded.thread_ts,
                   reply_count = excluded.reply_count,
                   raw         = excluded.raw",
            )?;
            for m in messages {
                let Some(ts) = m.get("ts").and_then(|v| v.as_str()) else {
                    // Without a `ts` there is no primary key: we skip it instead
                    // of inventing one.
                    continue;
                };
                stmt.execute(params![
                    channel_id,
                    ts,
                    ts_to_num(ts),
                    m.get("user").and_then(|v| v.as_str()).unwrap_or(""),
                    m.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                    m.get("subtype").and_then(|v| v.as_str()).unwrap_or(""),
                    m.get("thread_ts").and_then(|v| v.as_str()),
                    m.get("reply_count").and_then(|v| v.as_i64()).unwrap_or(0),
                    serde_json::to_string(m)?,
                ])?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// The last `limit` messages of a channel, from oldest to newest (the
    /// order they are read in).
    ///
    /// It excludes thread replies, as `conversations.history` does: a reply has a
    /// `thread_ts` different from its own `ts`. The root message has the two equal
    /// and stays in the timeline, and a `thread_broadcast` is by definition meant
    /// to be shown in the channel too.
    ///
    /// The replies stay in the cache all the same: the thread view needs them.
    pub fn messages(&self, channel_id: &str, limit: u32) -> Result<Vec<CachedMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, user, text, subtype, thread_ts, reply_count, raw
             FROM (
                 SELECT ts, ts_num, user, text, subtype, thread_ts, reply_count, raw
                 FROM messages
                 WHERE channel_id = ?1
                   AND (thread_ts IS NULL
                        OR thread_ts = ts
                        OR subtype = 'thread_broadcast')
                 ORDER BY ts_num DESC LIMIT ?2
             ) ORDER BY ts_num ASC",
        )?;
        let rows = stmt.query_map(params![channel_id, limit], |r| {
            let raw: String = r.get(6)?;
            Ok(CachedMessage {
                ts: r.get(0)?,
                user: r.get(1)?,
                text: r.get(2)?,
                subtype: r.get(3)?,
                thread_ts: r.get(4)?,
                reply_count: r.get(5)?,
                raw: serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The raw JSON of a single message, if it is cached.
    ///
    /// Reactions arrive as events about a message that is already stored: to
    /// keep them we have to reread that message, change it and write it back.
    pub fn raw_message(&self, channel_id: &str, ts: &str) -> Result<Option<serde_json::Value>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT raw FROM messages WHERE channel_id = ?1 AND ts = ?2",
                params![channel_id, ts],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
    }

    /// Keeps only the newest `keep` messages per conversation.
    ///
    /// Without it the cache grows without bound over months. Called once at
    /// startup, not during the session: a jump can legitimately grow the loaded
    /// window large, and pruning mid-session would drop what is on screen.
    pub fn prune_messages(&self, keep: i64) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM messages WHERE rowid NOT IN (
                 SELECT rowid FROM (
                     SELECT rowid,
                            ROW_NUMBER() OVER (
                                PARTITION BY channel_id ORDER BY ts_num DESC
                            ) AS rn
                     FROM messages
                 ) WHERE rn <= ?1
             )",
            params![keep],
        )?;
        Ok(n)
    }

    /// The `ts` of the newest message in the cache: to pass as `oldest` to
    /// `conversations.history` to download only what is new.
    pub fn newest_ts(&self, channel_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ts FROM messages WHERE channel_id = ?1
                   AND (thread_ts IS NULL OR thread_ts = ts OR subtype = 'thread_broadcast')
                 ORDER BY ts_num DESC LIMIT 1",
                params![channel_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The `ts` of the oldest message in the cache: to pass as `latest` for
    /// backfilling backwards.
    pub fn oldest_ts(&self, channel_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ts FROM messages WHERE channel_id = ?1
                   AND (thread_ts IS NULL OR thread_ts = ts OR subtype = 'thread_broadcast')
                 ORDER BY ts_num ASC LIMIT 1",
                params![channel_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// A thread's replies, root included, oldest first.
    pub fn thread_messages(&self, channel_id: &str, thread_ts: &str) -> Result<Vec<CachedMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, user, text, subtype, thread_ts, reply_count, raw
             FROM messages
             WHERE channel_id = ?1 AND (thread_ts = ?2 OR ts = ?2)
             ORDER BY ts_num ASC",
        )?;
        let rows = stmt.query_map(params![channel_id, thread_ts], |r| {
            let raw: String = r.get(6)?;
            Ok(CachedMessage {
                ts: r.get(0)?,
                user: r.get(1)?,
                text: r.get(2)?,
                subtype: r.get(3)?,
                thread_ts: r.get(4)?,
                reply_count: r.get(5)?,
                raw: serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Removes a message deleted on Slack's side.
    ///
    /// Returns `true` if it was really there: a `message_deleted` for a message
    /// never cached is not an error.
    pub fn delete_message(&self, channel_id: &str, ts: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM messages WHERE channel_id = ?1 AND ts = ?2",
            params![channel_id, ts],
        )?;
        Ok(n > 0)
    }

    pub fn message_count(&self, channel_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE channel_id = ?1",
            params![channel_id],
            |r| r.get(0),
        )?)
    }

    /// Size of the cache in bytes.
    pub fn size_bytes(&self) -> Result<i64> {
        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok(page_count * page_size)
    }

    pub fn clear(&self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM messages; DELETE FROM channels; DELETE FROM users; DELETE FROM meta;")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(ts: &str, text: &str) -> serde_json::Value {
        json!({ "ts": ts, "user": "U1", "text": text })
    }

    #[test]
    fn round_trip_messages_ordered_oldest_first() {
        let mut s = Store::open_in_memory().unwrap();
        // inserted out of order on purpose
        s.upsert_messages("C1", &[msg("1712345680.000100", "b"), msg("1712345670.000100", "a")])
            .unwrap();

        let got = s.messages("C1", 10).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].text, "a", "the oldest must come first");
        assert_eq!(got[1].text, "b");
    }

    #[test]
    fn upsert_is_idempotent_and_updates() {
        let mut s = Store::open_in_memory().unwrap();
        s.upsert_messages("C1", &[msg("1712345670.000100", "originale")]).unwrap();
        s.upsert_messages("C1", &[msg("1712345670.000100", "modificato")]).unwrap();

        assert_eq!(s.message_count("C1").unwrap(), 1, "no duplicate");
        assert_eq!(s.messages("C1", 10).unwrap()[0].text, "modificato");
    }

    #[test]
    fn ts_ordering_is_numeric_not_lexicographic() {
        let mut s = Store::open_in_memory().unwrap();
        // "9999999999" > "10000000000" lexicographically, but not numerically
        s.upsert_messages("C1", &[msg("10000000000.000000", "nuovo"), msg("9999999999.000000", "vecchio")])
            .unwrap();

        assert_eq!(s.newest_ts("C1").unwrap().unwrap(), "10000000000.000000");
        assert_eq!(s.oldest_ts("C1").unwrap().unwrap(), "9999999999.000000");
    }

    #[test]
    fn message_without_ts_is_skipped_not_fabricated() {
        let mut s = Store::open_in_memory().unwrap();
        let written = s
            .upsert_messages("C1", &[json!({ "text": "senza ts" }), msg("1712345670.000100", "ok")])
            .unwrap();
        assert_eq!(written, 1);
        assert_eq!(s.message_count("C1").unwrap(), 1);
    }

    #[test]
    fn empty_cache_reports_stale() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.channels_are_stale().unwrap());
        assert_eq!(s.channels_age_secs().unwrap(), None);
    }

    /// The cached names show the **full name**, not the nickname: a user whose
    /// display name is an alias is still shown by name, and the alias map
    /// follows suit.
    #[test]
    fn cached_names_prefer_the_real_name() {
        use crate::api::client::{UserInfo, UserProfile};
        let u = UserInfo {
            id: "U1".to_string(),
            name: "giulia.conti".to_string(),
            real_name: "Giulia Conti".to_string(),
            display_name: String::new(),
            profile: UserProfile {
                display_name: "giuly".to_string(),
                real_name: "Giulia Conti".to_string(),
                ..Default::default()
            },
            is_bot: false,
            is_app_user: false,
        };
        let mut s = Store::open_in_memory().unwrap();
        s.upsert_users(&[u]).unwrap();
        assert_eq!(s.user_names().unwrap()["U1"], "Giulia Conti");
        assert_eq!(s.username_names().unwrap()["giulia.conti"], "Giulia Conti");
    }

    #[test]
    fn thread_replies_are_stored_but_hidden_from_the_channel_timeline() {
        let mut s = Store::open_in_memory().unwrap();
        let root = json!({ "ts": "100.000000", "user": "U1", "text": "radice",
                           "thread_ts": "100.000000", "reply_count": 1 });
        let reply = json!({ "ts": "101.000000", "user": "U2", "text": "risposta",
                            "thread_ts": "100.000000" });
        s.upsert_messages("C1", &[root, reply]).unwrap();

        let timeline = s.messages("C1", 10).unwrap();
        assert_eq!(timeline.len(), 1, "the reply does not go into the timeline");
        assert_eq!(timeline[0].text, "radice");

        let thread = s.thread_messages("C1", "100.000000").unwrap();
        assert_eq!(thread.len(), 2, "but it stays readable in the thread");
        assert_eq!(s.message_count("C1").unwrap(), 2);
    }

    #[test]
    fn thread_broadcast_stays_in_the_timeline() {
        let mut s = Store::open_in_memory().unwrap();
        s.upsert_messages("C1", &[json!({
            "ts": "101.000000", "user": "U2", "text": "anche nel canale",
            "thread_ts": "100.000000", "subtype": "thread_broadcast"
        })]).unwrap();
        assert_eq!(s.messages("C1", 10).unwrap().len(), 1);
    }

    #[test]
    fn delete_removes_and_reports_whether_it_existed() {
        let s_ = &mut Store::open_in_memory().unwrap();
        s_.upsert_messages("C1", &[msg("100.000000", "ciao")]).unwrap();

        assert!(s_.delete_message("C1", "100.000000").unwrap());
        assert_eq!(s_.message_count("C1").unwrap(), 0);
        assert!(
            !s_.delete_message("C1", "100.000000").unwrap(),
            "deleting twice is not an error"
        );
    }

    #[test]
    fn raw_json_survives_round_trip() {
        let mut s = Store::open_in_memory().unwrap();
        let m = json!({
            "ts": "1712345670.000100",
            "text": "ciao",
            "reactions": [{ "name": "tada", "count": 3 }],
            "campo_sconosciuto": { "annidato": true }
        });
        s.upsert_messages("C1", &[m.clone()]).unwrap();
        assert_eq!(s.messages("C1", 1).unwrap()[0].raw, m);
    }
}
