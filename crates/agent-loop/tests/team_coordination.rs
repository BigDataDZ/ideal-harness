use agent_loop::{SubagentPolicy, TeamCoordinator};
use protocol::{ErrorCode, Event, TeamMember, TeamMessage, TeamTask, TeamTaskStatus};
use session::{replay_session, JsonlSession};
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ih-team-{}-{name}.jsonl", std::process::id()))
}

fn parent_policy() -> SubagentPolicy {
    SubagentPolicy::new(
        3,
        4,
        20,
        20_000,
        ["model-a".into(), "model-b".into()],
        ["read".into(), "write".into()],
        ["danger".into()],
    )
    .unwrap()
}

fn member_policy() -> SubagentPolicy {
    SubagentPolicy::new(
        2,
        2,
        10,
        10_000,
        ["model-a".into()],
        ["read".into()],
        ["danger".into()],
    )
    .unwrap()
}

fn member(id: &str) -> TeamMember {
    TeamMember {
        member_id: id.into(),
        parent_id: "root".into(),
    }
}

fn message(id: &str) -> TeamMessage {
    TeamMessage {
        message_id: id.into(),
        from_member_id: "member-a".into(),
        to_member_id: "member-b".into(),
        body: "review the registry".into(),
    }
}

fn task(id: &str, owner: &str, revision: u64, blocked_by: &[&str], scopes: &[&str]) -> TeamTask {
    TeamTask {
        task_id: id.into(),
        owner_member_id: owner.into(),
        revision,
        status: TeamTaskStatus::InProgress,
        blocked_by: blocked_by.iter().map(|value| (*value).into()).collect(),
        write_scopes: scopes.iter().map(|value| (*value).into()).collect(),
    }
}

fn register_pair(coordinator: &mut TeamCoordinator<'_>) {
    let parent = parent_policy();
    let child = member_policy();
    coordinator
        .register_member(member("member-a"), &parent, &child)
        .unwrap();
    coordinator
        .register_member(member("member-b"), &parent, &child)
        .unwrap();
}

#[test]
fn crash_replay_restores_roster_tasks_and_exactly_once_mailbox() {
    let path = tmp("replay");
    std::fs::remove_file(&path).ok();
    {
        let mut session = JsonlSession::create(path.clone()).unwrap();
        let mut coordinator = TeamCoordinator::open(&mut session, "root").unwrap();
        register_pair(&mut coordinator);
        assert!(coordinator.enqueue_message(message("msg-1")).unwrap());
        assert!(!coordinator.enqueue_message(message("msg-1")).unwrap());
        assert_eq!(
            coordinator.deliver_next("member-b").unwrap(),
            Some(message("msg-1"))
        );
        coordinator
            .create_task(task("task-a", "member-a", 1, &[], &["crates/tools"]))
            .unwrap();
    }

    let mut reopened = JsonlSession::create(path.clone()).unwrap();
    let mut coordinator = TeamCoordinator::open(&mut reopened, "root").unwrap();
    assert_eq!(coordinator.state().members().count(), 2);
    assert_eq!(coordinator.state().tasks().count(), 1);
    assert_eq!(
        coordinator.state().message("msg-1"),
        Some((&message("msg-1"), true))
    );
    assert!(!coordinator.enqueue_message(message("msg-1")).unwrap());
    assert_eq!(coordinator.deliver_next("member-b").unwrap(), None);

    let events = replay_session(&path).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event, Event::TeamMessageEnqueued { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event, Event::TeamMessageDelivered { .. }))
            .count(),
        1
    );
    std::fs::remove_file(path).ok();
}

#[test]
fn stale_revision_and_dependency_cycle_fail_without_appending() {
    let path = tmp("cas-cycle");
    std::fs::remove_file(&path).ok();
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let mut coordinator = TeamCoordinator::open(&mut session, "root").unwrap();
    register_pair(&mut coordinator);
    coordinator
        .create_task(task("task-a", "member-a", 1, &[], &["a"]))
        .unwrap();
    coordinator
        .create_task(task("task-b", "member-b", 1, &["task-a"], &["b"]))
        .unwrap();
    let before = coordinator.state().task("task-a").unwrap().clone();

    let stale = coordinator
        .update_task(0, task("task-a", "member-a", 1, &[], &["a"]))
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::TeamRevisionConflict);
    let cycle = coordinator
        .update_task(1, task("task-a", "member-a", 2, &["task-b"], &["a"]))
        .unwrap_err();
    assert_eq!(cycle.code, ErrorCode::TeamDependencyCycle);
    assert_eq!(coordinator.state().task("task-a"), Some(&before));
    std::fs::remove_file(path).ok();
}

#[test]
fn overlapping_write_scopes_emit_auditable_warning_without_locking() {
    let path = tmp("scope-conflict");
    std::fs::remove_file(&path).ok();
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let mut coordinator = TeamCoordinator::open(&mut session, "root").unwrap();
    register_pair(&mut coordinator);
    coordinator
        .create_task(task("task-a", "member-a", 1, &[], &["crates/tools"]))
        .unwrap();
    let conflicts = coordinator
        .create_task(task("task-b", "member-b", 1, &[], &["crates/tools/src"]))
        .unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].scope, "crates/tools/src");
    assert_eq!(coordinator.state().conflicts(), conflicts);
    assert!(replay_session(&path)
        .unwrap()
        .iter()
        .any(|record| matches!(record.event, Event::TeamWriteScopeConflictDetected { .. })));
    std::fs::remove_file(path).ok();
}

#[test]
fn expanded_member_policy_is_rejected_before_roster_mutation() {
    let path = tmp("policy");
    std::fs::remove_file(&path).ok();
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let mut coordinator = TeamCoordinator::open(&mut session, "root").unwrap();
    let parent = member_policy();
    let expanded = parent_policy();
    let error = coordinator
        .register_member(member("member-a"), &parent, &expanded)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::SandboxDenied);
    assert_eq!(coordinator.state().members().count(), 0);
    assert!(replay_session(&path).unwrap().is_empty());
    std::fs::remove_file(path).ok();
}

#[test]
fn task_and_conflict_events_form_one_atomic_contiguous_batch() {
    // TASK-805：任务生效与冲突审计整批落盘，seq 连续、重放一致
    let path = tmp("atomic-batch");
    std::fs::remove_file(&path).ok();
    let mut session = JsonlSession::create(path.clone()).unwrap();
    let mut coordinator = TeamCoordinator::open(&mut session, "root").unwrap();
    register_pair(&mut coordinator);
    let base = replay_session(&path).unwrap().len() as u64;
    let conflicts = coordinator
        .create_task(task("task-x", "member-a", 1, &[], &["crates/session"]))
        .unwrap();
    assert_eq!(conflicts.len(), 0);
    let conflicts = coordinator
        .create_task(task("task-y", "member-b", 1, &[], &["crates/session/src"]))
        .unwrap();
    assert_eq!(conflicts.len(), 1);
    // 事件序连续无缺口：任务事件与其冲突审计相邻
    let events = replay_session(&path).unwrap();
    for (index, record) in events.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "重放无缺口");
    }
    let tail_kinds: Vec<&str> = events
        .iter()
        .skip(base as usize)
        .map(|record| match &record.event {
            Event::TeamTaskCreated { .. } => "created",
            Event::TeamWriteScopeConflictDetected { .. } => "conflict",
            _ => "other",
        })
        .collect();
    // 第二个任务批次 = created + conflict 相邻（一个不可分割提交）
    let window: Vec<&str> = tail_kinds[tail_kinds.len() - 2..].to_vec();
    assert_eq!(window, vec!["created", "conflict"]);
    assert_eq!(coordinator.state().conflicts().len(), 1);
    // 重放恢复一致
    let reopened = TeamCoordinator::open(&mut session, "root").unwrap();
    assert_eq!(reopened.state().conflicts().len(), 1);
    std::fs::remove_file(path).ok();
}
