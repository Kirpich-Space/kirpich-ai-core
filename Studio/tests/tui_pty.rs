//! Интеграционные тесты реального бинарника `kaic-studio tui` через настоящий POSIX PTY (`openpty` + `TIOCSWINSZ`).
//! Проверяет inline-архитектуру (без `\x1b[?1049h` alt-screen), нативный scrollback,
//! приветственную секцию с официальным знаком KAIC (без Кича), интерактивные меню команд и выбора модели,
//! реальное выполнение инструмента `apply_patch` с цветным diff, потоковый ответ с подсветкой синтаксиса,
//! сворачивание инструментов (`Ctrl+O`) и адаптацию при нескольких размерах терминала (`120×36`, `80×24`, `60×20`).
//! Все сетевые обращения идут строго на локальный loopback HTTP-мок (GPU не используется).

#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::io::{FromRawFd, RawFd},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct PtySession {
    master_fd: RawFd,
    child: Child,
}

impl PtySession {
    fn spawn(
        cols: u16,
        rows: u16,
        workspace: &Path,
        data_dir: &Path,
        endpoint: &str,
        extra_env: &[(&str, &str)],
        extra_args: &[&str],
    ) -> Self {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        assert_eq!(rc, 0, "openpty failed");

        unsafe {
            let flags = libc::fcntl(master, libc::F_GETFL);
            libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let bin = env!("CARGO_BIN_EXE_kaic-studio");
        let mut cmd = Command::new(bin);
        cmd.arg("tui")
            .arg("--workspace")
            .arg(workspace)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--provider")
            .arg("ollama")
            .arg("--endpoint")
            .arg(endpoint)
            .arg("--model")
            .arg("qwen2.5-coder:14b");
        for a in extra_args {
            cmd.arg(a);
        }

        cmd.env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env_remove("NO_COLOR")
            .env_remove("KITTY_WINDOW_ID")
            .env_remove("TERM_PROGRAM")
            .env_remove("SSH_CONNECTION")
            .env_remove("SSH_TTY")
            .env_remove("TMUX")
            .env_remove("KAIC_TUI_GRAPHICS");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let slave_in = unsafe { Stdio::from_raw_fd(libc::dup(slave)) };
        let slave_out = unsafe { Stdio::from_raw_fd(libc::dup(slave)) };
        let slave_err = unsafe { Stdio::from_raw_fd(libc::dup(slave)) };
        cmd.stdin(slave_in).stdout(slave_out).stderr(slave_err);

        unsafe {
            cmd.pre_exec(move || {
                libc::setsid();
                let _ = libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }

        let child = cmd.spawn().expect("spawn kaic-studio tui in PTY");
        unsafe {
            libc::close(slave);
        }

        Self {
            master_fd: master,
            child,
        }
    }

    fn write_input(&self, bytes: &[u8]) {
        let mut written = 0usize;
        while written < bytes.len() {
            let n = unsafe {
                libc::write(
                    self.master_fd,
                    bytes[written..].as_ptr() as *const libc::c_void,
                    bytes.len() - written,
                )
            };
            if n > 0 {
                written += n as usize;
            } else {
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master_fd, libc::TIOCSWINSZ, &ws);
        }
    }

    fn read_available(&self, dst: &mut Vec<u8>, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 4096];
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remain_ms = (deadline - now).as_millis().min(50) as i32;
            let mut pfd = libc::pollfd {
                fd: self.master_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, remain_ms) };
            if rc > 0 && (pfd.revents & libc::POLLIN) != 0 {
                let n = unsafe {
                    libc::read(
                        self.master_fd,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n > 0 {
                    dst.extend_from_slice(&buf[..n as usize]);
                } else {
                    break;
                }
            } else if rc > 0 && (pfd.revents & (libc::POLLHUP | libc::POLLERR)) != 0 {
                break;
            }
        }
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            thread::sleep(Duration::from_millis(15));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        unsafe {
            libc::close(self.master_fd);
        }
    }
}

/// Лёгкий эмулятор основного экрана терминала + scrollback для точной проверки inline-отрисовки.
struct VirtualTerm {
    cols: usize,
    rows: usize,
    scrollback: Vec<String>,
    grid: Vec<Vec<char>>,
    cur_row: usize,
    cur_col: usize,
    saw_alt_screen: bool,
    raw_log: String,
    pending: Vec<u8>,
}

impl VirtualTerm {
    fn new(cols: u16, rows: u16) -> Self {
        let c = cols as usize;
        let r = rows as usize;
        Self {
            cols: c,
            rows: r,
            scrollback: Vec::new(),
            grid: vec![vec![' '; c]; r],
            cur_row: 0,
            cur_col: 0,
            saw_alt_screen: false,
            raw_log: String::new(),
            pending: Vec::new(),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let new_c = cols as usize;
        let new_r = rows as usize;
        let mut new_grid = vec![vec![' '; new_c]; new_r];
        for r in 0..self.rows.min(new_r) {
            for c in 0..self.cols.min(new_c) {
                new_grid[r][c] = self.grid[r][c];
            }
        }
        self.cols = new_c;
        self.rows = new_r;
        self.grid = new_grid;
        self.cur_row = self.cur_row.min(new_r.saturating_sub(1));
        self.cur_col = self.cur_col.min(new_c.saturating_sub(1));
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.raw_log.push_str(&String::from_utf8_lossy(bytes));
        if self.raw_log.contains("\x1b[?1049h") || self.raw_log.contains("\x1b[?1049l") {
            self.saw_alt_screen = true;
        }
        self.pending.extend_from_slice(bytes);
        let data = std::mem::take(&mut self.pending);
        let mut i = 0usize;
        while i < data.len() {
            let b = data[i];
            if b == 0x1b {
                if i + 1 >= data.len() {
                    self.pending.extend_from_slice(&data[i..]);
                    break;
                }
                match data[i + 1] {
                    b'[' => {
                        let mut j = i + 2;
                        while j < data.len() && !(0x40..=0x7e).contains(&data[j]) {
                            j += 1;
                        }
                        if j >= data.len() {
                            self.pending.extend_from_slice(&data[i..]);
                            break;
                        }
                        let params = String::from_utf8_lossy(&data[i + 2..j]).to_string();
                        let final_b = data[j];
                        self.apply_csi(&params, final_b);
                        i = j + 1;
                        continue;
                    }
                    b']' | b'_' | b'P' => {
                        let mut j = i + 2;
                        let mut found = None;
                        while j < data.len() {
                            if data[j] == 0x07 {
                                found = Some(j + 1);
                                break;
                            }
                            if data[j] == 0x1b && j + 1 < data.len() && data[j + 1] == b'\\' {
                                found = Some(j + 2);
                                break;
                            }
                            j += 1;
                        }
                        if let Some(next_i) = found {
                            i = next_i;
                            continue;
                        } else {
                            self.pending.extend_from_slice(&data[i..]);
                            break;
                        }
                    }
                    _ => {
                        i += 2;
                        continue;
                    }
                }
            }
            if b == b'\r' {
                self.cur_col = 0;
                i += 1;
                continue;
            }
            if b == b'\n' {
                if self.cur_row + 1 < self.rows {
                    self.cur_row += 1;
                } else {
                    let evicted: String = self.grid.remove(0).into_iter().collect();
                    self.scrollback.push(evicted.trim_end().to_string());
                    self.grid.push(vec![' '; self.cols]);
                }
                i += 1;
                continue;
            }
            if b < 0x20 {
                i += 1;
                continue;
            }
            let seq_len = if b < 0x80 {
                1
            } else if b & 0xE0 == 0xC0 {
                2
            } else if b & 0xF0 == 0xE0 {
                3
            } else if b & 0xF8 == 0xF0 {
                4
            } else {
                i += 1;
                continue;
            };
            if i + seq_len > data.len() {
                self.pending.extend_from_slice(&data[i..]);
                break;
            }
            if let Ok(s) = std::str::from_utf8(&data[i..i + seq_len])
                && let Some(ch) = s.chars().next()
            {
                if self.cur_col < self.cols {
                    self.grid[self.cur_row][self.cur_col] = ch;
                    self.cur_col += 1;
                }
            }
            i += seq_len;
        }
    }

    fn apply_csi(&mut self, params: &str, final_b: u8) {
        let num = |default: usize| -> usize {
            params
                .split(';')
                .next()
                .and_then(|p| p.parse::<usize>().ok())
                .unwrap_or(default)
        };
        match final_b {
            b'A' => {
                let n = num(1).max(1);
                self.cur_row = self.cur_row.saturating_sub(n);
            }
            b'B' => {
                let n = num(1).max(1);
                self.cur_row = (self.cur_row + n).min(self.rows.saturating_sub(1));
            }
            b'H' | b'f' => {
                let mut parts = params.split(';');
                let r = parts
                    .next()
                    .and_then(|p| p.parse::<usize>().ok())
                    .unwrap_or(1)
                    .saturating_sub(1);
                let c = parts
                    .next()
                    .and_then(|p| p.parse::<usize>().ok())
                    .unwrap_or(1)
                    .saturating_sub(1);
                self.cur_row = r.min(self.rows.saturating_sub(1));
                self.cur_col = c.min(self.cols.saturating_sub(1));
            }
            b'J' => {
                let mode = num(0);
                if mode == 2 {
                    for row in &mut self.grid {
                        row.fill(' ');
                    }
                } else if mode == 0 {
                    for c in self.cur_col..self.cols {
                        self.grid[self.cur_row][c] = ' ';
                    }
                    for r in (self.cur_row + 1)..self.rows {
                        self.grid[r].fill(' ');
                    }
                }
            }
            b'K' => {
                let mode = num(0);
                if mode == 2 {
                    self.grid[self.cur_row].fill(' ');
                } else if mode == 0 {
                    for c in self.cur_col..self.cols {
                        self.grid[self.cur_row][c] = ' ';
                    }
                }
            }
            _ => {}
        }
    }

    fn visible_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .grid
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines
    }

    fn full_lines(&self) -> Vec<String> {
        let mut all = self.scrollback.clone();
        all.extend(self.visible_lines());
        all
    }
}

fn spawn_mock_ollama() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock ollama");
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = stop.clone();
    let turn_counter = Arc::new(AtomicUsize::new(0));

    let handle = thread::spawn(move || {
        while !stop_bg.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let tc = turn_counter.fetch_add(1, Ordering::SeqCst);
                    handle_mock_ollama_conn(stream, tc);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    (format!("http://{addr}"), stop, handle)
}

fn handle_mock_ollama_conn(mut stream: TcpStream, turn_idx: usize) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut req_buf = vec![0u8; 65536];
    let _ = stream.read(&mut req_buf);

    let header = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.flush();

    let send_chunk = |s: &mut TcpStream, line: &str| {
        let payload = format!("{line}\n");
        let frame = format!("{:x}\r\n{payload}\r\n", payload.len());
        let _ = s.write_all(frame.as_bytes());
        let _ = s.flush();
    };

    if turn_idx == 0 {
        // Небольшая задержка (~220 мс), чтобы спиннер активности реально сделал несколько кадров
        thread::sleep(Duration::from_millis(220));
        let patch = "--- a/calc.rs\n+++ b/calc.rs\n@@ -1,3 +1,3 @@\n pub fn add(a: i32, b: i32) -> i32 {\n-    a - b\n+    a + b\n }\n";
        let tc_json = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "apply_patch",
                        "arguments": { "patch": patch }
                    }
                }]
            },
            "done": false
        });
        send_chunk(&mut stream, &tc_json.to_string());
        send_chunk(
            &mut stream,
            r#"{"message":{"role":"assistant","content":""},"done":true}"#,
        );
    } else {
        let chunks = [
            "Исправил ошибку знака в `calc.rs`:\n\n",
            "```rust\npub fn add(a: i32, b: i32) -> i32 {\n",
            "    a + b\n}\n```\n",
            "Патч применён и проверен.",
        ];
        for part in chunks {
            thread::sleep(Duration::from_millis(25));
            let line = serde_json::json!({
                "message": { "role": "assistant", "content": part },
                "done": false
            });
            send_chunk(&mut stream, &line.to_string());
        }
        send_chunk(
            &mut stream,
            r#"{"message":{"role":"assistant","content":""},"done":true}"#,
        );
    }
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

fn drain_until(pty: &PtySession, vt: &mut VirtualTerm, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut chunk = Vec::new();
    while Instant::now() < deadline {
        chunk.clear();
        pty.read_available(&mut chunk, Duration::from_millis(40));
        if !chunk.is_empty() {
            vt.feed(&chunk);
        }
        let combined = vt.full_lines().join("\n");
        if combined.contains(needle) {
            // Догоняем остаток кадра
            chunk.clear();
            pty.read_available(&mut chunk, Duration::from_millis(80));
            if !chunk.is_empty() {
                vt.feed(&chunk);
            }
            return true;
        }
    }
    false
}

#[test]
fn real_pty_inline_welcome_menus_tool_diff_and_scrollback_across_dimensions() {
    let (endpoint, stop_srv, srv_handle) = spawn_mock_ollama();

    // 1. Проверяем компактное приветствие и композер на трёх реальных размерах PTY: 120×36, 80×24, 60×20
    for (cols, rows) in [(120u16, 36u16), (80, 24), (60, 20)] {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let data = tmp.path().join("data");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&data).unwrap();

        let mut pty = PtySession::spawn(cols, rows, &ws, &data, &endpoint, &[], &[]);
        let mut vt = VirtualTerm::new(cols, rows);
        assert!(
            drain_until(&pty, &mut vt, "❯", Duration::from_secs(5)),
            "PTY {cols}x{rows} did not render welcome + composer; got:\n{}",
            vt.visible_lines().join("\n")
        );

        let vis = vt.visible_lines();
        let joined = vis.join("\n");
        println!(
            "\n=== PTY CAPTURE WELCOME ({cols}x{rows}) ===\n{joined}\n=========================================="
        );

        assert!(!vt.saw_alt_screen, "TUI must NOT enter alt-screen (?1049h)");
        assert_eq!(
            joined.matches("KAIC").count(),
            1,
            "KAIC label must appear exactly once in {cols}x{rows}"
        );
        assert!(
            !joined.to_lowercase().contains("кич") && !joined.to_lowercase().contains("kich"),
            "Kich must be completely absent from TUI"
        );
        assert!(
            !joined.contains("f:rw") && !joined.contains("[DYN:OFF]") && !joined.contains("ctx:—"),
            "Cryptic abbreviations must not appear"
        );
        // Компактная секция вверху: композер ❯ должен быть в первых 10 строках (никакого вертикального центрирования!)
        let composer_row = vis
            .iter()
            .position(|l| l.contains('❯'))
            .expect("composer line missing");
        assert!(
            composer_row <= 8,
            "Composer row {composer_row} too low on {cols}x{rows}; expected compact top layout"
        );
        // Официальный акцент kiln-500 (#D2602C -> 210;96;44) присутствует в сыром выводе
        assert!(vt.raw_log.contains("210;96;44"));

        // Проверяем чистый выход по /quit
        pty.write_input(b"/quit\r");
        let code = pty.wait_exit(Duration::from_secs(5));
        assert_eq!(
            code,
            Some(0),
            "kaic-studio tui did not exit cleanly on /quit"
        );
    }

    // 2. Полный интерактивный сценарий в PTY 100×30:
    //    - Меню slash-команд (`/`)
    //    - Интерактивное меню выбора модели (`/model` -> фильтрация -> выбор -> Esc/Enter)
    //    - Реальный диалог с задержкой (спиннер) + инструмент `apply_patch` + цветной diff + подсветка Rust + `Ctrl+O` + resize (72×22) + `/quit`
    {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let data = tmp.path().join("data");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&data).unwrap();
        fs::write(
            ws.join("calc.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .unwrap();

        let mut pty = PtySession::spawn(100, 30, &ws, &data, &endpoint, &[], &[]);
        let mut vt = VirtualTerm::new(100, 30);
        assert!(drain_until(&pty, &mut vt, "❯", Duration::from_secs(5)));

        // A. Открываем меню slash-команд `/`
        pty.write_input(b"/");
        assert!(drain_until(&pty, &mut vt, "/model", Duration::from_secs(3)));
        let slash_snap = vt.visible_lines().join("\n");
        println!(
            "\n=== PTY CAPTURE SLASH MENU (100x30) ===\n{slash_snap}\n========================================"
        );
        assert!(slash_snap.contains("/theme"));
        assert!(slash_snap.contains("/policy"));

        // B. Нажимаем Enter (выбран первый пункт `/model`) -> открывается интерактивное меню выбора модели!
        pty.write_input(b"\r");
        assert!(drain_until(
            &pty,
            &mut vt,
            "Выбор модели",
            Duration::from_secs(3)
        ));
        let picker_snap = vt.visible_lines().join("\n");
        println!(
            "\n=== PTY CAPTURE MODEL PICKER (100x30) ===\n{picker_snap}\n=========================================="
        );
        assert!(picker_snap.contains("qwen2.5-coder:14b"));
        assert!(picker_snap.contains("Локально · Ollama"));
        // Нажимаем Enter — подтверждаем текущую модель `qwen2.5-coder:14b`
        pty.write_input(b"\r");
        assert!(drain_until(
            &pty,
            &mut vt,
            "Активна модель: qwen2.5-coder:14b",
            Duration::from_secs(3)
        ));

        // C. Отправляем реальную задачу агенту -> mock вызывает `apply_patch` -> проверяем diff, код и файл на диске!
        pty.write_input("Исправь calc.rs\r".as_bytes());
        assert!(
            drain_until(
                &pty,
                &mut vt,
                "Патч применён и проверен.",
                Duration::from_secs(10)
            ),
            "Agent turn did not complete in PTY; got:\n{}",
            vt.full_lines().join("\n")
        );

        let conv_snap = vt.full_lines().join("\n");
        println!(
            "\n=== PTY CAPTURE CONVERSATION + TOOL + DIFF (100x30) ===\n{conv_snap}\n========================================================"
        );

        // Проверяем: реальный файл на диске исправлен, diff и Rust-блок отображены, спиннер прокрутился!
        let patched_file = fs::read_to_string(ws.join("calc.rs")).unwrap();
        assert!(patched_file.contains("a + b"));
        assert!(conv_snap.contains("apply_patch"));
        assert!(conv_snap.contains("-    a - b"));
        assert!(conv_snap.contains("+    a + b"));
        assert!(conv_snap.contains("┌─ rust"));
        assert!(
            vt.raw_log.contains("93;181;122"),
            "Expected signal-ok green diff"
        );
        assert!(
            vt.raw_log.contains("242;100;95"),
            "Expected signal-error red diff"
        );
        assert!(
            ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
                .iter()
                .any(|sp| vt.raw_log.contains(sp)),
            "Expected smooth Braille activity spinner in raw PTY stream"
        );

        // D. Проверяем сворачивание блока инструмента по Ctrl+O (`0x0f`)
        pty.write_input(&[0x0f]);
        assert!(
            drain_until(&pty, &mut vt, "▸ ✓ apply_patch", Duration::from_secs(3)),
            "Ctrl+O did not collapse apply_patch tool block; visible:\n{}",
            vt.visible_lines().join("\n")
        );

        // E. Динамическое изменение размера PTY (`TIOCSWINSZ` -> `SIGWINCH`: 72×22)
        pty.resize(72, 22);
        vt.resize(72, 22);
        let mut after_resize = Vec::new();
        pty.read_available(&mut after_resize, Duration::from_millis(250));
        vt.feed(&after_resize);
        assert!(vt.visible_lines().iter().any(|l| l.contains('❯')));

        // F. Чистый выход по `/quit` — диалог остаётся в нативном scrollback, alt-screen не включался!
        pty.write_input(b"/quit\r");
        let mut tail = Vec::new();
        pty.read_available(&mut tail, Duration::from_millis(300));
        vt.feed(&tail);
        let code = pty.wait_exit(Duration::from_secs(5));
        assert_eq!(code, Some(0));
        assert!(!vt.saw_alt_screen);
        let final_scrollback = vt.full_lines().join("\n");
        assert!(final_scrollback.contains("KAIC"));
        assert!(final_scrollback.contains("Патч применён и проверен."));
    }

    // 3. Проверяем graceful fallback для --no-color (голый TTY) и графических протоколов (Kitty)
    {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let data = tmp.path().join("data");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&data).unwrap();

        let mut pty_plain = PtySession::spawn(80, 24, &ws, &data, &endpoint, &[], &["--no-color"]);
        let mut vt_plain = VirtualTerm::new(80, 24);
        assert!(drain_until(
            &pty_plain,
            &mut vt_plain,
            "[▸▸▸] KAIC",
            Duration::from_secs(5)
        ));
        pty_plain.write_input(b"/quit\r");
        assert_eq!(pty_plain.wait_exit(Duration::from_secs(5)), Some(0));
        assert!(!vt_plain.saw_alt_screen);

        let mut pty_kitty = PtySession::spawn(
            90,
            24,
            &ws,
            &data,
            &endpoint,
            &[("KAIC_TUI_GRAPHICS", "kitty")],
            &[],
        );
        let mut vt_kitty = VirtualTerm::new(90, 24);
        assert!(drain_until(
            &pty_kitty,
            &mut vt_kitty,
            "❯",
            Duration::from_secs(5)
        ));
        assert!(vt_kitty.raw_log.contains("\x1b_Ga=T,f=100"));
        pty_kitty.write_input(b"/quit\r");
        assert_eq!(pty_kitty.wait_exit(Duration::from_secs(5)), Some(0));
    }

    stop_srv.store(true, Ordering::Relaxed);
    let _ = srv_handle.join();
}
