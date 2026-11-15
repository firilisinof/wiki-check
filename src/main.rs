//! MCP service for a personal Markdown wiki kept in Git.

mod auth;
mod config;
mod embeddings;
mod error;
mod git;
mod maintenance;
mod notes;
mod search;
mod service;
mod store;
#[cfg(test)]
mod store_tests;
mod tools;

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use axum::routing::get;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::auth::Authenticator;
use crate::config::{Listen, Server, Storage};
use crate::embeddings::{CandleEmbedder, EmbedWorker, Embedder};
use crate::service::{StoreHandle, Wiki, index_pending};
use crate::store::{Store, StoreConfig};

/// the SDK logs every session and handshake at info
const DEFAULT_LOG_FILTER: &str = "info,rmcp=warn";
const USAGE: &str = "usage: wiki-mcp <command>

  serve                          run the MCP service
  check                          validate committed notes and list what a migration must review
  reindex                        rebuild the index and embed every pending note
  export <directory>             write the committed files into an empty directory
  import <directory> [--allow-protected]
                                 commit the edits made to an export
  embed <text>                   load the model and time one embedding

Storage settings come from WIKI_MCP_GIT_DIR, WIKI_MCP_STATE_DIR, and WIKI_MCP_MODEL_DIR.";

fn load_embedder(storage: &Storage) -> Result<Option<CandleEmbedder>> {
    storage
        .model_dir
        .as_deref()
        .map(|directory| {
            CandleEmbedder::load(directory)
                .with_context(|| format!("loading the model from {}", directory.display()))
        })
        .transpose()
}

fn open_store(storage: &Storage, model: Option<String>) -> Result<Store> {
    Store::open(&StoreConfig {
        git_dir: storage.git_dir.clone(),
        state_dir: storage.state_dir.clone(),
        identity: storage.identity.clone(),
        model,
    })
}

async fn shutdown() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing the SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

async fn serve() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
        )
        .init();
    let storage = Storage::from_env()?;
    let server = Server::from_env()?;

    let started = Instant::now();
    let embedder = load_embedder(&storage)?;
    let model = embedder.as_ref().map(|e| e.model_id().to_string());
    let store = open_store(&storage, model.clone())?;
    store.repository().configure_durability()?;
    tracing::info!(
        operation = "startup",
        semantic_model = model.as_deref().unwrap_or("none"),
        ms = started.elapsed().as_millis() as u64,
        "store ready"
    );
    let wiki = Wiki::start(
        store,
        embedder.map(EmbedWorker::spawn),
        storage.push_remote.clone(),
    );

    let mut transport = StreamableHttpServerConfig::default();
    if let Some(auth) = &server.auth {
        let host = auth
            .resource
            .parse::<axum::http::Uri>()
            .ok()
            .and_then(|uri| uri.authority().map(|authority| authority.to_string()))
            .with_context(|| format!("WIKI_MCP_RESOURCE {} has no host", auth.resource))?;
        transport = transport.with_allowed_hosts([host]);
    }
    let mcp = StreamableHttpService::new(
        move || Ok(tools::WikiTools::new(wiki.clone())),
        LocalSessionManager::default().into(),
        transport,
    );
    let router = match server.auth {
        Some(settings) => {
            let authenticator = Authenticator::start(settings).await?;
            let metadata = {
                let authenticator = authenticator.clone();
                move || async move { axum::Json(authenticator.resource_metadata()) }
            };
            let guarded = tower::ServiceBuilder::new()
                .layer(axum::middleware::from_fn_with_state(
                    authenticator,
                    auth::require_token,
                ))
                .service(mcp);
            axum::Router::new()
                .route(
                    "/.well-known/oauth-protected-resource",
                    get(metadata.clone()),
                )
                .route("/.well-known/oauth-protected-resource/mcp", get(metadata))
                .nest_service("/mcp", guarded)
        }
        None => {
            tracing::warn!("authentication is disabled, so serve only on a private listener");
            axum::Router::new().nest_service("/mcp", mcp)
        }
    };

    match server.listen {
        Listen::Tcp(address) => {
            let listener = tokio::net::TcpListener::bind(&address)
                .await
                .with_context(|| format!("binding {address}"))?;
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown())
                .await?;
        }
        Listen::Unix(path) => {
            use std::os::unix::fs::PermissionsExt;
            // a socket left by an unclean stop would block the bind
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path)
                .with_context(|| format!("binding {}", path.display()))?;
            // the tunnel reaches the socket through its group
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown())
                .await?;
        }
    }
    Ok(())
}

fn reindex() -> Result<()> {
    let storage = Storage::from_env()?;
    let embedder = load_embedder(&storage)?;
    let model = embedder.as_ref().map(|e| e.model_id().to_string());
    let mut store = open_store(&storage, model)?;
    let head = store.sync().map_err(anyhow::Error::new)?;
    store.rebuild(head.as_deref()).map_err(anyhow::Error::new)?;
    let handle = StoreHandle::spawn(store);
    if let Some(embedder) = embedder {
        let started = Instant::now();
        let stored =
            index_pending(&handle, &EmbedWorker::spawn(embedder)).map_err(anyhow::Error::new)?;
        println!(
            "Embedded {stored} inputs in {:.1}s.",
            started.elapsed().as_secs_f64()
        );
    }
    let health = handle
        .call_blocking(|store| store.health())
        .map_err(anyhow::Error::new)?;
    println!("{}", serde_json::to_string_pretty(&health)?);
    Ok(())
}

fn embed(text: &str) -> Result<()> {
    let storage = Storage::from_env()?;
    let started = Instant::now();
    let embedder = load_embedder(&storage)?.context("WIKI_MCP_MODEL_DIR must be set")?;
    println!(
        "model {} loaded in {} ms",
        embedder.model_id(),
        started.elapsed().as_millis()
    );
    for run in 1..=3 {
        let started = Instant::now();
        let vector = embedder.embed(text)?;
        println!(
            "run {run}: {} dimensions in {:.1} ms",
            vector.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}

fn maintain(command: &str, arguments: &[String]) -> Result<()> {
    let storage = Storage::from_env()?;
    let mut store = open_store(&storage, None)?;
    match (command, arguments) {
        ("check", []) => {
            let report = maintenance::check(&mut store)?;
            report.print();
            if !report.problems.is_empty() {
                bail!("{} records have schema problems", report.problems.len());
            }
        }
        ("export", [directory]) => {
            let commit = maintenance::export(&mut store, Path::new(directory))?;
            println!("Exported {commit} to {directory}.");
        }
        ("import", [directory, flags @ ..]) => {
            let allow_protected = match flags {
                [] => false,
                [flag] if flag == "--allow-protected" => true,
                _ => bail!("{USAGE}"),
            };
            match maintenance::import(&mut store, Path::new(directory), allow_protected)? {
                Some(receipt) => {
                    println!(
                        "Committed {} with {} changes:",
                        receipt.commit,
                        receipt.changes.len()
                    );
                    for change in &receipt.changes {
                        println!("  {} {}", change.operation, change.id);
                    }
                    println!("Run reindex, or start the service, to embed the changed notes.");
                }
                None => println!("Nothing to import."),
            }
        }
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.split_first() {
        Some((command, [])) if command == "serve" => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(serve()),
        Some((command, [])) if command == "reindex" => reindex(),
        Some((command, [text])) if command == "embed" => embed(text),
        Some((command, rest)) if ["check", "export", "import"].contains(&command.as_str()) => {
            maintain(command, rest)
        }
        _ => bail!("{USAGE}"),
    }
}
