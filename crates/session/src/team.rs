//! TASK-606：从 append-only 事件流派生团队 roster、mailbox 与任务 DAG。

use protocol::{
    ErrorCode, ErrorEnvelope, Event, SequencedEvent, TeamMember, TeamMessage, TeamTask,
    TeamTaskStatus, TeamWriteScopeConflict,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredMessage {
    message: TeamMessage,
    enqueued_seq: u64,
    delivered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamState {
    root_id: String,
    members: BTreeMap<String, TeamMember>,
    messages: BTreeMap<String, StoredMessage>,
    tasks: BTreeMap<String, TeamTask>,
    conflicts: Vec<TeamWriteScopeConflict>,
}

impl TeamState {
    pub fn replay(
        root_id: impl Into<String>,
        events: &[SequencedEvent],
    ) -> Result<Self, ErrorEnvelope> {
        let root_id = root_id.into();
        if root_id.trim().is_empty() {
            return Err(args("team root id must not be blank"));
        }
        let mut state = Self {
            root_id,
            members: BTreeMap::new(),
            messages: BTreeMap::new(),
            tasks: BTreeMap::new(),
            conflicts: Vec::new(),
        };
        for record in events {
            state.apply(record.seq, &record.event)?;
        }
        Ok(state)
    }

    pub fn members(&self) -> impl Iterator<Item = &TeamMember> {
        self.members.values()
    }

    pub fn tasks(&self) -> impl Iterator<Item = &TeamTask> {
        self.tasks.values()
    }

    pub fn task(&self, task_id: &str) -> Option<&TeamTask> {
        self.tasks.get(task_id)
    }

    pub fn message(&self, message_id: &str) -> Option<(&TeamMessage, bool)> {
        self.messages
            .get(message_id)
            .map(|stored| (&stored.message, stored.delivered))
    }

    pub fn next_message(&self, member_id: &str) -> Option<&TeamMessage> {
        self.messages
            .values()
            .filter(|stored| !stored.delivered && stored.message.to_member_id == member_id)
            .min_by_key(|stored| stored.enqueued_seq)
            .map(|stored| &stored.message)
    }

    pub fn conflicts(&self) -> &[TeamWriteScopeConflict] {
        &self.conflicts
    }

    pub fn validate_member(&self, member: &TeamMember) -> Result<(), ErrorEnvelope> {
        nonblank(&member.member_id, "team member id")?;
        nonblank(&member.parent_id, "team parent id")?;
        if member.member_id == self.root_id || self.members.contains_key(&member.member_id) {
            return Err(args("team member id is already registered"));
        }
        if member.parent_id != self.root_id && !self.members.contains_key(&member.parent_id) {
            return Err(args("team member parent is not registered"));
        }
        Ok(())
    }

    pub fn validate_message(&self, message: &TeamMessage) -> Result<bool, ErrorEnvelope> {
        for (value, label) in [
            (&message.message_id, "team message id"),
            (&message.from_member_id, "team message sender"),
            (&message.to_member_id, "team message recipient"),
            (&message.body, "team message body"),
        ] {
            nonblank(value, label)?;
        }
        self.ensure_participant(&message.from_member_id)?;
        self.ensure_participant(&message.to_member_id)?;
        match self.messages.get(&message.message_id) {
            Some(existing) if existing.message == *message => Ok(false),
            Some(_) => Err(args("team message id conflicts with existing payload")),
            None => Ok(true),
        }
    }

    pub fn validate_new_task(
        &self,
        task: &TeamTask,
    ) -> Result<Vec<TeamWriteScopeConflict>, ErrorEnvelope> {
        if self.tasks.contains_key(&task.task_id) {
            return Err(args("team task id already exists"));
        }
        if task.revision != 1 {
            return Err(revision("new team task revision must be one"));
        }
        self.validate_task_shape(task)?;
        self.validate_dependencies(task)?;
        Ok(self.detect_conflicts(task))
    }

    pub fn validate_task_update(
        &self,
        expected_revision: u64,
        task: &TeamTask,
    ) -> Result<Vec<TeamWriteScopeConflict>, ErrorEnvelope> {
        let current = self
            .tasks
            .get(&task.task_id)
            .ok_or_else(|| args("team task does not exist"))?;
        if current.revision != expected_revision
            || task.revision
                != expected_revision
                    .checked_add(1)
                    .ok_or_else(|| revision("team task revision overflow"))?
        {
            return Err(revision("stale team task revision"));
        }
        self.validate_task_shape(task)?;
        self.validate_dependencies(task)?;
        Ok(self.detect_conflicts(task))
    }

    pub fn apply(&mut self, seq: u64, event: &Event) -> Result<(), ErrorEnvelope> {
        match event {
            Event::TeamMemberRegistered { member } => {
                self.validate_member(member)?;
                self.members
                    .insert(member.member_id.clone(), member.clone());
            }
            Event::TeamMessageEnqueued { message } => {
                if self.validate_message(message)? {
                    self.messages.insert(
                        message.message_id.clone(),
                        StoredMessage {
                            message: message.clone(),
                            enqueued_seq: seq,
                            delivered: false,
                        },
                    );
                }
            }
            Event::TeamMessageDelivered {
                message_id,
                to_member_id,
            } => {
                let stored = self
                    .messages
                    .get_mut(message_id)
                    .ok_or_else(|| args("delivered team message was never enqueued"))?;
                if stored.message.to_member_id != *to_member_id {
                    return Err(args("team message delivered to the wrong member"));
                }
                stored.delivered = true;
            }
            Event::TeamTaskCreated { task } => {
                self.validate_new_task(task)?;
                self.tasks.insert(task.task_id.clone(), task.clone());
            }
            Event::TeamTaskUpdated {
                expected_revision,
                task,
            } => {
                self.validate_task_update(*expected_revision, task)?;
                self.tasks.insert(task.task_id.clone(), task.clone());
            }
            Event::TeamWriteScopeConflictDetected { conflict } => {
                if !self.tasks.contains_key(&conflict.task_id)
                    || !self.tasks.contains_key(&conflict.conflicting_task_id)
                    || conflict.scope.trim().is_empty()
                {
                    return Err(args("invalid team write-scope conflict event"));
                }
                self.conflicts.push(conflict.clone());
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_task_shape(&self, task: &TeamTask) -> Result<(), ErrorEnvelope> {
        nonblank(&task.task_id, "team task id")?;
        self.ensure_participant(&task.owner_member_id)?;
        unique_nonblank(&task.blocked_by, "team task dependency")?;
        unique_nonblank(&task.write_scopes, "team task write scope")?;
        for scope in &task.write_scopes {
            if normalize_scope(scope)? != *scope {
                return Err(args("team write scope must use canonical relative syntax"));
            }
        }
        Ok(())
    }

    fn validate_dependencies(&self, candidate: &TeamTask) -> Result<(), ErrorEnvelope> {
        for dependency in &candidate.blocked_by {
            if dependency == &candidate.task_id || !self.tasks.contains_key(dependency) {
                return Err(cycle("team task has an unknown or self dependency"));
            }
            if self.reaches(
                dependency,
                &candidate.task_id,
                candidate,
                &mut BTreeSet::new(),
            ) {
                return Err(cycle("team task dependency cycle detected"));
            }
        }
        Ok(())
    }

    fn reaches(
        &self,
        current: &str,
        target: &str,
        candidate: &TeamTask,
        visited: &mut BTreeSet<String>,
    ) -> bool {
        if current == target {
            return true;
        }
        if !visited.insert(current.to_string()) {
            return false;
        }
        let task = if current == candidate.task_id {
            Some(candidate)
        } else {
            self.tasks.get(current)
        };
        task.is_some_and(|task| {
            task.blocked_by
                .iter()
                .any(|next| self.reaches(next, target, candidate, visited))
        })
    }

    fn detect_conflicts(&self, candidate: &TeamTask) -> Vec<TeamWriteScopeConflict> {
        if terminal(candidate.status) {
            return Vec::new();
        }
        let mut conflicts = Vec::new();
        for other in self.tasks.values() {
            if other.task_id == candidate.task_id
                || other.owner_member_id == candidate.owner_member_id
                || terminal(other.status)
            {
                continue;
            }
            for left in &candidate.write_scopes {
                for right in &other.write_scopes {
                    if scopes_overlap(left, right) {
                        conflicts.push(TeamWriteScopeConflict {
                            task_id: candidate.task_id.clone(),
                            conflicting_task_id: other.task_id.clone(),
                            scope: narrower_scope(left, right),
                        });
                    }
                }
            }
        }
        conflicts
    }

    fn ensure_participant(&self, member_id: &str) -> Result<(), ErrorEnvelope> {
        if member_id == self.root_id || self.members.contains_key(member_id) {
            Ok(())
        } else {
            Err(args("team participant is not registered"))
        }
    }
}

fn terminal(status: TeamTaskStatus) -> bool {
    matches!(
        status,
        TeamTaskStatus::Completed | TeamTaskStatus::Failed | TeamTaskStatus::Cancelled
    )
}

fn scopes_overlap(left: &str, right: &str) -> bool {
    let left = normalize_scope(left).expect("validated task scope");
    let right = normalize_scope(right).expect("validated task scope");
    left == right
        || left
            .strip_prefix(&right)
            .is_some_and(|tail| tail.starts_with('/'))
        || right
            .strip_prefix(&left)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn narrower_scope(left: &str, right: &str) -> String {
    let left = normalize_scope(left).expect("validated task scope");
    let right = normalize_scope(right).expect("validated task scope");
    if left.len() >= right.len() {
        left
    } else {
        right
    }
}

fn normalize_scope(scope: &str) -> Result<String, ErrorEnvelope> {
    let normalized = scope.replace('\\', "/");
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty()
        || normalized.starts_with('/')
        || normalized.contains(':')
        || parts.contains(&"..")
    {
        return Err(args("team write scope must be a safe relative path"));
    }
    Ok(parts.join("/"))
}

fn unique_nonblank(values: &[String], label: &str) -> Result<(), ErrorEnvelope> {
    let mut seen = BTreeSet::new();
    for value in values {
        nonblank(value, label)?;
        if !seen.insert(value) {
            return Err(args(format!("{label} values must be unique")));
        }
    }
    Ok(())
}

fn nonblank(value: &str, label: &str) -> Result<(), ErrorEnvelope> {
    if value.trim().is_empty() {
        Err(args(format!("{label} must not be blank")))
    } else {
        Ok(())
    }
}

fn args(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::ToolArgsInvalid, message)
}

fn revision(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::TeamRevisionConflict, message)
}

fn cycle(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::TeamDependencyCycle, message)
}
