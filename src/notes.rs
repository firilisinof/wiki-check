//! Note paths, scopes, and frontmatter validation.
//!
//! A path is normalized and relative to the wiki root. Scope and project come
//! from that path, never from metadata.

use serde_yaml_ng::Value;

pub const ROOT_PATH: &str = "README.md";
pub const MAX_MEMORY_NOTE_BYTES: usize = 4096;
pub const PLANS_INDEX: &str = "plans/index.md";

const NOTE_KINDS: &[&str] = &[
    "overview",
    "rules",
    "configuration",
    "procedure",
    "finding",
    "research",
    "decisions",
    "plan",
    "reference",
    "state",
];
const NOTE_FIELDS: &[&str] = &[
    "created",
    "verified",
    "description",
    "type",
    "tags",
    "status",
    "things_project_id",
];
const REFERENCE_FIELDS: &[&str] = &["title", "url", "description", "added", "tags", "status"];
const STATUSES: &[&str] = &["current", "proposed", "superseded"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Root,
    TaskInstructions,
    Prompts,
    References,
    Standalone,
    PlanIndex,
    Plan,
    Project(String),
}

impl Scope {
    pub fn project(&self) -> Option<&str> {
        match self {
            Scope::Project(name) => Some(name),
            _ => None,
        }
    }
}

/// Whether a path is the root instructions or a task rule, which change only
/// when the user asks.
pub fn is_protected(path: &str) -> bool {
    path == ROOT_PATH || path.starts_with("global/")
}

fn valid_component(component: &str) -> bool {
    !component.is_empty()
        && !component.starts_with('.')
        && component
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Derive the scope of a path, rejecting anything outside the wiki layout.
///
/// `classify("memory/pi/deploy.md")` is `Scope::Project("pi")`. Hidden files,
/// traversal, nested project directories, and files outside the known
/// directories are errors.
pub fn classify(path: &str) -> Result<Scope, String> {
    let parts: Vec<&str> = path.split('/').collect();
    if !parts.iter().all(|part| valid_component(part)) {
        return Err(
            "not a normalized relative path of ASCII letters, digits, '.', '_' and '-'".to_string(),
        );
    }
    let file = parts[parts.len() - 1];
    if !file.ends_with(".md") || file == ".md" {
        return Err("not a Markdown file".to_string());
    }
    match parts.as_slice() {
        [ROOT_PATH] => Ok(Scope::Root),
        ["global", _] => Ok(Scope::TaskInstructions),
        ["prompts", _] => Ok(Scope::Prompts),
        ["references", _] => Ok(Scope::References),
        ["plans", "index.md"] => Ok(Scope::PlanIndex),
        ["plans", _] | ["plans", _, _] | ["plans", _, _, _] => Ok(Scope::Plan),
        ["projects", project, _] => Ok(Scope::Project(project.to_string())),
        ["notes", _] => Ok(Scope::Standalone),
        ["memory", _] => Ok(Scope::Standalone),
        ["memory", project, _] => Ok(Scope::Project(project.to_string())),
        _ => Err("outside the supported wiki layout".to_string()),
    }
}

/// What the cross-note checks need from a parsed note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub scope: Scope,
    pub kind: String,
    /// whether the frontmatter states the status instead of leaving the default
    pub status_declared: bool,
    pub applies_to: Vec<String>,
    pub is_project_overview: bool,
}

fn split_frontmatter(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---\n")?;
    if rest.starts_with("---\n") {
        return Some("");
    }
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.ends_with("\n---").then(|| rest.len() - 4))?;
    Some(&rest[..end + 1])
}

fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let number = |range: std::ops::Range<usize>| text[range].parse::<u32>().ok();
    match (number(0..4), number(5..7), number(8..10)) {
        (Some(_), Some(month), Some(day)) => (1..=12).contains(&month) && (1..=31).contains(&day),
        _ => false,
    }
}

fn is_label(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn has_url_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
}

struct Fields<'a> {
    map: &'a serde_yaml_ng::Mapping,
    problems: &'a mut Vec<String>,
}

impl Fields<'_> {
    fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(Value::String(key.to_string()))
    }

    fn required(&mut self, key: &str) -> Option<&Value> {
        if self.get(key).is_none() {
            self.problems
                .push(format!("frontmatter field {key} is missing"));
        }
        self.get(key)
    }

    fn text(&mut self, key: &str) -> String {
        match self.required(key) {
            Some(Value::String(text)) if !text.trim().is_empty() => text.clone(),
            Some(_) => {
                self.problems.push(format!("{key} must be nonempty text"));
                String::new()
            }
            None => String::new(),
        }
    }

    fn date(&mut self, key: &str, nullable: bool) {
        match self.required(key) {
            Some(Value::String(text)) if is_date(text) => {}
            Some(Value::Null) if nullable => {}
            Some(_) => {
                let or_null = if nullable { " or null" } else { "" };
                self.problems
                    .push(format!("{key} must be a YYYY-MM-DD date{or_null}"));
            }
            None => {}
        }
    }

    fn string_list(&mut self, key: &str, required: bool) -> Vec<String> {
        let value = if required {
            self.required(key)
        } else {
            self.get(key)
        };
        let Some(value) = value else {
            return Vec::new();
        };
        let items: Option<Vec<String>> = value.as_sequence().and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_string))
                .collect()
        });
        match items {
            Some(items) => {
                let mut seen = std::collections::HashSet::new();
                if !items.iter().all(|item| seen.insert(item.as_str())) {
                    self.problems.push(format!("{key} repeats a value"));
                }
                items
            }
            None => {
                self.problems
                    .push(format!("{key} must be a list of strings"));
                Vec::new()
            }
        }
    }

    fn description(&mut self) {
        let description = self.text("description");
        if !description.is_empty()
            && (description.contains('\n')
                || !description.ends_with('.')
                || description.contains(". "))
        {
            self.problems
                .push("description must be one sentence ending in a period".to_string());
        }
    }

    fn status(&mut self) -> bool {
        let Some(value) = self.get("status") else {
            return false;
        };
        if !value.as_str().is_some_and(|text| STATUSES.contains(&text)) {
            self.problems
                .push("status must be current, proposed, or superseded".to_string());
        }
        true
    }

    fn only(&mut self, allowed: &[&str]) {
        for key in self.map.keys() {
            match key.as_str() {
                Some(name) if allowed.contains(&name) => {}
                _ => self
                    .problems
                    .push(format!("frontmatter field {key:?} is not in the schema")),
            }
        }
    }
}

/// Parse a note and list every schema problem found in it alone.
pub fn parse(path: &str, scope: &Scope, markdown: &str) -> (Note, Vec<String>) {
    let mut problems = Vec::new();
    let mut note = Note {
        scope: scope.clone(),
        kind: String::new(),
        status_declared: false,
        applies_to: Vec::new(),
        is_project_overview: matches!(scope, Scope::Project(name) if path == format!("projects/{name}/{name}.md") || path == format!("memory/{name}/README.md")),
    };

    if matches!(scope, Scope::Root | Scope::PlanIndex) {
        if markdown.starts_with("---\n") {
            problems.push("entry pages must not have frontmatter".to_string());
        }
        return (note, problems);
    }

    let Some(frontmatter) = split_frontmatter(markdown) else {
        problems.push("expected YAML frontmatter enclosed by --- lines".to_string());
        return (note, problems);
    };
    let map = match serde_yaml_ng::from_str::<Value>(frontmatter) {
        Ok(Value::Mapping(map)) => map,
        Ok(_) => {
            problems.push("frontmatter must be a mapping of fields".to_string());
            return (note, problems);
        }
        Err(error) => {
            problems.push(format!("frontmatter is not valid YAML: {error}"));
            return (note, problems);
        }
    };

    let mut fields = Fields {
        map: &map,
        problems: &mut problems,
    };
    fields.description();
    fields.string_list("tags", true);
    note.status_declared = fields.status();
    if *scope == Scope::References {
        fields.only(REFERENCE_FIELDS);
        fields.text("title");
        let url = fields.text("url");
        if !url.is_empty() && !has_url_scheme(&url) {
            fields
                .problems
                .push("url must include a URL scheme".to_string());
        }
        fields.date("added", false);
        note.kind = "reference".to_string();
    } else {
        let legacy = matches!(scope, Scope::TaskInstructions | Scope::Prompts)
            || path.starts_with("memory/");
        if legacy {
            fields.only(&[NOTE_FIELDS, &["applies_to"]].concat());
        } else {
            fields.only(NOTE_FIELDS);
        }
        fields.date("created", false);
        fields.date("verified", true);
        note.kind = fields.text("type");
        if !note.kind.is_empty()
            && !NOTE_KINDS.contains(&note.kind.as_str())
            && !(legacy && note.kind == "prompt")
        {
            fields
                .problems
                .push(format!("type {:?} is not in the vocabulary", note.kind));
        }
        if legacy {
            note.applies_to = fields.string_list("applies_to", false);
            if !note.applies_to.iter().all(|label| is_label(label)) {
                fields.problems.push(
                    "applies_to labels use lowercase letters, digits, and hyphens".to_string(),
                );
            }
        }
        if let Some(value) = fields.get("things_project_id")
            && value.as_str().is_none_or(|text| text.trim().is_empty())
        {
            fields
                .problems
                .push("things_project_id must be nonempty text".to_string());
        }
    }

    if note.is_project_overview && note.kind != "overview" {
        problems.push("a project home must have type overview".to_string());
    }
    if *scope == Scope::Plan && note.kind != "plan" {
        problems.push("files under plans/ must have type plan".to_string());
    }
    if note.kind == "plan" && (path.starts_with("projects/") || path.starts_with("notes/")) {
        problems.push("move plan notes to plans/".to_string());
    }
    let limited = matches!(scope, Scope::Standalone | Scope::Project(_) | Scope::Plan);
    if limited && markdown.len() > MAX_MEMORY_NOTE_BYTES {
        problems.push(format!(
            "{} bytes exceeds the {MAX_MEMORY_NOTE_BYTES} byte limit for notes",
            markdown.len()
        ));
    }
    (note, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "---\ncreated: 2026-11-15\nverified: null\ndescription: A test note.\ntype: finding\ntags:\n  - nix\n---\n\n# Title here\n\nBody.\n";

    #[test]
    fn classifies_known_locations() {
        assert_eq!(classify("README.md"), Ok(Scope::Root));
        assert_eq!(classify("global/coding.md"), Ok(Scope::TaskInstructions));
        assert_eq!(classify("prompts/orchestrator.md"), Ok(Scope::Prompts));
        assert_eq!(classify("references/candle.md"), Ok(Scope::References));
        assert_eq!(classify("memory/idea.md"), Ok(Scope::Standalone));
        assert_eq!(
            classify("memory/pi/deploy.md"),
            Ok(Scope::Project("pi".to_string()))
        );
    }

    #[test]
    fn rejects_paths_outside_the_layout() {
        for path in [
            "",
            "/README.md",
            "memory/../README.md",
            "memory/./a.md",
            "memory//a.md",
            "notes.md",
            "memory/pi/sub/a.md",
            "memory/.hidden/a.md",
            "memory/a b.md",
            "memory\\a.md",
            "memory/pi/a.txt",
        ] {
            assert!(classify(path).is_err(), "{path:?} must be rejected");
        }
    }

    #[test]
    fn protects_the_root_and_task_rules_only() {
        assert!(is_protected("README.md"));
        assert!(is_protected("global/coding.md"));
        assert!(!is_protected("memory/README.md"));
        assert!(!is_protected("memory/global/a.md"));
    }

    #[test]
    fn parses_a_valid_note() {
        let scope = Scope::Project("pi".to_string());
        let (note, problems) = parse("memory/pi/a.md", &scope, NOTE);
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(note.kind, "finding");
        assert!(!note.status_declared);
        assert!(!note.is_project_overview);
    }

    #[test]
    fn reports_schema_problems() {
        let bad = NOTE
            .replace("type: finding", "type: diary")
            .replace("A test note.", "Two. Sentences.")
            .replace(
                "verified: null",
                "verified: yesterday\nstatus: old\nextra: 1",
            );
        let (_, problems) = parse("memory/a.md", &Scope::Standalone, &bad);
        let text = problems.join("\n");
        for expected in ["vocabulary", "one sentence", "verified", "status", "extra"] {
            assert!(text.contains(expected), "missing {expected} in {text}");
        }
    }

    #[test]
    fn enforces_the_memory_size_limit() {
        let big = format!("{NOTE}{}", "x".repeat(MAX_MEMORY_NOTE_BYTES));
        let (_, problems) = parse("memory/a.md", &Scope::Standalone, &big);
        assert!(problems.iter().any(|p| p.contains("byte limit")));
        let (_, problems) = parse("global/a.md", &Scope::TaskInstructions, &big);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn parses_a_reference() {
        let text = "---\ntitle: Candle\nurl: https://github.com/huggingface/candle\ndescription: A Rust ML framework.\nadded: 2026-11-15\ntags: []\n---\n";
        let (note, problems) = parse("references/candle.md", &Scope::References, text);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(note.kind, "reference");
    }

    #[test]
    fn project_overview_must_be_an_overview() {
        let scope = Scope::Project("pi".to_string());
        let (note, problems) = parse("memory/pi/README.md", &scope, NOTE);
        assert!(note.is_project_overview);
        assert!(problems.iter().any(|p| p.contains("type overview")));
    }

    #[test]
    fn missing_frontmatter_is_a_problem() {
        let (_, problems) = parse("memory/a.md", &Scope::Standalone, "# Only body\n");
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn root_instructions_have_no_frontmatter() {
        let (_, problems) = parse("README.md", &Scope::Root, NOTE);
        assert_eq!(problems.len(), 1);
        let (_, problems) = parse("README.md", &Scope::Root, "# AI wiki\n");
        assert!(problems.is_empty());
    }
}
