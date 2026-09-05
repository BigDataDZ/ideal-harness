//! D25/TASK-903: fail-closed command gate shared by restricted desktop hosts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use approval::Approver;
use protocol::{ErrorCode, ErrorEnvelope, SessionEventFrame};
use session::{fork, replay_session, revert_before_turn, JsonlSession};
use tools::CancellationToken;

use crate::{HostConfig, ProductionHost, ValidatedHostConfig};

const MAX_SESSION_ID_BYTES: usize = 128;
const MAX_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandContext {
    pub generation: u64,
    pub permission_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandReceipt {
    pub operation: &'static str,
    pub generation: u64,
    pub permission_epoch: u64,
    pub turn_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionOperation {
    Create {
        session_id: String,
    },
    Resume {
        session_id: String,
    },
    Fork {
        source_id: String,
        target_id: String,
        boundary: Option<usize>,
    },
    Revert {
        source_id: String,
        target_id: String,
        turn_id: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionReceipt {
    pub session_id: String,
    pub event_count: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveTurn {
    id: u64,
    session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingApproval {
    request_id: String,
    executor_generation: u64,
}

trait ManagedHost: Send + Sync {
    fn cancellation_token(&self) -> CancellationToken;
    fn shutdown(&self) -> anyhow::Result<()>;

    /// TASK-909：在宿主自己的后台线程执行一个完整 turn（模型采样 → 工具 → 结论），
    /// 完成时置位 done。宿主实现负责把事件写入 session 日志。
    fn spawn_turn(
        self: Arc<Self>,
        session_path: PathBuf,
        input: String,
        done: Arc<AtomicBool>,
    ) -> Result<(), ErrorEnvelope> {
        let _ = (session_path, input, done);
        Err(internal("turn execution is not supported by this host"))
    }
}

impl ManagedHost for ProductionHost {
    fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token()
    }

    fn shutdown(&self) -> anyhow::Result<()> {
        self.shutdown()
    }

    fn spawn_turn(
        self: Arc<Self>,
        session_path: PathBuf,
        input: String,
        done: Arc<AtomicBool>,
    ) -> Result<(), ErrorEnvelope> {
        let host_for_thread = Arc::clone(&self);
        // 后台线程拥有宿主 Arc，跑完整个 agent loop；结束置位 done。
        std::thread::spawn(move || {
            struct TurnDoneGuard(Arc<AtomicBool>);
            impl Drop for TurnDoneGuard {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _done = TurnDoneGuard(Arc::clone(&done));
            run_turn_blocking(&host_for_thread, &session_path, &input);
        });
        Ok(())
    }
}

pub struct DesktopBridge {
    workspace_root: PathBuf,
    sessions_root: PathBuf,
    generation: u64,
    permission_epoch: u64,
    next_turn_id: u64,
    closed: bool,
    active_turn: Option<ActiveTurn>,
    pending_approval: Option<PendingApproval>,
    host: Option<Arc<dyn ManagedHost>>,
    host_config: Option<ValidatedHostConfig>,
    /// TASK-909：当前 turn 的完成标志；新 turn 启动前据此清理已完成的登记。
    turn_done: Option<Arc<AtomicBool>>,
}

impl DesktopBridge {
    pub fn new(workspace_root: &Path, sessions_root: &Path) -> Result<Self, ErrorEnvelope> {
        let workspace_root = canonical_directory(workspace_root, "workspace root")?;
        let sessions_root = canonical_directory(sessions_root, "sessions root")?;
        Ok(Self {
            workspace_root,
            sessions_root,
            generation: 1,
            permission_epoch: 1,
            next_turn_id: 0,
            closed: false,
            turn_done: None,
            active_turn: None,
            pending_approval: None,
            host: None,
            host_config: None,
        })
    }

    pub fn context(&self) -> CommandContext {
        CommandContext {
            generation: self.generation,
            permission_epoch: self.permission_epoch,
        }
    }

    pub fn validate_command_context(&self, context: CommandContext) -> Result<(), ErrorEnvelope> {
        self.validate_context(context)
    }

    pub fn has_production_host(&self) -> bool {
        self.host.is_some()
    }

    pub fn install_production_host(
        &mut self,
        context: CommandContext,
        config: HostConfig,
        approver: Option<Arc<dyn Approver + Send + Sync>>,
    ) -> Result<(), ErrorEnvelope> {
        self.validate_context(context)?;
        if self.host.is_some() {
            return Err(invalid("production host is already installed"));
        }
        let validated = config.clone().validate().map_err(internal)?;
        self.validate_workspace(&validated.workspace)?;
        let host = ProductionHost::start(config, approver).map_err(internal)?;
        self.host = Some(Arc::new(host));
        self.host_config = Some(validated);
        Ok(())
    }

    pub fn install_production_host_with_api_key(
        &mut self,
        context: CommandContext,
        config: HostConfig,
        api_key: impl Into<String>,
        approver: Option<Arc<dyn Approver + Send + Sync>>,
    ) -> Result<(), ErrorEnvelope> {
        self.validate_context(context)?;
        if self.host.is_some() {
            return Err(invalid("production host is already installed"));
        }
        let validated = config.clone().validate().map_err(internal)?;
        self.validate_workspace(&validated.workspace)?;
        let host =
            ProductionHost::start_with_api_key(config, api_key, approver).map_err(internal)?;
        self.host = Some(Arc::new(host));
        self.host_config = Some(validated);
        Ok(())
    }

    /// Invalidates every permission fact after provider settings or credentials change.
    pub fn configuration_changed(
        &mut self,
        context: CommandContext,
    ) -> Result<CommandReceipt, ErrorEnvelope> {
        self.validate_configuration_change(context)?;
        let shutdown = self.cancel_and_shutdown_host();
        let advanced = self.advance_security_context();
        shutdown?;
        advanced?;
        Ok(self.receipt("configuration_changed", None))
    }

    pub fn validate_configuration_change(
        &self,
        context: CommandContext,
    ) -> Result<(), ErrorEnvelope> {
        self.validate_context(context)?;
        if self.active_turn.is_some() || self.pending_approval.is_some() {
            return Err(ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "configuration cannot change while a turn or approval is active",
            ));
        }
        Ok(())
    }

    pub fn validate_installed_host(
        &self,
        context: CommandContext,
        config: HostConfig,
    ) -> Result<(), ErrorEnvelope> {
        self.validate_context(context)?;
        let requested = config.validate().map_err(internal)?;
        if self.host.is_none() || self.host_config.as_ref() != Some(&requested) {
            return Err(ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "production host security facts changed; restart with a new generation",
            ));
        }
        Ok(())
    }

    pub fn start_turn(
        &mut self,
        context: CommandContext,
        session_id: &str,
        workspace: &Path,
        input: &str,
    ) -> Result<CommandReceipt, ErrorEnvelope> {
        self.validate_context(context)?;
        if self.host.is_none() {
            return Err(ErrorEnvelope::new(
                ErrorCode::Internal,
                "production host is unavailable; failing closed",
            ));
        }
        self.validate_workspace(workspace)?;
        validate_nonblank_bounded(input, "turn input")?;
        let session_path = self.session_path(session_id, false)?;
        // TASK-909：上一 turn 已完成则清理登记，允许开启新 turn
        if let Some(done) = &self.turn_done {
            if done.load(Ordering::SeqCst) {
                self.active_turn = None;
            }
        }
        if self.active_turn.is_some() {
            return Err(invalid("another turn is already active"));
        }
        let turn_id = self.next_turn_id;
        self.next_turn_id = self
            .next_turn_id
            .checked_add(1)
            .ok_or_else(|| internal("turn id overflow"))?;
        self.active_turn = Some(ActiveTurn {
            id: turn_id,
            session_id: session_id.to_owned(),
        });
        let done = Arc::new(AtomicBool::new(false));
        self.turn_done = Some(Arc::clone(&done));
        let host = self.host.as_ref().cloned().ok_or_else(|| {
            ErrorEnvelope::new(
                ErrorCode::Internal,
                "production host is unavailable; failing closed",
            )
        })?;
        host.spawn_turn(session_path, input.to_string(), done)
            .map_err(|error| ErrorEnvelope::new(ErrorCode::Internal, error.message.to_string()))?;
        Ok(self.receipt("start_turn", Some(turn_id)))
    }

    pub fn steer(
        &self,
        context: CommandContext,
        turn_id: u64,
        input: &str,
    ) -> Result<CommandReceipt, ErrorEnvelope> {
        self.validate_context(context)?;
        validate_nonblank_bounded(input, "steer input")?;
        self.require_turn(turn_id)?;
        Ok(self.receipt("steer", Some(turn_id)))
    }

    pub fn finish_turn(
        &mut self,
        context: CommandContext,
        turn_id: u64,
    ) -> Result<CommandReceipt, ErrorEnvelope> {
        self.validate_context(context)?;
        self.require_turn(turn_id)?;
        self.active_turn = None;
        self.pending_approval = None;
        Ok(self.receipt("finish_turn", Some(turn_id)))
    }

    pub fn cancel_turn(
        &mut self,
        context: CommandContext,
        turn_id: u64,
    ) -> Result<CommandReceipt, ErrorEnvelope> {
        self.validate_context(context)?;
        self.require_turn(turn_id)?;
        let shutdown = self.cancel_and_shutdown_host();
        self.active_turn = None;
        self.pending_approval = None;
        let advanced = self.advance_security_context();
        shutdown?;
        advanced?;
        Ok(self.receipt("cancel_turn", Some(turn_id)))
    }

    pub fn register_approval(
        &mut self,
        context: CommandContext,
        turn_id: u64,
        request_id: &str,
        executor_generation: u64,
    ) -> Result<(), ErrorEnvelope> {
        self.validate_context(context)?;
        self.require_turn(turn_id)?;
        validate_identifier(request_id, "approval request id")?;
        if executor_generation == 0 || self.pending_approval.is_some() {
            return Err(ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "approval security facts are unavailable or another request is pending",
            ));
        }
        self.pending_approval = Some(PendingApproval {
            request_id: request_id.to_owned(),
            executor_generation,
        });
        Ok(())
    }

    pub fn respond_approval(
        &mut self,
        context: CommandContext,
        request_id: &str,
        executor_generation: u64,
        _approved: bool,
    ) -> Result<CommandReceipt, ErrorEnvelope> {
        self.validate_context(context)?;
        let pending = self.pending_approval.as_ref().ok_or_else(|| {
            ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "approval request is absent; failing closed",
            )
        })?;
        if pending.request_id != request_id
            || pending.executor_generation != executor_generation
            || executor_generation == 0
        {
            return Err(ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "approval request or execution generation is stale",
            ));
        }
        self.pending_approval = None;
        Ok(self.receipt(
            "respond_approval",
            self.active_turn.as_ref().map(|turn| turn.id),
        ))
    }

    pub fn session_operation(
        &self,
        context: CommandContext,
        operation: SessionOperation,
    ) -> Result<SessionReceipt, ErrorEnvelope> {
        self.validate_context(context)?;
        match operation {
            SessionOperation::Create { session_id } => {
                let path = self.session_path(&session_id, true)?;
                let session = JsonlSession::create(path).map_err(internal)?;
                Ok(self.session_receipt(session_id, session.len()))
            }
            SessionOperation::Resume { session_id } => {
                let path = self.session_path(&session_id, false)?;
                let events = replay_session(&path).map_err(internal)?;
                let event_count = u64::try_from(events.len()).map_err(internal)?;
                Ok(self.session_receipt(session_id, event_count))
            }
            SessionOperation::Fork {
                source_id,
                target_id,
                boundary,
            } => {
                let source = self.session_path(&source_id, false)?;
                let target = self.session_path(&target_id, true)?;
                let event_count = replay_session(&source).map_err(internal)?.len();
                let boundary = boundary.unwrap_or(event_count);
                if boundary > event_count {
                    return Err(invalid("fork boundary exceeds source event count"));
                }
                let child = fork(&source, target, boundary).map_err(internal)?;
                Ok(self.session_receipt(target_id, child.len()))
            }
            SessionOperation::Revert {
                source_id,
                target_id,
                turn_id,
            } => {
                let source = self.session_path(&source_id, false)?;
                let target = self.session_path(&target_id, true)?;
                let child = revert_before_turn(&source, target, turn_id).map_err(internal)?;
                Ok(self.session_receipt(target_id, child.len()))
            }
        }
    }

    pub fn close(&mut self) -> Result<(), ErrorEnvelope> {
        if self.closed {
            return Ok(());
        }
        let shutdown = self.cancel_and_shutdown_host();
        self.active_turn = None;
        self.pending_approval = None;
        self.closed = true;
        let advanced = self.advance_security_context();
        shutdown?;
        advanced
    }

    fn validate_context(&self, context: CommandContext) -> Result<(), ErrorEnvelope> {
        if self.closed {
            return Err(ErrorEnvelope::new(
                ErrorCode::SandboxDenied,
                "desktop host is closed; failing closed",
            ));
        }
        if context.generation != self.generation {
            return Err(ErrorEnvelope::new(
                ErrorCode::CursorInvalid,
                format!(
                    "desktop command generation is stale (host {}, client {}); refresh and retry",
                    self.generation, context.generation
                ),
            ));
        }
        if context.permission_epoch != self.permission_epoch {
            return Err(ErrorEnvelope::new(
                ErrorCode::ApprovalRejected,
                "desktop permission epoch is stale",
            ));
        }
        Ok(())
    }

    fn validate_workspace(&self, workspace: &Path) -> Result<PathBuf, ErrorEnvelope> {
        let canonical = workspace.canonicalize().map_err(internal)?;
        if !canonical.starts_with(&self.workspace_root) {
            return Err(ErrorEnvelope::new(
                ErrorCode::SandboxDenied,
                "workspace resolves outside the configured root",
            ));
        }
        Ok(canonical)
    }

    /// TASK-909：按序返回会话事件帧（seq > last_seq，至多 limit 条），
    /// 供桌面客户端以 SessionProjection 重建唯一真相源。
    pub fn session_event_frames(
        &self,
        session_id: &str,
        last_seq: u64,
        limit: usize,
    ) -> Result<Vec<SessionEventFrame>, ErrorEnvelope> {
        if self.closed {
            return Err(invalid("bridge is closed"));
        }
        let path = self.session_path(session_id, false)?;
        let events = replay_session(&path).map_err(internal)?;
        let frames = events
            .iter()
            .filter(|record| record.seq > last_seq)
            .take(limit)
            .map(|record| SessionEventFrame {
                session_id: session_id.to_string(),
                connection_generation: self.generation,
                record: record.clone(),
            })
            .collect();
        Ok(frames)
    }

    fn session_path(&self, session_id: &str, must_be_new: bool) -> Result<PathBuf, ErrorEnvelope> {
        validate_identifier(session_id, "session id")?;
        let path = self.sessions_root.join(format!("{session_id}.jsonl"));
        if must_be_new {
            if path.exists() {
                return Err(invalid("target session already exists"));
            }
            return Ok(path);
        }
        if !path.is_file() {
            return Err(ErrorEnvelope::new(
                ErrorCode::SessionNotFound,
                "session does not exist",
            ));
        }
        let canonical = path.canonicalize().map_err(internal)?;
        if !canonical.starts_with(&self.sessions_root) {
            return Err(ErrorEnvelope::new(
                ErrorCode::SandboxDenied,
                "session path resolves outside the configured root",
            ));
        }
        Ok(canonical)
    }

    fn require_turn(&self, turn_id: u64) -> Result<&ActiveTurn, ErrorEnvelope> {
        self.active_turn
            .as_ref()
            .filter(|active| active.id == turn_id)
            .ok_or_else(|| invalid("turn is absent or stale"))
    }

    fn cancel_and_shutdown_host(&mut self) -> Result<(), ErrorEnvelope> {
        let result = if let Some(host) = self.host.take() {
            host.cancellation_token().cancel();
            host.shutdown().map_err(internal)
        } else {
            Ok(())
        };
        self.host_config = None;
        result
    }

    fn advance_security_context(&mut self) -> Result<(), ErrorEnvelope> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| internal("generation overflow"))?;
        self.permission_epoch = self
            .permission_epoch
            .checked_add(1)
            .ok_or_else(|| internal("permission epoch overflow"))?;
        Ok(())
    }

    fn receipt(&self, operation: &'static str, turn_id: Option<u64>) -> CommandReceipt {
        CommandReceipt {
            operation,
            generation: self.generation,
            permission_epoch: self.permission_epoch,
            turn_id,
        }
    }

    fn session_receipt(&self, session_id: String, event_count: u64) -> SessionReceipt {
        SessionReceipt {
            session_id,
            event_count,
            generation: self.generation,
        }
    }
}

impl Drop for DesktopBridge {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, ErrorEnvelope> {
    let canonical = path.canonicalize().map_err(internal)?;
    if !canonical.is_dir() {
        return Err(invalid(format!("{label} must be a directory")));
    }
    Ok(canonical)
}

fn validate_identifier(value: &str, label: &str) -> Result<(), ErrorEnvelope> {
    if value.is_empty()
        || value.len() > MAX_SESSION_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid(format!(
            "{label} must contain only ASCII letters, digits, '-' or '_'"
        )));
    }
    Ok(())
}

fn validate_nonblank_bounded(value: &str, label: &str) -> Result<(), ErrorEnvelope> {
    if value.trim().is_empty() || value.len() > MAX_INPUT_BYTES {
        return Err(invalid(format!(
            "{label} must be nonblank and within size limits"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::ToolArgsInvalid, message)
}

fn internal(error: impl std::fmt::Display) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::Internal, format!("{error}"))
}

/// TASK-909：turn 后台线程主体——重建模型表面历史、注入输入、跑完整闭环。
/// 全部错误以 TurnAborted 等事件落在会话日志内（run_turn 自身保证配对）。
fn run_turn_blocking(host: &ProductionHost, session_path: &Path, input: &str) {
    use model_provider::ChatMessage;
    let _ = std::fs::create_dir_all(session_path.parent().unwrap_or(Path::new(".")));
    let mut session = match JsonlSession::create(session_path.to_path_buf()) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("[desktop-turn] session open failed: {error}");
            return;
        }
    };
    let events = match replay_session(session_path) {
        Ok(events) => events,
        Err(error) => {
            eprintln!("[desktop-turn] replay failed: {error}");
            return;
        }
    };
    let history: Vec<ChatMessage> = session::project_model_surface(&events)
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| ChatMessage::try_from(entry.message.clone()).ok())
        .collect();
    let queue = host.proxy_event_queue();
    let external = move || {
        queue
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    };
    match host.build_agent_loop(&mut session, history, &external) {
        Ok(mut agent) => {
            agent.inbox.push(input.to_string());
            agent.run_turn();
        }
        Err(error) => eprintln!(
            "[desktop-turn] loop build failed: {:?} {}",
            error.code, error.message
        ),
    }
    // 诊断：打印事件摘要
    if let Ok(events) = replay_session(session_path) {
        for sequenced in events.iter().rev().take(6).rev() {
            let kind = match &sequenced.event {
                protocol::Event::TurnAborted { reason, .. } => format!("turn_aborted: {reason}"),
                protocol::Event::TurnCompleted { .. } => "turn_completed".into(),
                protocol::Event::ToolCallRequested { tool, .. } => format!("call {tool}"),
                protocol::Event::ToolResultAdded { outcome, .. } => {
                    format!("result {outcome:?}").chars().take(120).collect()
                }
                _ => continue,
            };
            eprintln!("[desktop-turn] seq={} {}", sequenced.seq, kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use protocol::Event;

    use super::*;

    struct FakeHost {
        token: CancellationToken,
        shutdown: Arc<AtomicBool>,
    }

    impl ManagedHost for FakeHost {
        fn cancellation_token(&self) -> CancellationToken {
            self.token.clone()
        }

        fn shutdown(&self) -> anyhow::Result<()> {
            self.shutdown.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn spawn_turn(
            self: Arc<Self>,
            _session_path: PathBuf,
            _input: String,
            done: Arc<AtomicBool>,
        ) -> Result<(), ErrorEnvelope> {
            done.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn roots(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("ih-desktop-bridge-{}-{name}", std::process::id()));
        let workspace = root.join("workspace");
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        (root, workspace, sessions)
    }

    fn bridge_with_fake(
        name: &str,
    ) -> (PathBuf, DesktopBridge, CancellationToken, Arc<AtomicBool>) {
        let (root, workspace, sessions) = roots(name);
        let mut bridge = DesktopBridge::new(&workspace, &sessions).unwrap();
        let token = CancellationToken::new();
        let shutdown = Arc::new(AtomicBool::new(false));
        bridge.host = Some(Arc::new(FakeHost {
            token: token.clone(),
            shutdown: Arc::clone(&shutdown),
        }));
        (root, bridge, token, shutdown)
    }

    #[test]
    fn stale_context_path_escape_and_absent_services_fail_closed() {
        let (root, workspace, sessions) = roots("reject");
        let mut bridge = DesktopBridge::new(&workspace, &sessions).unwrap();
        let context = bridge.context();
        let stale = CommandContext {
            generation: context.generation + 1,
            ..context
        };
        assert_eq!(
            bridge
                .session_operation(
                    stale,
                    SessionOperation::Create {
                        session_id: "safe".into(),
                    },
                )
                .unwrap_err()
                .code,
            ErrorCode::CursorInvalid
        );
        assert_eq!(
            bridge
                .session_operation(
                    context,
                    SessionOperation::Create {
                        session_id: "../escape".into(),
                    },
                )
                .unwrap_err()
                .code,
            ErrorCode::ToolArgsInvalid
        );
        assert_eq!(
            bridge
                .start_turn(context, "safe", &workspace, "hello")
                .unwrap_err()
                .code,
            ErrorCode::Internal
        );
        assert_eq!(
            bridge
                .respond_approval(context, "missing", 1, true)
                .unwrap_err()
                .code,
            ErrorCode::ApprovalRejected
        );
        let (hosted_root, mut hosted, _, _) = bridge_with_fake("outside-workspace");
        let hosted_context = hosted.context();
        assert_eq!(
            hosted
                .start_turn(hosted_context, "safe", &root, "hello")
                .unwrap_err()
                .code,
            ErrorCode::SandboxDenied
        );
        drop(hosted);
        std::fs::remove_dir_all(hosted_root).ok();
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn cancellation_and_close_cancel_resources_and_advance_security_facts() {
        let (root, mut bridge, token, shutdown) = bridge_with_fake("lifecycle");
        let workspace = bridge.workspace_root.clone();
        let context = bridge.context();
        bridge
            .session_operation(
                context,
                SessionOperation::Create {
                    session_id: "demo".into(),
                },
            )
            .unwrap();
        let turn = bridge
            .start_turn(context, "demo", &workspace, "hello")
            .unwrap();
        let receipt = bridge.cancel_turn(context, turn.turn_id.unwrap()).unwrap();
        assert!(token.is_cancelled());
        assert!(shutdown.load(Ordering::SeqCst));
        assert!(receipt.generation > context.generation);
        assert_eq!(
            bridge
                .steer(context, turn.turn_id.unwrap(), "late")
                .unwrap_err()
                .code,
            ErrorCode::CursorInvalid
        );
        bridge.close().unwrap();
        assert_eq!(
            bridge
                .session_operation(
                    bridge.context(),
                    SessionOperation::Resume {
                        session_id: "demo".into(),
                    },
                )
                .unwrap_err()
                .code,
            ErrorCode::SandboxDenied
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn configuration_change_invalidates_host_and_is_blocked_during_turn() {
        let (root, mut bridge, token, shutdown) = bridge_with_fake("configuration");
        let context = bridge.context();
        let receipt = bridge.configuration_changed(context).unwrap();
        assert!(token.is_cancelled());
        assert!(shutdown.load(Ordering::SeqCst));
        assert!(receipt.generation > context.generation);
        assert!(receipt.permission_epoch > context.permission_epoch);
        assert!(!bridge.has_production_host());

        let (active_root, mut active, _, _) = bridge_with_fake("configuration-active");
        let active_context = active.context();
        active.active_turn = Some(ActiveTurn {
            id: 7,
            session_id: "demo".into(),
        });
        assert_eq!(
            active
                .validate_configuration_change(active_context)
                .unwrap_err()
                .code,
            ErrorCode::ApprovalRejected
        );
        assert_eq!(active.context(), active_context);
        std::fs::remove_dir_all(root).ok();
        std::fs::remove_dir_all(active_root).ok();
    }

    #[test]
    fn session_operations_and_approval_epoch_are_validated() {
        let (root, mut bridge, _, _) = bridge_with_fake("session");
        let workspace = bridge.workspace_root.clone();
        let context = bridge.context();
        let config = HostConfig {
            workspace: workspace.clone(),
            ..HostConfig::default()
        };
        bridge.host_config = Some(config.clone().validate().unwrap());
        assert!(bridge
            .validate_installed_host(context, config.clone())
            .is_ok());
        assert_eq!(
            bridge
                .validate_installed_host(
                    context,
                    HostConfig {
                        model: "changed-model".into(),
                        ..config
                    },
                )
                .unwrap_err()
                .code,
            ErrorCode::ApprovalRejected
        );
        let created = bridge
            .session_operation(
                context,
                SessionOperation::Create {
                    session_id: "parent".into(),
                },
            )
            .unwrap();
        let parent = bridge.sessions_root.join("parent.jsonl");
        let mut session = JsonlSession::create(parent).unwrap();
        session
            .append(Event::UserMessage { text: "hi".into() })
            .unwrap();
        drop(session);
        let forked = bridge
            .session_operation(
                context,
                SessionOperation::Fork {
                    source_id: "parent".into(),
                    target_id: "child".into(),
                    boundary: None,
                },
            )
            .unwrap();
        assert_eq!(created.event_count, 0);
        assert_eq!(forked.event_count, 1);

        let turn = bridge
            .start_turn(context, "parent", &workspace, "hello")
            .unwrap();
        bridge
            .register_approval(context, turn.turn_id.unwrap(), "approval_1", 7)
            .unwrap();
        assert_eq!(
            bridge
                .respond_approval(context, "approval_1", 8, true)
                .unwrap_err()
                .code,
            ErrorCode::ApprovalRejected
        );
        assert!(bridge
            .respond_approval(context, "approval_1", 7, false)
            .is_ok());
        assert!(bridge
            .steer(context, turn.turn_id.unwrap(), "follow-up")
            .is_ok());
        assert!(bridge.finish_turn(context, turn.turn_id.unwrap()).is_ok());
        assert_eq!(
            bridge
                .finish_turn(context, turn.turn_id.unwrap())
                .unwrap_err()
                .code,
            ErrorCode::ToolArgsInvalid
        );
        std::fs::remove_dir_all(root).ok();
    }
}
