//! P2/TASK-204：终端 y/n 人工审批通道。

use crate::{Approver, Decision, EscalationRequest};
use sandbox_policy::SandboxMode;
use std::{
    io::{BufRead, Write},
    sync::Mutex,
};

/// 从终端输入读取人工审批结果。
///
/// 只有明确输入 `y` 或 `yes` 才批准。EOF、I/O 错误、锁异常和其他所有输入
/// 均拒绝，确保审批通道 fail-closed。
pub struct TerminalApprover<R, W> {
    io: Mutex<TerminalIo<R, W>>,
}

struct TerminalIo<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> TerminalApprover<R, W> {
    /// 使用可注入的输入输出创建终端审批器。
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            io: Mutex::new(TerminalIo { reader, writer }),
        }
    }
}

impl<R: BufRead, W: Write> Approver for TerminalApprover<R, W> {
    fn decide(&self, request: &EscalationRequest) -> Decision {
        let mut io = match self.io.lock() {
            Ok(io) => io,
            Err(_) => return Decision::Rejected,
        };

        let prompt = format!(
            "Approval requested\n  sandbox: {}\n  reason: {:?}\nApprove? [y/N]: ",
            mode_label(request.requested_mode),
            request.justification
        );
        if io.writer.write_all(prompt.as_bytes()).is_err() || io.writer.flush().is_err() {
            return Decision::Rejected;
        }

        let mut answer = String::new();
        match io.reader.read_line(&mut answer) {
            Ok(0) | Err(_) => Decision::Rejected,
            Ok(_) if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") => {
                Decision::Approved
            }
            Ok(_) => Decision::Rejected,
        }
    }
}

fn mode_label(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::ReadOnly => "read-only",
        SandboxMode::WorkspaceWrite => "workspace-write",
        SandboxMode::DangerFullAccess => "danger-full-access",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn poisoned_terminal_lock_rejects() {
        let approver = TerminalApprover::new(Cursor::new("yes\n"), Vec::new());
        std::thread::scope(|scope| {
            let result = scope
                .spawn(|| {
                    let _guard = approver.io.lock().unwrap();
                    panic!("poison terminal lock");
                })
                .join();
            assert!(result.is_err());
        });

        let request = EscalationRequest {
            requested_mode: SandboxMode::WorkspaceWrite,
            justification: "need write access".into(),
        };
        assert_eq!(approver.decide(&request), Decision::Rejected);
    }
}
