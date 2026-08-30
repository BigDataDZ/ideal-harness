//! P2/TASK-203：代理目标精确白名单；空配置即默认断网。

use std::collections::BTreeSet;

#[derive(Debug, Clone, Default)]
pub struct ProxyPolicy {
    allowed_hosts: BTreeSet<String>,
    /// TASK-801：是否放行解析到 loopback/私网的目标。仅测试环境显式开启；
    /// 生产装配永不调用（fail-closed 默认）。
    allow_forbidden_targets: bool,
}

impl ProxyPolicy {
    /// 创建默认拒绝所有目标的策略。
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// 创建只放行当前模型 provider 域名的策略。
    pub fn for_provider(host: &str) -> Result<Self, &'static str> {
        let mut policy = Self::deny_all();
        policy.allow_host(host)?;
        Ok(policy)
    }

    /// 增加一个精确主机名或 IP；不接受 URL、端口和通配符。
    pub fn allow_host(&mut self, host: &str) -> Result<(), &'static str> {
        let host = normalize_host(host)?;
        self.allowed_hosts.insert(host);
        Ok(())
    }

    pub fn allows(&self, host: &str) -> bool {
        normalize_host(host)
            .map(|host| self.allowed_hosts.contains(&host))
            .unwrap_or(false)
    }

    /// 测试逃生口：允许解析到 loopback/私网的目标（CI 里的本地源站）。
    /// 仅供测试代码调用；生产装配不得使用。
    pub fn allow_forbidden_targets(&mut self) {
        self.allow_forbidden_targets = true;
    }

    pub fn forbidden_targets_allowed(&self) -> bool {
        self.allow_forbidden_targets
    }
}

fn normalize_host(host: &str) -> Result<String, &'static str> {
    let normalized = host.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.contains(['/', '\\', '@', '*', ':'])
        || normalized.chars().any(char::is_whitespace)
    {
        return Err("allowlist entry must be an exact host without scheme, port, or wildcard");
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_denies_and_provider_policy_is_exact_case_insensitive() {
        assert!(!ProxyPolicy::deny_all().allows("api.openai.com"));
        let policy = ProxyPolicy::for_provider("API.OpenAI.COM").unwrap();
        assert!(policy.allows("api.openai.com"));
        assert!(!policy.allows("evil.api.openai.com"));
    }

    #[test]
    fn wildcard_url_and_host_port_entries_are_rejected() {
        for invalid in [
            "*.example.com",
            "https://example.com",
            "example.com:443",
            "",
        ] {
            assert!(ProxyPolicy::for_provider(invalid).is_err());
        }
    }
}
