//! TASK-604：受监管 MCP registry；服务状态、代际与失败彼此隔离。

use crate::{McpCallResult, McpClient, McpServerConfig, McpTool};
use protocol::{ErrorCode, ErrorEnvelope};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

const MAX_SAFE_RESULT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServiceRequirement {
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServiceStatus {
    Ready,
    Degraded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFailureStage {
    Configuration,
    Discovery,
    Call,
    ResultSafety,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServiceFailure {
    pub source: String,
    pub stage: McpFailureStage,
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServiceSnapshot {
    pub source: String,
    pub requirement: McpServiceRequirement,
    pub status: McpServiceStatus,
    pub generation: u64,
    pub failure: Option<McpServiceFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRegistration {
    pub config: McpServerConfig,
    pub requirement: McpServiceRequirement,
    pub discovery_grace: Duration,
}

impl McpRegistration {
    pub fn validate(&self) -> Result<(), ErrorEnvelope> {
        self.config.validate()?;
        if self.discovery_grace.is_zero() {
            return Err(ErrorEnvelope::new(
                ErrorCode::ToolArgsInvalid,
                "MCP discovery grace must be greater than zero",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolHandle {
    pub source: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_limit_bytes: usize,
    pub generation: u64,
}

struct ManagedService {
    registration: McpRegistration,
    status: McpServiceStatus,
    generation: u64,
    failure: Option<McpServiceFailure>,
    client: Option<McpClient>,
}

#[derive(Default)]
pub struct McpRegistry {
    services: BTreeMap<String, ManagedService>,
    catalog: BTreeMap<(String, String), McpToolHandle>,
}

impl McpRegistry {
    pub fn start(registrations: Vec<McpRegistration>) -> Result<Self, ErrorEnvelope> {
        let mut registry = Self::default();
        for registration in registrations {
            let source = registration.config.source.clone();
            if registry.services.contains_key(&source) {
                return Err(ErrorEnvelope::new(
                    ErrorCode::ToolArgsInvalid,
                    format!("duplicate MCP source: {source}"),
                ));
            }
            if let Err(error) = registration.validate() {
                if registration.requirement == McpServiceRequirement::Required {
                    return Err(error);
                }
                registry.insert_failed(registration, 1, McpFailureStage::Configuration, error);
                continue;
            }
            match McpClient::connect_with_timeout(
                registration.config.clone(),
                registration.discovery_grace,
            ) {
                Ok(client) => registry.insert_ready(registration, 1, client),
                Err(error) if registration.requirement == McpServiceRequirement::Optional => {
                    registry.insert_failed(registration, 1, McpFailureStage::Discovery, error);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(registry)
    }

    pub fn tools(&self) -> impl Iterator<Item = &McpToolHandle> {
        self.catalog.values()
    }

    pub fn tool(&self, source: &str, name: &str) -> Option<&McpToolHandle> {
        self.catalog.get(&(source.to_string(), name.to_string()))
    }

    pub fn services(&self) -> impl Iterator<Item = McpServiceSnapshot> + '_ {
        self.services
            .iter()
            .map(|(source, service)| McpServiceSnapshot {
                source: source.clone(),
                requirement: service.registration.requirement,
                status: service.status,
                generation: service.generation,
                failure: service.failure.clone(),
            })
    }

    /// Reconnect one service. Every attempt advances generation and invalidates old handles.
    pub fn refresh(&mut self, source: &str) -> Result<bool, ErrorEnvelope> {
        let (registration, generation) = {
            let service = self.services.get(source).ok_or_else(|| {
                ErrorEnvelope::new(
                    ErrorCode::ToolArgsInvalid,
                    format!("unknown MCP source: {source}"),
                )
            })?;
            let generation = service.generation.checked_add(1).ok_or_else(|| {
                ErrorEnvelope::new(ErrorCode::Internal, "MCP generation overflow")
            })?;
            (service.registration.clone(), generation)
        };
        self.remove_catalog_source(source);
        if let Some(service) = self.services.get_mut(source) {
            service.client = None;
            service.generation = generation;
        }
        match McpClient::connect_with_timeout(
            registration.config.clone(),
            registration.discovery_grace,
        ) {
            Ok(client) => {
                self.replace_ready(registration, generation, client);
                Ok(true)
            }
            Err(error) => {
                let required = registration.requirement == McpServiceRequirement::Required;
                self.replace_failed(
                    registration,
                    generation,
                    McpFailureStage::Discovery,
                    error.clone(),
                );
                if required {
                    Err(error)
                } else {
                    Ok(false)
                }
            }
        }
    }

    pub fn call(
        &mut self,
        handle: &McpToolHandle,
        arguments: &Value,
    ) -> Result<McpCallResult, ErrorEnvelope> {
        let key = (handle.source.clone(), handle.name.clone());
        let current = self.catalog.get(&key).ok_or_else(|| stale_handle(handle))?;
        if current.generation != handle.generation || current != handle {
            return Err(stale_handle(handle));
        }
        let result = {
            let service = self
                .services
                .get_mut(&handle.source)
                .ok_or_else(|| stale_handle(handle))?;
            if service.status != McpServiceStatus::Ready || service.generation != handle.generation
            {
                return Err(stale_handle(handle));
            }
            service
                .client
                .as_mut()
                .ok_or_else(|| stale_handle(handle))?
                .call(&handle.name, arguments)
        };
        match result {
            Ok(result) => {
                if let Err(error) = safe_result(handle, &result) {
                    self.degrade_after_call(handle, McpFailureStage::ResultSafety, &error);
                    return Err(error);
                }
                Ok(result)
            }
            Err(error) if error.code == ErrorCode::ToolArgsInvalid => Err(error),
            Err(error) => {
                self.degrade_after_call(handle, McpFailureStage::Call, &error);
                Err(error)
            }
        }
    }

    fn insert_ready(&mut self, registration: McpRegistration, generation: u64, client: McpClient) {
        let source = registration.config.source.clone();
        self.add_catalog(&client, generation);
        self.services.insert(
            source,
            ManagedService {
                registration,
                status: McpServiceStatus::Ready,
                generation,
                failure: None,
                client: Some(client),
            },
        );
    }

    fn insert_failed(
        &mut self,
        registration: McpRegistration,
        generation: u64,
        stage: McpFailureStage,
        error: ErrorEnvelope,
    ) {
        let source = registration.config.source.clone();
        let status = failure_status(registration.requirement);
        let failure = service_failure(&source, stage, &error);
        self.services.insert(
            source,
            ManagedService {
                registration,
                status,
                generation,
                failure: Some(failure),
                client: None,
            },
        );
    }

    fn replace_ready(&mut self, registration: McpRegistration, generation: u64, client: McpClient) {
        self.add_catalog(&client, generation);
        if let Some(service) = self.services.get_mut(&registration.config.source) {
            service.registration = registration;
            service.status = McpServiceStatus::Ready;
            service.generation = generation;
            service.failure = None;
            service.client = Some(client);
        }
    }

    fn replace_failed(
        &mut self,
        registration: McpRegistration,
        generation: u64,
        stage: McpFailureStage,
        error: ErrorEnvelope,
    ) {
        let source = registration.config.source.clone();
        if let Some(service) = self.services.get_mut(&source) {
            service.registration = registration;
            service.status = failure_status(service.registration.requirement);
            service.generation = generation;
            service.failure = Some(service_failure(&source, stage, &error));
            service.client = None;
        }
    }

    fn add_catalog(&mut self, client: &McpClient, generation: u64) {
        for tool in client.tools() {
            let handle = tool_handle(tool, generation);
            self.catalog
                .insert((handle.source.clone(), handle.name.clone()), handle);
        }
    }

    fn remove_catalog_source(&mut self, source: &str) {
        self.catalog
            .retain(|(tool_source, _), _| tool_source != source);
    }

    fn degrade_after_call(
        &mut self,
        handle: &McpToolHandle,
        stage: McpFailureStage,
        error: &ErrorEnvelope,
    ) {
        self.remove_catalog_source(&handle.source);
        if let Some(service) = self.services.get_mut(&handle.source) {
            service.status = failure_status(service.registration.requirement);
            service.failure = Some(service_failure(&handle.source, stage, error));
            service.client = None;
        }
    }
}

fn tool_handle(tool: &McpTool, generation: u64) -> McpToolHandle {
    McpToolHandle {
        source: tool.source.clone(),
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: tool.input_schema.clone(),
        output_limit_bytes: tool.output_limit_bytes,
        generation,
    }
}

fn failure_status(requirement: McpServiceRequirement) -> McpServiceStatus {
    match requirement {
        McpServiceRequirement::Required => McpServiceStatus::Failed,
        McpServiceRequirement::Optional => McpServiceStatus::Degraded,
    }
}

fn service_failure(
    source: &str,
    stage: McpFailureStage,
    error: &ErrorEnvelope,
) -> McpServiceFailure {
    McpServiceFailure {
        source: source.to_string(),
        stage,
        code: error.code,
        message: error.message.clone(),
    }
}

fn stale_handle(handle: &McpToolHandle) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::ToolArgsInvalid,
        format!(
            "stale or unavailable MCP tool handle: {}:{} generation {}",
            handle.source, handle.name, handle.generation
        ),
    )
}

fn safe_result(handle: &McpToolHandle, result: &McpCallResult) -> Result<(), ErrorEnvelope> {
    if result.source() != handle.source
        || result.tool() != handle.name
        || result.output_limit_bytes() != handle.output_limit_bytes
        || result.visible_output().len() > handle.output_limit_bytes
        || result.full_output().len() > MAX_SAFE_RESULT_BYTES
    {
        return Err(ErrorEnvelope::new(
            ErrorCode::Internal,
            "MCP result failed source, tool or output safety validation",
        ));
    }
    Ok(())
}
