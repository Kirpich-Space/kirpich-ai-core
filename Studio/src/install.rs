//! Не вызывается при обычном запуске: установка только явной --install-local.
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
};
pub fn desktop_entry(binary: &Path) -> Result<String> {
    let path = binary.to_str().context("Путь установки не UTF-8")?;
    if path.chars().any(char::is_control) {
        bail!("Путь содержит управляющие символы");
    }
    let path = path
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
        .replace('%', "%%");
    Ok(format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName=KAIC Studio\nComment=Локальные диалоги, анализ текста и генерация медиа\nExec=\"{path}\"\nTerminal=false\nCategories=Development;Utility;\nStartupWMClass=KAIC Studio\n"
    ))
}
pub fn install_local() -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .context("HOME должен быть абсолютным путём")?;
    let prefix = home.join(".local");
    let bin = prefix.join("bin/kaic-studio");
    let desktop = prefix.join("share/applications/kaic-studio.desktop");
    if bin.exists() || desktop.exists() {
        bail!(
            "Файлы KAIC Studio уже существуют в ~/.local. Они сохранены; автоматическая перезапись не выполняется."
        );
    }
    let entry = desktop_entry(&bin)?;
    fs::create_dir_all(bin.parent().unwrap())?;
    fs::create_dir_all(desktop.parent().unwrap())?;
    let mut source = fs::File::open(std::env::current_exe()?)?;
    let mut target = OpenOptions::new().create_new(true).write(true).open(&bin)?;
    std::io::copy(&mut source, &mut target)?;
    target.sync_all()?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&desktop)?;
    file.write_all(entry.as_bytes())?;
    file.sync_all()?;
    println!(
        "Установлено: {}\nЯрлык меню: {}\nЗначок на рабочем столе не создаётся.",
        bin.display(),
        desktop.display()
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_exec_is_quoted_and_literal() {
        let s = desktop_entry(Path::new("/tmp/Папка с пробелом/100%/kaic-studio")).unwrap();
        assert!(s.contains("Exec=\"/tmp/Папка с пробелом/100%%/kaic-studio\""));
        assert!(!s.contains("sh -c"));
        assert!(desktop_entry(Path::new("/tmp/a\nb")).is_err());
    }
}
