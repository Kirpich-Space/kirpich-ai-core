//! Кроссплатформенные POSIX-адаптеры процесса и сокетов для Linux и FreeBSD.
//! На FreeBSD логика реализована по официальным API (`procctl`/`sockstat`),
//! но НЕ тестировалась на живой FreeBSD-машине — в отчётах честно маркируется как unverified/unavailable.

use anyhow::Result;

/// Назначает дочернему процессу сигнал `SIGTERM` при завершении родителя.
/// Безопасно вызывать внутри `CommandExt::pre_exec` после `fork()`.
///
/// # Safety
/// Вызывает только async-signal-safe системные вызовы ОС (`prctl` / `procctl`).
pub unsafe fn set_parent_death_signal() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `PR_SET_PDEATHSIG` с `SIGTERM` корректен в дочернем процессе после `fork()`.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "freebsd")]
    {
        let sig: libc::c_int = libc::SIGTERM;
        // SAFETY: `PROC_PDEATHSIG_CTL` на `P_PID, 0` (текущий процесс) устанавливает сигнал завершения при смерти родителя.
        if unsafe {
            libc::procctl(
                libc::P_PID,
                0,
                libc::PROC_PDEATHSIG_CTL,
                (&sig as *const libc::c_int) as *mut libc::c_void,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        Ok(())
    }
}

/// Проверяет, жив ли процесс с данным `pid` (не полагается жёстко на `/proc`).
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: `kill(pid, 0)` только проверяет существование процесса/сигнальных прав.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            // Исключаем зомби-процесс на Linux `/proc/<pid>/stat` при наличии procfs.
            #[cfg(target_os = "linux")]
            if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status"))
                && status.lines().any(|l| l.starts_with("State:\tZ"))
            {
                return false;
            }
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Проверяет, что локальный TCP LISTEN-сокет на `127.0.0.1:<port>` принадлежит процессу `pid`.
pub fn listener_owned_by(pid: u32, port: u16) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        use anyhow::{Context, bail};
        let table = std::fs::read_to_string("/proc/net/tcp")
            .context("Не удалось проверить владельца loopback-сокета")?;
        let local = format!("0100007F:{port:04X}");
        let inodes = table
            .lines()
            .skip(1)
            .filter_map(|l| {
                let f = l.split_whitespace().collect::<Vec<_>>();
                if f.len() > 9 && f[1] == local && f[3] == "0A" {
                    Some(f[9].to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if inodes.is_empty() {
            return Ok(false);
        }
        let fds = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => {
                bail!("Не удалось проверить fd собственного процесса; соединение отклонено")
            }
        };
        for fd in fds.flatten() {
            if let Ok(link) = std::fs::read_link(fd.path()) {
                let target = link.to_string_lossy();
                if inodes
                    .iter()
                    .any(|inode| target == format!("socket:[{inode}]"))
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    #[cfg(target_os = "freebsd")]
    {
        use anyhow::{Context, bail};
        let output = std::process::Command::new("sockstat")
            .args(["-4", "-l", "-P", "tcp", "-p", &port.to_string()])
            .output()
            .context("Не удалось выполнить sockstat на FreeBSD для проверки владельца сокета")?;
        if !output.status.success() {
            bail!("sockstat завершился с ошибкой; владение сокетом не подтверждено");
        }
        let pid_str = pid.to_string();
        let port_suffix = format!(":{port}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // USER COMMAND PID FD PROTO LOCAL_ADDRESS FOREIGN_ADDRESS
            if cols.len() >= 6 && cols[2] == pid_str && cols[5].ends_with(&port_suffix) {
                return Ok(true);
            }
        }
        Ok(false)
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _ = (pid, port);
        anyhow::bail!("Проверка владения loopback-сокетом не реализована для этой ОС")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_alive_reports_self_and_rejects_pid_zero() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(0));
    }
}
