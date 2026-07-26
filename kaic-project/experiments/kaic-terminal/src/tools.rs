use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Context, Result};
use similar::TextDiff;

const AUDIT_DIRECTORY: &str = ".kaic";
const AUDIT_LOG_NAME: &str = "write-audit.log";
const UNIQUE_FILE_ATTEMPTS: u32 = 100;

/// Инструменты вызываются вручную пользователем через команды REPL — модель
/// их автономно не вызывает (никакой оркестрации инструментов на этом этапе).
/// Выполнение команд по-прежнему запрещено.
///
/// Добавление сверх исходной спецификации: resolve_safe() не даёт выйти за
/// пределы project_root через "..". Раз это read-only-инструменты, которые
/// пользователь просит модель почитать, разумно не позволять по ошибке (или
/// по подсказанному моделью пути) прочитать что-то за пределами открытого
/// проекта.
pub fn list_directory(project_root: &Path, rel_path: &str) -> Result<String> {
    let target = resolve_safe(project_root, rel_path)?;
    if !target.is_dir() {
        bail!("{} не является директорией", target.display());
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&target)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let suffix = if entry.path().is_dir() { "/" } else { "" };
        entries.push(format!("{name}{suffix}"));
    }
    entries.sort();
    if entries.is_empty() {
        Ok("(пусто)".to_string())
    } else {
        Ok(entries.join("\n"))
    }
}

pub fn read_file(project_root: &Path, rel_path: &str) -> Result<String> {
    let target = resolve_safe(project_root, rel_path)?;
    if !target.is_file() {
        bail!("{} не является файлом", target.display());
    }
    std::fs::read_to_string(&target)
        .map_err(|e| anyhow!("не удалось прочитать {}: {e}", target.display()))
}

#[derive(Debug)]
pub struct WriteOutcome {
    pub changed: bool,
    pub backup_path: Option<PathBuf>,
}

/// Единственная публичная точка записи файлов проекта.
///
/// Перед заменой существующего файла создаётся timestamped backup. Новое
/// содержимое сначала синхронизируется во временный файл в той же директории,
/// затем атомарно переименовывается поверх целевого файла. После операции файл
/// перечитывается через read_file() и сравнивается с ожидаемым содержимым.
pub fn write_file(project_root: &Path, rel_path: &str, content: &str) -> Result<WriteOutcome> {
    validate_relative_write_path(rel_path)?;

    let audit_log = prepare_audit_log(project_root)?;
    let target = resolve_write_safe(project_root, rel_path)?;
    ensure!(
        target != audit_log,
        "the KAIC audit log is managed internally and cannot be overwritten"
    );

    if target.exists() && !target.is_file() {
        bail!("write target is not a file: {}", target.display());
    }

    let existed = target.is_file();
    let old_content = if existed {
        fs::read_to_string(&target)
            .with_context(|| format!("failed to read existing file: {}", target.display()))?
    } else {
        String::new()
    };
    let operation_id = operation_id()?;
    let diff = build_unified_diff(rel_path, &old_content, content, existed);

    // Аудит пишется и синхронизируется до изменения целевого файла. Если лог
    // недоступен, запись не начинается.
    append_audit_record(&audit_log, &operation_id, rel_path, &diff)?;

    if existed && old_content == content {
        return Ok(WriteOutcome {
            changed: false,
            backup_path: None,
        });
    }

    let permissions = if existed {
        Some(
            fs::metadata(&target)
                .with_context(|| format!("failed to read metadata: {}", target.display()))?
                .permissions(),
        )
    } else {
        None
    };

    let backup_path = if existed {
        Some(create_backup(
            &target,
            &old_content,
            permissions.as_ref(),
            &operation_id,
        )?)
    } else {
        None
    };

    atomic_replace(&target, content, permissions.as_ref(), &operation_id)?;

    let actual = read_file(project_root, rel_path)
        .with_context(|| format!("write completed but verification read failed: {rel_path}"))?;
    ensure!(
        actual == content,
        "write verification failed: content mismatch for {rel_path}"
    );

    Ok(WriteOutcome {
        changed: true,
        backup_path,
    })
}

fn resolve_safe(project_root: &Path, rel_path: &str) -> Result<PathBuf> {
    let candidate = project_root.join(rel_path);
    let canonical_root = project_root
        .canonicalize()
        .map_err(|e| anyhow!("не удалось определить корень проекта: {e}"))?;
    let canonical_candidate = candidate
        .canonicalize()
        .map_err(|_| anyhow!("путь не найден: {rel_path}"))?;
    if !canonical_candidate.starts_with(&canonical_root) {
        bail!("доступ за пределы проекта запрещён: {rel_path}");
    }
    Ok(canonical_candidate)
}

fn validate_relative_write_path(rel_path: &str) -> Result<()> {
    if rel_path.is_empty() {
        bail!("write path cannot be empty");
    }

    let path = Path::new(rel_path);
    if path.is_absolute() {
        bail!("absolute paths are forbidden: {rel_path}");
    }

    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                bail!("access outside the project is forbidden: {rel_path}");
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }

    ensure!(
        path.file_name().is_some(),
        "write path must point to a file: {rel_path}"
    );
    Ok(())
}

fn resolve_write_safe(project_root: &Path, rel_path: &str) -> Result<PathBuf> {
    validate_relative_write_path(rel_path)?;

    let canonical_root = project_root
        .canonicalize()
        .context("failed to resolve the project root")?;
    let candidate = canonical_root.join(rel_path);

    match fs::symlink_metadata(&candidate) {
        Ok(_) => {
            let canonical_candidate = candidate
                .canonicalize()
                .with_context(|| format!("failed to resolve write path: {rel_path}"))?;
            ensure!(
                canonical_candidate.starts_with(&canonical_root),
                "access outside the project is forbidden: {rel_path}"
            );
            Ok(canonical_candidate)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let parent = candidate
                .parent()
                .ok_or_else(|| anyhow!("write path has no parent: {rel_path}"))?;
            let canonical_parent = parent.canonicalize().with_context(|| {
                format!("parent directory does not exist: {}", parent.display())
            })?;
            ensure!(
                canonical_parent.starts_with(&canonical_root),
                "access outside the project is forbidden: {rel_path}"
            );
            ensure!(
                canonical_parent.is_dir(),
                "write parent is not a directory: {}",
                canonical_parent.display()
            );
            let file_name = candidate
                .file_name()
                .ok_or_else(|| anyhow!("write path must point to a file: {rel_path}"))?;
            Ok(canonical_parent.join(file_name))
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect write path: {}", candidate.display())),
    }
}

fn prepare_audit_log(project_root: &Path) -> Result<PathBuf> {
    let canonical_root = project_root
        .canonicalize()
        .context("failed to resolve the project root")?;
    let audit_directory = canonical_root.join(AUDIT_DIRECTORY);

    match fs::create_dir(&audit_directory) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to create audit directory: {}",
                    audit_directory.display()
                )
            });
        }
    }

    let canonical_audit_directory = audit_directory
        .canonicalize()
        .context("failed to resolve the audit directory")?;
    ensure!(
        canonical_audit_directory.starts_with(&canonical_root)
            && canonical_audit_directory.is_dir(),
        "audit directory must be inside the project"
    );

    let audit_log = canonical_audit_directory.join(AUDIT_LOG_NAME);
    match fs::symlink_metadata(&audit_log) {
        Ok(_) => {
            let canonical_audit_log = audit_log
                .canonicalize()
                .context("failed to resolve the audit log")?;
            ensure!(
                canonical_audit_log.starts_with(&canonical_root) && canonical_audit_log.is_file(),
                "audit log must be a file inside the project"
            );
            Ok(canonical_audit_log)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(audit_log),
        Err(error) => Err(error).context("failed to inspect the audit log"),
    }
}

fn append_audit_record(
    audit_log: &Path,
    operation_id: &str,
    rel_path: &str,
    diff: &str,
) -> Result<()> {
    let safe_path = sanitize_log_path(rel_path);
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_log)
        .with_context(|| format!("failed to open audit log: {}", audit_log.display()))?;

    writeln!(log, "=== {operation_id} write {safe_path} ===")?;
    log.write_all(diff.as_bytes())?;
    if !diff.ends_with('\n') {
        writeln!(log)?;
    }
    writeln!(log)?;
    log.flush()?;
    log.sync_all()
        .with_context(|| format!("failed to sync audit log: {}", audit_log.display()))
}

fn build_unified_diff(rel_path: &str, old: &str, new: &str, existed: bool) -> String {
    if old == new && existed {
        return "(no changes)\n".to_string();
    }

    let safe_path = sanitize_log_path(rel_path);
    let old_header = if existed {
        format!("a/{safe_path}")
    } else {
        "/dev/null".to_string()
    };
    let new_header = format!("b/{safe_path}");
    TextDiff::from_lines(old, new)
        .unified_diff()
        .header(&old_header, &new_header)
        .to_string()
}

fn sanitize_log_path(path: &str) -> String {
    path.replace('\r', "\\r").replace('\n', "\\n")
}

fn operation_id() -> Result<String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    Ok(format!(
        "{}.{:09}-{}",
        elapsed.as_secs(),
        elapsed.subsec_nanos(),
        std::process::id()
    ))
}

fn create_backup(
    target: &Path,
    old_content: &str,
    permissions: Option<&Permissions>,
    operation_id: &str,
) -> Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("write target has no parent: {}", target.display()))?;
    let file_name = target
        .file_name()
        .ok_or_else(|| anyhow!("write target has no file name: {}", target.display()))?;

    for attempt in 0..UNIQUE_FILE_ATTEMPTS {
        let mut backup_name = OsString::from(file_name);
        backup_name.push(format!(".bak.{operation_id}"));
        if attempt > 0 {
            backup_name.push(format!(".{attempt}"));
        }
        let backup_path = parent.join(backup_name);

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup_path)
        {
            Ok(mut backup) => {
                let write_result = (|| -> std::io::Result<()> {
                    backup.write_all(old_content.as_bytes())?;
                    backup.flush()?;
                    backup.sync_all()
                })();
                drop(backup);

                if let Err(error) = write_result {
                    let _ = fs::remove_file(&backup_path);
                    return Err(error).with_context(|| {
                        format!("failed to create backup: {}", backup_path.display())
                    });
                }

                if let Some(permissions) = permissions {
                    fs::set_permissions(&backup_path, permissions.clone()).with_context(|| {
                        format!(
                            "failed to preserve backup permissions: {}",
                            backup_path.display()
                        )
                    })?;
                }
                return Ok(backup_path);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create backup: {}", backup_path.display())
                });
            }
        }
    }

    bail!(
        "failed to allocate a unique backup name for {}",
        target.display()
    )
}

fn atomic_replace(
    target: &Path,
    content: &str,
    permissions: Option<&Permissions>,
    operation_id: &str,
) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("write target has no parent: {}", target.display()))?;
    let (temp_path, mut temp_file) = create_temp_file(parent, operation_id)?;

    let write_result = (|| -> std::io::Result<()> {
        temp_file.write_all(content.as_bytes())?;
        temp_file.flush()?;
        temp_file.sync_all()
    })();
    drop(temp_file);

    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error)
            .with_context(|| format!("failed to write temporary file: {}", temp_path.display()));
    }

    if let Some(permissions) = permissions {
        if let Err(error) = fs::set_permissions(&temp_path, permissions.clone()) {
            let _ = fs::remove_file(&temp_path);
            return Err(error).with_context(|| {
                format!(
                    "failed to preserve target permissions: {}",
                    temp_path.display()
                )
            });
        }
    }

    if let Err(error) = fs::rename(&temp_path, target) {
        let _ = fs::remove_file(&temp_path);
        return Err(error).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                target.display(),
                temp_path.display()
            )
        });
    }

    Ok(())
}

fn create_temp_file(parent: &Path, operation_id: &str) -> Result<(PathBuf, File)> {
    for attempt in 0..UNIQUE_FILE_ATTEMPTS {
        let temp_path = parent.join(format!(".kaic-write-{operation_id}-{attempt}.tmp"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create temporary file: {}", temp_path.display())
                });
            }
        }
    }

    bail!(
        "failed to allocate a unique temporary file in {}",
        parent.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestProject {
        root: PathBuf,
    }

    impl TestProject {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "kaic-tools-test-{}",
                operation_id().expect("test operation id")
            ));
            fs::create_dir(&root).expect("create test project");
            Self { root }
        }
    }

    impl Drop for TestProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn writes_new_file_and_records_diff() {
        let project = TestProject::new();

        let outcome = write_file(&project.root, "new.txt", "first line\n").expect("write new file");

        assert!(outcome.changed);
        assert!(outcome.backup_path.is_none());
        assert_eq!(
            fs::read_to_string(project.root.join("new.txt")).expect("read new file"),
            "first line\n"
        );

        let audit =
            fs::read_to_string(project.root.join(".kaic/write-audit.log")).expect("read audit");
        assert!(audit.contains("--- /dev/null"));
        assert!(audit.contains("+++ b/new.txt"));
        assert!(audit.contains("+first line"));
    }

    #[test]
    fn updates_file_and_keeps_timestamped_backup() {
        let project = TestProject::new();
        let target = project.root.join("existing.txt");
        fs::write(&target, "before\n").expect("seed file");

        let first_outcome =
            write_file(&project.root, "existing.txt", "after\n").expect("update file");

        assert!(first_outcome.changed);
        let first_backup = first_outcome.backup_path.expect("first backup path");
        assert!(first_backup
            .file_name()
            .expect("backup file name")
            .to_string_lossy()
            .starts_with("existing.txt.bak."));
        assert_eq!(
            fs::read_to_string(&first_backup).expect("read first backup"),
            "before\n"
        );
        assert_eq!(fs::read_to_string(&target).expect("read target"), "after\n");

        let second_outcome =
            write_file(&project.root, "existing.txt", "final\n").expect("update file again");
        let second_backup = second_outcome.backup_path.expect("second backup path");

        assert_ne!(first_backup, second_backup);
        assert_eq!(
            fs::read_to_string(first_backup).expect("reread first backup"),
            "before\n"
        );
        assert_eq!(
            fs::read_to_string(second_backup).expect("read second backup"),
            "after\n"
        );
        assert_eq!(
            fs::read_to_string(target).expect("read final target"),
            "final\n"
        );

        let audit =
            fs::read_to_string(project.root.join(".kaic/write-audit.log")).expect("read audit");
        assert!(audit.contains("--- a/existing.txt"));
        assert!(audit.contains("-before"));
        assert!(audit.contains("+after"));
        assert!(audit.contains("+final"));
    }

    #[test]
    fn unchanged_file_is_not_rewritten_or_backed_up() {
        let project = TestProject::new();
        fs::write(project.root.join("same.txt"), "same\n").expect("seed file");

        let outcome = write_file(&project.root, "same.txt", "same\n").expect("no-op write");

        assert!(!outcome.changed);
        assert!(outcome.backup_path.is_none());
        let audit =
            fs::read_to_string(project.root.join(".kaic/write-audit.log")).expect("read audit");
        assert!(audit.contains("(no changes)"));
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let project = TestProject::new();

        let error =
            write_file(&project.root, "../outside.txt", "forbidden").expect_err("reject traversal");

        assert!(error
            .to_string()
            .contains("access outside the project is forbidden"));
        assert!(!project
            .root
            .parent()
            .expect("test project parent")
            .join("outside.txt")
            .exists());
    }

    #[test]
    fn rejects_direct_audit_log_overwrite() {
        let project = TestProject::new();

        let error = write_file(
            &project.root,
            ".kaic/write-audit.log",
            "forged audit record",
        )
        .expect_err("protect audit log");

        assert!(error
            .to_string()
            .contains("audit log is managed internally"));
        assert!(!project.root.join(".kaic/write-audit.log").exists());
    }
}
