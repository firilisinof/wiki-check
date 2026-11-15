//! Operator commands. They run under the same process lock as the service and
//! write through the same validated commit path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::notes::{self, Scope};
use crate::store::{Change, Store, UpdateReceipt, UpdateRequest};

/// names the commit an exported checkout was taken from
const EXPORT_MARKER: &str = ".wiki-mcp-export";

fn wiki<T>(result: crate::error::Result<T>) -> Result<T> {
    result.map_err(|error| {
        let details: Vec<String> = error
            .details
            .iter()
            .map(|problem| {
                format!(
                    "\n  {}: {}",
                    problem.id.as_deref().unwrap_or("-"),
                    problem.message
                )
            })
            .collect();
        anyhow::anyhow!("{}{}", error.message, details.concat())
    })
}

/// What `check` found in the committed content.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub counts: BTreeMap<String, usize>,
    pub problems: BTreeMap<String, Vec<String>>,
    /// plans and research without a stated status, which need a decision before migration
    pub undeclared_status: Vec<String>,
    /// task instructions without an applicability label
    pub unlabeled_rules: Vec<String>,
}

/// Validate every committed note and list what a metadata migration must review.
pub fn check(store: &mut Store) -> Result<Report> {
    let mut report = Report {
        problems: wiki(store.problems())?,
        ..Report::default()
    };
    for document in wiki(store.documents())? {
        let scope = notes::classify(&document.id).map_err(anyhow::Error::msg)?;
        *report.counts.entry(scope.name().to_string()).or_default() += 1;
        let (note, _) = notes::parse(&document.id, &scope, &document.markdown);
        if matches!(note.kind.as_str(), "plan" | "research") && !note.status_declared {
            report.undeclared_status.push(document.id.clone());
        }
        if scope == Scope::TaskInstructions && note.applies_to.is_empty() {
            report.unlabeled_rules.push(document.id);
        }
    }
    Ok(report)
}

impl Report {
    pub fn print(&self) {
        println!("Records by scope:");
        for (scope, count) in &self.counts {
            println!("  {scope}: {count}");
        }
        let section = |title: &str, ids: &[String]| {
            if !ids.is_empty() {
                println!("\n{title} ({}):", ids.len());
                for id in ids {
                    println!("  {id}");
                }
            }
        };
        section(
            "Plans and research without a status, to classify as current or proposed",
            &self.undeclared_status,
        );
        section(
            "Task instructions without applies_to",
            &self.unlabeled_rules,
        );
        if self.problems.is_empty() {
            println!("\nNo schema problems.");
        } else {
            println!("\nSchema problems ({} records):", self.problems.len());
            for (id, problems) in &self.problems {
                for problem in problems {
                    println!("  {id}: {problem}");
                }
            }
        }
    }
}

/// Write the committed files into an empty directory for manual editing.
pub fn export(store: &mut Store, destination: &Path) -> Result<String> {
    let head = wiki(store.sync())?.context("the repository has no commit to export")?;
    store.repository().export(&head, destination)?;
    std::fs::write(destination.join(EXPORT_MARKER), format!("{head}\n"))?;
    Ok(head)
}

fn local_notes(root: &Path, directory: &Path, found: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            local_notes(root, &path, found)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(root)?
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if notes::classify(&relative).is_ok() {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("{} must be UTF-8 text", path.display()))?;
                found.insert(relative, text);
            }
        }
    }
    Ok(())
}

/// The changes an edited export makes relative to the commit it came from.
///
/// A record that also changed in the repository since the export is a
/// conflict. Nothing is merged.
pub fn plan_import(store: &mut Store, directory: &Path) -> Result<Vec<Change>> {
    let marker = directory.join(EXPORT_MARKER);
    let base = std::fs::read_to_string(&marker)
        .with_context(|| format!("{} is missing, so this is not an export", marker.display()))?
        .trim()
        .to_string();
    let head = wiki(store.sync())?.context("the repository has no commit")?;
    let repository = store.repository().clone();
    if !repository.is_ancestor(&base, &head)? {
        bail!("export base {base} is not part of the current history");
    }

    let base_files: Vec<_> = repository
        .tree(&base)?
        .into_iter()
        .filter(|file| notes::classify(&file.path).is_ok())
        .collect();
    let blobs: Vec<String> = base_files.iter().map(|file| file.blob.clone()).collect();
    let mut before = BTreeMap::new();
    for (file, content) in base_files.iter().zip(repository.blobs(&blobs)?) {
        before.insert(
            file.path.clone(),
            String::from_utf8_lossy(&content).to_string(),
        );
    }
    let mut after = BTreeMap::new();
    local_notes(directory, directory, &mut after)?;

    let ids: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let edited: Vec<&String> = ids
        .into_iter()
        .filter(|id| before.get(*id) != after.get(*id))
        .collect();
    let moved: BTreeSet<String> = repository
        .changed_paths(&base, &head)?
        .into_iter()
        .collect();
    let conflicts: Vec<&str> = edited
        .iter()
        .filter(|id| moved.contains(**id))
        .map(|id| id.as_str())
        .collect();
    if !conflicts.is_empty() {
        bail!(
            "these records changed in the repository after the export, so export again and reapply the edits: {}",
            conflicts.join(", ")
        );
    }
    let owned: Vec<String> = edited.iter().map(|id| id.to_string()).collect();
    let revisions = wiki(store.revisions(&owned))?;
    let revision = |id: &String| {
        revisions
            .get(id)
            .cloned()
            .with_context(|| format!("{id} has no current revision"))
    };
    edited
        .into_iter()
        .map(|id| {
            Ok(match (before.contains_key(id), after.get(id)) {
                (false, Some(markdown)) => Change::Create {
                    id: id.clone(),
                    markdown: markdown.clone(),
                },
                (true, Some(markdown)) => Change::Update {
                    id: id.clone(),
                    expected_revision: revision(id)?,
                    markdown: markdown.clone(),
                },
                (true, None) => Change::Delete {
                    id: id.clone(),
                    expected_revision: revision(id)?,
                },
                (false, None) => unreachable!("an edited record exists on one side"),
            })
        })
        .collect()
}

/// Commit the edits of an export as one revision-checked update.
pub fn import(
    store: &mut Store,
    directory: &Path,
    allow_protected: bool,
) -> Result<Option<UpdateReceipt>> {
    let changes = plan_import(store, directory)?;
    if changes.is_empty() {
        return Ok(None);
    }
    let payload = notes::sha256_hex(&serde_json::to_vec(&changes)?);
    let request = UpdateRequest {
        request_id: format!("import-{}", &payload[..32]),
        changes,
    };
    wiki(store.update(&request, allow_protected)).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testing;
    use crate::store::StoreConfig;

    fn note(kind: &str, description: &str) -> String {
        format!(
            "---\ncreated: 2026-11-15\nverified: null\ndescription: {description}\ntype: {kind}\ntags: []\n---\n\n# Note\n"
        )
    }

    fn seeded() -> (tempfile::TempDir, tempfile::TempDir, Store) {
        let (repository, _) = testing::bare();
        let state = tempfile::tempdir().unwrap();
        let mut store = Store::open(&StoreConfig {
            git_dir: repository.path().to_path_buf(),
            state_dir: state.path().to_path_buf(),
            identity: testing::identity(),
            model: None,
        })
        .unwrap();
        let create = |id: &str, markdown: String| Change::Create {
            id: id.to_string(),
            markdown,
        };
        let request = UpdateRequest {
            request_id: "seed".to_string(),
            changes: vec![
                create("README.md", "# Wiki\n".to_string()),
                create("global/coding.md", note("rules", "Coding rules.")),
                create("memory/idea.md", note("plan", "An idea.")),
                create("memory/old.md", note("finding", "An old finding.")),
            ],
        };
        store.update(&request, true).unwrap();
        (repository, state, store)
    }

    #[test]
    fn check_reports_what_a_migration_must_review() {
        let (_repository, _state, mut store) = seeded();
        let report = check(&mut store).unwrap();
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert_eq!(report.counts["standalone"], 2);
        assert_eq!(report.undeclared_status, vec!["memory/idea.md"]);
        assert_eq!(report.unlabeled_rules, vec!["global/coding.md"]);
    }

    #[test]
    fn an_edited_export_is_imported_as_one_checked_update() {
        let (_repository, _state, mut store) = seeded();
        let out = tempfile::tempdir().unwrap();
        let base = export(&mut store, out.path()).unwrap();
        assert_eq!(import(&mut store, out.path(), false).unwrap(), None);

        std::fs::write(
            out.path().join("memory/idea.md"),
            note("plan", "A better idea."),
        )
        .unwrap();
        std::fs::write(
            out.path().join("memory/new.md"),
            note("finding", "A new finding."),
        )
        .unwrap();
        std::fs::remove_file(out.path().join("memory/old.md")).unwrap();
        std::fs::write(out.path().join("notes.txt"), "ignored").unwrap();

        let receipt = import(&mut store, out.path(), false).unwrap().unwrap();
        let operations: Vec<_> = receipt
            .changes
            .iter()
            .map(|change| (change.id.as_str(), change.operation.as_str()))
            .collect();
        assert_eq!(
            operations,
            vec![
                ("memory/idea.md", "update"),
                ("memory/new.md", "create"),
                ("memory/old.md", "delete"),
            ]
        );
        assert_ne!(receipt.commit, base);

        // the export is now behind for the records it edited
        std::fs::write(
            out.path().join("memory/idea.md"),
            note("plan", "A third idea."),
        )
        .unwrap();
        let error = import(&mut store, out.path(), false).unwrap_err();
        assert!(error.to_string().contains("memory/idea.md"), "{error}");
    }

    #[test]
    fn protected_instructions_need_the_explicit_flag() {
        let (_repository, _state, mut store) = seeded();
        let out = tempfile::tempdir().unwrap();
        export(&mut store, out.path()).unwrap();
        std::fs::write(out.path().join("README.md"), "# Wiki\n\nNew rule.\n").unwrap();
        assert!(import(&mut store, out.path(), false).is_err());
        let receipt = import(&mut store, out.path(), true).unwrap().unwrap();
        assert_eq!(store.instructions().unwrap().revision, receipt.commit);
    }
}
