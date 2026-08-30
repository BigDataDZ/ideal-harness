use super::*;
use crate::{ToolRegistry, ToolSpec};
use std::sync::Arc;

fn workspace(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ih-plugins-{}-{name}", std::process::id()))
}

fn manifest_json(name: &str, hash: &str) -> String {
    serde_json::json!({
        "name": name,
        "version": "1.0.0",
        "payload": "payload.json",
        "hash": hash,
        "tools": [{
            "name": format!("{name}_hello"),
            "description": "Greet via plugin",
            "parameters_schema": { "type": "object", "properties": {} }
        }]
    })
    .to_string()
}

fn write_plugin(root: &Path, dir: &str, manifest: &str, payload: &str) {
    let dir = root.join(".harness/plugins").join(dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("manifest.json"), manifest).unwrap();
    fs::write(dir.join("payload.json"), payload).unwrap();
}

fn valid_plugin(root: &Path, dir: &str, payload: &str) {
    write_plugin(
        root,
        dir,
        &manifest_json(dir, &content_hash(payload.as_bytes())),
        payload,
    );
}

#[test]
fn valid_plugin_discovers_verifies_binds_and_dispatches_payload() {
    let root = workspace("valid");
    fs::remove_dir_all(&root).ok();
    let payload = r#"{"message":"hello from plugin"}"#;
    valid_plugin(&root, "greeter", payload);
    // 无关条目不产生隔离失败
    fs::create_dir_all(root.join(".harness/plugins")).unwrap();
    fs::write(root.join(".harness/plugins/notes.txt"), "stray").unwrap();

    let catalog = Arc::new(PluginCatalog::discover(&root).unwrap());
    assert!(catalog.failures().is_empty());
    let plugin = catalog.get("greeter").unwrap();
    assert_eq!(plugin.version(), "1.0.0");
    assert_eq!(plugin.tools()[0].name(), "greeter_hello");
    assert!(catalog
        .verify_capability("greeter", "greeter_hello")
        .is_ok());
    assert_eq!(
        catalog
            .verify_capability("greeter", "undeclared_tool")
            .unwrap_err()
            .code,
        ErrorCode::SandboxDenied
    );

    let mut registry = ToolRegistry::default();
    assert_eq!(
        catalog.bind_static_tools(&mut registry, "greeter").unwrap(),
        1
    );
    assert_eq!(registry.plugin_provenance("greeter_hello"), Some("greeter"));
    match registry.dispatch("greeter_hello", &serde_json::json!({})) {
        Some(ToolOutcome::Success { value }) => {
            assert_eq!(value["message"], "hello from plugin")
        }
        other => panic!("expected payload result, got {other:?}"),
    }
    fs::remove_dir_all(root).ok();
}

#[test]
fn hash_drift_is_quarantined_at_discovery_and_rejected_at_dispatch() {
    let root = workspace("drift");
    fs::remove_dir_all(&root).ok();
    let payload = r#"{"message":"original"}"#;
    valid_plugin(&root, "greeter", payload);
    let catalog = Arc::new(PluginCatalog::discover(&root).unwrap());
    let mut registry = ToolRegistry::default();
    catalog.bind_static_tools(&mut registry, "greeter").unwrap();

    // 发现后篡改 payload：能力校验与调度都必须拒绝（哈希漂移 fail-closed）
    fs::write(
        root.join(".harness/plugins/greeter/payload.json"),
        r#"{"message":"tampered"}"#,
    )
    .unwrap();
    assert_eq!(
        catalog
            .verify_capability("greeter", "greeter_hello")
            .unwrap_err()
            .code,
        ErrorCode::SandboxDenied
    );
    match registry.dispatch("greeter_hello", &serde_json::json!({})) {
        Some(ToolOutcome::Failure { error }) => {
            assert_eq!(error.code, ErrorCode::SandboxDenied)
        }
        other => panic!("expected fail-closed dispatch, got {other:?}"),
    }
    fs::remove_dir_all(root).ok();
}

#[test]
fn bad_plugin_is_quarantined_without_shadowing_good_plugins() {
    let root = workspace("mixed");
    fs::remove_dir_all(&root).ok();
    let good_payload = r#"{"ok":true}"#;
    valid_plugin(&root, "good", good_payload);
    // 坏插件：manifest 声明的哈希与 payload 实际内容不符
    write_plugin(
        &root,
        "bad",
        &manifest_json("bad", &content_hash(b"declared-content")),
        r#"{"evil":true}"#,
    );
    let catalog = PluginCatalog::discover(&root).unwrap();
    assert!(catalog.get("good").is_some());
    assert!(catalog.get("bad").is_none());
    assert_eq!(catalog.failures().len(), 1);
    assert_eq!(catalog.failures()[0].plugin, "bad");
    assert_eq!(catalog.failures()[0].stage, PluginFailureStage::Hash);
    assert!(catalog.verify_capability("good", "good_hello").is_ok());
    fs::remove_dir_all(root).ok();
}

#[test]
fn undeclared_capability_spec_drift_and_missing_gate_fail_closed() {
    let root = workspace("capability");
    fs::remove_dir_all(&root).ok();
    let payload = r#"{"message":"hi"}"#;
    valid_plugin(&root, "greeter", payload);
    let catalog = Arc::new(PluginCatalog::discover(&root).unwrap());

    // 门未安装：注册直接拒绝
    let mut ungated = ToolRegistry::default();
    assert_eq!(
        ungated
            .register_plugin_tool(
                "greeter",
                ToolSpec {
                    name: "greeter_hello".into(),
                    description: "Greet via plugin".into(),
                    parameters_schema: serde_json::json!({ "type": "object", "properties": {} }),
                    escalation_capable: false,
                },
                Box::new(|_| ToolOutcome::Success { value: ().into() }),
            )
            .unwrap_err()
            .code,
        ErrorCode::Internal
    );

    let mut registry = ToolRegistry::default();
    catalog.bind_static_tools(&mut registry, "greeter").unwrap();
    // 未声明能力：清单外的工具名拒绝注册
    assert_eq!(
        registry
            .register_plugin_tool(
                "greeter",
                ToolSpec {
                    name: "sneaky_extra".into(),
                    description: "not declared".into(),
                    parameters_schema: serde_json::json!({ "type": "object" }),
                    escalation_capable: false,
                },
                Box::new(|_| ToolOutcome::Success { value: ().into() }),
            )
            .unwrap_err()
            .code,
        ErrorCode::SandboxDenied
    );
    // spec 漂移：与声明不一致的 schema/描述拒绝注册
    for (label, spec) in [
        (
            "schema-drift",
            ToolSpec {
                name: "greeter_hello".into(),
                description: "Greet via plugin".into(),
                parameters_schema: serde_json::json!({ "type": "object", "properties": {
                    "extra": { "type": "string" }
                } }),
                escalation_capable: false,
            },
        ),
        (
            "description-drift",
            ToolSpec {
                name: "greeter_hello".into(),
                description: "different description".into(),
                parameters_schema: serde_json::json!({ "type": "object", "properties": {} }),
                escalation_capable: false,
            },
        ),
        (
            "escalation-capable",
            ToolSpec {
                name: "greeter_hello".into(),
                description: "Greet via plugin".into(),
                parameters_schema: serde_json::json!({ "type": "object", "properties": {} }),
                escalation_capable: true,
            },
        ),
    ] {
        let mut fresh = ToolRegistry::default();
        fresh.set_plugin_gate(Arc::clone(&catalog));
        assert_eq!(
            fresh
                .register_plugin_tool(
                    "greeter",
                    spec,
                    Box::new(|_| ToolOutcome::Success { value: ().into() }),
                )
                .unwrap_err()
                .code,
            ErrorCode::SandboxDenied,
            "{label} 必须 fail-closed"
        );
    }
    fs::remove_dir_all(root).ok();
}

#[test]
fn manifest_violations_are_quarantined_per_plugin() {
    let payload = r#"{"ok":true}"#;
    let hash = content_hash(payload.as_bytes());
    let cases: Vec<(&str, String)> = vec![
        (
            "unknown-field",
            format!(
                r#"{{"name":"unknown-field","version":"1.0.0","payload":"payload.json","hash":"{hash}","tools":[],"owner":"attacker"}}"#
            ),
        ),
        (
            "missing-tools",
            format!(
                r#"{{"name":"missing-tools","version":"1.0.0","payload":"payload.json","hash":"{hash}"}}"#
            ),
        ),
        (
            "payload-escape",
            format!(
                r#"{{"name":"payload-escape","version":"1.0.0","payload":"../outside.json","hash":"{hash}","tools":[]}}"#
            ),
        ),
        (
            "hash-format",
            r#"{"name":"hash-format","version":"1.0.0","payload":"payload.json","hash":"sha256:deadbeef","tools":[]}"#.into(),
        ),
        (
            "bad-tool-name",
            format!(
                r#"{{"name":"bad-tool-name","version":"1.0.0","payload":"payload.json","hash":"{hash}","tools":[{{"name":"../escape","description":"d","parameters_schema":{{}}}}]}}"#
            ),
        ),
    ];
    for (case, manifest) in cases {
        let root = workspace(&format!("manifest-{case}"));
        fs::remove_dir_all(&root).ok();
        write_plugin(&root, case, &manifest, payload);
        let catalog = PluginCatalog::discover(&root).unwrap();
        assert!(catalog.get(case).is_none(), "{case} 必须被隔离");
        assert_eq!(catalog.failures().len(), 1, "{case}");
        assert_eq!(catalog.failures()[0].stage, PluginFailureStage::Manifest);
        assert!(matches!(
            catalog.failures()[0].error.code,
            ErrorCode::ToolArgsInvalid | ErrorCode::SandboxDenied
        ));
        fs::remove_dir_all(root).ok();
    }
}

#[test]
fn duplicate_plugin_names_quarantine_both_entries() {
    let root = workspace("duplicate");
    fs::remove_dir_all(&root).ok();
    let payload = r#"{"ok":true}"#;
    write_plugin(
        &root,
        "one",
        &manifest_json("dupe", &content_hash(payload.as_bytes())),
        payload,
    );
    write_plugin(
        &root,
        "two",
        &manifest_json("dupe", &content_hash(payload.as_bytes())),
        payload,
    );
    let catalog = PluginCatalog::discover(&root).unwrap();
    assert!(catalog.get("dupe").is_none());
    assert_eq!(catalog.failures().len(), 2);
    let quarantined: Vec<_> = catalog
        .failures()
        .iter()
        .map(|failure| failure.plugin.as_str())
        .collect();
    assert_eq!(quarantined, vec!["one", "two"]);
    fs::remove_dir_all(root).ok();
}

#[test]
fn symlinked_plugin_dir_is_quarantined_when_platform_allows_creation() {
    let root = workspace("symlink");
    let outside = workspace("outside");
    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&outside).ok();
    fs::create_dir_all(root.join(".harness/plugins")).unwrap();
    let payload = r#"{"ok":true}"#;
    valid_plugin(&outside, "real", payload);
    let source = outside.join(".harness/plugins/real");
    let target = root.join(".harness/plugins/linked");
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(&source, &target).is_ok();
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_dir(&source, &target).is_ok();
    if created {
        let catalog = PluginCatalog::discover(&root).unwrap();
        assert!(catalog.get("real").is_none());
        assert_eq!(catalog.failures().len(), 1);
        assert_eq!(catalog.failures()[0].stage, PluginFailureStage::Containment);
    }
    fs::remove_dir_all(root).ok();
    fs::remove_dir_all(outside).ok();
}

#[test]
fn missing_plugin_root_is_empty_catalog_and_bad_workspace_fails() {
    let root = workspace("empty");
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    let catalog = PluginCatalog::discover(&root).unwrap();
    assert_eq!(catalog.plugins().count(), 0);
    assert!(catalog.failures().is_empty());
    fs::remove_dir_all(&root).ok();
    assert_eq!(
        PluginCatalog::discover(&root).unwrap_err().code,
        ErrorCode::Internal
    );
}

#[test]
fn manifest_name_must_match_plugin_directory() {
    let root = workspace("name-mismatch");
    fs::remove_dir_all(&root).ok();
    let payload = r#"{"ok":true}"#;
    write_plugin(
        &root,
        "directory-name",
        &manifest_json("manifest-name", &content_hash(payload.as_bytes())),
        payload,
    );
    let catalog = PluginCatalog::discover(&root).unwrap();
    assert_eq!(catalog.failures().len(), 1);
    assert_eq!(catalog.failures()[0].stage, PluginFailureStage::Manifest);
    fs::remove_dir_all(root).ok();
}
