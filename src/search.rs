//! Search requests, ranking, and cursors. The store supplies the candidates.

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::error::{Code, Result, WikiError};
use crate::notes::{Status, sha256_hex};

pub const DEFAULT_LIMIT: u32 = 10;
pub const MAX_LIMIT: u32 = 50;
/// reciprocal rank fusion constant, equal for both rankings
const FUSION_CONSTANT: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    /// project overviews, to discover which projects exist
    Projects,
    /// notes of one project, named by `project`
    Project,
    /// notes directly under memory/, outside any project
    Standalone,
    /// task rules under global/
    TaskInstructions,
    /// role prompts, only when the user asks for a behavior
    Prompts,
    /// saved references, only when the user asks for them
    References,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Keyword,
    Semantic,
    Hybrid,
}

impl Mode {
    pub fn uses_semantic(self) -> bool {
        matches!(self, Mode::Semantic | Mode::Hybrid)
    }

    pub fn uses_keyword(self) -> bool {
        matches!(self, Mode::Keyword | Mode::Hybrid)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    /// Where to search. The service never broadens it.
    pub scope: SearchScope,
    /// Project identifier, required when scope is `project`.
    #[serde(default)]
    pub project: Option<String>,
    /// English query text. Omit it to enumerate every record matching the filters, unranked.
    #[serde(default)]
    pub query: Option<String>,
    /// Retrieval method for a text query. Defaults to `hybrid`.
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Note type, such as `rules`, `decisions`, or `state`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Tags that a record must all carry.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Task applicability label, such as `coding`.
    #[serde(default)]
    pub applies_to: Option<String>,
    /// Statuses to include. Defaults to current and proposed, which excludes superseded history.
    #[serde(default)]
    pub status: Vec<Status>,
    /// Records per page, default 10 and at most 50.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Opaque cursor from `next_cursor` of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

impl SearchRequest {
    pub fn text(&self) -> Option<&str> {
        self.query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
    }

    /// The retrieval method this request runs, or `None` for plain enumeration.
    pub fn effective_mode(&self) -> Option<Mode> {
        self.text().map(|_| self.mode.unwrap_or(Mode::Hybrid))
    }

    pub fn statuses(&self) -> Vec<Status> {
        if self.status.is_empty() {
            vec![Status::Current, Status::Proposed]
        } else {
            self.status.clone()
        }
    }

    pub fn page_size(&self) -> Result<usize> {
        match self.limit.unwrap_or(DEFAULT_LIMIT) {
            0 => Err(WikiError::invalid("limit must be at least 1")),
            limit if limit > MAX_LIMIT => Err(WikiError::invalid(format!(
                "limit {limit} exceeds the maximum of {MAX_LIMIT}"
            ))),
            limit => Ok(limit as usize),
        }
    }

    /// Identifies the query and filters a cursor belongs to, ignoring paging.
    fn fingerprint(&self) -> String {
        let mut unpaged = self.clone();
        unpaged.cursor = None;
        unpaged.limit = None;
        unpaged.mode = self.effective_mode();
        sha256_hex(&serde_json::to_vec(&unpaged).expect("request serializes"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchRecord {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub kind: String,
    pub status: Status,
    pub title: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    pub revision: String,
}

/// How much of the eligible records the semantic index covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SemanticCoverage {
    pub eligible: usize,
    pub indexed: usize,
    pub pending: usize,
    /// records whose embedding input exceeds the model limit and has no vector
    pub overlength: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResponse {
    pub records: Vec<SearchRecord>,
    /// `enumeration`, `keyword`, `semantic`, or `hybrid`
    pub mode_used: &'static str,
    /// number of records matching the filters and query across all pages
    pub total: usize,
    pub next_cursor: Option<String>,
    /// whether the retrieval methods covered every eligible record, not whether
    /// everything relevant was found
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_coverage: Option<SemanticCoverage>,
}

#[derive(Serialize, Deserialize)]
struct CursorData {
    generation: String,
    fingerprint: String,
    offset: usize,
}

pub fn encode_cursor(request: &SearchRequest, generation: &str, offset: usize) -> String {
    let data = CursorData {
        generation: generation.to_string(),
        fingerprint: request.fingerprint(),
        offset,
    };
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&data).expect("cursor serializes"))
}

/// The offset a cursor points at. A cursor from another index generation expires.
pub fn decode_cursor(request: &SearchRequest, generation: &str) -> Result<usize> {
    let Some(cursor) = &request.cursor else {
        return Ok(0);
    };
    let data: CursorData = URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| WikiError::invalid("cursor is not a value returned by search_memory"))?;
    if data.fingerprint != request.fingerprint() {
        return Err(WikiError::invalid(
            "cursor belongs to a different query or filters",
        ));
    }
    if data.generation != generation {
        return Err(WikiError::new(
            Code::CursorExpired,
            "the index changed since this cursor was issued, so restart the search without a cursor",
        ));
    }
    Ok(data.offset)
}

/// Turn plain query text into an FTS5 expression of quoted terms and phrases.
///
/// `fts_query("core.fsync \"write contract\"")` is
/// `"core.fsync" OR "write contract"`. The text is never interpreted as FTS5
/// syntax. `None` means the query has nothing searchable.
pub fn fts_query(text: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        rest = rest.trim_start();
        let (term, after) = match rest.strip_prefix('"') {
            Some(quoted) => match quoted.find('"') {
                Some(end) => (&quoted[..end], &quoted[end + 1..]),
                None => (quoted, ""),
            },
            None => {
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == '"')
                    .unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
        };
        if term.chars().any(char::is_alphanumeric) {
            terms.push(format!("\"{}\"", term.replace('"', " ")));
        }
        rest = after;
    }
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

pub fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

pub fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

/// Cosine similarity of two normalized vectors.
pub fn similarity(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

/// Order candidates by similarity, best first, with identifiers breaking ties.
pub fn rank_by_similarity(mut scored: Vec<(String, f32)>) -> Vec<String> {
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().map(|(id, _)| id).collect()
}

/// Reciprocal rank fusion of rankings, best first, with identifiers breaking ties.
pub fn fuse(rankings: &[&[String]]) -> Vec<String> {
    let mut scores: HashMap<&str, f64> = HashMap::new();
    for ranking in rankings {
        for (position, id) in ranking.iter().enumerate() {
            *scores.entry(id).or_default() += 1.0 / (FUSION_CONSTANT + position as f64 + 1.0);
        }
    }
    let mut fused: Vec<(&str, f64)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    fused.into_iter().map(|(id, _)| id.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(query: Option<&str>) -> SearchRequest {
        SearchRequest {
            scope: SearchScope::Standalone,
            project: None,
            query: query.map(str::to_string),
            mode: None,
            kind: None,
            tags: Vec::new(),
            applies_to: None,
            status: Vec::new(),
            limit: None,
            cursor: None,
        }
    }

    #[test]
    fn query_text_is_quoted_not_interpreted() {
        assert_eq!(
            fts_query("core.fsync \"write contract\" NOT").unwrap(),
            "\"core.fsync\" OR \"write contract\" OR \"NOT\""
        );
        assert_eq!(fts_query("update_memory*").unwrap(), "\"update_memory*\"");
        assert_eq!(fts_query("a\"b").unwrap(), "\"a\" OR \"b\"");
        assert_eq!(fts_query("  -- () * "), None);
        assert_eq!(
            fts_query("\"unterminated phrase").unwrap(),
            "\"unterminated phrase\""
        );
    }

    #[test]
    fn hybrid_is_the_default_for_text() {
        assert_eq!(request(Some("x")).effective_mode(), Some(Mode::Hybrid));
        assert_eq!(request(Some("  ")).effective_mode(), None);
        assert_eq!(request(None).effective_mode(), None);
    }

    #[test]
    fn limits_are_bounded() {
        let mut r = request(None);
        assert_eq!(r.page_size().unwrap(), 10);
        r.limit = Some(51);
        assert_eq!(r.page_size().unwrap_err().code, Code::InvalidInput);
        r.limit = Some(0);
        assert!(r.page_size().is_err());
    }

    #[test]
    fn cursors_bind_query_and_generation() {
        let mut r = request(Some("nix"));
        r.cursor = Some(encode_cursor(&r, "gen-1", 10));
        assert_eq!(decode_cursor(&r, "gen-1").unwrap(), 10);
        assert_eq!(
            decode_cursor(&r, "gen-2").unwrap_err().code,
            Code::CursorExpired
        );
        let mut other = request(Some("rust"));
        other.cursor = r.cursor.clone();
        assert_eq!(
            decode_cursor(&other, "gen-1").unwrap_err().code,
            Code::InvalidInput
        );
        other.cursor = Some("garbage".to_string());
        assert_eq!(
            decode_cursor(&other, "gen-1").unwrap_err().code,
            Code::InvalidInput
        );
    }

    #[test]
    fn fusion_rewards_agreement_and_breaks_ties_by_id() {
        let keyword = vec!["b".to_string(), "a".to_string()];
        let semantic = vec!["c".to_string(), "a".to_string()];
        assert_eq!(fuse(&[&keyword, &semantic]), vec!["a", "b", "c"]);
    }

    #[test]
    fn vectors_round_trip() {
        let vector = vec![0.5_f32, -1.25, 3.0];
        assert_eq!(decode_vector(&encode_vector(&vector)), vector);
        assert_eq!(
            rank_by_similarity(vec![
                ("b".into(), 0.5),
                ("a".into(), 0.5),
                ("c".into(), 0.9)
            ]),
            vec!["c", "a", "b"]
        );
    }
}
