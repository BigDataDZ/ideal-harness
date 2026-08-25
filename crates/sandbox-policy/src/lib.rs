//! P2：一个 SandboxMode 枚举贯穿所有层——
//! 词法栅栏、OS 级沙箱、审批流都以它为唯一语义锚点。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 三档沙箱模式。声明顺序即宽严顺序：
/// ReadOnly < WorkspaceWrite < DangerFullAccess（提权只许加宽）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// 沙箱策略：模式 + 可写根。
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub workspace_root: PathBuf,
}

impl SandboxPolicy {
    /// 词法栅栏（P2 第一层）。
    /// 生产版必须在词法判断之上叠加 dev/ino 身份校验以抗符号链接别名。
    pub fn ensures_writable(&self, path: &Path) -> bool {
        match self.mode {
            // 只读模式：一切写路径都拒绝（fail-closed）
            SandboxMode::ReadOnly => false,
            SandboxMode::DangerFullAccess => true,
            SandboxMode::WorkspaceWrite => path.starts_with(&self.workspace_root),
        }
    }

    /// 提权加宽表：to > from 才合法，任何方向的收窄都拒绝。
    pub fn can_widen(from: SandboxMode, to: SandboxMode) -> bool {
        to > from
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: SandboxMode) -> SandboxPolicy {
        SandboxPolicy {
            mode,
            workspace_root: PathBuf::from("/ws"),
        }
    }

    #[test]
    fn read_only_rejects_everything_inside_or_outside() {
        let p = policy(SandboxMode::ReadOnly);
        assert!(!p.ensures_writable(Path::new("/ws/a.txt")));
        assert!(!p.ensures_writable(Path::new("/etc/passwd")));
    }

    #[test]
    fn workspace_write_allows_only_under_root() {
        let p = policy(SandboxMode::WorkspaceWrite);
        assert!(p.ensures_writable(Path::new("/ws/sub/a.txt")));
        assert!(!p.ensures_writable(Path::new("/tmp/../etc/passwd")));
    }

    #[test]
    fn danger_full_access_is_explicit_and_total() {
        let p = policy(SandboxMode::DangerFullAccess);
        assert!(p.ensures_writable(Path::new("/anything")));
    }

    #[test]
    fn widening_only_grows_never_narrows() {
        use SandboxMode::*;
        assert!(SandboxPolicy::can_widen(ReadOnly, WorkspaceWrite));
        assert!(SandboxPolicy::can_widen(WorkspaceWrite, DangerFullAccess));
        assert!(!SandboxPolicy::can_widen(WorkspaceWrite, ReadOnly));
        assert!(!SandboxPolicy::can_widen(DangerFullAccess, ReadOnly));
        assert!(!SandboxPolicy::can_widen(ReadOnly, ReadOnly));
    }

    #[test]
    fn mode_strings_match_dsh_contract() {
        // 与业界契约字符串对齐，便于互操作
        assert_eq!(
            serde_json::to_value(SandboxMode::WorkspaceWrite).unwrap(),
            "workspace-write"
        );
    }
}
