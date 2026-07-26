use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

pub fn git_show(project_root: &Path, revision: &str) -> Result<String> {
    if revision.is_empty() || revision.len() > 256 || revision.chars().any(char::is_control) {
        bail!("invalid git revision");
    }

    let commit_spec = format!("{revision}^{{commit}}");
    let resolve_output = Command::new("git")
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "rev-parse",
            "--verify",
            "--end-of-options",
            &commit_spec,
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .current_dir(project_root)
        .output()
        .context("failed to resolve git revision")?;

    if !resolve_output.status.success() {
        let stderr = String::from_utf8_lossy(&resolve_output.stderr);
        bail!("git revision not found: {revision}: {}", stderr.trim());
    }

    let commit_id = String::from_utf8_lossy(&resolve_output.stdout)
        .trim()
        .to_string();
    let valid_id = matches!(commit_id.len(), 40 | 64)
        && commit_id
            .chars()
            .all(|character| character.is_ascii_hexdigit());
    if !valid_id {
        bail!("git returned an invalid commit id");
    }

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
            "show",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--format=fuller",
            "--stat",
            "--patch",
        ])
        .arg(&commit_id)
        .arg("--")
        .arg(".")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .current_dir(project_root)
        .output()
        .context("failed to start git show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git show failed: {}", stderr.trim());
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
    fn shows_resolved_commit() {
        let repo = TestRepo::new();

        let output = git_show(repo.path(), "HEAD").expect("git show");

        assert!(output.contains("initial"));
        assert!(output.contains("tracked.txt"));
    }

    #[test]
    fn rejects_option_as_revision() {
        let repo = TestRepo::new();

        let error = git_show(repo.path(), "--help").expect_err("reject option");

        assert!(error.to_string().contains("git revision not found"));
    }
}
