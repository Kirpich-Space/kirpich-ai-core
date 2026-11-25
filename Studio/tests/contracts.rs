use kaic_studio::{attachments::*, catalog::*, kaic::*, media::*, network::*, sse::*, storage::*};
use serde_json::json;
use std::path::Path;

#[test]
fn sse_every_byte_utf8_and_crlf() {
    let wire =
        "data: {\"choices\":[{\"delta\":{\"content\":\"Привет 🌑\"}}]}\r\n\r\ndata: [DONE]\n\n";
    let mut decoder = SseDecoder::default();
    let mut events = vec![];
    for b in wire.as_bytes() {
        events.extend(decoder.push(&[*b]).unwrap());
    }
    assert_eq!(
        events,
        vec![StreamEvent::Text("Привет 🌑".into()), StreamEvent::Done]
    );
    decoder.finish().unwrap();
}
#[test]
fn sse_errors_and_truncation_are_not_success() {
    assert!(
        SseDecoder::default()
            .push(b"data: {\"error\":{\"message\":\"bad\"}}\n\n")
            .is_err()
    );
    let mut d = SseDecoder::default();
    d.push(b"data: {\"choices\":[{\"delta\":{}}]}\n\n").unwrap();
    assert!(d.finish().is_err());
    assert!(SseDecoder::default().push(b"data: broken\n\n").is_err());
}
#[test]
fn sse_comments_multiline_reasoning_and_finish() {
    let mut d = SseDecoder::default();
    let ev = d.push(b": ping\n\ndata: {\"choices\":\ndata: [{\"delta\":{\"reasoning_content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").unwrap();
    assert_eq!(
        ev,
        vec![StreamEvent::Reasoning("x".into()), StreamEvent::Done]
    );
}
#[test]
fn catalog_requires_exact_path_status_and_size() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.gguf");
    std::fs::write(&file, b"GGUF").unwrap();
    let mut model: Model = serde_json::from_value(json!({"repo":"r", "filename":"a.gguf","path":file,"size":4,"sha256":"abc","status":"verified-existing"})).unwrap();
    assert!(model.loadable());
    model.status = "downloading".into();
    assert!(!model.loadable());
    model.status = "verified".into();
    model.filename = "a.gguf.part".into();
    assert!(!model.loadable());
    model.filename = "a.gguf".into();
    model.size = 7;
    assert!(!model.loadable());
    let mut m = model.clone();
    m.size = 4;
    assert!(!benchmark_matches(
        &m,
        &json!({"repo":"r","path":"/wrong/a.gguf","sha256":"abc"})
    ));
    assert!(benchmark_matches(
        &m,
        &json!({"repo":"r","path":file,"sha256":"abc"})
    ));
}
#[test]
fn profile_selects_by_repo_not_alias() {
    let v = json!({"r":{"context":32768,"cache":"q8_0","flags":["-ncmoe","4"],"threads":8}});
    assert_eq!(profile_from_value(&v, "r").unwrap().context, 32768);
    assert_eq!(profile_from_value(&v, "other").unwrap().context, 4096);
    assert!(profile_from_value(&json!({"r":{"context":0}}), "r").is_err());
}
#[test]
fn attachment_limits_binary_and_numbering() {
    assert!(validate_text("x.rs", b"a\0b", 0).is_err());
    assert!(validate_text("x.png", b"abc", 0).is_err());
    assert!(validate_text("x.rs", &vec![b'a'; MAX_FILE_BYTES + 1], 0).is_err());
    assert!(validate_text("x.rs", b"abc", MAX_TOTAL_BYTES).is_err());
    let a = validate_text("x.rs", "a\nб".as_bytes(), 0).unwrap();
    assert!(a.for_prompt().contains("2 | б"));
}
#[test]
fn loopback_only_no_auth_query_or_redirect() {
    for u in ["http://127.0.0.1:4545", "http://[::1]:8188"] {
        assert!(LocalEndpoint::parse(u).is_ok());
    }
    for u in [
        "https://example.com",
        "http://127.0.0.1.evil",
        "http://0.0.0.0",
        "http://user:pass@127.0.0.1",
        "file:///tmp/x",
        "http://127.0.0.1/?token=x",
        "http://localhost:80",
    ] {
        assert!(LocalEndpoint::parse(u).is_err(), "{u}");
    }
}
#[test]
fn atomic_persistence_and_corruption_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("state.json");
    let mut state = SavedState::default();
    state.chats.push(Chat::new());
    save_json(&p, &state).unwrap();
    assert_eq!(load_json::<SavedState>(&p).unwrap().chats.len(), 1);
    std::fs::write(&p, "{broken").unwrap();
    assert!(load_json::<SavedState>(&p).is_err());
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "{broken");
}
#[test]
fn kaic_terminal_and_waiting_states() {
    assert_eq!(task_phase("done"), TaskPhase::Done);
    assert_eq!(task_phase("paused"), TaskPhase::Waiting);
    assert_eq!(task_phase("waiting_for_human"), TaskPhase::Waiting);
    assert_eq!(task_phase("failed"), TaskPhase::Failed);
    assert_eq!(task_phase("cancelled"), TaskPhase::Cancelled);
    assert_eq!(task_phase("something_new"), TaskPhase::Active);
    let s = Secret::new("sensitive-token".into());
    assert!(!format!("{s:?}").contains("sensitive"));
}
#[test]
fn comfy_errors_and_completion() {
    assert!(parse_history(&json!({"id":{"status":{"status_str":"error","completed":false,"messages":[["execution_error",{"exception_message":"broken"}]]}}}),"id").is_err());
    assert!(parse_history(&json!({}), "id").unwrap().is_none());
    let h = json!({"id":{"status":{"status_str":"success","completed":true},"outputs":{"9":{"images":[{"filename":"a.png","subfolder":"","type":"output"}]}}}});
    assert_eq!(parse_history(&h, "id").unwrap().unwrap().len(), 1);
    assert!(
        parse_history(
            &json!({"id":{"status":{"status_str":"success","completed":true},"outputs":{}}}),
            "id"
        )
        .is_err()
    );
}
#[test]
fn comfy_safe_names_and_prompt_node() {
    for bad in ["../a.png", "/a.png", "a\\b.png", "..", ""] {
        assert!(safe_output_name("id", 0, bad).is_err());
    }
    assert!(
        safe_output_name("id", 0, "a.png")
            .unwrap()
            .ends_with(".png")
    );
    let mut w = json!({"1":{"class_type":"CLIPTextEncode","inputs":{"text":"positive"}},"2":{"inputs":{"text":"negative"}}});
    set_positive_prompt(&mut w, "1", "new").unwrap();
    assert_eq!(w["2"]["inputs"]["text"], "negative");
    assert!(set_positive_prompt(&mut w, "3", "bad").is_err());
    assert!(safe_output_name("id", 0, "a.exe").is_err());
}
#[test]
fn context_budget_rejects_without_truncation() {
    assert!(
        kaic_studio::llama::check_budget(&[Message::user("a".repeat(5000))], 1024, 64).is_err()
    );
    assert!(kaic_studio::llama::check_budget(&[Message::user("Привет".into())], 4096, 64).is_ok());
}
#[test]
fn server_args_keep_auto_gpu_and_allowlisted_profile_flags() {
    let p = Profile {
        flags: vec!["-ncmoe".into(), "4".into()],
        ..Profile::default()
    };
    let args = kaic_studio::llama::server_args(Path::new("/tmp/a.gguf"), 12345, &p).unwrap();
    assert!(!args.iter().any(|x| x == "-ngl"));
    assert!(args.windows(2).any(|x| x == ["--fit", "on"]));
    let bad = Profile {
        flags: vec!["--host".into(), "0.0.0.0".into()],
        ..p
    };
    assert!(kaic_studio::llama::server_args(Path::new("/tmp/a.gguf"), 12345, &bad).is_err());
}

#[test]
fn downloaded_html_is_not_media() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("x.png");
    std::fs::write(&p, b"<html>server error</html>").unwrap();
    assert!(validate_artifact(&p).is_err());
}
#[test]
fn output_subfolder_cannot_traverse() {
    let h = json!({"p":{"status":{"status_str":"success","completed":true},"outputs":{"1":{"videos":[{"filename":"a.mp4","subfolder":"../private","type":"output"}]}}}});
    assert!(parse_history(&h, "p").is_err());
}

#[test]
fn sse_limit_and_named_errors_are_explicit() {
    let mut d = SseDecoder::default();
    assert_eq!(d.push(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n").unwrap(),vec![StreamEvent::Limit,StreamEvent::Done]);
    assert!(
        SseDecoder::default()
            .push(b"event: error\ndata: [DONE]\n\n")
            .is_err()
    );
}

#[test]
fn png_signature_alone_is_not_valid_image() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("broken.png");
    std::fs::write(&p, b"\x89PNG\r\n\x1a\nnot-an-image").unwrap();
    assert!(validate_artifact(&p).is_err());
}
#[test]
fn persistence_refuses_unreadable_size_and_keeps_previous() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("state.json");
    save_json(&p, &json!({"old":true})).unwrap();
    assert!(save_json(&p, &"x".repeat(32 * 1024 * 1024)).is_err());
    assert_eq!(
        load_json::<serde_json::Value>(&p).unwrap(),
        json!({"old":true})
    );
}

#[test]
fn explicit_invalid_data_dir_never_falls_back() {
    use std::path::PathBuf;
    assert!(
        resolve_data_dir(
            Some(PathBuf::from("relative")),
            None,
            Some(PathBuf::from("/home/test"))
        )
        .is_err()
    );
    assert!(
        resolve_data_dir(
            None,
            Some(PathBuf::from("relative")),
            Some(PathBuf::from("/home/test"))
        )
        .is_err()
    );
    assert_eq!(
        resolve_data_dir(
            Some(PathBuf::from("/qa")),
            Some(PathBuf::from("/xdg")),
            Some(PathBuf::from("/home/test"))
        )
        .unwrap(),
        PathBuf::from("/qa")
    );
}
#[test]
fn fake_video_header_is_not_validated_media() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("bad.mp4");
    std::fs::write(&p, b"\0\0\0\x18ftypisom0000000000000000").unwrap();
    assert!(validate_artifact(&p).is_err());
}
