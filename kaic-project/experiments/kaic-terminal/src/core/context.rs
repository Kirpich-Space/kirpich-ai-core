use std::path::{Path, PathBuf};

use anyhow::Result;

const IGNORED_DIRS: &[&str] = &["target", "node_modules", ".git", "dist", "build"];

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub language: String,
    pub build_file: Option<String>,
    pub file_count: usize,
    pub top_level_entries: Vec<String>,
}

/// Определяет язык проекта по наличию известных маркер-файлов, строит
/// плоский верхнеуровневый список и считает файлы рекурсивно (с разумным
/// ограничением по глубине, чтобы не зависнуть на огромных деревьях).
/// Никаких внешних крейтов (walkdir и т.п.) — простой std::fs::read_dir.
pub fn detect(root: &Path) -> Result<ProjectInfo> {
    let markers: &[(&str, &str)] = &[
        ("Cargo.toml", "Rust"),
        ("CMakeLists.txt", "C/C++ (CMake)"),
        ("Makefile", "C/C++ (Make)"),
        ("package.json", "Node.js/JavaScript"),
        ("pyproject.toml", "Python"),
        ("requirements.txt", "Python"),
        ("go.mod", "Go"),
        ("pom.xml", "Java (Maven)"),
        ("build.gradle", "Java/Kotlin (Gradle)"),
    ];

    let mut language = "неизвестно".to_string();
    let mut build_file = None;
    for (marker, lang) in markers {
        if root.join(marker).is_file() {
            language = lang.to_string();
            build_file = Some(marker.to_string());
            break;
        }
    }

    let mut top_level_entries = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(root) {
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || IGNORED_DIRS.contains(&name.as_str()) {
                continue;
            }
            let suffix = if entry.path().is_dir() { "/" } else { "" };
            top_level_entries.push(format!("{name}{suffix}"));
        }
    }
    top_level_entries.sort();

    let file_count = count_files(root, 0);

    Ok(ProjectInfo {
        root: root.to_path_buf(),
        language,
        build_file,
        file_count,
        top_level_entries,
    })
}

fn count_files(dir: &Path, depth: usize) -> usize {
    const MAX_DEPTH: usize = 8;
    if depth > MAX_DEPTH {
        return 0;
    }
    let mut count = 0;
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || IGNORED_DIRS.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            count += count_files(&path, depth + 1);
        } else {
            count += 1;
        }
    }
    count
}

impl ProjectInfo {
    pub fn summary(&self) -> String {
        format!(
            "Проект: {}\nЯзык: {}\nФайл сборки: {}\nВсего файлов (без .git/target/node_modules/dist/build): {}\nВерхний уровень: {}",
            self.root.display(),
            self.language,
            self.build_file.clone().unwrap_or_else(|| "не найден".into()),
            self.file_count,
            if self.top_level_entries.is_empty() {
                "(пусто)".to_string()
            } else {
                self.top_level_entries.join(", ")
            }
        )
    }
}
