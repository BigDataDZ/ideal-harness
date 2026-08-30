//! TASK-802：协作式取消令牌。
//! 超时不强杀宿主线程（明确不做）；有副作用的 handler 在提交点调用
//! [`CancellationToken::check`]，被取消后以稳定 ToolTimeout 拒绝继续，
//! 从而保证「ToolTimeout 返回后不再产生文件写入/事件追加」。

use protocol::{ErrorCode, ErrorEnvelope};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取消；重复调用幂等。
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// 提交点检查：已取消时返回稳定 ToolTimeout，调用方必须放弃副作用。
    pub fn check(&self) -> Result<(), ErrorEnvelope> {
        if self.is_cancelled() {
            return Err(ErrorEnvelope::new(
                ErrorCode::ToolTimeout,
                "cancelled via deadline token",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_is_idempotent_and_check_fails_closed() {
        let token = CancellationToken::new();
        assert!(token.check().is_ok());
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
        assert_eq!(token.check().unwrap_err().code, ErrorCode::ToolTimeout);
    }
}
