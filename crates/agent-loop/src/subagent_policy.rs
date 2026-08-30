//! P3/TASK-409：子代理资源预算与模型/工具选择策略，所有检查 fail-closed。

use protocol::{ErrorCode, ErrorEnvelope};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentPolicy {
    max_depth: u32,
    max_concurrency: u32,
    max_turns: u32,
    max_tokens: u64,
    allowed_models: BTreeSet<String>,
    allowed_tools: BTreeSet<String>,
    denied_tools: BTreeSet<String>,
}

impl SubagentPolicy {
    pub fn new(
        max_depth: u32,
        max_concurrency: u32,
        max_turns: u32,
        max_tokens: u64,
        allowed_models: impl IntoIterator<Item = String>,
        allowed_tools: impl IntoIterator<Item = String>,
        denied_tools: impl IntoIterator<Item = String>,
    ) -> Result<Self, ErrorEnvelope> {
        if max_depth == 0 || max_concurrency == 0 || max_turns == 0 || max_tokens == 0 {
            return Err(args_error(
                "subagent policy limits must be greater than zero",
            ));
        }
        let policy = Self {
            max_depth,
            max_concurrency,
            max_turns,
            max_tokens,
            allowed_models: collect_names(allowed_models, "model")?,
            allowed_tools: collect_names(allowed_tools, "tool")?,
            denied_tools: collect_names(denied_tools, "denied tool")?,
        };
        Ok(policy)
    }

    pub fn local_default() -> Self {
        Self {
            max_depth: 1,
            max_concurrency: 1,
            max_turns: 8,
            max_tokens: 8_192,
            allowed_models: BTreeSet::new(),
            allowed_tools: BTreeSet::new(),
            denied_tools: BTreeSet::new(),
        }
    }

    pub fn max_depth(&self) -> u32 {
        self.max_depth
    }

    pub fn max_concurrency(&self) -> u32 {
        self.max_concurrency
    }

    pub fn max_turns(&self) -> u32 {
        self.max_turns
    }

    pub fn max_tokens(&self) -> u64 {
        self.max_tokens
    }

    pub fn allowed_models(&self) -> &BTreeSet<String> {
        &self.allowed_models
    }

    pub fn allowed_tools(&self) -> &BTreeSet<String> {
        &self.allowed_tools
    }

    pub fn denied_tools(&self) -> &BTreeSet<String> {
        &self.denied_tools
    }

    fn is_within(&self, parent: &Self) -> bool {
        self.max_depth <= parent.max_depth
            && self.max_concurrency <= parent.max_concurrency
            && self.max_turns <= parent.max_turns
            && self.max_tokens <= parent.max_tokens
            && self.allowed_models.is_subset(&parent.allowed_models)
            && self.allowed_tools.is_subset(&parent.allowed_tools)
            && parent.denied_tools.is_subset(&self.denied_tools)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRequest {
    depth: u32,
    active_children: u32,
    turn_budget: u32,
    token_budget: u64,
    model: Option<String>,
    tools: BTreeSet<String>,
}

impl SubagentRequest {
    pub fn new(
        depth: u32,
        active_children: u32,
        turn_budget: u32,
        token_budget: u64,
        model: Option<String>,
        tools: impl IntoIterator<Item = String>,
    ) -> Result<Self, ErrorEnvelope> {
        if depth == 0 || turn_budget == 0 || token_budget == 0 {
            return Err(args_error(
                "subagent request budgets must be greater than zero",
            ));
        }
        if model.as_ref().is_some_and(|name| name.trim().is_empty()) {
            return Err(args_error("subagent model must not be blank"));
        }
        Ok(Self {
            depth,
            active_children,
            turn_budget,
            token_budget,
            model,
            tools: collect_names(tools, "tool")?,
        })
    }

    pub fn local_default() -> Self {
        Self {
            depth: 1,
            active_children: 0,
            turn_budget: 8,
            token_budget: 8_192,
            model: None,
            tools: BTreeSet::new(),
        }
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn active_children(&self) -> u32 {
        self.active_children
    }

    pub fn turn_budget(&self) -> u32 {
        self.turn_budget
    }

    pub fn token_budget(&self) -> u64 {
        self.token_budget
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn tools(&self) -> &BTreeSet<String> {
        &self.tools
    }
}

pub(crate) fn validate_delegation(
    parent: &SubagentPolicy,
    child: &SubagentPolicy,
    request: &SubagentRequest,
) -> Result<(), ErrorEnvelope> {
    validate_child_policy(parent, child)?;
    if request.depth > child.max_depth {
        return Err(policy_error("subagent depth limit exceeded"));
    }
    if request.active_children >= child.max_concurrency {
        return Err(policy_error("subagent concurrency limit exceeded"));
    }
    if request.turn_budget > child.max_turns {
        return Err(ErrorEnvelope::new(
            ErrorCode::ContextWindowExceeded,
            "subagent turn budget exceeds policy",
        ));
    }
    if request.token_budget > child.max_tokens {
        return Err(ErrorEnvelope::new(
            ErrorCode::ContextWindowExceeded,
            "subagent token budget exceeds policy",
        ));
    }
    if let Some(model) = request.model() {
        if !child.allowed_models.contains(model) {
            return Err(policy_error("subagent model is not allowed"));
        }
    }
    if !request.tools.is_subset(&child.allowed_tools)
        || !request.tools.is_disjoint(&child.denied_tools)
    {
        return Err(policy_error("subagent tool selection is not allowed"));
    }
    Ok(())
}

pub(crate) fn validate_child_policy(
    parent: &SubagentPolicy,
    child: &SubagentPolicy,
) -> Result<(), ErrorEnvelope> {
    if child.is_within(parent) {
        Ok(())
    } else {
        Err(policy_error(
            "child subagent policy expands its parent policy",
        ))
    }
}

fn collect_names(
    names: impl IntoIterator<Item = String>,
    kind: &str,
) -> Result<BTreeSet<String>, ErrorEnvelope> {
    let mut collected = BTreeSet::new();
    for name in names {
        if name.trim().is_empty() || !collected.insert(name) {
            return Err(args_error(format!(
                "subagent {kind} names must be non-empty and unique"
            )));
        }
    }
    Ok(collected)
}

fn args_error(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::ToolArgsInvalid, message)
}

fn policy_error(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::SandboxDenied, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SubagentPolicy {
        SubagentPolicy::new(
            2,
            2,
            5,
            1_000,
            ["model-a".into()],
            ["read".into(), "exec".into()],
            ["exec".into()],
        )
        .unwrap()
    }

    fn request() -> SubagentRequest {
        SubagentRequest::new(1, 0, 3, 500, Some("model-a".into()), ["read".into()]).unwrap()
    }

    #[test]
    fn valid_narrowed_policy_and_request_are_accepted() {
        let parent = policy();
        let child = SubagentPolicy::new(
            1,
            1,
            4,
            800,
            ["model-a".into()],
            ["read".into()],
            ["exec".into()],
        )
        .unwrap();
        assert_eq!(validate_delegation(&parent, &child, &request()), Ok(()));
    }

    #[test]
    fn resource_and_selection_limits_have_stable_codes() {
        let policy = policy();
        let cases = [
            (
                3,
                0,
                3,
                500,
                Some("model-a"),
                vec!["read"],
                ErrorCode::SandboxDenied,
            ),
            (
                1,
                2,
                3,
                500,
                Some("model-a"),
                vec!["read"],
                ErrorCode::SandboxDenied,
            ),
            (
                1,
                0,
                6,
                500,
                Some("model-a"),
                vec!["read"],
                ErrorCode::ContextWindowExceeded,
            ),
            (
                1,
                0,
                3,
                1_001,
                Some("model-a"),
                vec!["read"],
                ErrorCode::ContextWindowExceeded,
            ),
            (
                1,
                0,
                3,
                500,
                Some("model-b"),
                vec!["read"],
                ErrorCode::SandboxDenied,
            ),
            (
                1,
                0,
                3,
                500,
                Some("model-a"),
                vec!["exec"],
                ErrorCode::SandboxDenied,
            ),
        ];
        for (depth, active, turns, tokens, model, tools, code) in cases {
            let request = SubagentRequest::new(
                depth,
                active,
                turns,
                tokens,
                model.map(str::to_string),
                tools.into_iter().map(str::to_string),
            )
            .unwrap();
            assert_eq!(
                validate_delegation(&policy, &policy, &request)
                    .unwrap_err()
                    .code,
                code
            );
        }
    }

    #[test]
    fn child_policy_cannot_expand_parent() {
        let parent = policy();
        let expanded = SubagentPolicy::new(
            3,
            2,
            5,
            1_000,
            ["model-a".into()],
            ["read".into(), "exec".into()],
            ["exec".into()],
        )
        .unwrap();
        assert_eq!(
            validate_delegation(&parent, &expanded, &request())
                .unwrap_err()
                .code,
            ErrorCode::SandboxDenied
        );
    }
}
