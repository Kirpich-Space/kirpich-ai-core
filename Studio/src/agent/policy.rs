//! Общий policy/tool gateway:
//! 1) Политика файловых инструментов (`AgentPolicy`) с kernel `openat(..., O_NOFOLLOW)`.
//! 2) Отдельная политика исполнения кода (`ExecPolicy`) с честной классификацией
//!    `SafeInspect` vs `UserCodeExecution` и явным указанием отсутствия OS-sandbox.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File},
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPolicy {
    ReadOnly,
    #[default]
    ApprovalRequired,
    WorkspaceWrite,
}

impl AgentPolicy {
    pub fn label_ru(self) -> &'static str {
        match self {
            Self::ReadOnly => "Только чтение (read-only)",
            Self::ApprovalRequired => "Подтверждение правок (approval-required)",
            Self::WorkspaceWrite => "Запись в workspace (workspace-write)",
        }
    }

    pub fn label_en(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read-only",
            Self::ApprovalRequired => "Approval required",
            Self::WorkspaceWrite => "Workspace write",
        }
    }

    pub fn parse_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read-only" | "read_only" | "readonly" => Ok(Self::ReadOnly),
            "approval-required" | "approval_required" | "approval" => Ok(Self::ApprovalRequired),
            "workspace-write" | "workspace_write" | "write" => Ok(Self::WorkspaceWrite),
            other => bail!("Неизвестная файловая политика агента: {other}"),
        }
    }

    pub fn cap(self, requested: Self) -> Self {
        std::cmp::min(self, requested)
    }
}

/// Отдельная политика исполнения команд/кода (не связана автоматически с `WorkspaceWrite`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecPolicy {
    Disabled,
    SafeInspectOnly,
    #[default]
    ConfirmUserCode,
    AllowWorkspaceBuilds,
}

impl ExecPolicy {
    pub fn label_ru(self) -> &'static str {
        match self {
            Self::Disabled => "Запуск команд отключён (disabled)",
            Self::SafeInspectOnly => "Только безопасный осмотр (safe-inspect)",
            Self::ConfirmUserCode => "Подтверждение запуска кода (confirm-user-code)",
            Self::AllowWorkspaceBuilds => "Сборки/тесты без запроса (allow-builds)",
        }
    }

    pub fn label_en(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::SafeInspectOnly => "safe-inspect",
            Self::ConfirmUserCode => "confirm-user-code",
            Self::AllowWorkspaceBuilds => "allow-builds",
        }
    }

    pub fn parse_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "disabled" | "off" | "none" => Ok(Self::Disabled),
            "safe-inspect" | "safe_inspect" | "safe_inspect_only" | "inspect" => {
                Ok(Self::SafeInspectOnly)
            }
            "confirm-user-code" | "confirm_user_code" | "confirm" => Ok(Self::ConfirmUserCode),
            "allow-workspace-builds" | "allow_workspace_builds" | "allow-builds" => {
                Ok(Self::AllowWorkspaceBuilds)
            }
            other => bail!("Неизвестная политика исполнения кода: {other}"),
        }
    }

    pub fn cap(self, requested: Self) -> Self {
        std::cmp::min(self, requested)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandClass {
    /// Не исполняет произвольный код/скрипты проекта (git status/diff с отключёнными хуками, py_compile без sitecustomize, ls, wc, diff).
    SafeInspect,
    /// Исполняет код/тесты/сборочные скрипты проекта с правами текущего пользователя (cargo, make, cmake, pytest, unittest, npm и т.д.).
    UserCodeExecution,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    /// Файловые операции ограничены workspace на уровне ядра (`openat` + `O_NOFOLLOW`).
    #[default]
    WorkspaceFdNoFollow,
    /// Реальный OS-sandbox отсутствует: процесс исполняется с правами текущего пользователя.
    NoneUserPrivileges,
    /// Изоляция обеспечивается внешним CLI (например, `codex --sandbox`).
    ExternalCliSandbox,
}

impl IsolationLevel {
    pub fn label_ru(self) -> &'static str {
        match self {
            Self::WorkspaceFdNoFollow => "workspace (kernel openat O_NOFOLLOW)",
            Self::NoneUserPrivileges => "НЕТ OS-SANDBOX (права текущего пользователя)",
            Self::ExternalCliSandbox => "sandbox внешнего CLI",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub raw_args: Vec<String>,
    pub class: CommandClass,
    pub isolation: IsolationLevel,
    pub risk_factors: Vec<String>,
    /// Является ли команда реальной проверкой сборки/компиляции/тестов (а не `ls`/`git status`).
    pub is_verification_check: bool,
}

#[derive(Clone, Debug)]
pub struct WorkspaceGuard {
    root: PathBuf,
}

impl WorkspaceGuard {
    pub fn new(workspace: &Path) -> Result<Self> {
        if !workspace.is_absolute() {
            bail!("Рабочий каталог (workspace) должен быть абсолютным путём");
        }
        if !workspace.exists() || !workspace.is_dir() {
            bail!(
                "Рабочий каталог не существует или не является папкой: {}",
                workspace.display()
            );
        }
        let root = workspace.canonicalize().with_context(|| {
            format!(
                "Не удалось канонизировать workspace: {}",
                workspace.display()
            )
        })?;
        if is_forbidden_workspace_root(&root) {
            bail!(
                "Системный или корневой каталог запрещён как workspace: {}",
                root.display()
            );
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve_existing(&self, raw: &str) -> Result<(PathBuf, String)> {
        let candidate = self.join_checked(raw)?;
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("Файл или каталог не найден: {raw}"))?;
        if !canonical.starts_with(&self.root) {
            bail!("Доступ запрещён: путь выходит за пределы workspace ({raw})");
        }
        if is_sensitive_path(&canonical, &self.root, false) {
            bail!("Доступ запрещён политикой защиты секретов: {raw}");
        }
        let rel = display_relative(&canonical, &self.root);
        Ok((canonical, rel))
    }

    pub fn resolve_write_target(&self, raw: &str) -> Result<(PathBuf, String)> {
        let segs = self.relative_segments(raw, true)?;
        let mut resolved = self.root.clone();
        for s in &segs {
            resolved.push(s);
        }
        let rel = segs.join("/");
        Ok((resolved, rel))
    }

    /// Открывает существующий обычный файл строго внутри workspace пошагово через
    /// `openat(..., O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)` и `O_NOFOLLOW` на листе.
    /// Устойчиво к TOCTOU-подмене symlink между проверкой пути и открытием.
    pub fn open_read_no_follow(&self, raw: &str) -> Result<(File, String)> {
        let segs = self.relative_segments(raw, false)?;
        if segs.is_empty() {
            bail!("Ожидался путь к файлу, а не корень workspace");
        }
        let mut dir_fd = open_root_dir(&self.root)?;
        for seg in &segs[..segs.len() - 1] {
            dir_fd = openat_dir_nofollow(dir_fd.as_raw_fd(), seg, false)
                .with_context(|| format!("Каталог недоступен или является symlink: {raw}"))?;
        }
        let leaf = segs.last().unwrap();
        let file_fd = openat_file_nofollow(dir_fd.as_raw_fd(), leaf, libc::O_RDONLY, 0)
            .with_context(|| {
                format!("Не удалось безопасно открыть файл (symlink запрещён): {raw}")
            })?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(file_fd.as_raw_fd(), &mut st) } != 0 {
            bail!(
                "Не удалось получить метаданные открытого файла: {}",
                std::io::Error::last_os_error()
            );
        }
        if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
            bail!("Путь не является обычным файлом: {raw}");
        }
        let rel = segs.join("/");
        Ok((File::from(file_fd), rel))
    }

    /// Вычисляет SHA-256 содержимого файла внутри workspace через `open_read_no_follow`.
    pub fn hash_file_no_follow(&self, raw: &str) -> Result<(String, String)> {
        let (mut file, rel) = self.open_read_no_follow(raw)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let hex: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok((rel, hex))
    }

    /// Атомарно записывает файл внутри workspace через `openat(..., O_NOFOLLOW)` + `mkdirat` + `renameat`.
    pub fn atomic_write_no_follow(&self, raw: &str, bytes: &[u8]) -> Result<String> {
        let segs = self.relative_segments(raw, true)?;
        if segs.is_empty() {
            bail!("Ожидался путь к файлу, а не корень workspace");
        }
        let mut dir_fd = open_root_dir(&self.root)?;
        for seg in &segs[..segs.len() - 1] {
            dir_fd = openat_dir_nofollow(dir_fd.as_raw_fd(), seg, true).with_context(|| {
                format!("Каталог записи недоступен или является symlink: {raw}")
            })?;
        }
        let leaf = segs.last().unwrap();
        let c_leaf = to_cstring(leaf)?;

        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let stat_rc = unsafe {
            libc::fstatat(
                dir_fd.as_raw_fd(),
                c_leaf.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_rc == 0 {
            let ftype = st.st_mode & libc::S_IFMT;
            if ftype == libc::S_IFLNK {
                bail!("Запись запрещена: целевой файл является symlink ({raw})");
            }
            if ftype != libc::S_IFREG {
                bail!("Запись запрещена: цель не является обычным файлом ({raw})");
            }
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ENOENT) {
                bail!("Ошибка проверки цели записи {raw}: {err}");
            }
        }

        let tmp_name = format!(".kaic-agent-{}.tmp", uuid::Uuid::new_v4());
        let c_tmp = to_cstring(&tmp_name)?;
        let tmp_fd = openat_file_nofollow(
            dir_fd.as_raw_fd(),
            &tmp_name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        let mut file = File::from(tmp_fd);
        if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            drop(file);
            unsafe {
                libc::unlinkat(dir_fd.as_raw_fd(), c_tmp.as_ptr(), 0);
            }
            return Err(e.into());
        }
        drop(file);

        let stat_rc2 = unsafe {
            libc::fstatat(
                dir_fd.as_raw_fd(),
                c_leaf.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_rc2 == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
            unsafe {
                libc::unlinkat(dir_fd.as_raw_fd(), c_tmp.as_ptr(), 0);
            }
            bail!("Запись отменена: обнаружена подмена цели на symlink ({raw})");
        }

        let ren_rc = unsafe {
            libc::renameat(
                dir_fd.as_raw_fd(),
                c_tmp.as_ptr(),
                dir_fd.as_raw_fd(),
                c_leaf.as_ptr(),
            )
        };
        if ren_rc != 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::unlinkat(dir_fd.as_raw_fd(), c_tmp.as_ptr(), 0);
            }
            bail!("Не удалось атомарно сохранить файл {raw}: {err}");
        }
        unsafe {
            libc::fsync(dir_fd.as_raw_fd());
        }
        Ok(segs.join("/"))
    }

    pub fn relative_segments(&self, raw: &str, for_write: bool) -> Result<Vec<String>> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
            bail!("Путь пуст или содержит управляющие символы");
        }
        let p = Path::new(trimmed);
        let rel_path = if p.is_absolute() {
            p.strip_prefix(&self.root).map_err(|_| {
                anyhow::anyhow!("Доступ запрещён: абсолютный путь вне workspace ({raw})")
            })?
        } else {
            p
        };
        let mut segs = Vec::new();
        for comp in rel_path.components() {
            match comp {
                Component::CurDir => {}
                Component::Normal(os) => {
                    let s = os
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("Путь не является UTF-8: {raw}"))?;
                    if s.contains('\0') {
                        bail!("Недопустимый символ NUL в пути");
                    }
                    segs.push(s.to_string());
                }
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!("Доступ запрещён: выход за пределы workspace ({raw})");
                }
            }
        }
        let mut virtual_path = self.root.clone();
        for s in &segs {
            virtual_path.push(s);
        }
        if is_sensitive_path(&virtual_path, &self.root, for_write) {
            bail!("Доступ запрещён политикой защиты секретов и служебных файлов: {raw}");
        }
        Ok(segs)
    }

    /// Проверяет и ЧЕСТНО классифицирует команду:
    /// - `SafeInspect`: только неисполняющий инспекционный вызов (`git status/diff` с отключёнными хуками,
    ///   `python3 -I -B -m py_compile` без `sitecustomize.py`, `rustc --version`, `ls`, `wc`, `diff`).
    /// - `UserCodeExecution`: реальные сборки и тесты (`cargo`, `make`, `cmake`, `ctest`, `ninja`,
    ///   `pytest`, `python3 -m unittest|pytest`, `npm`/`pnpm`/`yarn`) — с честным
    ///   `IsolationLevel::NoneUserPrivileges` и списком обнаруженных векторов исполнения кода.
    pub fn validate_command(&self, program: &str, args: &[String]) -> Result<ValidatedCommand> {
        let prog = program.trim();
        if prog.is_empty()
            || prog.contains('/')
            || prog.contains('\\')
            || prog.chars().any(char::is_control)
        {
            bail!("Команда запрещена: укажите имя программы из поддерживаемого списка без пути");
        }
        for arg in args {
            if arg.chars().any(char::is_control) {
                bail!("Аргумент команды содержит управляющие символы");
            }
        }

        // Жёстко запрещены прямые оболочки, повышение прав, деструктивные и сетевые утилиты.
        // Запрет этих команд НЕ является OS-sandbox для реальных сборок (см. IsolationLevel).
        let lower = prog.to_ascii_lowercase();
        const HARD_DENY: &[&str] = &[
            "sh", "bash", "zsh", "fish", "dash", "ash", "csh", "tcsh", "ksh", "env", "sudo", "su",
            "doas", "pkexec", "rm", "rmdir", "dd", "mkfs", "mount", "umount", "chmod", "chown",
            "kill", "pkill", "killall", "shutdown", "reboot", "curl", "wget", "ssh", "scp",
            "rsync", "nc", "ncat", "socat", "ftp", "telnet", "python",
        ];
        if HARD_DENY.contains(&lower.as_str()) {
            bail!("Команда «{prog}» запрещена политикой безопасности");
        }

        for arg in args {
            self.check_command_arg_safety(arg)?;
        }

        match prog {
            "git" => {
                for a in args {
                    if matches!(
                        a.as_str(),
                        "-c" | "--config-env"
                            | "--exec-path"
                            | "--git-dir"
                            | "--work-tree"
                            | "--upload-pack"
                            | "--ext-diff"
                    ) || a.starts_with("-c=")
                        || a.starts_with("--config-env=")
                        || a.starts_with("--exec-path=")
                        || a.starts_with("--git-dir=")
                        || a.starts_with("--work-tree=")
                    {
                        bail!(
                            "Аргумент git «{a}» запрещён политикой безопасности (подмена конфигурации/хуков)"
                        );
                    }
                }
                let sub = args
                    .first()
                    .map(String::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Укажите подкоманду git"))?;
                match sub {
                    "status" | "diff" | "log" | "show" | "rev-parse" => {}
                    _ => bail!("Подкоманда «git {sub}» запрещена политикой безопасности"),
                }
                let mut safe_args = vec![
                    "-c".into(),
                    "core.fsmonitor=false".into(),
                    "-c".into(),
                    "core.pager=cat".into(),
                    "-c".into(),
                    "diff.external=".into(),
                    "-c".into(),
                    "core.hooksPath=/dev/null".into(),
                ];
                safe_args.push(sub.to_string());
                if matches!(sub, "diff" | "show" | "log") {
                    safe_args.push("--no-ext-diff".into());
                }
                safe_args.extend_from_slice(&args[1..]);
                Ok(ValidatedCommand {
                    program: "git".into(),
                    args: safe_args,
                    raw_args: args.to_vec(),
                    class: CommandClass::SafeInspect,
                    isolation: IsolationLevel::NoneUserPrivileges,
                    risk_factors: vec![],
                    is_verification_check: false,
                })
            }
            "cargo" => {
                let sub = args
                    .first()
                    .map(String::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Укажите подкоманду cargo"))?;
                if !matches!(
                    sub,
                    "test" | "check" | "clippy" | "build" | "fmt" | "metadata" | "bench" | "doc"
                ) {
                    bail!("Подкоманда «cargo {sub}» запрещена политикой безопасности");
                }
                let mut risks = self.inspect_cargo_risk_factors();
                if args
                    .iter()
                    .any(|a| a == "--config" || a.starts_with("--config="))
                {
                    risks.push("явный флаг cargo --config".into());
                }
                let (class, is_verif) = if sub == "fmt" && args.iter().any(|a| a == "--check") {
                    if risks.is_empty() {
                        (CommandClass::SafeInspect, true)
                    } else {
                        (CommandClass::UserCodeExecution, true)
                    }
                } else {
                    (
                        CommandClass::UserCodeExecution,
                        matches!(sub, "test" | "check" | "clippy" | "build" | "fmt"),
                    )
                };
                Ok(ValidatedCommand {
                    program: "cargo".into(),
                    args: args.to_vec(),
                    raw_args: args.to_vec(),
                    class,
                    isolation: IsolationLevel::NoneUserPrivileges,
                    risk_factors: risks,
                    is_verification_check: is_verif,
                })
            }
            "python3" => {
                if args.len() < 2 || args[0] != "-m" {
                    bail!(
                        "python3 разрешён только в виде «python3 -m py_compile|unittest|pytest ...»"
                    );
                }
                let mut risks = self.inspect_python_risk_factors();
                match args[1].as_str() {
                    "py_compile" => {
                        for f in &args[2..] {
                            if f.starts_with('-') {
                                bail!("Флаги python3 -m py_compile запрещены: {f}");
                            }
                            self.relative_segments(f, false)?;
                        }
                        let class = if risks.is_empty() {
                            CommandClass::SafeInspect
                        } else {
                            CommandClass::UserCodeExecution
                        };
                        let mut safe_args =
                            vec!["-I".into(), "-B".into(), "-m".into(), "py_compile".into()];
                        safe_args.extend_from_slice(&args[2..]);
                        Ok(ValidatedCommand {
                            program: "python3".into(),
                            args: safe_args,
                            raw_args: args.to_vec(),
                            class,
                            isolation: IsolationLevel::NoneUserPrivileges,
                            risk_factors: risks,
                            is_verification_check: true,
                        })
                    }
                    "unittest" => {
                        for a in &args[2..] {
                            if !a.starts_with('-')
                                && a != "discover"
                                && (a.contains('/') || a.ends_with(".py"))
                            {
                                self.relative_segments(a, false)?;
                            }
                        }
                        risks.push("unittest исполняет Python-код проекта и тестов".into());
                        let mut safe_args =
                            vec!["-B".into(), "-s".into(), "-m".into(), "unittest".into()];
                        safe_args.extend_from_slice(&args[2..]);
                        Ok(ValidatedCommand {
                            program: "python3".into(),
                            args: safe_args,
                            raw_args: args.to_vec(),
                            class: CommandClass::UserCodeExecution,
                            isolation: IsolationLevel::NoneUserPrivileges,
                            risk_factors: risks,
                            is_verification_check: true,
                        })
                    }
                    "pytest" => {
                        risks.push("pytest исполняет conftest.py, плагины и код тестов".into());
                        let mut safe_args =
                            vec!["-B".into(), "-s".into(), "-m".into(), "pytest".into()];
                        safe_args.extend_from_slice(&args[2..]);
                        Ok(ValidatedCommand {
                            program: "python3".into(),
                            args: safe_args,
                            raw_args: args.to_vec(),
                            class: CommandClass::UserCodeExecution,
                            isolation: IsolationLevel::NoneUserPrivileges,
                            risk_factors: risks,
                            is_verification_check: true,
                        })
                    }
                    other => bail!("Модуль python3 -m {other} не входит в список проверок"),
                }
            }
            "pytest" => {
                let mut risks = self.inspect_python_risk_factors();
                risks.push("pytest исполняет conftest.py, плагины и код тестов".into());
                Ok(ValidatedCommand {
                    program: "pytest".into(),
                    args: args.to_vec(),
                    raw_args: args.to_vec(),
                    class: CommandClass::UserCodeExecution,
                    isolation: IsolationLevel::NoneUserPrivileges,
                    risk_factors: risks,
                    is_verification_check: true,
                })
            }
            "npm" | "pnpm" | "yarn" => {
                let sub = args
                    .first()
                    .map(String::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Укажите подкоманду {prog}"))?;
                if !matches!(sub, "test" | "run" | "build" | "check" | "lint") {
                    bail!("Подкоманда «{prog} {sub}» не входит в команды сборки/проверки");
                }
                Ok(ValidatedCommand {
                    program: prog.to_string(),
                    args: args.to_vec(),
                    raw_args: args.to_vec(),
                    class: CommandClass::UserCodeExecution,
                    isolation: IsolationLevel::NoneUserPrivileges,
                    risk_factors: vec![format!(
                        "{prog} исполняет shell-скрипты из package.json с правами пользователя"
                    )],
                    is_verification_check: true,
                })
            }
            "make" | "cmake" | "ctest" | "ninja" => {
                let mut risks = vec![format!(
                    "{prog} исполняет рецепты сборки/тестов проекта с правами пользователя"
                )];
                if self.root.join("Makefile").exists() || self.root.join("GNUmakefile").exists() {
                    risks.push("обнаружен Makefile в workspace".into());
                }
                if self.root.join("CMakeLists.txt").exists() {
                    risks.push("обнаружен CMakeLists.txt в workspace".into());
                }
                Ok(ValidatedCommand {
                    program: prog.to_string(),
                    args: args.to_vec(),
                    raw_args: args.to_vec(),
                    class: CommandClass::UserCodeExecution,
                    isolation: IsolationLevel::NoneUserPrivileges,
                    risk_factors: risks,
                    is_verification_check: true,
                })
            }
            "rustc" => {
                if args.len() == 1 && args[0] == "--version" {
                    Ok(ValidatedCommand {
                        program: "rustc".into(),
                        args: args.to_vec(),
                        raw_args: args.to_vec(),
                        class: CommandClass::SafeInspect,
                        isolation: IsolationLevel::NoneUserPrivileges,
                        risk_factors: vec![],
                        is_verification_check: false,
                    })
                } else {
                    Ok(ValidatedCommand {
                        program: "rustc".into(),
                        args: args.to_vec(),
                        raw_args: args.to_vec(),
                        class: CommandClass::UserCodeExecution,
                        isolation: IsolationLevel::NoneUserPrivileges,
                        risk_factors: vec!["rustc компилирует код проекта и proc-macro".into()],
                        is_verification_check: true,
                    })
                }
            }
            "ls" | "wc" | "diff" => Ok(ValidatedCommand {
                program: prog.to_string(),
                args: args.to_vec(),
                raw_args: args.to_vec(),
                class: CommandClass::SafeInspect,
                isolation: IsolationLevel::NoneUserPrivileges,
                risk_factors: vec![],
                is_verification_check: false,
            }),
            _ => bail!("Команда «{prog}» не входит в поддерживаемый список инструментов"),
        }
    }

    fn inspect_cargo_risk_factors(&self) -> Vec<String> {
        let mut risks = Vec::new();
        for cfg in [".cargo/config.toml", ".cargo/config"] {
            let p = self.root.join(cfg);
            if p.exists() {
                if let Ok(text) = fs::read_to_string(&p) {
                    let lower = text.to_ascii_lowercase();
                    for key in [
                        "rustc-wrapper",
                        "rustc-workspace-wrapper",
                        "rustc =",
                        "rustc=",
                        "rustdoc",
                        "linker",
                        "runner",
                        "[alias]",
                    ] {
                        if lower.contains(key) {
                            risks.push(format!("{cfg} задаёт «{key}»"));
                            break;
                        }
                    }
                } else {
                    risks.push(format!("обнаружен {cfg}"));
                }
            }
        }
        if has_build_rs(&self.root, 0).unwrap_or(false) {
            risks.push("обнаружен сборочный скрипт build.rs".into());
        }
        risks
    }

    fn inspect_python_risk_factors(&self) -> Vec<String> {
        let mut risks = Vec::new();
        for hook in ["sitecustomize.py", "usercustomize.py", "conftest.py"] {
            if self.root.join(hook).exists() {
                risks.push(format!("обнаружен {hook} в workspace"));
            }
        }
        risks
    }

    fn join_checked(&self, raw: &str) -> Result<PathBuf> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
            bail!("Путь пуст или содержит управляющие символы");
        }
        let p = Path::new(trimmed);
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        for comp in p.components() {
            if matches!(comp, Component::ParentDir) {
                bail!("Доступ запрещён: относительный выход «..» за пределы workspace ({raw})");
            }
        }
        Ok(full)
    }

    fn check_command_arg_safety(&self, arg: &str) -> Result<()> {
        if arg.contains("..") {
            bail!("Аргумент команды запрещён: содержит выход из каталога «..» ({arg})");
        }
        let lower = arg.to_ascii_lowercase();
        if lower.contains(".env")
            || lower.contains("auth.json")
            || lower.contains("id_rsa")
            || lower.contains("id_ed25519")
            || lower.contains(".ssh")
            || lower.contains(".gnupg")
            || lower.contains(".codex")
        {
            bail!("Аргумент команды затрагивает защищённый секрет: {arg}");
        }
        if arg.starts_with('/') {
            let p = Path::new(arg);
            if let Ok(canon) = p.canonicalize() {
                if !canon.starts_with(&self.root) {
                    bail!("Аргумент команды ссылается на путь вне workspace: {arg}");
                }
            } else if !p.starts_with(&self.root) {
                bail!("Аргумент команды ссылается на путь вне workspace: {arg}");
            }
        }
        Ok(())
    }
}

fn has_build_rs(dir: &Path, depth: usize) -> Result<bool> {
    if depth > 6 {
        return Ok(false);
    }
    for entry in fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == "build.rs" {
            return Ok(true);
        }
        if matches!(name_str.as_ref(), ".git" | "target" | "node_modules") {
            continue;
        }
        if let Ok(ft) = entry.file_type()
            && ft.is_dir()
            && has_build_rs(&entry.path(), depth + 1)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn to_cstring(s: &str) -> Result<CString> {
    CString::new(s.as_bytes()).map_err(|_| anyhow::anyhow!("Недопустимый символ NUL в пути"))
}

fn open_root_dir(root: &Path) -> Result<OwnedFd> {
    let c_root = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("Некорректный путь workspace"))?;
    let fd = unsafe {
        libc::open(
            c_root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        bail!(
            "Не удалось открыть корневой каталог workspace: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn openat_dir_nofollow(parent_fd: libc::c_int, seg: &str, create: bool) -> Result<OwnedFd> {
    let c_seg = to_cstring(seg)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mut fd = unsafe { libc::openat(parent_fd, c_seg.as_ptr(), flags) };
    if fd < 0 && create {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOENT) {
            let mk = unsafe { libc::mkdirat(parent_fd, c_seg.as_ptr(), 0o700) };
            if mk != 0 {
                let mk_err = std::io::Error::last_os_error();
                if mk_err.raw_os_error() != Some(libc::EEXIST) {
                    bail!("Не удалось создать подкаталог «{seg}»: {mk_err}");
                }
            }
            fd = unsafe { libc::openat(parent_fd, c_seg.as_ptr(), flags) };
        }
    }
    if fd < 0 {
        bail!(
            "Каталог «{seg}» отклонён (symlink или недоступен): {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn openat_file_nofollow(
    parent_fd: libc::c_int,
    name: &str,
    extra_flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<OwnedFd> {
    let c_name = to_cstring(name)?;
    let flags = extra_flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(parent_fd, c_name.as_ptr(), flags, mode as libc::c_uint) };
    if fd < 0 {
        bail!(
            "Открытие файла «{name}» отклонено (symlink или ошибка ОС): {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn is_forbidden_workspace_root(root: &Path) -> bool {
    let forbidden = [
        "/", "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/proc", "/sys", "/dev", "/var",
        "/boot", "/root", "/opt",
    ];
    if forbidden.iter().any(|p| root == Path::new(p)) {
        return true;
    }
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(canon_home) = PathBuf::from(home).canonicalize()
        && root == canon_home
    {
        return true;
    }
    false
}

pub fn is_sensitive_path(path: &Path, workspace_root: &Path, for_write: bool) -> bool {
    let rel = path.strip_prefix(workspace_root).unwrap_or(path);
    let components: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    for (idx, seg) in components.iter().enumerate() {
        if matches!(
            seg.as_str(),
            ".ssh"
                | ".gnupg"
                | ".aws"
                | ".codex"
                | ".claude"
                | ".kube"
                | ".docker"
                | ".netrc"
                | ".env"
                | "auth.json"
                | "credentials"
                | "credentials.json"
                | "secrets"
                | "secrets.json"
                | "secrets.toml"
                | "secrets.yaml"
                | "secrets.yml"
                | "id_rsa"
                | "id_rsa.pub"
                | "id_ed25519"
                | "id_ed25519.pub"
                | "authorized_keys"
        ) {
            return true;
        }
        if seg.starts_with(".env.") {
            return true;
        }
        if seg == ".git" {
            if for_write {
                return true;
            }
            if let Some(next) = components.get(idx + 1)
                && matches!(next.as_str(), "config" | "credentials" | "hooks")
            {
                return true;
            }
        }
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && matches!(
            ext.to_ascii_lowercase().as_str(),
            "pem" | "key" | "p12" | "pfx" | "kdbx"
        )
    {
        return true;
    }
    false
}

pub fn display_relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| {
            let s = p.to_string_lossy().into_owned();
            if s.is_empty() { ".".into() } else { s }
        })
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}
