//! The memory database: one SQLite file with full-text search, embeddings stored beside the text,
//! a temporal fact graph and a list of dead ends.
//!
//! The schema and the ideas behind it (a temporal entity graph whose facts expire instead of
//! being overwritten, source-linked memories, hybrid keyword + vector retrieval) follow Palace RS
//! by AncientiCe (MIT), and the dead-end list and experience weight follow OmniMem by richarvey
//! (MIT). The code is written for Rhea's own types; see `THIRD_PARTY.md`.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde_json::{Value, json};

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}

/// FNV-1a over the normalized text: a stable hash (unlike `DefaultHasher`) for duplicate detection.
pub fn text_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in normalize(text).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Lowercase, collapsed whitespace, no surrounding punctuation.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().trim_matches(|c: char| !c.is_alphanumeric()).to_owned()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub id: i64,
    pub kind: String,
    pub scope: String,
    pub text: String,
    pub source: String,
    pub verified: bool,
    pub chat_id: Option<String>,
    pub importance: f64,
    pub weight: f64,
    pub status: String,
    pub supersedes: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    pub uses: i64,
    pub meta: Value,
}

/// A memory about to be stored.
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub kind: String,
    pub scope: String,
    pub text: String,
    pub source: String,
    pub verified: bool,
    pub chat_id: Option<String>,
    pub importance: f64,
    /// A key for single-valued facts ("name", "residence"): a newer memory with the same topic
    /// replaces the older one instead of living beside it as a contradiction.
    pub topic: Option<String>,
    /// Identifies one event (a turn of a chat): saving it again, for example when chats are
    /// indexed after they were already remembered live, refreshes the record instead of adding a copy.
    pub key: Option<String>,
    pub meta: Value,
}

impl NewMemory {
    pub fn new(kind: &str, scope: &str, text: &str, source: &str) -> Self {
        Self { kind: kind.to_owned(), scope: scope.to_owned(), text: text.trim().to_owned(), source: source.to_owned(), verified: false, chat_id: None, importance: 0.5, topic: None, key: None, meta: json!({}) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stored {
    New(i64),
    /// The same thing was already remembered; its record was refreshed.
    Duplicate(i64),
    /// New, and it replaced these (now superseded) older memories on the same topic.
    Replaced { id: i64, superseded: Vec<i64> },
}

impl Stored {
    pub fn id(&self) -> i64 {
        match self {
            Stored::New(id) | Stored::Duplicate(id) | Stored::Replaced { id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub scopes: Vec<String>,
    pub kinds: Vec<String>,
    /// `None` = active memories only.
    pub status: Option<String>,
    pub contains: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Triple {
    pub id: i64,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
    pub confidence: f64,
    pub source_memory: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeadEnd {
    pub id: i64,
    pub scope: String,
    pub approach: String,
    pub reason: String,
    pub created_at: i64,
}

pub struct Store {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'global',
    text TEXT NOT NULL,
    source TEXT NOT NULL,
    verified INTEGER NOT NULL DEFAULT 0,
    chat_id TEXT,
    importance REAL NOT NULL DEFAULT 0.5,
    weight REAL NOT NULL DEFAULT 1.0,
    status TEXT NOT NULL DEFAULT 'active',
    supersedes INTEGER,
    hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_used_at INTEGER,
    uses INTEGER NOT NULL DEFAULT 0,
    embedding BLOB,
    embed_model TEXT,
    meta TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS memories_status ON memories(status, kind, scope);
CREATE INDEX IF NOT EXISTS memories_hash ON memories(hash);
CREATE INDEX IF NOT EXISTS memories_created ON memories(created_at);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(text, content='memories', content_rowid='id', tokenize='porter unicode61');
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF text ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO memories_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TABLE IF NOT EXISTS triples (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    subject TEXT NOT NULL,
    predicate TEXT NOT NULL,
    object TEXT NOT NULL,
    valid_from INTEGER NOT NULL,
    valid_to INTEGER,
    confidence REAL NOT NULL DEFAULT 1.0,
    source_memory INTEGER
);
CREATE INDEX IF NOT EXISTS triples_subject ON triples(subject, predicate);
CREATE TABLE IF NOT EXISTS dead_ends (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,
    approach TEXT NOT NULL,
    reason TEXT NOT NULL,
    memory_id INTEGER,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

fn memory_from_row(row: &Row<'_>) -> rusqlite::Result<Memory> {
    let meta: String = row.get("meta")?;
    Ok(Memory {
        id: row.get("id")?,
        kind: row.get("kind")?,
        scope: row.get("scope")?,
        text: row.get("text")?,
        source: row.get("source")?,
        verified: row.get::<_, i64>("verified")? != 0,
        chat_id: row.get("chat_id")?,
        importance: row.get("importance")?,
        weight: row.get("weight")?,
        status: row.get("status")?,
        supersedes: row.get("supersedes")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        last_used_at: row.get("last_used_at")?,
        uses: row.get("uses")?,
        meta: serde_json::from_str(&meta).unwrap_or_else(|_| json!({})),
    })
}

const COLUMNS: &str = "id, kind, scope, text, source, verified, chat_id, importance, weight, status, supersedes, created_at, updated_at, last_used_at, uses, meta";

pub fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|value| value.to_le_bytes()).collect()
}

pub fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4).map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])).collect()
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        Self::with(conn)
    }

    pub fn in_memory() -> rusqlite::Result<Self> {
        Self::with(Connection::open_in_memory()?)
    }

    fn with(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    fn filter_sql(filter: &Filter, args: &mut Vec<String>, prefix: &str) -> String {
        let mut clauses = vec![format!("{prefix}status = ?")];
        args.push(filter.status.clone().unwrap_or_else(|| "active".to_owned()));
        if !filter.scopes.is_empty() {
            clauses.push(format!("{prefix}scope IN ({})", vec!["?"; filter.scopes.len()].join(",")));
            args.extend(filter.scopes.iter().cloned());
        }
        if !filter.kinds.is_empty() {
            clauses.push(format!("{prefix}kind IN ({})", vec!["?"; filter.kinds.len()].join(",")));
            args.extend(filter.kinds.iter().cloned());
        }
        if let Some(text) = &filter.contains {
            clauses.push(format!("{prefix}text LIKE ? ESCAPE '!'"));
            args.push(format!("%{}%", text.replace('!', "!!").replace('%', "!%").replace('_', "!_")));
        }
        clauses.join(" AND ")
    }

    /// Store a memory, unless the same thing is already known. A memory with a `topic` retires
    /// the active ones with that topic and a different text.
    pub fn insert(&mut self, new: &NewMemory, embedding: Option<(&str, &[f32])>) -> rusqlite::Result<Stored> {
        let hash = text_hash(&new.text);
        let now = now_ms();
        let tx = self.conn.transaction()?;
        if let Some(existing) = tx
            .query_row("SELECT id FROM memories WHERE hash = ?1 AND scope = ?2 AND kind = ?3 AND status = 'active' LIMIT 1", params![hash, new.scope, new.kind], |row| row.get::<_, i64>(0))
            .optional()?
        {
            tx.execute("UPDATE memories SET updated_at = ?1, uses = uses + 1, importance = MAX(importance, ?2), verified = MAX(verified, ?3) WHERE id = ?4", params![now, new.importance, new.verified as i64, existing])?;
            tx.commit()?;
            return Ok(Stored::Duplicate(existing));
        }
        if let Some(key) = new.key.as_deref() {
            if let Some(existing) = tx
                .query_row("SELECT id FROM memories WHERE status = 'active' AND kind = ?1 AND json_extract(meta, '$.key') = ?2 LIMIT 1", params![new.kind, key], |row| row.get::<_, i64>(0))
                .optional()?
            {
                tx.execute("UPDATE memories SET updated_at = ?1, uses = uses + 1 WHERE id = ?2", params![now, existing])?;
                tx.commit()?;
                return Ok(Stored::Duplicate(existing));
            }
        }
        let mut superseded = Vec::new();
        if let Some(topic) = new.topic.as_deref().filter(|topic| !topic.is_empty()) {
            let mut statement = tx.prepare("SELECT id FROM memories WHERE status = 'active' AND kind = ?1 AND json_extract(meta, '$.topic') = ?2")?;
            superseded = statement.query_map(params![new.kind, topic], |row| row.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
        }
        let mut meta = new.meta.clone();
        if let (Some(topic), Some(object)) = (new.topic.as_deref(), meta.as_object_mut()) {
            object.insert("topic".into(), json!(topic));
        }
        if let (Some(key), Some(object)) = (new.key.as_deref(), meta.as_object_mut()) {
            object.insert("key".into(), json!(key));
        }
        let (blob, model) = match embedding {
            Some((model, vector)) => (Some(encode_vector(vector)), Some(model.to_owned())),
            None => (None, None),
        };
        tx.execute(
            "INSERT INTO memories (kind, scope, text, source, verified, chat_id, importance, weight, status, supersedes, hash, created_at, updated_at, embedding, embed_model, meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1.0, 'active', ?8, ?9, ?10, ?10, ?11, ?12, ?13)",
            params![new.kind, new.scope, new.text, new.source, new.verified as i64, new.chat_id, new.importance, superseded.first(), hash, now, blob, model, meta.to_string()],
        )?;
        let id = tx.last_insert_rowid();
        for old in &superseded {
            tx.execute("UPDATE memories SET status = 'superseded', updated_at = ?1 WHERE id = ?2", params![now, old])?;
        }
        tx.commit()?;
        Ok(if superseded.is_empty() { Stored::New(id) } else { Stored::Replaced { id, superseded } })
    }

    pub fn get(&self, id: i64) -> rusqlite::Result<Option<Memory>> {
        self.conn.query_row(&format!("SELECT {COLUMNS} FROM memories WHERE id = ?1"), params![id], memory_from_row).optional()
    }

    /// Other memories from the same chat made within `window_ms` of `around_ms`: what else
    /// happened around a memory, so a recalled outcome comes with its context.
    pub fn neighbors(&self, chat_id: &str, around_ms: i64, window_ms: i64, kinds: &[&str], exclude: &[i64], limit: usize) -> rusqlite::Result<Vec<Memory>> {
        let mut statement = self.conn.prepare(&format!("SELECT {COLUMNS} FROM memories WHERE chat_id = ?1 AND status = 'active' AND created_at BETWEEN ?2 AND ?3 ORDER BY ABS(created_at - ?4) LIMIT 50"))?;
        let rows = statement.query_map(params![chat_id, around_ms - window_ms, around_ms + window_ms, around_ms], memory_from_row)?;
        let mut found = Vec::new();
        for row in rows {
            let memory = row?;
            if kinds.contains(&memory.kind.as_str()) && !exclude.contains(&memory.id) {
                found.push(memory);
                if found.len() >= limit {
                    break;
                }
            }
        }
        Ok(found)
    }

    pub fn get_many(&self, ids: &[i64]) -> rusqlite::Result<Vec<Memory>> {
        let mut found = Vec::new();
        for id in ids {
            if let Some(memory) = self.get(*id)? {
                found.push(memory);
            }
        }
        Ok(found)
    }

    pub fn list(&self, filter: &Filter, limit: usize, offset: usize) -> rusqlite::Result<Vec<Memory>> {
        let mut args = Vec::new();
        let clause = Self::filter_sql(filter, &mut args, "");
        let sql = format!("SELECT {COLUMNS} FROM memories WHERE {clause} ORDER BY updated_at DESC, id DESC LIMIT {limit} OFFSET {offset}");
        let mut statement = self.conn.prepare(&sql)?;
        statement.query_map(params_from_iter(args.iter()), memory_from_row)?.collect()
    }

    pub fn count(&self, filter: &Filter) -> rusqlite::Result<i64> {
        let mut args = Vec::new();
        let clause = Self::filter_sql(filter, &mut args, "");
        self.conn.query_row(&format!("SELECT COUNT(*) FROM memories WHERE {clause}"), params_from_iter(args.iter()), |row| row.get(0))
    }

    /// Counts of active memories per kind.
    pub fn counts_by_kind(&self) -> rusqlite::Result<Vec<(String, i64)>> {
        let mut statement = self.conn.prepare("SELECT kind, COUNT(*) FROM memories WHERE status = 'active' GROUP BY kind ORDER BY COUNT(*) DESC")?;
        statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect()
    }

    /// Full-text matches with their BM25 score (higher is better), best first.
    pub fn fts_search(&self, fts_query: &str, filter: &Filter, limit: usize) -> rusqlite::Result<Vec<(i64, f64)>> {
        if fts_query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec![fts_query.to_owned()];
        let clause = Self::filter_sql(filter, &mut args, "m.");
        let sql = format!("SELECT m.id, -bm25(memories_fts) FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid WHERE memories_fts MATCH ? AND {clause} ORDER BY bm25(memories_fts) LIMIT {limit}");
        let mut statement = self.conn.prepare(&sql)?;
        statement.query_map(params_from_iter(args.iter()), |row| Ok((row.get(0)?, row.get(1)?)))?.collect()
    }

    /// Cosine similarity against every stored vector made by `model`. Vectors are unit length, so
    /// it is a dot product. A scan is fine for the tens of thousands of memories one person makes.
    pub fn vector_search(&self, query: &[f32], model: &str, filter: &Filter, limit: usize) -> rusqlite::Result<Vec<(i64, f32)>> {
        let mut args = vec![model.to_owned()];
        let clause = Self::filter_sql(filter, &mut args, "");
        let sql = format!("SELECT id, embedding FROM memories WHERE embed_model = ? AND embedding IS NOT NULL AND {clause}");
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(args.iter()), |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)))?;
        let mut scored: Vec<(i64, f32)> = Vec::new();
        for row in rows {
            let (id, blob) = row?;
            let vector = decode_vector(&blob);
            if vector.len() == query.len() {
                scored.push((id, vector.iter().zip(query).map(|(a, b)| a * b).sum()));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    pub fn mark_used(&self, ids: &[i64]) -> rusqlite::Result<()> {
        let now = now_ms();
        for id in ids {
            self.conn.execute("UPDATE memories SET uses = uses + 1, last_used_at = ?1 WHERE id = ?2", params![now, id])?;
        }
        Ok(())
    }

    pub fn set_status(&self, id: i64, status: &str) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("UPDATE memories SET status = ?1, updated_at = ?2 WHERE id = ?3", params![status, now_ms(), id])? > 0)
    }

    pub fn set_weight(&self, id: i64, weight: f64) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE memories SET weight = ?1 WHERE id = ?2", params![weight, id])?;
        Ok(())
    }

    /// Change a memory's text (a correction by the user) and its vector.
    pub fn update_text(&self, id: i64, text: &str, embedding: Option<(&str, &[f32])>) -> rusqlite::Result<bool> {
        let (blob, model) = match embedding {
            Some((model, vector)) => (Some(encode_vector(vector)), Some(model.to_owned())),
            None => (None, None),
        };
        Ok(self.conn.execute(
            "UPDATE memories SET text = ?1, hash = ?2, updated_at = ?3, source = 'user', verified = 1, embedding = ?4, embed_model = ?5 WHERE id = ?6",
            params![text.trim(), text_hash(text), now_ms(), blob, model, id],
        )? > 0)
    }

    pub fn set_embedding(&self, id: i64, model: &str, vector: &[f32]) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE memories SET embedding = ?1, embed_model = ?2 WHERE id = ?3", params![encode_vector(vector), model, id])?;
        Ok(())
    }

    /// Memories whose vector is missing or made by a different model than `model`.
    pub fn needing_embedding(&self, model: &str, limit: usize) -> rusqlite::Result<Vec<(i64, String)>> {
        let mut statement = self.conn.prepare("SELECT id, text FROM memories WHERE status != 'deleted' AND (embed_model IS NULL OR embed_model != ?1) LIMIT ?2")?;
        statement.query_map(params![model, limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))?.collect()
    }

    pub fn delete_forever(&self, id: i64) -> rusqlite::Result<bool> {
        self.conn.execute("DELETE FROM triples WHERE source_memory = ?1", params![id])?;
        self.conn.execute("DELETE FROM dead_ends WHERE memory_id = ?1", params![id])?;
        Ok(self.conn.execute("DELETE FROM memories WHERE id = ?1", params![id])? > 0)
    }

    pub fn clear_all(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("DELETE FROM memories; DELETE FROM triples; DELETE FROM dead_ends; INSERT INTO memories_fts(memories_fts) VALUES('rebuild');")
    }

    // ---- the fact graph ----------------------------------------------------------------

    /// Record `subject predicate object` as true from now. A `single_valued` predicate (a name, a
    /// home town) first ends whatever it said before, so the graph never holds two current answers.
    pub fn add_triple(&self, subject: &str, predicate: &str, object: &str, source_memory: Option<i64>, single_valued: bool) -> rusqlite::Result<i64> {
        let (subject, predicate, object) = (subject.trim().to_lowercase(), predicate.trim().to_lowercase().replace(' ', "_"), object.trim().to_owned());
        if let Some(existing) = self
            .conn
            .query_row("SELECT id FROM triples WHERE subject = ?1 AND predicate = ?2 AND object = ?3 AND valid_to IS NULL", params![subject, predicate, object], |row| row.get::<_, i64>(0))
            .optional()?
        {
            return Ok(existing);
        }
        let now = now_ms();
        if single_valued {
            self.conn.execute("UPDATE triples SET valid_to = ?1 WHERE subject = ?2 AND predicate = ?3 AND valid_to IS NULL", params![now, subject, predicate])?;
        }
        self.conn.execute("INSERT INTO triples (subject, predicate, object, valid_from, source_memory) VALUES (?1, ?2, ?3, ?4, ?5)", params![subject, predicate, object, now, source_memory])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// A memory was corrected: the facts it used to state stop being true, and the facts its new
    /// text states are recorded in their place (a single-valued answer also ends other answers).
    pub fn restate_facts(&self, memory_id: i64, facts: &[(String, String, String, bool)]) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE triples SET valid_to = ?1 WHERE source_memory = ?2 AND valid_to IS NULL", params![now_ms(), memory_id])?;
        for (subject, predicate, object, single) in facts {
            self.add_triple(subject, predicate, object, Some(memory_id), *single)?;
        }
        Ok(())
    }

    pub fn invalidate_triple(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE triples SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL", params![now_ms(), id])?;
        Ok(())
    }

    pub fn triples(&self, subject: Option<&str>, only_current: bool) -> rusqlite::Result<Vec<Triple>> {
        let mut sql = "SELECT id, subject, predicate, object, valid_from, valid_to, confidence, source_memory FROM triples WHERE 1 = 1".to_owned();
        let mut args: Vec<String> = Vec::new();
        if let Some(subject) = subject {
            sql.push_str(" AND subject = ?");
            args.push(subject.trim().to_lowercase());
        }
        if only_current {
            sql.push_str(" AND valid_to IS NULL");
        }
        sql.push_str(" ORDER BY valid_from DESC");
        let mut statement = self.conn.prepare(&sql)?;
        statement
            .query_map(params_from_iter(args.iter()), |row| {
                Ok(Triple { id: row.get(0)?, subject: row.get(1)?, predicate: row.get(2)?, object: row.get(3)?, valid_from: row.get(4)?, valid_to: row.get(5)?, confidence: row.get(6)?, source_memory: row.get(7)? })
            })?
            .collect()
    }

    // ---- dead ends ---------------------------------------------------------------------

    pub fn add_dead_end(&self, scope: &str, approach: &str, reason: &str, memory_id: Option<i64>) -> rusqlite::Result<i64> {
        self.conn.execute("INSERT INTO dead_ends (scope, approach, reason, memory_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![scope, approach, reason, memory_id, now_ms()])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Dead ends that apply here: the global ones and those of `scope` (a project). With no project
    /// only the global ones, never another project's.
    pub fn dead_ends(&self, scope: Option<&str>, limit: usize) -> rusqlite::Result<Vec<DeadEnd>> {
        let mut statement = self.conn.prepare("SELECT id, scope, approach, reason, created_at FROM dead_ends WHERE (scope = 'global' OR scope = ?1) ORDER BY created_at DESC LIMIT ?2")?;
        statement.query_map(params![scope, limit as i64], |row| Ok(DeadEnd { id: row.get(0)?, scope: row.get(1)?, approach: row.get(2)?, reason: row.get(3)?, created_at: row.get(4)? }))?.collect()
    }

    pub fn meta_get(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| row.get(0)).optional().ok().flatten()
    }

    pub fn meta_set(&self, key: &str, value: &str) {
        let _ = self.conn.execute("INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pref(text: &str) -> NewMemory {
        NewMemory::new("preference", "global", text, "user")
    }

    #[test]
    fn the_same_memory_is_stored_once_and_refreshed() {
        let mut store = Store::in_memory().unwrap();
        let first = store.insert(&pref("I prefer dark mode."), None).unwrap();
        let again = store.insert(&pref("  i prefer DARK mode "), None).unwrap();
        assert!(matches!(first, Stored::New(_)));
        assert_eq!(again, Stored::Duplicate(first.id()));
        assert_eq!(store.count(&Filter::default()).unwrap(), 1);
        assert_eq!(store.get(first.id()).unwrap().unwrap().uses, 1);
    }

    #[test]
    fn an_event_saved_twice_under_one_key_is_stored_once() {
        let mut store = Store::in_memory().unwrap();
        let mut first = NewMemory::new("summary", "global", "Asked: x Result: y Tools: a.", "assistant");
        first.key = Some("chat-1:abc".into());
        let mut again = NewMemory::new("summary", "global", "Asked: x Result: y", "assistant");
        again.key = Some("chat-1:abc".into());
        let id = store.insert(&first, None).unwrap().id();
        assert_eq!(store.insert(&again, None).unwrap(), Stored::Duplicate(id));
        assert_eq!(store.count(&Filter::default()).unwrap(), 1);
    }

    #[test]
    fn what_happened_around_a_memory_in_the_same_chat_can_be_found() {
        let mut store = Store::in_memory().unwrap();
        let mut a = NewMemory::new("failure", "global", "build failed", "tool");
        a.chat_id = Some("c1".into());
        let mut b = NewMemory::new("outcome", "global", "build fixed", "tool");
        b.chat_id = Some("c1".into());
        let mut other = NewMemory::new("outcome", "global", "another chat", "tool");
        other.chat_id = Some("c2".into());
        let first = store.insert(&a, None).unwrap().id();
        store.insert(&b, None).unwrap();
        store.insert(&other, None).unwrap();
        let near = store.neighbors("c1", now_ms(), 60_000, &["outcome", "failure"], &[first], 5).unwrap();
        assert_eq!(near.len(), 1);
        assert_eq!(near[0].text, "build fixed");
    }

    #[test]
    fn a_new_fact_on_the_same_topic_retires_the_old_one() {
        let mut store = Store::in_memory().unwrap();
        let mut old = NewMemory::new("fact", "global", "The user lives in Lisbon.", "user");
        old.topic = Some("residence".into());
        let first = store.insert(&old, None).unwrap().id();
        let mut new = NewMemory::new("fact", "global", "The user lives in Berlin.", "user");
        new.topic = Some("residence".into());
        let second = store.insert(&new, None).unwrap();
        assert_eq!(second, Stored::Replaced { id: second.id(), superseded: vec![first] });
        assert_eq!(store.get(first).unwrap().unwrap().status, "superseded");
        let active = store.list(&Filter::default(), 10, 0).unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].text.contains("Berlin") && active[0].supersedes == Some(first));
    }

    #[test]
    fn full_text_search_finds_word_forms_and_respects_the_filter() {
        let mut store = Store::in_memory().unwrap();
        let a = store.insert(&NewMemory::new("outcome", "project:rhea", "Fixed the installer crash on startup", "tool"), None).unwrap().id();
        store.insert(&NewMemory::new("outcome", "project:other", "Fixed a different installer problem", "tool"), None).unwrap();
        let all = store.fts_search("\"installing\" OR \"crash\"", &Filter::default(), 10).unwrap();
        assert_eq!(all.len(), 2, "the stemmer matches installer/installing");
        let scoped = store.fts_search("\"installer\"", &Filter { scopes: vec!["project:rhea".into()], ..Default::default() }, 10).unwrap();
        assert_eq!(scoped.iter().map(|hit| hit.0).collect::<Vec<_>>(), [a]);
        assert!(scoped[0].1 > 0.0);
    }

    #[test]
    fn vectors_are_compared_only_within_one_model() {
        let mut store = Store::in_memory().unwrap();
        let near = store.insert(&pref("alpha"), Some(("m1", &[1.0, 0.0]))).unwrap().id();
        store.insert(&pref("beta"), Some(("m1", &[0.0, 1.0]))).unwrap();
        store.insert(&pref("gamma"), Some(("m2", &[1.0, 0.0]))).unwrap();
        let hits = store.vector_search(&[1.0, 0.0], "m1", &Filter::default(), 5).unwrap();
        assert_eq!(hits[0].0, near);
        assert_eq!(hits.len(), 2);
        assert_eq!(store.needing_embedding("m1", 10).unwrap().len(), 1, "the m2 row needs re-embedding");
    }

    #[test]
    fn single_valued_facts_expire_instead_of_piling_up() {
        let store = Store::in_memory().unwrap();
        store.add_triple("user", "works at", "Acme", None, true).unwrap();
        store.add_triple("user", "works_at", "Globex", None, true).unwrap();
        store.add_triple("user", "uses", "Rust", None, false).unwrap();
        store.add_triple("user", "uses", "Python", None, false).unwrap();
        let current = store.triples(Some("user"), true).unwrap();
        assert_eq!(current.len(), 3);
        assert!(current.iter().any(|t| t.predicate == "works_at" && t.object == "Globex"));
        assert!(!current.iter().any(|t| t.object == "Acme"));
        assert_eq!(store.triples(Some("user"), false).unwrap().len(), 4, "history is kept");
    }

    #[test]
    fn deleting_a_memory_removes_it_from_search() {
        let mut store = Store::in_memory().unwrap();
        let id = store.insert(&pref("remember the secret handshake"), None).unwrap().id();
        assert_eq!(store.fts_search("\"handshake\"", &Filter::default(), 5).unwrap().len(), 1);
        assert!(store.delete_forever(id).unwrap());
        assert!(store.fts_search("\"handshake\"", &Filter::default(), 5).unwrap().is_empty());
        assert_eq!(store.count(&Filter::default()).unwrap(), 0);
    }

    #[test]
    fn dead_ends_are_listed_for_their_project_and_for_everyone() {
        let store = Store::in_memory().unwrap();
        store.add_dead_end("project:rhea", "the onig regex backend", "needs a C compiler on CI", None).unwrap();
        store.add_dead_end("project:other", "x", "y", None).unwrap();
        store.add_dead_end("global", "closing the app to rebuild", "the exe is locked", None).unwrap();
        let found = store.dead_ends(Some("project:rhea"), 10).unwrap();
        assert_eq!(found.len(), 2);
    }
}
