//! Windows Restricted Token 后端；所有失败路径均拒绝普通进程回退。

use crate::{
    windows_command_line::build_command_line,
    windows_pipe_reader::{join_reader, reader_thread},
    CommandSpec, ExecutionOutput,
};
use std::time::Duration;
use std::{
    ffi::c_void,
    io,
    mem::{size_of, zeroed},
    os::windows::ffi::OsStrExt,
    ptr::{null, null_mut},
};

type Handle = *mut c_void;
type Bool = i32;

const TRUE: Bool = 1;
const TOKEN_ASSIGN_PRIMARY: u32 = 0x0001;
const TOKEN_DUPLICATE: u32 = 0x0002;
const TOKEN_QUERY: u32 = 0x0008;
const DISABLE_MAX_PRIVILEGE: u32 = 0x0001;
const HANDLE_FLAG_INHERIT: u32 = 0x0001;
const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const CREATE_SUSPENDED: u32 = 0x0000_0004;
const INFINITE: u32 = 0xffff_ffff;
const WAIT_FAILED: u32 = 0xffff_ffff;

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    security_descriptor: *mut c_void,
    inherit_handle: Bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: u32,
}

#[repr(C)]
struct TokenUser {
    user: SidAndAttributes,
}

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_count_chars: u32,
    y_count_chars: u32,
    fill_attribute: u32,
    flags: u32,
    show_window: u16,
    reserved_size: u16,
    reserved_bytes: *mut u8,
    stdin: Handle,
    stdout: Handle,
    stderr: Handle,
}

#[repr(C)]
struct ProcessInformation {
    process: Handle,
    thread: Handle,
    process_id: u32,
    thread_id: u32,
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> Bool;
    fn CreateRestrictedToken(
        existing_token: Handle,
        flags: u32,
        disable_sid_count: u32,
        sids_to_disable: *const c_void,
        delete_privilege_count: u32,
        privileges_to_delete: *const c_void,
        restricted_sid_count: u32,
        sids_to_restrict: *const c_void,
        new_token: *mut Handle,
    ) -> Bool;
    fn CreateProcessAsUserW(
        token: Handle,
        application_name: *const u16,
        command_line: *mut u16,
        process_attributes: *const SecurityAttributes,
        thread_attributes: *const SecurityAttributes,
        inherit_handles: Bool,
        creation_flags: u32,
        environment: *const c_void,
        current_directory: *const u16,
        startup_info: *const StartupInfoW,
        process_information: *mut ProcessInformation,
    ) -> Bool;
    fn IsTokenRestricted(token: Handle) -> Bool;
    fn CreateWellKnownSid(
        sid_type: i32,
        domain_sid: *mut c_void,
        sid: *mut c_void,
        sid_size: *mut u32,
    ) -> Bool;
    fn GetTokenInformation(
        token: Handle,
        information_class: i32,
        information: *mut c_void,
        information_length: u32,
        return_length: *mut u32,
    ) -> Bool;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn CloseHandle(handle: Handle) -> Bool;
    fn CreatePipe(
        read_pipe: *mut Handle,
        write_pipe: *mut Handle,
        attributes: *const SecurityAttributes,
        size: u32,
    ) -> Bool;
    fn SetHandleInformation(handle: Handle, mask: u32, flags: u32) -> Bool;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> Bool;
    fn ResumeThread(thread: Handle) -> u32;
    fn TerminateProcess(process: Handle, exit_code: u32) -> Bool;
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(
        job: Handle,
        class: i32,
        information: *const c_void,
        length: u32,
    ) -> Bool;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> Bool;
}

const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
/// 实证确认的 x64 JOBOBJECT_EXTENDED_LIMIT_INFORMATION 长度（探针：144 接受，120 报
/// ERROR_BAD_LENGTH）；LimitFlags 位于 JOBOBJECT_BASIC_LIMIT_INFORMATION 偏移 32。
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_SIZE: usize = 144;
const JOB_OBJECT_LIMIT_FLAGS_OFFSET: usize = 32;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
/// 超时被终止时的约定退出码（类 Unix timeout 惯例）。
const DEADLINE_EXIT_CODE: u32 = 124;

struct OwnedHandle(Handle);

impl OwnedHandle {
    fn null() -> Self {
        Self(null_mut())
    }

    fn take(&mut self) -> Handle {
        std::mem::replace(&mut self.0, null_mut())
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this wrapper uniquely owns the non-null Win32 handle.
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct Pipe {
    read: OwnedHandle,
    write: OwnedHandle,
}

impl Pipe {
    fn inheritable() -> io::Result<Self> {
        let mut read = OwnedHandle::null();
        let mut write = OwnedHandle::null();
        let attributes = SecurityAttributes {
            length: size_of::<SecurityAttributes>() as u32,
            security_descriptor: null_mut(),
            inherit_handle: TRUE,
        };
        // SAFETY: output pointers and SECURITY_ATTRIBUTES are valid for this call.
        if unsafe { CreatePipe(&mut read.0, &mut write.0, &attributes, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // The parent read side must not be inherited or EOF would never be observable.
        // SAFETY: read is a valid pipe handle created immediately above.
        if unsafe { SetHandleInformation(read.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { read, write })
    }
}

/// TASK-802：创建 KILL_ON_JOB_CLOSE 的作业对象；句柄关闭即终止残留子进程。
fn create_kill_on_close_job() -> io::Result<OwnedHandle> {
    // SAFETY: 无名作业对象，attributes/name 均为空。
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job_handle = OwnedHandle(job);
    let mut limits = [0u8; JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_SIZE];
    limits[JOB_OBJECT_LIMIT_FLAGS_OFFSET..JOB_OBJECT_LIMIT_FLAGS_OFFSET + 4]
        .copy_from_slice(&JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.to_ne_bytes());
    // SAFETY: limits 是完整零初始化的 Win32 结构缓冲，长度为实证确认的类大小。
    if unsafe {
        SetInformationJobObject(
            job_handle.0,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
            limits.as_ptr() as *const c_void,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_SIZE as u32,
        )
    } == 0
    {
        return Err(io::Error::other(format!(
            "SetInformationJobObject failed: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(job_handle)
}

pub(crate) fn execute_with_deadline(
    command: &CommandSpec,
    deadline: Option<Duration>,
) -> io::Result<ExecutionOutput> {
    execute_inner(command, deadline)
}

pub(crate) fn execute(command: &CommandSpec) -> io::Result<ExecutionOutput> {
    execute_inner(command, None)
}

fn execute_inner(command: &CommandSpec, deadline: Option<Duration>) -> io::Result<ExecutionOutput> {
    let mut process_token = OwnedHandle::null();
    // SAFETY: GetCurrentProcess returns a pseudo-handle and token points to writable storage.
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut process_token.0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let restricted_token = create_restricted_token(process_token.0)?;

    let mut stdout_pipe = Pipe::inheritable()?;
    let mut stderr_pipe = Pipe::inheritable()?;
    let mut startup: StartupInfoW = unsafe { zeroed() };
    startup.cb = size_of::<StartupInfoW>() as u32;
    startup.flags = STARTF_USESTDHANDLES;
    startup.stdout = stdout_pipe.write.0;
    startup.stderr = stderr_pipe.write.0;

    let mut command_line = wide_null(std::ffi::OsStr::new(&build_command_line(command)));
    let current_directory = command
        .current_dir
        .as_ref()
        .map(|path| wide_null(path.as_os_str()));
    let current_directory_ptr = current_directory
        .as_ref()
        .map_or(null(), |value| value.as_ptr());
    let mut process_info: ProcessInformation = unsafe { zeroed() };

    // SAFETY: all pointers remain alive through the call; inherited handles are explicit pipes.
    if unsafe {
        CreateProcessAsUserW(
            restricted_token.0,
            null(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            TRUE,
            CREATE_SUSPENDED,
            null(),
            current_directory_ptr,
            &startup,
            &mut process_info,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let process = OwnedHandle(process_info.process);
    let thread_handle = OwnedHandle(process_info.thread);
    if let Err(error) = verify_restricted(process.0) {
        // SAFETY: process is valid and still suspended.
        unsafe { TerminateProcess(process.0, 1) };
        return Err(error);
    }

    // TASK-802：进程入作业对象（KILL_ON_JOB_CLOSE）——超时或句柄关闭都会
    // 终止整棵进程树，受限子进程无法留下孤儿。
    let job = create_kill_on_close_job()?;
    // SAFETY: job 与刚创建的 process 句柄均有效。
    if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
        let error = io::Error::other(format!(
            "AssignProcessToJobObject failed: {}",
            io::Error::last_os_error()
        ));
        unsafe { TerminateProcess(process.0, 1) };
        return Err(error);
    }

    // Drop parent copies of child write handles before readers start.
    drop(stdout_pipe.write);
    drop(stderr_pipe.write);
    let stdout_reader = reader_thread(stdout_pipe.read.take() as usize);
    let stderr_reader = reader_thread(stderr_pipe.read.take() as usize);

    // SAFETY: thread_handle identifies the suspended primary thread.
    if unsafe { ResumeThread(thread_handle.0) } == u32::MAX {
        // SAFETY: process is valid and must not remain suspended after failure.
        unsafe { TerminateProcess(process.0, 1) };
        return Err(io::Error::last_os_error());
    }
    drop(thread_handle);

    // SAFETY: process is a valid process handle.
    let wait_ms = deadline.map_or(INFINITE, |value| value.as_millis() as u32);
    let waited = unsafe { WaitForSingleObject(process.0, wait_ms) };
    if waited == WAIT_FAILED {
        // SAFETY: best-effort termination prevents reader threads hanging on inherited pipes.
        unsafe { TerminateProcess(process.0, 1) };
        return Err(io::Error::last_os_error());
    }
    if waited == WAIT_TIMEOUT {
        // TASK-802：超时终止整棵进程树；终止失败 fail-closed。
        // SAFETY: job 与 process 句柄均有效。
        if unsafe { TerminateJobObject(job.0, DEADLINE_EXIT_CODE) } == 0 {
            return Err(io::Error::other(
                "failed to terminate timed-out process tree",
            ));
        }
        if unsafe { WaitForSingleObject(process.0, INFINITE) } == WAIT_FAILED {
            return Err(io::Error::last_os_error());
        }
    }
    let mut exit_code = 0;
    // SAFETY: process has completed and exit_code is writable.
    if unsafe { GetExitCodeProcess(process.0, &mut exit_code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if waited == WAIT_TIMEOUT {
        exit_code = DEADLINE_EXIT_CODE;
    }

    Ok(ExecutionOutput {
        process_id: process_info.process_id,
        exit_code,
        stdout: join_reader(stdout_reader)?,
        stderr: join_reader(stderr_reader)?,
        restricted: true,
    })
}

fn create_restricted_token(existing_token: Handle) -> io::Result<OwnedHandle> {
    const TOKEN_USER_CLASS: i32 = 1;
    const WIN_WORLD_SID: i32 = 1;
    const WIN_AUTHENTICATED_USER_SID: i32 = 17;
    const WIN_BUILTIN_USERS_SID: i32 = 27;
    const SECURITY_MAX_SID_SIZE: u32 = 68;
    let mut required_bytes = 0;
    // SAFETY: the first query intentionally obtains the required TokenUser buffer size.
    unsafe {
        GetTokenInformation(
            existing_token,
            TOKEN_USER_CLASS,
            null_mut(),
            0,
            &mut required_bytes,
        )
    };
    if required_bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let words = (required_bytes as usize).div_ceil(size_of::<usize>());
    let mut user_buffer = vec![0usize; words];
    // SAFETY: aligned storage has the byte capacity requested by Windows.
    if unsafe {
        GetTokenInformation(
            existing_token,
            TOKEN_USER_CLASS,
            user_buffer.as_mut_ptr().cast(),
            required_bytes,
            &mut required_bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful TokenUser query initialized the aligned buffer.
    let user = unsafe { &*(user_buffer.as_ptr().cast::<TokenUser>()) };

    let mut world = [0u32; SECURITY_MAX_SID_SIZE as usize / size_of::<u32>()];
    let mut authenticated = [0u32; SECURITY_MAX_SID_SIZE as usize / size_of::<u32>()];
    let mut users = [0u32; SECURITY_MAX_SID_SIZE as usize / size_of::<u32>()];
    create_well_known_sid(WIN_WORLD_SID, &mut world)?;
    create_well_known_sid(WIN_AUTHENTICATED_USER_SID, &mut authenticated)?;
    create_well_known_sid(WIN_BUILTIN_USERS_SID, &mut users)?;
    let restricting_sids = [
        SidAndAttributes {
            sid: user.user.sid,
            attributes: 0,
        },
        SidAndAttributes {
            sid: world.as_mut_ptr().cast(),
            attributes: 0,
        },
        SidAndAttributes {
            sid: authenticated.as_mut_ptr().cast(),
            attributes: 0,
        },
        SidAndAttributes {
            sid: users.as_mut_ptr().cast(),
            attributes: 0,
        },
    ];
    let mut restricted_token = OwnedHandle::null();
    // SAFETY: restricting SIDs reference live aligned buffers; other arrays are empty.
    if unsafe {
        CreateRestrictedToken(
            existing_token,
            DISABLE_MAX_PRIVILEGE,
            0,
            null(),
            0,
            null(),
            restricting_sids.len() as u32,
            restricting_sids.as_ptr().cast(),
            &mut restricted_token.0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(restricted_token)
}

fn create_well_known_sid(sid_type: i32, buffer: &mut [u32; 17]) -> io::Result<()> {
    let mut size = (buffer.len() * size_of::<u32>()) as u32;
    // SAFETY: the aligned fixed-size SID buffer is writable and sufficiently large.
    if unsafe { CreateWellKnownSid(sid_type, null_mut(), buffer.as_mut_ptr().cast(), &mut size) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn verify_restricted(process: Handle) -> io::Result<()> {
    let mut token = OwnedHandle::null();
    // SAFETY: process is valid and token points to writable storage.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token.0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: token is a valid process token handle.
    if unsafe { IsTokenRestricted(token.0) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "child token was not restricted; refusing execution",
        ));
    }
    Ok(())
}

fn wide_null(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn timed_out_process_tree_is_terminated_with_deadline_code() {
        let command = CommandSpec::new("cmd")
            .arg("/C")
            .arg("for /L %i in (1,1,2147483647) do @rem loop");
        let started = Instant::now();
        let output = execute_with_deadline(&command, Some(Duration::from_millis(300))).unwrap();
        let elapsed = started.elapsed();
        // 终止结果结构化：约定退出码 124 + restricted 仍为真
        assert_eq!(output.exit_code, 124, "stderr: {:?}", output.stderr);
        assert!(output.restricted);
        assert!(
            elapsed < Duration::from_secs(10),
            "超时必须在 deadline 附近返回而不是等命令跑完（实际 {elapsed:?}）"
        );
    }

    #[test]
    fn fast_command_within_deadline_succeeds_normally() {
        let command = CommandSpec::new("cmd").arg("/C").arg("echo ok");
        let output = execute_with_deadline(&command, Some(Duration::from_secs(10))).unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(String::from_utf8_lossy(&output.stdout).contains("ok"));
    }
}
