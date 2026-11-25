use kaic_studio::desktop::hf::resume_offset;
#[test]
fn resume_uses_only_exact_content_range_or_restarts_from_zero() {
    assert_eq!(resume_offset(200, None, 42, 100).unwrap(), 0);
    assert_eq!(
        resume_offset(206, Some("bytes 42-99/100"), 42, 100).unwrap(),
        42
    );
    for h in [
        None,
        Some("bytes 0-99/100"),
        Some("bytes 42-98/100"),
        Some("bytes 42-99/101"),
    ] {
        assert!(resume_offset(206, h, 42, 100).is_err());
    }
    assert!(resume_offset(416, None, 42, 100).is_err());
}
