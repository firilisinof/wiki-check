# wiki-check

Validation for a personal Markdown wiki kept in Git. Agents read and edit the
wiki as plain files. This tool checks the layout and frontmatter before a
commit, and refuses changes to the root instructions and task rules unless
the user allows them.

## Usage

Run it anywhere inside the wiki's working tree.

```sh
wiki-check            # the working tree, including untracked files
wiki-check --staged   # what is staged for the next commit
```

It prints one `path: problem` line per problem and exits with status 1 when
there is any.

`--staged` also rejects staged changes to `README.md` and `global/`. Set
`WIKI_ALLOW_PROTECTED=1` for a commit the user asked for. Install it as the
wiki's pre-commit hook:

```sh
# .githooks/pre-commit
#!/bin/sh
exec wiki-check --staged
```

```sh
git config core.hooksPath .githooks
```

A hook only binds clients that run it. `git commit --no-verify` skips it.

## Layout

The path decides the scope:

| Path | Scope |
| --- | --- |
| `README.md` | root instructions, protected, no frontmatter |
| `global/*.md` | task rules, protected, must declare `applies_to` |
| `prompts/*.md` | role prompts |
| `references/*.md` | saved references |
| `memory/*.md` | standalone notes |
| `memory/<project>/*.md` | project notes, with a `README.md` of type `overview` |

Any other file under these directories, a nested directory, a symlink, or a
Markdown file elsewhere is a problem. Repository files such as `.gitignore`
and `.githooks/` are left alone.

## Checks

- Notes have `created`, `verified` (a date or null), a one-sentence
  `description` ending in a period, a `type` from the vocabulary, and `tags`.
  `status`, `applies_to`, and `things_project_id` are optional. No other
  field is allowed.
- References have `title`, `url` with a scheme, `description`, `added`, and
  `tags`, and optionally `status`.
- `status` is `current`, `proposed`, or `superseded`. Plans and research must
  state it.
- Notes under `memory/` are at most 4096 bytes.

Links are not checked.

## Build and test

```sh
cargo test
nix build
```

The tests need `git` in `PATH`.
