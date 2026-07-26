use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

pub fn git_diff(project_root: &Path) -> Result<String> {
    let unstaged = run_diff(project_root, false)?;
    let staged = run_diff(project_root, true)?;

    match (unstaged.is_empty(), staged.is_empty()) {
        (true, true) => Ok("(no tracked changes)".to_string()),
        (false, true) => Ok(format!("--- unstaged ---\n{unstaged}")),
        (true, false) => Ok(format!("--- staged ---\n{staged}")),
        (false, false) => Ok(format!(
            "--- unstaged ---\n{unstaged}\n\n--- staged ---\n{staged}"
        )),
    }
}

fn run_diff(project_root: &Path, staged: bool) -> Result<String> {
    let mut command = Command::new("git");
    command.args([
        "--no-pager",
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "color.ui=false",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--submodule=short",
    ]);
    if staged {
        command.arg("--cached");
    }
    let output = command
        .arg("--")
        .arg(".")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .current_dir(project_root)
        .output()
        .context("failed to start git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff failed: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_test_support::TestRepo;

    #[test]
    fn reports_staged_and_unstaged_changes() {
        let repo = TestRepo::new();
        repo.write("tracked.txt", "unstaged\n");
        repo.write("staged.txt", "staged\n");
        repo.git(&["add", "staged.txt"]);

        let output = git_diff(repo.path()).expect("git diff");

        assert!(output.contains("--- unstaged ---"));
        assert!(output.contains("+unstaged"));
        assert!(output.contains("--- staged ---"));
        assert!(output.contains("+staged"));
    }
}
