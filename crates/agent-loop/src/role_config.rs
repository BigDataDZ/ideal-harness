//! P3/TASK-411：严格、确定性的 Agent Role JSON 配置；未知输入一律拒绝。

use crate::subagent::SubagentTask;
use crate::subagent_policy::{validate_delegation, SubagentPolicy, SubagentRequest};
use protocol::{ErrorCode, ErrorEnvelope};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

const MAX_TEXT_LEN: usize = 4_096;
const ROLE_FIELDS: [&str; 6] = [
    "nickname",
    "description",
    "instructions",
    "model",
    "allowed_tools",
    "denied_tools",
];

const BUILTIN_ROLES_JSON: &str = r#"[
  {
    "nickname": "researcher",
    "description": "Read-only repository researcher",
    "instructions": "Inspect evidence and return a concise, source-grounded report.",
    "model": "deepseek-chat",
    "allowed_tools": ["read"],
    "denied_tools": []
  }
]"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRole {
    nickname: String,
    description: String,
    instructions: String,
    model: String,
    allowed_tools: BTreeSet<String>,
    denied_tools: BTreeSet<String>,
}

impl AgentRole {
    pub fn nickname(&self) -> &str {
        &self.nickname
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn allowed_tools(&self) -> &BTreeSet<String> {
        &self.allowed_tools
    }

    pub fn denied_tools(&self) -> &BTreeSet<String> {
        &self.denied_tools
    }

    /// 把角色约束确定性收窄成一份可执行子任务配置。
    pub fn build_subtask(
        &self,
        identity: &RoleTaskIdentity,
        budget: &RoleTaskBudget,
        parent_policy: &SubagentPolicy,
    ) -> Result<RoleSubtask, ErrorEnvelope> {
        let denied_tools: BTreeSet<_> = parent_policy
            .denied_tools()
            .union(&self.denied_tools)
            .cloned()
            .collect();
        let child_policy = SubagentPolicy::new(
            budget.depth,
            parent_policy.max_concurrency(),
            budget.turns,
            budget.tokens,
            [self.model.clone()],
            self.allowed_tools.iter().cloned(),
            denied_tools,
        )?;
        let selected_tools = self
            .allowed_tools
            .difference(child_policy.denied_tools())
            .cloned();
        let request = SubagentRequest::new(
            budget.depth,
            budget.active_children,
            budget.turns,
            budget.tokens,
            Some(self.model.clone()),
            selected_tools,
        )?;
        validate_delegation(parent_policy, &child_policy, &request)?;
        let prompt = format!("{}\n\n{}", self.instructions, identity.prompt);
        let task = SubagentTask::with_lineage(
            identity.task_id.clone(),
            prompt,
            identity.parent_id.clone(),
            identity.child_id.clone(),
        )?;
        Ok(RoleSubtask {
            role_nickname: self.nickname.clone(),
            task,
            request,
            child_policy,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleTaskIdentity {
    task_id: String,
    prompt: String,
    parent_id: String,
    child_id: String,
}

impl RoleTaskIdentity {
    pub fn new(
        task_id: impl Into<String>,
        prompt: impl Into<String>,
        parent_id: impl Into<String>,
        child_id: impl Into<String>,
    ) -> Result<Self, ErrorEnvelope> {
        let identity = Self {
            task_id: task_id.into(),
            prompt: prompt.into(),
            parent_id: parent_id.into(),
            child_id: child_id.into(),
        };
        for (name, value) in [
            ("task_id", identity.task_id.as_str()),
            ("prompt", identity.prompt.as_str()),
            ("parent_id", identity.parent_id.as_str()),
            ("child_id", identity.child_id.as_str()),
        ] {
            validate_text(name, value, false)?;
        }
        Ok(identity)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleTaskBudget {
    depth: u32,
    active_children: u32,
    turns: u32,
    tokens: u64,
}

impl RoleTaskBudget {
    pub fn new(
        depth: u32,
        active_children: u32,
        turns: u32,
        tokens: u64,
    ) -> Result<Self, ErrorEnvelope> {
        if depth == 0 || turns == 0 || tokens == 0 {
            return Err(args_error("role task budgets must be greater than zero"));
        }
        Ok(Self {
            depth,
            active_children,
            turns,
            tokens,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSubtask {
    role_nickname: String,
    task: SubagentTask,
    request: SubagentRequest,
    child_policy: SubagentPolicy,
}

impl RoleSubtask {
    pub fn role_nickname(&self) -> &str {
        &self.role_nickname
    }

    pub fn task(&self) -> &SubagentTask {
        &self.task
    }

    pub fn request(&self) -> &SubagentRequest {
        &self.request
    }

    pub fn child_policy(&self) -> &SubagentPolicy {
        &self.child_policy
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCatalog {
    roles: BTreeMap<String, AgentRole>,
}

impl RoleCatalog {
    pub fn with_builtins(user_json: Option<&str>) -> Result<Self, ErrorEnvelope> {
        let mut roles = parse_roles(BUILTIN_ROLES_JSON)?;
        if let Some(json) = user_json {
            roles.extend(parse_roles(json)?);
        }
        Self::from_roles(roles)
    }

    pub fn from_roles(roles: impl IntoIterator<Item = AgentRole>) -> Result<Self, ErrorEnvelope> {
        let mut catalog = BTreeMap::new();
        for role in roles {
            let nickname = role.nickname.clone();
            if catalog.insert(nickname.clone(), role).is_some() {
                return Err(args_error(format!("duplicate role nickname: {nickname}")));
            }
        }
        Ok(Self { roles: catalog })
    }

    pub fn get(&self, nickname: &str) -> Result<&AgentRole, ErrorEnvelope> {
        self.roles
            .get(nickname)
            .ok_or_else(|| args_error(format!("unknown role nickname: {nickname}")))
    }

    pub fn nicknames(&self) -> impl Iterator<Item = &str> {
        self.roles.keys().map(String::as_str)
    }
}

/// 解析严格 JSON 数组；不接受注释、别名、默认字段或未知字段。
pub fn parse_roles(json: &str) -> Result<Vec<AgentRole>, ErrorEnvelope> {
    let value: Value = serde_json::from_str(json)
        .map_err(|error| args_error(format!("invalid role JSON: {error}")))?;
    let entries = value
        .as_array()
        .ok_or_else(|| args_error("role document must be a JSON array"))?;
    entries.iter().map(parse_role).collect()
}

fn parse_role(value: &Value) -> Result<AgentRole, ErrorEnvelope> {
    let object = value
        .as_object()
        .ok_or_else(|| args_error("each role must be a JSON object"))?;
    reject_unknown_fields(object)?;
    let nickname = required_string(object, "nickname", true)?;
    let description = required_string(object, "description", false)?;
    let instructions = required_string(object, "instructions", false)?;
    let model = required_string(object, "model", true)?;
    let allowed_tools = required_names(object, "allowed_tools")?;
    let denied_tools = required_names(object, "denied_tools")?;
    Ok(AgentRole {
        nickname,
        description,
        instructions,
        model,
        allowed_tools,
        denied_tools,
    })
}

fn reject_unknown_fields(object: &Map<String, Value>) -> Result<(), ErrorEnvelope> {
    for field in object.keys() {
        if !ROLE_FIELDS.contains(&field.as_str()) {
            return Err(args_error(format!("unknown role field: {field}")));
        }
    }
    if object.len() != ROLE_FIELDS.len() {
        return Err(args_error("role is missing one or more required fields"));
    }
    Ok(())
}

fn required_string(
    object: &Map<String, Value>,
    field: &str,
    identifier: bool,
) -> Result<String, ErrorEnvelope> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| args_error(format!("role field {field} must be a string")))?;
    validate_text(field, value, identifier)?;
    Ok(value.to_string())
}

fn required_names(
    object: &Map<String, Value>,
    field: &str,
) -> Result<BTreeSet<String>, ErrorEnvelope> {
    let values = object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| args_error(format!("role field {field} must be an array")))?;
    let mut names = BTreeSet::new();
    for value in values {
        let name = value
            .as_str()
            .ok_or_else(|| args_error(format!("role field {field} must contain strings")))?;
        validate_text(field, name, true)?;
        if !names.insert(name.to_string()) {
            return Err(args_error(format!(
                "duplicate name in role field {field}: {name}"
            )));
        }
    }
    Ok(names)
}

fn validate_text(field: &str, value: &str, identifier: bool) -> Result<(), ErrorEnvelope> {
    if value.trim().is_empty() || value.len() > MAX_TEXT_LEN || value.contains('\0') {
        return Err(args_error(format!(
            "role field {field} is blank or invalid"
        )));
    }
    if identifier
        && !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-/".contains(character))
    {
        return Err(args_error(format!(
            "role field {field} is not a safe identifier"
        )));
    }
    Ok(())
}

fn args_error(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::ToolArgsInvalid, message)
}

#[cfg(test)]
#[path = "role_config_tests.rs"]
mod tests;
