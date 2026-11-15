//! MCP adapter: four tools over the wiki operations.

use std::time::Instant;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::error::{Code, WikiError};
use crate::search::SearchRequest;
use crate::service::Wiki;
use crate::store::{Change, UpdateRequest};

const SERVER_INSTRUCTIONS: &str = "Personal Markdown wiki: instructions, project memory, and saved references. \
At the start of every chat call load_instructions and follow what it returns, before any other wiki tool. \
Then search_memory within one scope, read_memory for full notes, and update_memory for revision-checked changes. \
A failure is never a reason to continue silently: report it to the user and let them decide.";

const LOAD_INSTRUCTIONS: &str = "Return the complete root instructions of the wiki and their revision. \
Call this first in every chat, before any other wiki tool, and follow the instructions. \
The text is never summarized or truncated.";

const SEARCH_MEMORY: &str = "Search or enumerate records in exactly one scope. \
Scopes: projects (project overviews, to find a project), project (one project's notes, with `project`), \
standalone, task_instructions, prompts and references (those two only when the user asks). \
With `query` (English, keep identifiers literal) results are ranked by `mode`: hybrid (default), keyword, or semantic. \
Without `query` every record matching the filters is listed unranked, and `next_cursor` must be followed to the end \
when enumerating instructions. Results hold descriptions and short excerpts. Use read_memory for full text. \
`complete: false` means the semantic index did not cover every eligible record. \
A semantic failure is an error, never a silent keyword fallback: ask the user how to proceed.";

const READ_MEMORY: &str = "Return the full Markdown, metadata, and current revision of 1 to 10 records \
by identifier, from one committed view. Identifiers that do not exist are listed in `missing`.";

const UPDATE_MEMORY: &str = "Apply a batch of create, update, and delete changes atomically: all or none. \
`create` needs `id` (destination path) and complete `markdown` with frontmatter. \
`update` needs `id`, `expected_revision` from the last read, and complete replacement `markdown`. \
`delete` needs `id` and `expected_revision`. \
On stale_revision read the record again and reconcile. Root and global instructions cannot be changed here. \
Choose a fresh `request_id` per logical update. If the outcome is uncertain, retry the same `request_id` \
and payload to recover it, and never assume a rollback.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadArgs {
    /// 1 to 10 record identifiers, such as `memory/<project>/decisions.md`
    ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ChangeArgs {
    operation: Operation,
    /// record identifier: the destination path for create, the existing record otherwise
    id: String,
    /// revision the edit is based on, required for update and delete
    #[serde(default)]
    expected_revision: Option<String>,
    /// complete Markdown including frontmatter, required for create and update
    #[serde(default)]
    markdown: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UpdateArgs {
    /// identifier of this logical update, 1 to 128 characters from letters, digits, `.`, `_`, `:` and `-`
    request_id: String,
    /// changes applied together or not at all
    changes: Vec<ChangeArgs>,
}

fn change(args: ChangeArgs) -> Result<Change, WikiError> {
    let missing = |field: &str| {
        WikiError::invalid(format!(
            "{:?} of {} requires {field}",
            args.operation, args.id
        ))
    };
    let unexpected = |field: &str| {
        WikiError::invalid(format!(
            "{:?} of {} does not take {field}",
            args.operation, args.id
        ))
    };
    Ok(
        match (args.operation, &args.expected_revision, &args.markdown) {
            (Operation::Create, None, Some(markdown)) => Change::Create {
                id: args.id.clone(),
                markdown: markdown.clone(),
            },
            (Operation::Create, Some(_), _) => return Err(unexpected("expected_revision")),
            (Operation::Update, Some(revision), Some(markdown)) => Change::Update {
                id: args.id.clone(),
                expected_revision: revision.clone(),
                markdown: markdown.clone(),
            },
            (Operation::Delete, Some(revision), None) => Change::Delete {
                id: args.id.clone(),
                expected_revision: revision.clone(),
            },
            (Operation::Delete, _, Some(_)) => return Err(unexpected("markdown")),
            (Operation::Update | Operation::Delete, None, _) => {
                return Err(missing("expected_revision"));
            }
            (Operation::Create | Operation::Update, _, None) => return Err(missing("markdown")),
        },
    )
}

fn respond<T: Serialize>(
    operation: &'static str,
    started: Instant,
    result: Result<T, WikiError>,
) -> Result<CallToolResult, ErrorData> {
    let ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(value) => {
            tracing::info!(operation, outcome = "ok", ms, "tool call");
            let value = serde_json::to_value(value)
                .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
            Ok(CallToolResult::structured(value))
        }
        Err(error) => {
            let unavailable = matches!(
                error.code,
                Code::Unavailable | Code::OutcomeUncertain | Code::SemanticUnavailable
            );
            if unavailable {
                tracing::error!(operation, outcome = ?error.code, ms, error = %error.message, "tool call");
            } else {
                tracing::info!(operation, outcome = ?error.code, ms, "tool call");
            }
            let value = serde_json::to_value(&error)
                .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
            Ok(CallToolResult::structured_error(value))
        }
    }
}

#[derive(Clone)]
pub struct WikiTools {
    wiki: Wiki,
}

#[tool_router]
impl WikiTools {
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }

    #[tool(description = LOAD_INSTRUCTIONS)]
    async fn load_instructions(&self) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        match self.wiki.load_instructions().await {
            // plain text keeps the instructions verbatim instead of JSON-escaped
            Ok(document) => {
                tracing::info!(
                    operation = "load_instructions",
                    outcome = "ok",
                    ms = started.elapsed().as_millis() as u64,
                    "tool call"
                );
                Ok(CallToolResult::success(vec![
                    ContentBlock::text(format!("revision: {}", document.revision)),
                    ContentBlock::text(document.markdown),
                ]))
            }
            Err(error) => respond::<()>("load_instructions", started, Err(error)),
        }
    }

    #[tool(description = SEARCH_MEMORY)]
    async fn search_memory(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        respond("search_memory", started, self.wiki.search(request).await)
    }

    #[tool(description = READ_MEMORY)]
    async fn read_memory(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        respond("read_memory", started, self.wiki.read(args.ids).await)
    }

    #[tool(description = UPDATE_MEMORY)]
    async fn update_memory(
        &self,
        Parameters(args): Parameters<UpdateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        let changes: Result<Vec<Change>, WikiError> =
            args.changes.into_iter().map(change).collect();
        let result = match changes {
            Ok(changes) => {
                self.wiki
                    .update(UpdateRequest {
                        request_id: args.request_id,
                        changes,
                    })
                    .await
            }
            Err(error) => Err(error),
        };
        respond("update_memory", started, result)
    }
}

#[tool_handler]
impl ServerHandler for WikiTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(SERVER_INSTRUCTIONS.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(operation: Operation, revision: Option<&str>, markdown: Option<&str>) -> ChangeArgs {
        ChangeArgs {
            operation,
            id: "memory/a.md".to_string(),
            expected_revision: revision.map(str::to_string),
            markdown: markdown.map(str::to_string),
        }
    }

    #[test]
    fn changes_require_exactly_their_fields() {
        assert!(matches!(
            change(args(Operation::Create, None, Some("x"))),
            Ok(Change::Create { .. })
        ));
        assert!(matches!(
            change(args(Operation::Update, Some("r"), Some("x"))),
            Ok(Change::Update { .. })
        ));
        assert!(matches!(
            change(args(Operation::Delete, Some("r"), None)),
            Ok(Change::Delete { .. })
        ));
        for invalid in [
            args(Operation::Create, None, None),
            args(Operation::Create, Some("r"), Some("x")),
            args(Operation::Update, None, Some("x")),
            args(Operation::Update, Some("r"), None),
            args(Operation::Delete, None, None),
            args(Operation::Delete, Some("r"), Some("x")),
        ] {
            assert_eq!(change(invalid).unwrap_err().code, Code::InvalidInput);
        }
    }
}
