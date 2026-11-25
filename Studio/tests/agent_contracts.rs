//! Контрактные и регрессионные тесты агентного runtime KAIC Studio.
//! Используют временные каталоги и локальные тестовые HTTP/CLI-заглушки (не реальный инференс).
use kaic_studio::{
    agent::{
        self, AgentEvent, AgentLimits, AgentPolicy, AgentStreamEvent, AnthropicApiBackend,
        ApprovalDecision, ApprovalGate, ApprovalGranularity, CodexCliConfig, CommandClass,
        ExecPolicy, ExecutionOrigin, FeatureStatus, IsolationLevel, LiveEditOp, LocalHttpBackend,
        OllamaBackend, OpenAiCompatBackend, ProviderKind, SeatManager, WorkspaceGuard,
        probe_providers, run_agent_with_backend, run_agent_with_policies, run_agy_cli_with_binary,
        run_claude_cli_with_binary, run_codex_cli_with_binary,
    },
    kaic::Secret,
    network::{Cancel, LocalEndpoint},
    sse::{SseDecoder, StreamEvent},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Reply {
    method: &'static str,
    path: &'static str,
    body: Vec<u8>,
    status: u16,
    sse: bool,
}

impl Reply {
    fn json(method: &'static str, path: &'static str, v: Value) -> Self {
        Self {
            method,
            path,
            body: serde_json::to_vec(&v).unwrap(),
            status: 200,
            sse: false,
        }
    }
    fn sse(path: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: "POST",
            path,
            body: body.into(),
            status: 200,
            sse: true,
        }
    }
}

fn fixture(replies: Vec<Reply>) -> (LocalEndpoint, thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint =
        LocalEndpoint::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let join = thread::spawn(move || {
        let mut received = vec![];
        for reply in replies {
            let start = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            start.elapsed() < Duration::from_secs(10),
                            "Тестовый сервер не получил запрос вовремя"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("Тестовый accept не выполнен: {e}"),
                }
            };
            let (method, path, body) = read_request(&mut stream);
            assert_eq!(method, reply.method);
            assert_eq!(path, reply.path);
            let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            received.push(parsed);
            let headers = format!(
                "HTTP/1.1 {} Test\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                if reply.sse {
                    "text/event-stream"
                } else {
                    "application/json"
                },
                reply.body.len()
            );
            stream.write_all(headers.as_bytes()).unwrap();
            stream.write_all(&reply.body).unwrap();
        }
        received
    });
    (endpoint, join)
}

fn read_request(s: &mut TcpStream) -> (String, String, Vec<u8>) {
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut b = vec![];
    let mut byte = [0];
    while !b.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut byte).unwrap();
        b.push(byte[0]);
        assert!(b.len() < 65536);
    }
    let h = String::from_utf8(b).unwrap();
    let mut lines = h.lines();
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_string();
    let path = first.next().unwrap().to_string();
    let mut length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    s.read_exact(&mut body).unwrap();
    (method, path, body)
}

fn sse_tool_call(id: &str, name: &str, args_json: &str) -> Vec<u8> {
    let chunk1 = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": "" }
                }]
            }
        }]
    });
    let mid = args_json.len() / 2;
    let mut split = mid;
    while !args_json.is_char_boundary(split) {
        split += 1;
    }
    let chunk2 = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": &args_json[..split] }
                }]
            }
        }]
    });
    let chunk3 = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": &args_json[split..] }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {chunk1}\n\ndata: {chunk2}\n\ndata: {chunk3}\n\ndata: [DONE]\n\n").into_bytes()
}

fn sse_text(text: &str) -> Vec<u8> {
    let chunk = json!({
        "choices": [{
            "delta": { "content": text },
            "finish_reason": "stop"
        }]
    });
    format!("data: {chunk}\n\ndata: [DONE]\n\n").into_bytes()
}

#[test]
fn sse_parses_split_tool_call_deltas_with_utf8() {
    let mut dec = SseDecoder::default();
    let raw = sse_tool_call(
        "call_1",
        "write_file",
        r#"{"path":"note.txt","content":"Привет"}"#,
    );
    let mut events = vec![];
    for byte in &raw {
        events.extend(dec.push(&[*byte]).unwrap());
    }
    dec.finish().unwrap();
    let mut name = String::new();
    let mut args = String::new();
    let mut saw_tool_done = false;
    for ev in events {
        match ev {
            StreamEvent::ToolCallDelta {
                index,
                name: n,
                arguments_delta: a,
                ..
            } => {
                assert_eq!(index, 0);
                if let Some(n) = n {
                    name.push_str(&n);
                }
                if let Some(a) = a {
                    args.push_str(&a);
                }
            }
            StreamEvent::ToolCallsDone => saw_tool_done = true,
            _ => {}
        }
    }
    assert_eq!(name, "write_file");
    assert_eq!(args, r#"{"path":"note.txt","content":"Привет"}"#);
    assert!(saw_tool_done);
}

#[test]
fn workspace_guard_blocks_traversal_symlinks_secrets_and_dangerous_commands() {
    let outer = tempfile::tempdir().unwrap();
    let ws = outer.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let secret_outside = outer.path().join("outside.txt");
    std::fs::write(&secret_outside, "secret").unwrap();
    std::os::unix::fs::symlink(&secret_outside, ws.join("escape_link.txt")).unwrap();
    std::fs::write(ws.join(".env"), "API_KEY=123").unwrap();
    std::fs::write(ws.join("auth.json"), "{\"token\":\"x\"}").unwrap();
    std::fs::write(ws.join("ok.txt"), "hello").unwrap();

    let guard = WorkspaceGuard::new(&ws).unwrap();
    assert!(guard.resolve_existing("ok.txt").is_ok());
    assert!(guard.resolve_existing("../outside.txt").is_err());
    assert!(guard.resolve_existing("escape_link.txt").is_err());
    assert!(guard.resolve_existing(".env").is_err());
    assert!(guard.resolve_existing("auth.json").is_err());
    assert!(guard.resolve_write_target(".ssh/id_rsa").is_err());
    assert!(guard.resolve_write_target("../pwn.txt").is_err());

    assert!(WorkspaceGuard::new(Path::new("/")).is_err());

    assert!(
        guard
            .validate_command("sh", &["-c".into(), "ls".into()])
            .is_err()
    );
    assert!(
        guard
            .validate_command("rm", &["-rf".into(), "ok.txt".into()])
            .is_err()
    );
    assert!(guard.validate_command("sudo", &["id".into()]).is_err());
    assert!(
        guard
            .validate_command("curl", &["http://example.com".into()])
            .is_err()
    );
    assert!(guard.validate_command("git", &["push".into()]).is_err());
    assert!(
        guard
            .validate_command("git", &["reset".into(), "--hard".into()])
            .is_err()
    );
    assert!(guard.validate_command("git", &["status".into()]).is_ok());
    assert!(
        guard
            .validate_command("python3", &["-m".into(), "unittest".into()])
            .is_ok()
    );
    assert!(
        guard
            .validate_command("python3", &["-c".into(), "print(1)".into()])
            .is_err()
    );
}

#[tokio::test]
async fn dynamic_agent_reads_edits_runs_check_with_approval_and_blocks_dangerous_action() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("calc.py"),
        "def add(a, b):\n    return a - b\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("test_calc.py"),
        "import unittest\nfrom calc import add\n\nclass T(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n",
    )
    .unwrap();

    // 1) read calc.py
    // 2) attempt dangerous rm -> denied by policy gateway
    // 3) edit calc.py (requires approval -> approved for exact request_id)
    // 4) run python3 -m unittest (requires approval -> approved)
    // 5) final verified response
    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("c1", "read_file", r#"{"path":"calc.py"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "c2",
                "run_check",
                r#"{"program":"rm","args":["-rf","calc.py"]}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4, 5]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "c3",
                "edit_file",
                r#"{"path":"calc.py","old_text":"return a - b","new_text":"return a + b"}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4, 5, 6]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "c4",
                "run_check",
                r#"{"program":"python3","args":["-m","unittest","test_calc.py"]}"#,
            ),
        ),
        Reply::json(
            "POST",
            "/tokenize",
            json!({"tokens": [1, 2, 3, 4, 5, 6, 7]}),
        ),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("Исправил calc.py (a + b) и подтвердил тестом unittest."),
        ),
    ];
    let (endpoint, join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 16384, "test-model".into());
    let cancel = Cancel::default();
    let approvals_seen = Arc::new(AtomicUsize::new(0));
    let approvals_seen_cb = approvals_seen.clone();
    let gate = ApprovalGate::callback(move |req| {
        approvals_seen_cb.fetch_add(1, Ordering::SeqCst);
        ApprovalDecision {
            request_id: req.request_id,
            action_digest: req.action_digest,
            approved: true,
        }
    });
    let mut streamed_events = vec![];
    let result = run_agent_with_backend(
        &mut backend,
        dir.path(),
        "Исправь add в calc.py и проверь тестом",
        &[],
        AgentPolicy::ApprovalRequired,
        AgentLimits::default(),
        &cancel,
        &gate,
        |ev| streamed_events.push(ev),
    )
    .await
    .unwrap();

    let requests = join.join().unwrap();
    assert_eq!(requests.len(), 10);
    assert_eq!(approvals_seen.load(Ordering::SeqCst), 2);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("calc.py")).unwrap(),
        "def add(a, b):\n    return a + b\n"
    );
    assert!(result.changed_files.iter().any(|p| p == "calc.py"));
    assert!(result.final_text.contains("Исправил calc.py"));
    // Verify dangerous rm was blocked without asking for approval
    assert!(result.events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolResult { name, ok: false, output_preview, .. }
            if name == "run_check" && output_preview.contains("запрещ")
    )));
    // Verify unittest actually passed
    assert!(result.events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolResult { name, ok: true, output_preview, .. }
            if name == "run_check" && output_preview.contains("OK")
    )));
    assert!(
        streamed_events
            .iter()
            .any(|e| matches!(e, AgentStreamEvent::TextDelta(_)))
    );
}

#[tokio::test]
async fn approval_denial_and_mismatched_request_id_block_write() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "original").unwrap();

    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "c1",
                "write_file",
                r#"{"path":"data.txt","content":"overwritten"}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("Запись отклонена пользователем; файл не изменён."),
        ),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 8192, "test-model".into());
    let cancel = Cancel::default();
    // Returning a mismatched request_id must NEVER approve the action!
    let gate = ApprovalGate::callback(|req| ApprovalDecision {
        request_id: "wrong-request-id".into(),
        action_digest: req.action_digest,
        approved: true,
    });
    let result = run_agent_with_backend(
        &mut backend,
        dir.path(),
        "Перезапиши data.txt",
        &[],
        AgentPolicy::ApprovalRequired,
        AgentLimits::default(),
        &cancel,
        &gate,
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("data.txt")).unwrap(),
        "original"
    );
    assert!(result.changed_files.is_empty());
    assert!(result.events.iter().any(|e| matches!(
        e,
        AgentEvent::ApprovalResolved {
            approved: false,
            ..
        }
    )));
}

#[tokio::test]
async fn repeated_failing_tool_calls_stop_with_honest_blocker() {
    let dir = tempfile::tempdir().unwrap();
    let mut replies = vec![];
    for i in 0..3 {
        replies.push(Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})));
        replies.push(Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(&format!("c{i}"), "read_file", r#"{"path":"missing.txt"}"#),
        ));
    }
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 8192, "test-model".into());
    let cancel = Cancel::default();
    let gate = ApprovalGate::deny_all();
    let limits = AgentLimits {
        max_steps: 10,
        max_repeat_failures: 3,
        ..AgentLimits::default()
    };
    let result = run_agent_with_backend(
        &mut backend,
        dir.path(),
        "Прочитай missing.txt",
        &[],
        AgentPolicy::ReadOnly,
        limits,
        &cancel,
        &gate,
        |_| {},
    )
    .await
    .unwrap();

    assert!(result.blocked);
    assert!(
        result
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::Blocked { .. }))
    );
}

#[tokio::test]
async fn cancellation_during_approval_wait_preserves_prior_edits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "before").unwrap();
    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("c1", "write_file", r#"{"path":"a.txt","content":"step1"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("c2", "write_file", r#"{"path":"b.txt","content":"step2"}"#),
        ),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 8192, "test-model".into());
    let cancel = Cancel::default();
    let cancel_bg = cancel.clone();
    let (gate, handle) = ApprovalGate::interactive();
    let driver = thread::spawn(move || {
        // Wait for first approval, approve it; on second, cancel!
        let req1 = handle.wait_request(Duration::from_secs(5)).unwrap();
        handle.respond(req1.request_id, req1.action_digest, true);
        let _req2 = handle.wait_request(Duration::from_secs(5)).unwrap();
        cancel_bg.cancel();
    });

    let result = run_agent_with_backend(
        &mut backend,
        dir.path(),
        "Создай a.txt и b.txt",
        &[],
        AgentPolicy::ApprovalRequired,
        AgentLimits::default(),
        &cancel,
        &gate,
        |_| {},
    )
    .await
    .unwrap();
    driver.join().unwrap();

    assert!(result.cancelled);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "step1"
    );
    assert!(!dir.path().join("b.txt").exists());
    assert!(result.changed_files.iter().any(|f| f == "a.txt"));
}

#[tokio::test]
async fn bounded_subtasks_cap_policy_reject_write_overlap_and_mark_unverified() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alpha.txt"), "alpha-value=42\n").unwrap();
    std::fs::write(dir.path().join("beta.txt"), "beta-value=99\n").unwrap();

    // Parent delegates 2 subtasks:
    // 1) first tries overlapping write -> rejected
    // 2) next delegates 2 read-only subtasks (requesting workspace_write under ReadOnly parent -> capped!)
    //    subtask-1 reads alpha.txt and answers
    //    subtask-2 reads beta.txt and answers
    // 3) parent produces final answer
    let overlap_args = json!({
        "subtasks": [
            {"title": "W1", "instructions": "edit shared", "policy": "workspace_write", "owned_paths": ["alpha.txt"]},
            {"title": "W2", "instructions": "edit shared", "policy": "workspace_write", "owned_paths": ["alpha.txt"]}
        ]
    })
    .to_string();
    let valid_args = json!({
        "subtasks": [
            {"title": "Inspect alpha", "instructions": "Read alpha.txt", "policy": "workspace_write", "owned_paths": ["alpha.txt"]},
            {"title": "Inspect beta", "instructions": "Read beta.txt", "policy": "read_only", "owned_paths": ["beta.txt"]}
        ]
    })
    .to_string();

    let replies = vec![
        // Parent step 1 -> overlapping writes (rejected by policy gateway)
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("p1", "delegate_subtasks", &overlap_args),
        ),
        // Parent step 2 -> valid 2 subtasks
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("p2", "delegate_subtasks", &valid_args),
        ),
        // Subtask 1 step 1 -> reads alpha.txt
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("s1_1", "read_file", r#"{"path":"alpha.txt"}"#),
        ),
        // Subtask 1 step 2 -> tries to write alpha.txt (must be denied because parent is ReadOnly!)
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "s1_2",
                "write_file",
                r#"{"path":"alpha.txt","content":"hack"}"#,
            ),
        ),
        // Subtask 1 step 3 -> final report
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse("/v1/chat/completions", sse_text("alpha-value=42")),
        // Subtask 2 step 1 -> reads beta.txt
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("s2_1", "read_file", r#"{"path":"beta.txt"}"#),
        ),
        // Subtask 2 step 2 -> final report
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse("/v1/chat/completions", sse_text("beta-value=99")),
        // Parent step 3 -> final answer
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("Обе подзадачи выполнены: alpha=42, beta=99."),
        ),
    ];

    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 16384, "local-gemma".into());
    let cancel = Cancel::default();
    let gate = ApprovalGate::deny_all();
    let result = run_agent_with_backend(
        &mut backend,
        dir.path(),
        "Проверь alpha.txt и beta.txt через две подзадачи",
        &[],
        AgentPolicy::ReadOnly,
        AgentLimits::default(),
        &cancel,
        &gate,
        |_| {},
    )
    .await
    .unwrap();

    // alpha.txt was NOT modified despite subtask requesting workspace_write!
    assert_eq!(
        std::fs::read_to_string(dir.path().join("alpha.txt")).unwrap(),
        "alpha-value=42\n"
    );
    assert_eq!(result.subtasks.len(), 2);
    for st in &result.subtasks {
        assert!(!st.id.is_empty());
        assert!(!st.parent_id.is_empty());
        assert_eq!(st.effective_policy, AgentPolicy::ReadOnly);
        assert_eq!(st.provider, ProviderKind::LocalLlama);
        assert_eq!(st.model, "local-gemma");
        assert!(
            !st.verified,
            "Итог подзадачи остаётся неподтверждённым до проверки"
        );
    }
    assert!(result.events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolResult { name, ok: false, output_preview, .. }
            if name == "delegate_subtasks" && output_preview.contains("конфликт")
    )));
}

#[tokio::test]
async fn codex_cli_adapter_requires_external_consent_enforces_sandbox_and_streams_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    let fake_cli = dir.path().join("fake-codex");
    std::fs::write(
        &fake_cli,
        r#"#!/bin/sh
args="$*"
cat >/dev/null
printf '{"type":"thread.started","thread_id":"t1"}\n'
printf '{"type":"item.completed","item":{"type":"agent_message","text":"ARGS=%s"}}\n' "$args"
printf '{"type":"turn.completed","usage":{"output_tokens":7}}\n'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake_cli, std::fs::Permissions::from_mode(0o700)).unwrap();

    let cancel = Cancel::default();
    let gate = ApprovalGate::deny_all();

    // 1) Without external_consent -> must refuse before spawning CLI
    let denied = run_codex_cli_with_binary(
        &fake_cli,
        CodexCliConfig {
            workspace: dir.path().to_path_buf(),
            prompt: "Проверь репозиторий".into(),
            policy: AgentPolicy::ReadOnly,
            external_consent: false,
            model: None,
        },
        &cancel,
        &gate,
        |_| {},
    )
    .await;
    assert!(denied.is_err());

    // 2) With external_consent + ReadOnly -> passes --sandbox read-only and parses JSONL
    let res = run_codex_cli_with_binary(
        &fake_cli,
        CodexCliConfig {
            workspace: dir.path().to_path_buf(),
            prompt: "Проверь репозиторий".into(),
            policy: AgentPolicy::ReadOnly,
            external_consent: true,
            model: None,
        },
        &cancel,
        &gate,
        |_| {},
    )
    .await
    .unwrap();
    assert!(res.final_text.contains("--sandbox read-only"));
    assert!(res.final_text.contains("--json"));
    assert!(res.final_text.contains("--ephemeral"));

    // 3) Provider capabilities report honest facts (approval granularity + FreeBSD unverified)
    let caps = probe_providers();
    let codex_cap = caps
        .iter()
        .find(|c| c.provider == ProviderKind::CodexCli)
        .unwrap();
    assert_eq!(
        codex_cap.approval_granularity,
        ApprovalGranularity::LaunchConsentAndSandbox
    );
    assert_eq!(codex_cap.freebsd_status, FeatureStatus::Unavailable);
    let local_cap = caps
        .iter()
        .find(|c| c.provider == ProviderKind::LocalLlama)
        .unwrap();
    assert_eq!(
        local_cap.approval_granularity,
        ApprovalGranularity::PerAction
    );
    let _ = agent::tool_definitions(false);
}

#[tokio::test]
async fn approval_is_single_use_and_bound_to_action_arguments_digest() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "init").unwrap();

    // 1) Replay test: first write_file is approved; second write_file gets the OLD decision replayed -> MUST BE DENIED.
    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("c1", "write_file", r#"{"path":"a.txt","content":"first"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "c2",
                "write_file",
                r#"{"path":"a.txt","content":"replayed"}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4]})),
        Reply::sse("/v1/chat/completions", sse_text("Готово")),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 8192, "test-model".into());
    let cancel = Cancel::default();
    let first_decision: Arc<std::sync::Mutex<Option<ApprovalDecision>>> =
        Arc::new(std::sync::Mutex::new(None));
    let first_clone = first_decision.clone();
    let gate = ApprovalGate::callback(move |req| {
        let mut slot = first_clone.lock().unwrap();
        if let Some(prev) = slot.clone() {
            // Replay exact previous decision for the second request
            return prev;
        }
        let dec = ApprovalDecision {
            request_id: req.request_id,
            action_digest: req.action_digest,
            approved: true,
        };
        *slot = Some(dec.clone());
        dec
    });
    let res = run_agent_with_backend(
        &mut backend,
        dir.path(),
        "Сделай две записи",
        &[],
        AgentPolicy::ApprovalRequired,
        AgentLimits::default(),
        &cancel,
        &gate,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "first",
        "Повторное использование старого подтверждения не должно разрешать вторую запись"
    );
    assert!(res.events.iter().any(|e| matches!(
        e,
        AgentEvent::ApprovalResolved {
            approved: false,
            ..
        }
    )));

    // 2) Digest mismatch test: matching request_id but tampered action_digest -> MUST BE DENIED.
    let replies2 = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "c3",
                "write_file",
                r#"{"path":"a.txt","content":"tampered"}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse("/v1/chat/completions", sse_text("Отклонено")),
    ];
    let (endpoint2, _join2) = fixture(replies2);
    let mut backend2 = LocalHttpBackend::new(endpoint2, 8192, "test-model".into());
    let gate2 = ApprovalGate::callback(|req| ApprovalDecision {
        request_id: req.request_id,
        action_digest: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        approved: true,
    });
    let _ = run_agent_with_backend(
        &mut backend2,
        dir.path(),
        "Перезапиши a.txt",
        &[],
        AgentPolicy::ApprovalRequired,
        AgentLimits::default(),
        &cancel,
        &gate2,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "first",
        "Несовпадение digest действия/аргументов должно отклонять запись"
    );
}

#[test]
fn workspace_guard_blocks_toctou_symlink_swap_via_openat_nofollow() {
    let outer = tempfile::tempdir().unwrap();
    let ws = outer.path().join("ws");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    let outside_secret = outer.path().join("outside_secret.txt");
    std::fs::write(&outside_secret, "TOP_SECRET_OUTSIDE").unwrap();
    let target_file = ws.join("sub/file.txt");
    std::fs::write(&target_file, "safe_inside").unwrap();

    // Direct symlink inside workspace pointing outside
    std::os::unix::fs::symlink(&outside_secret, ws.join("sub/link.txt")).unwrap();
    // Directory symlink inside workspace pointing to outer directory
    std::os::unix::fs::symlink(outer.path(), ws.join("dir_escape")).unwrap();

    let guard = WorkspaceGuard::new(&ws).unwrap();
    // Kernel O_NOFOLLOW openat must reject both leaf symlinks and intermediate directory symlinks
    assert!(guard.open_read_no_follow("sub/link.txt").is_err());
    assert!(
        guard
            .open_read_no_follow("dir_escape/outside_secret.txt")
            .is_err()
    );
    assert!(
        guard
            .atomic_write_no_follow("sub/link.txt", b"overwrite_escape")
            .is_err()
    );
    assert!(
        guard
            .atomic_write_no_follow("dir_escape/pwn.txt", b"escape")
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(&outside_secret).unwrap(),
        "TOP_SECRET_OUTSIDE"
    );

    // Concurrent symlink-swap stress test: one thread swaps sub/race.txt between a real file
    // and a symlink to outside_secret while the reader repeatedly calls open_read_no_follow.
    // It must NEVER return "TOP_SECRET_OUTSIDE".
    let race_path = ws.join("sub/race.txt");
    std::fs::write(&race_path, "safe_inside").unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_bg = stop.clone();
    let race_bg = race_path.clone();
    let outside_bg = outside_secret.clone();
    let swapper = thread::spawn(move || {
        let mut toggle = false;
        while !stop_bg.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&race_bg);
            if toggle {
                let _ = std::os::unix::fs::symlink(&outside_bg, &race_bg);
            } else {
                let _ = std::fs::write(&race_bg, "safe_inside");
            }
            toggle = !toggle;
        }
    });
    for _ in 0..300 {
        if let Ok((mut f, _)) = guard.open_read_no_follow("sub/race.txt") {
            let mut s = String::new();
            let _ = f.read_to_string(&mut s);
            assert_ne!(
                s, "TOP_SECRET_OUTSIDE",
                "TOCTOU symlink swap escaped workspace!"
            );
        }
    }
    stop.store(true, Ordering::Relaxed);
    swapper.join().unwrap();
}

#[test]
fn command_classification_distinguishes_safe_inspect_user_code_builds_and_hard_denies() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    let guard = WorkspaceGuard::new(ws).unwrap();

    // 1) Реальные сборочные и тестовые команды (npm, pytest, make, cargo, unittest) НЕ маскируются под safe inspect,
    // а классифицируются честно как UserCodeExecution с IsolationLevel::NoneUserPrivileges.
    for (prog, args) in [
        ("npm", vec!["test".into()]),
        ("npm", vec!["run".into(), "build".into()]),
        ("pytest", vec![]),
        ("python3", vec!["-m".into(), "pytest".into()]),
        ("make", vec!["check".into()]),
        ("cargo", vec!["test".into()]),
        (
            "python3",
            vec!["-m".into(), "unittest".into(), "test_a.py".into()],
        ),
    ] {
        let v = guard.validate_command(prog, &args).unwrap_or_else(|e| {
            panic!("Команда разработки {prog} {args:?} должна быть поддержана: {e:#}")
        });
        assert_eq!(v.class, CommandClass::UserCodeExecution);
        assert_eq!(v.isolation, IsolationLevel::NoneUserPrivileges);
    }

    // 2) Инспекционные команды (git status/diff с отключёнными хуками, python3 -m py_compile, ls, wc, diff) — SafeInspect.
    let git_cmd = guard.validate_command("git", &["status".into()]).unwrap();
    assert_eq!(git_cmd.class, CommandClass::SafeInspect);
    assert!(git_cmd.args.iter().any(|a| a == "core.fsmonitor=false"));
    assert!(git_cmd.args.iter().any(|a| a == "core.hooksPath=/dev/null"));

    let pyc = guard
        .validate_command(
            "python3",
            &["-m".into(), "py_compile".into(), "a.py".into()],
        )
        .unwrap();
    assert_eq!(pyc.class, CommandClass::SafeInspect);

    // 3) Обход git через -c / --ext-diff и python3 -c остаются жёстко запрещены.
    assert!(
        guard
            .validate_command(
                "git",
                &[
                    "-c".into(),
                    "core.fsmonitor=sh -c id".into(),
                    "status".into()
                ]
            )
            .is_err()
    );
    assert!(
        guard
            .validate_command("git", &["diff".into(), "--ext-diff".into()])
            .is_err()
    );

    // 4) Наличие build.rs, .cargo/config.toml или sitecustomize.py НЕ ломает реальные сборки,
    // но честно фиксируется в risk_factors для явного предупреждения пользователя!
    std::fs::create_dir_all(ws.join(".cargo")).unwrap();
    std::fs::write(
        ws.join(".cargo/config.toml"),
        "[build]\nrustc-wrapper = \"sccache\"\n",
    )
    .unwrap();
    std::fs::write(ws.join("build.rs"), "fn main() {}").unwrap();
    let cargo_cmd = guard.validate_command("cargo", &["check".into()]).unwrap();
    assert_eq!(cargo_cmd.class, CommandClass::UserCodeExecution);
    assert!(
        cargo_cmd
            .risk_factors
            .iter()
            .any(|r| r.contains("build.rs"))
    );
    assert!(
        cargo_cmd
            .risk_factors
            .iter()
            .any(|r| r.contains(".cargo/config"))
    );

    std::fs::write(ws.join("sitecustomize.py"), "import os\n").unwrap();
    // При наличии sitecustomize.py даже py_compile/unittest не считаются безвредными и требуют подтверждения как UserCodeExecution!
    let py_with_hook = guard
        .validate_command(
            "python3",
            &["-m".into(), "unittest".into(), "test_a.py".into()],
        )
        .unwrap();
    assert_eq!(py_with_hook.class, CommandClass::UserCodeExecution);
    assert!(
        py_with_hook
            .risk_factors
            .iter()
            .any(|r| r.contains("sitecustomize.py"))
    );
}

#[tokio::test]
async fn file_policy_and_exec_policy_are_independent_and_real_make_build_works() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Makefile"),
        "all:\n\t@printf 'BUILT_OK\\n' > built.txt\n",
    )
    .unwrap();

    // Часть 1: file_policy = WorkspaceWrite + exec_policy = ConfirmUserCode (по умолчанию!).
    // write_file выполняется БЕЗ подтверждения, а make/unittest ТРЕБУЮТ явного подтверждения с предупреждением о правах пользователя.
    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("w1", "write_file", r#"{"path":"note.txt","content":"ok"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("m1", "run_check", r#"{"program":"make","args":["all"]}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("Сборка выполнена после подтверждения."),
        ),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 8192, "test-model".into());
    let cancel = Cancel::default();
    let seen_tools: Arc<std::sync::Mutex<Vec<(String, IsolationLevel, String)>>> =
        Arc::new(std::sync::Mutex::new(vec![]));
    let seen_cb = seen_tools.clone();
    let gate = ApprovalGate::callback(move |req| {
        seen_cb
            .lock()
            .unwrap()
            .push((req.tool.clone(), req.isolation, req.risk_note.clone()));
        ApprovalDecision {
            request_id: req.request_id,
            action_digest: req.action_digest,
            approved: true,
        }
    });
    let res = run_agent_with_policies(
        &mut backend,
        dir.path(),
        "Создай note.txt и собери make all",
        &[],
        AgentPolicy::WorkspaceWrite,
        ExecPolicy::ConfirmUserCode,
        AgentLimits::default(),
        &cancel,
        &gate,
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
        "ok"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("built.txt"))
            .unwrap()
            .trim(),
        "BUILT_OK",
        "Реальная сборка make должна работать после явного подтверждения"
    );
    let seen = seen_tools.lock().unwrap();
    // Подтверждение запрашивалось ТОЛЬКО для run_check (make), а не для write_file!
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "run_check");
    assert_eq!(seen[0].1, IsolationLevel::NoneUserPrivileges);
    assert!(
        seen[0].2.contains("правами текущего пользователя"),
        "Предупреждение должно честно указывать запуск кода с правами пользователя: {}",
        seen[0].2
    );
    assert!(!res.blocked && !res.cancelled);

    // Часть 2: exec_policy = SafeInspectOnly отклоняет make, но разрешает git status / ls.
    let replies2 = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("m2", "run_check", r#"{"program":"make","args":["all"]}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("l1", "run_check", r#"{"program":"ls","args":[]}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("make запрещён в safe_inspect_only."),
        ),
    ];
    let (endpoint2, _join2) = fixture(replies2);
    let mut backend2 = LocalHttpBackend::new(endpoint2, 8192, "test-model".into());
    let res2 = run_agent_with_policies(
        &mut backend2,
        dir.path(),
        "Попробуй make и ls",
        &[],
        AgentPolicy::WorkspaceWrite,
        ExecPolicy::SafeInspectOnly,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |_| {},
    )
    .await
    .unwrap();
    assert!(res2.events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolResult { name, ok: false, output_preview, .. }
            if name == "run_check" && output_preview.contains("safe_inspect_only")
    )));
    assert!(res2.events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolResult { name, ok: true, summary, .. }
            if name == "run_check" && summary.contains("ls")
    )));
}

#[tokio::test]
async fn parent_cancel_terminates_child_subtask_and_grandchild_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("grandchild.pid");
    // A unittest that spawns a sleeping grandchild process and records its PID
    std::fs::write(
        dir.path().join("test_slow.py"),
        format!(
            concat!(
                "import subprocess, time, unittest\n",
                "class Slow(unittest.TestCase):\n",
                "    def test_wait(self):\n",
                "        p = subprocess.Popen(['sleep', '60'])\n",
                "        with open({:?}, 'w') as f:\n",
                "            f.write(str(p.pid))\n",
                "        time.sleep(60)\n",
            ),
            pid_file.to_string_lossy()
        ),
    )
    .unwrap();

    let sub_args = json!({
        "subtasks": [{
            "title": "Slow child check",
            "instructions": "Run test_slow.py",
            "policy": "workspace_write",
            "owned_paths": ["test_slow.py"]
        }]
    })
    .to_string();

    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("p1", "delegate_subtasks", &sub_args),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "s1",
                "run_check",
                r#"{"program":"python3","args":["-m","unittest","test_slow.py"]}"#,
            ),
        ),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 8192, "test-model".into());
    let cancel = Cancel::default();
    let cancel_bg = cancel.clone();
    let pid_file_bg = pid_file.clone();

    let killer = thread::spawn(move || {
        let start = Instant::now();
        while !pid_file_bg.is_file() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(pid_file_bg.is_file(), "Grandchild PID file must be written");
        cancel_bg.cancel();
    });

    let res = run_agent_with_policies(
        &mut backend,
        dir.path(),
        "Делегируй долгую проверку",
        &[],
        AgentPolicy::WorkspaceWrite,
        ExecPolicy::AllowWorkspaceBuilds,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |_| {},
    )
    .await
    .unwrap();
    killer.join().unwrap();

    assert!(res.cancelled);
    let gpid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Wait briefly for kernel process reaping
    let wait_start = Instant::now();
    let is_still_running = |pid: u32| -> (bool, String) {
        let p = format!("/proc/{pid}/status");
        match std::fs::read_to_string(&p) {
            Err(_) => (false, "reaped".into()),
            Ok(st) => {
                let state_line = st
                    .lines()
                    .find(|l| l.starts_with("State:"))
                    .unwrap_or("")
                    .to_string();
                let dead = state_line.contains("\tZ") || state_line.contains("zombie");
                (!dead, state_line)
            }
        }
    };
    let mut last_state = String::new();
    while wait_start.elapsed() < Duration::from_secs(2) {
        let (running, st) = is_still_running(gpid);
        last_state = st;
        if !running {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let (still_running, _) = is_still_running(gpid);
    assert!(
        !still_running,
        "Отмена родителя должна завершить всю группу процессов дочерней задачи (grandchild PID {gpid} остался жив: {last_state})"
    );
}

#[tokio::test]
async fn subtask_owned_paths_enforce_real_write_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alpha.txt"), "old_alpha").unwrap();
    std::fs::write(dir.path().join("beta.txt"), "old_beta").unwrap();

    let sub_args = json!({
        "subtasks": [{
            "title": "Edit only alpha",
            "instructions": "Update alpha.txt only",
            "policy": "workspace_write",
            "owned_paths": ["alpha.txt"]
        }]
    })
    .to_string();

    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("p1", "delegate_subtasks", &sub_args),
        ),
        // Child tries to write beta.txt (OUTSIDE owned_paths -> MUST BE DENIED!)
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "s1_1",
                "write_file",
                r#"{"path":"beta.txt","content":"hacked"}"#,
            ),
        ),
        // Child writes alpha.txt (INSIDE owned_paths -> MUST SUCCEED!)
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "s1_2",
                "write_file",
                r#"{"path":"alpha.txt","content":"new_alpha"}"#,
            ),
        ),
        // Child finishes
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse("/v1/chat/completions", sse_text("Обновил alpha.txt")),
        // Parent finishes without verifying -> subtask stays unverified
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4]})),
        Reply::sse("/v1/chat/completions", sse_text("Подзадача завершилась.")),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 16384, "test-model".into());
    let cancel = Cancel::default();
    let res = run_agent_with_backend(
        &mut backend,
        dir.path(),
        "Делегируй правку alpha.txt",
        &[],
        AgentPolicy::WorkspaceWrite,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("alpha.txt")).unwrap(),
        "new_alpha"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("beta.txt")).unwrap(),
        "old_beta",
        "Запись за пределами owned_paths ребёнка должна быть заблокирована"
    );
    assert_eq!(res.subtasks.len(), 1);
    assert!(
        !res.subtasks[0].verified,
        "Без проверки оркестратором подзадача должна остаться неподтверждённой"
    );
    assert_eq!(res.unverified_subtasks, 1);
}

#[tokio::test]
async fn two_writing_subtasks_transition_to_verified_only_after_orchestrator_checks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("part_a.py"), "def a():\n    return 0\n").unwrap();
    std::fs::write(dir.path().join("part_b.py"), "def b():\n    return 0\n").unwrap();
    std::fs::write(
        dir.path().join("test_parts.py"),
        "import unittest\nfrom part_a import a\nfrom part_b import b\n\nclass T(unittest.TestCase):\n    def test_sum(self):\n        self.assertEqual(a() + b(), 30)\n",
    )
    .unwrap();

    let sub_args = json!({
        "subtasks": [
            {"title": "Fix part_a", "instructions": "Return 10", "policy": "workspace_write", "owned_paths": ["part_a.py"]},
            {"title": "Fix part_b", "instructions": "Return 20", "policy": "workspace_write", "owned_paths": ["part_b.py"]}
        ]
    })
    .to_string();

    let replies = vec![
        // Parent delegates 2 writing subtasks on disjoint files
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("p1", "delegate_subtasks", &sub_args),
        ),
        // Subtask 1 edits part_a.py
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "s1_1",
                "edit_file",
                r#"{"path":"part_a.py","old_text":"return 0","new_text":"return 10"}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse("/v1/chat/completions", sse_text("part_a -> 10")),
        // Subtask 2 edits part_b.py
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "s2_1",
                "edit_file",
                r#"{"path":"part_b.py","old_text":"return 0","new_text":"return 20"}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse("/v1/chat/completions", sse_text("part_b -> 20")),
        // Orchestrator independently inspects BOTH files AND runs real unittest!
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("v1", "read_file", r#"{"path":"part_a.py"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("v2", "read_file", r#"{"path":"part_b.py"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4, 5]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "v3",
                "run_check",
                r#"{"program":"python3","args":["-m","unittest","test_parts.py"]}"#,
            ),
        ),
        // Orchestrator final verified answer
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4, 5, 6]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("Обе подзадачи выполнены, файлы проверены и unittest прошёл."),
        ),
    ];

    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 16384, "test-model".into());
    let cancel = Cancel::default();
    let mut saw_initial_unverified = 0usize;
    let mut saw_transition_verified = 0usize;
    let res = run_agent_with_policies(
        &mut backend,
        dir.path(),
        "Исправь part_a и part_b двумя подзадачами и проверь тестом",
        &[],
        AgentPolicy::WorkspaceWrite,
        ExecPolicy::AllowWorkspaceBuilds,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |ev| {
            if let AgentStreamEvent::Subtask(st) = ev {
                if st.status == "completed_unverified" && !st.verified {
                    saw_initial_unverified += 1;
                }
                if st.status == "verified" && st.verified {
                    saw_transition_verified += 1;
                }
            }
        },
    )
    .await
    .unwrap();

    assert_eq!(
        saw_initial_unverified, 2,
        "Обе подзадачи должны сначала завершиться как unverified"
    );
    assert_eq!(
        saw_transition_verified, 2,
        "Обе подзадачи должны перейти в verified после реальной проверки оркестратором"
    );
    assert_eq!(res.subtasks.len(), 2);
    assert!(res.subtasks.iter().all(|s| s.verified
        && s.verification_evidence.is_some()
        && !s.verified_file_hashes.is_empty()));
    assert_eq!(res.unverified_subtasks, 0);
}

#[tokio::test]
async fn unrelated_test_cannot_verify_subtask_and_subsequent_edit_invalidates_verification() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("part_a.py"), "def a():\n    return 0\n").unwrap();
    std::fs::write(dir.path().join("part_b.py"), "def b():\n    return 0\n").unwrap();
    std::fs::write(
        dir.path().join("test_unrelated.py"),
        "import unittest\nclass U(unittest.TestCase):\n    def test_ok(self):\n        self.assertTrue(True)\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("test_part_a.py"),
        "import unittest\nfrom part_a import a\nclass A(unittest.TestCase):\n    def test_a(self):\n        self.assertEqual(a(), 10)\n",
    )
    .unwrap();

    let sub_args = json!({
        "subtasks": [
            {"title": "Fix part_a", "instructions": "Return 10", "policy": "workspace_write", "owned_paths": ["part_a.py"]},
            {"title": "Fix part_b", "instructions": "Return 20", "policy": "workspace_write", "owned_paths": ["part_b.py"]}
        ]
    })
    .to_string();

    let replies = vec![
        // 1) Parent delegates 2 subtasks
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("p1", "delegate_subtasks", &sub_args),
        ),
        // Subtask 1 edits part_a.py
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "s1",
                "edit_file",
                r#"{"path":"part_a.py","old_text":"return 0","new_text":"return 10"}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse("/v1/chat/completions", sse_text("part_a -> 10")),
        // Subtask 2 edits part_b.py
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "s2",
                "edit_file",
                r#"{"path":"part_b.py","old_text":"return 0","new_text":"return 20"}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse("/v1/chat/completions", sse_text("part_b -> 20")),
        // 2) Orchestrator reads both files
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("r1", "read_file", r#"{"path":"part_a.py"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("r2", "read_file", r#"{"path":"part_b.py"}"#),
        ),
        // 3) Orchestrator runs UNRELATED test_unrelated.py -> must verify NEITHER subtask!
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4, 5]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "u1",
                "run_check",
                r#"{"program":"python3","args":["-m","unittest","test_unrelated.py"]}"#,
            ),
        ),
        // 4) Orchestrator runs test_part_a.py -> must verify ONLY Subtask 1 (part_a.py), NOT Subtask 2 (part_b.py)!
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4, 5, 6]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "t1",
                "run_check",
                r#"{"program":"python3","args":["-m","unittest","test_part_a.py"]}"#,
            ),
        ),
        // 5) Orchestrator then edits part_a.py again -> MUST INVALIDATE Subtask 1's verification!
        Reply::json(
            "POST",
            "/tokenize",
            json!({"tokens": [1, 2, 3, 4, 5, 6, 7]}),
        ),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "e1",
                "edit_file",
                r#"{"path":"part_a.py","old_text":"return 10","new_text":"return 11"}"#,
            ),
        ),
        // 6) Final response without re-running tests
        Reply::json(
            "POST",
            "/tokenize",
            json!({"tokens": [1, 2, 3, 4, 5, 6, 7, 8]}),
        ),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("Изменил part_a.py повторно без перезапуска тестов."),
        ),
    ];

    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 16384, "test-model".into());
    let cancel = Cancel::default();
    let mut verified_ids_seen = vec![];
    let mut invalidated_ids_seen = vec![];
    let res = run_agent_with_policies(
        &mut backend,
        dir.path(),
        "Проверь частичную верификацию и аннулирование",
        &[],
        AgentPolicy::WorkspaceWrite,
        ExecPolicy::AllowWorkspaceBuilds,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |ev| {
            if let AgentStreamEvent::Subtask(st) = ev {
                if st.verified && st.status == "verified" {
                    verified_ids_seen.push(st.title.clone());
                }
                if !st.verified && st.status == "invalidated_after_edit" {
                    invalidated_ids_seen.push(st.title.clone());
                }
            }
        },
    )
    .await
    .unwrap();

    // test_part_a.py verified ONLY "Fix part_a", NEVER "Fix part_b" (and test_unrelated.py verified neither)!
    assert_eq!(verified_ids_seen, vec!["Fix part_a".to_string()]);
    // Subsequent edit_file on part_a.py invalidated "Fix part_a"!
    assert_eq!(invalidated_ids_seen, vec!["Fix part_a".to_string()]);
    assert!(!res.subtasks[0].verified);
    assert_eq!(res.subtasks[0].status, "invalidated_after_edit");
    assert!(res.subtasks[0].verification_evidence.is_none());
    assert!(!res.subtasks[1].verified);
    assert_eq!(res.unverified_subtasks, 2);
}

#[tokio::test]
async fn detached_setsid_descendant_exposes_process_group_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let same_grp_pid_file = dir.path().join("same_group.pid");
    let detached_pid_file = dir.path().join("detached.pid");

    std::fs::write(
        dir.path().join("test_detached.py"),
        format!(
            concat!(
                "import subprocess, time, unittest\n",
                "class Detached(unittest.TestCase):\n",
                "    def test_spawn(self):\n",
                "        p_same = subprocess.Popen(['sleep', '60'])\n",
                "        p_det = subprocess.Popen(['sleep', '60'], start_new_session=True)\n",
                "        with open({:?}, 'w') as f:\n",
                "            f.write(str(p_same.pid))\n",
                "        with open({:?}, 'w') as f:\n",
                "            f.write(str(p_det.pid))\n",
                "        time.sleep(60)\n",
            ),
            same_grp_pid_file.to_string_lossy(),
            detached_pid_file.to_string_lossy()
        ),
    )
    .unwrap();

    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "r1",
                "run_check",
                r#"{"program":"python3","args":["-m","unittest","test_detached.py"]}"#,
            ),
        ),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 8192, "test-model".into());
    let cancel = Cancel::default();
    let cancel_bg = cancel.clone();
    let det_bg = detached_pid_file.clone();

    let killer = thread::spawn(move || {
        let start = Instant::now();
        while !det_bg.is_file() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            det_bg.is_file(),
            "PID отделившегося процесса должен быть записан"
        );
        cancel_bg.cancel();
    });

    let cancel_started = Instant::now();
    let res = run_agent_with_policies(
        &mut backend,
        dir.path(),
        "Запусти test_detached.py",
        &[],
        AgentPolicy::WorkspaceWrite,
        ExecPolicy::AllowWorkspaceBuilds,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |_| {},
    )
    .await
    .unwrap();
    let elapsed = cancel_started.elapsed();
    killer.join().unwrap();
    assert!(res.cancelled);
    assert!(
        elapsed < Duration::from_secs(5),
        "Отмена не должна зависать на 60с из-за удержанного pipe fd отделившегося потомка (прошло {:?})",
        elapsed
    );

    let same_pid: i32 = std::fs::read_to_string(&same_grp_pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let detached_pid: i32 = std::fs::read_to_string(&detached_pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let is_alive = |pid: i32| -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/status")) {
            Err(_) => false,
            Ok(st) => !st
                .lines()
                .find(|l| l.starts_with("State:"))
                .unwrap_or("")
                .contains("\tZ"),
        }
    };

    // 1) Потомок в той же группе процессов ДОЛЖЕН быть завершён через kill(-pgid, SIGKILL).
    let wait_start = Instant::now();
    while is_alive(same_pid) && wait_start.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !is_alive(same_pid),
        "Потомок в той же группе процессов (PID {same_pid}) должен завершиться при отмене"
    );

    // 2) Отделившийся через setsid() потомок НЕ попадает под kill(-pgid, SIGKILL) —
    // честно фиксируем это ограничение и прибираем тестовый PID вручную.
    let detached_survived_pgid_kill = is_alive(detached_pid);
    if detached_survived_pgid_kill {
        unsafe {
            libc::kill(detached_pid, libc::SIGKILL);
        }
    }
    assert!(
        detached_survived_pgid_kill,
        "Ожидалось честное наблюдение границы kill(-pgid): процесс после setsid() не покрывается без cgroup/PID-namespace"
    );
}

#[test]
fn github_architecture_svg_renders_cleanly_via_resvg() {
    let svg_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/kaic-architecture.svg");
    let bytes = std::fs::read(&svg_path).expect("assets/kaic-architecture.svg must exist");
    let (w, h, rgba) =
        kaic_studio::attachments::render_svg_premultiplied(&bytes, 1080).expect("valid SVG");
    assert!(w >= 900 && h >= 450);
    assert_eq!(rgba.len(), (w as usize) * (h as usize) * 4);
}

#[test]
fn seat_manager_supports_multiple_cursors_and_keyboards_with_disjoint_locks_and_live_edits() {
    use kaic_studio::agent::workspace_live::{
        LiveEditOp, MAX_AGENT_SEATS, SeatManager, probe_os_input_backends,
    };

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "x = 1\ny = 2\n").unwrap();
    std::fs::write(dir.path().join("b.py"), "name = 'old'\n").unwrap();
    std::fs::write(dir.path().join("c.py"), "z = 0\n").unwrap();
    let guard = WorkspaceGuard::new(dir.path()).unwrap();

    let mut mgr = SeatManager::new();
    // По умолчанию динамическое взаимодействие ОТКЛЮЧЕНО
    assert!(!mgr.is_dynamic_interaction_enabled());
    let s1_id = mgr
        .add_seat("agent-1", vec!["a.py".into()])
        .unwrap()
        .seat_id
        .clone();
    assert!(
        mgr.apply_live_edit(
            &guard,
            &s1_id,
            LiveEditOp::MoveCursor {
                file: "a.py".into(),
                line: 1,
                col: 1,
            },
        )
        .is_err()
    );
    // Явная активация пользователем
    mgr.set_dynamic_interaction(true);
    assert!(mgr.is_dynamic_interaction_enabled());
    let s2_id = mgr
        .add_seat("agent-2", vec!["b.py".into()])
        .unwrap()
        .seat_id
        .clone();
    let _s3_id = mgr
        .add_seat("agent-3", vec!["c.py".into()])
        .unwrap()
        .seat_id
        .clone();

    // Overlapping owned_paths must be rejected!
    assert!(mgr.add_seat("agent-conflict", vec!["a.py".into()]).is_err());

    // 4th seat with disjoint path succeeds; 5th exceeds MAX_AGENT_SEATS
    let _s4 = mgr.add_seat("agent-4", vec!["d.py".into()]).unwrap();
    assert_eq!(mgr.seats().len(), MAX_AGENT_SEATS);
    assert!(mgr.add_seat("agent-5", vec!["e.py".into()]).is_err());

    // Seat 1 & Seat 2 dynamically edit their own files with independent cursors + keyboards
    let r1 = mgr
        .apply_live_edit(
            &guard,
            &s1_id,
            LiveEditOp::ReplaceText {
                file: "a.py".into(),
                old_text: "y = 2".into(),
                new_text: "y = 42".into(),
            },
        )
        .unwrap();
    assert_eq!(r1.file, "a.py");
    assert_eq!(r1.new_line, 2);
    assert!(mgr.is_control_live());

    let r2 = mgr
        .apply_live_edit(
            &guard,
            &s2_id,
            LiveEditOp::InsertAt {
                file: "b.py".into(),
                line: 1,
                col: 13,
                text: " # edited".into(),
            },
        )
        .unwrap();
    assert_eq!(r2.file, "b.py");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.py")).unwrap(),
        "x = 1\ny = 42\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.py")).unwrap(),
        "name = 'old' # edited\n"
    );

    // Seat 2 trying to type into a.py (owned/locked by Seat 1) MUST be rejected!
    assert!(
        mgr.apply_live_edit(
            &guard,
            &s2_id,
            LiveEditOp::ReplaceText {
                file: "a.py".into(),
                old_text: "y = 42".into(),
                new_text: "y = 0".into(),
            },
        )
        .is_err()
    );

    // Disabling Seat 2's virtual keyboard blocks typing
    mgr.toggle_keyboard(&s2_id, false).unwrap();
    assert!(
        mgr.apply_live_edit(
            &guard,
            &s2_id,
            LiveEditOp::ReplaceText {
                file: "b.py".into(),
                old_text: "old".into(),
                new_text: "new".into(),
            },
        )
        .is_err()
    );

    // Emergency stop immediately trips hardware AtomicSeatGate, revokes control_live and disables all keyboards/locks
    mgr.emergency_stop();
    assert!(!mgr.is_control_live());
    assert!(mgr.is_emergency_stopped());
    assert!(
        mgr.seats()
            .iter()
            .all(|s| !s.control_live && !s.keyboard.enabled && !s.pointer.active)
    );
    // Even MoveCursor is blocked by AtomicSeatGate until explicitly resumed by user
    assert!(
        mgr.apply_live_edit(
            &guard,
            &s1_id,
            LiveEditOp::MoveCursor {
                file: "a.py".into(),
                line: 1,
                col: 1,
            },
        )
        .is_err()
    );
    // Re-enabling Seat 1 clears emergency stop latch and restores operation
    mgr.toggle_keyboard(&s1_id, true).unwrap();
    assert!(!mgr.is_emergency_stopped());
    let r_after = mgr
        .apply_live_edit(
            &guard,
            &s1_id,
            LiveEditOp::MoveCursor {
                file: "a.py".into(),
                line: 2,
                col: 1,
            },
        )
        .unwrap();
    assert_eq!(r_after.new_line, 2);
    #[cfg(target_arch = "x86_64")]
    assert!(mgr.seats().iter().any(|s| s.last_op_tsc > 0));

    // Honest host OS input backend probe
    let backends = probe_os_input_backends();
    assert!(
        backends
            .iter()
            .any(|b| b.id == "internal_workspace_seats" && b.status == FeatureStatus::Tested)
    );
    assert!(
        backends
            .iter()
            .any(|b| b.id == "wayland_virtual_input" && b.status == FeatureStatus::Unavailable)
    );
    assert!(
        backends
            .iter()
            .all(|b| b.freebsd_status == FeatureStatus::Unavailable)
    );
}

#[tokio::test]
async fn multi_agent_run_streams_distinct_seat_cursors_and_keyboards_for_orchestrator_and_subtasks()
{
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("part_a.py"), "a = 0\n").unwrap();
    std::fs::write(dir.path().join("part_b.py"), "b = 0\n").unwrap();

    let sub_args = json!({
        "subtasks": [
            {"title": "Edit A", "instructions": "Set a=1", "policy": "workspace_write", "owned_paths": ["part_a.py"]},
            {"title": "Edit B", "instructions": "Set b=2", "policy": "workspace_write", "owned_paths": ["part_b.py"]}
        ]
    })
    .to_string();

    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("p1", "delegate_subtasks", &sub_args),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "s1",
                "edit_file",
                r#"{"path":"part_a.py","old_text":"a = 0","new_text":"a = 1"}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse("/v1/chat/completions", sse_text("a=1")),
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "s2",
                "edit_file",
                r#"{"path":"part_b.py","old_text":"b = 0","new_text":"b = 2"}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse("/v1/chat/completions", sse_text("b=2")),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("Обе подзадачи отработали со своими курсорами."),
        ),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 16384, "test-model".into());
    let cancel = Cancel::default();
    let mut seat_events = vec![];
    let _res = run_agent_with_policies(
        &mut backend,
        dir.path(),
        "Делегируй правки A и B",
        &[],
        AgentPolicy::WorkspaceWrite,
        ExecPolicy::SafeInspectOnly,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |ev| {
            if let AgentStreamEvent::SeatUpdate(seat) = ev {
                seat_events.push(seat);
            }
        },
    )
    .await
    .unwrap();

    // Must observe distinct seats/keyboards for Subtask 1 (seat-2/kbd-2 on part_a.py) and Subtask 2 (seat-3/kbd-3 on part_b.py)
    assert!(seat_events.iter().any(|s| {
        s.seat_id == "seat-2"
            && s.keyboard.keyboard_id == "kbd-2"
            && s.pointer.target_file.as_deref() == Some("part_a.py")
    }));
    assert!(seat_events.iter().any(|s| {
        s.seat_id == "seat-3"
            && s.keyboard.keyboard_id == "kbd-3"
            && s.pointer.target_file.as_deref() == Some("part_b.py")
    }));
}

#[tokio::test]
async fn qwen_fallback_tool_call_repair_and_missing_arg_recovery() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "секретный-факт-42\n").unwrap();

    // Локальная модель не вернула Structured tool_calls, а отдала <tool_call> в тексте:
    // шаг 1: пустые аргументы (не хватает обязательного path) -> ошибка схемы -> самокоррекция;
    // шаг 2: <tool_call> с висящей запятой `{ "path": "note.txt", }` -> авто-ремонт JSON -> успех;
    // шаг 3: финальный текстовый ответ по реальному чтению.
    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("<tool_call>\n{\"name\":\"read_file\",\"arguments\":{}}\n</tool_call>"),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_text(
                "Проверяю файл:\n<tool_call>\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"note.txt\",}}\n</tool_call>",
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("В файле note.txt указано: секретный-факт-42."),
        ),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 8192, "Qwen2.5-Coder-7B".into());
    let cancel = Cancel::default();
    let mut events = vec![];
    let res = run_agent_with_policies(
        &mut backend,
        dir.path(),
        "Прочитай note.txt",
        &[],
        AgentPolicy::ReadOnly,
        ExecPolicy::SafeInspectOnly,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |ev| events.push(ev),
    )
    .await
    .unwrap();

    assert!(res.final_text.contains("секретный-факт-42"));
    // Сначала зафиксирован сбой отсутствующего обязательного параметра, затем успешный ремонт JSON
    assert!(events.iter().any(|e| matches!(
        e,
        AgentStreamEvent::Event(AgentEvent::ToolResult { name, ok: false, output_preview, .. })
            if name == "read_file" && output_preview.contains("path")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentStreamEvent::Event(AgentEvent::ToolResult { name, ok: true, output_preview, .. })
            if name == "read_file" && output_preview.contains("секретный-факт-42")
    )));
}

#[tokio::test]
async fn apply_patch_produces_diff_preview_and_drives_iterative_test_fix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("math_mod.py"),
        "def mul(a, b):\n    return a + b\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("test_math.py"),
        "import unittest\nfrom math_mod import mul\nclass T(unittest.TestCase):\n    def test_m(self):\n        self.assertEqual(mul(3, 4), 12)\n",
    )
    .unwrap();

    let patch = "--- a/math_mod.py\n+++ b/math_mod.py\n@@ -1,2 +1,2 @@\n def mul(a, b):\n-    return a + b\n+    return a * b\n";
    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "c1",
                "run_check",
                r#"{"program":"python3","args":["-m","unittest"]}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "c2",
                "apply_patch",
                &json!({"path": "math_mod.py", "patch": patch}).to_string(),
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call(
                "c3",
                "run_check",
                r#"{"program":"python3","args":["-m","unittest"]}"#,
            ),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1, 2, 3, 4, 5]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("Патч применён, unittest прошёл."),
        ),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 16384, "local-model".into());
    let cancel = Cancel::default();
    let mut events = vec![];
    let res = run_agent_with_policies(
        &mut backend,
        dir.path(),
        "Исправь math_mod.py патчем и проверь тестом",
        &[],
        AgentPolicy::WorkspaceWrite,
        ExecPolicy::AllowWorkspaceBuilds,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |ev| events.push(ev),
    )
    .await
    .unwrap();

    assert!(!res.blocked);
    assert!(res.changed_files.iter().any(|p| p == "math_mod.py"));
    assert!(
        std::fs::read_to_string(dir.path().join("math_mod.py"))
            .unwrap()
            .contains("a * b")
    );
    // apply_patch должен порождать unified diff_preview и ошибку анализа до исправления
    assert!(events.iter().any(|e| matches!(
        e,
        AgentStreamEvent::Event(AgentEvent::ToolResult { name, ok: false, output_preview, .. })
            if name == "run_check" && output_preview.contains("FAILED")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentStreamEvent::Event(AgentEvent::ToolResult {
            name,
            ok: true,
            diff_preview: Some(diff),
            ..
        }) if name == "apply_patch" && diff.contains("-    return a + b") && diff.contains("+    return a * b")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentStreamEvent::Event(AgentEvent::ToolResult { name, ok: true, .. })
            if name == "run_check"
    )));
}

#[tokio::test]
async fn context_usage_compaction_loop_guard_and_workspace_skills_work() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".kaic/skills/check_style")).unwrap();
    std::fs::write(
        dir.path().join(".kaic/skills/check_style/SKILL.md"),
        "---\nname: check-style\ndescription: \"Правило стиля\"\n---\n# Инструкция\nВсегда проверяй типы.\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();

    // Проверка list_skills, read_skill, traversal-отказа и защиты от зацикливания
    let replies = vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("s1", "list_skills", "{}"),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("s2", "read_skill", r#"{"id":"check_style"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("s3", "read_skill", r#"{"id":"../etc/passwd"}"#),
        ),
        // Повторяем один и тот же read_file 5 раз подряд -> на 5-м вызове сработает детектор цикла!
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("l1", "read_file", r#"{"path":"a.txt"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("l2", "read_file", r#"{"path":"a.txt"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("l3", "read_file", r#"{"path":"a.txt"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("l4", "read_file", r#"{"path":"a.txt"}"#),
        ),
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("l5", "read_file", r#"{"path":"a.txt"}"#),
        ),
    ];
    let (endpoint, _join) = fixture(replies);
    let mut backend = LocalHttpBackend::new(endpoint, 4096, "local-model".into());
    let cancel = Cancel::default();
    let mut saw_ctx = false;
    let mut events = vec![];
    let limits = AgentLimits {
        max_steps: 12,
        max_repeat_failures: 2,
        max_context_tokens: 1024,
        ..AgentLimits::default()
    };
    let res = run_agent_with_policies(
        &mut backend,
        dir.path(),
        "Проверь скиллы и избегай циклов",
        &[],
        AgentPolicy::ReadOnly,
        ExecPolicy::SafeInspectOnly,
        limits,
        &cancel,
        &ApprovalGate::deny_all(),
        |ev| {
            if matches!(ev, AgentStreamEvent::ContextUsage(_)) {
                saw_ctx = true;
            }
            events.push(ev);
        },
    )
    .await
    .unwrap();

    assert!(saw_ctx);
    assert!(res.blocked);
    assert!(res.final_text.contains("зацикленный повтор"));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentStreamEvent::Event(AgentEvent::ToolResult { name, ok: true, output_preview, .. })
            if name == "read_skill" && output_preview.contains("Всегда проверяй типы")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentStreamEvent::Event(AgentEvent::ToolResult { name, ok: false, .. })
            if name == "read_skill"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentStreamEvent::Event(AgentEvent::Blocked { reason })
            if reason.contains("зацикленный повтор")
    )));
}

#[tokio::test]
async fn ollama_openai_and_anthropic_backends_drive_native_loop_and_enforce_boundaries() {
    // 1. Границы адресов и согласий
    assert!(OllamaBackend::new("https://evil.example:11434", "m".into(), 4096).is_err());
    assert!(OpenAiCompatBackend::new("https://api.openai.com", "gpt".into(), None, false).is_err());
    assert!(OpenAiCompatBackend::new("http://api.openai.com", "gpt".into(), None, true).is_err());
    assert!(
        AnthropicApiBackend::new(
            "https://api.anthropic.com",
            "claude-sonnet-4-5".into(),
            Secret::new(String::new()),
            true,
        )
        .is_err()
    );
    assert!(
        AnthropicApiBackend::new(
            "https://api.anthropic.com",
            "claude-sonnet-4-5".into(),
            Secret::new("sk-ant-test".into()),
            false,
        )
        .is_err()
    );

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "Привет из файла!\n").unwrap();

    // 2. Ollama loopback (/api/tags + /api/chat NDJSON tool_calls -> native KAIC read_file -> final)
    let ollama_tags = Reply::json(
        "GET",
        "/api/tags",
        json!({"models": [{"name": "qwen2.5-coder:7b"}]}),
    );
    let ollama_chat_1 = Reply {
        method: "POST",
        path: "/api/chat",
        body: format!(
            "{}\n{}\n",
            json!({
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "data.txt"}}}]
                },
                "done": false
            }),
            json!({"done": true, "eval_count": 12, "eval_duration": 500_000_000u64})
        )
        .into_bytes(),
        status: 200,
        sse: false,
    };
    let ollama_chat_2 = Reply {
        method: "POST",
        path: "/api/chat",
        body: format!(
            "{}\n",
            json!({
                "message": {"role": "assistant", "content": "Ollama прочитала: Привет из файла!"},
                "done": true,
                "eval_count": 8,
                "eval_duration": 400_000_000u64
            })
        )
        .into_bytes(),
        status: 200,
        sse: false,
    };
    let (ep_ol, _j_ol) = fixture(vec![ollama_tags, ollama_chat_1, ollama_chat_2]);
    let mut ol =
        OllamaBackend::new(ep_ol.route("/").as_str(), "qwen2.5-coder:7b".into(), 8192).unwrap();
    assert_eq!(ol.list_models().await.unwrap(), vec!["qwen2.5-coder:7b"]);
    let cancel = Cancel::default();
    let res_ol = run_agent_with_policies(
        &mut ol,
        dir.path(),
        "Прочитай data.txt через Ollama",
        &[],
        AgentPolicy::ReadOnly,
        ExecPolicy::SafeInspectOnly,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |_| {},
    )
    .await
    .unwrap();
    assert!(res_ol.final_text.contains("Привет из файла!"));

    // 3. OpenAI-compat loopback (разрешён loopback HTTP без external consent!)
    let (ep_oa, _j_oa) = fixture(vec![
        Reply::sse(
            "/v1/chat/completions",
            sse_tool_call("o1", "read_file", r#"{"path":"data.txt"}"#),
        ),
        Reply::sse(
            "/v1/chat/completions",
            sse_text("OpenAI-compat видит data.txt."),
        ),
    ]);
    let mut oa =
        OpenAiCompatBackend::new(ep_oa.route("/").as_str(), "local-oa".into(), None, false)
            .unwrap();
    let res_oa = run_agent_with_policies(
        &mut oa,
        dir.path(),
        "Прочитай data.txt через OpenAI-compat",
        &[],
        AgentPolicy::ReadOnly,
        ExecPolicy::SafeInspectOnly,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |_| {},
    )
    .await
    .unwrap();
    assert!(res_oa.final_text.contains("OpenAI-compat"));

    // 4. Anthropic /v1/messages SSE with tool_use + Cyrillic UTF-8
    let anth_sse_1 = concat!(
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"data.txt\\\"}\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let anth_sse_2 = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Anthropic прочитал кириллицу: Привет!\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let (ep_an, _j_an) = fixture(vec![
        Reply::sse("/v1/messages", anth_sse_1.as_bytes()),
        Reply::sse("/v1/messages", anth_sse_2.as_bytes()),
    ]);
    let mut an = AnthropicApiBackend::new(
        ep_an.route("/").as_str(),
        "claude-sonnet-4-5".into(),
        Secret::new("sk-ant-local-mock".into()),
        false,
    )
    .unwrap();
    let res_an = run_agent_with_policies(
        &mut an,
        dir.path(),
        "Прочитай data.txt через Anthropic API",
        &[],
        AgentPolicy::ReadOnly,
        ExecPolicy::SafeInspectOnly,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |_| {},
    )
    .await
    .unwrap();
    assert!(res_an.final_text.contains("Anthropic прочитал кириллицу"));
}

#[tokio::test]
async fn claude_and_agy_cli_adapters_enforce_consent_exact_flags_and_never_skip_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    // Мок Claude Code CLI скрипт
    let claude_bin = dir.path().join("mock_claude.sh");
    let claude_argv = dir.path().join("claude_argv.txt");
    std::fs::write(
        &claude_bin,
        format!(
            "#!/usr/bin/env bash\nset -e\nprintf '%s\n' \"$@\" > \"{}\"\nprintf '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"Claude mock ok\"}}]}}}}\\n'\n",
            claude_argv.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&claude_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Мок Agy CLI скрипт
    let agy_bin = dir.path().join("mock_agy.sh");
    let agy_argv = dir.path().join("agy_argv.txt");
    std::fs::write(
        &agy_bin,
        format!(
            "#!/usr/bin/env bash\nset -e\nprintf '%s\n' \"$@\" > \"{}\"\nprintf '{{\"type\":\"result\",\"result\":\"Agy mock ok\"}}\\n'\n",
            agy_argv.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&agy_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let cancel = Cancel::default();
    let approve_all = ApprovalGate::callback(|req| ApprovalDecision {
        request_id: req.request_id,
        action_digest: req.action_digest,
        approved: true,
    });

    // Без external_consent запуск отклоняется ДО вызова бинарника!
    let denied = run_claude_cli_with_binary(
        &claude_bin,
        CodexCliConfig {
            workspace: ws.clone(),
            prompt: "test".into(),
            policy: AgentPolicy::ReadOnly,
            external_consent: false,
            model: None,
        },
        &cancel,
        &approve_all,
        |_| {},
    )
    .await;
    assert!(denied.is_err());
    assert!(!claude_argv.exists());

    // Отказ ApprovalGate::deny_all() при ApprovalRequired также блокирует запуск
    let launch_denied = run_claude_cli_with_binary(
        &claude_bin,
        CodexCliConfig {
            workspace: ws.clone(),
            prompt: "test".into(),
            policy: AgentPolicy::ApprovalRequired,
            external_consent: true,
            model: None,
        },
        &cancel,
        &ApprovalGate::deny_all(),
        |_| {},
    )
    .await
    .unwrap();
    assert!(launch_denied.blocked);
    assert!(!claude_argv.exists());

    // ReadOnly Claude CLI должен передавать --permission-mode plan и --restricted, НИКОГДА --dangerously-skip-permissions!
    let c_res = run_claude_cli_with_binary(
        &claude_bin,
        CodexCliConfig {
            workspace: ws.clone(),
            prompt: "Inspect repo".into(),
            policy: AgentPolicy::ReadOnly,
            external_consent: true,
            model: Some("sonnet".into()),
        },
        &cancel,
        &approve_all,
        |_| {},
    )
    .await
    .unwrap();
    assert!(!c_res.blocked);
    assert!(c_res.final_text.contains("Claude mock ok"));
    let c_args = std::fs::read_to_string(&claude_argv).unwrap();
    assert!(c_args.contains("--permission-mode"));
    assert!(c_args.contains("plan"));
    assert!(c_args.contains("--restricted"));
    assert!(c_args.contains("--no-session-persistence"));
    assert!(!c_args.contains("dangerously-skip-permissions"));

    // WorkspaceWrite Agy CLI должен передавать --sandbox и --mode accept-edits
    let a_res = run_agy_cli_with_binary(
        &agy_bin,
        CodexCliConfig {
            workspace: ws,
            prompt: "Inspect agy".into(),
            policy: AgentPolicy::WorkspaceWrite,
            external_consent: true,
            model: None,
        },
        &cancel,
        &approve_all,
        |_| {},
    )
    .await
    .unwrap();
    assert!(!a_res.blocked);
    assert!(a_res.final_text.contains("Agy mock ok"));
    let a_args = std::fs::read_to_string(&agy_argv).unwrap();
    assert!(a_args.contains("--print"));
    assert!(a_args.contains("--sandbox"));
    assert!(a_args.contains("--mode"));
    assert!(a_args.contains("accept-edits"));

    // Проверка probe_providers: чёткое разделение KaicNativeAgent vs ExternallyManagedCli
    let caps = probe_providers();
    assert_eq!(
        caps.iter()
            .find(|c| c.provider == ProviderKind::LocalLlama)
            .unwrap()
            .execution_origin,
        ExecutionOrigin::KaicNativeAgent
    );
    assert_eq!(
        caps.iter()
            .find(|c| c.provider == ProviderKind::ClaudeCli)
            .unwrap()
            .execution_origin,
        ExecutionOrigin::ExternallyManagedCli
    );
}

#[tokio::test]
async fn dynamic_interaction_is_disabled_by_default_across_runtime_and_seats() {
    // 1. В SeatManager по умолчанию редактирование курсором/клавиатурой запрещено
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.txt"), "old\n").unwrap();
    let mut mgr = SeatManager::new();
    assert!(!mgr.is_dynamic_interaction_enabled());
    assert!(!mgr.is_control_live());
    mgr.add_seat("agent-1", vec!["x.txt".into()]).unwrap();
    assert!(!mgr.seats()[0].pointer.enabled);
    assert!(!mgr.seats()[0].keyboard.enabled);
    let guard = WorkspaceGuard::new(dir.path()).unwrap();
    let err = mgr
        .apply_live_edit(
            &guard,
            "seat-1",
            LiveEditOp::ReplaceText {
                file: "x.txt".into(),
                old_text: "old".into(),
                new_text: "new".into(),
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("отключено"));

    // После явного включения — работает
    mgr.set_dynamic_interaction(true);
    assert!(mgr.is_dynamic_interaction_enabled());
    mgr.apply_live_edit(
        &guard,
        "seat-1",
        LiveEditOp::ReplaceText {
            file: "x.txt".into(),
            old_text: "old".into(),
            new_text: "new".into(),
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("x.txt")).unwrap(),
        "new\n"
    );

    // 2. В run_agent_with_policies с AgentLimits::default() все SeatUpdate остаются неактивными
    let (endpoint, _join) = fixture(vec![
        Reply::json("POST", "/tokenize", json!({"tokens": [1]})),
        Reply::sse("/v1/chat/completions", sse_text("ok")),
    ]);
    let mut backend = LocalHttpBackend::new(endpoint, 8192, "local".into());
    let cancel = Cancel::default();
    let mut seats = vec![];
    run_agent_with_policies(
        &mut backend,
        dir.path(),
        "Простой ответ",
        &[],
        AgentPolicy::ReadOnly,
        ExecPolicy::SafeInspectOnly,
        AgentLimits::default(),
        &cancel,
        &ApprovalGate::deny_all(),
        |ev| {
            if let AgentStreamEvent::SeatUpdate(s) = ev {
                seats.push(s);
            }
        },
    )
    .await
    .unwrap();
    assert!(!seats.is_empty());
    assert!(
        seats
            .iter()
            .all(|s| !s.control_live && !s.pointer.enabled && !s.keyboard.enabled)
    );
}
