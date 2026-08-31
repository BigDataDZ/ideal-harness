//! TASK-706：Linux Landlock 生产后端（ABI v1 文件系统访问控制）。
//!
//! 隔离模型：子进程先 `PR_SET_NO_NEW_PRIVS`，再建立 Landlock ruleset，
//! 对「整个文件系统」声明只读 + 执行权；WorkspaceWrite 模式额外对
//! workspace 根授予全量文件权。未声明的访问一律默认拒绝（fail-closed）。
//! 已知边界：不做网络套接字限制（Landlock v4 的 NET 域，另行排卡）。

use crate::{CommandSpec, ExecutionOutput, RestrictedBackend};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

// ---- Landlock ABI v1 常量（内核文档 landlock.rst，x86_64/aarch64 同号）----

const ACCESS_READ_FILE: u64 = 1 << 0;
const ACCESS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_READ_DIR: u64 = 1 << 2;
const ACCESS_REMOVE_DIR: u64 = 1 << 3;
const ACCESS_REMOVE_FILE: u64 = 1 << 4;
const ACCESS_MAKE_CHAR: u64 = 1 << 5;
const ACCESS_MAKE_DIR: u64 = 1 << 6;
const ACCESS_MAKE_REG: u64 = 1 << 7;
const ACCESS_MAKE_SOCK: u64 = 1 << 8;
const ACCESS_MAKE_FIFO: u64 = 1 << 9;
const ACCESS_MAKE_BLOCK: u64 = 1 << 10;
const ACCESS_MAKE_SYM: u64 = 1 << 11;
const ACCESS_EXEC: u64 = 1 << 12;

/// 只读档：读文件/读目录/执行。
const READONLY_ACCESS: u64 = ACCESS_READ_FILE | ACCESS_READ_DIR | ACCESS_EXEC;
/// 工作区写档：全部文件权。
const WORKSPACE_ACCESS: u64 = READONLY_ACCESS
    | ACCESS_WRITE_FILE
    | ACCESS_REMOVE_DIR
    | ACCESS_REMOVE_FILE
    | ACCESS_MAKE_CHAR
    | ACCESS_MAKE_DIR
    | ACCESS_MAKE_REG
    | ACCESS_MAKE_SOCK
    | ACCESS_MAKE_FIFO
    | ACCESS_MAKE_BLOCK
    | ACCESS_MAKE_SYM;

const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
const O_PATH_FLAGS: libc::c_int = libc::O_PATH | libc::O_CLOEXEC;

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C)]
struct PathBeneathAttr {
    parent_fd: u64,
    allowed_access: u64,
}

/// Landlock 文件系统隔离档位（与 SandboxMode 三档中可落地的两档对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandlockFsMode {
    ReadOnly,
    WorkspaceWrite,
}

/// Landlock 受限后端：workspace 根为写权限边界。
#[derive(Debug, Clone)]
pub struct LandlockBackend {
    workspace_root: PathBuf,
    mode: LandlockFsMode,
    generation: u64,
}

impl LandlockBackend {
    pub fn new(workspace_root: PathBuf, mode: LandlockFsMode, generation: u64) -> Self {
        Self {
            workspace_root,
            mode,
            generation,
        }
    }

    /// 从进程当前目录与 `IDEAL_HARNESS_SANDBOX_MODE` 推导配置；
    /// 未知档位 fail-closed 拒绝（不静默回退）。
    pub fn from_environment(generation: u64) -> io::Result<Self> {
        let mode = match std::env::var("IDEAL_HARNESS_SANDBOX_MODE").as_deref() {
            Ok("read-only") | Ok("ReadOnly") => LandlockFsMode::ReadOnly,
            Ok("") | Err(_) => LandlockFsMode::WorkspaceWrite,
            Ok("workspace-write") | Ok("WorkspaceWrite") => LandlockFsMode::WorkspaceWrite,
            Ok(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown sandbox mode: {other}"),
                ))
            }
        };
        Ok(Self::new(std::env::current_dir()?, mode, generation))
    }
}

impl RestrictedBackend for LandlockBackend {
    fn execute(&self, command: &CommandSpec) -> io::Result<ExecutionOutput> {
        execute_under_landlock(self, command, None)
    }

    fn execute_with_deadline(
        &self,
        command: &CommandSpec,
        deadline: Option<std::time::Duration>,
    ) -> io::Result<ExecutionOutput> {
        execute_under_landlock(self, command, deadline)
    }

    fn environment(&self) -> io::Result<protocol::ExecutorEnvironment> {
        crate::local_executor_environment(self.generation)
    }
}

fn landlock_syscall(
    syscall: libc::c_long,
    a: usize,
    b: usize,
    c: usize,
) -> io::Result<libc::c_long> {
    let result = unsafe { libc::syscall(syscall, a, b, c) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result)
}

/// ABI 探测：内核返回其支持的最高 ABI 版本；不支持 Landlock 时 fail-closed。
fn probe_abi() -> io::Result<u64> {
    let version = landlock_syscall(
        SYS_LANDLOCK_CREATE_RULESET,
        0,
        0,
        LANDLOCK_CREATE_RULESET_VERSION as usize,
    )?;
    Ok(version as u64)
}

fn create_ruleset(handled_access_fs: u64) -> io::Result<RawFd> {
    let attr = RulesetAttr { handled_access_fs };
    let fd = landlock_syscall(
        SYS_LANDLOCK_CREATE_RULESET,
        &attr as *const RulesetAttr as usize,
        std::mem::size_of::<RulesetAttr>(),
        0,
    )?;
    Ok(fd as RawFd)
}

fn add_path_beneath(ruleset_fd: RawFd, parent_fd: RawFd, allowed_access: u64) -> io::Result<()> {
    let attr = PathBeneathAttr {
        parent_fd: parent_fd as u64,
        allowed_access,
    };
    landlock_syscall(
        SYS_LANDLOCK_ADD_RULE,
        ruleset_fd as usize,
        LANDLOCK_RULE_PATH_BENEATH as usize,
        &attr as *const PathBeneathAttr as usize,
    )?;
    Ok(())
}

fn open_dir_fd(path: &Path) -> io::Result<RawFd> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Landlock path contains an interior NUL byte",
        )
    })?;
    let fd = unsafe { libc::open(path.as_ptr(), O_PATH_FLAGS) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// 在子进程内落地 Landlock 限制；成功即不返回（exec），失败 _exit(126)。
fn restrict_child(workspace_root: &Path, mode: LandlockFsMode) {
    unsafe {
        if libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            libc::_exit(126);
        }
    }
    let outcome = (|| -> io::Result<()> {
        if probe_abi()? < 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "landlock abi unavailable",
            ));
        }
        let ruleset_fd = create_ruleset(WORKSPACE_ACCESS)?;
        // 整个文件系统：只读 + 执行
        let root_fd = open_dir_fd(Path::new("/"))?;
        add_path_beneath(ruleset_fd, root_fd, READONLY_ACCESS)?;
        unsafe { libc::close(root_fd) };
        if mode == LandlockFsMode::WorkspaceWrite {
            let canonical = workspace_root
                .canonicalize()
                .map_err(|error| io::Error::new(io::ErrorKind::NotFound, error))?;
            let workspace_fd = open_dir_fd(&canonical)?;
            add_path_beneath(ruleset_fd, workspace_fd, WORKSPACE_ACCESS)?;
            unsafe { libc::close(workspace_fd) };
        }
        landlock_syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd as usize, 0, 0)?;
        unsafe { libc::close(ruleset_fd) };
        Ok(())
    })();
    if outcome.is_err() {
        unsafe { libc::_exit(126) };
    }
}

fn execute_under_landlock(
    backend: &LandlockBackend,
    command: &CommandSpec,
    deadline: Option<std::time::Duration>,
) -> io::Result<ExecutionOutput> {
    // ABI 预检：无 Landlock 的内核直接拒绝，不降级为不受限执行
    probe_abi().and_then(|version| {
        if version < 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "landlock abi unavailable",
            ));
        }
        Ok(())
    })?;

    let mut stdout_pipe = [0 as libc::c_int; 2];
    let mut stderr_pipe = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(stdout_pipe.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::pipe(stderr_pipe.as_mut_ptr()) } != 0 {
        unsafe {
            libc::close(stdout_pipe[0]);
            libc::close(stdout_pipe[1]);
        }
        return Err(io::Error::last_os_error());
    }

    let program = command.program.clone();
    let args = command.args.clone();
    let current_dir = command.current_dir.clone();
    let current_dir_c = current_dir
        .as_ref()
        .map(|dir| {
            std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "working directory contains an interior NUL byte",
                )
            })
        })
        .transpose()?;
    let workspace_root = backend.workspace_root.clone();
    let mode = backend.mode;

    // exec 参数的 C 字符串在 fork 前构造（避免子进程内分配）
    let program_c = std::ffi::CString::new(
        program
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "program is not utf-8"))?,
    )?;
    let mut argv: Vec<std::ffi::CString> = Vec::with_capacity(args.len() + 1);
    argv.push(program_c.clone());
    for arg in &args {
        argv.push(std::ffi::CString::new(arg.as_str())?);
    }
    let mut argv_raw: Vec<*const libc::c_char> = argv.iter().map(|item| item.as_ptr()).collect();
    argv_raw.push(std::ptr::null());

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // 子进程：自成进程组（setpgid）→ 接管管道 → chdir → Landlock → exec
        unsafe {
            libc::setpgid(0, 0);
        }
        unsafe {
            libc::close(stdout_pipe[0]);
            libc::close(stderr_pipe[0]);
            libc::dup2(stdout_pipe[1], libc::STDOUT_FILENO);
            libc::dup2(stderr_pipe[1], libc::STDERR_FILENO);
            libc::close(stdout_pipe[1]);
            libc::close(stderr_pipe[1]);
            if let Some(dir) = &current_dir_c {
                if libc::chdir(dir.as_ptr()) != 0 {
                    libc::_exit(126);
                }
            }
        }
        restrict_child(&workspace_root, mode);
        unsafe {
            libc::execvp(program_c.as_ptr(), argv_raw.as_ptr());
            libc::_exit(127);
        }
    }

    // 父进程：关闭写端，读到 EOF，回收子进程
    unsafe {
        libc::close(stdout_pipe[1]);
        libc::close(stderr_pipe[1]);
    }
    let stdout = read_fd_to_end(stdout_pipe[0])?;
    let stderr = read_fd_to_end(stderr_pipe[0])?;
    unsafe {
        libc::close(stdout_pipe[0]);
        libc::close(stderr_pipe[0]);
    }

    // TASK-802：轮询等待；deadline 到期 killpg 终止整组（含受控子进程）。
    let deadline_at = deadline.map(|value| std::time::Instant::now() + value);
    let mut status: libc::c_int = 0;
    let mut timed_out = false;
    let wait = loop {
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            break result;
        }
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if deadline_at.is_some_and(|at| std::time::Instant::now() >= at) {
            timed_out = true;
            // SAFETY: pid 是自成进程组的子进程；SIGKILL 终止整组。
            if unsafe { libc::killpg(pid, libc::SIGKILL) } != 0 {
                return Err(io::Error::other(
                    "failed to terminate timed-out process group",
                ));
            }
            break unsafe { libc::waitpid(pid, &mut status, 0) };
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    if wait < 0 {
        return Err(io::Error::last_os_error());
    }
    let exit_code = if timed_out {
        124
    } else if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status) as u32
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status) as u32
    } else {
        1
    };
    Ok(ExecutionOutput {
        process_id: pid as u32,
        exit_code,
        stdout,
        stderr,
        restricted: true,
    })
}

fn read_fd_to_end(fd: RawFd) -> io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    // SAFETY: the caller uniquely owns this open pipe fd and closes it after this borrowed read.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut file = std::mem::ManuallyDrop::new(file);
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ih-landlock-{}-{name}", std::process::id()))
    }

    #[test]
    fn workspace_write_allows_inside_and_denies_outside() {
        let inside = workspace("write-in");
        std::fs::create_dir_all(&inside).unwrap();
        let backend = LandlockBackend::new(inside.clone(), LandlockFsMode::WorkspaceWrite, 0);
        // 工作区内写：允许
        let inside_file = inside.join("note.txt");
        let output = backend
            .execute(
                &CommandSpec::new("sh")
                    .arg("-c")
                    .arg(format!("echo hi > {}", inside_file.display())),
            )
            .unwrap();
        assert_eq!(output.exit_code, 0, "stderr: {:?}", output.stderr);
        assert!(output.restricted);
        assert_eq!(
            std::fs::read_to_string(&inside_file).unwrap(),
            "hi
"
        );
        // 工作区外写：Landlock 拒绝（EPERM → 非零退出）
        let outside = workspace("write-out");
        std::fs::create_dir_all(&outside).unwrap();
        let output = backend
            .execute(&CommandSpec::new("sh").arg("-c").arg(format!(
                "echo hi > {}",
                outside.join("escape.txt").display()
            )))
            .unwrap();
        assert_ne!(output.exit_code, 0, "越界写必须被拒绝");
        std::fs::remove_dir_all(inside).ok();
        std::fs::remove_dir_all(outside).ok();
    }

    #[test]
    fn read_only_mode_denies_write_even_inside_workspace() {
        let inside = workspace("ro");
        std::fs::create_dir_all(&inside).unwrap();
        let backend = LandlockBackend::new(inside.clone(), LandlockFsMode::ReadOnly, 0);
        let output = backend
            .execute(
                &CommandSpec::new("sh")
                    .arg("-c")
                    .arg(format!("echo hi > {}", inside.join("note.txt").display())),
            )
            .unwrap();
        assert_ne!(output.exit_code, 0, "只读档内写必须被拒绝");
        std::fs::remove_dir_all(inside).ok();
    }

    #[test]
    fn unknown_mode_from_environment_fails_closed() {
        std::env::set_var("IDEAL_HARNESS_SANDBOX_MODE", "danger-full-access");
        assert!(LandlockBackend::from_environment(0).is_err());
        std::env::remove_var("IDEAL_HARNESS_SANDBOX_MODE");
    }
}
