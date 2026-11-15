# wiki-mcp

An MCP service for a personal Markdown wiki. Git holds the content. A SQLite
index, which the service can rebuild at any time, serves search.

## Tools

| Tool | Purpose |
| --- | --- |
| `load_instructions` | Return the complete root `README.md` and its revision. |
| `search_memory` | Search or enumerate records in one scope, by keyword, meaning, or both. |
| `read_memory` | Return the full Markdown and revision of 1 to 10 records. |
| `update_memory` | Apply create, update, and delete changes as one atomic, revision-checked batch. |

A record identifier is its path in the wiki, such as `memory/<project>/decisions.md`.
The path decides the scope:

| Path | Scope |
| --- | --- |
| `README.md` | root instructions, protected |
| `global/*.md` | `task_instructions`, protected |
| `prompts/*.md` | `prompts` |
| `references/*.md` | `references` |
| `memory/*.md` | `standalone` |
| `memory/<project>/*.md` | `project` |

Other files in the repository, such as generated `index.md` files, are not served.

## Build and test

```sh
cargo test
cargo build --release
nix build
```

The tests need `git` in `PATH`. The embedding test runs only when
`WIKI_MCP_MODEL_DIR` points at the model files. The Nix build sets it.

## Configuration

Storage, for every command:

| Variable | Meaning |
| --- | --- |
| `WIKI_MCP_GIT_DIR` | The authoritative Git repository. The service commits to `refs/heads/main`. |
| `WIKI_MCP_STATE_DIR` | Directory for the process lock and the SQLite index. |
| `WIKI_MCP_MODEL_DIR` | Directory with `config.json`, `tokenizer.json`, and `model.safetensors` of all-MiniLM-L6-v2. The Nix package provides it as `passthru.model`. Without it, semantic and hybrid searches fail with `semantic_unavailable`. |
| `WIKI_MCP_PUSH_REMOTE` | Optional Git remote to push each commit to. |
| `WIKI_MCP_GIT_NAME`, `WIKI_MCP_GIT_EMAIL` | Optional commit identity. |

Server, for `serve`:

| Variable | Meaning |
| --- | --- |
| `WIKI_MCP_LISTEN` | `tcp:<address>` or `unix:<path>`. |
| `WIKI_MCP_ISSUER` | OAuth issuer of the access tokens. |
| `WIKI_MCP_AUDIENCE` | Required token audience. |
| `WIKI_MCP_REQUIRED_SCOPE` | Required token scope. |
| `WIKI_MCP_RESOURCE` | Public URL of the MCP endpoint. |
| `WIKI_MCP_RESOURCE_METADATA_URL` | Public URL of the protected-resource metadata. |
| `WIKI_MCP_JWKS_URI` | Optional. The default is `<issuer>/public_key.jwk`. |
| `WIKI_MCP_AUTH` | Set to `disabled` to serve without tokens. Use this only on a private listener. |

The endpoint is `/mcp`. Logs are JSON lines on standard error. `RUST_LOG`
changes the filter.

## Commands

```sh
wiki-mcp serve
wiki-mcp check
wiki-mcp reindex
wiki-mcp export <directory>
wiki-mcp import <directory> [--allow-protected]
wiki-mcp embed <text>
```

Only one process can hold the state directory. Stop the service before you run
a maintenance command.

- `check` validates the committed notes. It also lists the plans without a
  status and the task instructions without `applies_to`.
- `reindex` rebuilds the index and computes all missing embeddings.
- `export` writes the committed files into an empty directory.
- `import` commits the edits made in an exported directory as one update. It
  stops if a record that you edited also changed in the repository after the
  export. Changes to `README.md` and `global/` need `--allow-protected`.
- `embed` loads the model and times one embedding.

## Recovery

- The index is derived data. Delete the state directory, and the service
  rebuilds it from Git at the next start.
- Each commit made by the service records its request in the commit message.
  A retry of the same `request_id` returns the first result, also after the
  index is rebuilt or the repository is restored from a clone.
- A push that fails does not undo the local commit. The next `update_memory`
  result reports the failure.
