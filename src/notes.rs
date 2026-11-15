//! Note identifiers, scopes, and frontmatter validation.
//!
//! An identifier is a normalized path relative to the wiki root. Scope and
//! project come from that path, never from metadata.

use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;
use sha2::{Digest, Sha256};

pub const ROOT_ID: &str = "README.md";
pub const MAX_MEMORY_NOTE_BYTES: usize = 4096;
const GENERATED_INDEX: &str = "index.md";
const PROJECT_OVERVIEW: &str = "README.md";

const NOTE_KINDS: &[&str] = &[
    "overview",
    "rules",
    "prompt",
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
    "applies_to",
    "things_project_id",
];
const REFERENCE_FIELDS: &[&str] = &["title", "url", "description", "added", "tags", "status"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Root,
    TaskInstructions,
    Prompts,
    References,
    Standalone,
    Project(String),
}

impl Scope {
    pub fn name(&self) -> &'static str {
        match self {
            Scope::Root => "root",
            Scope::TaskInstructions => "task_instructions",
            Scope::Prompts => "prompts",
            Scope::References => "references",
            Scope::Standalone => "standalone",
            Scope::Project(_) => "project",
        }
    }

    pub fn project(&self) -> Option<&str> {
        match self {
            Scope::Project(name) => Some(name),
            _ => None,
        }
    }

    /// Root and task instructions change only through explicit maintenance.
    pub fn is_protected(&self) -> bool {
        matches!(self, Scope::Root | Scope::TaskInstructions)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Current,
    Proposed,
    Superseded,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Current => "current",
            Status::Proposed => "proposed",
            Status::Superseded => "superseded",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "current" => Some(Status::Current),
            "proposed" => Some(Status::Proposed),
            "superseded" => Some(Status::Superseded),
            _ => None,
        }
    }
}

fn valid_component(component: &str) -> bool {
    !component.is_empty()
        && !component.starts_with('.')
        && component
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Derive the scope of an identifier, rejecting anything the service does not serve.
///
/// `classify("memory/pi/deploy.md")` is `Scope::Project("pi")`. Generated
/// indexes, hidden files, traversal, and files outside the known directories
/// are errors.
pub fn classify(id: &str) -> std::result::Result<Scope, String> {
    let parts: Vec<&str> = id.split('/').collect();
    if !parts.iter().all(|part| valid_component(part)) {
        return Err(format!(
            "{id:?} is not a normalized relative path of ASCII letters, digits, '.', '_' and '-'"
        ));
    }
    let file = parts[parts.len() - 1];
    if !file.ends_with(".md") || file == ".md" {
        return Err(format!("{id:?} is not a Markdown file"));
    }
    if file == GENERATED_INDEX {
        return Err(format!("{id:?} is a generated index"));
    }
    match parts.as_slice() {
        [ROOT_ID] => Ok(Scope::Root),
        ["global", _] => Ok(Scope::TaskInstructions),
        ["prompts", _] => Ok(Scope::Prompts),
        ["references", _] => Ok(Scope::References),
        ["memory", _] => Ok(Scope::Standalone),
        ["memory", project, _] => Ok(Scope::Project(project.to_string())),
        _ => Err(format!(
            "{id:?} is outside README.md, global/, prompts/, references/, memory/ and memory/<project>/"
        )),
    }
}

/// A note as the index and the tools see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: String,
    pub scope: Scope,
    pub kind: String,
    pub status: Status,
    /// whether the frontmatter states the status instead of leaving the default
    pub status_declared: bool,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub applies_to: Vec<String>,
    pub body: String,
}

impl Note {
    pub fn is_project_overview(&self) -> bool {
        matches!(self.scope, Scope::Project(_))
            && self.id.ends_with(&format!("/{PROJECT_OVERVIEW}"))
    }

    /// The text embedded for semantic search. Bodies are covered by keyword search.
    pub fn embedding_input(&self) -> String {
        let mut text = self.title.clone();
        if !self.description.is_empty() {
            text.push('\n');
            text.push_str(&self.description);
        }
        if !self.tags.is_empty() {
            text.push('\n');
            text.push_str(&self.tags.join(", "));
        }
        text
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    if let Some(body) = rest.strip_prefix("---\n") {
        return Some(("", body));
    }
    let end = rest.find("\n---\n").map(|at| (at, at + 5)).or_else(|| {
        rest.ends_with("\n---")
            .then(|| (rest.len() - 4, rest.len()))
    })?;
    Some((&rest[..end.0 + 1], &rest[end.1..]))
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

fn first_heading(body: &str) -> Option<String> {
    let mut in_fence = false;
    for line in body.lines() {
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && let Some(heading) = line.strip_prefix("# ") {
            return Some(heading.trim().to_string());
        }
    }
    None
}

fn file_stem(id: &str) -> String {
    let file = id.rsplit('/').next().unwrap_or(id);
    file.strip_suffix(".md").unwrap_or(file).to_string()
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

    fn description(&mut self) -> String {
        let description = self.text("description");
        if !description.is_empty()
            && (description.contains('\n')
                || !description.ends_with('.')
                || description.contains(". "))
        {
            self.problems
                .push("description must be one sentence ending in a period".to_string());
        }
        description
    }

    fn status(&mut self) -> Status {
        match self.get("status") {
            None => Status::Current,
            Some(value) => value.as_str().and_then(Status::parse).unwrap_or_else(|| {
                self.problems
                    .push("status must be current, proposed, or superseded".to_string());
                Status::Current
            }),
        }
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

/// Parse a note and list every schema problem found.
///
/// A write is accepted only when the list is empty. Indexing existing content
/// keeps the note readable and reports the problems instead.
pub fn parse(id: &str, scope: &Scope, markdown: &str) -> (Note, Vec<String>) {
    let mut problems = Vec::new();
    let mut note = Note {
        id: id.to_string(),
        scope: scope.clone(),
        kind: String::new(),
        status: Status::Current,
        status_declared: false,
        title: String::new(),
        description: String::new(),
        tags: Vec::new(),
        applies_to: Vec::new(),
        body: markdown.to_string(),
    };

    if *scope == Scope::Root {
        if markdown.starts_with("---\n") {
            problems.push("root instructions must not have frontmatter".to_string());
        }
        note.kind = "instructions".to_string();
        note.title = first_heading(markdown).unwrap_or_else(|| file_stem(id));
        note.description = "Root instructions that every chat loads in full.".to_string();
        return (note, problems);
    }

    let Some((frontmatter, body)) = split_frontmatter(markdown) else {
        problems.push("expected YAML frontmatter enclosed by --- lines".to_string());
        note.title = first_heading(markdown).unwrap_or_else(|| file_stem(id));
        return (note, problems);
    };
    note.body = body.to_string();
    let empty = serde_yaml_ng::Mapping::new();
    let parsed: std::result::Result<Value, _> = serde_yaml_ng::from_str(frontmatter);
    let map = match &parsed {
        Ok(Value::Mapping(map)) => map,
        Ok(_) => {
            problems.push("frontmatter must be a mapping of fields".to_string());
            &empty
        }
        Err(error) => {
            problems.push(format!("frontmatter is not valid YAML: {error}"));
            &empty
        }
    };
    if parsed.is_err() || !matches!(parsed, Ok(Value::Mapping(_))) {
        note.title = first_heading(body).unwrap_or_else(|| file_stem(id));
        return (note, problems);
    }

    let mut fields = Fields {
        map,
        problems: &mut problems,
    };
    note.description = fields.description();
    note.tags = fields.string_list("tags", true);
    note.status = fields.status();
    note.status_declared = fields.get("status").is_some();
    if *scope == Scope::References {
        fields.only(REFERENCE_FIELDS);
        let title = fields.text("title");
        let url = fields.text("url");
        if !url.is_empty() && !has_url_scheme(&url) {
            fields
                .problems
                .push("url must include a URL scheme".to_string());
        }
        fields.date("added", false);
        note.kind = "reference".to_string();
        note.title = title;
    } else {
        fields.only(NOTE_FIELDS);
        fields.date("created", false);
        fields.date("verified", true);
        note.kind = fields.text("type");
        if !note.kind.is_empty() && !NOTE_KINDS.contains(&note.kind.as_str()) {
            fields
                .problems
                .push(format!("type {:?} is not in the vocabulary", note.kind));
        }
        note.applies_to = fields.string_list("applies_to", false);
        if !note.applies_to.iter().all(|label| is_label(label)) {
            fields
                .problems
                .push("applies_to labels use lowercase letters, digits, and hyphens".to_string());
        }
        if let Some(value) = fields.get("things_project_id")
            && value.as_str().is_none_or(|text| text.trim().is_empty())
        {
            fields
                .problems
                .push("things_project_id must be nonempty text".to_string());
        }
        note.title = first_heading(body).unwrap_or_else(|| file_stem(id));
    }
    if note.title.is_empty() {
        note.title = file_stem(id);
    }

    if note.is_project_overview() && note.kind != "overview" {
        problems.push("a project README.md must have type overview".to_string());
    }
    let limited = matches!(scope, Scope::Standalone | Scope::Project(_));
    if limited && markdown.len() > MAX_MEMORY_NOTE_BYTES {
        problems.push(format!(
            "{} bytes exceeds the {MAX_MEMORY_NOTE_BYTES} byte limit for notes under memory/",
            markdown.len()
        ));
    }
    (note, problems)
}

fn has_url_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
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
    fn rejects_unserved_paths() {
        for id in [
            "",
            "/README.md",
            "memory/../README.md",
            "memory/./a.md",
            "memory//a.md",
            "memory/pi/index.md",
            "index.md",
            "tools/index.py",
            "notes.md",
            "memory/pi/sub/a.md",
            "memory/.hidden/a.md",
            "memory/a b.md",
            "memory\\a.md",
            "memory/pi/a.txt",
            ".git/config",
        ] {
            assert!(classify(id).is_err(), "{id:?} must be rejected");
        }
    }

    #[test]
    fn parses_a_valid_note() {
        let scope = Scope::Project("pi".to_string());
        let (note, problems) = parse("memory/pi/a.md", &scope, NOTE);
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(note.kind, "finding");
        assert_eq!(note.status, Status::Current);
        assert_eq!(note.title, "Title here");
        assert_eq!(note.tags, vec!["nix"]);
        assert_eq!(note.embedding_input(), "Title here\nA test note.\nnix");
    }

    #[test]
    fn reports_schema_problems() {
        let scope = Scope::Standalone;
        let bad = NOTE
            .replace("type: finding", "type: diary")
            .replace("A test note.", "Two. Sentences.")
            .replace(
                "verified: null",
                "verified: yesterday\nstatus: old\nextra: 1",
            );
        let (_, problems) = parse("memory/a.md", &scope, &bad);
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
        assert_eq!(
            (note.kind.as_str(), note.title.as_str()),
            ("reference", "Candle")
        );
    }

    #[test]
    fn project_overview_must_be_an_overview() {
        let scope = Scope::Project("pi".to_string());
        let (_, problems) = parse("memory/pi/README.md", &scope, NOTE);
        assert!(problems.iter().any(|p| p.contains("type overview")));
    }

    #[test]
    fn missing_frontmatter_is_a_problem() {
        let (note, problems) = parse("memory/a.md", &Scope::Standalone, "# Only body\n");
        assert_eq!(note.title, "Only body");
        assert_eq!(problems.len(), 1);
    }
}
