//! The four wiki operations, independent of the MCP adapter.
//!
//! One storage thread owns the `Store`. An indexer thread feeds it embeddings,
//! and a replication thread pushes committed state. Neither can change content.

use std::sync::mpsc::{Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::embeddings::{EmbedError, EmbedWorker};
use crate::error::{Code, Result, WikiError};
use crate::git::Repository;
use crate::search::{SearchRequest, SearchResponse};
use crate::store::{Document, Embedded, ReadResponse, Store, UpdateReceipt, UpdateRequest};

const INDEXING_BATCH: usize = 16;

type StoreJob = Box<dyn FnOnce(&mut Store) + Send>;

/// Sends closures to the storage thread, which runs them one at a time.
#[derive(Clone)]
pub struct StoreHandle {
    jobs: Sender<StoreJob>,
}

impl StoreHandle {
    pub fn spawn(mut store: Store) -> Self {
        let (jobs, queue) = channel::<StoreJob>();
        std::thread::Builder::new()
            .name("storage".to_string())
            .spawn(move || {
                for job in queue {
                    job(&mut store);
                }
            })
            .expect("starting the storage thread");
        Self { jobs }
    }

    pub async fn call<R, F>(&self, job: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Store) -> Result<R> + Send + 'static,
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.jobs
            .send(Box::new(move |store| {
                let _ = sender.send(job(store));
            }))
            .map_err(|_| stopped())?;
        receiver.await.map_err(|_| stopped())?
    }

    pub fn call_blocking<R, F>(&self, job: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Store) -> Result<R> + Send + 'static,
    {
        let (sender, receiver) = channel();
        self.jobs
            .send(Box::new(move |store| {
                let _ = sender.send(job(store));
            }))
            .map_err(|_| stopped())?;
        receiver.recv().map_err(|_| stopped())?
    }
}

fn stopped() -> WikiError {
    WikiError::unavailable("the storage worker stopped")
}

/// Embed every pending input once. Returns how many results were stored.
///
/// A failed embedding stops the pass so a broken model cannot spin.
pub fn index_pending(store: &StoreHandle, worker: &EmbedWorker) -> Result<usize> {
    let model = worker.model_id().to_string();
    let mut stored = 0;
    loop {
        let batch = store.call_blocking(|store| store.pending_embeddings(INDEXING_BATCH))?;
        if batch.is_empty() {
            return Ok(stored);
        }
        for (hash, input) in batch {
            let embedded = match worker.embed_background(&input) {
                Ok(vector) => Embedded::Vector(vector),
                Err(EmbedError::Overlength { tokens, limit }) => {
                    tracing::warn!(
                        operation = "embed",
                        input_hash = %hash,
                        tokens,
                        limit,
                        "embedding input is overlength and stays out of semantic search"
                    );
                    Embedded::Overlength
                }
                Err(EmbedError::Failed(message)) => {
                    return Err(WikiError::new(Code::SemanticUnavailable, message));
                }
            };
            let model = model.clone();
            if store.call_blocking(move |store| store.store_embedding(&model, &hash, &embedded))? {
                stored += 1;
            }
        }
    }
}

fn spawn_indexer(store: StoreHandle, worker: EmbedWorker) -> SyncSender<()> {
    let (wake, wakes) = sync_channel::<()>(1);
    std::thread::Builder::new()
        .name("indexer".to_string())
        .spawn(move || {
            for () in wakes {
                match index_pending(&store, &worker) {
                    Ok(0) => {}
                    Ok(stored) => {
                        tracing::info!(operation = "embed", stored, "semantic index updated")
                    }
                    Err(error) => {
                        tracing::error!(operation = "embed", error = %error.message, "semantic indexing stopped")
                    }
                }
            }
        })
        .expect("starting the indexer thread");
    wake
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Replication {
    /// no push remote is configured
    Disabled,
    /// committed locally, and a push is queued or running
    Pending,
    Replicated {
        commit: String,
    },
    /// the local commit stands, and the remote is behind until an operator reconciles
    Failed {
        error: String,
    },
}

fn spawn_replicator(
    repository: Repository,
    remote: String,
    status: Arc<Mutex<Replication>>,
) -> SyncSender<()> {
    let (wake, wakes) = sync_channel::<()>(1);
    std::thread::Builder::new()
        .name("replication".to_string())
        .spawn(move || {
            for () in wakes {
                let head = repository.head().ok().flatten().unwrap_or_default();
                let outcome = match repository.push(&remote) {
                    Ok(()) => {
                        tracing::info!(operation = "replicate", commit = %head, "pushed");
                        Replication::Replicated { commit: head }
                    }
                    Err(error) => {
                        tracing::error!(operation = "replicate", commit = %head, error = %format!("{error:#}"), "push failed");
                        Replication::Failed {
                            error: format!("{error:#}"),
                        }
                    }
                };
                *status.lock().expect("replication status lock") = outcome;
            }
        })
        .expect("starting the replication thread");
    wake
}

fn wake(sender: &Option<SyncSender<()>>) {
    if let Some(sender) = sender {
        // a full channel already holds a wakeup that covers this one
        if let Err(TrySendError::Disconnected(())) = sender.try_send(()) {
            tracing::error!("a background worker stopped");
        }
    }
}

#[derive(Debug, Serialize)]
pub struct UpdateResponse {
    #[serde(flatten)]
    pub receipt: UpdateReceipt,
    pub replication: Replication,
}

#[derive(Clone)]
pub struct Wiki {
    store: StoreHandle,
    embedder: Option<EmbedWorker>,
    indexer: Option<SyncSender<()>>,
    replicator: Option<SyncSender<()>>,
    replication: Arc<Mutex<Replication>>,
}

impl Wiki {
    /// Start the workers around an opened store.
    pub fn start(store: Store, embedder: Option<EmbedWorker>, push_remote: Option<String>) -> Self {
        let repository = store.repository().clone();
        let store = StoreHandle::spawn(store);
        let indexer = embedder
            .clone()
            .map(|worker| spawn_indexer(store.clone(), worker));
        let replication = Arc::new(Mutex::new(match push_remote {
            Some(_) => Replication::Pending,
            None => Replication::Disabled,
        }));
        let replicator =
            push_remote.map(|remote| spawn_replicator(repository, remote, replication.clone()));
        wake(&indexer);
        wake(&replicator);
        Self {
            store,
            embedder,
            indexer,
            replicator,
            replication,
        }
    }

    pub async fn load_instructions(&self) -> Result<Document> {
        self.store.call(|store| store.instructions()).await
    }

    pub async fn search(&self, request: SearchRequest) -> Result<SearchResponse> {
        let vector = if Store::needs_query_vector(&request) {
            let worker = self.embedder.as_ref().ok_or_else(|| {
                WikiError::new(
                    Code::SemanticUnavailable,
                    "semantic search is unavailable because no embedding model is loaded, so ask the user whether to use mode keyword",
                )
            })?;
            let text = request.text().expect("a semantic mode implies query text");
            Some(worker.embed_query(text).await.map_err(|error| match error {
                EmbedError::Overlength { tokens, limit } => WikiError::invalid(format!(
                    "query has {tokens} tokens, above the semantic limit of {limit}, so shorten it or use mode keyword"
                )),
                EmbedError::Failed(message) => WikiError::new(Code::SemanticUnavailable, message),
            })?)
        } else {
            None
        };
        self.store
            .call(move |store| store.search(&request, vector.as_deref()))
            .await
    }

    pub async fn read(&self, ids: Vec<String>) -> Result<ReadResponse> {
        self.store.call(move |store| store.read(&ids)).await
    }

    pub async fn update(&self, request: UpdateRequest) -> Result<UpdateResponse> {
        let receipt = self
            .store
            .call(move |store| store.update(&request, false))
            .await?;
        if !receipt.replayed {
            wake(&self.indexer);
            if self.replicator.is_some() {
                *self.replication.lock().expect("replication status lock") = Replication::Pending;
                wake(&self.replicator);
            }
        }
        let replication = self
            .replication
            .lock()
            .expect("replication status lock")
            .clone();
        Ok(UpdateResponse {
            receipt,
            replication,
        })
    }
}

/// Lets a test wait for a background thread without sleeping blindly.
#[cfg(test)]
fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("condition not reached");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::testing::FakeEmbedder;
    use crate::git::testing;
    use crate::search::{Mode, SearchScope};
    use crate::store::{Change, StoreConfig};

    fn note(title: &str, description: &str) -> String {
        format!(
            "---\ncreated: 2026-11-15\nverified: null\ndescription: {description}\ntype: plan\ntags: []\n---\n\n# {title}\n"
        )
    }

    fn request(query: &str, mode: Mode) -> SearchRequest {
        SearchRequest {
            scope: SearchScope::Standalone,
            project: None,
            query: Some(query.to_string()),
            mode: Some(mode),
            kind: None,
            tags: Vec::new(),
            applies_to: None,
            status: Vec::new(),
            limit: None,
            cursor: None,
        }
    }

    fn update(request_id: &str, id: &str, markdown: String) -> UpdateRequest {
        UpdateRequest {
            request_id: request_id.to_string(),
            changes: vec![Change::Create {
                id: id.to_string(),
                markdown,
            }],
        }
    }

    struct Fixture {
        _repository: tempfile::TempDir,
        _state: tempfile::TempDir,
        store: Option<Store>,
    }

    fn fixture(model: Option<&str>) -> Fixture {
        let (repository, _) = testing::bare();
        let state = tempfile::tempdir().unwrap();
        let store = Store::open(&StoreConfig {
            git_dir: repository.path().to_path_buf(),
            state_dir: state.path().to_path_buf(),
            identity: testing::identity(),
            model: model.map(str::to_string),
        })
        .unwrap();
        Fixture {
            _repository: repository,
            _state: state,
            store: Some(store),
        }
    }

    #[tokio::test]
    async fn semantic_indexing_follows_writes_in_the_background() {
        let mut fixture = fixture(Some("fake-model"));
        let worker = EmbedWorker::spawn(FakeEmbedder {
            directions: vec![("garden", [1.0, 0.0]), ("engine", [0.0, 1.0])],
        });
        let wiki = Wiki::start(fixture.store.take().unwrap(), Some(worker), None);

        let response = wiki
            .update(update(
                "r1",
                "memory/garden.md",
                note("Garden", "Notes about the garden."),
            ))
            .await
            .unwrap();
        assert_eq!(response.replication, Replication::Disabled);
        assert_eq!(
            response.receipt.changes[0].semantic.as_deref(),
            Some("pending")
        );
        wiki.update(update(
            "r2",
            "memory/engine.md",
            note("Engine", "Notes about the engine."),
        ))
        .await
        .unwrap();
        wiki.update(update(
            "r3",
            "memory/long.md",
            note("Long", "An overlength description."),
        ))
        .await
        .unwrap();

        let store = wiki.store.clone();
        tokio::task::spawn_blocking(move || {
            eventually(|| {
                let health = store.call_blocking(|store| store.health()).unwrap();
                health.semantic_pending == 0
            })
        })
        .await
        .unwrap();

        let found = wiki
            .search(request("my garden", Mode::Semantic))
            .await
            .unwrap();
        assert_eq!(found.records[0].id, "memory/garden.md");
        assert!(!found.complete);
        let coverage = found.semantic_coverage.unwrap();
        assert_eq!(
            (coverage.indexed, coverage.overlength, coverage.pending),
            (2, 1, 0)
        );

        let failure = wiki
            .search(request("unknown words", Mode::Hybrid))
            .await
            .unwrap_err();
        assert_eq!(failure.code, Code::SemanticUnavailable);
    }

    #[tokio::test]
    async fn concurrent_edits_of_one_revision_apply_once() {
        let mut fixture = fixture(None);
        let wiki = Wiki::start(fixture.store.take().unwrap(), None, None);
        let created = wiki
            .update(update("create", "memory/a.md", note("A", "Note a.")))
            .await
            .unwrap();
        let edit = |request_id: &str, description: &str| UpdateRequest {
            request_id: request_id.to_string(),
            changes: vec![Change::Update {
                id: "memory/a.md".to_string(),
                expected_revision: created.receipt.commit.clone(),
                markdown: note("A", description),
            }],
        };
        let (left, right) = tokio::join!(
            wiki.update(edit("left", "Edited on the left.")),
            wiki.update(edit("right", "Edited on the right.")),
        );
        let outcomes = [
            left.map(|r| r.receipt.commit),
            right.map(|r| r.receipt.commit),
        ];
        let winners: Vec<_> = outcomes.iter().filter_map(|o| o.as_ref().ok()).collect();
        let losers: Vec<_> = outcomes.iter().filter_map(|o| o.as_ref().err()).collect();
        assert_eq!((winners.len(), losers.len()), (1, 1));
        assert_eq!(losers[0].code, Code::StaleRevision);
        assert_eq!(
            losers[0].details[0].current_revision.as_ref(),
            Some(winners[0])
        );
        let read = wiki.read(vec!["memory/a.md".to_string()]).await.unwrap();
        assert_eq!(&read.records[0].revision, winners[0]);
    }

    #[tokio::test]
    async fn semantic_modes_fail_explicitly_without_a_model() {
        let mut fixture = fixture(None);
        let wiki = Wiki::start(fixture.store.take().unwrap(), None, None);
        let response = wiki
            .update(update(
                "r1",
                "memory/garden.md",
                note("Garden", "Notes about the garden."),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.receipt.changes[0].semantic.as_deref(),
            Some("unavailable")
        );

        let hybrid = wiki
            .search(request("garden", Mode::Hybrid))
            .await
            .unwrap_err();
        assert_eq!(hybrid.code, Code::SemanticUnavailable);
        let keyword = wiki.search(request("garden", Mode::Keyword)).await.unwrap();
        assert_eq!(keyword.records.len(), 1);
    }

    #[tokio::test]
    async fn replication_pushes_commits_and_surfaces_failures() {
        let (remote, remote_repository) = testing::bare();
        let mut pushing = fixture(None);
        let wiki = Wiki::start(
            pushing.store.take().unwrap(),
            None,
            Some(remote.path().to_string_lossy().to_string()),
        );
        let response = wiki
            .update(update("r1", "memory/a.md", note("A", "Note a.")))
            .await
            .unwrap();
        assert_eq!(response.replication, Replication::Pending);
        let commit = response.receipt.commit.clone();
        let status = wiki.replication.clone();
        let pushed = commit.clone();
        tokio::task::spawn_blocking(move || {
            eventually(|| {
                *status.lock().unwrap()
                    == Replication::Replicated {
                        commit: pushed.clone(),
                    }
            })
        })
        .await
        .unwrap();
        assert_eq!(remote_repository.head().unwrap(), Some(commit));

        let mut failing = fixture(None);
        let wiki = Wiki::start(
            failing.store.take().unwrap(),
            None,
            Some("/nonexistent/remote.git".to_string()),
        );
        wiki.update(update("r1", "memory/a.md", note("A", "Note a.")))
            .await
            .unwrap();
        let status = wiki.replication.clone();
        tokio::task::spawn_blocking(move || {
            eventually(|| matches!(*status.lock().unwrap(), Replication::Failed { .. }))
        })
        .await
        .unwrap();
        // the local commit stands, and the next receipt reports the failed replication
        let read = wiki.read(vec!["memory/a.md".to_string()]).await.unwrap();
        assert_eq!(read.records.len(), 1);
    }
}
