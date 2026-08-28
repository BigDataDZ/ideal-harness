//! P2/TASK-202：外部命令只通过独立的 OS 受限子进程执行。

use std::{io, path::PathBuf};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_command_line;
#[cfg(windows)]
mod windows_pipe_reader;

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
}

/// 当前平台的生产后端。没有可用 OS 隔离时拒绝执行，不回退到普通进程。
#[derive(Debug, Default, Clone, Copy)]
pub struct PlatformRestrictedBackend;

#[cfg(windows)]
impl RestrictedBackend for PlatformRestrictedBackend {
    fn execute(&self, command: &CommandSpec) -> io::Result<ExecutionOutput> {
        windows::execute(command)
    }
}

#[cfg(not(windows))]
impl RestrictedBackend for PlatformRestrictedBackend {
    fn execute(&self, _: &CommandSpec) -> io::Result<ExecutionOutput> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no restricted process backend is composed for this platform",
        ))
    }
}
