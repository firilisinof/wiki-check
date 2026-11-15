use tempfile::TempDir;

use crate::error::Code;
use crate::git::testing;
use crate::notes::Status;
use crate::search::{Mode, SearchRequest, SearchScope};
use crate::store::{Change, Embedded, Store, StoreConfig, UpdateReceipt, UpdateRequest};

const MODEL: &str = "fake-model";

struct Wiki {
    repository: TempDir,
    state: TempDir,
}

impl Wiki {
    fn new() -> Self {
        let (repository, _) = testing::bare();
        Self {
            repository,
            state: tempfile::tempdir().unwrap(),
        }
    }

    fn config(&self) -> StoreConfig {
        StoreConfig {
            git_dir: self.repository.path().to_path_buf(),
            state_dir: self.state.path().to_path_buf(),
            identity: testing::identity(),
            model: Some(MODEL.to_string()),
        }
    }

    fn open(&self) -> Store {
        Store::open(&self.config()).expect("store opens")
    }
}

fn note(kind: &str, title: &str, description: &str, body: &str) -> String {
    format!(
        "---\ncreated: 2026-11-15\nverified: null\ndescription: {description}\ntype: {kind}\ntags: []\n---\n\n# {title}\n\n{body}\n"
    )
}

fn create(id: &str, markdown: String) -> Change {
    Change::Create {
        id: id.to_string(),
        markdown,
    }
}

fn apply(store: &mut Store, request_id: &str, changes: Vec<Change>) -> UpdateReceipt {
    store
        .update(
            &UpdateRequest {
                request_id: request_id.to_string(),
                changes,
            },
            false,
        )
        .expect("update applies")
}

fn reject(store: &mut Store, request_id: &str, changes: Vec<Change>) -> crate::error::WikiError {
    store
        .update(
            &UpdateRequest {
                request_id: request_id.to_string(),
                changes,
            },
            false,
        )
        .expect_err("update is rejected")
}

fn seed_project(store: &mut Store, project: &str) -> String {
    let receipt = apply(
        store,
        &format!("seed-{project}"),
        vec![
            create(
                &format!("memory/{project}/README.md"),
                note(
                    "overview",
                    project,
                    &format!("The {project} project."),
                    "Constraints.",
                ),
            ),
            create(
                &format!("memory/{project}/deploy.md"),
                note(
                    "procedure",
                    "Deploying",
                    &format!("How to deploy {project}."),
                    "Run nixos-rebuild switch.",
                ),
            ),
        ],
    );
    receipt.commit
}

fn search(scope: SearchScope) -> SearchRequest {
    SearchRequest {
        scope,
        project: None,
        query: None,
        mode: None,
        kind: None,
        tags: Vec::new(),
        applies_to: None,
        status: Vec::new(),
        limit: None,
        cursor: None,
    }
}

fn project_search(project: &str, query: Option<&str>, mode: Option<Mode>) -> SearchRequest {
    SearchRequest {
        project: Some(project.to_string()),
        query: query.map(str::to_string),
        mode,
        ..search(SearchScope::Project)
    }
}

fn ids(store: &mut Store, request: &SearchRequest, vector: Option<&[f32]>) -> Vec<String> {
    store
        .search(request, vector)
        .expect("search succeeds")
        .records
        .into_iter()
        .map(|record| record.id)
        .collect()
}

fn read_one(store: &mut Store, id: &str) -> crate::store::Document {
    let mut response = store.read(&[id.to_string()]).unwrap();
    assert!(response.missing.is_empty(), "{id} is missing");
    response.records.remove(0)
}

#[test]
fn writes_are_immediately_readable_and_keyword_searchable() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    let commit = seed_project(&mut store, "pi");

    let document = read_one(&mut store, "memory/pi/deploy.md");
    assert_eq!(document.revision, commit);
    assert_eq!(document.project.as_deref(), Some("pi"));
    assert_eq!(document.kind, "procedure");
    assert!(document.markdown.contains("nixos-rebuild"));

    let response = store
        .search(
            &project_search("pi", Some("nixos-rebuild"), Some(Mode::Keyword)),
            None,
        )
        .unwrap();
    assert_eq!(response.mode_used, "keyword");
    assert!(response.complete);
    assert_eq!(response.records.len(), 1);
    assert_eq!(response.records[0].id, "memory/pi/deploy.md");
    assert!(
        response.records[0]
            .excerpt
            .as_deref()
            .unwrap()
            .contains("nixos-rebuild")
    );
}

#[test]
fn a_stale_revision_rejects_the_whole_batch() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    let first = seed_project(&mut store, "pi");
    let second = apply(
        &mut store,
        "edit-1",
        vec![Change::Update {
            id: "memory/pi/deploy.md".to_string(),
            expected_revision: first.clone(),
            markdown: note(
                "procedure",
                "Deploying",
                "How to deploy pi.",
                "Second version.",
            ),
        }],
    )
    .commit;

    let error = reject(
        &mut store,
        "edit-2",
        vec![
            create(
                "memory/pi/new.md",
                note("finding", "New", "A new finding.", "Text."),
            ),
            Change::Update {
                id: "memory/pi/deploy.md".to_string(),
                expected_revision: first,
                markdown: note(
                    "procedure",
                    "Deploying",
                    "How to deploy pi.",
                    "Lost update.",
                ),
            },
        ],
    );
    assert_eq!(error.code, Code::StaleRevision);
    assert_eq!(
        error.details[0].current_revision.as_deref(),
        Some(second.as_str())
    );
    let response = store.read(&["memory/pi/new.md".to_string()]).unwrap();
    assert_eq!(response.missing, vec!["memory/pi/new.md"]);
    assert!(
        read_one(&mut store, "memory/pi/deploy.md")
            .markdown
            .contains("Second version")
    );
    assert_eq!(store.repository().head().unwrap(), Some(second));
}

#[test]
fn a_retry_returns_the_original_outcome_once() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    seed_project(&mut store, "pi");
    let changes = vec![create(
        "memory/pi/a.md",
        note("finding", "A", "Finding a.", "Text."),
    )];
    let original = apply(&mut store, "req-1", changes.clone());
    assert!(!original.replayed);

    let replay = apply(&mut store, "req-1", changes);
    assert!(replay.replayed);
    assert_eq!(replay.commit, original.commit);
    assert_eq!(replay.changes, original.changes);
    assert_eq!(store.repository().head().unwrap(), Some(original.commit));

    let error = reject(
        &mut store,
        "req-1",
        vec![create(
            "memory/pi/b.md",
            note("finding", "B", "Finding b.", "Text."),
        )],
    );
    assert_eq!(error.code, Code::RequestIdReuse);
}

#[test]
fn deletion_and_recreation_change_the_revision() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    let first = seed_project(&mut store, "pi");
    let markdown = read_one(&mut store, "memory/pi/deploy.md").markdown;

    let deleted = apply(
        &mut store,
        "delete",
        vec![Change::Delete {
            id: "memory/pi/deploy.md".to_string(),
            expected_revision: first.clone(),
        }],
    );
    assert_eq!(deleted.changes[0].revision, None);
    assert!(
        ids(
            &mut store,
            &project_search("pi", Some("nixos-rebuild"), Some(Mode::Keyword)),
            None
        )
        .is_empty()
    );

    let recreated = apply(
        &mut store,
        "recreate",
        vec![create("memory/pi/deploy.md", markdown.clone())],
    );
    assert_ne!(recreated.commit, first);
    let error = reject(
        &mut store,
        "stale",
        vec![Change::Update {
            id: "memory/pi/deploy.md".to_string(),
            expected_revision: first,
            markdown: markdown.replace("Run", "Never run"),
        }],
    );
    assert_eq!(error.code, Code::StaleRevision);
}

#[test]
fn the_projection_and_receipts_survive_database_loss() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    seed_project(&mut store, "pi");
    let changes = vec![create(
        "memory/pi/a.md",
        note("finding", "A", "Finding a.", "Text."),
    )];
    let original = apply(&mut store, "req-1", changes.clone());
    let before = read_one(&mut store, "memory/pi/deploy.md");
    drop(store);

    let rebuilt = Wiki {
        repository: wiki.repository,
        state: tempfile::tempdir().unwrap(),
    };
    let mut store = rebuilt.open();
    assert_eq!(read_one(&mut store, "memory/pi/deploy.md"), before);
    assert_eq!(
        read_one(&mut store, "memory/pi/a.md").revision,
        original.commit
    );
    let replay = apply(&mut store, "req-1", changes);
    assert!(replay.replayed);
    assert_eq!(replay.changes, original.changes);
}

#[test]
fn an_unpublished_commit_changes_nothing() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    let head = seed_project(&mut store, "pi");
    let edit = crate::git::TreeEdit {
        path: "memory/pi/deploy.md".to_string(),
        content: Some("interrupted before publication".to_string()),
    };
    store
        .repository()
        .commit(Some(&head), &[edit], "interrupted")
        .unwrap();
    drop(store);

    let mut store = wiki.open();
    assert_eq!(store.repository().head().unwrap(), Some(head.clone()));
    assert_eq!(read_one(&mut store, "memory/pi/deploy.md").revision, head);
}

#[test]
fn a_failure_after_publication_is_recovered_by_retrying() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    seed_project(&mut store, "pi");
    let changes = vec![create(
        "memory/pi/a.md",
        note("finding", "A", "Finding a.", "Text."),
    )];
    let request = UpdateRequest {
        request_id: "req-1".to_string(),
        changes,
    };

    store.fail_next_projection = true;
    let error = store.update(&request, false).unwrap_err();
    assert_eq!(error.code, Code::OutcomeUncertain);

    let replay = store.update(&request, false).unwrap();
    assert!(replay.replayed);
    assert_eq!(
        store.repository().head().unwrap(),
        Some(replay.commit.clone())
    );
    assert_eq!(
        read_one(&mut store, "memory/pi/a.md").revision,
        replay.commit
    );
}

#[test]
fn protected_and_unserved_destinations_are_rejected() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    let rules = note("rules", "Coding", "Coding rules.", "Rules.");
    for (id, code) in [
        ("README.md", Code::Protected),
        ("global/coding.md", Code::Protected),
        ("memory/pi/index.md", Code::InvalidInput),
        ("memory/../README.md", Code::InvalidInput),
        ("tools/index.py", Code::InvalidInput),
    ] {
        let error = reject(&mut store, "req", vec![create(id, rules.clone())]);
        assert_eq!(error.code, code, "{id}");
    }
    assert_eq!(store.repository().head().unwrap(), None);

    let request = UpdateRequest {
        request_id: "maintenance".to_string(),
        changes: vec![
            create("README.md", "# Wiki\n\nInstructions.\n".to_string()),
            create("global/coding.md", rules),
        ],
    };
    let receipt = store.update(&request, true).unwrap();
    let instructions = store.instructions().unwrap();
    assert_eq!(instructions.markdown, "# Wiki\n\nInstructions.\n");
    assert_eq!(instructions.revision, receipt.commit);
}

#[test]
fn invalid_content_is_rejected_before_any_commit() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    let head = seed_project(&mut store, "pi");
    let valid = note("finding", "A", "Finding a.", "Text.");

    let cases = vec![
        (
            "no frontmatter",
            vec![create("memory/pi/a.md", "# A\n".to_string())],
        ),
        (
            "bad type",
            vec![create("memory/pi/a.md", valid.replace("finding", "diary"))],
        ),
        (
            "too large",
            vec![create(
                "memory/pi/a.md",
                format!("{valid}{}", "x".repeat(4096)),
            )],
        ),
        (
            "duplicate id",
            vec![
                create("memory/pi/a.md", valid.clone()),
                create("memory/pi/a.md", valid.clone()),
            ],
        ),
        (
            "no overview",
            vec![create("memory/other/a.md", valid.clone())],
        ),
        (
            "overview removed",
            vec![Change::Delete {
                id: "memory/pi/README.md".to_string(),
                expected_revision: head.clone(),
            }],
        ),
        (
            "unchanged",
            vec![Change::Update {
                id: "memory/pi/deploy.md".to_string(),
                expected_revision: head.clone(),
                markdown: note(
                    "procedure",
                    "Deploying",
                    "How to deploy pi.",
                    "Run nixos-rebuild switch.",
                ),
            }],
        ),
    ];
    for (name, changes) in cases {
        assert_eq!(
            reject(&mut store, "req", changes).code,
            Code::InvalidInput,
            "{name}"
        );
    }
    let exists = reject(
        &mut store,
        "req",
        vec![create("memory/pi/deploy.md", valid.clone())],
    );
    assert_eq!(exists.code, Code::AlreadyExists);
    let missing = reject(
        &mut store,
        "req",
        vec![Change::Delete {
            id: "memory/pi/none.md".to_string(),
            expected_revision: head.clone(),
        }],
    );
    assert_eq!(missing.code, Code::NotFound);
    assert_eq!(
        reject(&mut store, "bad id!", vec![create("memory/a.md", valid)]).code,
        Code::InvalidInput
    );
    assert_eq!(store.repository().head().unwrap(), Some(head));
}

#[test]
fn search_stays_inside_its_scope() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    seed_project(&mut store, "pi");
    seed_project(&mut store, "mac");
    apply(
        &mut store,
        "standalone",
        vec![create(
            "memory/idea.md",
            note("plan", "Idea", "An idea about deploy tooling.", "Deploy."),
        )],
    );

    assert_eq!(
        ids(
            &mut store,
            &project_search("pi", Some("deploy"), Some(Mode::Keyword)),
            None
        ),
        vec!["memory/pi/deploy.md"]
    );
    assert_eq!(
        ids(&mut store, &search(SearchScope::Projects), None),
        vec!["memory/mac/README.md", "memory/pi/README.md"]
    );
    assert_eq!(
        ids(&mut store, &search(SearchScope::Standalone), None),
        vec!["memory/idea.md"]
    );
    assert!(ids(&mut store, &search(SearchScope::References), None).is_empty());

    let unknown = store
        .search(&project_search("none", None, None), None)
        .unwrap_err();
    assert_eq!(unknown.code, Code::NotFound);
    let unnamed = store
        .search(&search(SearchScope::Project), None)
        .unwrap_err();
    assert_eq!(unnamed.code, Code::InvalidInput);
}

#[test]
fn enumeration_pages_through_every_filtered_record() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    let rules = |label: &str| {
        note("rules", label, &format!("Rules for {label}."), "Text.")
            .replace("tags: []", &format!("tags: []\napplies_to:\n  - {label}"))
    };
    let request = UpdateRequest {
        request_id: "rules".to_string(),
        changes: vec![
            create("global/coding.md", rules("coding")),
            create("global/plotting.md", rules("plotting")),
            create("global/review.md", rules("coding")),
        ],
    };
    store.update(&request, true).unwrap();

    let mut request = SearchRequest {
        applies_to: Some("coding".to_string()),
        limit: Some(1),
        ..search(SearchScope::TaskInstructions)
    };
    let first = store.search(&request, None).unwrap();
    assert_eq!(
        (first.mode_used, first.total, first.complete),
        ("enumeration", 2, true)
    );
    assert_eq!(first.records[0].id, "global/coding.md");
    request.cursor = first.next_cursor;
    let second = store.search(&request, None).unwrap();
    assert_eq!(second.records[0].id, "global/review.md");
    assert_eq!(second.next_cursor, None);
}

#[test]
fn a_cursor_expires_when_the_index_changes() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    seed_project(&mut store, "pi");
    let mut request = SearchRequest {
        limit: Some(1),
        ..project_search("pi", None, None)
    };
    request.cursor = store.search(&request, None).unwrap().next_cursor;
    assert!(request.cursor.is_some());
    apply(
        &mut store,
        "more",
        vec![create(
            "memory/pi/a.md",
            note("finding", "A", "Finding a.", "Text."),
        )],
    );
    assert_eq!(
        store.search(&request, None).unwrap_err().code,
        Code::CursorExpired
    );
}

#[test]
fn superseded_history_is_excluded_unless_requested() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    seed_project(&mut store, "pi");
    let history = note("decisions", "History", "Superseded pi decisions.", "Old.")
        .replace("tags: []", "tags: []\nstatus: superseded");
    let idea = note("plan", "Idea", "A proposed pi change.", "Maybe.")
        .replace("tags: []", "tags: []\nstatus: proposed");
    apply(
        &mut store,
        "statuses",
        vec![
            create("memory/pi/history.md", history),
            create("memory/pi/idea.md", idea),
        ],
    );

    let default = store
        .search(&project_search("pi", None, None), None)
        .unwrap();
    let listed: Vec<_> = default
        .records
        .iter()
        .map(|r| (r.id.as_str(), r.status))
        .collect();
    assert!(listed.contains(&("memory/pi/idea.md", Status::Proposed)));
    assert!(!listed.iter().any(|(id, _)| *id == "memory/pi/history.md"));

    let request = SearchRequest {
        status: vec![Status::Superseded],
        ..project_search("pi", None, None)
    };
    assert_eq!(
        ids(&mut store, &request, None),
        vec!["memory/pi/history.md"]
    );
}

#[test]
fn semantic_search_reports_coverage_and_never_uses_obsolete_vectors() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    let head = seed_project(&mut store, "pi");
    let semantic = project_search("pi", Some("shipping a new system"), Some(Mode::Semantic));

    let missing_vector = store.search(&semantic, None).unwrap_err();
    assert_eq!(missing_vector.code, Code::SemanticUnavailable);

    let pending = store.pending_embeddings(10).unwrap();
    assert_eq!(pending.len(), 2);
    let empty = store.search(&semantic, Some(&[1.0, 0.0])).unwrap();
    assert!(empty.records.is_empty());
    assert!(!empty.complete);
    assert_eq!(empty.semantic_coverage.unwrap().pending, 2);

    // the deploy note points along the query, the overview across it
    for (hash, input) in &pending {
        let vector = if input.contains("Deploying") {
            vec![1.0, 0.0]
        } else {
            vec![0.0, 1.0]
        };
        assert!(
            store
                .store_embedding(MODEL, hash, &Embedded::Vector(vector))
                .unwrap()
        );
    }
    assert!(
        !store
            .store_embedding("other-model", &pending[0].0, &Embedded::Overlength)
            .unwrap()
    );
    let full = store.search(&semantic, Some(&[1.0, 0.0])).unwrap();
    assert!(full.complete);
    assert_eq!(full.records[0].id, "memory/pi/deploy.md");
    assert_eq!(full.records.len(), 2);

    let hybrid = project_search("pi", Some("constraints"), None);
    let fused = store.search(&hybrid, Some(&[1.0, 0.0])).unwrap();
    assert_eq!(fused.mode_used, "hybrid");
    assert_eq!(fused.records.len(), 2);

    // a description change needs a new vector, and the old one must not stand in
    let old_hash = pending
        .iter()
        .find(|(_, input)| input.contains("Deploying"))
        .unwrap()
        .0
        .clone();
    let receipt = apply(
        &mut store,
        "retitle",
        vec![Change::Update {
            id: "memory/pi/deploy.md".to_string(),
            expected_revision: head,
            markdown: note(
                "procedure",
                "Deploying",
                "A different description.",
                "Body.",
            ),
        }],
    );
    assert_eq!(receipt.changes[0].semantic.as_deref(), Some("pending"));
    let partial = store.search(&semantic, Some(&[1.0, 0.0])).unwrap();
    assert!(!partial.complete);
    assert_eq!(partial.records.len(), 1);
    assert_eq!(partial.records[0].id, "memory/pi/README.md");
    assert!(
        !store
            .store_embedding(MODEL, &old_hash, &Embedded::Vector(vec![1.0, 0.0]))
            .unwrap()
    );

    let (new_hash, _) = store.pending_embeddings(10).unwrap().remove(0);
    assert!(
        store
            .store_embedding(MODEL, &new_hash, &Embedded::Overlength)
            .unwrap()
    );
    let overlength = store.search(&semantic, Some(&[1.0, 0.0])).unwrap();
    assert!(!overlength.complete);
    assert_eq!(overlength.semantic_coverage.unwrap().overlength, 1);
    let health = store.health().unwrap();
    assert_eq!(
        (
            health.notes,
            health.semantic_indexed,
            health.semantic_pending,
            health.semantic_overlength
        ),
        (2, 1, 0, 1)
    );
}

#[test]
fn vectors_survive_a_restart_when_the_input_is_unchanged() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    seed_project(&mut store, "pi");
    for (hash, _) in store.pending_embeddings(10).unwrap() {
        store
            .store_embedding(MODEL, &hash, &Embedded::Vector(vec![1.0, 0.0]))
            .unwrap();
    }
    drop(store);
    let mut store = wiki.open();
    assert!(store.pending_embeddings(10).unwrap().is_empty());
    assert_eq!(store.health().unwrap().semantic_indexed, 2);
}

#[test]
fn only_one_process_holds_the_store() {
    let wiki = Wiki::new();
    let _store = wiki.open();
    assert!(Store::open(&wiki.config()).is_err());
}

#[test]
fn reads_validate_and_report_missing_identifiers() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    seed_project(&mut store, "pi");
    assert_eq!(store.read(&[]).unwrap_err().code, Code::InvalidInput);
    assert_eq!(
        store.read(&["../etc/passwd".to_string()]).unwrap_err().code,
        Code::InvalidInput
    );
    let response = store
        .read(&[
            "memory/pi/deploy.md".to_string(),
            "memory/pi/none.md".to_string(),
        ])
        .unwrap();
    assert_eq!(response.records.len(), 1);
    assert_eq!(response.missing, vec!["memory/pi/none.md"]);
    assert_eq!(store.instructions().unwrap_err().code, Code::NotFound);
}

#[test]
fn existing_content_is_indexed_and_its_problems_are_reported() {
    let wiki = Wiki::new();
    let (git_dir, identity) = (wiki.repository.path().to_path_buf(), testing::identity());
    let git = crate::git::Repository::open(&git_dir, identity).unwrap();
    let file = |path: &str, content: &str| crate::git::TreeEdit {
        path: path.to_string(),
        content: Some(content.to_string()),
    };
    let commit = git
        .commit(
            None,
            &[
                file("README.md", "# Wiki\n"),
                file("index.md", "# Generated\n"),
                file("tools/index.py", "print()\n"),
                file(
                    "memory/pi/broken.md",
                    "# No frontmatter\n\nSearchable body.\n",
                ),
            ],
            "manual history",
        )
        .unwrap();
    git.publish(&commit, None).unwrap();

    let mut store = wiki.open();
    assert_eq!(store.health().unwrap().notes, 2);
    assert_eq!(read_one(&mut store, "memory/pi/broken.md").revision, commit);
    let problems = store.problems().unwrap();
    assert!(problems["memory/pi/broken.md"][0].contains("frontmatter"));
    assert!(problems["memory/pi/README.md"][0].contains("no README.md"));
}

#[test]
fn keyword_search_finds_punctuated_identifiers() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    seed_project(&mut store, "pi");
    let body = "Set `core.fsync=committed,reference` and call update_memory with WIKI_MCP_GIT_DIR. See C++ notes in src/main.rs.";
    apply(
        &mut store,
        "identifiers",
        vec![create(
            "memory/pi/mcp-shell.md",
            note(
                "configuration",
                "Remote shell",
                "How the remote shell is configured.",
                body,
            ),
        )],
    );
    for query in [
        "core.fsync",
        "fsync",
        "update_memory",
        "WIKI_MCP_GIT_DIR",
        "wiki_mcp_git_dir",
        "src/main.rs",
        "mcp-shell.md",
        "\"committed,reference\"",
        "C++",
        "configuring the shell",
    ] {
        let found = ids(
            &mut store,
            &project_search("pi", Some(query), Some(Mode::Keyword)),
            None,
        );
        // terms are alternatives, so a common word may also match lower-ranked notes
        assert_eq!(found[0], "memory/pi/mcp-shell.md", "{query}");
    }
    // FTS5 operators in the text are searched as words, not run as syntax
    for query in [
        "NOT deploy",
        "deploy AND",
        "title: nothing*",
        "(nixos",
        "a OR",
    ] {
        let request = project_search("pi", Some(query), Some(Mode::Keyword));
        assert!(store.search(&request, None).is_ok(), "{query}");
    }
    let empty = project_search("pi", Some("--- ***"), Some(Mode::Keyword));
    assert_eq!(
        store.search(&empty, None).unwrap_err().code,
        Code::InvalidInput
    );
}

#[test]
fn a_restored_clone_serves_the_same_revisions() {
    let wiki = Wiki::new();
    let mut store = wiki.open();
    seed_project(&mut store, "pi");
    let changes = vec![create(
        "memory/pi/a.md",
        note("finding", "A", "Finding a.", "Text."),
    )];
    let original = apply(&mut store, "req-1", changes.clone());
    let before = store.documents().unwrap();
    drop(store);

    let restored = Wiki {
        repository: tempfile::tempdir().unwrap(),
        state: tempfile::tempdir().unwrap(),
    };
    let status = std::process::Command::new("git")
        .args(["clone", "--quiet", "--bare"])
        .arg(wiki.repository.path())
        .arg(restored.repository.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut store = restored.open();
    assert_eq!(store.documents().unwrap(), before);
    assert!(apply(&mut store, "req-1", changes).replayed);
    assert_eq!(store.repository().head().unwrap(), Some(original.commit));
}
