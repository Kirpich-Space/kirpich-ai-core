//! Sandboxed built-in filesystem tools for KAIC agents.
//!
//! Every operation is descriptor-relative to one pre-opened workspace root.
//! Linux `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` keeps validation and
//! opening atomic, so a symlink swap cannot escape the workspace between a
//! string path check and the actual I/O.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path};

use anyhow::{bail, Context, Result};
use rustix::fs::{mkdirat, open, openat2, Mode, OFlags, ResolveFlags};

use crate::model_backend::ToolSpec;

pub const DEFAULT_WORKSPACE_ROOT: &str = "/mnt/storage/KAIC-Workspace";
const MAX_TEXT_BYTES: usize = 1_048_576;
const CREATE_DIRECTORY: &str = "workspace_create_directory";
const WRITE_NEW_FILE: &str = "workspace_write_new_file";
const READ_TEXT_FILE: &str = "workspace_read_text_file";
const INSPECT_PATH: &str = "workspace_inspect_path";
const PROTECTED_ROOTS: &[&str] = &[
    "/boot", "/dev", "/etc", "/proc", "/root", "/run", "/sys", "/usr", "/var",
];
const PROTECTED_COMPONENTS: &[&str] = &[".config", ".git", ".gnupg", ".hermes", ".ssh"];
pub const AUTO_APPROVED_TOOLS: &[&str] = &[
    CREATE_DIRECTORY,
    WRITE_NEW_FILE,
    READ_TEXT_FILE,
    INSPECT_PATH,
];
const SAFE_RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS);

pub struct WorkspaceTools {
    root: File,
    cwd: File,
    allow_absolute: bool,
}

pub fn configured_root() -> std::path::PathBuf {
    std::env::var_os("KAIC_WORKSPACE_ROOT")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_WORKSPACE_ROOT))
}

impl WorkspaceTools {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        std::fs::create_dir_all(root)
            .with_context(|| format!("не удалось создать workspace {}", root.display()))?;
        let metadata = std::fs::symlink_metadata(root)
            .with_context(|| format!("не удалось проверить workspace {}", root.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "workspace должен быть обычным каталогом, а не ссылкой: {}",
                root.display()
            );
        }
        let canonical = root
            .canonicalize()
            .with_context(|| format!("не удалось разрешить workspace {}", root.display()))?;
        let root = open_directory_fd(&canonical)?;
        let cwd = root
            .try_clone()
            .context("не удалось клонировать workspace fd")?;
        Ok(Self {
            root,
            cwd,
            allow_absolute: false,
        })
    }

    /// Open the filesystem root. Absolute paths are allowed only outside the
    /// protected system roots checked by `safe_components`.
    pub fn new_anywhere() -> Result<Self> {
        let root = open_directory_fd(Path::new("/"))?;
        let cwd_path = std::env::current_dir().context("не удалось определить текущий каталог")?;
        let cwd = open_directory_fd(&cwd_path)?;
        Ok(Self {
            root,
            cwd,
            allow_absolute: true,
        })
    }

    pub fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<String> {
        match name {
            CREATE_DIRECTORY => {
                let path = required_string(arguments, "path")?;
                let components = safe_components(path, self.allow_absolute)?;
                self.open_directory_chain(self.base_for(path), &components, true)?;
                Ok(format!("Каталог создан: {path}"))
            }
            WRITE_NEW_FILE => {
                let path = required_string(arguments, "path")?;
                let content = required_string(arguments, "content")?;
                if content.len() > MAX_TEXT_BYTES {
                    bail!("содержимое превышает лимит {MAX_TEXT_BYTES} байт");
                }
                let components = safe_components(path, self.allow_absolute)?;
                let (parents, file_name) = split_file_path(&components)?;
                let parent = self.open_directory_chain(self.base_for(path), parents, false)?;
                let fd = match openat2(
                    &parent,
                    file_name,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                    SAFE_RESOLVE,
                ) {
                    Ok(fd) => fd,
                    Err(rustix::io::Errno::EXIST) => bail!("файл уже существует: {path}"),
                    Err(error) => {
                        return Err(error).with_context(|| format!("не удалось создать {path}"))
                    }
                };
                let mut file: File = fd.into();
                file.write_all(content.as_bytes())
                    .with_context(|| format!("не удалось записать {path}"))?;
                file.sync_all()
                    .with_context(|| format!("не удалось сохранить {path}"))?;
                Ok(format!("Файл создан: {path} ({} байт)", content.len()))
            }
            READ_TEXT_FILE => {
                let path = required_string(arguments, "path")?;
                let components = safe_components(path, self.allow_absolute)?;
                let (parents, file_name) = split_file_path(&components)?;
                let parent = self.open_directory_chain(self.base_for(path), parents, false)?;
                let fd = openat2(
                    &parent,
                    file_name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                    SAFE_RESOLVE,
                )
                .with_context(|| format!("не удалось открыть {path}"))?;
                let file: File = fd.into();
                let metadata = file
                    .metadata()
                    .with_context(|| format!("не удалось проверить {path}"))?;
                if !metadata.is_file() {
                    bail!("путь не является обычным файлом: {path}");
                }
                if metadata.len() > MAX_TEXT_BYTES as u64 {
                    bail!("файл превышает лимит {MAX_TEXT_BYTES} байт: {path}");
                }
                let mut content = String::new();
                file.take((MAX_TEXT_BYTES + 1) as u64)
                    .read_to_string(&mut content)
                    .with_context(|| format!("файл не является UTF-8 текстом: {path}"))?;
                if content.len() > MAX_TEXT_BYTES {
                    bail!("файл превышает лимит {MAX_TEXT_BYTES} байт: {path}");
                }
                Ok(content)
            }
            INSPECT_PATH => {
                let path = required_string(arguments, "path")?;
                let components = safe_components(path, self.allow_absolute)?;
                let (parents, name) = split_file_path(&components)?;
                let parent = self.open_directory_chain(self.base_for(path), parents, false)?;
                let fd = openat2(
                    &parent,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                    SAFE_RESOLVE,
                )
                .with_context(|| format!("не удалось открыть {path}"))?;
                let metadata = File::from(fd)
                    .metadata()
                    .with_context(|| format!("не удалось проверить {path}"))?;
                if metadata.is_dir() {
                    Ok(format!("Каталог существует: {path}"))
                } else if metadata.is_file() {
                    Ok(format!("Файл существует: {path} ({} байт)", metadata.len()))
                } else {
                    bail!("путь не является обычным файлом или каталогом: {path}")
                }
            }
            _ => bail!("неизвестный встроенный инструмент: {name}"),
        }
    }

    fn base_for(&self, path: &str) -> &File {
        if Path::new(path).is_absolute() {
            &self.root
        } else {
            &self.cwd
        }
    }

    fn open_directory_chain(
        &self,
        base: &File,
        components: &[OsString],
        create: bool,
    ) -> Result<File> {
        let mut current = base
            .try_clone()
            .context("не удалось клонировать корневой fd")?;
        for component in components {
            let opened = openat2(
                &current,
                component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                SAFE_RESOLVE,
            );
            let fd = match opened {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) if create => {
                    mkdirat(&current, component, Mode::RWXU)
                        .with_context(|| format!("не удалось создать каталог {:?}", component))?;
                    openat2(
                        &current,
                        component,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                        SAFE_RESOLVE,
                    )
                    .with_context(|| {
                        format!("не удалось открыть созданный каталог {:?}", component)
                    })?
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("не удалось открыть каталог {:?}", component))
                }
            };
            current = fd.into();
        }
        Ok(current)
    }
}

fn open_directory_fd(path: &Path) -> Result<File> {
    open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(Into::into)
    .with_context(|| format!("не удалось безопасно открыть корень {}", path.display()))
}

fn required_string<'a>(arguments: &'a serde_json::Value, field: &str) -> Result<&'a str> {
    arguments
        .as_object()
        .and_then(|object| object.get(field))
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("обязательное строковое поле отсутствует: {field}"))
}

fn safe_components(path: &str, allow_absolute: bool) -> Result<Vec<OsString>> {
    let path = Path::new(path);
    if path.as_os_str().is_empty() || (!allow_absolute && path.is_absolute()) {
        bail!("путь должен быть непустым и относительным");
    }
    if path.components().any(|component| {
        matches!(component, Component::Normal(value)
            if PROTECTED_COMPONENTS.iter().any(|protected| value == OsStr::new(protected)))
    }) {
        bail!("путь находится в защищённом каталоге: {}", path.display());
    }
    if allow_absolute
        && path.is_absolute()
        && PROTECTED_ROOTS
            .iter()
            .any(|root| path == Path::new(root) || path.starts_with(root))
    {
        bail!("путь находится в защищённом каталоге: {}", path.display());
    }
    path.components()
        .filter(|component| !matches!(component, Component::RootDir))
        .map(|component| match component {
            Component::Normal(value) if value != OsStr::new("") => Ok(value.to_os_string()),
            _ => bail!("путь содержит запрещённый компонент: {}", path.display()),
        })
        .collect()
}

fn split_file_path(components: &[OsString]) -> Result<(&[OsString], &OsStr)> {
    let (file_name, parents) = components
        .split_last()
        .context("путь файла не содержит имени")?;
    Ok((parents, file_name.as_os_str()))
}

pub fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        CREATE_DIRECTORY | WRITE_NEW_FILE | READ_TEXT_FILE | INSPECT_PATH
    )
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: CREATE_DIRECTORY.to_string(),
            description: "Создать каталог по абсолютному или относительному пути вне защищённых системных каталогов. .. и ссылки запрещены.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string", "minLength": 1 } },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: WRITE_NEW_FILE.to_string(),
            description: "Создать новый UTF-8 текстовый файл по абсолютному или относительному пути вне защищённых системных каталогов. Существующие файлы никогда не перезаписываются.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1 },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: READ_TEXT_FILE.to_string(),
            description: "Прочитать UTF-8 текстовый файл по абсолютному или относительному пути вне защищённых системных каталогов, размером до 1 MiB.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string", "minLength": 1 } },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: INSPECT_PATH.to_string(),
            description: "Проверить, существует ли обычный файл или каталог по абсолютному либо относительному пути. Используй после создания каталога; не читай каталог как текстовый файл.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string", "minLength": 1 } },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("kaic-workspace-tools-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn creates_directory_and_new_text_file_then_reads_it() {
        let root = temp_root();
        std::fs::create_dir_all(&root).unwrap();
        let tools = WorkspaceTools::new(&root).unwrap();
        tools
            .execute(CREATE_DIRECTORY, &serde_json::json!({"path":"project/src"}))
            .unwrap();
        tools
            .execute(
                WRITE_NEW_FILE,
                &serde_json::json!({"path":"project/src/main.rs","content":"fn main() {}\n"}),
            )
            .unwrap();
        let content = tools
            .execute(
                READ_TEXT_FILE,
                &serde_json::json!({"path":"project/src/main.rs"}),
            )
            .unwrap();
        assert_eq!(content, "fn main() {}\n");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creates_an_explicitly_empty_new_text_file() {
        let root = temp_root();
        std::fs::create_dir_all(&root).unwrap();
        let tools = WorkspaceTools::new(&root).unwrap();
        tools
            .execute(
                WRITE_NEW_FILE,
                &serde_json::json!({"path":"Test.md","content":""}),
            )
            .expect("an explicit empty content string must create a zero-byte file");
        assert_eq!(std::fs::read(root.join("Test.md")).unwrap(), b"");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inspects_a_created_directory_without_reading_it_as_text() {
        let root = temp_root();
        std::fs::create_dir_all(&root).unwrap();
        let tools = WorkspaceTools::new(&root).unwrap();
        tools
            .execute(
                CREATE_DIRECTORY,
                &serde_json::json!({"path":"project/assets"}),
            )
            .unwrap();
        assert_eq!(
            tools
                .execute(
                    "workspace_inspect_path",
                    &serde_json::json!({"path":"project/assets"}),
                )
                .unwrap(),
            "Каталог существует: project/assets"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn allows_safe_absolute_user_paths_in_anywhere_mode() {
        let root = temp_root();
        std::fs::create_dir_all(&root).unwrap();
        let tools = WorkspaceTools::new_anywhere().unwrap();
        let file = root.join("kaic-absolute-test.txt");
        tools
            .execute(
                WRITE_NEW_FILE,
                &serde_json::json!({
                    "path": file.to_str().unwrap(),
                    "content": "KAIC_ABSOLUTE_OK"
                }),
            )
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "KAIC_ABSOLUTE_OK");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_protected_absolute_paths() {
        let tools = WorkspaceTools::new_anywhere().unwrap();
        for path in [
            "/etc/kaic-test",
            "/usr/kaic-test",
            "/proc/kaic-test",
            "/root/kaic-test",
        ] {
            let error = tools
                .execute(
                    WRITE_NEW_FILE,
                    &serde_json::json!({"path":path,"content":"x"}),
                )
                .unwrap_err();
            assert!(error.to_string().contains("защищён"), "path: {path}");
        }
    }

    #[test]
    fn rejects_absolute_parent_and_symlink_escape_paths() {
        let root = temp_root();
        let outside = root.with_extension("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let tools = WorkspaceTools::new(&root).unwrap();
        for path in ["/tmp/no", "../no", "escape/no"] {
            assert!(
                tools
                    .execute(
                        WRITE_NEW_FILE,
                        &serde_json::json!({"path":path,"content":"x"})
                    )
                    .is_err(),
                "path must fail: {path}"
            );
        }
        assert!(!outside.join("no").exists());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn refuses_to_overwrite_an_existing_file() {
        let root = temp_root();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.txt"), "original").unwrap();
        let tools = WorkspaceTools::new(&root).unwrap();
        let error = tools
            .execute(
                WRITE_NEW_FILE,
                &serde_json::json!({"path":"note.txt","content":"replacement"}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("уже существует"));
        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).unwrap(),
            "original"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exposes_only_bounded_workspace_tool_specs() {
        let names = specs()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                CREATE_DIRECTORY,
                WRITE_NEW_FILE,
                READ_TEXT_FILE,
                INSPECT_PATH
            ]
        );
        assert!(names.iter().all(|name| is_builtin(name)));
    }
}
