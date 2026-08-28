//! TASK-204 验收：终端 y/n 审批与所有 I/O 失败路径均 fail-closed。

use approval::{approve_escalation, Approver, Decision, EscalationRequest, TerminalApprover};
use protocol::ErrorCode;
use sandbox_policy::SandboxMode::{DangerFullAccess, ReadOnly, WorkspaceWrite};
use std::{
    io::{self, BufRead, Cursor, Read, Write},
    sync::{Arc, Mutex},
};

fn request() -> EscalationRequest {
    EscalationRequest {
        requested_mode: WorkspaceWrite,
        justification: "need to update workspace files".into(),
    }
}

#[test]
fn explicit_y_or_yes_approves_case_insensitively() {
    for answer in ["y\n", "YES\r\n", " Yes \n"] {
        let approver = TerminalApprover::new(Cursor::new(answer), Vec::new());
        assert_eq!(approver.decide(&request()), Decision::Approved);
    }
}

#[test]
fn negative_blank_and_unknown_answers_reject() {
    for answer in ["n\n", "\n", "approve\n", "y please\n"] {
        let approver = TerminalApprover::new(Cursor::new(answer), Vec::new());
        assert_eq!(approver.decide(&request()), Decision::Rejected);
    }
}

#[test]
fn eof_rejects_and_approve_escalation_returns_stable_code() {
    let approver = TerminalApprover::new(Cursor::new(Vec::<u8>::new()), Vec::new());
    let error = approve_escalation(ReadOnly, request(), Some(&approver)).unwrap_err();
    assert_eq!(error.code, ErrorCode::ApprovalRejected);
}

#[test]
fn prompt_explains_requested_mode_and_justification() {
    let output = SharedWriter::default();
    let captured = output.bytes.clone();
    let approver = TerminalApprover::new(Cursor::new("n\n"), output);

    assert_eq!(approver.decide(&request()), Decision::Rejected);
    let prompt = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(prompt.contains("workspace-write"));
    assert!(prompt.contains("need to update workspace files"));
    assert!(prompt.contains("[y/N]"));
}

#[test]
fn read_failure_rejects() {
    let approver = TerminalApprover::new(FailingReader, Vec::new());
    assert_eq!(approver.decide(&request()), Decision::Rejected);
}

#[test]
fn write_failure_rejects_without_reading() {
    let reader = CountingReader::default();
    let reads = Arc::clone(&reader.reads);
    let approver = TerminalApprover::new(reader, FailingWriter);

    assert_eq!(approver.decide(&request()), Decision::Rejected);
    assert_eq!(*reads.lock().unwrap(), 0);
}

#[test]
fn flush_failure_rejects_without_reading() {
    let reader = CountingReader::default();
    let reads = Arc::clone(&reader.reads);
    let approver = TerminalApprover::new(reader, FlushFailingWriter);

    assert_eq!(approver.decide(&request()), Decision::Rejected);
    assert_eq!(*reads.lock().unwrap(), 0);
}

#[test]
fn terminal_approval_integrates_with_widening_contract() {
    let approver = TerminalApprover::new(Cursor::new("yes\n"), Vec::new());
    let widened = approve_escalation(ReadOnly, request(), Some(&approver)).unwrap();
    assert_eq!(widened, WorkspaceWrite);

    let narrowing = EscalationRequest {
        requested_mode: ReadOnly,
        justification: "reduce access".into(),
    };
    let error = approve_escalation(DangerFullAccess, narrowing, Some(&approver)).unwrap_err();
    assert_eq!(error.code, ErrorCode::SandboxDenied);
}

#[derive(Default)]
struct SharedWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for SharedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("injected read failure"))
    }
}

impl BufRead for FailingReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Err(io::Error::other("injected read failure"))
    }

    fn consume(&mut self, _: usize) {}
}

#[derive(Default)]
struct CountingReader {
    reads: Arc<Mutex<usize>>,
}

impl Read for CountingReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        *self.reads.lock().unwrap() += 1;
        Ok(0)
    }
}

impl BufRead for CountingReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        *self.reads.lock().unwrap() += 1;
        Ok(&[])
    }

    fn consume(&mut self, _: usize) {}
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("injected write failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("injected flush failure"))
    }
}

struct FlushFailingWriter;

impl Write for FlushFailingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("injected flush failure"))
    }
}
