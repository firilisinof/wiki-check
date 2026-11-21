//! Reading wiki snapshots through the Git CLI, always with argument arrays.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::check::File;

fn git(root: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .context("running git, which must be in PATH")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn nul_separated(bytes: &[u8]) -> Result<Vec<String>> {
    bytes
        .split(|&b| b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            String::from_utf8(entry.to_vec())
                .with_context(|| format!("git listed a path that is not UTF-8: {entry:?}"))
        })
        .collect()
}

/// The top of the working tree that contains `directory`.
pub fn toplevel(directory: &Path) -> Result<PathBuf> {
    let output = git(directory, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(String::from_utf8(output)?.trim_end()))
}

/// Tracked and untracked files of the working tree, without ignored ones.
/// Tracked files deleted from the working tree are absent.
pub fn worktree_files(root: &Path) -> Result<Vec<File>> {
    let listed = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    // a path with conflicting stages is listed once per stage
    let paths: BTreeSet<String> = nul_separated(&listed)?.into_iter().collect();
    let mut files = Vec::new();
    for path in paths {
        let full = root.join(&path);
        let metadata = match std::fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("reading {path}")),
        };
        let regular = metadata.is_file();
        let bytes = if regular {
            std::fs::read(&full).with_context(|| format!("reading {path}"))?
        } else {
            Vec::new()
        };
        files.push(File {
            path,
            regular,
            bytes,
        });
    }
    Ok(files)
}

/// The files staged for the next commit, with their staged content.
pub fn staged_files(root: &Path) -> Result<Vec<File>> {
    let listed = git(root, &["ls-files", "-z", "--stage"])?;
    let mut entries = Vec::new();
    for entry in nul_separated(&listed)? {
        // <mode> <object> <stage>\t<path>
        let (meta, path) = entry
            .split_once('\t')
            .with_context(|| format!("unexpected ls-files entry {entry:?}"))?;
        let fields: Vec<&str> = meta.split(' ').collect();
        let [mode, object, stage] = fields.as_slice() else {
            bail!("unexpected ls-files entry {entry:?}");
        };
        if *stage != "0" {
            bail!("{path} has unresolved merge conflicts");
        }
        entries.push((
            path.to_string(),
            *mode == "100644" || *mode == "100755",
            object.to_string(),
        ));
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("running git cat-file")?;
    let mut stdin = child.stdin.take().context("git cat-file has no stdin")?;
    let objects: Vec<String> = entries
        .iter()
        .filter(|(_, regular, _)| *regular)
        .map(|(_, _, object)| object.clone())
        .collect();
    // write from another thread so a full stdout pipe cannot block both sides
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for object in objects {
            writeln!(stdin, "{object}")?;
        }
        Ok(())
    });
    let mut stdout = BufReader::new(child.stdout.take().context("git cat-file has no stdout")?);

    let mut files = Vec::new();
    for (path, regular, object) in entries {
        let mut bytes = Vec::new();
        if regular {
            let mut header = String::new();
            stdout.read_line(&mut header)?;
            // <object> blob <size>
            let size: usize = match header.trim_end().split(' ').collect::<Vec<_>>().as_slice() {
                [found, "blob", size] if *found == object => size.parse()?,
                _ => bail!("unexpected git cat-file header {header:?} for {path}"),
            };
            bytes.resize(size, 0);
            stdout.read_exact(&mut bytes)?;
            let mut newline = [0u8; 1];
            stdout.read_exact(&mut newline)?;
        }
        files.push(File {
            path,
            regular,
            bytes,
        });
    }
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("the git cat-file writer panicked"))??;
    if !child.wait()?.success() {
        bail!("git cat-file failed");
    }
    Ok(files)
}

/// Paths whose staged state differs from HEAD, listing both sides of a rename.
pub fn staged_changes(root: &Path) -> Result<Vec<String>> {
    let output = git(
        root,
        &["diff", "--cached", "--name-only", "-z", "--no-renames"],
    )?;
    nul_separated(&output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(root: &Path, arguments: &[&str]) {
        git(root, arguments).unwrap();
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        run(root, &["init", "--quiet", "--initial-branch=main"]);
        run(root, &["config", "user.name", "Test"]);
        run(root, &["config", "user.email", "test@example.com"]);
        run(root, &["config", "commit.gpgsign", "false"]);
        directory
    }

    #[test]
    fn staged_files_hold_the_staged_content() {
        let directory = repository();
        let root = directory.path();
        std::fs::write(root.join("README.md"), "# staged\n").unwrap();
        run(root, &["add", "README.md"]);
        std::fs::write(root.join("README.md"), "# working tree\n").unwrap();
        std::fs::write(root.join("untracked.md"), "x").unwrap();

        let staged = staged_files(root).unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].bytes, b"# staged\n");

        let worktree = worktree_files(root).unwrap();
        let paths: Vec<&str> = worktree.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["README.md", "untracked.md"]);
        assert_eq!(worktree[0].bytes, b"# working tree\n");
    }

    #[test]
    fn staged_changes_list_both_sides_of_a_rename() {
        let directory = repository();
        let root = directory.path();
        std::fs::create_dir(root.join("global")).unwrap();
        std::fs::write(root.join("global/a.md"), "rule\n").unwrap();
        // before the first commit every staged path is a change
        run(root, &["add", "."]);
        assert_eq!(staged_changes(root).unwrap(), vec!["global/a.md"]);
        run(root, &["commit", "--quiet", "-m", "first"]);

        std::fs::create_dir(root.join("memory")).unwrap();
        run(root, &["mv", "global/a.md", "memory/a.md"]);
        assert_eq!(
            staged_changes(root).unwrap(),
            vec!["global/a.md", "memory/a.md"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_regular_files() {
        let directory = repository();
        let root = directory.path();
        std::fs::write(root.join("target.md"), "x").unwrap();
        std::os::unix::fs::symlink("target.md", root.join("link.md")).unwrap();
        run(root, &["add", "."]);
        for files in [staged_files(root).unwrap(), worktree_files(root).unwrap()] {
            let link = files.iter().find(|f| f.path == "link.md").unwrap();
            assert!(!link.regular);
        }
    }
}
