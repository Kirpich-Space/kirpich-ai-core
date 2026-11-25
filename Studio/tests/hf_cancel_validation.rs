use kaic_studio::desktop::hf::validate_payload_cancellable;
use std::sync::atomic::AtomicBool;
#[test]
fn cancellation_stops_validation_without_publishing_a_ready_file() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("model.gguf.part");
    std::fs::write(&p, b"GGUFxxxx").unwrap();
    assert!(validate_payload_cancellable(&p, 8, None, &AtomicBool::new(true)).is_err());
    assert!(p.exists());
    assert!(!d.path().join("model.gguf").exists());
}
