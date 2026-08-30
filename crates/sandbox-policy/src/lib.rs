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

    /// 稳定权限配置摘要；不依赖进程随机种子，跨重放结果一致。
    pub fn profile_hash(&self) -> String {
        let mode = match self.mode {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        };
        let canonical = format!("v1\0{mode}\0{}", self.workspace_root.to_string_lossy());
        format!("fnv1a64:{:016x}", fnv1a64(canonical.as_bytes()))
    }
}

/// 权限配置状态；任何语义变化都推进 epoch，幂等刷新不推进。
#[derive(Debug, Clone)]
pub struct PermissionProfileState {
    policy: SandboxPolicy,
    epoch: u64,
}

impl PermissionProfileState {
    pub fn new(policy: SandboxPolicy) -> Self {
        Self { policy, epoch: 0 }
    }

    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn replace(&mut self, policy: SandboxPolicy) -> Result<bool, &'static str> {
        if self.policy.mode == policy.mode && self.policy.workspace_root == policy.workspace_root {
            return Ok(false);
        }
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or("permission epoch overflow")?;
        self.policy = policy;
        Ok(true)
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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

    #[test]
    fn profile_hash_and_epoch_change_only_with_permission_semantics() {
        let original = policy(SandboxMode::ReadOnly);
        let original_hash = original.profile_hash();
        let mut state = PermissionProfileState::new(original.clone());
        assert!(!state.replace(original).unwrap());
        assert_eq!(state.epoch(), 0);

        state
            .replace(SandboxPolicy {
                mode: SandboxMode::ReadOnly,
                workspace_root: PathBuf::from("/other"),
            })
            .unwrap();
        assert_eq!(state.epoch(), 1);
        assert_ne!(state.policy().profile_hash(), original_hash);
    }
}
