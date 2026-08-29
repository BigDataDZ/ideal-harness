use super::*;

fn user_role(model: &str, tools: &str) -> String {
    format!(
        r#"[{{
          "nickname":"reviewer",
          "description":"Review code",
          "instructions":"Find correctness issues and cite evidence.",
          "model":"{model}",
          "allowed_tools":[{tools}],
          "denied_tools":[]
        }}]"#
    )
}

fn parent_policy() -> SubagentPolicy {
    SubagentPolicy::new(
        3,
        2,
        10,
        10_000,
        ["deepseek-chat".into(), "model-a".into()],
        ["read".into(), "search".into()],
        ["search".into()],
    )
    .unwrap()
}

#[test]
fn built_in_and_user_roles_parse_into_sorted_catalog() {
    let user = user_role("model-a", r#""read""#);
    let catalog = RoleCatalog::with_builtins(Some(&user)).unwrap();
    assert_eq!(
        catalog.nicknames().collect::<Vec<_>>(),
        vec!["researcher", "reviewer"]
    );
    assert_eq!(catalog.get("researcher").unwrap().model(), "deepseek-chat");
    assert_eq!(
        catalog.get("reviewer").unwrap().description(),
        "Review code"
    );
    assert_eq!(
        catalog.get("missing").unwrap_err().code,
        ErrorCode::ToolArgsInvalid
    );
}

#[test]
fn unknown_malicious_blank_and_duplicate_configuration_fails_closed() {
    let unknown = r#"[{"nickname":"x","description":"d","instructions":"i","model":"m","allowed_tools":[],"denied_tools":[],"__proto__":{}}]"#;
    assert!(parse_roles(unknown).is_err());
    let blank = r#"[{"nickname":" ","description":"d","instructions":"i","model":"m","allowed_tools":[],"denied_tools":[]}]"#;
    assert!(parse_roles(blank).is_err());
    let duplicate_tool = user_role("model-a", r#""read","read""#);
    assert!(parse_roles(&duplicate_tool).is_err());
    let duplicate_roles = [
        parse_roles(&user_role("model-a", r#""read""#)).unwrap(),
        parse_roles(&user_role("model-a", r#""read""#)).unwrap(),
    ]
    .concat();
    assert!(RoleCatalog::from_roles(duplicate_roles).is_err());
    assert!(parse_roles("{}").is_err());
}

#[test]
fn role_generates_deterministic_bounded_subtask_configuration() {
    let role = parse_roles(&user_role("model-a", r#""search","read""#))
        .unwrap()
        .remove(0);
    let identity = RoleTaskIdentity::new("task-1", "Inspect module", "root", "child-1").unwrap();
    let budget = RoleTaskBudget::new(2, 0, 4, 2_000).unwrap();
    let first = role
        .build_subtask(&identity, &budget, &parent_policy())
        .unwrap();
    let second = role
        .build_subtask(&identity, &budget, &parent_policy())
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.role_nickname(), "reviewer");
    assert_eq!(first.request().model(), Some("model-a"));
    assert_eq!(
        first
            .request()
            .tools()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["read"]
    );
    assert!(first.task().prompt().starts_with(role.instructions()));
    assert!(first.child_policy().denied_tools().contains("search"));
}

#[test]
fn role_override_cannot_expand_parent_model_tool_or_budget() {
    let identity = RoleTaskIdentity::new("task-1", "Inspect", "root", "child-1").unwrap();
    let budget = RoleTaskBudget::new(2, 0, 4, 2_000).unwrap();
    let bad_model = parse_roles(&user_role("model-b", r#""read""#))
        .unwrap()
        .remove(0);
    assert_eq!(
        bad_model
            .build_subtask(&identity, &budget, &parent_policy())
            .unwrap_err()
            .code,
        ErrorCode::SandboxDenied
    );
    let bad_tool = parse_roles(&user_role("model-a", r#""exec""#))
        .unwrap()
        .remove(0);
    assert_eq!(
        bad_tool
            .build_subtask(&identity, &budget, &parent_policy())
            .unwrap_err()
            .code,
        ErrorCode::SandboxDenied
    );
    let excessive = RoleTaskBudget::new(4, 0, 4, 2_000).unwrap();
    let role = parse_roles(&user_role("model-a", r#""read""#))
        .unwrap()
        .remove(0);
    assert_eq!(
        role.build_subtask(&identity, &excessive, &parent_policy())
            .unwrap_err()
            .code,
        ErrorCode::SandboxDenied
    );
}
