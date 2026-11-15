//! The storage worker's state: the Git repository and its SQLite projection.
//!
//! Git holds the content. SQLite is derived from one published commit and can
//! be deleted and rebuilt. A single `Store` owns both, under a process lock.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::error::{Code, Problem, Result, WikiError};
use crate::git::{
    Identity, Repository, TRAILER_PAYLOAD, TRAILER_REQUEST, TRAILER_RESULT, TreeEdit,
};
use crate::notes::{self, Note, ROOT_ID, Scope, Status, sha256_hex};
use crate::search::{
    self, Mode, SearchRecord, SearchRequest, SearchResponse, SearchScope, SemanticCoverage,
};

const SCHEMA_VERSION: i64 = 1;
const MAX_READ_IDS: usize = 10;
const MAX_REQUEST_ID_BYTES: usize = 128;
const EXCERPT_TOKENS: u32 = 24;
const LOCK_ATTEMPTS: u32 = 20;
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
const VECTOR_OK: &str = "ok";
const VECTOR_OVERLENGTH: &str = "overlength";

const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE notes (
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    scope TEXT NOT NULL,
    project TEXT,
    overview INTEGER NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT NOT NULL,
    tags TEXT NOT NULL,
    applies_to TEXT NOT NULL,
    revision TEXT NOT NULL,
    markdown TEXT NOT NULL,
    embed_hash TEXT,
    embed_input TEXT
);
CREATE INDEX notes_scope ON notes (scope, project);
CREATE INDEX notes_embed ON notes (embed_hash);
CREATE VIRTUAL TABLE notes_fts USING fts5 (
    title, description, tags, path, body,
    tokenize = 'porter unicode61 remove_diacritics 2'
);
CREATE TABLE vectors (
    input_hash TEXT NOT NULL,
    model TEXT NOT NULL,
    state TEXT NOT NULL,
    data BLOB,
    PRIMARY KEY (input_hash, model)
) WITHOUT ROWID;
CREATE TABLE requests (
    request_id TEXT PRIMARY KEY,
    payload_hash TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    result TEXT NOT NULL
) WITHOUT ROWID;
";

#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub git_dir: PathBuf,
    pub state_dir: PathBuf,
    pub identity: Identity,
    /// identity of the embedding model, or `None` when semantic search is off
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Change {
    /// Create a record. Fails if the identifier already exists.
    Create {
        /// destination path, such as `memory/<project>/<name>.md`
        id: String,
        /// complete Markdown, including frontmatter
        markdown: String,
    },
    /// Replace a record that is still at `expected_revision`.
    Update {
        id: String,
        expected_revision: String,
        /// complete replacement Markdown, including frontmatter
        markdown: String,
    },
    /// Delete a record that is still at `expected_revision`.
    Delete {
        id: String,
        expected_revision: String,
    },
}

impl Change {
    pub fn id(&self) -> &str {
        match self {
            Change::Create { id, .. } | Change::Update { id, .. } | Change::Delete { id, .. } => id,
        }
    }

    fn operation(&self) -> &'static str {
        match self {
            Change::Create { .. } => "create",
            Change::Update { .. } => "update",
            Change::Delete { .. } => "delete",
        }
    }

    fn markdown(&self) -> Option<&str> {
        match self {
            Change::Create { markdown, .. } | Change::Update { markdown, .. } => Some(markdown),
            Change::Delete { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpdateRequest {
    /// Caller-chosen identifier of this logical update: 1 to 128 characters
    /// from letters, digits, `.`, `_`, `:` and `-`. Retry with the same
    /// identifier and payload to recover the outcome of an uncertain call.
    pub request_id: String,
    /// Changes applied together or not at all.
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeResult {
    pub id: String,
    pub operation: String,
    /// `indexed`, `pending`, `overlength`, or `unavailable` when the change was
    /// committed. Absent for a deletion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic: Option<String>,
    /// the new revision, absent for a deletion
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateReceipt {
    pub request_id: String,
    pub commit: String,
    /// true when this request ID was already committed and the original outcome is returned
    pub replayed: bool,
    pub changes: Vec<ChangeResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Document {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub scope: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub applies_to: Vec<String>,
    pub revision: String,
    pub markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadResponse {
    pub records: Vec<Document>,
    /// requested identifiers that do not exist
    pub missing: Vec<String>,
}

/// The result of embedding one input.
#[derive(Debug, Clone, PartialEq)]
pub enum Embedded {
    Vector(Vec<f32>),
    Overlength,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexHealth {
    pub commit: Option<String>,
    pub notes: usize,
    pub semantic_model: Option<String>,
    pub semantic_indexed: usize,
    pub semantic_pending: usize,
    pub semantic_overlength: usize,
}

pub struct Store {
    git: Repository,
    db: Connection,
    model: Option<String>,
    /// set when the branch moved but the projection could not follow
    stale: bool,
    /// makes the next projection fail after publication, to test recovery
    #[cfg(test)]
    pub fail_next_projection: bool,
    _lock: File,
}

fn db_error(error: rusqlite::Error) -> WikiError {
    WikiError::unavailable(format!("index database failed: {error}"))
}

fn git_error(error: anyhow::Error) -> WikiError {
    WikiError::unavailable(format!("repository failed: {error:#}"))
}

fn json_list(items: &[String]) -> String {
    serde_json::to_string(items).expect("string list serializes")
}

fn from_json_list(text: &str) -> Vec<String> {
    serde_json::from_str(text).unwrap_or_default()
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

impl Store {
    /// Take the process lock, open the projection, and bring it to the branch head.
    pub fn open(config: &StoreConfig) -> anyhow::Result<Self> {
        use anyhow::Context;
        std::fs::create_dir_all(&config.state_dir)
            .with_context(|| format!("creating {}", config.state_dir.display()))?;
        let lock_path = config.state_dir.join("lock");
        let lock = File::create(&lock_path)
            .with_context(|| format!("creating {}", lock_path.display()))?;
        // a child process being spawned elsewhere can hold a copy of a just-released
        // lock until it execs, so a restart waits briefly before giving up
        let mut attempts = LOCK_ATTEMPTS;
        while lock.try_lock().is_err() {
            attempts -= 1;
            if attempts == 0 {
                anyhow::bail!(
                    "{} is held by another wiki-mcp process, so stop the service before maintenance",
                    lock_path.display()
                );
            }
            std::thread::sleep(LOCK_RETRY_DELAY);
        }
        let git = Repository::open(&config.git_dir, config.identity.clone())?;
        let db_path = config.state_dir.join("index.sqlite3");
        let db = match Self::open_database(&db_path) {
            Ok(db) => db,
            Err(error) => {
                // the projection is derived, so an unreadable or older one is replaced
                tracing::warn!(error = %error, "replacing the index database");
                for suffix in ["", "-wal", "-shm"] {
                    let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
                }
                Self::open_database(&db_path)?
            }
        };
        let mut store = Self {
            git,
            db,
            model: config.model.clone(),
            stale: true,
            #[cfg(test)]
            fail_next_projection: false,
            _lock: lock,
        };
        store.sync().map_err(|error| anyhow::anyhow!(error))?;
        store
            .collect_vectors()
            .map_err(|error| anyhow::anyhow!(error))?;
        Ok(store)
    }

    fn open_database(path: &std::path::Path) -> anyhow::Result<Connection> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => {
                db.execute_batch(&format!(
                    "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
            }
            SCHEMA_VERSION => {
                db.query_row("SELECT count(*) FROM notes", [], |row| row.get::<_, i64>(0))?;
            }
            other => anyhow::bail!("index schema {other} is not {SCHEMA_VERSION}"),
        }
        Ok(db)
    }

    pub fn repository(&self) -> &Repository {
        &self.git
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        self.db
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db_error)
    }

    fn indexed_commit(&self) -> Result<Option<String>> {
        Ok(self
            .meta("indexed_commit")?
            .filter(|commit| !commit.is_empty()))
    }

    fn vector_generation(&self) -> Result<String> {
        Ok(self
            .meta("vector_generation")?
            .unwrap_or_else(|| "0".to_string()))
    }

    /// Make the projection describe the branch head, rebuilding it when they differ.
    pub fn sync(&mut self) -> Result<Option<String>> {
        let head = self.git.head().map_err(git_error)?;
        if self.stale || self.indexed_commit()? != head {
            self.rebuild(head.as_deref())?;
            self.stale = false;
        }
        Ok(head)
    }

    fn ensure_current(&mut self) -> Result<()> {
        if self.stale {
            self.sync()?;
        }
        Ok(())
    }

    /// Derive every row from one commit. Vectors are kept and matched by input hash.
    pub fn rebuild(&mut self, head: Option<&str>) -> Result<()> {
        let started = std::time::Instant::now();
        let mut rows = Vec::new();
        let mut receipts = Vec::new();
        if let Some(head) = head {
            let files: Vec<_> = self
                .git
                .tree(head)
                .map_err(git_error)?
                .into_iter()
                .filter_map(|file| notes::classify(&file.path).ok().map(|scope| (file, scope)))
                .collect();
            let blobs: Vec<String> = files.iter().map(|(file, _)| file.blob.clone()).collect();
            let contents = self.git.blobs(&blobs).map_err(git_error)?;
            let revisions = self.git.path_revisions(head).map_err(git_error)?;
            for ((file, scope), content) in files.into_iter().zip(contents) {
                let Ok(markdown) = String::from_utf8(content) else {
                    tracing::warn!(id = %file.path, "skipping a note that is not UTF-8");
                    continue;
                };
                let (note, _) = notes::parse(&file.path, &scope, &markdown);
                let revision = revisions
                    .get(&file.path)
                    .cloned()
                    .unwrap_or_else(|| head.to_string());
                rows.push((note, markdown, revision));
            }
            receipts = self.git.receipts(head).map_err(git_error)?;
        }
        let count = rows.len();
        let tx = self.db.transaction().map_err(db_error)?;
        tx.execute_batch("DELETE FROM notes; DELETE FROM notes_fts; DELETE FROM requests;")
            .map_err(db_error)?;
        for (note, markdown, revision) in &rows {
            insert_note(&tx, note, markdown, revision)?;
        }
        for receipt in &receipts {
            tx.execute(
                "INSERT OR IGNORE INTO requests VALUES (?1, ?2, ?3, ?4)",
                params![
                    receipt.request_id,
                    receipt.payload_hash,
                    receipt.commit,
                    receipt.result
                ],
            )
            .map_err(db_error)?;
        }
        set_meta(&tx, "indexed_commit", head.unwrap_or(""))?;
        tx.commit().map_err(db_error)?;
        tracing::info!(
            operation = "rebuild",
            commit = head.unwrap_or(""),
            notes = count,
            ms = started.elapsed().as_millis() as u64,
            "index rebuilt"
        );
        Ok(())
    }

    /// Drop vectors that no note or current model needs.
    fn collect_vectors(&mut self) -> Result<()> {
        self.db
            .execute(
                "DELETE FROM vectors WHERE model IS NOT ?1
                 OR input_hash NOT IN (SELECT embed_hash FROM notes WHERE embed_hash IS NOT NULL)",
                params![self.model],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// The complete root instructions, never summarized or truncated.
    pub fn instructions(&mut self) -> Result<Document> {
        self.ensure_current()?;
        self.document(ROOT_ID)?.ok_or_else(|| {
            WikiError::new(
                Code::NotFound,
                "the wiki has no root README.md, so no instructions can be loaded",
            )
        })
    }

    fn document(&self, id: &str) -> Result<Option<Document>> {
        self.db
            .query_row(
                "SELECT id, project, scope, kind, status, title, description, tags, applies_to,
                        revision, markdown
                 FROM notes WHERE id = ?1",
                [id],
                |row| {
                    Ok(Document {
                        id: row.get(0)?,
                        project: row.get(1)?,
                        scope: row.get(2)?,
                        kind: row.get(3)?,
                        status: row.get(4)?,
                        title: row.get(5)?,
                        description: row.get(6)?,
                        tags: from_json_list(&row.get::<_, String>(7)?),
                        applies_to: from_json_list(&row.get::<_, String>(8)?),
                        revision: row.get(9)?,
                        markdown: row.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(db_error)
    }

    /// Every committed record, ordered by identifier.
    pub fn documents(&mut self) -> Result<Vec<Document>> {
        self.ensure_current()?;
        let ids = self.ids("SELECT n.id FROM notes n ORDER BY n.id", &[])?;
        let mut documents = Vec::with_capacity(ids.len());
        for id in ids {
            documents.extend(self.document(&id)?);
        }
        Ok(documents)
    }

    /// Full records from one committed view.
    pub fn read(&mut self, ids: &[String]) -> Result<ReadResponse> {
        self.ensure_current()?;
        if ids.is_empty() || ids.len() > MAX_READ_IDS {
            return Err(WikiError::invalid(format!(
                "ids must hold 1 to {MAX_READ_IDS} identifiers, got {}",
                ids.len()
            )));
        }
        let problems: Vec<Problem> = ids
            .iter()
            .filter_map(|id| {
                notes::classify(id)
                    .err()
                    .map(|message| Problem::new(Code::InvalidInput, id, message))
            })
            .collect();
        if !problems.is_empty() {
            return Err(WikiError::rejected(problems));
        }
        let mut response = ReadResponse {
            records: Vec::new(),
            missing: Vec::new(),
        };
        let mut seen = HashSet::new();
        for id in ids.iter().filter(|id| seen.insert(id.as_str())) {
            match self.document(id)? {
                Some(document) => response.records.push(document),
                None => response.missing.push(id.clone()),
            }
        }
        Ok(response)
    }

    fn filters(&self, request: &SearchRequest) -> Result<(String, Vec<Box<dyn ToSql>>)> {
        let mut clauses: Vec<String> = Vec::new();
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        let scope = match request.scope {
            SearchScope::Projects => {
                clauses.push("n.overview = 1".to_string());
                "project"
            }
            SearchScope::Project => "project",
            SearchScope::Standalone => "standalone",
            SearchScope::TaskInstructions => "task_instructions",
            SearchScope::Prompts => "prompts",
            SearchScope::References => "references",
        };
        clauses.push("n.scope = ?".to_string());
        values.push(Box::new(scope.to_string()));
        match (&request.scope, &request.project) {
            (SearchScope::Project, Some(project)) => {
                let known: bool = self
                    .db
                    .query_row(
                        "SELECT EXISTS (SELECT 1 FROM notes WHERE scope = 'project' AND project = ?1)",
                        [project],
                        |row| row.get(0),
                    )
                    .map_err(db_error)?;
                if !known {
                    return Err(WikiError::new(
                        Code::NotFound,
                        format!("no project {project:?}, so search scope projects to list them"),
                    ));
                }
                clauses.push("n.project = ?".to_string());
                values.push(Box::new(project.clone()));
            }
            (SearchScope::Project, None) => {
                return Err(WikiError::invalid(
                    "scope project requires the project identifier",
                ));
            }
            (_, Some(_)) => {
                return Err(WikiError::invalid(
                    "project is only valid with scope project",
                ));
            }
            (_, None) => {}
        }
        if let Some(kind) = &request.kind {
            clauses.push("n.kind = ?".to_string());
            values.push(Box::new(kind.clone()));
        }
        let statuses = request.statuses();
        let marks = vec!["?"; statuses.len()].join(", ");
        clauses.push(format!("n.status IN ({marks})"));
        for status in statuses {
            values.push(Box::new(status.name().to_string()));
        }
        for tag in &request.tags {
            clauses.push("EXISTS (SELECT 1 FROM json_each(n.tags) WHERE value = ?)".to_string());
            values.push(Box::new(tag.clone()));
        }
        if let Some(label) = &request.applies_to {
            clauses
                .push("EXISTS (SELECT 1 FROM json_each(n.applies_to) WHERE value = ?)".to_string());
            values.push(Box::new(label.clone()));
        }
        Ok((clauses.join(" AND "), values))
    }

    /// Whether a request needs a query vector from the embedding worker.
    pub fn needs_query_vector(request: &SearchRequest) -> bool {
        request.effective_mode().is_some_and(Mode::uses_semantic)
    }

    /// Search or enumerate one scope. `query_vector` must come from the current model.
    pub fn search(
        &mut self,
        request: &SearchRequest,
        query_vector: Option<&[f32]>,
    ) -> Result<SearchResponse> {
        self.ensure_current()?;
        let limit = request.page_size()?;
        let mode = request.effective_mode();
        let commit = self.indexed_commit()?.unwrap_or_default();
        let generation = match mode {
            Some(mode) if mode.uses_semantic() => {
                format!("{commit}:{}", self.vector_generation()?)
            }
            _ => commit,
        };
        let offset = search::decode_cursor(request, &generation)?;
        let (filter, values) = self.filters(request)?;

        let mut excerpts: HashMap<String, String> = HashMap::new();
        let mut coverage = None;
        let ranked: Vec<String> = match mode {
            None => self.ids(
                &format!("SELECT n.id FROM notes n WHERE {filter} ORDER BY n.id"),
                &values,
            )?,
            Some(mode) => {
                let text = request.text().expect("a mode implies query text");
                let keyword = if mode.uses_keyword() {
                    Some(self.keyword_ranking(text, &filter, &values, &mut excerpts)?)
                } else {
                    None
                };
                let semantic = if mode.uses_semantic() {
                    let vector = query_vector.ok_or_else(|| {
                        WikiError::new(
                            Code::SemanticUnavailable,
                            "semantic search has no query embedding",
                        )
                    })?;
                    let (ranking, covered) = self.semantic_ranking(vector, &filter, &values)?;
                    coverage = Some(covered);
                    Some(ranking)
                } else {
                    None
                };
                match (keyword, semantic) {
                    (Some(keyword), Some(semantic)) => search::fuse(&[&keyword, &semantic]),
                    (Some(ranking), None) | (None, Some(ranking)) => ranking,
                    (None, None) => unreachable!("every mode uses a retrieval method"),
                }
            }
        };

        let total = ranked.len();
        let page: Vec<&String> = ranked.iter().skip(offset).take(limit).collect();
        let mut records = Vec::with_capacity(page.len());
        for id in page {
            let document = self.document(id)?.ok_or_else(|| {
                WikiError::unavailable(format!("index lost {id} during a search"))
            })?;
            records.push(SearchRecord {
                // an excerpt that only repeats the description adds nothing
                excerpt: excerpts
                    .remove(id)
                    .filter(|excerpt| *excerpt != document.description),
                id: document.id,
                project: document.project,
                kind: document.kind,
                status: match document.status.as_str() {
                    "proposed" => Status::Proposed,
                    "superseded" => Status::Superseded,
                    _ => Status::Current,
                },
                title: document.title,
                description: document.description,
                revision: document.revision,
            });
        }
        let end = offset + records.len();
        Ok(SearchResponse {
            records,
            mode_used: match mode {
                None => "enumeration",
                Some(Mode::Keyword) => "keyword",
                Some(Mode::Semantic) => "semantic",
                Some(Mode::Hybrid) => "hybrid",
            },
            total,
            next_cursor: (end < total).then(|| search::encode_cursor(request, &generation, end)),
            complete: coverage.is_none_or(|c: SemanticCoverage| c.indexed == c.eligible),
            semantic_coverage: coverage,
        })
    }

    fn ids(&self, sql: &str, values: &[Box<dyn ToSql>]) -> Result<Vec<String>> {
        let mut statement = self.db.prepare(sql).map_err(db_error)?;
        let rows = statement
            .query_map(params_from_iter(values.iter().map(|v| v.as_ref())), |row| {
                row.get(0)
            })
            .map_err(db_error)?;
        rows.collect::<rusqlite::Result<Vec<String>>>()
            .map_err(db_error)
    }

    fn keyword_ranking(
        &self,
        text: &str,
        filter: &str,
        values: &[Box<dyn ToSql>],
        excerpts: &mut HashMap<String, String>,
    ) -> Result<Vec<String>> {
        let expression = search::fts_query(text).ok_or_else(|| {
            WikiError::invalid(format!(
                "query {text:?} has no letters or digits to search for"
            ))
        })?;
        // title and description outweigh tags and path, which outweigh the body
        let sql = format!(
            "SELECT n.id, snippet(notes_fts, -1, '', '', ' ... ', {EXCERPT_TOKENS})
             FROM notes_fts JOIN notes n ON n.rowid = notes_fts.rowid
             WHERE notes_fts MATCH ? AND {filter}
             ORDER BY bm25(notes_fts, 8.0, 5.0, 3.0, 2.0, 1.0), n.id"
        );
        let mut all: Vec<&dyn ToSql> = vec![&expression];
        all.extend(values.iter().map(|v| v.as_ref()));
        let mut statement = self.db.prepare(&sql).map_err(db_error)?;
        let rows = statement
            .query_map(all.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_error)?;
        let mut ranking = Vec::new();
        for row in rows {
            let (id, excerpt) = row.map_err(db_error)?;
            let excerpt = excerpt.split_whitespace().collect::<Vec<_>>().join(" ");
            if !excerpt.is_empty() {
                excerpts.insert(id.clone(), excerpt);
            }
            ranking.push(id);
        }
        Ok(ranking)
    }

    fn semantic_ranking(
        &self,
        query: &[f32],
        filter: &str,
        values: &[Box<dyn ToSql>],
    ) -> Result<(Vec<String>, SemanticCoverage)> {
        let model = self.model.as_ref().ok_or_else(|| {
            WikiError::new(
                Code::SemanticUnavailable,
                "this service runs without an embedding model",
            )
        })?;
        let sql = format!(
            "SELECT n.id, v.state, v.data FROM notes n
             LEFT JOIN vectors v ON v.input_hash = n.embed_hash AND v.model = ?
             WHERE {filter}"
        );
        let mut all: Vec<&dyn ToSql> = vec![model];
        all.extend(values.iter().map(|v| v.as_ref()));
        let mut statement = self.db.prepare(&sql).map_err(db_error)?;
        let rows = statement
            .query_map(all.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                ))
            })
            .map_err(db_error)?;
        let mut coverage = SemanticCoverage {
            eligible: 0,
            indexed: 0,
            pending: 0,
            overlength: 0,
        };
        let mut scored = Vec::new();
        for row in rows {
            let (id, state, data) = row.map_err(db_error)?;
            coverage.eligible += 1;
            match (state.as_deref(), data) {
                (Some(VECTOR_OK), Some(data)) => {
                    let vector = search::decode_vector(&data);
                    if vector.len() != query.len() {
                        return Err(WikiError::unavailable(format!(
                            "stored vector of {id} has {} dimensions, expected {}",
                            vector.len(),
                            query.len()
                        )));
                    }
                    coverage.indexed += 1;
                    scored.push((id, search::similarity(query, &vector)));
                }
                (Some(VECTOR_OVERLENGTH), _) => coverage.overlength += 1,
                _ => coverage.pending += 1,
            }
        }
        Ok((search::rank_by_similarity(scored), coverage))
    }

    /// Embedding inputs that have no vector for the current model yet.
    pub fn pending_embeddings(&mut self, limit: usize) -> Result<Vec<(String, String)>> {
        self.ensure_current()?;
        let Some(model) = &self.model else {
            return Ok(Vec::new());
        };
        let mut statement = self
            .db
            .prepare(
                "SELECT DISTINCT n.embed_hash, n.embed_input FROM notes n
                 WHERE n.embed_hash IS NOT NULL AND NOT EXISTS (
                     SELECT 1 FROM vectors v WHERE v.input_hash = n.embed_hash AND v.model = ?1)
                 ORDER BY n.embed_hash LIMIT ?2",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map(params![model, limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(db_error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)
    }

    /// Keep a finished embedding only if a current note still needs that exact input.
    ///
    /// Returns whether it was stored. A result for changed content is dropped.
    pub fn store_embedding(
        &mut self,
        model: &str,
        input_hash: &str,
        embedded: &Embedded,
    ) -> Result<bool> {
        if self.model.as_deref() != Some(model) {
            return Ok(false);
        }
        let tx = self.db.transaction().map_err(db_error)?;
        let needed: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM notes WHERE embed_hash = ?1)",
                [input_hash],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if !needed {
            return Ok(false);
        }
        let (state, data) = match embedded {
            Embedded::Vector(vector) => (VECTOR_OK, Some(search::encode_vector(vector))),
            Embedded::Overlength => (VECTOR_OVERLENGTH, None),
        };
        let stored = tx
            .execute(
                "INSERT OR IGNORE INTO vectors VALUES (?1, ?2, ?3, ?4)",
                params![input_hash, model, state, data],
            )
            .map_err(db_error)?;
        if stored > 0 {
            let generation: u64 = tx
                .query_row(
                    "SELECT value FROM meta WHERE key = 'vector_generation'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(db_error)?
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            set_meta(&tx, "vector_generation", &(generation + 1).to_string())?;
        }
        tx.commit().map_err(db_error)?;
        Ok(stored > 0)
    }

    pub fn health(&mut self) -> Result<IndexHealth> {
        self.ensure_current()?;
        let count = |sql: &str, values: &[&dyn ToSql]| -> Result<usize> {
            self.db
                .query_row(sql, values, |row| row.get::<_, i64>(0))
                .map(|n| n as usize)
                .map_err(db_error)
        };
        let with_state = |state: &str| {
            count(
                "SELECT count(*) FROM notes n JOIN vectors v
                 ON v.input_hash = n.embed_hash AND v.model = ?1 AND v.state = ?2",
                &[&self.model, &state],
            )
        };
        let indexed = with_state(VECTOR_OK)?;
        let overlength = with_state(VECTOR_OVERLENGTH)?;
        let embeddable = match self.model {
            Some(_) => count(
                "SELECT count(*) FROM notes WHERE embed_hash IS NOT NULL",
                &[],
            )?,
            None => 0,
        };
        Ok(IndexHealth {
            commit: self.indexed_commit()?,
            notes: count("SELECT count(*) FROM notes", &[])?,
            semantic_model: self.model.clone(),
            semantic_indexed: indexed,
            semantic_pending: embeddable - indexed - overlength,
            semantic_overlength: overlength,
        })
    }

    /// Schema problems of the committed notes, keyed by identifier.
    pub fn problems(&mut self) -> Result<BTreeMap<String, Vec<String>>> {
        self.ensure_current()?;
        let mut statement = self
            .db
            .prepare("SELECT id, markdown FROM notes ORDER BY id")
            .map_err(db_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_error)?;
        let mut found = BTreeMap::new();
        let mut projects: BTreeMap<String, bool> = BTreeMap::new();
        for row in rows {
            let (id, markdown) = row.map_err(db_error)?;
            let scope = notes::classify(&id).map_err(WikiError::unavailable)?;
            let (note, problems) = notes::parse(&id, &scope, &markdown);
            if let Some(project) = scope.project() {
                *projects.entry(project.to_string()).or_default() |= note.is_project_overview();
            }
            if !problems.is_empty() {
                found.insert(id, problems);
            }
        }
        for (project, _) in projects
            .into_iter()
            .filter(|(_, has_overview)| !has_overview)
        {
            found.insert(
                format!("memory/{project}/README.md"),
                vec!["the project has notes but no README.md".to_string()],
            );
        }
        Ok(found)
    }

    fn revision(&self, id: &str) -> Result<Option<String>> {
        self.db
            .query_row("SELECT revision FROM notes WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db_error)
    }

    fn semantic_state(&self, note: &Note) -> Result<&'static str> {
        let Some(model) = &self.model else {
            return Ok("unavailable");
        };
        let hash = sha256_hex(note.embedding_input().as_bytes());
        let state: Option<String> = self
            .db
            .query_row(
                "SELECT state FROM vectors WHERE input_hash = ?1 AND model = ?2",
                params![hash, model],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        Ok(match state.as_deref() {
            Some(VECTOR_OK) => "indexed",
            Some(VECTOR_OVERLENGTH) => "overlength",
            _ => "pending",
        })
    }

    /// Check a whole batch against the current view. Returns the parsed notes
    /// to write, in the order of the changes that carry Markdown.
    fn validate(&self, changes: &[Change], allow_protected: bool) -> Result<Vec<Note>> {
        let mut problems = Vec::new();
        let mut written = Vec::new();
        let mut seen = HashSet::new();
        let mut touched_projects = BTreeSet::new();
        for change in changes {
            let id = change.id();
            if !seen.insert(id) {
                problems.push(Problem::new(
                    Code::InvalidInput,
                    id,
                    "the batch names this identifier more than once",
                ));
                continue;
            }
            let scope = match notes::classify(id) {
                Ok(scope) => scope,
                Err(message) => {
                    problems.push(Problem::new(Code::InvalidInput, id, message));
                    continue;
                }
            };
            if scope.is_protected() && !allow_protected {
                problems.push(Problem::new(
                    Code::Protected,
                    id,
                    "root and task instructions change only through operator maintenance",
                ));
                continue;
            }
            if let Some(project) = scope.project() {
                touched_projects.insert(project.to_string());
            }
            let current = self.document(id)?;
            match (change, &current) {
                (Change::Create { .. }, Some(existing)) => {
                    let mut problem = Problem::new(
                        Code::AlreadyExists,
                        id,
                        "a record with this identifier exists",
                    );
                    problem.current_revision = Some(existing.revision.clone());
                    problems.push(problem);
                    continue;
                }
                (Change::Update { .. } | Change::Delete { .. }, None) => {
                    problems.push(Problem::new(
                        Code::NotFound,
                        id,
                        "no record has this identifier",
                    ));
                    continue;
                }
                (
                    Change::Update {
                        expected_revision, ..
                    }
                    | Change::Delete {
                        expected_revision, ..
                    },
                    Some(existing),
                ) if *expected_revision != existing.revision => {
                    let mut problem = Problem::new(
                        Code::StaleRevision,
                        id,
                        "the record changed since expected_revision, so read it again and reconcile",
                    );
                    problem.current_revision = Some(existing.revision.clone());
                    problems.push(problem);
                    continue;
                }
                _ => {}
            }
            if let Some(markdown) = change.markdown() {
                if current.is_some_and(|existing| existing.markdown == markdown) {
                    problems.push(Problem::new(
                        Code::InvalidInput,
                        id,
                        "markdown equals the current content, so there is nothing to update",
                    ));
                    continue;
                }
                let (note, schema_problems) = notes::parse(id, &scope, markdown);
                if schema_problems.is_empty() {
                    written.push(note);
                } else {
                    problems.extend(
                        schema_problems
                            .into_iter()
                            .map(|message| Problem::new(Code::InvalidInput, id, message)),
                    );
                }
            }
        }
        if problems.is_empty() {
            problems.extend(self.overview_problems(changes, &touched_projects)?);
        }
        if problems.is_empty() {
            Ok(written)
        } else {
            Err(WikiError::rejected(problems))
        }
    }

    /// After the batch, every project that has notes must still have its README.md.
    fn overview_problems(
        &self,
        changes: &[Change],
        projects: &BTreeSet<String>,
    ) -> Result<Vec<Problem>> {
        let mut problems = Vec::new();
        for project in projects {
            let prefix = format!("memory/{project}/");
            let mut ids: BTreeSet<String> = self
                .ids(
                    "SELECT n.id FROM notes n WHERE n.scope = 'project' AND n.project = ?",
                    &[Box::new(project.clone()) as Box<dyn ToSql>],
                )?
                .into_iter()
                .collect();
            for change in changes.iter().filter(|c| c.id().starts_with(&prefix)) {
                match change {
                    Change::Delete { id, .. } => ids.remove(id),
                    other => ids.insert(other.id().to_string()),
                };
            }
            let overview = format!("{prefix}README.md");
            if !ids.is_empty() && !ids.contains(&overview) {
                problems.push(Problem::new(
                    Code::InvalidInput,
                    &overview,
                    format!("project {project:?} would have notes without its README.md overview"),
                ));
            }
        }
        Ok(problems)
    }

    /// Apply a batch atomically, or return the saved outcome of an earlier commit
    /// of the same request.
    pub fn update(
        &mut self,
        request: &UpdateRequest,
        allow_protected: bool,
    ) -> Result<UpdateReceipt> {
        let started = std::time::Instant::now();
        if !valid_request_id(&request.request_id) {
            return Err(WikiError::invalid(format!(
                "request_id must be 1 to {MAX_REQUEST_ID_BYTES} characters from letters, digits, '.', '_', ':' and '-'"
            )));
        }
        if request.changes.is_empty() {
            return Err(WikiError::invalid("changes must hold at least one change"));
        }
        let head = self.sync()?;
        let payload_hash =
            sha256_hex(&serde_json::to_vec(&request.changes).expect("changes serialize"));

        let saved: Option<(String, String, String)> = self
            .db
            .query_row(
                "SELECT payload_hash, commit_sha, result FROM requests WHERE request_id = ?1",
                [&request.request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(db_error)?;
        if let Some((saved_hash, commit, result)) = saved {
            if saved_hash != payload_hash {
                return Err(WikiError::new(
                    Code::RequestIdReuse,
                    format!(
                        "request_id {:?} was already committed with a different payload",
                        request.request_id
                    ),
                ));
            }
            let changes: Vec<ChangeResult> = serde_json::from_str(&result).map_err(|error| {
                WikiError::unavailable(format!("saved receipt in {commit} is unreadable: {error}"))
            })?;
            return Ok(receipt(&request.request_id, &commit, true, changes));
        }

        let written = match self.validate(&request.changes, allow_protected) {
            Ok(written) => written,
            Err(error) => {
                tracing::info!(
                    operation = "update",
                    request_id = %request.request_id,
                    outcome = "rejected",
                    code = ?error.code,
                    problems = error.details.len(),
                    "update rejected"
                );
                return Err(error);
            }
        };
        let mut notes_by_id: HashMap<&str, &Note> = written
            .iter()
            .map(|note| (note.id.as_str(), note))
            .collect();
        let mut results = Vec::with_capacity(request.changes.len());
        for change in &request.changes {
            let semantic = match notes_by_id.get(change.id()) {
                Some(note) if note.scope != Scope::Root => {
                    Some(self.semantic_state(note)?.to_string())
                }
                _ => None,
            };
            results.push(ChangeResult {
                id: change.id().to_string(),
                operation: change.operation().to_string(),
                semantic,
                revision: None,
            });
        }
        let edits: Vec<TreeEdit> = request
            .changes
            .iter()
            .map(|change| TreeEdit {
                path: change.id().to_string(),
                content: change.markdown().map(str::to_string),
            })
            .collect();
        let result_json = serde_json::to_string(&results).expect("results serialize");
        let message = commit_message(&request.request_id, &payload_hash, &result_json, &results);

        let commit = self
            .git
            .commit(head.as_deref(), &edits, &message)
            .map_err(git_error)?;
        // nothing is visible before this point, and the change is committed after it
        self.git
            .publish(&commit, head.as_deref())
            .map_err(git_error)?;

        let projected = (|| -> Result<()> {
            #[cfg(test)]
            if std::mem::take(&mut self.fail_next_projection) {
                return Err(WikiError::unavailable("injected projection failure"));
            }
            let tx = self.db.transaction().map_err(db_error)?;
            for change in &request.changes {
                delete_note(&tx, change.id())?;
                if let (Some(note), Some(markdown)) =
                    (notes_by_id.remove(change.id()), change.markdown())
                {
                    insert_note(&tx, note, markdown, &commit)?;
                }
            }
            tx.execute(
                "INSERT INTO requests VALUES (?1, ?2, ?3, ?4)",
                params![request.request_id, payload_hash, commit, result_json],
            )
            .map_err(db_error)?;
            set_meta(&tx, "indexed_commit", &commit)?;
            tx.commit().map_err(db_error)
        })();
        if let Err(error) = projected {
            self.stale = true;
            tracing::error!(
                operation = "update",
                request_id = %request.request_id,
                commit = %commit,
                error = %error.message,
                "committed, but the index did not follow"
            );
            return Err(WikiError::new(
                Code::OutcomeUncertain,
                format!(
                    "the change was committed as {commit} but indexing failed, so retry request_id {:?} with the same payload to recover the outcome, and do not assume a rollback",
                    request.request_id
                ),
            ));
        }
        tracing::info!(
            operation = "update",
            request_id = %request.request_id,
            outcome = "committed",
            commit = %commit,
            changes = results.len(),
            ms = started.elapsed().as_millis() as u64,
            "update committed"
        );
        Ok(receipt(&request.request_id, &commit, false, results))
    }

    /// The current revision of each identifier, for revision-checked imports.
    pub fn revisions(&mut self, ids: &[String]) -> Result<HashMap<String, String>> {
        self.ensure_current()?;
        let mut revisions = HashMap::new();
        for id in ids {
            if let Some(revision) = self.revision(id)? {
                revisions.insert(id.clone(), revision);
            }
        }
        Ok(revisions)
    }
}

fn receipt(
    request_id: &str,
    commit: &str,
    replayed: bool,
    mut changes: Vec<ChangeResult>,
) -> UpdateReceipt {
    for change in &mut changes {
        change.revision = (change.operation != "delete").then(|| commit.to_string());
    }
    UpdateReceipt {
        request_id: request_id.to_string(),
        commit: commit.to_string(),
        replayed,
        changes,
    }
}

fn commit_message(
    request_id: &str,
    payload_hash: &str,
    result: &str,
    changes: &[ChangeResult],
) -> String {
    let describe = |change: &ChangeResult| {
        let mut verb = change.operation.clone();
        verb[..1].make_ascii_uppercase();
        format!("{verb} {}", change.id)
    };
    let mut message = match changes {
        [only] => describe(only),
        many => {
            let lines: Vec<String> = many.iter().map(describe).collect();
            format!("Apply {} wiki changes\n\n{}", many.len(), lines.join("\n"))
        }
    };
    message.push_str(&format!(
        "\n\n{TRAILER_REQUEST}: {request_id}\n{TRAILER_PAYLOAD}: {payload_hash}\n{TRAILER_RESULT}: {result}\n"
    ));
    message
}

fn set_meta(db: &Connection, key: &str, value: &str) -> Result<()> {
    db.execute(
        "INSERT INTO meta VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map(|_| ())
    .map_err(db_error)
}

fn delete_note(db: &Connection, id: &str) -> Result<()> {
    let rowid: Option<i64> = db
        .query_row("SELECT rowid FROM notes WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(db_error)?;
    if let Some(rowid) = rowid {
        db.execute("DELETE FROM notes_fts WHERE rowid = ?1", [rowid])
            .map_err(db_error)?;
        db.execute("DELETE FROM notes WHERE rowid = ?1", [rowid])
            .map_err(db_error)?;
    }
    Ok(())
}

fn insert_note(db: &Connection, note: &Note, markdown: &str, revision: &str) -> Result<()> {
    // root instructions are loaded whole and never searched
    let embed_input = (note.scope != Scope::Root).then(|| note.embedding_input());
    let embed_hash = embed_input
        .as_ref()
        .map(|input| sha256_hex(input.as_bytes()));
    db.execute(
        "INSERT INTO notes (id, scope, project, overview, kind, status, title, description,
                            tags, applies_to, revision, markdown, embed_hash, embed_input)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            note.id,
            note.scope.name(),
            note.scope.project(),
            note.is_project_overview(),
            note.kind,
            note.status.name(),
            note.title,
            note.description,
            json_list(&note.tags),
            json_list(&note.applies_to),
            revision,
            markdown,
            embed_hash,
            embed_input,
        ],
    )
    .map_err(db_error)?;
    db.execute(
        "INSERT INTO notes_fts (rowid, title, description, tags, path, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            db.last_insert_rowid(),
            note.title,
            note.description,
            note.tags.join(" "),
            note.id,
            note.body,
        ],
    )
    .map_err(db_error)?;
    Ok(())
}
