//! D25/TASK-903: explicit DTO command bridge and bounded Host lifecycle.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use harness_host::{
    CommandContext, CommandReceipt, DesktopBridge, ErrorCode, ErrorEnvelope, HostConfig,
    ProductionHost, ProviderProbeFailure, SessionOperation, SessionReceipt,
};
use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::secret_store::{SecretStore, SecretStoreError, SystemSecretStore};
use crate::settings::{ProviderSettings, SettingsStore};

#[derive(Clone)]
pub(crate) struct DesktopState {
    bridge: Arc<Mutex<DesktopBridge>>,
    settings: Arc<Mutex<SettingsStore>>,
    secrets: Arc<dyn SecretStore>,
    workspace: PathBuf,
}

fn internal_error_dto(message: String) -> CommandErrorDto {
    CommandErrorDto {
        code: "internal",
        message: Box::leak(message.into_boxed_str()),
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SecurityContextDto {
    generation: u64,
    permission_epoch: u64,
}

impl From<SecurityContextDto> for CommandContext {
    fn from(value: SecurityContextDto) -> Self {
        Self {
            generation: value.generation,
            permission_epoch: value.permission_epoch,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StartTurnDto {
    context: SecurityContextDto,
    session_id: String,
    input: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SettingsCommandDto {
    context: SecurityContextDto,
    settings: ProviderSettings,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SecretCommandDto {
    context: SecurityContextDto,
    settings: ProviderSettings,
    api_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ContextCommandDto {
    context: SecurityContextDto,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderSettingsDto {
    settings: ProviderSettings,
    has_api_key: bool,
    secure_storage_available: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderProbeDto {
    Connected,
    AuthenticationFailed { provider_message: Option<String> },
    NetworkUnavailable,
    TimedOut,
    Rejected { provider_message: Option<String> },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TurnCommandDto {
    context: SecurityContextDto,
    turn_id: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SteerDto {
    context: SecurityContextDto,
    turn_id: u64,
    input: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApprovalResponseDto {
    context: SecurityContextDto,
    request_id: String,
    executor_generation: u64,
    approved: bool,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SessionOperationDto {
    Create {
        context: SecurityContextDto,
        session_id: String,
    },
    Resume {
        context: SecurityContextDto,
        session_id: String,
    },
    Fork {
        context: SecurityContextDto,
        source_id: String,
        target_id: String,
        boundary: Option<usize>,
    },
    Revert {
        context: SecurityContextDto,
        source_id: String,
        target_id: String,
        turn_id: u64,
    },
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandReceiptDto {
    operation: String,
    generation: u64,
    permission_epoch: u64,
    turn_id: Option<u64>,
}

impl From<CommandReceipt> for CommandReceiptDto {
    fn from(value: CommandReceipt) -> Self {
        Self {
            operation: value.operation.into(),
            generation: value.generation,
            permission_epoch: value.permission_epoch,
            turn_id: value.turn_id,
        }
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionReceiptDto {
    session_id: String,
    event_count: u64,
    generation: u64,
}

impl From<SessionReceipt> for SessionReceiptDto {
    fn from(value: SessionReceipt) -> Self {
        Self {
            session_id: value.session_id,
            event_count: value.event_count,
            generation: value.generation,
        }
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandErrorDto {
    code: &'static str,
    message: &'static str,
}

impl From<ErrorEnvelope> for CommandErrorDto {
    fn from(value: ErrorEnvelope) -> Self {
        let (code, message) = match value.code {
            ErrorCode::ToolArgsInvalid => ("tool_args_invalid", "请求参数无效"),
            ErrorCode::SandboxDenied => ("sandbox_denied", "请求超出安全边界"),
            ErrorCode::ApprovalRejected => ("approval_rejected", "审批不存在、已拒绝或已过期"),
            ErrorCode::SessionNotFound => ("session_not_found", "会话不存在"),
            ErrorCode::CursorInvalid => ("cursor_invalid", "客户端代际已过期，请刷新后重试"),
            ErrorCode::ToolTimeout => ("tool_timeout", "操作已取消或超时"),
            ErrorCode::ContextWindowExceeded => ("context_window_exceeded", "上下文窗口不足"),
            ErrorCode::ModelStreamBroken => ("model_stream_broken", "模型流已中断"),
            ErrorCode::SubagentCancelled => ("subagent_cancelled", "子任务已取消"),
            ErrorCode::TeamRevisionConflict => ("team_revision_conflict", "团队状态版本冲突"),
            ErrorCode::TeamDependencyCycle => ("team_dependency_cycle", "团队任务依赖成环"),
            ErrorCode::ToolLoopDetected => ("tool_loop_detected", "工具循环已被阻止"),
            ErrorCode::FileRevisionConflict => ("file_revision_conflict", "文件版本已变化"),
            ErrorCode::Internal => ("internal", "宿主暂时不可用"),
        };
        Self { code, message }
    }
}

#[tauri::command]
pub(crate) fn desktop_status(
    state: tauri::State<'_, DesktopState>,
) -> Result<CommandReceiptDto, CommandErrorDto> {
    let bridge = lock_bridge(&state)?;
    let context = bridge.context();
    Ok(CommandReceiptDto {
        operation: format!("desktop_status_v{}", env!("CARGO_PKG_VERSION")),
        generation: context.generation,
        permission_epoch: context.permission_epoch,
        turn_id: None,
    })
}

#[tauri::command]
pub(crate) async fn get_provider_settings(
    state: tauri::State<'_, DesktopState>,
) -> Result<ProviderSettingsDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || get_provider_settings_blocking(&state))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn get_provider_settings_blocking(state: &DesktopState) -> Result<ProviderSettingsDto, CommandErrorDto> {

    let settings = lock_settings(state)?
        .load()
        .map_err(CommandErrorDto::from)?;
    let (has_api_key, secure_storage_available) = match state.secrets.has_api_key() {
        Ok(value) => (value, true),
        Err(SecretStoreError::Unavailable) => (false, false),
        Err(_) => (false, true),
    };
    Ok(ProviderSettingsDto {
        settings,
        has_api_key,
        secure_storage_available,
    })

}

#[tauri::command]
pub(crate) async fn save_provider_settings(
    request: SettingsCommandDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<CommandReceiptDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || save_provider_settings_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn save_provider_settings_blocking(state: &DesktopState, request: SettingsCommandDto) -> Result<CommandReceiptDto, CommandErrorDto> {

    let context = request.context.into();
    let mut bridge = lock_bridge(state)?;
    bridge
        .validate_configuration_change(context)
        .map_err(CommandErrorDto::from)?;
    let config = host_config(&state.workspace, &request.settings);
    config.validate().map_err(|_| {
        CommandErrorDto::from(ErrorEnvelope::new(
            ErrorCode::ToolArgsInvalid,
            "provider settings are invalid",
        ))
    })?;
    let settings = request.settings.validate().map_err(CommandErrorDto::from)?;
    lock_settings(state)?
        .save(settings)
        .map_err(CommandErrorDto::from)?;
    bridge
        .configuration_changed(context)
        .map(CommandReceiptDto::from)
        .map_err(CommandErrorDto::from)

}

#[tauri::command]
pub(crate) async fn store_api_key(
    request: SecretCommandDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<CommandReceiptDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || store_api_key_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn store_api_key_blocking(state: &DesktopState, request: SecretCommandDto) -> Result<CommandReceiptDto, CommandErrorDto> {
    log_diag(&state.workspace, "store_api_key: starting");
    if request.api_key.trim().is_empty() {
        return Err(CommandErrorDto::from(ErrorEnvelope::new(
            ErrorCode::ToolArgsInvalid,
            "API key is blank",
        )));
    }
    // TASK-909 修复：粘贴带入的换行/空格会让 Bearer 头被 Provider 拒绝——
    // 入库前 trim；key 内部含空白字符属于明显的粘贴错误，直接拒绝而非静默截断。
    let trimmed_key = request.api_key.trim().to_owned();
    if trimmed_key.chars().any(|character| character.is_whitespace()) {
        return Err(CommandErrorDto::from(ErrorEnvelope::new(
            ErrorCode::ToolArgsInvalid,
            "API key contains internal whitespace; please re-paste it without line breaks",
        )));
    }
    let receipt = lock_bridge(state)?
        .configuration_changed(request.context.into())
        .map_err(CommandErrorDto::from)?;
    state
        .secrets
        .set_api_key(&trimmed_key)
        .map_err(secret_error)?;
    // TASK-909 修复：记录掩码指纹（非密钥材料），设置页显示「存的是哪把」
    {
        let settings = lock_settings(state)?;
        let mut current = settings.load().map_err(CommandErrorDto::from)?;
        // TASK-909 修复：表单里的 Base URL/Model/白名单与密钥同批生效
        current.base_url = request.settings.base_url;
        current.model = request.settings.model;
        current.fetch_allow = request.settings.fetch_allow;
        current.compact_mode = request.settings.compact_mode;
        current.api_key_mask = Some(key_mask(&trimmed_key));
        settings.save(current).map_err(CommandErrorDto::from)?;
    }
    Ok(receipt.into())

}

/// key 掩码指纹：首 4 + 尾 4，中间省略。
fn key_mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

#[tauri::command]
pub(crate) async fn delete_api_key(
    request: ContextCommandDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<CommandReceiptDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || delete_api_key_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn delete_api_key_blocking(state: &DesktopState, request: ContextCommandDto) -> Result<CommandReceiptDto, CommandErrorDto> {
    let receipt = lock_bridge(state)?
        .configuration_changed(request.context.into())
        .map_err(CommandErrorDto::from)?;
    match state.secrets.delete_api_key() {
        Ok(()) | Err(SecretStoreError::Missing) => {
            // TASK-909 修复：删 key 同步清除掩码指纹
            let settings = lock_settings(state)?;
            let mut current = settings.load().map_err(CommandErrorDto::from)?;
            current.api_key_mask = None;
            settings.save(current).map_err(CommandErrorDto::from)?;
            Ok(receipt.into())
        }
        Err(error) => Err(secret_error(error)),
    }
}

#[tauri::command]
pub(crate) async fn test_provider_connection(
    request: ContextCommandDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<ProviderProbeDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || test_provider_connection_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn test_provider_connection_blocking(state: &DesktopState, request: ContextCommandDto) -> Result<ProviderProbeDto, CommandErrorDto> {
    log_diag(&state.workspace, "probe: starting connectivity test");

    let context = request.context.into();
    lock_bridge(state)?
        .validate_command_context(context)
        .map_err(CommandErrorDto::from)?;
    let settings = lock_settings(state)?
        .load()
        .map_err(CommandErrorDto::from)?;
    let api_key = state
        .secrets
        .get_api_key()
        .map_err(secret_error)?
        .trim()
        .to_owned();
    let host =
        ProductionHost::start_with_api_key(host_config(&state.workspace, &settings), api_key, None)
            .map_err(|_| {
                CommandErrorDto::from(ErrorEnvelope::new(
                    ErrorCode::Internal,
                    "provider probe could not initialize",
                ))
            })?;
    let result = host.probe_provider();
    if let Err(ref failure) = result {
        log_diag(&state.workspace, &format!("probe: FAILED {:?}", failure));
    } else {
        log_diag(&state.workspace, "probe: connected");
    }
    let _ = host.shutdown();
    Ok(match result {
        Ok(()) => ProviderProbeDto::Connected,
        Err(ProviderProbeFailure::Authentication { provider_message }) => {
            ProviderProbeDto::AuthenticationFailed { provider_message }
        }
        Err(ProviderProbeFailure::Network) => ProviderProbeDto::NetworkUnavailable,
        Err(ProviderProbeFailure::Timeout) => ProviderProbeDto::TimedOut,
        Err(ProviderProbeFailure::Rejected { provider_message }) => {
            ProviderProbeDto::Rejected { provider_message }
        }
    })

}

#[tauri::command]
pub(crate) async fn start_turn(
    request: StartTurnDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<CommandReceiptDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || start_turn_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn start_turn_blocking(state: &DesktopState, request: StartTurnDto) -> Result<CommandReceiptDto, CommandErrorDto> {
    log_diag(&state.workspace, &format!("start_turn: session={} input={:?}", request.session_id, request.input.chars().take(80).collect::<String>()));
    let context = request.context.into();
    let settings = lock_settings(state)?
        .load()
        .map_err(CommandErrorDto::from)?;
    let config = host_config(&state.workspace, &settings);
    if !bridge.has_production_host() {
        let api_key = state
        .secrets
        .get_api_key()
        .map_err(secret_error)?
        .trim()
        .to_owned();
        bridge
            .install_production_host_with_api_key(context, config, api_key, None)
            .map_err(CommandErrorDto::from)?;
    } else {
        bridge
            .validate_installed_host(context, config)
            .map_err(CommandErrorDto::from)?;
    }
    bridge
        .start_turn(
            context,
            &request.session_id,
            &state.workspace,
            &request.input,
        )
        .map(CommandReceiptDto::from)
        .map_err(CommandErrorDto::from)

}

#[tauri::command]
pub(crate) async fn stop_turn(
    request: TurnCommandDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<CommandReceiptDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || stop_turn_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn stop_turn_blocking(state: &DesktopState, request: TurnCommandDto) -> Result<CommandReceiptDto, CommandErrorDto> {
    cancel_active_turn_blocking(state, request)
}

#[tauri::command]
pub(crate) async fn cancel_turn(
    request: TurnCommandDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<CommandReceiptDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || cancel_turn_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn cancel_turn_blocking(state: &DesktopState, request: TurnCommandDto) -> Result<CommandReceiptDto, CommandErrorDto> {
    cancel_active_turn_blocking(state, request)
}

fn cancel_active_turn_blocking(
    state: &DesktopState,
    request: TurnCommandDto,
) -> Result<CommandReceiptDto, CommandErrorDto> {
    lock_bridge(state)?
        .cancel_turn(request.context.into(), request.turn_id)
        .map(CommandReceiptDto::from)
        .map_err(CommandErrorDto::from)
}

#[tauri::command]
pub(crate) async fn steer_turn(
    request: SteerDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<CommandReceiptDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || steer_turn_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn steer_turn_blocking(state: &DesktopState, request: SteerDto) -> Result<CommandReceiptDto, CommandErrorDto> {

    lock_bridge(state)?
        .steer(request.context.into(), request.turn_id, &request.input)
        .map(CommandReceiptDto::from)
        .map_err(CommandErrorDto::from)

}

#[tauri::command]
pub(crate) async fn respond_approval(
    request: ApprovalResponseDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<CommandReceiptDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || respond_approval_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn respond_approval_blocking(state: &DesktopState, request: ApprovalResponseDto) -> Result<CommandReceiptDto, CommandErrorDto> {

    lock_bridge(state)?
        .respond_approval(
            request.context.into(),
            &request.request_id,
            request.executor_generation,
            request.approved,
        )
        .map(CommandReceiptDto::from)
        .map_err(CommandErrorDto::from)

}

#[tauri::command]
pub(crate) async fn session_operation(
    request: SessionOperationDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<SessionReceiptDto, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || session_operation_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn session_operation_blocking(state: &DesktopState, request: SessionOperationDto) -> Result<SessionReceiptDto, CommandErrorDto> {

    let (context, operation) = session_request(request);
    lock_bridge(state)?
        .session_operation(context, operation)
        .map(SessionReceiptDto::from)
        .map_err(CommandErrorDto::from)

}

/// TASK-909：按序返回会话事件帧（seq > last_seq），供前端 SessionProjection 重建真相源。
#[tauri::command]
pub(crate) async fn session_event_frames(
    request: SessionEventFramesRequestDto,
    state: tauri::State<'_, DesktopState>,
) -> Result<Vec<SessionEventFrameDto>, CommandErrorDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || session_event_frames_blocking(&state, request))
        .await
        .map_err(|error| internal_error_dto(format!("command join failed: {error}")))?
}

fn session_event_frames_blocking(state: &DesktopState, request: SessionEventFramesRequestDto) -> Result<Vec<SessionEventFrameDto>, CommandErrorDto> {

    let frames = lock_bridge(state)?
        .session_event_frames(&request.session_id, request.last_seq, request.limit)
        .map_err(CommandErrorDto::from)?;
    Ok(frames
        .into_iter()
        .map(SessionEventFrameDto::from)
        .collect())

}

fn session_request(request: SessionOperationDto) -> (CommandContext, SessionOperation) {
    match request {
        SessionOperationDto::Create {
            context,
            session_id,
        } => (context.into(), SessionOperation::Create { session_id }),
        SessionOperationDto::Resume {
            context,
            session_id,
        } => (context.into(), SessionOperation::Resume { session_id }),
        SessionOperationDto::Fork {
            context,
            source_id,
            target_id,
            boundary,
        } => (
            context.into(),
            SessionOperation::Fork {
                source_id,
                target_id,
                boundary,
            },
        ),
        SessionOperationDto::Revert {
            context,
            source_id,
            target_id,
            turn_id,
        } => (
            context.into(),
            SessionOperation::Revert {
                source_id,
                target_id,
                turn_id,
            },
        ),
    }
}

fn lock_bridge<'a>(
    state: &'a DesktopState,
) -> Result<std::sync::MutexGuard<'a, DesktopBridge>, CommandErrorDto> {
    state.bridge.lock().map_err(|_| CommandErrorDto {
        code: "internal",
        message: "宿主状态不可用",
    })
}

fn lock_settings<'a>(
    state: &'a DesktopState,
) -> Result<std::sync::MutexGuard<'a, SettingsStore>, CommandErrorDto> {
    state.settings.lock().map_err(|_| CommandErrorDto {
        code: "internal",
        message: "设置状态不可用",
    })
}

fn host_config(workspace: &Path, settings: &ProviderSettings) -> HostConfig {
    HostConfig {
        base_url: settings.base_url.clone(),
        model: settings.model.clone(),
        fetch_allow: settings.fetch_allow.clone(),
        workspace: workspace.to_path_buf(),
        plugin_root: None,
    }
}

fn secret_error(error: SecretStoreError) -> CommandErrorDto {
    let envelope = match error {
        SecretStoreError::Missing => {
            ErrorEnvelope::new(ErrorCode::ApprovalRejected, "API credential is missing")
        }
        SecretStoreError::Rejected => {
            ErrorEnvelope::new(ErrorCode::ToolArgsInvalid, "API credential was rejected")
        }
        SecretStoreError::Unavailable => ErrorEnvelope::new(
            ErrorCode::Internal,
            "secure credential storage is unavailable",
        ),
    };
    envelope.into()
}

pub(crate) fn initialize_state(workspace: &Path) -> Result<DesktopState, ErrorEnvelope> {
    initialize_state_with_secrets(workspace, Arc::new(SystemSecretStore))
}

fn initialize_state_with_secrets(
    workspace: &Path,
    secrets: Arc<dyn SecretStore>,
) -> Result<DesktopState, ErrorEnvelope> {
    let workspace = workspace
        .canonicalize()
        .map_err(|_| ErrorEnvelope::new(ErrorCode::Internal, "workspace is unavailable"))?;
    let sessions = workspace.join(".harness").join("desktop-sessions");
    std::fs::create_dir_all(&sessions)
        .map_err(|_| ErrorEnvelope::new(ErrorCode::Internal, "cannot create session directory"))?;
    DesktopBridge::new(&workspace, &sessions).map(|bridge| DesktopState {
        bridge: Arc::new(Mutex::new(bridge)),
        settings: Arc::new(Mutex::new(SettingsStore::new(&workspace))),
        secrets,
        workspace,
    })
}

pub(crate) fn close_window(window: &tauri::Window) {
    if let Ok(mut bridge) = window.state::<DesktopState>().bridge.lock() {
        let _ = bridge.close();
    }
}

#[cfg(test)]
/// TASK-909 诊断：追加到 `<workspace>/.harness/desktop.log` 的日志行。
pub(crate) fn log_diag(workspace: &Path, message: &str) {
    let log_dir = workspace.join(".harness");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("desktop.log");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        use std::io::Write as _;
        let _ = writeln!(file, "[{}] {}", timestamp, message);
    }
    eprintln!("[ideal-harness] {}", message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MemorySecretStore(Mutex<Option<String>>);

    impl SecretStore for MemorySecretStore {
        fn set_api_key(&self, value: &str) -> Result<(), SecretStoreError> {
            *self.0.lock().unwrap() = Some(value.to_owned());
            Ok(())
        }

        fn get_api_key(&self) -> Result<String, SecretStoreError> {
            self.0
                .lock()
                .unwrap()
                .clone()
                .ok_or(SecretStoreError::Missing)
        }

        fn delete_api_key(&self) -> Result<(), SecretStoreError> {
            self.0.lock().unwrap().take();
            Ok(())
        }
    }

    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-tauri-{}-{name}", std::process::id()))
    }

    /// TASK-909/808 验收 2：真实模型全流程冒烟。
    /// 需要 IDEAL_HARNESS_SMOKE_KEY 环境变量与真实网络；#[ignore] 与 CI 隔离。
    /// 流程：设置+key 原子生效 → 新建会话 → start_turn（读文件→CAS 编辑→完成）→ 校验文件与事件。
    #[test]
    #[ignore = "requires IDEAL_HARNESS_SMOKE_KEY and network access to the real provider"]
    fn real_model_smoke_end_to_end() {
        let Ok(api_key) = std::env::var("IDEAL_HARNESS_SMOKE_KEY") else {
            eprintln!("smoke skipped: IDEAL_HARNESS_SMOKE_KEY not set");
            return;
        };
        let workspace = root("real-smoke");
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(
            workspace.join("src/hello.rs"),
            "pub const GREETING: &str = \"foo-version\";\n",
        )
        .unwrap();

        let state = initialize_state_with_secrets(
            &workspace,
            std::sync::Arc::new(SystemSecretStore) as std::sync::Arc<dyn SecretStore>,
        )
        .unwrap();

        // 1) 设置 + key 原子生效（单次代际推进 1→2）
        let context = SecurityContextDto { generation: 1, permission_epoch: 1 };
        store_api_key_blocking(
            &state,
            SecretCommandDto {
                context: context.clone(),
                settings: ProviderSettings {
                    base_url: "https://open.bigmodel.cn/api/coding/paas/v4".into(),
                    model: "glm-5.3-flash".into(),
                    fetch_allow: vec![],
                    compact_mode: false,
                    api_key_mask: None,
                },
                api_key,
            },
        )
        .expect("store key");
        // 代际已推进：后续命令必须携带新代际（旧代际被拒是 603 防护的正确行为）
        let context = SecurityContextDto { generation: 2, permission_epoch: 2 };

        // 2) 新建会话
        session_operation_blocking(
            &state,
            SessionOperationDto::Create {
                context: context.clone(),
                session_id: "smoke".into(),
            },
        )
        .expect("create session");

        // 3) start_turn：真实模型驱动的编辑任务
        let receipt = start_turn_blocking(
            &state,
            StartTurnDto {
                context: context.clone(),
                session_id: "smoke".into(),
                input: "请读取 src/hello.rs，把其中的 foo-version 改成 bar-version 并保存文件。".into(),
            },
        )
        .expect("start turn");
        let _turn_id = receipt.turn_id;

        // 4) 轮询等待 turn 完成（最长 180 秒）——只读会话文件原始文本，桌面 crate
        //    不直接依赖 session/protocol，保持依赖方向纯粹。
        let session_path =
            workspace.join(".harness").join("desktop-sessions").join("smoke.jsonl");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        loop {
            let raw = std::fs::read_to_string(&session_path).unwrap_or_default();
            if raw.contains("turn_completed") || raw.contains("turn_aborted") {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("turn did not finish within 180s; session: {raw}");
            }
            std::thread::sleep(std::time::Duration::from_millis(1_500));
        }

        // 5) 断言：文件被真实 CAS 编辑
        let final_content = std::fs::read_to_string(workspace.join("src/hello.rs")).unwrap();
        assert!(
            final_content.contains("bar-version"),
            "真实模型必须完成编辑，实际内容: {final_content}"
        );
        // 6) 断言：事件轨迹包含真实工具链
        let events_raw = std::fs::read_to_string(&session_path).unwrap();
        assert!(events_raw.contains("fs_read"), "缺 fs_read");
        assert!(events_raw.contains("fs_edit"), "缺 fs_edit");
        std::fs::remove_dir_all(&workspace).ok();
    }

    #[test]
    fn state_uses_shared_bridge_and_closes_fail_closed() {
        let root = root("state");
        std::fs::create_dir_all(&root).unwrap();
        let state =
            initialize_state_with_secrets(&root, Arc::new(MemorySecretStore::default())).unwrap();
        let mut bridge = state.bridge.lock().unwrap();
        let context = bridge.context();
        assert_eq!(context.generation, 1);
        bridge.close().unwrap();
        assert_eq!(
            bridge
                .session_operation(
                    bridge.context(),
                    SessionOperation::Create {
                        session_id: "late".into(),
                    },
                )
                .unwrap_err()
                .code,
            ErrorCode::SandboxDenied
        );
        drop(bridge);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn secret_store_deletion_is_observable_without_returning_plaintext() {
        let store = MemorySecretStore::default();
        store.set_api_key("top-secret-key").unwrap();
        assert_eq!(store.has_api_key(), Ok(true));
        store.delete_api_key().unwrap();
        assert_eq!(store.has_api_key(), Ok(false));

        let response = ProviderSettingsDto {
            settings: ProviderSettings::default(),
            has_api_key: true,
            secure_storage_available: true,
        };
        let encoded = serde_json::to_string(&response).unwrap();
        assert!(!encoded.contains("top-secret-key"));
        assert!(!encoded.to_ascii_lowercase().contains("api_key"));
    }

    #[test]
    fn command_errors_expose_only_static_safe_text() {
        let error = CommandErrorDto::from(ErrorEnvelope::new(
            ErrorCode::Internal,
            "IDEAL_HARNESS_API_KEY=secret approval payload",
        ));
        let encoded = serde_json::to_string(&error).unwrap();
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("approval payload"));
        assert_eq!(error.code, "internal");
    }

    #[test]
    fn tauri_configuration_keeps_plugin_permissions_empty_and_pages_local() {
        let capability = include_str!("../capabilities/desktop-shell.json");
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert!(capability.contains("\"permissions\": []"));
        assert_eq!(
            config["build"]["devUrl"].as_str(),
            Some("http://127.0.0.1:1420")
        );
        assert!(config["app"]["windows"][0].get("url").is_none());
        assert!(config["app"]["security"]["csp"]
            .as_str()
            .unwrap()
            .contains("frame-src 'none'"));
    }
}

/// TASK-909：session_event_frames 请求 DTO。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionEventFramesRequestDto {
    pub session_id: String,
    #[serde(default)]
    pub last_seq: u64,
    #[serde(default = "default_frame_limit")]
    pub limit: usize,
}

fn default_frame_limit() -> usize {
    500
}

/// TASK-909：事件帧响应 DTO（wire 与 protocol::SessionEventFrame 对齐）。
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SessionEventFrameDto {
    pub session_id: String,
    pub connection_generation: u64,
    pub record: protocol::SequencedEvent,
}

impl From<protocol::SessionEventFrame> for SessionEventFrameDto {
    fn from(frame: protocol::SessionEventFrame) -> Self {
        Self {
            session_id: frame.session_id,
            connection_generation: frame.connection_generation,
            record: frame.record,
        }
    }
}
