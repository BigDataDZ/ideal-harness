//! P2/TASK-202：外部命令只通过独立的 OS 受限子进程执行。

use protocol::ExecutorEnvironment;
use std::{io, path::PathBuf};

#[cfg(target_os = "linux")]
mod landlock_backend;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_command_line;
#[cfg(windows)]
mod windows_pipe_reader;

#[cfg(target_os = "linux")]
pub use landlock_backend::{LandlockBackend, LandlockFsMode};

/// 一次外部命令执行请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub current_dir: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }
}

/// 受限子进程的可审计执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOutput {
    pub process_id: u32,
    pub exit_code: u32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// 后端已验证子进程 token/隔离机制处于受限状态。
    pub restricted: bool,
}

/// OS 受限执行后端。Linux Landlock 后端将实现同一接口。
pub trait RestrictedBackend {
    fn execute(&self, command: &CommandSpec) -> io::Result<ExecutionOutput>;

    /// TASK-802：带 deadline 的执行。外部命令超时必须终止进程**及其受控子进程**；
    /// 终止失败时返回 Err（fail-closed），绝不把无法终止的进程伪装成已收口。
    /// None = 不限时（与 execute 等价，行为兼容）。
    fn execute_with_deadline(
        &self,
        command: &CommandSpec,
        deadline: Option<std::time::Duration>,
    ) -> io::Result<ExecutionOutput> {
        let _ = deadline;
        self.execute(command)
    }

    /// 返回执行器自身的 OS/home/workspace 事实；缺失时调用方必须拒绝授权。
    fn environment(&self) -> io::Result<ExecutorEnvironment> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "restricted backend did not expose executor environment facts",
        ))
    }
}

/// 屏障式执行入口：调用方永远不能绕过所组合的受限后端。
pub struct RestrictedProcessPool<B> {
    backend: B,
}

impl<B> RestrictedProcessPool<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

impl<B: RestrictedBackend> RestrictedProcessPool<B> {
    pub fn execute(&self, command: &CommandSpec) -> io::Result<ExecutionOutput> {
        self.backend.execute(command)
    }

    /// TASK-802：带 deadline 的执行；后端负责终止进程树。
    pub fn execute_with_deadline(
        &self,
        command: &CommandSpec,
        deadline: Option<std::time::Duration>,
    ) -> io::Result<ExecutionOutput> {
        self.backend.execute_with_deadline(command, deadline)
    }

    pub fn environment(&self) -> io::Result<ExecutorEnvironment> {
        self.backend.environment()
    }
}

/// 当前平台的生产后端。没有可用 OS 隔离时拒绝执行，不回退到普通进程。
#[derive(Debug, Default, Clone, Copy)]
pub struct PlatformRestrictedBackend;

#[cfg(windows)]
impl RestrictedBackend for PlatformRestrictedBackend {
    fn execute(&self, command: &CommandSpec) -> io::Result<ExecutionOutput> {
        windows::execute(command)
    }

    fn execute_with_deadline(
        &self,
        command: &CommandSpec,
        deadline: Option<std::time::Duration>,
    ) -> io::Result<ExecutionOutput> {
        windows::execute_with_deadline(command, deadline)
    }

    fn environment(&self) -> io::Result<ExecutorEnvironment> {
        local_executor_environment(0)
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
impl RestrictedBackend for PlatformRestrictedBackend {
    fn execute(&self, _: &CommandSpec) -> io::Result<ExecutionOutput> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no restricted process backend is composed for this platform",
        ))
    }

    fn environment(&self) -> io::Result<ExecutorEnvironment> {
        local_executor_environment(0)
    }
}

#[cfg(target_os = "linux")]
impl RestrictedBackend for PlatformRestrictedBackend {
    fn execute(&self, command: &CommandSpec) -> io::Result<ExecutionOutput> {
        landlock_backend::LandlockBackend::from_environment(0)?.execute(command)
    }

    fn execute_with_deadline(
        &self,
        command: &CommandSpec,
        deadline: Option<std::time::Duration>,
    ) -> io::Result<ExecutionOutput> {
        landlock_backend::LandlockBackend::from_environment(0)?
            .execute_with_deadline(command, deadline)
    }

    fn environment(&self) -> io::Result<ExecutorEnvironment> {
        landlock_backend::LandlockBackend::from_environment(0)?.environment()
    }
}

pub(crate) fn local_executor_environment(generation: u64) -> io::Result<ExecutorEnvironment> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "executor home is unavailable"))?;
    let workspace = std::env::current_dir()?;
    Ok(ExecutorEnvironment {
        os: std::env::consts::OS.to_string(),
        home: PathBuf::from(home).to_string_lossy().into_owned(),
        workspace: workspace.to_string_lossy().into_owned(),
        generation,
    })
}
