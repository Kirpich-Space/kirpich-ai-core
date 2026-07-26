use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

pub fn git_status(project_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "color.ui=false",
            "status",
            "--short",
            "--branch",
            "--",
            ".",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .current_dir(project_root)
        .output()
        .context("failed to start git status")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git status failed: {}", stderr.trim());
    }

    let status = String::from_utf8_lossy(&output.stdout);
    let status = status.trim_end();
    if status.is_empty() {
        Ok("(working tree clean)".to_string())
    } else {
        Ok(status.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_test_support::TestRepo;

    #[test]
    fn reports_branch_and_modified_file() {
        let repo = TestRepo::new();
        repo.write("tracked.txt", "changed\n");

        let output = git_status(repo.path()).expect("git status");

        assert!(output.contains("##"));
        assert!(output.contains("tracked.txt"));
    }
}
