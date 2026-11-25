//! Ограниченный запуск собственных служебных процессов без shell.
use anyhow::{Context, Result, bail};
use std::{
    io::Read,
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
pub fn capture(
    mut command: Command,
    timeout: Duration,
    limit: usize,
) -> Result<(ExitStatus, Vec<u8>)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Не удалось запустить локальный валидатор")?;
    let pipe = child.stdout.take().context("Нет stdout валидатора")?;
    let reader = std::thread::spawn(move || {
        let mut b = vec![];
        pipe.take((limit + 1) as u64).read_to_end(&mut b).map(|_| b)
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            bail!("Истёк срок проверки локальным валидатором");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("Чтение результата валидатора прервано"))??;
    if bytes.len() > limit {
        bail!("Ответ валидатора превышает лимит");
    }
    Ok((status, bytes))
}
