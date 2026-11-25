//! Только явно обозначенные локальные тестовые HTTP backend. Это не инференс/медиа.
use kaic_studio::{
    kaic::*, llama::stream_chat, media::*, network::*, sse::StreamEvent, storage::Message,
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
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
}
struct Request {
    method: String,
    path: String,
    auth: bool,
    body: Vec<u8>,
}
fn fixture(replies: Vec<Reply>) -> (LocalEndpoint, thread::JoinHandle<Vec<Request>>) {
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
                    Err(_) => panic!("Тестовый accept не выполнен"),
                }
            };
            let request = read_request(&mut stream);
            assert_eq!(request.method, reply.method);
            assert_eq!(request.path, reply.path);
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
            if reply.sse {
                for byte in &reply.body {
                    stream.write_all(&[*byte]).unwrap();
                }
            } else {
                stream.write_all(&reply.body).unwrap();
            }
            received.push(request);
        }
        received
    });
    (endpoint, join)
}
fn read_request(s: &mut TcpStream) -> Request {
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
    let method = first.next().unwrap().into();
    let path = first.next().unwrap().into();
    let mut length = 0;
    let mut auth = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
            if name.eq_ignore_ascii_case("authorization") {
                auth = value.trim() == "Bearer test-only-token";
            }
        }
    }
    assert!(length < 2 * 1024 * 1024);
    let mut body = vec![0; length];
    s.read_exact(&mut body).unwrap();
    Request {
        method,
        path,
        auth,
        body,
    }
}
fn task(status: &str) -> Value {
    json!({"id":"t1","category":"coding","status":status,"context":[{"role":"user","content":"Вопрос","at":"2026-01-01T00:00:00Z"},{"role":"assistant","content":"Реальный транскрипт API","at":"2026-01-01T00:00:01Z"}]})
}
#[tokio::test(flavor = "current_thread")]
async fn kaic_auth_submit_poll_and_real_status() {
    let (ep, server) = fixture(vec![
        Reply::json("GET", "/status", json!({"ok":true})),
        Reply::json("GET", "/backend", json!({})),
        Reply::json("GET", "/models", json!([])),
        Reply::json("GET", "/local-models", json!([])),
        Reply::json("POST", "/tasks", task("running")),
        Reply::json("GET", "/tasks/t1", task("done")),
    ]);
    let c = KaicClient::new(ep, Secret::new("test-only-token".into())).unwrap();
    c.inspect().await.unwrap();
    let t = c.submit("Вопрос").await.unwrap();
    let mut seen = vec![];
    let end = c
        .follow(t, &Cancel::default(), |t| seen.push(t.status.clone()))
        .await
        .unwrap();
    assert_eq!(task_phase(&end.status), TaskPhase::Done);
    assert_eq!(end.context[1].content, "Реальный транскрипт API");
    assert_eq!(seen, vec!["running", "done"]);
    let requests = server.join().unwrap();
    assert!(requests.iter().all(|r| r.auth));
    let body: Value = serde_json::from_slice(&requests[4].body).unwrap();
    assert_eq!(body, json!({"text":"Вопрос","allow_manual":false}));
}
#[tokio::test(flavor = "current_thread")]
async fn kaic_cancel_only_own_task_and_wait_is_not_done() {
    let (ep, server) = fixture(vec![
        Reply::json("POST", "/tasks/t1/cancel", json!({})),
        Reply::json("GET", "/tasks/t1", task("cancelled")),
    ]);
    let c = KaicClient::new(ep, Secret::new("test-only-token".into())).unwrap();
    let cancel = Cancel::default();
    cancel.cancel();
    let t = c
        .follow(
            serde_json::from_value(task("running")).unwrap(),
            &cancel,
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(task_phase(&t.status), TaskPhase::Cancelled);
    server.join().unwrap();
    let (ep, server) = fixture(vec![]);
    let c = KaicClient::new(ep, Secret::new("test-only-token".into())).unwrap();
    let t = c
        .follow(
            serde_json::from_value(task("paused")).unwrap(),
            &Cancel::default(),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(task_phase(&t.status), TaskPhase::Waiting);
    server.join().unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn comfy_real_http_flow_download_and_metadata() {
    let history = json!({"p1":{"status":{"completed":true,"status_str":"success"},"outputs":{"9":{"images":[{"filename":"a b.png","subfolder":"тест","type":"output"}]}}}});
    let mut png = std::io::Cursor::new(vec![]);
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let (ep, server) = fixture(vec![
        Reply::json("GET", "/system_stats", json!({"system":{}})),
        Reply::json(
            "GET",
            "/object_info",
            json!({"CheckpointLoaderSimple":{"input":{"required":{"ckpt_name":[["model.safetensors"]]}}}}),
        ),
        Reply::json(
            "POST",
            "/prompt",
            json!({"prompt_id":"p1","node_errors":{}}),
        ),
        Reply::json("GET", "/history/p1", json!({})),
        Reply::json("GET", "/queue", json!({"queue_running":[[0,"p1"]]})),
        Reply::json("GET", "/history/p1", history),
        Reply {
            method: "GET",
            path: "/view?filename=a+b.png&subfolder=%D1%82%D0%B5%D1%81%D1%82&type=output",
            body: png.into_inner(),
            status: 200,
            sse: false,
        },
    ]);
    let c = ComfyClient::new(ep).unwrap();
    let info = c.inspect().await.unwrap();
    assert_eq!(model_choices(&info)[0].2, vec!["model.safetensors"]);
    let out = tempfile::tempdir().unwrap();
    let w = json!({"1":{"class_type":"KSampler","inputs":{"seed":42}}});
    let mut statuses = vec![];
    let r = c
        .run(
            w.clone(),
            "промпт".into(),
            out.path(),
            &Cancel::default(),
            |s| statuses.push(s),
        )
        .await
        .unwrap();
    assert_eq!(r.outputs.len(), 1);
    assert_eq!(r.seeds, vec![json!(42)]);
    assert!(statuses.iter().any(|s| s.contains("выполняет")));
    assert!(r.outputs[0].path.starts_with(out.path()));
    assert_eq!(image::open(&r.outputs[0].path).unwrap().width(), 1);
    let requests = server.join().unwrap();
    let body: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(body["prompt"], w);
    assert!(body["client_id"].is_string());
    assert!(!requests.iter().any(|r| r.path.contains("interrupt")));
}
#[tokio::test(flavor = "current_thread")]
async fn llama_shared_transport_streams_and_checks_template_budget() {
    let (ep, server) = fixture(vec![
        Reply::json("POST", "/apply-template", json!({"prompt":"<user>Привет"})),
        Reply::json("POST", "/tokenize", json!({"tokens":[1,2,3]})),
        Reply {
            method: "POST",
            path: "/v1/chat/completions",
            body:
                "data: {\"choices\":[{\"delta\":{\"content\":\"Ответ 🌑\"}}]}\n\ndata: [DONE]\n\n"
                    .as_bytes()
                    .to_vec(),
            status: 200,
            sse: true,
        },
    ]);
    let mut events = vec![];
    stream_chat(
        &ep,
        4096,
        &[Message::user("Привет".into())],
        64,
        &Cancel::default(),
        |e| events.push(e),
    )
    .await
    .unwrap();
    assert_eq!(
        events,
        vec![StreamEvent::Text("Ответ 🌑".into()), StreamEvent::Done]
    );
    let requests = server.join().unwrap();
    let body: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 64);
    let (ep, server) = fixture(vec![
        Reply::json("POST", "/apply-template", json!({"prompt":"x"})),
        Reply::json("POST", "/tokenize", json!({"tokens":vec![1;1000]})),
    ]);
    assert!(
        stream_chat(
            &ep,
            1024,
            &[Message::user("x".into())],
            64,
            &Cancel::default(),
            |_| {}
        )
        .await
        .is_err()
    );
    server.join().unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn cancellation_drops_own_stream_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let ep = LocalEndpoint::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = thread::spawn(move || {
        for body in [json!({"prompt":"test"}), json!({"tokens":[1]})] {
            let (mut s, _) = listener.accept().unwrap();
            read_request(&mut s);
            let body = serde_json::to_vec(&body).unwrap();
            s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
            s.write_all(&body).unwrap();
        }
        let (mut s, _) = listener.accept().unwrap();
        read_request(&mut s);
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000000\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n").unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut b = [0];
        let closed = match s.read(&mut b) {
            Ok(0) => true,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => true,
            _ => false,
        };
        let _ = closed_tx.send(closed);
    });
    let cancel = Cancel::default();
    let c = cancel.clone();
    let mut saw_text = false;
    let start = Instant::now();
    let r = stream_chat(&ep, 4096, &[Message::user("x".into())], 64, &cancel, |e| {
        if matches!(e, StreamEvent::Text(_)) {
            saw_text = true;
            c.cancel();
        }
    })
    .await;
    assert!(r.is_err());
    assert!(saw_text);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(
        tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .unwrap()
            .unwrap(),
        "Собственное SSE-соединение должно закрываться при отмене"
    );
    server.join().unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn redirects_are_not_followed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        read_request(&mut s);
        s.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://192.0.2.1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let r = client()
        .unwrap()
        .get(format!("http://{address}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 302);
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn comfy_cancel_monitor_does_not_interrupt_backend_and_is_recorded() {
    let (ep, server) = fixture(vec![Reply::json(
        "POST",
        "/prompt",
        json!({"prompt_id":"p2"}),
    )]);
    let c = ComfyClient::new(ep).unwrap();
    let cancel = Cancel::default();
    let stop = cancel.clone();
    let out = tempfile::tempdir().unwrap();
    let r = c
        .run(
            json!({"1":{"class_type":"TestFixture","inputs":{}}}),
            "test".into(),
            out.path(),
            &cancel,
            |_| {
                stop.cancel();
            },
        )
        .await;
    assert!(r.is_err());
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    let f = std::fs::read_dir(out.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let saved: MediaRecord = kaic_studio::storage::load_json(&f).unwrap();
    assert!(saved.state.contains("не отменён"));
    assert!(saved.outputs.is_empty());
    assert!(saved.error.is_some());
}
#[tokio::test(flavor = "current_thread")]
async fn comfy_execution_error_is_persisted_without_success() {
    let (ep, server) = fixture(vec![
        Reply::json("POST", "/prompt", json!({"prompt_id":"p3"})),
        Reply::json(
            "GET",
            "/history/p3",
            json!({"p3":{"status":{"status_str":"error","completed":false,"messages":[["execution_error",{}]]}}}),
        ),
    ]);
    let c = ComfyClient::new(ep).unwrap();
    let out = tempfile::tempdir().unwrap();
    assert!(
        c.run(
            json!({"1":{"class_type":"TestFixture","inputs":{}}}),
            "test".into(),
            out.path(),
            &Cancel::default(),
            |_| {}
        )
        .await
        .is_err()
    );
    server.join().unwrap();
    let f = std::fs::read_dir(out.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let saved: MediaRecord = kaic_studio::storage::load_json(&f).unwrap();
    assert_eq!(saved.state, "ошибка");
    assert!(saved.outputs.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn llama_done_without_text_is_not_chat_success() {
    let (ep, server) = fixture(vec![
        Reply::json("POST", "/apply-template", json!({"prompt":"x"})),
        Reply::json("POST", "/tokenize", json!({"tokens":[1]})),
        Reply {
            method: "POST",
            path: "/v1/chat/completions",
            body: b"data: [DONE]\n\n".to_vec(),
            status: 200,
            sse: true,
        },
    ]);
    assert!(
        stream_chat(
            &ep,
            4096,
            &[Message::user("x".into())],
            64,
            &Cancel::default(),
            |_| {}
        )
        .await
        .is_err()
    );
    server.join().unwrap();
}

#[test]
fn worker_rejects_second_admission_without_resetting_cancel() {
    use kaic_studio::service::{Job, Worker};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        read_request(&mut s);
        tx.send(()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut b = [0];
        let _ = s.read(&mut b);
    });
    let worker = Worker::new(|| {});
    worker.submit(Job::InspectComfy { url }).unwrap();
    rx.recv_timeout(Duration::from_secs(3)).unwrap();
    worker.cancel.cancel();
    let rejected = worker
        .submit(Job::Preview(std::path::PathBuf::from(
            "/nonexistent-test-file",
        )))
        .is_err();
    let kept = worker.cancel.is_cancelled();
    drop(worker);
    server.join().unwrap();
    assert!(
        rejected,
        "Второй submit во время задания должен быть отклонён"
    );
    assert!(kept, "Отклонённый submit не сбрасывает отмену");
}

fn hanging_last(
    replies: Vec<Reply>,
) -> (
    LocalEndpoint,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    thread::JoinHandle<Request>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let ep = LocalEndpoint::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let (started, rx) = std::sync::mpsc::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        for reply in replies {
            let (mut s, _) = listener.accept().unwrap();
            let r = read_request(&mut s);
            assert_eq!(r.path, reply.path);
            s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.body.len()
                )
                .as_bytes(),
            )
            .unwrap();
            s.write_all(&reply.body).unwrap();
        }
        let (mut s, _) = listener.accept().unwrap();
        let request = read_request(&mut s);
        started.send(()).unwrap();
        gate.recv_timeout(Duration::from_secs(4)).unwrap();
        request
    });
    (ep, rx, release, handle)
}
#[tokio::test(flavor = "current_thread")]
async fn cancelling_kaic_creation_has_uncertain_outcome_and_is_prompt() {
    let (ep, started, release, server) = hanging_last(vec![]);
    let client = KaicClient::new(ep, Secret::new("test-only-token".into())).unwrap();
    let cancel = Cancel::default();
    let c = cancel.clone();
    let trigger = thread::spawn(move || {
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        c.cancel();
    });
    let start = Instant::now();
    let error = client
        .submit_cancellable("test", &cancel)
        .await
        .unwrap_err()
        .to_string();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(error.contains("мог принять"));
    release.send(()).unwrap();
    assert_eq!(server.join().unwrap().path, "/tasks");
    trigger.join().unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn cancelling_comfy_creation_records_uncertainty() {
    let (ep, started, release, server) = hanging_last(vec![]);
    let client = ComfyClient::new(ep).unwrap();
    let cancel = Cancel::default();
    let c = cancel.clone();
    let trigger = thread::spawn(move || {
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        c.cancel();
    });
    let start = Instant::now();
    let out = tempfile::tempdir().unwrap();
    assert!(
        client
            .run(
                json!({"1":{"class_type":"TestFixture","inputs":{}}}),
                "test".into(),
                out.path(),
                &cancel,
                |_| {}
            )
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    release.send(()).unwrap();
    assert_eq!(server.join().unwrap().path, "/prompt");
    trigger.join().unwrap();
    let f = std::fs::read_dir(out.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record: MediaRecord = kaic_studio::storage::load_json(&f).unwrap();
    assert_eq!(record.state, "исход отправки неизвестен");
    assert!(record.prompt_id.is_empty());
}
#[tokio::test(flavor = "current_thread")]
async fn cancelling_comfy_view_headers_does_not_wait_for_http_timeout() {
    let (ep, started, release, server) = hanging_last(vec![
        Reply::json("POST", "/prompt", json!({"prompt_id":"p"})),
        Reply::json(
            "GET",
            "/history/p",
            json!({"p":{"status":{"status_str":"success","completed":true},"outputs":{"1":{"images":[{"filename":"a.png","subfolder":"","type":"output"}]}}}}),
        ),
    ]);
    let client = ComfyClient::new(ep).unwrap();
    let cancel = Cancel::default();
    let c = cancel.clone();
    let trigger = thread::spawn(move || {
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        c.cancel();
    });
    let start = Instant::now();
    let out = tempfile::tempdir().unwrap();
    assert!(
        client
            .run(
                json!({"1":{"class_type":"TestFixture","inputs":{}}}),
                "test".into(),
                out.path(),
                &cancel,
                |_| {}
            )
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    release.send(()).unwrap();
    assert!(server.join().unwrap().path.starts_with("/view?"));
    trigger.join().unwrap();
}
