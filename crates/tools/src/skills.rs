//! P3/TASK-502：可信根内 Skill 发现、受限 frontmatter、指纹刷新与继承校验。

use protocol::{ErrorCode, ErrorEnvelope};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_DESCRIPTION_BYTES: usize = 1_024;
const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSkill {
    name: String,
    description: String,
    instructions: String,
    canonical_path: PathBuf,
    fingerprint: u64,
}

impl VerifiedSkill {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillRefresh {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSkillScope {
    fingerprints: BTreeMap<String, u64>,
}

impl VerifiedSkillScope {
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.fingerprints.keys().map(String::as_str)
    }
}

#[derive(Debug, Clone)]
pub struct SkillCatalog {
    workspace_root: PathBuf,
    skill_root: PathBuf,
    skills: BTreeMap<String, VerifiedSkill>,
}

impl SkillCatalog {
    pub fn discover(workspace_root: &Path) -> Result<Self, ErrorEnvelope> {
        let workspace_root = workspace_root
            .canonicalize()
            .map_err(|error| io_error("canonicalize workspace", error))?;
        let skill_root = workspace_root.join(".harness").join("skills");
        let mut catalog = Self {
            workspace_root,
            skill_root,
            skills: BTreeMap::new(),
        };
        catalog.refresh()?;
        Ok(catalog)
    }

    pub fn refresh(&mut self) -> Result<SkillRefresh, ErrorEnvelope> {
        let discovered = scan_skills(&self.workspace_root, &self.skill_root)?;
        let old_names: BTreeSet<_> = self.skills.keys().cloned().collect();
        let new_names: BTreeSet<_> = discovered.keys().cloned().collect();
        let added = new_names.difference(&old_names).cloned().collect();
        let removed = old_names.difference(&new_names).cloned().collect();
        let modified = old_names
            .intersection(&new_names)
            .filter(|name| {
                self.skills.get(*name).map(VerifiedSkill::fingerprint)
                    != discovered.get(*name).map(VerifiedSkill::fingerprint)
            })
            .cloned()
            .collect();
        self.skills = discovered;
        Ok(SkillRefresh {
            added,
            modified,
            removed,
        })
    }

    pub fn skills(&self) -> impl Iterator<Item = &VerifiedSkill> {
        self.skills.values()
    }

    pub fn get(&self, name: &str) -> Option<&VerifiedSkill> {
        self.skills.get(name)
    }

    pub fn verified_scope<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<VerifiedSkillScope, ErrorEnvelope> {
        let mut fingerprints = BTreeMap::new();
        for name in names {
            let skill = self
                .skills
                .get(name)
                .ok_or_else(|| denied(format!("skill is not verified: {name}")))?;
            if fingerprints
                .insert(name.to_string(), skill.fingerprint)
                .is_some()
            {
                return Err(args_error(format!("duplicate requested skill: {name}")));
            }
        }
        Ok(VerifiedSkillScope { fingerprints })
    }

    pub fn inherit_scope<'a>(
        &self,
        parent: &VerifiedSkillScope,
        requested: impl IntoIterator<Item = &'a str>,
    ) -> Result<VerifiedSkillScope, ErrorEnvelope> {
        let mut fingerprints = BTreeMap::new();
        for name in requested {
            let current = self
                .skills
                .get(name)
                .ok_or_else(|| denied(format!("child requested unverified skill: {name}")))?;
            let parent_fingerprint = parent
                .fingerprints
                .get(name)
                .ok_or_else(|| denied(format!("child skill expands parent scope: {name}")))?;
            if *parent_fingerprint != current.fingerprint {
                return Err(denied(format!(
                    "parent skill verification is stale: {name}"
                )));
            }
            fingerprints.insert(name.to_string(), current.fingerprint);
        }
        Ok(VerifiedSkillScope { fingerprints })
    }
}

fn scan_skills(
    workspace_root: &Path,
    skill_root: &Path,
) -> Result<BTreeMap<String, VerifiedSkill>, ErrorEnvelope> {
    if !skill_root.exists() {
        return Ok(BTreeMap::new());
    }
    reject_symlink(skill_root, "skill root")?;
    let canonical_root = skill_root
        .canonicalize()
        .map_err(|error| io_error("canonicalize skill root", error))?;
    ensure_contained(
        workspace_root,
        &canonical_root,
        "skill root escapes workspace",
    )?;
    let entries =
        fs::read_dir(&canonical_root).map_err(|error| io_error("read skill directory", error))?;
    let mut skills = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error("read skill entry", error))?;
        let metadata = entry
            .file_type()
            .map_err(|error| io_error("read skill entry type", error))?;
        if metadata.is_symlink() {
            return Err(denied("skill directory symlinks are not trusted"));
        }
        if !metadata.is_dir() {
            continue;
        }
        let directory = entry
            .path()
            .canonicalize()
            .map_err(|error| io_error("canonicalize skill directory", error))?;
        ensure_contained(
            &canonical_root,
            &directory,
            "skill directory escapes trusted root",
        )?;
        let skill_file = directory.join("SKILL.md");
        reject_symlink(&skill_file, "SKILL.md")?;
        let canonical_file = skill_file
            .canonicalize()
            .map_err(|error| io_error("canonicalize SKILL.md", error))?;
        ensure_contained(
            &canonical_root,
            &canonical_file,
            "SKILL.md escapes trusted root",
        )?;
        let content = fs::read_to_string(&canonical_file)
            .map_err(|error| io_error("read SKILL.md", error))?
            .replace("\r\n", "\n");
        let parsed = parse_skill(&content, canonical_file)?;
        if skills.insert(parsed.name.clone(), parsed).is_some() {
            return Err(args_error("duplicate skill name"));
        }
    }
    Ok(skills)
}

fn parse_skill(content: &str, canonical_path: PathBuf) -> Result<VerifiedSkill, ErrorEnvelope> {
    let remainder = content
        .strip_prefix("---\n")
        .ok_or_else(|| args_error("SKILL.md must start with YAML frontmatter"))?;
    let (frontmatter, instructions) = remainder
        .split_once("\n---\n")
        .ok_or_else(|| args_error("SKILL.md frontmatter is not closed"))?;
    let mut fields = BTreeMap::new();
    for line in frontmatter.lines() {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| args_error("frontmatter entries must be key: value"))?;
        let key = key.trim();
        let value = value.trim();
        if !matches!(key, "name" | "description") {
            return Err(args_error(format!(
                "unsupported skill frontmatter field: {key}"
            )));
        }
        if value.is_empty()
            || value.starts_with(['|', '>', '&', '*', '!', '{', '['])
            || fields.insert(key, value).is_some()
        {
            return Err(args_error("invalid or duplicate skill frontmatter value"));
        }
    }
    if fields.len() != 2 {
        return Err(args_error(
            "skill frontmatter requires name and description",
        ));
    }
    let name = fields["name"];
    if !safe_name(name) {
        return Err(denied("skill name contains traversal or unsafe characters"));
    }
    let description = fields["description"];
    if description.len() > MAX_DESCRIPTION_BYTES {
        return Err(args_error("skill description is too large"));
    }
    let instructions = instructions.trim();
    if instructions.is_empty() || instructions.len() > MAX_INSTRUCTIONS_BYTES {
        return Err(args_error("skill instructions are empty or too large"));
    }
    Ok(VerifiedSkill {
        name: name.to_string(),
        description: description.to_string(),
        instructions: instructions.to_string(),
        canonical_path,
        fingerprint: fnv1a(content.as_bytes()),
    })
}

fn reject_symlink(path: &Path, label: &str) -> Result<(), ErrorEnvelope> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io_error(format!("inspect {label}"), error))?;
    if metadata.file_type().is_symlink() {
        return Err(denied(format!("{label} symlinks are not trusted")));
    }
    Ok(())
}

fn ensure_contained(root: &Path, path: &Path, message: &str) -> Result<(), ErrorEnvelope> {
    if !path.starts_with(root) {
        return Err(denied(message));
    }
    Ok(())
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
        && name != "."
        && name != ".."
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn args_error(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::ToolArgsInvalid, message)
}

fn denied(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::SandboxDenied, message)
}

fn io_error(action: impl AsRef<str>, error: std::io::Error) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("failed to {}: {error}", action.as_ref()),
    )
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
