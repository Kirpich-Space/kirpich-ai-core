use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

/// Только два read-only инструмента, вызываются вручную пользователем через
/// команды /read и /ls в REPL — модель их автономно не вызывает (никакой
/// оркестрации инструментов в v0.2). Никакой записи, никакого выполнения.
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
