//! Validation for a personal Markdown wiki kept in Git.

mod check;
mod git;
mod notes;

use std::process::ExitCode;

use anyhow::{Result, bail};

const USAGE: &str = "usage: wiki-check [--staged]

Validate the wiki that contains the current directory.

  (no option)  check the working tree, including untracked files
  --staged     check what is staged for the next commit, and reject staged
               changes to README.md or legacy global/ unless WIKI_ALLOW_PROTECTED=1";

const ALLOW_PROTECTED: &str = "WIKI_ALLOW_PROTECTED";

fn run(staged: bool) -> Result<ExitCode> {
    let root = git::toplevel(&std::env::current_dir()?)?;
    let mut problems = if staged {
        check::check(&git::staged_files(&root)?)
    } else {
        check::check(&git::worktree_files(&root)?)
    };
    if staged && std::env::var(ALLOW_PROTECTED).as_deref() != Ok("1") {
        problems.extend(check::protected_changes(&git::staged_changes(&root)?));
    }
    for problem in &problems {
        println!("{}: {}", problem.path, problem.message);
    }
    if problems.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("wiki-check: {} problems", problems.len());
        Ok(ExitCode::FAILURE)
    }
}

fn main() -> Result<ExitCode> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => run(false),
        ["--staged"] => run(true),
        ["-h" | "--help"] => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        _ => bail!("{USAGE}"),
    }
}
