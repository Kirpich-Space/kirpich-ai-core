//! Реальные инструменты рабочего каталога (read/list/search/write/edit/apply_patch/run_check/skills).
use super::asm_accel;
use super::policy::{WorkspaceGuard, display_relative, is_sensitive_path};
use super::subagents::{is_path_owned, normalize_rel};
use crate::network::Cancel;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_READ_BYTES: usize = 128 * 1024;
const MAX_WRITE_BYTES: usize = 512 * 1024;
const MAX_SEARCH_FILES: usize = 500;
const MAX_SEARCH_FILE_BYTES: u64 = 256 * 1024;
const MAX_COMMAND_OUTPUT_BYTES: usize = 32 * 1024;
const COMMAND_TIMEOUT_SECS: u64 = 60;

#[derive(Clone, Debug)]
pub struct ToolExecution {
    pub ok: bool,
    pub summary: String,
    pub output: String,
    pub changed_files: Vec<String>,
    pub diff_preview: Option<String>,
}

pub fn tool_definitions(allow_subagents: bool) -> Vec<Value> {
    let mut defs = vec![
        json!({
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "Показать список файлов и каталогов внутри выбранного workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Относительный путь внутри workspace (по умолчанию '.')" },
                        "max_entries": { "type": "integer", "description": "Максимум элементов (по умолчанию 100, макс 300)" }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Прочитать UTF-8 текстовый файл внутри workspace с номерами строк.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Относительный путь файла внутри workspace" },
                        "start_line": { "type": "integer", "description": "Начальная строка (1-based)" },
                        "end_line": { "type": "integer", "description": "Конечная строка (1-based, включительно)" }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "search_text",
                "description": "Найти подстроку в текстовых файлах внутри workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Искомая подстрока" },
                        "path": { "type": "string", "description": "Относительный подкаталог (по умолчанию '.')" },
                        "max_matches": { "type": "integer", "description": "Максимум совпадений (по умолчанию 40, макс 100)" }
                    },
                    "required": ["query"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Создать или перезаписать текстовый файл внутри workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Относительный путь файла внутри workspace" },
                        "content": { "type": "string", "description": "Полное содержимое файла" }
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Точечно заменить уникальный фрагмент old_text на new_text в существующем файле внутри workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Относительный путь файла внутри workspace" },
                        "old_text": { "type": "string", "description": "Точный фрагмент для замены" },
                        "new_text": { "type": "string", "description": "Новый текст замены" }
                    },
                    "required": ["path", "old_text", "new_text"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "apply_patch",
                "description": "Применить unified diff патч (--- a/file, +++ b/file, @@ -l,c +l,c @@) к одному или нескольким файлам внутри workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "patch": { "type": "string", "description": "Текст патча в формате unified diff" }
                    },
                    "required": ["patch"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_skills",
                "description": "Показать список доступных локальных SKILL.md в каталоге .kaic/skills workspace и системе.",
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_skill",
                "description": "Прочитать содержимое локального навыка SKILL.md по его идентификатору id.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Идентификатор навыка из list_skills" }
                    },
                    "required": ["id"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "run_check",
                "description": "Выполнить локальную проверку или сборку внутри workspace (git status/diff, python3 -m py_compile|unittest|pytest, pytest, cargo, make, cmake, ctest, ninja, npm test/run). Запуск кода выполняется с правами пользователя по отдельной политике exec_policy (не OS-sandbox).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "program": { "type": "string", "description": "Имя программы (cargo, python3, pytest, make, cmake, ctest, ninja, npm, git, ls, wc, diff)" },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Список аргументов программы"
                        }
                    },
                    "required": ["program", "args"]
                }
            }
        }),
    ];
    if allow_subagents {
        defs.push(json!({
            "type": "function",
            "function": {
                "name": "delegate_subtasks",
                "description": "Делегировать до 2 ограниченных подзадач с явными входами, правами (не выше родительских) и непересекающимися owned_paths для записи. Итоги подзадач требуют проверки оркестратором.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "subtasks": {
                            "type": "array",
                            "maxItems": 2,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "title": { "type": "string" },
                                    "instructions": { "type": "string" },
                                    "policy": { "type": "string", "enum": ["read_only", "approval_required", "workspace_write"] },
                                    "owned_paths": {
                                        "type": "array",
                                        "items": { "type": "string" }
                                    }
                                },
                                "required": ["title", "instructions"]
                            }
                        }
                    },
                    "required": ["subtasks"]
                }
            }
        }));
    }
    defs
}

pub fn exec_list_files(guard: &WorkspaceGuard, args: &Value) -> Result<ToolExecution> {
    let raw_path = args["path"].as_str().unwrap_or(".");
    let max_entries = args["max_entries"].as_u64().unwrap_or(100).clamp(1, 300) as usize;
    let (dir, rel) = guard.resolve_existing(raw_path)?;
    if !dir.is_dir() {
        bail!("Путь не является каталогом: {rel}");
    }
    let mut entries = Vec::new();
    let mut truncated = false;
    collect_entries(
        &dir,
        guard.root(),
        max_entries,
        &mut entries,
        &mut truncated,
    )?;
    entries.sort();
    let mut output = entries.join("\n");
    if truncated {
        output.push_str(&format!("\n… (показаны первые {max_entries} элементов)"));
    }
    if output.is_empty() {
        output = "(пустой каталог)".into();
    }
    Ok(ToolExecution {
        ok: true,
        summary: format!("list_files({rel}) → {} элементов", entries.len()),
        output,
        changed_files: vec![],
        diff_preview: None,
    })
}

fn collect_entries(
    dir: &Path,
    root: &Path,
    limit: usize,
    out: &mut Vec<String>,
    truncated: &mut bool,
) -> Result<()> {
    let mut items: Vec<_> = fs::read_dir(dir)?.filter_map(Result::ok).collect();
    items.sort_by_key(|e| e.file_name());
    for entry in items {
        if out.len() >= limit {
            *truncated = true;
            return Ok(());
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), ".git" | "target" | "node_modules") {
            continue;
        }
        let Ok(canon) = path.canonicalize() else {
            continue;
        };
        if !canon.starts_with(root) || is_sensitive_path(&canon, root, false) {
            continue;
        }
        let rel = display_relative(&canon, root);
        if canon.is_dir() {
            out.push(format!("{rel}/"));
            collect_entries(&canon, root, limit, out, truncated)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

pub fn exec_read_file(guard: &WorkspaceGuard, args: &Value) -> Result<ToolExecution> {
    let raw_path = args["path"]
        .as_str()
        .context("Для read_file требуется строковый параметр path")?;
    let (mut file, rel) = guard.open_read_no_follow(raw_path)?;
    let meta = file.metadata()?;
    if meta.len() as usize > MAX_READ_BYTES {
        bail!(
            "Файл {rel} превышает лимит чтения ({} байт > {MAX_READ_BYTES} байт)",
            meta.len()
        );
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_READ_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_READ_BYTES {
        bail!("Файл {rel} превышает лимит чтения {MAX_READ_BYTES} байт");
    }
    if bytes.contains(&0) {
        bail!("Бинарный файл отклонён: {rel}");
    }
    let text = String::from_utf8(bytes).with_context(|| format!("Файл {rel} не является UTF-8"))?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = args["start_line"].as_u64().unwrap_or(1).max(1) as usize;
    let end = args["end_line"]
        .as_u64()
        .map(|v| v as usize)
        .unwrap_or(total)
        .min(total);
    let mut rendered = String::new();
    if start <= end && total > 0 {
        for (idx, line) in lines[start - 1..end].iter().enumerate() {
            rendered.push_str(&format!("{}: {line}\n", start + idx));
        }
    }
    Ok(ToolExecution {
        ok: true,
        summary: format!("read_file({rel}) · {total} строк"),
        output: rendered,
        changed_files: vec![],
        diff_preview: None,
    })
}

pub fn exec_search_text(guard: &WorkspaceGuard, args: &Value) -> Result<ToolExecution> {
    let query = args["query"]
        .as_str()
        .context("Для search_text требуется строковый параметр query")?;
    if query.is_empty() {
        bail!("Поисковый запрос не может быть пустым");
    }
    let raw_path = args["path"].as_str().unwrap_or(".");
    let max_matches = args["max_matches"].as_u64().unwrap_or(40).clamp(1, 100) as usize;
    let (target, rel) = guard.resolve_existing(raw_path)?;
    let mut matches = Vec::new();
    let mut visited = 0usize;
    search_walk(
        &target,
        guard.root(),
        query,
        max_matches,
        &mut visited,
        &mut matches,
    )?;
    let output = if matches.is_empty() {
        format!("Совпадений для «{query}» в {rel} не найдено.")
    } else {
        matches.join("\n")
    };
    Ok(ToolExecution {
        ok: true,
        summary: format!(
            "search_text(«{query}» в {rel}) → {} совпадений",
            matches.len()
        ),
        output,
        changed_files: vec![],
        diff_preview: None,
    })
}

fn search_walk(
    path: &Path,
    root: &Path,
    query: &str,
    max_matches: usize,
    visited: &mut usize,
    matches: &mut Vec<String>,
) -> Result<()> {
    if matches.len() >= max_matches || *visited >= MAX_SEARCH_FILES {
        return Ok(());
    }
    if path.is_file() {
        *visited += 1;
        if let Ok(meta) = path.metadata()
            && meta.len() <= MAX_SEARCH_FILE_BYTES
            && let Ok(bytes) = fs::read(path)
            && !bytes.contains(&0)
            && let Ok(text) = std::str::from_utf8(&bytes)
        {
            let rel = display_relative(path, root);
            for (idx, line) in text.lines().enumerate() {
                if line.contains(query) {
                    let clipped: String = line.chars().take(180).collect();
                    matches.push(format!("{rel}:{}: {clipped}", idx + 1));
                    if matches.len() >= max_matches {
                        break;
                    }
                }
            }
        }
        return Ok(());
    }
    if path.is_dir() {
        let mut items: Vec<_> = fs::read_dir(path)?.filter_map(Result::ok).collect();
        items.sort_by_key(|e| e.file_name());
        for entry in items {
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches!(name.as_str(), ".git" | "target" | "node_modules") {
                continue;
            }
            let Ok(canon) = entry.path().canonicalize() else {
                continue;
            };
            if !canon.starts_with(root) || is_sensitive_path(&canon, root, false) {
                continue;
            }
            search_walk(&canon, root, query, max_matches, visited, matches)?;
            if matches.len() >= max_matches || *visited >= MAX_SEARCH_FILES {
                break;
            }
        }
    }
    Ok(())
}

pub fn exec_write_file(guard: &WorkspaceGuard, args: &Value) -> Result<ToolExecution> {
    let raw_path = args["path"]
        .as_str()
        .context("Для write_file требуется строковый параметр path")?;
    let content = args["content"]
        .as_str()
        .context("Для write_file требуется строковый параметр content")?;
    if content.len() > MAX_WRITE_BYTES {
        bail!(
            "Размер записи превышает лимит ({} байт > {MAX_WRITE_BYTES} байт)",
            content.len()
        );
    }
    let before = guard
        .open_read_no_follow(raw_path)
        .ok()
        .and_then(|(mut f, _)| {
            let mut b = Vec::new();
            f.by_ref()
                .take(MAX_WRITE_BYTES as u64)
                .read_to_end(&mut b)
                .ok()?;
            String::from_utf8(b).ok()
        })
        .unwrap_or_default();
    let rel = guard.atomic_write_no_follow(raw_path, content.as_bytes())?;
    let lines = content.lines().count();
    let diff = unified_diff_preview(&rel, &before, content, 60);
    Ok(ToolExecution {
        ok: true,
        summary: format!("write_file({rel}) · {} байт, {lines} строк", content.len()),
        output: format!(
            "Файл {rel} записан ({} байт, {lines} строк).",
            content.len()
        ),
        changed_files: vec![rel],
        diff_preview: (!diff.is_empty()).then_some(diff),
    })
}

pub fn exec_edit_file(guard: &WorkspaceGuard, args: &Value) -> Result<ToolExecution> {
    let raw_path = args["path"]
        .as_str()
        .context("Для edit_file требуется строковый параметр path")?;
    let old_text = args["old_text"]
        .as_str()
        .context("Для edit_file требуется строковый параметр old_text")?;
    let new_text = args["new_text"]
        .as_str()
        .context("Для edit_file требуется строковый параметр new_text")?;
    if old_text.is_empty() {
        bail!("old_text не может быть пустым");
    }
    let (mut file, rel) = guard.open_read_no_follow(raw_path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_WRITE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    drop(file);
    if bytes.len() > MAX_WRITE_BYTES {
        bail!("Файл {rel} превышает лимит правки {MAX_WRITE_BYTES} байт");
    }
    if bytes.contains(&0) {
        bail!("Бинарный файл нельзя править через edit_file: {rel}");
    }
    let original = String::from_utf8(bytes).with_context(|| format!("Файл {rel} не UTF-8"))?;
    let count = original.matches(old_text).count();
    if count == 0 {
        bail!("Фрагмент old_text не найден в {rel}; файл не изменён");
    }
    if count > 1 {
        bail!(
            "Фрагмент old_text встречается {count} раз в {rel}; уточните контекст, файл не изменён"
        );
    }
    let updated = original.replacen(old_text, new_text, 1);
    if updated.len() > MAX_WRITE_BYTES {
        bail!("Итоговый файл превышает лимит записи {MAX_WRITE_BYTES} байт");
    }
    guard.atomic_write_no_follow(&rel, updated.as_bytes())?;
    let diff = unified_diff_preview(&rel, &original, &updated, 60);
    Ok(ToolExecution {
        ok: true,
        summary: format!("edit_file({rel}) · 1 замена"),
        output: format!("Файл {rel} обновлён (1 точечная замена выполнена)."),
        changed_files: vec![rel],
        diff_preview: (!diff.is_empty()).then_some(diff),
    })
}

/// Строит компактный inline unified diff между двумя UTF-8 версиями файла
/// (использует SSE2 `asm_common_prefix_len` на x86_64).
pub fn unified_diff_preview(path: &str, before: &str, after: &str, max_lines: usize) -> String {
    if before == after {
        return String::new();
    }
    let _prefix_hint = asm_accel::asm_common_prefix_len(before.as_bytes(), after.as_bytes());
    let old_lines: Vec<&str> = before.lines().collect();
    let new_lines: Vec<&str> = after.lines().collect();
    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix + prefix < old_lines.len()
        && suffix + prefix < new_lines.len()
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let old_end = old_lines.len().saturating_sub(suffix);
    let new_end = new_lines.len().saturating_sub(suffix);
    let ctx_start = prefix.saturating_sub(2);
    let ctx_old_end = (old_end + 2).min(old_lines.len());
    let ctx_new_end = (new_end + 2).min(new_lines.len());

    let mut out = vec![
        format!("--- a/{path}"),
        format!("+++ b/{path}"),
        format!(
            "@@ -{},{} +{},{} @@",
            ctx_start + 1,
            ctx_old_end.saturating_sub(ctx_start),
            ctx_start + 1,
            ctx_new_end.saturating_sub(ctx_start)
        ),
    ];
    for line in &old_lines[ctx_start..prefix] {
        out.push(format!(" {line}"));
    }
    for line in &old_lines[prefix..old_end] {
        out.push(format!("-{line}"));
    }
    for line in &new_lines[prefix..new_end] {
        out.push(format!("+{line}"));
    }
    for line in &old_lines[old_end..ctx_old_end] {
        out.push(format!(" {line}"));
    }
    if out.len() > max_lines {
        out.truncate(max_lines);
        out.push("… (diff обрезан)".into());
    }
    out.join("\n")
}

struct PatchHunk {
    old_start: usize,
    lines: Vec<(char, String)>,
}

struct FilePatch {
    path: String,
    hunks: Vec<PatchHunk>,
}

fn clean_patch_header_path(raw: &str) -> String {
    let head = raw.split('\t').next().unwrap_or(raw).trim();
    let stripped = head
        .strip_prefix("a/")
        .or_else(|| head.strip_prefix("b/"))
        .unwrap_or(head);
    normalize_rel(stripped)
}

fn parse_unified_patch(patch_text: &str) -> Result<Vec<FilePatch>> {
    let mut patches: Vec<FilePatch> = Vec::new();
    let mut cur_old: Option<String> = None;
    let mut cur_file: Option<FilePatch> = None;
    let mut cur_hunk: Option<PatchHunk> = None;

    for raw_line in patch_text.lines() {
        if let Some(rest) = raw_line.strip_prefix("--- ") {
            if let Some(h) = cur_hunk.take()
                && let Some(f) = cur_file.as_mut()
            {
                f.hunks.push(h);
            }
            if let Some(f) = cur_file.take() {
                patches.push(f);
            }
            cur_old = Some(clean_patch_header_path(rest));
            continue;
        }
        if let Some(rest) = raw_line.strip_prefix("+++ ") {
            let new_p = clean_patch_header_path(rest);
            let target = if new_p == "/dev/null" || new_p == "dev/null" {
                cur_old.take().unwrap_or_default()
            } else {
                cur_old = None;
                new_p
            };
            if target.is_empty() {
                bail!("Патч содержит пустой путь файла");
            }
            cur_file = Some(FilePatch {
                path: target,
                hunks: Vec::new(),
            });
            continue;
        }
        if raw_line.starts_with("@@ ") {
            if let Some(h) = cur_hunk.take()
                && let Some(f) = cur_file.as_mut()
            {
                f.hunks.push(h);
            }
            let header = raw_line.trim_start_matches("@@ ").trim();
            let old_range = header
                .split_whitespace()
                .next()
                .and_then(|s| s.strip_prefix('-'))
                .context("Некорректный заголовок hunk @@")?;
            let old_start: usize = old_range
                .split(',')
                .next()
                .unwrap_or("1")
                .parse()
                .context("Некорректный номер строки hunk")?;
            cur_hunk = Some(PatchHunk {
                old_start,
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(h) = cur_hunk.as_mut() {
            if let Some(first) = raw_line.chars().next()
                && matches!(first, ' ' | '-' | '+')
            {
                h.lines.push((first, raw_line[1..].to_string()));
            } else if raw_line.starts_with("\\ No newline at end of file") {
                continue;
            } else if raw_line.is_empty() {
                h.lines.push((' ', String::new()));
            } else {
                bail!("Некорректная строка в hunk патча: {raw_line}");
            }
        }
    }
    if let Some(h) = cur_hunk.take()
        && let Some(f) = cur_file.as_mut()
    {
        f.hunks.push(h);
    }
    if let Some(f) = cur_file.take() {
        patches.push(f);
    }
    if patches.is_empty() || patches.iter().all(|p| p.hunks.is_empty()) {
        bail!("Патч не содержит валидных секций ---/+++ и @@");
    }
    Ok(patches)
}

fn apply_hunks_to_text(path: &str, original: &str, hunks: &[PatchHunk]) -> Result<String> {
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let had_trailing_nl = original.ends_with('\n') || !original.is_empty();
    let mut delta: isize = 0;

    for (h_idx, hunk) in hunks.iter().enumerate() {
        let old_seq: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|(op, _)| *op == ' ' || *op == '-')
            .map(|(_, s)| s.as_str())
            .collect();
        let new_seq: Vec<String> = hunk
            .lines
            .iter()
            .filter(|(op, _)| *op == ' ' || *op == '+')
            .map(|(_, s)| s.clone())
            .collect();

        if old_seq.is_empty() {
            let pos = hunk
                .old_start
                .saturating_sub(1)
                .saturating_add_signed(delta)
                .min(lines.len());
            lines.splice(pos..pos, new_seq);
            delta += hunk.lines.iter().filter(|(op, _)| *op == '+').count() as isize;
            continue;
        }

        let approx = hunk
            .old_start
            .saturating_sub(1)
            .saturating_add_signed(delta)
            .min(lines.len());
        let match_at = |idx: usize| -> bool {
            idx + old_seq.len() <= lines.len()
                && lines[idx..idx + old_seq.len()]
                    .iter()
                    .map(String::as_str)
                    .eq(old_seq.iter().copied())
        };
        let found = if match_at(approx) {
            Some(approx)
        } else {
            let lo = approx.saturating_sub(25);
            let hi = (approx + 25).min(lines.len());
            (lo..=hi).find(|&i| match_at(i))
        };
        let Some(pos) = found else {
            bail!(
                "Hunk #{} не совпадает с текущим содержимым {path} (около строки {})",
                h_idx + 1,
                hunk.old_start
            );
        };
        let removed = old_seq.len() as isize;
        let inserted = new_seq.len() as isize;
        lines.splice(pos..pos + old_seq.len(), new_seq);
        delta += inserted - removed;
    }

    let mut out = lines.join("\n");
    if had_trailing_nl && !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if out.len() > MAX_WRITE_BYTES {
        bail!("Файл {path} после патча превышает лимит {MAX_WRITE_BYTES} байт");
    }
    Ok(out)
}

/// Безопасно применяет unified diff патч к файлам внутри workspace (включая проверку
/// `owned_paths`, `openat(O_NOFOLLOW)` и предвалидацию всех hunk до записи).
pub fn exec_apply_patch(
    guard: &WorkspaceGuard,
    args: &Value,
    owned_paths: Option<&[String]>,
) -> Result<ToolExecution> {
    let patch_text = args["patch"]
        .as_str()
        .context("Для apply_patch требуется строковый параметр patch")?;
    let file_patches = parse_unified_patch(patch_text)?;

    // Этап 1: проверка всех путей, прав владения и всех hunk до записи любого файла.
    let mut staged: Vec<(String, String, String)> = Vec::new();
    for fp in &file_patches {
        if let Some(owned) = owned_paths
            && !owned.iter().any(|o| is_path_owned(o, &fp.path))
        {
            bail!(
                "apply_patch отклонён: путь «{}» вне owned_paths подзадачи",
                fp.path
            );
        }
        let (rel, original) = match guard.open_read_no_follow(&fp.path) {
            Ok((mut f, rel)) => {
                let mut bytes = Vec::new();
                f.by_ref()
                    .take((MAX_WRITE_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > MAX_WRITE_BYTES {
                    bail!("Файл {rel} превышает лимит {MAX_WRITE_BYTES} байт");
                }
                if bytes.contains(&0) {
                    bail!("Бинарный файл нельзя патчить: {rel}");
                }
                let txt =
                    String::from_utf8(bytes).with_context(|| format!("Файл {rel} не UTF-8"))?;
                (rel, txt)
            }
            Err(_) => {
                let is_pure_addition = fp
                    .hunks
                    .iter()
                    .all(|h| h.lines.iter().all(|(op, _)| *op == '+'));
                if !is_pure_addition {
                    let _ = guard.open_read_no_follow(&fp.path)?;
                }
                (fp.path.clone(), String::new())
            }
        };
        let updated = apply_hunks_to_text(&rel, &original, &fp.hunks)?;
        staged.push((rel, original, updated));
    }

    // Этап 2: атомарная запись через openat(O_NOFOLLOW) + renameat.
    let mut changed = Vec::new();
    let mut diffs = Vec::new();
    for (rel, original, updated) in staged {
        let written_rel = guard.atomic_write_no_follow(&rel, updated.as_bytes())?;
        let diff = unified_diff_preview(&written_rel, &original, &updated, 50);
        if !diff.is_empty() {
            diffs.push(diff);
        }
        changed.push(written_rel);
    }

    Ok(ToolExecution {
        ok: true,
        summary: format!(
            "apply_patch → файлов: {} ({})",
            changed.len(),
            changed.join(", ")
        ),
        output: format!("Патч применён к файлам: {}.", changed.join(", ")),
        changed_files: changed,
        diff_preview: (!diffs.is_empty()).then(|| diffs.join("\n")),
    })
}

/// Показывает доступные локальные навыки (`SKILL.md`) в `.kaic/skills` workspace и системных каталогах.
pub fn exec_list_skills(guard: &WorkspaceGuard) -> Result<ToolExecution> {
    let mut entries = Vec::new();
    if let Ok((ws_skills_dir, _)) = guard.resolve_existing(".kaic/skills")
        && ws_skills_dir.is_dir()
        && let Ok(rd) = fs::read_dir(&ws_skills_dir)
    {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let rel_candidate = if entry.path().is_dir() {
                format!(".kaic/skills/{name}/SKILL.md")
            } else if name.ends_with(".md") {
                format!(".kaic/skills/{name}")
            } else {
                continue;
            };
            if guard.open_read_no_follow(&rel_candidate).is_ok() {
                entries.push(format!("[workspace] {name}"));
            }
        }
    }
    for s in crate::desktop::skills::discover() {
        entries.push(format!("[{}] {} — {}", s.source, s.id, s.description));
    }
    let count = entries.len();
    let output = if entries.is_empty() {
        "Локальные навыки (SKILL.md) не найдены.".into()
    } else {
        entries.join("\n")
    };
    Ok(ToolExecution {
        ok: true,
        summary: format!("list_skills → {count}"),
        output,
        changed_files: vec![],
        diff_preview: None,
    })
}

/// Читает содержимое `SKILL.md` из `.kaic/skills` workspace (через `WorkspaceGuard`) или глобального реестра.
pub fn exec_read_skill(guard: &WorkspaceGuard, args: &Value) -> Result<ToolExecution> {
    let id = args["id"]
        .as_str()
        .context("Для read_skill требуется строковый параметр id")?
        .trim();
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
        bail!("Некорректный идентификатор навыка: {id}");
    }
    for rel in [
        format!(".kaic/skills/{id}/SKILL.md"),
        format!(".kaic/skills/{id}.md"),
    ] {
        if let Ok((mut f, checked_rel)) = guard.open_read_no_follow(&rel) {
            let mut text = String::new();
            f.by_ref()
                .take(MAX_READ_BYTES as u64)
                .read_to_string(&mut text)?;
            return Ok(ToolExecution {
                ok: true,
                summary: format!("read_skill({id}) · workspace"),
                output: format!("# {checked_rel}\n{text}"),
                changed_files: vec![],
                diff_preview: None,
            });
        }
    }
    let val = crate::desktop::skills::read_json(id)?;
    let content = val["content"].as_str().unwrap_or("");
    Ok(ToolExecution {
        ok: true,
        summary: format!(
            "read_skill({id}) · {}",
            val["source"].as_str().unwrap_or("local")
        ),
        output: content.to_string(),
        changed_files: vec![],
        diff_preview: None,
    })
}

/// Запускает локальную команду проверки/сборки в собственной группе процессов (`setpgid(0, 0)`).
///
/// ОГРАНИЧЕНИЯ:
/// 1. Это НЕ OS-sandbox: команды класса `UserCodeExecution` (`cargo`, `make`, `pytest`, `npm`, `unittest`)
///    исполняются с правами текущего пользователя.
/// 2. Завершение по отмене/таймауту использует `kill(-pgid, SIGKILL)`, что гарантированно завершает
///    дочерний процесс и всех потомков в той же группе процессов, но НЕ покрывает потомков,
///    которые явно отделились в новую сессию (`setsid()` / `start_new_session=True`).
///    Для перехвата отделившихся демонов требуется cgroup v2 (`cgroup.kill`) или PID-namespace.
pub fn exec_run_check(
    guard: &WorkspaceGuard,
    program: &str,
    args: &[String],
    cancel: &Cancel,
) -> Result<ToolExecution> {
    let validated = guard.validate_command(program, args)?;
    let mut cmd = Command::new(&validated.program);
    cmd.args(&validated.args)
        .current_dir(guard.root())
        .env_clear()
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONNOUSERSITE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in [
        "PATH",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "TERM",
        "LANG",
        "LC_ALL",
    ] {
        if let Some(val) = std::env::var_os(var) {
            cmd.env(var, val);
        }
    }
    // Собственная группа процессов (pgid = pid) + PR_SET_PDEATHSIG, чтобы отмена
    // мгновенно завершала и дочерний процесс, и все его подпроцессы-внуки.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let _ = crate::platform::set_parent_death_signal();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Не удалось запустить проверку «{}»", validated.program))?;
    let pgid = child.id() as libc::pid_t;
    let stdout = child.stdout.take().context("Нет stdout проверки")?;
    let stderr = child.stderr.take().context("Нет stderr проверки")?;
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout
            .take(MAX_COMMAND_OUTPUT_BYTES as u64)
            .read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr
            .take(MAX_COMMAND_OUTPUT_BYTES as u64)
            .read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        if cancel.is_cancelled() {
            kill_process_group(pgid, &mut child);
            // Не ждём бесконечно EOF на stdout/stderr: отделившийся через setsid() потомок
            // мог унаследовать pipe fd и заблокировать join() на минуты.
            drop(out_reader);
            drop(err_reader);
            bail!(
                "Выполнение проверки остановлено пользователем; группа дочерних процессов завершена (потомки в новой сессии setsid() не покрываются kill(-pgid))"
            );
        }
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() > Duration::from_secs(COMMAND_TIMEOUT_SECS) {
            kill_process_group(pgid, &mut child);
            drop(out_reader);
            drop(err_reader);
            bail!(
                "Проверка «{}» превысила таймаут {} с и была остановлена",
                validated.program,
                COMMAND_TIMEOUT_SECS
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let out_bytes = out_reader.join().unwrap_or_default();
    let err_bytes = err_reader.join().unwrap_or_default();
    let stdout_text = String::from_utf8_lossy(&out_bytes);
    let stderr_text = String::from_utf8_lossy(&err_bytes);
    let code = status.code().unwrap_or(-1);
    let mut combined = format!("exit_code={code}\n");
    if !stdout_text.trim().is_empty() {
        combined.push_str("stdout:\n");
        combined.push_str(stdout_text.trim_end());
        combined.push('\n');
    }
    if !stderr_text.trim().is_empty() {
        combined.push_str("stderr:\n");
        combined.push_str(stderr_text.trim_end());
        combined.push('\n');
    }
    let ok = status.success();
    let cmdline = if validated.args.is_empty() {
        validated.program.clone()
    } else {
        format!("{} {}", validated.program, validated.args.join(" "))
    };
    Ok(ToolExecution {
        ok,
        summary: format!(
            "run_check({cmdline}) → {}",
            if ok {
                format!("OK (exit {code})")
            } else {
                format!("ошибка (exit {code})")
            }
        ),
        output: combined,
        changed_files: vec![],
        diff_preview: None,
    })
}

pub fn kill_process_group(pgid: libc::pid_t, child: &mut std::process::Child) {
    if pgid > 0 {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
