use kaic_studio::{
    catalog::{Model, resolve_selection},
    storage::{Chat, ChatOptions},
};
#[test]
fn selection_identifies_exact_file_with_legacy_repo_fallback() {
    let make = |name: &str| Model {
        repo: "owner/model".into(),
        filename: name.into(),
        path: format!("/models/{name}").into(),
        size: 8,
        sha256: "".into(),
        status: "verified".into(),
        generation_tps: None,
        meets_10_tps: None,
        chat_exit: None,
    };
    let models = vec![make("model-Q4.gguf"), make("model-Q8.gguf")];
    assert_ne!(models[0].selection_id(), models[1].selection_id());
    assert_eq!(
        resolve_selection(&models, &models[1].selection_id())
            .unwrap()
            .path,
        models[1].path
    );
    assert_eq!(
        resolve_selection(&models, "owner/model").unwrap().path,
        models[0].path
    );
}
#[test]
fn chat_options_persist_and_inject_system_and_skills_before_budget() {
    let mut c = Chat::new();
    c.options = ChatOptions {
        context: 8192,
        system_prompt: "Answer carefully".into(),
        skills: "Use Rust".into(),
        ..Default::default()
    };
    let msgs = c
        .options
        .messages(vec![kaic_studio::storage::Message::user("hi".into())])
        .unwrap();
    assert_eq!(msgs[0].role, "system");
    assert!(msgs[0].content.contains("Answer carefully"));
    assert!(msgs[0].content.contains("Use Rust"));
    assert_eq!(msgs[1].content, "hi");
    let restored: Chat = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
    assert_eq!(restored.options.context, 8192);
    assert_eq!(restored.options.skills, "Use Rust");
    assert!(
        ChatOptions {
            context: 1,
            ..Default::default()
        }
        .messages(vec![])
        .is_err()
    );
}
