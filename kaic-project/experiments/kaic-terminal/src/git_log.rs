use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

const LOG_LIMIT: usize = 20;

pub fn git_log(project_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "color.ui=false",
            "-c",
            "log.showSignature=false",
            "log",
            "--oneline",
            "--decorate=short",
            &format!("--max-count={LOG_LIMIT}"),
            "--",
            ".",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .current_dir(project_root)
        .output()
        .context("failed to start git log")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git log failed: {}", stderr.trim());
    }

    let log = String::from_utf8_lossy(&output.stdout);
    let log = log.trim_end();
    if log.is_empty() {
        Ok("(no commits)".to_string())
    } else {
        Ok(log.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_test_support::TestRepo;

    #[test]
    fn shows_recent_commit() {
        let repo = TestRepo::new();

        let output = git_log(repo.path()).expect("git log");

        assert!(output.contains("initial"));
    }
}
