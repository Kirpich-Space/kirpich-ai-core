//! Проверки настоящего CLI-процесса: конфигурация, HTTP и отказ от pipe-approval.
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

const TOKEN: &str = "cli-private-test-token-123456";
const TASK_ID: &str = "11111111-2222-4333-8444-555555555555";
const REQUEST_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

fn run(args: &[&str], token: Option<&str>, base: &str, input: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kaic-cli"));
    command
        .args(args)
        .env_remove("KAIC_CONTROL_TOKEN")
        .env("KAIC_API_BASE", base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(token) = token {
        command.env("KAIC_CONTROL_TOKEN", token);
    }
    let mut child = command.spawn().expect("CLI запущен");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "CLI не завершился за 10 секунд: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

struct Server {
    base: String,
    stopped: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<Request>>>,
}

impl Server {
    fn start(waiting: bool) -> Self {
        Self::with_recovery(waiting, false)
    }

    fn with_recovery(waiting: bool, recovery: bool) -> Self {
        Self::with_backend(waiting, recovery, true)
    }

    fn with_backend(waiting: bool, recovery: bool, connected: bool) -> Self {
        Self::with_local_models(
            waiting,
            recovery,
            connected,
            200,
            json!({"root":"/models", "models":[], "rejected":[]}),
        )
    }

    fn with_local_models(
        waiting: bool,
        recovery: bool,
        connected: bool,
        local_status: u16,
        local_models: Value,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(12);
            let mut requests = Vec::new();
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let (mut socket, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("accept: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_request(&mut socket);
                let status = if request.path == "/local-models" {
                    local_status
                } else {
                    200
                };
                let body = match (request.method.as_str(), request.path.as_str()) {
                    ("GET", "/tasks") => json!([task_with_recovery(waiting, recovery)]),
                    ("GET", "/models") => json!([]),
                    ("GET", "/local-models") => local_models.clone(),
                    ("GET", "/backend") => json!({
                        "name":"llama-server", "externally_managed":true,
                        "discovery_supported":true, "connected":connected,
                        "models":if connected { json!([{"id":"provider-model","architecture":{"output_modalities":["text"]},"meta":{"n_ctx_train":8192}}]) } else { json!([]) },
                        "error":if connected { Value::Null } else { json!("llama-server недоступен") }
                    }),
                    ("GET", "/status") => json!({
                        "total_model_memory_mb":8192,"used_model_memory_mb":0,
                        "loaded_models":[],"running_tasks":0,"max_concurrent_tasks":2,
                        "busy_models":[],"active_operation":null
                    }),
                    ("POST", "/tasks") => task(false),
                    ("GET", path) if path == format!("/tasks/{TASK_ID}") => {
                        task_with_recovery(waiting, recovery)
                    }
                    _ => json!({}),
                }
                .to_string();
                requests.push(request);
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        Self {
            base,
            stopped,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> Vec<Request> {
        self.stopped.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_request(socket: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0, "полный HTTP-запрос");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= 65536, "ограниченный размер запроса");
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                let mut first = headers.lines().next().unwrap().split_whitespace();
                return Request {
                    method: first.next().unwrap().into(),
                    path: first.next().unwrap().into(),
                    body: bytes[end + 4..end + 4 + length].to_vec(),
                };
            }
        }
    }
}

fn task(waiting: bool) -> Value {
    json!({"id":TASK_ID,"category":"Programming",
        "status":if waiting {"waiting_for_human"} else {"running"},
        "created_at":"2026-10-18T12:00:00Z","updated_at":"2026-10-18T12:00:00Z",
        "context":[],"pending_tool_approval":if waiting {json!({
            "request_id":REQUEST_ID,"task_id":TASK_ID,
            "execution_id":"12345678-1234-4234-8234-123456789012",
            "tool_call_id":"call-1","tool_name":"read_file","arguments":"{\"path\":\"src/main.rs\"}",
            "server":{"command":"local-mcp","args":["--stdio"],"peer_name":"files"}
        })} else {Value::Null}})
}

fn task_with_recovery(waiting: bool, recovery: bool) -> Value {
    let mut task = task(waiting);
    if recovery {
        let snapshot = task["pending_tool_approval"].take();
        task["status"] = json!("paused");
        task["recovery"] = json!({
            "kind":"unknown_outcome", "message":"Проверьте внешний результат",
            "unresolved_tools":[snapshot]
        });
    }
    task
}

#[test]
fn recovery_is_visible_and_pipe_cannot_acknowledge_or_continue_uncertain_task() {
    let server = Server::with_recovery(true, true);
    let output = run(&["--show", TASK_ID], Some(TOKEN), &server.base, "");
    assert!(output.status.success(), "{}", combined(&output));
    assert!(combined(&output).contains("unknown_outcome"));
    assert!(combined(&output).contains("Проверьте внешний результат"));
    let input = format!("/use {TASK_ID}\n/recovery\n/ack {REQUEST_ID}\nпродолжить\n/quit\n");
    let output = run(&[], Some(TOKEN), &server.base, &input);
    assert!(output.status.success(), "{}", combined(&output));
    let text = combined(&output);
    assert!(text.contains(REQUEST_ID));
    assert!(text.contains("Исполнение:"));
    assert!(text.contains("local-mcp"));
    assert!(text.contains("только с интерактивными stdin и stdout"));
    assert!(server
        .finish()
        .iter()
        .all(|request| request.method == "GET"));
}

#[test]
fn help_is_offline_and_does_not_require_a_token() {
    let output = run(&["--help"], None, "http://external.invalid/", "");
    assert_eq!(output.status.code(), Some(0), "{}", combined(&output));
    assert!(combined(&output).contains("kaic-cli"));
    assert!(combined(&output).contains("--send"));
}

#[test]
fn backend_diagnostics_show_real_ids_metadata_and_fail_when_inference_is_unavailable() {
    for connected in [false, true] {
        let server = Server::with_backend(false, false, connected);
        let output = run(&["--backend-info"], Some(TOKEN), &server.base, "");
        assert_eq!(output.status.success(), connected, "{}", combined(&output));
        let text = combined(&output);
        assert!(text.contains("llama-server"));
        assert!(text.contains("load/unload недоступны"));
        if connected {
            assert!(text.contains("provider-model"));
            assert!(text.contains("8192"));
        } else {
            assert!(text.contains("недоступен"));
        }
        assert!(!text.contains(TOKEN));
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/backend");
    }
}

#[test]
fn one_shot_models_reads_existing_registry_route() {
    let server = Server::start(false);
    let output = run(&["--models"], Some(TOKEN), &server.base, "");
    assert!(output.status.success(), "{}", combined(&output));
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/models");
}

#[test]
fn local_models_prints_sanitized_candidates_and_rejections_without_loading_models() {
    let server = Server::with_local_models(
        false,
        false,
        true,
        200,
        json!({
            "root":"/models/Каталог\u{001b}[2J",
            "models":[{
                "id":"candidate\u{202e}.gguf", "path":"/models/name\nforged.gguf",
                "size_bytes":12345678901u64, "gguf_version":3, "tensor_count":459,
                "architecture":"gptoss\u{0007}", "file_type":38
            }],
            "rejected":[{"path":"/models/bad\r.gguf", "error":"invalid\u{001b}[31m"}]
        }),
    );
    let output = run(&["--local-models"], Some(TOKEN), &server.base, "");
    assert!(output.status.success(), "{}", combined(&output));
    let text = combined(&output);
    assert!(text.contains("Каталог локальных GGUF: /models/Каталог\\u{001b}[2J"));
    assert!(text.contains("candidate\\u{202e}.gguf"));
    assert!(text.contains("name\\u{000a}forged.gguf"));
    assert!(text.contains("12345678901 байт; GGUF v3; тензоров: 459"));
    assert!(text.contains("architecture: gptoss\\u{0007}; file_type: 38"));
    assert!(text.contains("Отклонён: /models/bad\\u{000d}.gguf — invalid\\u{001b}[31m"));
    assert!(!text.contains(['\u{001b}', '\u{0007}', '\u{202e}', '\r']));
    assert!(!text.contains(TOKEN));
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/local-models");
}

#[test]
fn interactive_local_models_keeps_catalog_separate_from_registry() {
    let server = Server::start(false);
    let output = run(
        &[],
        Some(TOKEN),
        &server.base,
        "/local-models\n/models\n/quit\n",
    );
    assert!(output.status.success(), "{}", combined(&output));
    assert!(combined(&output).contains("Каталог локальных GGUF: /models"));
    assert!(combined(&output).contains("Подходящих GGUF-файлов нет."));
    let requests = server.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path == "/local-models")
            .count(),
        1
    );
    // Начальное обновление реестра и явная команда /models.
    assert_eq!(requests.iter().filter(|r| r.path == "/models").count(), 2);
    assert!(requests.iter().all(|r| r.method == "GET"));
}

#[test]
fn local_models_empty_catalog_reports_reasons_and_backend_errors_fail() {
    let empty = json!({
        "root":"/empty", "models":[],
        "rejected":[{"path":"/empty/broken.gguf", "error":"truncated header"}]
    });
    let server = Server::with_local_models(false, false, true, 200, empty);
    let output = run(&["--local-models"], Some(TOKEN), &server.base, "");
    assert!(output.status.success(), "{}", combined(&output));
    assert!(combined(&output).contains("Подходящих GGUF-файлов нет."));
    assert!(combined(&output).contains("truncated header"));
    assert_eq!(server.finish().len(), 1);

    for (status, response, message) in [
        (404, json!({}), "маршрут не найдены"),
        (503, json!({}), "недоступен"),
        (200, json!({"models":[]}), "не соответствует контракту"),
    ] {
        let server = Server::with_local_models(false, false, true, status, response);
        let output = run(&["--local-models"], Some(TOKEN), &server.base, "");
        assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
        assert!(combined(&output).contains(message));
        assert!(!combined(&output).contains("Подходящих GGUF-файлов нет."));
        assert_eq!(server.finish().len(), 1);
    }
}

#[test]
fn invalid_arguments_and_remote_backends_fail_before_any_operation() {
    for args in [
        &["--unknown"][..],
        &["--status", "--tasks"],
        &["--show", "invalid-uuid"],
        &["--backend"],
    ] {
        let output = run(args, None, "http://127.0.0.1:9/", "");
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            combined(&output)
        );
    }
    let output = run(
        &["--status", "--backend", "http://example.org/"],
        Some(TOKEN),
        "http://127.0.0.1:9/",
        "",
    );
    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    assert!(!combined(&output).contains(TOKEN));
}

#[test]
fn missing_or_short_credentials_are_rejected_without_disclosure() {
    for token in [None, Some("secret-short")] {
        let output = run(&["--status"], token, "http://127.0.0.1:9/", "");
        assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
        assert!(!combined(&output).contains("secret-short"));
        assert!(!combined(&output).trim().is_empty());
    }
}

#[test]
fn send_preserves_unicode_and_performs_one_exact_create_post() {
    let server = Server::start(false);
    let text = "Новая задача 🧱\nбез потери строки";
    let output = run(&["--send", text], Some(TOKEN), &server.base, "");
    assert_eq!(output.status.code(), Some(0), "{}", combined(&output));
    assert!(combined(&output).contains(TASK_ID));
    assert!(!combined(&output).contains(TOKEN));
    let requests = server.finish();
    assert_eq!(requests.len(), 1, "--send не ждёт inference: {requests:?}");
    assert_eq!(
        (requests[0].method.as_str(), requests[0].path.as_str()),
        ("POST", "/tasks")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
        json!({"text":text,"category":null,"allow_manual":false})
    );
}

#[test]
fn piped_approval_commands_never_authorize_a_tool_even_with_valid_pending_request() {
    let server = Server::start(true);
    let input = format!("/use {TASK_ID}\n/tool\n/more\n/approve {REQUEST_ID}\n/confirm {REQUEST_ID}\n/deny {REQUEST_ID}\n/confirm {REQUEST_ID}\n/close\n/confirm {REQUEST_ID}\n/quit\n");
    let output = run(&[], Some(TOKEN), &server.base, &input);
    assert!(output.status.success(), "{}", combined(&output));
    assert!(!combined(&output).contains(TOKEN));
    let requests = server.finish();
    assert!(
        requests.iter().any(|request| request.path == "/tasks"),
        "макет задачи действительно запрошен"
    );
    assert!(
        requests.iter().all(|request| request.method == "GET"),
        "pipe не выдаёт полномочий: {requests:?}"
    );
}
