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
pub struct PreparedWrite {
    project_root: PathBuf,
    rel_path: String,
    target: PathBuf,
    original_content: String,
    new_content: String,
    diff: String,
}

impl PreparedWrite {
    pub fn diff(&self) -> &str {
        &self.diff
    }

    pub fn has_changes(&self) -> bool {
        self.original_content != self.new_content
    }

    pub fn bytes_to_write(&self) -> usize {
        self.new_content.len()
    }
}

#[derive(Debug)]
pub struct WriteOutcome {
    pub path: PathBuf,
    pub changed: bool,
    pub backup_path: Option<PathBuf>,
    pub bytes_written: usize,
    pub verified: bool,
}

/// Готовит изменение существующего файла без записи на диск.
///
/// Возвращённый plan содержит diff и snapshot исходного содержимого. REPL
/// показывает diff пользователю и только после подтверждения передаёт plan в
/// write_file().
pub fn prepare_write_file(
    project_root: &Path,
    rel_path: &str,
    content: &str,
) -> Result<PreparedWrite> {
    validate_relative_write_path(rel_path)?;

    let canonical_root = project_root
        .canonicalize()
        .context("failed to resolve the project root")?;
    let target = resolve_write_safe(project_root, rel_path)?;
    if !target.exists() {
        bail!("write target does not exist: {rel_path}; use /create-file for new files");
    }
    if !target.is_file() {
        bail!("write target is not a file: {}", target.display());
    }

    let original_content = fs::read_to_string(&target)
        .with_context(|| format!("failed to read existing file: {}", target.display()))?;
    let diff = build_unified_diff(rel_path, &original_content, content, true);

    Ok(PreparedWrite {
        project_root: canonical_root,
        rel_path: rel_path.to_string(),
        target,
        original_content,
        new_content: content.to_string(),
        diff,
    })
}

/// Единственная публичная точка изменения существующих файлов проекта.
///
/// Принимает только plan, подготовленный prepare_write_file(). Перед записью
/// повторно проверяет путь и исходное содержимое, затем создаёт timestamped
/// backup, синхронизирует временный файл, атомарно заменяет цель и перечитывает
/// результат для проверки.
pub fn write_file(prepared: PreparedWrite) -> Result<WriteOutcome> {
    let PreparedWrite {
        project_root,
        rel_path,
        target,
        original_content,
        new_content,
        diff,
    } = prepared;

    let current_target = resolve_write_safe(&project_root, &rel_path)?;
    ensure!(
        current_target == target,
        "write preview is stale: target path changed; prepare the write again"
    );
    ensure!(
        current_target.is_file(),
        "write target is no longer a file: {}",
        current_target.display()
    );

    let current_content = fs::read_to_string(&current_target).with_context(|| {
        format!(
            "failed to re-read write target: {}",
            current_target.display()
        )
    })?;
    ensure!(
        current_content == original_content,
        "write preview is stale: file content changed; prepare the write again"
    );

    if original_content == new_content {
        return Ok(WriteOutcome {
            path: target,
            changed: false,
            backup_path: None,
            bytes_written: 0,
            verified: true,
        });
    }

    let audit_log = prepare_audit_log(&project_root)?;
    ensure!(
        target != audit_log,
        "the KAIC audit log is managed internally and cannot be overwritten"
    );
    let operation_id = operation_id()?;

    // Аудит пишется и синхронизируется до изменения целевого файла. Если лог
    // недоступен, запись не начинается.
    append_audit_record(&audit_log, &operation_id, "write_file", &rel_path, &diff)?;

    let permissions = fs::metadata(&target)
        .with_context(|| format!("failed to read metadata: {}", target.display()))?
        .permissions();
    let backup_path = Some(create_backup(
        &target,
        &original_content,
        Some(&permissions),
        &operation_id,
    )?);

    atomic_replace(&target, &new_content, Some(&permissions), &operation_id)?;

    let actual = read_file(&project_root, &rel_path)
        .with_context(|| format!("write completed but verification read failed: {rel_path}"))?;
    ensure!(
        actual == new_content,
        "write verification failed: content mismatch for {rel_path}"
    );

    Ok(WriteOutcome {
        path: target,
        changed: true,
        backup_path,
        bytes_written: new_content.len(),
        verified: true,
    })
}

#[derive(Debug)]
pub struct PreparedCreateFile {
    project_root: PathBuf,
    rel_path: String,
    target: PathBuf,
    content: String,
    diff: String,
}

impl PreparedCreateFile {
    pub fn diff(&self) -> &str {
        &self.diff
    }

    pub fn bytes_to_write(&self) -> usize {
        self.content.len()
    }
}

#[derive(Debug)]
pub struct CreateFileOutcome {
    pub path: PathBuf,
    pub bytes_written: usize,
    pub verified: bool,
}

pub fn prepare_create_file(
    project_root: &Path,
    rel_path: &str,
    content: &str,
) -> Result<PreparedCreateFile> {
    validate_relative_write_path(rel_path)?;

    let canonical_root = project_root
        .canonicalize()
        .context("failed to resolve the project root")?;
    let target = resolve_write_safe(project_root, rel_path)?;
    ensure!(
        target != canonical_root.join(AUDIT_DIRECTORY),
        "the KAIC audit directory is managed internally and cannot be created as a file"
    );
    ensure!(
        target != canonical_root.join(AUDIT_DIRECTORY).join(AUDIT_LOG_NAME),
        "the KAIC audit log is managed internally and cannot be created"
    );
    ensure!(
        !target.exists(),
        "create_file target already exists: {rel_path}"
    );
    let diff = build_unified_diff(rel_path, "", content, false);

    Ok(PreparedCreateFile {
        project_root: canonical_root,
        rel_path: rel_path.to_string(),
        target,
        content: content.to_string(),
        diff,
    })
}

pub fn create_file(prepared: PreparedCreateFile) -> Result<CreateFileOutcome> {
    let PreparedCreateFile {
        project_root,
        rel_path,
        target,
        content,
        diff,
    } = prepared;

    let current_target = resolve_write_safe(&project_root, &rel_path)?;
    ensure!(
        current_target == target && !current_target.exists(),
        "create_file preview is stale: target now exists or changed"
    );

    let audit_log = prepare_audit_log(&project_root)?;
    ensure!(
        target != audit_log,
        "the KAIC audit log is managed internally and cannot be created"
    );
    let operation_id = operation_id()?;
    append_audit_record(&audit_log, &operation_id, "create_file", &rel_path, &diff)?;

    atomic_create(&target, &content, &operation_id)?;

    let actual = read_file(&project_root, &rel_path).with_context(|| {
        format!("create_file completed but verification read failed: {rel_path}")
    })?;
    ensure!(
        actual == content,
        "create_file verification failed: content mismatch for {rel_path}"
    );

    Ok(CreateFileOutcome {
        path: target,
        bytes_written: content.len(),
        verified: true,
    })
}

#[derive(Debug)]
pub struct PreparedCreateDirectory {
    project_root: PathBuf,
    rel_path: String,
    target: PathBuf,
    preview: String,
}

impl PreparedCreateDirectory {
    pub fn preview(&self) -> &str {
        &self.preview
    }
}

#[derive(Debug)]
pub struct CreateDirectoryOutcome {
    pub path: PathBuf,
    pub verified: bool,
}

pub fn prepare_create_directory(
    project_root: &Path,
    rel_path: &str,
) -> Result<PreparedCreateDirectory> {
    validate_relative_write_path(rel_path)?;

    let canonical_root = project_root
        .canonicalize()
        .context("failed to resolve the project root")?;
    let target = resolve_write_safe(project_root, rel_path)?;
    ensure!(
        !target.exists(),
        "create_directory target already exists: {rel_path}"
    );
    ensure!(
        target != canonical_root.join(AUDIT_DIRECTORY),
        "the KAIC audit directory is managed internally and cannot be created"
    );
    ensure!(
        target != canonical_root.join(AUDIT_DIRECTORY).join(AUDIT_LOG_NAME),
        "the KAIC audit log is managed internally and cannot be created as a directory"
    );
    let safe_path = sanitize_log_path(rel_path);
    let preview = format!("create directory: {safe_path}/\nrecursive: no\nbackup: not required\n");

    Ok(PreparedCreateDirectory {
        project_root: canonical_root,
        rel_path: rel_path.to_string(),
        target,
        preview,
    })
}

pub fn create_directory(prepared: PreparedCreateDirectory) -> Result<CreateDirectoryOutcome> {
    let PreparedCreateDirectory {
        project_root,
        rel_path,
        target,
        preview,
    } = prepared;

    let current_target = resolve_write_safe(&project_root, &rel_path)?;
    ensure!(
        current_target == target && !current_target.exists(),
        "create_directory preview is stale: target now exists or changed"
    );

    let audit_log = prepare_audit_log(&project_root)?;
    let operation_id = operation_id()?;
    append_audit_record(
        &audit_log,
        &operation_id,
        "create_directory",
        &rel_path,
        &preview,
    )?;

    fs::create_dir(&target)
        .with_context(|| format!("failed to create directory: {}", target.display()))?;
    let verified_path = target
        .canonicalize()
        .with_context(|| format!("failed to verify directory: {}", target.display()))?;
    ensure!(
        verified_path.starts_with(&project_root) && verified_path.is_dir(),
        "create_directory verification failed for {rel_path}"
    );

    Ok(CreateDirectoryOutcome {
        path: verified_path,
        verified: true,
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
    operation: &str,
    rel_path: &str,
    details: &str,
) -> Result<()> {
    let safe_path = sanitize_log_path(rel_path);
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_log)
        .with_context(|| format!("failed to open audit log: {}", audit_log.display()))?;

    writeln!(log, "=== {operation_id} {operation} {safe_path} ===")?;
    log.write_all(details.as_bytes())?;
    if !details.ends_with('\n') {
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
    if !existed && old.is_empty() && new.is_empty() {
        return format!("--- /dev/null\n+++ b/{safe_path}\n");
    }
    let old_header = if existed {
        format!("a/{safe_path}")
    } else {
        "/dev/null".to_string()
    };
    let new_header = format!("b/{safe_path}");
    let rendered = TextDiff::from_lines(old, new)
        .unified_diff()
        .header(&old_header, &new_header)
        .to_string();
    if !existed && rendered.is_empty() {
        format!("--- /dev/null\n+++ {new_header}\n")
    } else {
        rendered
    }
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

fn atomic_create(target: &Path, content: &str, operation_id: &str) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("create_file target has no parent: {}", target.display()))?;
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

    if let Err(error) = fs::hard_link(&temp_path, target) {
        let _ = fs::remove_file(&temp_path);
        if error.kind() == ErrorKind::AlreadyExists {
            bail!(
                "create_file target appeared after confirmation: {}",
                target.display()
            );
        }
        return Err(error)
            .with_context(|| format!("failed to atomically create {}", target.display()));
    }

    // После успешного hard link целевой файл уже содержит полностью
    // синхронизированные данные. Удаление временного имени — best effort:
    // его сбой не должен превращать успешное создание цели в ложную ошибку.
    let _ = fs::remove_file(&temp_path);
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
    fn rejects_missing_file() {
        let project = TestProject::new();

        let error = prepare_write_file(&project.root, "new.txt", "first line\n")
            .expect_err("reject missing file");

        let message = error.to_string();
        assert!(message.contains("write target does not exist"));
        assert!(message.contains("use /create-file for new files"));
        assert!(!project.root.join("new.txt").exists());
        assert!(!project.root.join(".kaic").exists());
    }

    #[test]
    fn updates_file_and_keeps_timestamped_backup() {
        let project = TestProject::new();
        let target = project.root.join("existing.txt");
        fs::write(&target, "before\n").expect("seed file");

        let first_plan =
            prepare_write_file(&project.root, "existing.txt", "after\n").expect("prepare update");
        assert!(first_plan.has_changes());
        assert_eq!(first_plan.bytes_to_write(), "after\n".len());
        assert!(first_plan.diff().contains("-before"));
        assert!(first_plan.diff().contains("+after"));
        assert_eq!(
            fs::read_to_string(&target).expect("read target after preview"),
            "before\n"
        );
        assert!(!project.root.join(".kaic").exists());
        let first_outcome = write_file(first_plan).expect("update file");

        assert!(first_outcome.changed);
        assert!(first_outcome.verified);
        assert_eq!(
            first_outcome.path,
            target.canonicalize().expect("canonical target")
        );
        assert_eq!(first_outcome.bytes_written, "after\n".len());
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

        let second_plan = prepare_write_file(&project.root, "existing.txt", "final\n")
            .expect("prepare second update");
        let second_outcome = write_file(second_plan).expect("update file again");
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

        let plan =
            prepare_write_file(&project.root, "same.txt", "same\n").expect("prepare no-op write");
        assert!(!plan.has_changes());
        assert!(plan.diff().contains("(no changes)"));
        let outcome = write_file(plan).expect("no-op write");

        assert!(!outcome.changed);
        assert!(outcome.backup_path.is_none());
        assert_eq!(outcome.bytes_written, 0);
        assert!(outcome.verified);
        assert!(!project.root.join(".kaic").exists());
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let project = TestProject::new();

        let error = prepare_write_file(&project.root, "../outside.txt", "forbidden")
            .expect_err("reject traversal");

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
    fn rejects_stale_preview_before_writing() {
        let project = TestProject::new();
        let target = project.root.join("stale.txt");
        fs::write(&target, "before\n").expect("seed file");
        let plan =
            prepare_write_file(&project.root, "stale.txt", "planned\n").expect("prepare write");

        fs::write(&target, "changed elsewhere\n").expect("simulate concurrent change");
        let error = write_file(plan).expect_err("reject stale preview");

        assert!(error.to_string().contains("write preview is stale"));
        assert_eq!(
            fs::read_to_string(target).expect("read unchanged target"),
            "changed elsewhere\n"
        );
        assert!(!project.root.join(".kaic").exists());
    }

    #[test]
    fn rejects_direct_audit_log_overwrite() {
        let project = TestProject::new();
        fs::create_dir(project.root.join(".kaic")).expect("create audit directory");
        let audit_log = project.root.join(".kaic/write-audit.log");
        fs::write(&audit_log, "real audit\n").expect("seed audit log");

        let plan = prepare_write_file(
            &project.root,
            ".kaic/write-audit.log",
            "forged audit record",
        )
        .expect("prepare protected path");
        let error = write_file(plan).expect_err("protect audit log");

        assert!(error
            .to_string()
            .contains("audit log is managed internally"));
        assert_eq!(
            fs::read_to_string(audit_log).expect("read protected audit log"),
            "real audit\n"
        );
    }

    #[test]
    fn creates_file_after_preview_without_backup() {
        let project = TestProject::new();
        let plan = prepare_create_file(&project.root, "created.txt", "created\n")
            .expect("prepare create_file");

        assert!(plan.diff().contains("--- /dev/null"));
        assert!(plan.diff().contains("+++ b/created.txt"));
        assert_eq!(plan.bytes_to_write(), "created\n".len());
        assert!(!project.root.join("created.txt").exists());
        assert!(!project.root.join(".kaic").exists());

        let outcome = create_file(plan).expect("create file");

        assert!(outcome.verified);
        assert_eq!(outcome.bytes_written, "created\n".len());
        assert_eq!(
            fs::read_to_string(&outcome.path).expect("read created file"),
            "created\n"
        );
        let audit =
            fs::read_to_string(project.root.join(".kaic/write-audit.log")).expect("read audit");
        assert!(audit.contains("create_file created.txt"));
        assert!(audit.contains("--- /dev/null"));
        let hidden_temp_exists = fs::read_dir(&project.root)
            .expect("list project root")
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".kaic-write-")
            });
        assert!(!hidden_temp_exists);
    }

    #[test]
    fn creates_empty_file_with_explicit_preview() {
        let project = TestProject::new();
        let plan = prepare_create_file(&project.root, "empty.txt", "").expect("prepare empty file");

        assert!(plan.diff().contains("--- /dev/null"));
        assert!(plan.diff().contains("+++ b/empty.txt"));
        let outcome = create_file(plan).expect("create empty file");

        assert!(outcome.verified);
        assert_eq!(outcome.bytes_written, 0);
        assert_eq!(
            fs::metadata(outcome.path)
                .expect("read empty metadata")
                .len(),
            0
        );
    }

    #[test]
    fn create_file_never_overwrites_existing_target() {
        let project = TestProject::new();
        let target = project.root.join("existing.txt");
        fs::write(&target, "keep\n").expect("seed existing file");

        let error = prepare_create_file(&project.root, "existing.txt", "replace\n")
            .expect_err("reject existing target");

        assert!(error.to_string().contains("target already exists"));
        assert_eq!(
            fs::read_to_string(target).expect("read existing target"),
            "keep\n"
        );
    }

    #[test]
    fn create_file_rejects_target_created_after_preview() {
        let project = TestProject::new();
        let target = project.root.join("raced.txt");
        let plan =
            prepare_create_file(&project.root, "raced.txt", "planned\n").expect("prepare create");
        fs::write(&target, "other\n").expect("create raced target");

        let error = create_file(plan).expect_err("reject raced target");

        assert!(error.to_string().contains("preview is stale"));
        assert_eq!(
            fs::read_to_string(target).expect("read raced target"),
            "other\n"
        );
        assert!(!project.root.join(".kaic").exists());
    }

    #[test]
    fn creates_single_directory_after_preview() {
        let project = TestProject::new();
        let plan = prepare_create_directory(&project.root, "created-dir")
            .expect("prepare create_directory");

        assert!(plan.preview().contains("create directory: created-dir/"));
        assert!(plan.preview().contains("recursive: no"));
        assert!(!project.root.join("created-dir").exists());
        assert!(!project.root.join(".kaic").exists());

        let outcome = create_directory(plan).expect("create directory");

        assert!(outcome.verified);
        assert!(outcome.path.is_dir());
        let audit =
            fs::read_to_string(project.root.join(".kaic/write-audit.log")).expect("read audit");
        assert!(audit.contains("create_directory created-dir"));
        assert!(audit.contains("backup: not required"));
    }

    #[test]
    fn create_directory_is_not_recursive() {
        let project = TestProject::new();

        let error = prepare_create_directory(&project.root, "missing/child")
            .expect_err("reject missing parent");

        assert!(error
            .to_string()
            .contains("parent directory does not exist"));
        assert!(!project.root.join("missing").exists());
    }

    #[test]
    fn create_directory_rejects_existing_or_stale_target() {
        let project = TestProject::new();
        fs::create_dir(project.root.join("existing")).expect("seed existing directory");
        let existing_error = prepare_create_directory(&project.root, "existing")
            .expect_err("reject existing directory");
        assert!(existing_error.to_string().contains("target already exists"));

        let plan = prepare_create_directory(&project.root, "raced-dir").expect("prepare directory");
        fs::create_dir(project.root.join("raced-dir")).expect("create raced directory");
        let stale_error = create_directory(plan).expect_err("reject raced directory");
        assert!(stale_error.to_string().contains("preview is stale"));
        assert!(!project.root.join(".kaic").exists());
    }

    #[test]
    fn create_operations_reject_parent_traversal() {
        let project = TestProject::new();

        let file_error = prepare_create_file(&project.root, "../outside.txt", "forbidden")
            .expect_err("reject file traversal");
        let directory_error = prepare_create_directory(&project.root, "../outside")
            .expect_err("reject directory traversal");

        assert!(file_error
            .to_string()
            .contains("access outside the project is forbidden"));
        assert!(directory_error
            .to_string()
            .contains("access outside the project is forbidden"));
    }

    #[test]
    fn create_operations_protect_internal_audit_paths() {
        let project = TestProject::new();
        let audit_directory_file_error =
            prepare_create_file(&project.root, ".kaic", "not a directory")
                .expect_err("protect audit directory from file creation");
        assert!(audit_directory_file_error
            .to_string()
            .contains("audit directory is managed internally"));

        let directory_error =
            prepare_create_directory(&project.root, ".kaic").expect_err("protect audit directory");
        assert!(directory_error
            .to_string()
            .contains("audit directory is managed internally"));

        fs::create_dir(project.root.join(".kaic")).expect("create audit directory");
        let file_error =
            prepare_create_file(&project.root, ".kaic/write-audit.log", "forged audit")
                .expect_err("protect audit log");
        assert!(file_error
            .to_string()
            .contains("audit log is managed internally"));
        let directory_log_error = prepare_create_directory(&project.root, ".kaic/write-audit.log")
            .expect_err("protect audit log from directory creation");
        assert!(directory_log_error
            .to_string()
            .contains("audit log is managed internally"));
        assert!(!project.root.join(".kaic/write-audit.log").exists());
    }
}
