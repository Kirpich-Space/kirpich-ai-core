//! Загрузка локальных файлов SKILLS (SKILL.md / *.md) из каталогов пользователя.
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const MAX_SKILL_BYTES: u64 = 65_536;

#[derive(Clone, Debug, Serialize)]
pub struct SkillSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: String,
    pub bytes: u64,
    #[serde(skip)]
    path: PathBuf,
}

fn roots() -> Vec<(&'static str, PathBuf)> {
    let mut out = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        out.push(("KAIC Studio", xdg.join("kaic-studio/skills")));
    } else if let Some(ref h) = home {
        out.push(("KAIC Studio", h.join(".local/share/kaic-studio/skills")));
    }
    if let Some(ref h) = home {
        out.push(("Gemini / Antigravity", h.join(".gemini/config/skills")));
        out.push(("Claude", h.join(".claude/skills")));
    }
    out
}

fn parse_frontmatter(raw: &str, fallback_name: &str) -> (String, String) {
    let mut name = fallback_name.to_string();
    let mut desc = String::new();
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let header = &rest[..end];
            let mut in_desc = false;
            for line in header.lines() {
                if let Some(v) = line.strip_prefix("name:") {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        name = v.to_string();
                    }
                    in_desc = false;
                } else if let Some(v) = line.strip_prefix("description:") {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    if v == "|" || v == ">" || v.is_empty() {
                        in_desc = true;
                    } else {
                        desc = v.to_string();
                        in_desc = false;
                    }
                } else if in_desc && (line.starts_with(' ') || line.starts_with('\t')) {
                    if !desc.is_empty() {
                        desc.push(' ');
                    }
                    desc.push_str(line.trim());
                } else {
                    in_desc = false;
                }
            }
        }
    }
    if desc.is_empty() {
        desc = raw
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("---") && !l.starts_with('#'))
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
    } else {
        desc = desc.chars().take(180).collect();
    }
    (name, desc)
}

fn scan_dir(source: &str, dir: &Path, acc: &mut BTreeMap<String, SkillSummary>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let (id, file_path) = if meta.is_dir() {
            let candidate = path.join("SKILL.md");
            if !candidate.is_file() {
                continue;
            }
            let Some(id) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            (id.to_string(), candidate)
        } else if meta.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            let Some(stem) = path.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };
            (stem.to_string(), path)
        } else {
            continue;
        };
        if acc.contains_key(&id) || id.starts_with('.') {
            continue;
        }
        let Ok(fmeta) = std::fs::metadata(&file_path) else {
            continue;
        };
        let bytes = fmeta.len();
        if bytes == 0 || bytes > MAX_SKILL_BYTES {
            continue;
        }
        let raw = std::fs::read_to_string(&file_path).unwrap_or_default();
        let (name, description) = parse_frontmatter(&raw, &id);
        acc.insert(
            id.clone(),
            SkillSummary {
                id,
                name,
                description,
                source: source.to_string(),
                bytes,
                path: file_path,
            },
        );
    }
}

pub fn discover() -> Vec<SkillSummary> {
    let mut acc = BTreeMap::new();
    for (source, dir) in roots() {
        scan_dir(source, &dir, &mut acc);
    }
    acc.into_values().collect()
}

pub fn list_json() -> Value {
    json!({ "skills": discover() })
}

pub fn read_json(id: &str) -> Result<Value> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
        bail!("Некорректный идентификатор скилла");
    }
    let skill = discover()
        .into_iter()
        .find(|s| s.id == id)
        .context("Скилл не найден в локальных каталогах")?;
    let meta = std::fs::metadata(&skill.path)?;
    if meta.len() > MAX_SKILL_BYTES {
        bail!("Файл скилла превышает 64 КиБ");
    }
    let content = std::fs::read_to_string(&skill.path)?;
    Ok(json!({
        "id": skill.id,
        "name": skill.name,
        "description": skill.description,
        "source": skill.source,
        "bytes": meta.len(),
        "content": content,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yaml_frontmatter_and_rejects_traversal() {
        let sample = "---\nname: rust-helper\ndescription: \"Помощник по Rust\"\n---\n# Body\n";
        let (name, desc) = parse_frontmatter(sample, "fallback");
        assert_eq!(name, "rust-helper");
        assert_eq!(desc, "Помощник по Rust");
        assert!(read_json("../etc/passwd").is_err());
        assert!(read_json("a/b").is_err());
    }
}
