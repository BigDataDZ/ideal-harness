//! P4：token 计量与压缩触发（骨架）。
//! 双触发：① 压力阈值主动压缩 ② 溢出错误强制压缩后重试。

use protocol::ErrorCode;

/// token 用量。生产版以 provider 真实 usage 为锚，启发式仅兜底。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenUsage {
    pub total: u64,
}

/// 上下文预算。
#[derive(Debug, Clone)]
pub struct ContextBudget {
    pub context_window: u64,
    pub threshold_ratio: f64,
}

impl ContextBudget {
    /// 触发①：压力阈值 = context_window × threshold_ratio。
    pub fn pressure_threshold(&self) -> u64 {
        (self.context_window as f64 * self.threshold_ratio) as u64
    }

    pub fn needs_compaction(&self, usage: TokenUsage) -> bool {
        usage.total >= self.pressure_threshold()
    }
}

/// 触发②：模型侧报窗口超限 => 强制压缩并重试（失败不是终点而是恢复入口）。
pub fn is_context_overflow(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::ContextWindowExceeded)
}

/// 裁剪铁律检查（P4）：tool_call 与 tool_result 必须同生同死。
/// 骨架版给出判定函数，供未来 selectCompactableRange 类逻辑复用。
pub fn is_tool_pair_boundary(call_seq: u64, result_seq: u64, cut: u64) -> bool {
    // 裁剪点不得落在 call 与其 result 之间
    !(call_seq <= cut && cut < result_seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_triggers_at_threshold() {
        let b = ContextBudget {
            context_window: 1000,
            threshold_ratio: 0.8,
        };
        assert!(!b.needs_compaction(TokenUsage { total: 799 }));
        assert!(b.needs_compaction(TokenUsage { total: 800 }));
    }

    #[test]
    fn overflow_error_code_maps_to_forced_compaction() {
        assert!(is_context_overflow(ErrorCode::ContextWindowExceeded));
        assert!(!is_context_overflow(ErrorCode::Internal));
    }

    #[test]
    fn cut_between_tool_call_and_result_is_forbidden() {
        // call@3 result@7：cut=5 破坏配对；cut=8 安全
        assert!(!is_tool_pair_boundary(3, 7, 5));
        assert!(is_tool_pair_boundary(3, 7, 8));
        assert!(is_tool_pair_boundary(3, 7, 2));
    }
}
