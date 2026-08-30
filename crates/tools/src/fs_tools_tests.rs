use super::*;
use crate::{ToolOutcome, ToolRegistry};
use std::path::PathBuf;
use std::sync::Arc;

fn workspace(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ih-fs-tools-{}-{name}", std::process::id()))
}

struct Harness {
    root: PathBuf,
    registry: ToolRegistry,
}

impl Drop for Harness {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

fn setup(name: &str) -> (Harness, Arc<FsToolSet>) {
    let root = workspace(name);
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();
    let set = FsToolSet::new(&root).unwrap();
    let mut registry = ToolRegistry::default();
    set.register(&mut registry);
    (Harness { root, registry }, set)
}

fn ok(result: Option<ToolOutcome>) -> serde_json::Value {
    match result {
        Some(ToolOutcome::Success { value }) => value,
        Some(ToolOutcome::Failure { error }) => {
            panic!("expected success, got failure: {error:?}")
        }
        None => panic!("expected success, got unknown tool"),
    }
}

fn err(result: Option<ToolOutcome>) -> protocol::ErrorEnvelope {
    match result {
        Some(ToolOutcome::Failure { error }) => error,
        Some(ToolOutcome::Success { value }) => panic!("expected failure, got success: {value}"),
        None => panic!("expected failure, got unknown tool"),
    }
}

#[test]
fn write_then_read_roundtrip_and_read_before_write_is_enforced() {
    let (h, _set) = setup("roundtrip");
    // 新文件可以直接写
    ok(h.registry.dispatch(
        "fs_write",
        &serde_json::json!({ "path": "note.txt", "content": "hello harness" }),
    ));
    let value = ok(h
        .registry
        .dispatch("fs_read", &serde_json::json!({ "path": "note.txt" })));
    assert_eq!(value["content"], "hello harness");

    // 另一个实例未读过就覆盖 → read-before-write 拒绝
    let (h2, _set2) = setup("roundtrip-no-read");
    ok(h2.registry.dispatch(
        "fs_write",
        &serde_json::json!({ "path": "note.txt", "content": "v1" }),
    ));
    let fresh = FsToolSet::new(&h2.root).unwrap();
    let mut reg3 = ToolRegistry::default();
    fresh.register(&mut reg3);
    let error = err(reg3.dispatch(
        "fs_write",
        &serde_json::json!({ "path": "note.txt", "content": "v2" }),
    ));
    assert_eq!(error.code, ErrorCode::SandboxDenied);
    // 该实例读过之后覆盖放行
    ok(reg3.dispatch("fs_read", &serde_json::json!({ "path": "note.txt" })));
    ok(reg3.dispatch(
        "fs_write",
        &serde_json::json!({ "path": "note.txt", "content": "v2" }),
    ));
    assert_eq!(
        std::fs::read_to_string(h2.root.join("note.txt")).unwrap(),
        "v2"
    );
}

#[test]
fn edit_requires_read_unique_anchor_and_leaves_file_unchanged_on_failure() {
    let (h, _set) = setup("edit");
    std::fs::write(h.root.join("code.txt"), "alpha\nbeta\nalpha\n").unwrap();
    // 未读先编辑 → 拒绝
    assert_eq!(
        err(h.registry.dispatch(
            "fs_edit",
            &serde_json::json!({ "path": "code.txt", "old_string": "alpha", "new_string": "X" })
        ))
        .code,
        ErrorCode::SandboxDenied
    );
    ok(h.registry
        .dispatch("fs_read", &serde_json::json!({ "path": "code.txt" })));
    // 歧义锚串 → 拒绝且文件零改动
    let ambiguous = err(h.registry.dispatch(
        "fs_edit",
        &serde_json::json!({ "path": "code.txt", "old_string": "alpha", "new_string": "X" }),
    ));
    assert_eq!(ambiguous.code, ErrorCode::ToolArgsInvalid);
    assert!(ambiguous.message.contains("left unchanged"));
    assert_eq!(
        std::fs::read_to_string(h.root.join("code.txt")).unwrap(),
        "alpha\nbeta\nalpha\n"
    );
    // 唯一锚串替换
    let value = ok(h.registry.dispatch(
        "fs_edit",
        &serde_json::json!({ "path": "code.txt", "old_string": "beta", "new_string": "BETA" }),
    ));
    assert_eq!(value["replacements"], 1);
    // replace_all
    let value = ok(h.registry.dispatch(
        "fs_edit",
        &serde_json::json!({ "path": "code.txt", "old_string": "alpha", "new_string": "X", "replace_all": true }),
    ));
    assert_eq!(value["replacements"], 2);
    assert_eq!(
        std::fs::read_to_string(h.root.join("code.txt")).unwrap(),
        "X\nBETA\nX\n"
    );
    // 锚串不存在 → 拒绝且零改动
    assert_eq!(
        err(h.registry.dispatch(
            "fs_edit",
            &serde_json::json!({ "path": "code.txt", "old_string": "missing", "new_string": "y" })
        ))
        .code,
        ErrorCode::ToolArgsInvalid
    );
}

#[test]
fn path_escape_and_missing_paths_fail_closed() {
    let (h, _set) = setup("escape");
    assert_eq!(
        err(h
            .registry
            .dispatch("fs_read", &serde_json::json!({ "path": "../outside.txt" })))
        .code,
        ErrorCode::SandboxDenied
    );
    assert_eq!(
        err(h.registry.dispatch(
            "fs_read",
            &serde_json::json!({ "path": "does/not/exist.txt" })
        ))
        .code,
        ErrorCode::ToolArgsInvalid
    );
    // 写到不存在的父目录 → 拒绝（不自动建目录）
    assert_eq!(
        err(h.registry.dispatch(
            "fs_write",
            &serde_json::json!({ "path": "a/b/c.txt", "content": "x" })
        ))
        .code,
        ErrorCode::ToolArgsInvalid
    );
}

#[test]
fn cancelled_token_refuses_write_and_edit_at_commit_point() {
    let (h, set) = setup("cancelled");
    // 模拟 deadline 到期：令牌被取消
    set.set_cancellation_token(CancellationToken::default());
    set.cancellation_token
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .cancel();
    assert_eq!(
        err(h.registry.dispatch(
            "fs_write",
            &serde_json::json!({ "path": "new.txt", "content": "x" })
        ))
        .code,
        ErrorCode::ToolTimeout
    );
    assert_eq!(
        err(h.registry.dispatch(
            "fs_edit",
            &serde_json::json!({ "path": "new.txt", "old_string": "a", "new_string": "b" })
        ))
        .code,
        ErrorCode::ToolTimeout
    );
}

#[test]
fn symlinked_file_is_not_followed_when_platform_allows_creation() {
    let (h, _set) = setup("symlink");
    let outside = workspace("symlink-outside");
    std::fs::remove_dir_all(&outside).ok();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();
    let created = {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("secret.txt"), h.root.join("link.txt")).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(outside.join("secret.txt"), h.root.join("link.txt"))
                .is_ok()
        }
    };
    if created {
        assert_eq!(
            err(h
                .registry
                .dispatch("fs_read", &serde_json::json!({ "path": "link.txt" })))
            .code,
            ErrorCode::SandboxDenied
        );
    }
    std::fs::remove_dir_all(outside).ok();
}

#[test]
fn glob_supports_recursive_and_wildcard_segments() {
    let (h, _set) = setup("glob");
    for path in ["src/a.rs", "src/deep/b.rs", "docs/c.md"] {
        std::fs::create_dir_all(h.root.join(path).parent().unwrap()).unwrap();
        std::fs::write(h.root.join(path), "x").unwrap();
    }
    let all = ok(h
        .registry
        .dispatch("fs_glob", &serde_json::json!({ "pattern": "**/*.rs" })));
    let matches = all["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2, "{all}");
    assert!(matches.contains(&serde_json::json!("src/a.rs")));
    assert!(matches.contains(&serde_json::json!("src/deep/b.rs")));
    let one = ok(h
        .registry
        .dispatch("fs_glob", &serde_json::json!({ "pattern": "docs/?.md" })));
    assert_eq!(one["matches"].as_array().unwrap().len(), 1);
    // 模式穿越被拒绝
    assert_eq!(
        err(h
            .registry
            .dispatch("fs_glob", &serde_json::json!({ "pattern": "../**" })))
        .code,
        ErrorCode::SandboxDenied
    );
}

#[test]
fn grep_reports_lines_skips_binary_and_spills_on_overflow() {
    let (h, _set) = setup("grep");
    std::fs::create_dir_all(h.root.join("src")).unwrap();
    std::fs::write(h.root.join("src/a.rs"), "let apple = 1;\nlet banana = 2;\n").unwrap();
    std::fs::write(h.root.join("bin.dat"), [0u8, 1, 2, b'a']).unwrap();

    let value = ok(h
        .registry
        .dispatch("fs_grep", &serde_json::json!({ "query": "apple" })));
    let matches = value["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["file"], "src/a.rs");
    assert_eq!(matches[0]["line"], 1);

    let value = ok(h.registry.dispatch(
        "fs_grep",
        &serde_json::json!({ "query": "a", "glob": "**/*.rs" }),
    ));
    // a.rs 与 bin.dat 都含 'a'，但 binary 被跳过；glob 过滤同样排除 bin.dat
    assert_eq!(value["matches"].as_array().unwrap().len(), 2);

    // 超限 → spill，locator 可被 fs_read 取回全文
    std::fs::create_dir_all(h.root.join("many")).unwrap();
    for i in 0..250 {
        std::fs::write(
            h.root.join(format!("many/f{i:03}.txt")),
            format!("needle {i}\n"),
        )
        .unwrap();
    }
    let value = ok(h
        .registry
        .dispatch("fs_grep", &serde_json::json!({ "query": "needle" })));
    assert_eq!(value["truncated"], true);
    let locator = value["locator"].as_str().unwrap();
    assert!(locator.starts_with(".harness/spill/"));
    let spilled = ok(h
        .registry
        .dispatch("fs_read", &serde_json::json!({ "path": locator })));
    let content = spilled["content"].as_str().unwrap();
    assert_eq!(
        content.lines().count(),
        250,
        "spill 必须包含全部 250 条命中"
    );
}

#[test]
fn large_file_read_spills_with_locator_and_counts_as_read() {
    let (h, _set) = setup("big-read");
    let big = format!(
        "{}
unique-tail-marker
",
        "x".repeat(300 * 1024)
    ); // 超过 256KB 读上限
    std::fs::write(h.root.join("big.txt"), &big).unwrap();
    let value = ok(h
        .registry
        .dispatch("fs_read", &serde_json::json!({ "path": "big.txt" })));
    assert_eq!(value["truncated"], true);
    assert_eq!(
        value["content"].as_str().unwrap().chars().count(),
        4_000,
        "预览固定 4000 字符"
    );
    // 虽然截断交付，已读事实成立：编辑放行
    let value = ok(h.registry.dispatch(
        "fs_edit",
        &serde_json::json!({
            "path": "big.txt",
            "old_string": "unique-tail-marker",
            "new_string": "unique-tail-marked"
        }),
    ));
    assert_eq!(value["replacements"], 1);
}
