//! TASK-606：事件溯源的轻量 Agent Team 协调入口。

use crate::subagent_policy::{validate_child_policy, SubagentPolicy};
use protocol::{
    ErrorCode, ErrorEnvelope, Event, TeamMember, TeamMessage, TeamTask, TeamWriteScopeConflict,
};
use session::{SessionStore, TeamState};

pub struct TeamCoordinator<'a> {
    session: &'a mut dyn SessionStore,
    state: TeamState,
}

impl<'a> TeamCoordinator<'a> {
    pub fn open(
        session: &'a mut dyn SessionStore,
        root_id: impl Into<String>,
    ) -> Result<Self, ErrorEnvelope> {
        let events = session.replay_events().map_err(|error| {
            ErrorEnvelope::new(
                ErrorCode::Internal,
                format!("failed to replay team state: {error}"),
            )
        })?;
        let state = TeamState::replay(root_id, &events)?;
        Ok(Self { session, state })
    }

    pub fn state(&self) -> &TeamState {
        &self.state
    }

    pub fn register_member(
        &mut self,
        member: TeamMember,
        parent_policy: &SubagentPolicy,
        member_policy: &SubagentPolicy,
    ) -> Result<(), ErrorEnvelope> {
        validate_child_policy(parent_policy, member_policy)?;
        self.state.validate_member(&member)?;
        self.append(Event::TeamMemberRegistered { member })
    }

    /// Returns true only when a new message event was appended.
    pub fn enqueue_message(&mut self, message: TeamMessage) -> Result<bool, ErrorEnvelope> {
        if !self.state.validate_message(&message)? {
            return Ok(false);
        }
        self.append(Event::TeamMessageEnqueued { message })?;
        Ok(true)
    }

    /// Deterministically delivers the oldest pending message at most once.
    pub fn deliver_next(&mut self, member_id: &str) -> Result<Option<TeamMessage>, ErrorEnvelope> {
        let Some(message) = self.state.next_message(member_id).cloned() else {
            return Ok(None);
        };
        self.append(Event::TeamMessageDelivered {
            message_id: message.message_id.clone(),
            to_member_id: member_id.to_string(),
        })?;
        Ok(Some(message))
    }

    pub fn create_task(
        &mut self,
        task: TeamTask,
    ) -> Result<Vec<TeamWriteScopeConflict>, ErrorEnvelope> {
        let conflicts = self.state.validate_new_task(&task)?;
        // TASK-805：任务事件与冲突审计是不可分割提交——原子批量追加
        let mut events = vec![Event::TeamTaskCreated { task }];
        for conflict in &conflicts {
            events.push(Event::TeamWriteScopeConflictDetected {
                conflict: conflict.clone(),
            });
        }
        self.append_batch(events)?;
        Ok(conflicts)
    }

    pub fn update_task(
        &mut self,
        expected_revision: u64,
        task: TeamTask,
    ) -> Result<Vec<TeamWriteScopeConflict>, ErrorEnvelope> {
        let conflicts = self.state.validate_task_update(expected_revision, &task)?;
        // TASK-805：同上——任务更新与冲突审计整批落盘
        let mut events = vec![Event::TeamTaskUpdated {
            expected_revision,
            task,
        }];
        for conflict in &conflicts {
            events.push(Event::TeamWriteScopeConflictDetected {
                conflict: conflict.clone(),
            });
        }
        self.append_batch(events)?;
        Ok(conflicts)
    }

    /// TASK-805：原子批量追加并按序应用到内存状态。
    fn append_batch(&mut self, events: Vec<Event>) -> Result<(), ErrorEnvelope> {
        let records = self.session.append_batch(events).map_err(|error| {
            ErrorEnvelope::new(
                ErrorCode::Internal,
                format!("failed to append team event batch: {error}"),
            )
        })?;
        for record in records {
            self.state.apply(record.seq, &record.event)?;
        }
        Ok(())
    }

    fn append(&mut self, event: Event) -> Result<(), ErrorEnvelope> {
        let record = self.session.append(event).map_err(|error| {
            ErrorEnvelope::new(
                ErrorCode::Internal,
                format!("failed to append team event: {error}"),
            )
        })?;
        self.state.apply(record.seq, &record.event)
    }
}
