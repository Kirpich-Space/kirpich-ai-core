use kaic_studio::desktop::hf::{parse_files, validate_file, validate_payload, validate_repo};
use serde_json::json;
#[test]
fn rejects_traversal_urls_and_split_shards() {
    for r in [
        "../a",
        "https://evil/a",
        "user/repo/extra",
        "user/repo?token=x",
        "user\\repo",
    ] {
        assert!(validate_repo(r).is_err(), "{r}");
    }
    assert!(validate_repo("ggml-org/models").is_ok());
    for f in [
        "../a.gguf",
        "/a.gguf",
        "a/../../b.gguf",
        "a%2fb.gguf",
        "a\\b.gguf",
        "model-00001-of-00002.gguf",
        "x.gguf.part",
    ] {
        assert!(validate_file(f).is_err(), "{f}");
    }
    assert!(validate_file("tinyllamas/stories260K.gguf").is_ok());
}
#[test]
fn metadata_uses_pinned_revision_size_and_lfs_hash() {
    let v = json!({"sha":"499bc8821c6b12b4e53c5bffcb21ec206f212d81","siblings":[{"rfilename":"tinyllamas/stories260K.gguf","size":1185376,"lfs":{"sha256":"270cba1bd5109f42d03350f60406024560464db173c0e387d91f0426d3bd256d","size":1185376}},{"rfilename":"x.txt"},{"rfilename":"x-00001-of-00002.gguf","size":10}]});
    let files = parse_files(&v).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].size, 1185376);
    assert_eq!(files[0].filename, "tinyllamas/stories260K.gguf");
    assert_eq!(files[0].revision, v["sha"]);
}
#[test]
fn corrupt_or_wrong_size_download_cannot_be_ready() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.part");
    std::fs::write(&p, b"not a gguf").unwrap();
    assert!(validate_payload(&p, 10, None).is_err());
    std::fs::write(&p, b"GGUFxxxx").unwrap();
    assert!(validate_payload(&p, 9, None).is_err());
    assert!(validate_payload(&p, 8, Some(&"0".repeat(64))).is_err());
    assert!(validate_payload(&p, 8, None).is_ok());
}
