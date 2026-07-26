use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_REPO_ID: AtomicU64 = AtomicU64::new(0);

pub struct TestRepo {
    root: PathBuf,
}

impl TestRepo {
    pub fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        let sequence = NEXT_REPO_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "kaic-git-tools-test-{}-{timestamp}-{sequence}",
            std::process::id(),
        ));
        fs::create_dir(&root).expect("create test repository");

        run_git(&root, &["init", "--quiet"]);
        run_git(&root, &["config", "user.name", "KAIC Test"]);
        run_git(
            &root,
            &["config", "user.email", "kaic-test@example.invalid"],
        );
        run_git(&root, &["config", "core.autocrlf", "false"]);
        fs::create_dir(root.join(".git/kaic-empty-hooks")).expect("create empty hooks directory");
        run_git(
            &root,
            &["config", "core.hooksPath", ".git/kaic-empty-hooks"],
        );

        fs::write(root.join("tracked.txt"), "initial\n").expect("write tracked fixture");
        run_git(&root, &["add", "tracked.txt"]);
        run_git(
            &root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        );

        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn write(&self, relative_path: &str, content: &str) {
        fs::write(self.root.join(relative_path), content).expect("write test file");
    }

    pub fn git(&self, arguments: &[&str]) {
        run_git(&self.root, arguments);
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run_git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .output()
        .expect("start git fixture command");
    assert!(
        output.status.success(),
        "git fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{git_diff, git_log, git_show, git_status};

    #[test]
    fn read_tools_do_not_modify_git_index() {
        let repo = TestRepo::new();
        let index_path = repo.path().join(".git/index");
        let before = fs::read(&index_path).expect("read index before tools");

        git_status::git_status(repo.path()).expect("git status");
        git_diff::git_diff(repo.path()).expect("git diff");
        git_log::git_log(repo.path()).expect("git log");
        git_show::git_show(repo.path(), "HEAD").expect("git show");

        let after = fs::read(index_path).expect("read index after tools");
        assert_eq!(before, after);
    }

    #[test]
    fn tools_are_scoped_to_project_root() {
        let repo = TestRepo::new();
        let project_root = repo.path().join("project");
        fs::create_dir(&project_root).expect("create nested project");
        repo.write("project/local.txt", "local\n");
        repo.write("outside.txt", "outside\n");
        repo.git(&["add", "project/local.txt", "outside.txt"]);
        repo.git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "project files",
        ]);

        repo.write("project/local.txt", "local changed\n");
        repo.write("outside.txt", "outside changed\n");

        let status = git_status::git_status(&project_root).expect("scoped status");
        let diff = git_diff::git_diff(&project_root).expect("scoped diff");
        let log = git_log::git_log(&project_root).expect("scoped log");
        let show = git_show::git_show(&project_root, "HEAD").expect("scoped show");

        assert!(status.contains("local.txt"));
        assert!(!status.contains("outside.txt"));
        assert!(diff.contains("local changed"));
        assert!(!diff.contains("outside changed"));
        assert!(log.contains("project files"));
        assert!(show.contains("project/local.txt"));
        assert!(!show.contains("outside.txt"));
    }
}
