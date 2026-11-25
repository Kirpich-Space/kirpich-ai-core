use kaic_studio::desktop::hf::Manager;
use serde_json::json;
#[tokio::test(flavor = "current_thread")]
async fn corrupt_registry_is_preserved_and_blocks_download_mutations() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("hf-jobs.json");
    std::fs::write(&p, b"broken registry").unwrap();
    let m = Manager::open(d.path().into());
    assert!(m.snapshot()["error"].is_string());
    assert!(m.api("/api/hf/start",&serde_json::to_vec(&json!({"repo":"ggml-org/models","filename":"tinyllamas/stories260K.gguf","directory":d.path()})).unwrap()).await.is_err());
    assert_eq!(std::fs::read(p).unwrap(), b"broken registry");
}
#[test]
fn selection_id_disambiguates_same_basename_in_different_hf_subdirectories() {
    use kaic_studio::catalog::Model;
    let m = |path: &str| Model {
        repo: "owner/model".into(),
        filename: "model.gguf".into(),
        path: path.into(),
        size: 8,
        sha256: "x".into(),
        status: "verified".into(),
        generation_tps: None,
        meets_10_tps: None,
        chat_exit: None,
    };
    assert_ne!(
        m("/models/a/model.gguf").selection_id(),
        m("/models/b/model.gguf").selection_id()
    );
}
