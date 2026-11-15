//! Settings read from the environment.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::auth::AuthSettings;
use crate::git::Identity;

#[derive(Debug, Clone)]
pub struct Storage {
    /// the authoritative repository, usually bare
    pub git_dir: PathBuf,
    /// rebuildable state: the lock and the SQLite index
    pub state_dir: PathBuf,
    pub identity: Identity,
    /// directory with the pinned model files, or `None` to run without semantic search
    pub model_dir: Option<PathBuf>,
    pub push_remote: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Listen {
    Tcp(String),
    Unix(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Server {
    pub listen: Listen,
    /// `None` only when `WIKI_MCP_AUTH=disabled` was set for local testing
    pub auth: Option<AuthSettings>,
}

fn required(key: &str) -> Result<String> {
    optional(key).with_context(|| format!("{key} must be set"))
}

fn optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

impl Storage {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            git_dir: required("WIKI_MCP_GIT_DIR")?.into(),
            state_dir: required("WIKI_MCP_STATE_DIR")?.into(),
            identity: Identity {
                name: optional("WIKI_MCP_GIT_NAME").unwrap_or_else(|| "wiki-mcp".to_string()),
                email: optional("WIKI_MCP_GIT_EMAIL")
                    .unwrap_or_else(|| "wiki-mcp@localhost".to_string()),
            },
            model_dir: optional("WIKI_MCP_MODEL_DIR").map(PathBuf::from),
            push_remote: optional("WIKI_MCP_PUSH_REMOTE"),
        })
    }
}

impl Server {
    pub fn from_env() -> Result<Self> {
        let listen = required("WIKI_MCP_LISTEN")?;
        let listen = match listen.split_once(':') {
            Some(("tcp", address)) => Listen::Tcp(address.to_string()),
            Some(("unix", path)) => Listen::Unix(path.into()),
            _ => bail!("WIKI_MCP_LISTEN must be tcp:<address> or unix:<path>, got {listen:?}"),
        };
        let auth = match optional("WIKI_MCP_AUTH").as_deref() {
            Some("disabled") => None,
            Some(other) => bail!("WIKI_MCP_AUTH accepts only \"disabled\", got {other:?}"),
            None => {
                let issuer = required("WIKI_MCP_ISSUER")?;
                let resource = required("WIKI_MCP_RESOURCE")?;
                Some(AuthSettings {
                    jwks_uri: optional("WIKI_MCP_JWKS_URI")
                        .unwrap_or_else(|| format!("{issuer}/public_key.jwk")),
                    audience: required("WIKI_MCP_AUDIENCE")?,
                    required_scope: required("WIKI_MCP_REQUIRED_SCOPE")?,
                    resource_metadata_url: required("WIKI_MCP_RESOURCE_METADATA_URL")?,
                    issuer,
                    resource,
                })
            }
        };
        Ok(Self { listen, auth })
    }
}
