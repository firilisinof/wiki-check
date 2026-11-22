//! Checks over a whole snapshot of the wiki: each note alone, then the rules
//! that span notes.

use std::collections::BTreeMap;

use crate::notes::{self, PLANS_INDEX, ROOT_PATH, Scope};

/// One file of a snapshot, from the working tree or the Git index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: String,
    /// false for symlinks and submodules, whose bytes are not read
    pub regular: bool,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Problem {
    pub path: String,
    pub message: String,
}

const CONTENT_DIRECTORIES: &[&str] = &[
    "global/",
    "prompts/",
    "references/",
    "memory/",
    "projects/",
    "plans/",
    "notes/",
];

/// Files the wiki layout governs. Repository plumbing such as `.gitignore`
/// and `.githooks/` is left alone.
fn is_wiki_content(path: &str) -> bool {
    path.ends_with(".md") || CONTENT_DIRECTORIES.iter().any(|d| path.starts_with(d))
}

/// Validate a snapshot and return its problems sorted by path.
pub fn check(files: &[File]) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut report = |path: &str, message: String| {
        problems.push(Problem {
            path: path.to_string(),
            message,
        })
    };
    // support the old snapshot until all clients have switched in one commit
    let migrated = files.iter().any(|file| {
        ["projects/", "plans/", "notes/"]
            .iter()
            .any(|prefix| file.path.starts_with(prefix))
    });
    let mut has_index = false;
    let mut has_root = false;
    // project name to whether its README.md is present
    let mut projects: BTreeMap<String, bool> = BTreeMap::new();

    for file in files.iter().filter(|file| is_wiki_content(&file.path)) {
        let path = file.path.as_str();
        if migrated && path.starts_with("memory/") {
            report(path, "legacy memory/ path in a migrated vault".to_string());
            continue;
        }
        let scope = match notes::classify(path) {
            Ok(scope) => scope,
            Err(message) => {
                report(path, message);
                continue;
            }
        };
        if !file.regular {
            report(
                path,
                "must be a regular file, not a symlink or submodule".to_string(),
            );
            continue;
        }
        let Ok(markdown) = std::str::from_utf8(&file.bytes) else {
            report(path, "is not UTF-8 text".to_string());
            continue;
        };
        let (note, note_problems) = notes::parse(path, &scope, markdown);
        for message in note_problems {
            report(path, message);
        }
        if matches!(note.kind.as_str(), "plan" | "research") && !note.status_declared {
            report(
                path,
                "a plan or research note must state its status".to_string(),
            );
        }
        if note.scope == Scope::TaskInstructions && note.applies_to.is_empty() {
            report(path, "a task rule must declare applies_to".to_string());
        }
        match &note.scope {
            Scope::Root => has_root = true,
            Scope::PlanIndex => has_index = true,
            scope => {
                if let Some(project) = scope.project() {
                    *projects.entry(project.to_string()).or_default() |= note.is_project_overview;
                }
            }
        }
    }

    if !has_root {
        report(ROOT_PATH, "the wiki has no root instructions".to_string());
    }
    if migrated && !has_index {
        report(PLANS_INDEX, "missing plans index".to_string());
    }
    for (project, _) in projects.iter().filter(|(_, has_overview)| !**has_overview) {
        let path = if migrated {
            format!("projects/{project}/{project}.md")
        } else {
            format!("memory/{project}/README.md")
        };
        report(
            &path,
            "the project has no overview at its home path".to_string(),
        );
    }
    problems.sort();
    problems
}

/// Changed paths that only the user may change.
pub fn protected_changes(changed: &[String]) -> Vec<Problem> {
    changed
        .iter()
        .filter(|path| notes::is_protected(path))
        .map(|path| Problem {
            path: path.clone(),
            message: "is protected: change it only when the user asks, then commit with WIKI_ALLOW_PROTECTED=1".to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, text: &str) -> File {
        File {
            path: path.to_string(),
            regular: true,
            bytes: text.as_bytes().to_vec(),
        }
    }

    fn note(kind: &str, extra: &str) -> String {
        format!(
            "---\ncreated: 2026-11-21\nverified: null\ndescription: A note.\ntype: {kind}\ntags: []\n{extra}---\n\n# Note\n"
        )
    }

    fn valid_wiki() -> Vec<File> {
        vec![
            file("README.md", "# AI wiki\n"),
            file(".gitignore", "/target\n"),
            file(".githooks/pre-commit", "#!/bin/sh\n"),
            file(
                "global/coding.md",
                &note("rules", "applies_to:\n  - coding\n"),
            ),
            file("memory/pi/README.md", &note("overview", "")),
            file("memory/pi/plan.md", &note("plan", "status: proposed\n")),
            file("memory/idea.md", &note("finding", "")),
        ]
    }

    fn messages(problems: &[Problem]) -> Vec<(String, String)> {
        problems
            .iter()
            .map(|p| (p.path.clone(), p.message.clone()))
            .collect()
    }

    #[test]
    fn a_valid_wiki_has_no_problems() {
        assert_eq!(check(&valid_wiki()), Vec::new());
    }

    #[test]
    fn reports_rules_that_span_notes() {
        let mut files = valid_wiki();
        files.retain(|f| f.path != "README.md" && f.path != "memory/pi/README.md");
        files.push(file("global/web.md", &note("rules", "")));
        files.push(file("memory/idea-research.md", &note("research", "")));
        let found = messages(&check(&files));
        let paths: Vec<&str> = found.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "README.md",
                "global/web.md",
                "memory/idea-research.md",
                "memory/pi/README.md"
            ],
            "{found:?}"
        );
    }

    #[test]
    fn reports_files_outside_the_layout() {
        let mut files = valid_wiki();
        files.push(file("memory/pi/photo.png", ""));
        files.push(file("memory/pi/sub/a.md", &note("finding", "")));
        files.push(file("notes.md", &note("finding", "")));
        files.push(File {
            path: "memory/link.md".to_string(),
            regular: false,
            bytes: Vec::new(),
        });
        files.push(File {
            path: "memory/latin1.md".to_string(),
            regular: true,
            bytes: vec![0xe9],
        });
        let paths: Vec<String> = check(&files).into_iter().map(|p| p.path).collect();
        assert_eq!(
            paths,
            vec![
                "memory/latin1.md",
                "memory/link.md",
                "memory/pi/photo.png",
                "memory/pi/sub/a.md",
                "notes.md"
            ]
        );
    }

    #[test]
    fn migrated_vault_requires_a_plan_index_and_named_project_homes() {
        let mut files = vec![
            file("README.md", "# Wiki"),
            file("projects/tools/tools.md", &note("overview", "")),
            file("projects/tools/state.md", &note("state", "")),
            file("plans/index.md", "# Plans"),
            file("plans/tools/work.md", &note("plan", "status: proposed\n")),
            file("notes/idea.md", &note("finding", "")),
        ];
        assert!(check(&files).is_empty());
        files.retain(|f| f.path != PLANS_INDEX && f.path != "projects/tools/tools.md");
        files.push(file("memory/old.md", &note("finding", "")));
        let paths: Vec<_> = check(&files).into_iter().map(|p| p.path).collect();
        assert_eq!(
            paths,
            ["memory/old.md", PLANS_INDEX, "projects/tools/tools.md"]
        );
    }

    #[test]
    fn flags_only_protected_changes() {
        let changed = vec![
            "README.md".to_string(),
            "global/tasks.md".to_string(),
            "memory/pi/README.md".to_string(),
        ];
        let paths: Vec<String> = protected_changes(&changed)
            .into_iter()
            .map(|p| p.path)
            .collect();
        assert_eq!(paths, vec!["README.md", "global/tasks.md"]);
    }
}
