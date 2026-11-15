//! Git plumbing for the authoritative repository.
//!
//! Every call passes an argument array to the Git CLI. Nothing is interpolated
//! into a shell. Commits are built in a private temporary index, so readers of
//! the branch never see a partial tree.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

const BRANCH_REF: &str = "refs/heads/main";
const NULL_OID: &str = "0000000000000000000000000000000000000000";
const COMMIT_MARK: char = '\u{1}';
const FIELD_MARK: char = '\u{2}';
pub const TRAILER_REQUEST: &str = "Wiki-Request-Id";
pub const TRAILER_PAYLOAD: &str = "Wiki-Payload-Sha256";
pub const TRAILER_RESULT: &str = "Wiki-Result";

#[derive(Debug, Clone)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone)]
pub struct Repository {
    git_dir: PathBuf,
    identity: Identity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFile {
    pub path: String,
    pub blob: String,
}

/// One staged edit of a path. `None` removes the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEdit {
    pub path: String,
    pub content: Option<String>,
}

/// The request trailers of one commit made by the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub commit: String,
    pub request_id: String,
    pub payload_hash: String,
    pub result: String,
}

impl Repository {
    pub fn open(git_dir: &Path, identity: Identity) -> Result<Self> {
        let repository = Self {
            git_dir: git_dir.to_path_buf(),
            identity,
        };
        repository
            .run(&["rev-parse", "--git-dir"], &[], None)
            .with_context(|| format!("{} is not a Git repository", git_dir.display()))?;
        Ok(repository)
    }

    /// Make a committed object and reference update survive a power loss.
    pub fn configure_durability(&self) -> Result<()> {
        self.run(&["config", "core.fsync", "committed,reference"], &[], None)?;
        self.run(&["config", "core.fsyncMethod", "fsync"], &[], None)?;
        Ok(())
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["-c", "core.quotePath=false"])
            .args(args)
            .env("GIT_AUTHOR_NAME", &self.identity.name)
            .env("GIT_AUTHOR_EMAIL", &self.identity.email)
            .env("GIT_COMMITTER_NAME", &self.identity.name)
            .env("GIT_COMMITTER_EMAIL", &self.identity.email)
            .env("GIT_TERMINAL_PROMPT", "0")
            .envs(env.iter().copied());
        command
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut child = self
            .command(args, env)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting git")?;
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            let input = input.to_vec();
            // written from another thread so a large batch cannot deadlock on full pipes
            std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
            });
        }
        let output = child.wait_with_output().context("waiting for git")?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.first().copied().unwrap_or(""),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }

    fn run_text(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
        stdin: Option<&[u8]>,
    ) -> Result<String> {
        let output = self.run(args, env, stdin)?;
        Ok(String::from_utf8(output)
            .context("git output is not UTF-8")?
            .trim_end()
            .to_string())
    }

    /// The published commit, or `None` for a repository without one.
    pub fn head(&self) -> Result<Option<String>> {
        let output = self
            .command(&["rev-parse", "--verify", "--quiet", BRANCH_REF], &[])
            .output()
            .context("starting git")?;
        match output.status.code() {
            Some(0) => Ok(Some(String::from_utf8(output.stdout)?.trim().to_string())),
            Some(1) => Ok(None),
            _ => bail!(
                "git rev-parse failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }

    /// Regular files of a committed tree. Symbolic links and submodules are skipped.
    pub fn tree(&self, commit: &str) -> Result<Vec<TreeFile>> {
        let output = self.run(&["ls-tree", "-r", "-z", commit], &[], None)?;
        let mut files = Vec::new();
        for entry in output.split(|byte| *byte == 0).filter(|e| !e.is_empty()) {
            let entry = std::str::from_utf8(entry).context("tree path is not UTF-8")?;
            let (header, path) = entry
                .split_once('\t')
                .ok_or_else(|| anyhow!("unexpected ls-tree entry {entry:?}"))?;
            let fields: Vec<&str> = header.split(' ').collect();
            if let [mode, "blob", blob] = fields.as_slice()
                && mode.starts_with("100")
            {
                files.push(TreeFile {
                    path: path.to_string(),
                    blob: blob.to_string(),
                });
            }
        }
        Ok(files)
    }

    /// Contents of the given blobs, in order.
    pub fn blobs(&self, blobs: &[String]) -> Result<Vec<Vec<u8>>> {
        if blobs.is_empty() {
            return Ok(Vec::new());
        }
        let request = blobs.join("\n") + "\n";
        let output = self.run(&["cat-file", "--batch"], &[], Some(request.as_bytes()))?;
        let mut contents = Vec::with_capacity(blobs.len());
        let mut rest = output.as_slice();
        for blob in blobs {
            let line_end = rest
                .iter()
                .position(|byte| *byte == b'\n')
                .ok_or_else(|| anyhow!("truncated cat-file output for {blob}"))?;
            let header = std::str::from_utf8(&rest[..line_end])?;
            let size: usize = match header.split(' ').collect::<Vec<_>>().as_slice() {
                [_, "blob", size] => size.parse()?,
                _ => bail!("unexpected cat-file header {header:?}"),
            };
            let start = line_end + 1;
            if rest.len() < start + size + 1 {
                bail!("truncated cat-file content for {blob}");
            }
            contents.push(rest[start..start + size].to_vec());
            rest = &rest[start + size + 1..];
        }
        Ok(contents)
    }

    /// For every path ever changed, the newest first-parent commit that changed it.
    pub fn path_revisions(&self, commit: &str) -> Result<HashMap<String, String>> {
        let format = format!("--format={COMMIT_MARK}%H");
        let output = self.run_text(
            &[
                "log",
                "--first-parent",
                "-m",
                "--no-renames",
                "--name-only",
                &format,
                commit,
            ],
            &[],
            None,
        )?;
        let mut revisions = HashMap::new();
        let mut current = "";
        for line in output.lines() {
            if let Some(commit) = line.strip_prefix(COMMIT_MARK) {
                current = commit;
            } else if !line.is_empty() {
                revisions
                    .entry(line.to_string())
                    .or_insert_with(|| current.to_string());
            }
        }
        Ok(revisions)
    }

    /// Paths that differ between two commits.
    pub fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let output = self.run_text(
            &["diff", "--no-renames", "--name-only", from, to],
            &[],
            None,
        )?;
        Ok(output.lines().map(str::to_string).collect())
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let status = self
            .command(&["merge-base", "--is-ancestor", ancestor, descendant], &[])
            .stderr(Stdio::null())
            .status()
            .context("starting git")?;
        Ok(status.success())
    }

    /// Request receipts recorded in commit trailers, for rebuilding the lookup.
    pub fn receipts(&self, commit: &str) -> Result<Vec<Receipt>> {
        let format = format!("--format={COMMIT_MARK}%H{FIELD_MARK}%B");
        let grep = format!("--grep=^{TRAILER_REQUEST}: ");
        let output = self.run_text(
            &["log", "--first-parent", &grep, &format, commit],
            &[],
            None,
        )?;
        let mut receipts = Vec::new();
        for entry in output.split(COMMIT_MARK).filter(|e| !e.trim().is_empty()) {
            let (commit, message) = entry
                .split_once(FIELD_MARK)
                .ok_or_else(|| anyhow!("unexpected log entry"))?;
            let trailer = |key: &str| {
                message
                    .lines()
                    .rev()
                    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
                    .map(str::to_string)
            };
            if let (Some(request_id), Some(payload_hash), Some(result)) = (
                trailer(TRAILER_REQUEST),
                trailer(TRAILER_PAYLOAD),
                trailer(TRAILER_RESULT),
            ) {
                receipts.push(Receipt {
                    commit: commit.to_string(),
                    request_id,
                    payload_hash,
                    result,
                });
            }
        }
        Ok(receipts)
    }

    /// Build a commit on top of `parent` without publishing it.
    pub fn commit(
        &self,
        parent: Option<&str>,
        edits: &[TreeEdit],
        message: &str,
    ) -> Result<String> {
        let index = tempfile_path(&self.git_dir)?;
        let index_text = index.to_string_lossy().to_string();
        let env = [("GIT_INDEX_FILE", index_text.as_str())];
        let result = (|| {
            match parent {
                Some(parent) => self.run(&["read-tree", parent], &env, None)?,
                None => self.run(&["read-tree", "--empty"], &env, None)?,
            };
            // index-info stages additions and removals without a work tree
            let mut staged = String::new();
            for edit in edits {
                match &edit.content {
                    Some(content) => {
                        let blob = self.run_text(
                            &["hash-object", "-w", "--stdin"],
                            &[],
                            Some(content.as_bytes()),
                        )?;
                        staged.push_str(&format!("100644 {blob}\t{}\n", edit.path));
                    }
                    None => staged.push_str(&format!("0 {NULL_OID}\t{}\n", edit.path)),
                }
            }
            self.run(
                &["update-index", "--index-info"],
                &env,
                Some(staged.as_bytes()),
            )?;
            let tree = self.run_text(&["write-tree"], &env, None)?;
            let mut args = vec!["commit-tree", tree.as_str()];
            if let Some(parent) = parent {
                args.extend(["-p", parent]);
            }
            self.run_text(&args, &[], Some(message.as_bytes()))
        })();
        let _ = std::fs::remove_file(&index);
        result
    }

    /// Move the branch to `new` only if it still points at `expected`.
    ///
    /// This is the logical commit point of an update.
    pub fn publish(&self, new: &str, expected: Option<&str>) -> Result<()> {
        self.run(
            &["update-ref", BRANCH_REF, new, expected.unwrap_or(NULL_OID)],
            &[],
            None,
        )?;
        Ok(())
    }

    /// Replicate the branch. A push never forces and never creates a second writer.
    pub fn push(&self, remote: &str) -> Result<()> {
        let refspec = format!("{BRANCH_REF}:{BRANCH_REF}");
        self.run(&["push", "--porcelain", remote, &refspec], &[], None)?;
        Ok(())
    }

    /// Write the files of a commit into an empty directory.
    pub fn export(&self, commit: &str, destination: &Path) -> Result<()> {
        std::fs::create_dir_all(destination)?;
        if std::fs::read_dir(destination)?.next().is_some() {
            bail!("{} is not empty", destination.display());
        }
        let index = tempfile_path(&self.git_dir)?;
        let index_text = index.to_string_lossy().to_string();
        let work_tree = destination.to_string_lossy().to_string();
        let env = [
            ("GIT_INDEX_FILE", index_text.as_str()),
            ("GIT_WORK_TREE", work_tree.as_str()),
        ];
        let result = (|| {
            self.run(&["read-tree", commit], &env, None)?;
            self.run(&["checkout-index", "--all"], &env, None)
        })();
        let _ = std::fs::remove_file(&index);
        result.map(|_| ())
    }
}

fn tempfile_path(git_dir: &Path) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "wiki-mcp-index-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = git_dir.join(name);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(path)
}

#[cfg(test)]
pub mod testing {
    use super::*;

    pub fn identity() -> Identity {
        Identity {
            name: "wiki-mcp test".to_string(),
            email: "wiki-mcp@test.invalid".to_string(),
        }
    }

    /// A bare repository in a temporary directory, removed when dropped.
    pub fn bare() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("temporary directory");
        let status = Command::new("git")
            .args(["init", "--quiet", "--bare", "--initial-branch=main"])
            .arg(dir.path())
            .status()
            .expect("git init");
        assert!(status.success());
        let repository = Repository::open(dir.path(), identity()).expect("open repository");
        (dir, repository)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(path: &str, content: Option<&str>) -> TreeEdit {
        TreeEdit {
            path: path.to_string(),
            content: content.map(str::to_string),
        }
    }

    #[test]
    fn commits_are_invisible_until_published() {
        let (_dir, git) = testing::bare();
        assert_eq!(git.head().unwrap(), None);
        let first = git
            .commit(None, &[edit("memory/a.md", Some("one"))], "first")
            .unwrap();
        assert_eq!(git.head().unwrap(), None);
        git.publish(&first, None).unwrap();
        assert_eq!(git.head().unwrap(), Some(first.clone()));

        let files = git.tree(&first).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            git.blobs(&[files[0].blob.clone()]).unwrap(),
            vec![b"one".to_vec()]
        );
    }

    #[test]
    fn publish_rejects_a_moved_branch() {
        let (_dir, git) = testing::bare();
        let first = git
            .commit(None, &[edit("a.md", Some("1"))], "first")
            .unwrap();
        git.publish(&first, None).unwrap();
        let left = git
            .commit(Some(&first), &[edit("a.md", Some("2"))], "left")
            .unwrap();
        let right = git
            .commit(Some(&first), &[edit("a.md", Some("3"))], "right")
            .unwrap();
        git.publish(&left, Some(&first)).unwrap();
        assert!(git.publish(&right, Some(&first)).is_err());
        assert_eq!(git.head().unwrap(), Some(left));
    }

    #[test]
    fn revisions_follow_deletion_and_recreation() {
        let (_dir, git) = testing::bare();
        let first = git
            .commit(
                None,
                &[edit("a.md", Some("same")), edit("b.md", Some("b"))],
                "first",
            )
            .unwrap();
        let second = git
            .commit(Some(&first), &[edit("a.md", None)], "delete")
            .unwrap();
        let third = git
            .commit(Some(&second), &[edit("a.md", Some("same"))], "recreate")
            .unwrap();
        let revisions = git.path_revisions(&third).unwrap();
        assert_eq!(revisions["a.md"], third);
        assert_eq!(revisions["b.md"], first);
        assert_eq!(git.changed_paths(&first, &second).unwrap(), vec!["a.md"]);
        assert!(git.is_ancestor(&first, &third).unwrap());
        assert!(!git.is_ancestor(&third, &first).unwrap());
    }

    #[test]
    fn receipts_come_from_trailers() {
        let (_dir, git) = testing::bare();
        let plain = git
            .commit(None, &[edit("a.md", Some("1"))], "manual edit")
            .unwrap();
        let message = format!(
            "Update a.md\n\n{TRAILER_REQUEST}: req-1\n{TRAILER_PAYLOAD}: abc\n{TRAILER_RESULT}: [1]\n"
        );
        let served = git
            .commit(Some(&plain), &[edit("a.md", Some("2"))], &message)
            .unwrap();
        assert_eq!(
            git.receipts(&served).unwrap(),
            vec![Receipt {
                commit: served,
                request_id: "req-1".to_string(),
                payload_hash: "abc".to_string(),
                result: "[1]".to_string(),
            }]
        );
    }

    #[test]
    fn export_writes_the_committed_files() {
        let (_dir, git) = testing::bare();
        let first = git
            .commit(None, &[edit("memory/a.md", Some("one"))], "first")
            .unwrap();
        let out = tempfile::tempdir().unwrap();
        git.export(&first, out.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.path().join("memory/a.md")).unwrap(),
            "one"
        );
        assert!(git.export(&first, out.path()).is_err());
    }
}
