//! TASK-908: non-secret provider settings persisted separately from credentials.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use harness_host::{ErrorCode, ErrorEnvelope};
use serde::{Deserialize, Serialize};

const MAX_MODEL_BYTES: usize = 256;
const MAX_FETCH_HOSTS: usize = 64;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProviderSettings {
    pub(crate) base_url: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) fetch_allow: Vec<String>,
    #[serde(default)]
    pub(crate) compact_mode: bool,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
            fetch_allow: Vec::new(),
            compact_mode: false,
        }
    }
}

impl ProviderSettings {
    pub(crate) fn validate(mut self) -> Result<Self, ErrorEnvelope> {
        self.base_url = self.base_url.trim().trim_end_matches('/').to_owned();
        self.model = self.model.trim().to_owned();
        if self.model.is_empty() || self.model.len() > MAX_MODEL_BYTES {
            return Err(invalid("model is blank or too long"));
        }
        if self.fetch_allow.len() > MAX_FETCH_HOSTS {
            return Err(invalid("fetch allowlist has too many hosts"));
        }
        for host in &mut self.fetch_allow {
            *host = host.trim().to_ascii_lowercase();
            validate_fetch_host(host)?;
        }
        self.fetch_allow.sort();
        self.fetch_allow.dedup();
        Ok(self)
    }
}

pub(crate) struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub(crate) fn new(workspace: &Path) -> Self {
        Self {
            path: workspace.join(".harness").join("desktop-settings.json"),
        }
    }

    pub(crate) fn load(&self) -> Result<ProviderSettings, ErrorEnvelope> {
        if !self.path.exists() {
            return Ok(ProviderSettings::default());
        }
        let bytes = std::fs::read(&self.path).map_err(|_| internal())?;
        serde_json::from_slice::<ProviderSettings>(&bytes)
            .map_err(|_| internal())?
            .validate()
    }

    pub(crate) fn save(
        &self,
        settings: ProviderSettings,
    ) -> Result<ProviderSettings, ErrorEnvelope> {
        let settings = settings.validate()?;
        let parent = self.path.parent().ok_or_else(internal)?;
        std::fs::create_dir_all(parent).map_err(|_| internal())?;
        let encoded = serde_json::to_vec_pretty(&settings).map_err(|_| internal())?;
        std::fs::write(&self.path, encoded).map_err(|_| internal())?;
        Ok(settings)
    }
}

fn validate_fetch_host(host: &str) -> Result<(), ErrorEnvelope> {
    let labels_valid = !host.is_empty()
        && host.len() <= 253
        && host.is_ascii()
        && !host.contains(['/', ':', '*', '\\'])
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    let unsafe_address = host.parse::<IpAddr>().is_ok_and(|address| match address {
        IpAddr::V4(address) => {
            address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_broadcast()
                || address.is_unspecified()
        }
        IpAddr::V6(address) => {
            address.is_loopback() || address.is_unspecified() || address.is_unique_local()
        }
    });
    if !labels_valid || unsafe_address || host == "localhost" {
        return Err(invalid(
            "fetch allowlist contains an invalid or private host",
        ));
    }
    Ok(())
}

fn invalid(message: &'static str) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::ToolArgsInvalid, message)
}

fn internal() -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::Internal, "settings storage is unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_fetch_closed_and_file_contains_no_secret_field() {
        let root = std::env::temp_dir().join(format!("ih-settings-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(&root);
        let saved = store.save(ProviderSettings::default()).unwrap();
        assert!(saved.fetch_allow.is_empty());
        let file = std::fs::read_to_string(&store.path).unwrap();
        assert!(!file.to_ascii_lowercase().contains("api_key"));
        assert!(!file.to_ascii_lowercase().contains("apikey"));
        assert_eq!(store.load().unwrap(), saved);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn allowlist_rejects_broad_urls_and_private_targets() {
        for host in [
            "*.example.com",
            "https://example.com",
            "localhost",
            "127.0.0.1",
        ] {
            let settings = ProviderSettings {
                fetch_allow: vec![host.into()],
                ..ProviderSettings::default()
            };
            assert_eq!(
                settings.validate().unwrap_err().code,
                ErrorCode::ToolArgsInvalid
            );
        }
    }
}
