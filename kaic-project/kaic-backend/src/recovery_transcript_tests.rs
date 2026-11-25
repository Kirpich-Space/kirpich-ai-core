use super::*;

#[test]
fn damaged_requests_and_orphan_replies_are_system_notes_for_the_provider() {
    for (content, id) in [
        ("invalid json", Some("call")),
        (r#"{"name":"write","arguments":"{}"}"#, None),
        (r#"{"name":"write","arguments":"{}"}"#, Some("")),
    ] {
        let context = vec![
            ContextEntry {
                role: TOOL_REQUEST_ROLE.into(),
                content: content.into(),
                at: Utc::now(),
                tool_call_id: id.map(str::to_string),
            },
            ContextEntry {
                role: "tool".into(),
                content: "Неизвестный исход подтверждён; повтор не разрешён".into(),
                at: Utc::now(),
                tool_call_id: Some(id.unwrap_or("call").into()),
            },
        ];
        let messages = context_to_messages(&context);
        assert!(messages.iter().all(|message| message.role == Role::System));
        assert!(messages
            .iter()
            .all(|message| message.tool_call_id.is_none()));
        assert_eq!(messages[0].content, content);
    }
}
