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
