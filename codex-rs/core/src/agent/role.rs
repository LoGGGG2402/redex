//! Applies agent-role configuration layers on top of an existing session config.
//!
//! Roles are selected at spawn time and are loaded with the same config machinery as
//! `config.toml`. This module resolves built-in and user-defined role files, inserts the role as a
//! high-precedence layer, and preserves the caller's current profile/provider unless the role
//! explicitly takes ownership of model selection. It does not decide when to spawn a sub-agent or
//! which role to use; the multi-agent tool handler owns that orchestration.

use crate::config::AgentRoleConfig;
use crate::config::Config;
use crate::config::ConfigOverrides;
use crate::config::agent_roles::parse_agent_role_file_contents;
use crate::config::deserialize_config_toml_with_base;
use crate::config_loader::ConfigLayerEntry;
use crate::config_loader::ConfigLayerStack;
use crate::config_loader::ConfigLayerStackOrdering;
use crate::config_loader::resolve_relative_paths_in_config_toml;
use anyhow::anyhow;
use codex_app_server_protocol::ConfigLayerSource;
use codex_config::config_toml::ConfigToml;
use codex_protocol::models::DeveloperInstructions;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;
use toml::Value as TomlValue;

/// The role name used when a caller omits `agent_type`.
pub const DEFAULT_ROLE_NAME: &str = "worker";
/// The role name assigned to the root thread for metadata and root-only prompt overlay logic.
#[cfg_attr(not(test), allow(dead_code))]
pub const ROOT_AGENT_ROLE_NAME: &str = "orchestrator";
const LEGACY_DEFAULT_ROLE_NAME: &str = "default";
const AGENT_TYPE_UNAVAILABLE_ERROR: &str = "agent type is currently not available";

/// Applies a named role layer to `config` while preserving caller-owned model selection.
///
/// The role layer is inserted at session-flag precedence so it can override persisted config, but
/// the caller's current `profile` and `model_provider` remain sticky runtime choices unless the
/// role explicitly sets `profile`, explicitly sets `model_provider`, or rewrites the active
/// profile's `model_provider` in place. Rebuilding the config without those overrides would make a
/// spawned agent silently fall back to the default provider, which is the bug this preservation
/// logic avoids.
pub(crate) async fn apply_role_to_config(
    config: &mut Config,
    role_name: Option<&str>,
) -> Result<(), String> {
    let role_name = role_name.unwrap_or(DEFAULT_ROLE_NAME);

    let role = resolve_role_config(config, role_name)
        .cloned()
        .ok_or_else(|| format!("unknown agent_type '{role_name}'"))?;

    apply_role_to_config_inner(config, role_name, &role)
        .await
        .map_err(|err| {
            tracing::warn!("failed to apply role to config: {err}");
            AGENT_TYPE_UNAVAILABLE_ERROR.to_string()
        })
}

/// Applies the implicit default spawn role while preserving spawn-time runtime overrides.
///
/// Spawn handlers build a child config from the live turn, so values such as model selection,
/// provider overrides, reasoning effort, base instructions, and inherited developer context may
/// differ from the persisted config stack. Reloading the config stack for the default role would
/// otherwise discard those runtime values. For the implicit default role only, we reload the role
/// layer to keep config-layer metadata consistent, then restore runtime-owned fields and append
/// the role doctrine onto any inherited developer instructions.
pub(crate) async fn apply_default_spawn_role_to_config(config: &mut Config) -> Result<(), String> {
    let role_name = DEFAULT_ROLE_NAME;
    let role = resolve_role_config(config, role_name)
        .cloned()
        .ok_or_else(|| format!("unknown agent_type '{role_name}'"))?;

    apply_default_spawn_role_to_config_inner(config, role_name, &role)
        .await
        .map_err(|err| {
            tracing::warn!("failed to apply default spawn role to config: {err}");
            AGENT_TYPE_UNAVAILABLE_ERROR.to_string()
        })
}

/// Applies the built-in root-session role while preserving existing developer guidance.
pub(crate) async fn apply_root_role_to_config(config: &mut Config) -> Result<(), String> {
    apply_root_role_to_config_inner(config, ROOT_AGENT_ROLE_NAME, built_in::root_config())
        .await
        .map_err(|err| {
            tracing::warn!("failed to apply root role to config: {err}");
            AGENT_TYPE_UNAVAILABLE_ERROR.to_string()
        })
}

async fn apply_role_to_config_inner(
    config: &mut Config,
    role_name: &str,
    role: &AgentRoleConfig,
) -> anyhow::Result<()> {
    let is_built_in = !config.agent_roles.contains_key(role_name);
    let Some(config_file) = role.config_file.as_ref() else {
        return Ok(());
    };
    let role_layer_toml = load_role_layer_toml(config, config_file, is_built_in, role_name).await?;
    if role_layer_toml
        .as_table()
        .is_some_and(toml::map::Map::is_empty)
    {
        return Ok(());
    }
    let (preserve_current_profile, preserve_current_provider) =
        preservation_policy(config, &role_layer_toml);

    *config = reload::build_next_config(
        config,
        role_layer_toml,
        preserve_current_profile,
        preserve_current_provider,
    )?;
    Ok(())
}

async fn apply_default_spawn_role_to_config_inner(
    config: &mut Config,
    role_name: &str,
    role: &AgentRoleConfig,
) -> anyhow::Result<()> {
    let is_built_in = !config.agent_roles.contains_key(role_name);
    let Some(config_file) = role.config_file.as_ref() else {
        return Ok(());
    };
    let role_layer_toml = load_role_layer_toml(config, config_file, is_built_in, role_name).await?;
    if role_layer_toml
        .as_table()
        .is_some_and(toml::map::Map::is_empty)
    {
        return Ok(());
    }

    let runtime_fields = DefaultSpawnRoleRuntimeFields::from_config(config);
    let role_developer_instructions = role_layer_toml
        .get("developer_instructions")
        .and_then(TomlValue::as_str)
        .map(str::to_owned);
    let role_selects_profile = role_layer_toml.get("profile").is_some();
    let role_selects_provider = role_layer_toml.get("model_provider").is_some();
    let role_selects_model = role_layer_toml.get("model").is_some();
    let role_selects_reasoning_effort = role_layer_toml.get("model_reasoning_effort").is_some();
    let role_selects_reasoning_summary = role_layer_toml.get("model_reasoning_summary").is_some();
    let role_selects_compact_prompt = role_layer_toml.get("compact_prompt").is_some();
    let role_selects_agent_max_threads = role_layer_toml.get("agent_max_threads").is_some();
    let role_selects_agent_job_max_runtime_seconds = role_layer_toml
        .get("agent_job_max_runtime_seconds")
        .is_some();
    let role_selects_agent_max_depth = role_layer_toml.get("agent_max_depth").is_some();
    let role_selects_user_instructions = role_layer_toml.get("user_instructions").is_some();

    let (preserve_current_profile, preserve_current_provider) =
        preservation_policy(config, &role_layer_toml);
    *config = reload::build_next_config(
        config,
        role_layer_toml,
        preserve_current_profile,
        preserve_current_provider,
    )?;

    config.base_instructions = runtime_fields.base_instructions;
    if !role_selects_profile {
        config.active_profile = runtime_fields.active_profile;
    }
    if !role_selects_provider {
        config.model_provider = runtime_fields.model_provider;
        config.model_provider_id = runtime_fields.model_provider_id;
    }
    if !role_selects_model {
        config.model = runtime_fields.model;
    }
    if !role_selects_reasoning_effort {
        config.model_reasoning_effort = runtime_fields.model_reasoning_effort;
    }
    if !role_selects_reasoning_summary {
        config.model_reasoning_summary = runtime_fields.model_reasoning_summary;
    }
    if !role_selects_compact_prompt {
        config.compact_prompt = runtime_fields.compact_prompt;
    }
    config.multi_agent_v2 = runtime_fields.multi_agent_v2;
    config.features = runtime_fields.features;
    if !role_selects_agent_max_threads {
        config.agent_max_threads = runtime_fields.agent_max_threads;
    }
    if !role_selects_agent_job_max_runtime_seconds {
        config.agent_job_max_runtime_seconds = runtime_fields.agent_job_max_runtime_seconds;
    }
    if !role_selects_agent_max_depth {
        config.agent_max_depth = runtime_fields.agent_max_depth;
    }
    if !role_selects_user_instructions {
        config.user_instructions = runtime_fields.user_instructions;
    }
    config.developer_instructions = match (
        runtime_fields.developer_instructions,
        role_developer_instructions,
    ) {
        (Some(existing), Some(role_text)) => Some(
            DeveloperInstructions::new(existing)
                .concat(DeveloperInstructions::new(role_text))
                .into_text(),
        ),
        (None, Some(role_text)) => Some(role_text),
        (Some(existing), None) => Some(existing),
        (None, None) => None,
    };

    Ok(())
}

async fn apply_root_role_to_config_inner(
    config: &mut Config,
    role_name: &str,
    role: &AgentRoleConfig,
) -> anyhow::Result<()> {
    let Some(config_file) = role.config_file.as_ref() else {
        return Ok(());
    };
    let role_layer_toml =
        load_role_layer_toml(config, config_file, /*is_built_in*/ true, role_name).await?;
    if role_layer_toml
        .as_table()
        .is_some_and(toml::map::Map::is_empty)
    {
        return Ok(());
    }

    let existing_developer_instructions = config.developer_instructions.clone();
    let (preserve_current_profile, preserve_current_provider) =
        preservation_policy(config, &role_layer_toml);
    *config = reload::build_next_config(
        config,
        role_layer_toml,
        preserve_current_profile,
        preserve_current_provider,
    )?;
    config.developer_instructions = match (
        existing_developer_instructions,
        config.developer_instructions.take(),
    ) {
        (Some(existing), Some(role_text)) => Some(
            DeveloperInstructions::new(existing)
                .concat(DeveloperInstructions::new(role_text))
                .into_text(),
        ),
        (None, Some(role_text)) => Some(role_text),
        (Some(existing), None) => Some(existing),
        (None, None) => None,
    };

    Ok(())
}

struct DefaultSpawnRoleRuntimeFields {
    active_profile: Option<String>,
    user_instructions: Option<String>,
    model: Option<String>,
    model_provider: codex_model_provider_info::ModelProviderInfo,
    model_provider_id: String,
    model_reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    model_reasoning_summary: Option<codex_protocol::config_types::ReasoningSummary>,
    base_instructions: Option<String>,
    developer_instructions: Option<String>,
    compact_prompt: Option<String>,
    multi_agent_v2: crate::config::MultiAgentV2Config,
    features: crate::config::ManagedFeatures,
    agent_max_threads: Option<usize>,
    agent_job_max_runtime_seconds: Option<u64>,
    agent_max_depth: i32,
}

impl DefaultSpawnRoleRuntimeFields {
    fn from_config(config: &Config) -> Self {
        Self {
            active_profile: config.active_profile.clone(),
            user_instructions: config.user_instructions.clone(),
            model: config.model.clone(),
            model_provider: config.model_provider.clone(),
            model_provider_id: config.model_provider_id.clone(),
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            base_instructions: config.base_instructions.clone(),
            developer_instructions: config.developer_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            multi_agent_v2: config.multi_agent_v2.clone(),
            features: config.features.clone(),
            agent_max_threads: config.agent_max_threads,
            agent_job_max_runtime_seconds: config.agent_job_max_runtime_seconds,
            agent_max_depth: config.agent_max_depth,
        }
    }
}

async fn load_role_layer_toml(
    config: &Config,
    config_file: &Path,
    is_built_in: bool,
    role_name: &str,
) -> anyhow::Result<TomlValue> {
    let (role_config_toml, role_config_base) = if is_built_in {
        let role_config_contents = built_in::config_file_contents(config_file)
            .map(str::to_owned)
            .ok_or(anyhow!("No corresponding config content"))?;
        let role_config_toml: TomlValue = toml::from_str(&role_config_contents)?;
        (role_config_toml, config.codex_home.as_path())
    } else {
        let role_config_contents = tokio::fs::read_to_string(config_file).await?;
        let role_config_base = config_file
            .parent()
            .ok_or(anyhow!("No corresponding config content"))?;
        let role_config_toml = parse_agent_role_file_contents(
            &role_config_contents,
            config_file,
            role_config_base,
            Some(role_name),
        )?
        .config;
        (role_config_toml, role_config_base)
    };

    deserialize_config_toml_with_base(role_config_toml.clone(), role_config_base)?;
    Ok(resolve_relative_paths_in_config_toml(
        role_config_toml,
        role_config_base,
    )?)
}

pub(crate) fn resolve_role_config<'a>(
    config: &'a Config,
    role_name: &str,
) -> Option<&'a AgentRoleConfig> {
    config
        .agent_roles
        .get(role_name)
        .or_else(|| built_in::configs().get(role_name))
}

fn preservation_policy(config: &Config, role_layer_toml: &TomlValue) -> (bool, bool) {
    let role_selects_provider = role_layer_toml.get("model_provider").is_some();
    let role_selects_profile = role_layer_toml.get("profile").is_some();
    let role_updates_active_profile_provider = config
        .active_profile
        .as_ref()
        .and_then(|active_profile| {
            role_layer_toml
                .get("profiles")
                .and_then(TomlValue::as_table)
                .and_then(|profiles| profiles.get(active_profile))
                .and_then(TomlValue::as_table)
                .map(|profile| profile.contains_key("model_provider"))
        })
        .unwrap_or(false);
    let preserve_current_profile = !role_selects_provider && !role_selects_profile;
    let preserve_current_provider =
        preserve_current_profile && !role_updates_active_profile_provider;
    (preserve_current_profile, preserve_current_provider)
}

mod reload {
    use super::*;

    pub(super) fn build_next_config(
        config: &Config,
        role_layer_toml: TomlValue,
        preserve_current_profile: bool,
        preserve_current_provider: bool,
    ) -> anyhow::Result<Config> {
        let active_profile_name = preserve_current_profile
            .then_some(config.active_profile.as_deref())
            .flatten();
        let config_layer_stack =
            build_config_layer_stack(config, &role_layer_toml, active_profile_name)?;
        let mut merged_config = deserialize_effective_config(config, &config_layer_stack)?;
        if preserve_current_profile {
            merged_config.profile = None;
        }

        let mut next_config = Config::load_config_with_layer_stack(
            merged_config,
            reload_overrides(config, preserve_current_provider),
            config.codex_home.clone(),
            config_layer_stack,
        )?;
        if preserve_current_profile {
            next_config.active_profile = config.active_profile.clone();
        }
        Ok(next_config)
    }

    fn build_config_layer_stack(
        config: &Config,
        role_layer_toml: &TomlValue,
        active_profile_name: Option<&str>,
    ) -> anyhow::Result<ConfigLayerStack> {
        let mut layers = existing_layers(config);
        if let Some(resolved_profile_layer) =
            resolved_profile_layer(config, &layers, role_layer_toml, active_profile_name)?
        {
            insert_layer(&mut layers, resolved_profile_layer);
        }
        insert_layer(&mut layers, role_layer(role_layer_toml.clone()));
        Ok(ConfigLayerStack::new(
            layers,
            config.config_layer_stack.requirements().clone(),
            config.config_layer_stack.requirements_toml().clone(),
        )?)
    }

    fn resolved_profile_layer(
        config: &Config,
        existing_layers: &[ConfigLayerEntry],
        role_layer_toml: &TomlValue,
        active_profile_name: Option<&str>,
    ) -> anyhow::Result<Option<ConfigLayerEntry>> {
        let Some(active_profile_name) = active_profile_name else {
            return Ok(None);
        };

        let mut layers = existing_layers.to_vec();
        insert_layer(&mut layers, role_layer(role_layer_toml.clone()));
        let merged_config = deserialize_effective_config(
            config,
            &ConfigLayerStack::new(
                layers,
                config.config_layer_stack.requirements().clone(),
                config.config_layer_stack.requirements_toml().clone(),
            )?,
        )?;
        let resolved_profile =
            merged_config.get_config_profile(Some(active_profile_name.to_string()))?;
        Ok(Some(ConfigLayerEntry::new(
            ConfigLayerSource::SessionFlags,
            TomlValue::try_from(resolved_profile)?,
        )))
    }

    fn deserialize_effective_config(
        config: &Config,
        config_layer_stack: &ConfigLayerStack,
    ) -> anyhow::Result<ConfigToml> {
        Ok(deserialize_config_toml_with_base(
            config_layer_stack.effective_config(),
            &config.codex_home,
        )?)
    }

    fn existing_layers(config: &Config) -> Vec<ConfigLayerEntry> {
        config
            .config_layer_stack
            .get_layers(
                ConfigLayerStackOrdering::LowestPrecedenceFirst,
                /*include_disabled*/ true,
            )
            .into_iter()
            .cloned()
            .collect()
    }

    fn insert_layer(layers: &mut Vec<ConfigLayerEntry>, layer: ConfigLayerEntry) {
        let insertion_index =
            layers.partition_point(|existing_layer| existing_layer.name <= layer.name);
        layers.insert(insertion_index, layer);
    }

    fn role_layer(role_layer_toml: TomlValue) -> ConfigLayerEntry {
        ConfigLayerEntry::new(ConfigLayerSource::SessionFlags, role_layer_toml)
    }

    fn reload_overrides(config: &Config, preserve_current_provider: bool) -> ConfigOverrides {
        ConfigOverrides {
            cwd: Some(config.cwd.to_path_buf()),
            model_provider: preserve_current_provider.then(|| config.model_provider_id.clone()),
            codex_linux_sandbox_exe: config.codex_linux_sandbox_exe.clone(),
            main_execve_wrapper_exe: config.main_execve_wrapper_exe.clone(),
            js_repl_node_path: config.js_repl_node_path.clone(),
            ..Default::default()
        }
    }
}

pub(crate) mod spawn_tool_spec {
    use super::*;

    /// Builds the spawn-agent tool description text from built-in and configured roles.
    pub(crate) fn build(user_defined_agent_roles: &BTreeMap<String, AgentRoleConfig>) -> String {
        let built_in_roles = built_in::configs();
        build_from_configs(built_in_roles, user_defined_agent_roles)
    }

    // This function is not inlined for testing purpose.
    fn build_from_configs(
        built_in_roles: &BTreeMap<String, AgentRoleConfig>,
        user_defined_roles: &BTreeMap<String, AgentRoleConfig>,
    ) -> String {
        let mut seen = BTreeSet::new();
        let mut formatted_roles = Vec::new();
        for (name, declaration) in user_defined_roles {
            if seen.insert(name.as_str()) {
                formatted_roles.push(format_role(name, declaration));
            }
        }
        for (name, declaration) in built_in_roles {
            if seen.insert(name.as_str()) {
                formatted_roles.push(format_role(name, declaration));
            }
        }

        format!(
            "Optional type name for the new agent. If omitted, `{DEFAULT_ROLE_NAME}` is used.\nAvailable roles:\n{}",
            formatted_roles.join("\n"),
        )
    }

    fn format_role(name: &str, declaration: &AgentRoleConfig) -> String {
        if let Some(description) = &declaration.description {
            let locked_settings_note = declaration
                .config_file
                .as_ref()
                .and_then(|config_file| {
                    built_in::config_file_contents(config_file)
                        .map(str::to_owned)
                        .or_else(|| std::fs::read_to_string(config_file).ok())
                })
                .and_then(|contents| toml::from_str::<TomlValue>(&contents).ok())
                .map(|role_toml| {
                    let model = role_toml
                        .get("model")
                        .and_then(TomlValue::as_str);
                    let reasoning_effort = role_toml
                        .get("model_reasoning_effort")
                        .and_then(TomlValue::as_str);

                    match (model, reasoning_effort) {
                        (Some(model), Some(reasoning_effort)) => format!(
                            "\n- This role's model is set to `{model}` and its reasoning effort is set to `{reasoning_effort}`. These settings cannot be changed."
                        ),
                        (Some(model), None) => {
                            format!(
                                "\n- This role's model is set to `{model}` and cannot be changed."
                            )
                        }
                        (None, Some(reasoning_effort)) => {
                            format!(
                                "\n- This role's reasoning effort is set to `{reasoning_effort}` and cannot be changed."
                            )
                        }
                        (None, None) => String::new(),
                    }
                })
                .unwrap_or_default();
            format!("{name}: {{\n{description}{locked_settings_note}\n}}")
        } else {
            format!("{name}: no description")
        }
    }
}

mod built_in {
    use super::*;

    /// Returns the cached built-in role declarations defined in this module.
    pub(super) fn configs() -> &'static BTreeMap<String, AgentRoleConfig> {
        static CONFIG: LazyLock<BTreeMap<String, AgentRoleConfig>> = LazyLock::new(|| {
            BTreeMap::from([
                (
                    LEGACY_DEFAULT_ROLE_NAME.to_string(),
                    AgentRoleConfig {
                        description: Some(
                            "Legacy alias for `worker`; retained for compatibility."
                                .to_string(),
                        ),
                        config_file: Some("worker.toml".to_string().parse().unwrap_or_default()),
                        nickname_candidates: None,
                    }
                ),
                (
                    "worker".to_string(),
                    AgentRoleConfig {
                        description: Some(r#"Use for execution and production work.
Typical tasks:
- Implement part of a feature
- Fix tests or bugs
- Split large refactors into independent chunks
Rules:
- Explicitly assign **ownership** of the task (files / responsibility). When the subtask involves code changes, you should clearly specify which files or modules the worker is responsible for. This helps avoid merge conflicts and ensures accountability. For example, you can say "Worker 1 is responsible for updating the authentication module, while Worker 2 will handle the database layer." By defining clear ownership, you can delegate more effectively and reduce coordination overhead.
- Always tell workers they are **not alone in the codebase**, and they should not revert the edits made by others, and they should adjust their implementation to accommodate the changes made by others. This is important because there may be multiple workers making changes in parallel, and they need to be aware of each other's work to avoid conflicts and ensure a cohesive final product."#.to_string()),
                        config_file: Some("worker.toml".to_string().parse().unwrap_or_default()),
                        nickname_candidates: None,
                    }
                ),
                (
                    "explorer".to_string(),
                    AgentRoleConfig {
                        description: Some(r#"Use `explorer` for specific codebase questions.
Explorers are fast and authoritative.
They must be used to ask specific, well-scoped questions on the codebase.
Rules:
- In order to avoid redundant work, you should avoid exploring the same problem that explorers have already covered. Typically, you should trust the explorer results without additional verification. You are still allowed to inspect the code yourself to gain the needed context!
- You are encouraged to spawn up multiple explorers in parallel when you have multiple distinct questions to ask about the codebase that can be answered independently. This allows you to get more information faster without waiting for one question to finish before asking the next. While waiting for the explorer results, you can continue working on other local tasks that do not depend on those results. This parallelism is a key advantage of delegation, so use it whenever you have multiple questions to ask.
- Reuse existing explorers for related questions."#.to_string()),
                        config_file: Some("explorer.toml".to_string().parse().unwrap_or_default()),
                        nickname_candidates: None,
                    }
                ),
                (
                    "auditor".to_string(),
                    AgentRoleConfig {
                        description: Some(r#"Use `auditor` for whitebox review of code, docs, JS, and schemas.
Auditors review by trust boundary, privilege boundary, state transition, and sink rather than by folder.
They hunt authn/authz drift, tenant isolation flaws, broken invariants, hidden risky flows, and exploit chains.
When the `reddex-plugin` stack is available, they should prefer semantic narrowing and durable source refs over broad prose summaries.
They own one bounded slice only, receive bounded task context only, do not take checkpoint ownership or reprioritize the engagement, and return evidence, uncertainty, blockers, exit status, and the next justified action to root."#.to_string()),
                        config_file: Some("auditor.toml".to_string().parse().unwrap_or_default()),
                        nickname_candidates: None,
                    }
                ),
                (
                    "recon".to_string(),
                    AgentRoleConfig {
                        description: Some(r#"Use `recon` for coverage-first attack-surface mapping.
Recon agents work from outside in: assets, entry points, routes, parameters, exposed info, and hidden surface first.
They separate observed, inferred, and unverified surface, keep blind spots explicit, prefer plugin-backed browser or proxy discovery when available, and return observed surface, uncertainty, blockers, exit status, and the next justified action to root.
They receive bounded task context only, do not take checkpoint ownership or reprioritize the engagement, and escalate decisions back to root."#.to_string()),
                        config_file: Some("recon.toml".to_string().parse().unwrap_or_default()),
                        nickname_candidates: None,
                    }
                ),
                (
                    "toolsmith".to_string(),
                    AgentRoleConfig {
                        description: Some(r#"Use `toolsmith` to build offensive helpers that speed up investigation.
Toolsmith agents write parsers, replay helpers, reducers, extractors, PoCs, and triage automation.
They treat code as an offensive multiplier, prefer helpers that interoperate cleanly with plugin-native artifacts and refs when available, keep ownership bounded, and return the helper, any evidence it produced, uncertainty, blockers, exit status, and the next justified action to root.
They receive bounded task context only, do not take checkpoint ownership or reprioritize the engagement, and keep their output subordinate to root."#.to_string()),
                        config_file: Some("toolsmith.toml".to_string().parse().unwrap_or_default()),
                        nickname_candidates: None,
                    }
                ),
                (
                    "validator".to_string(),
                    AgentRoleConfig {
                        description: Some(r#"Use `validator` to confirm exploitability and reduce false positives.
Validators turn promising signals into minimal repros, impact proof, and clear evidence with explicit limits.
They classify outcomes as `confirmed`, `suspected`, or `needs more data`, keep scope fixed, prefer plugin-backed replay and browser proof when available, and return evidence, uncertainty, blockers, exit status, and a prove, chain, or drop recommendation to root.
They receive bounded task context only, do not take checkpoint ownership or reprioritize the engagement, and escalate final interpretation back to root."#.to_string()),
                        config_file: Some("validator.toml".to_string().parse().unwrap_or_default()),
                        nickname_candidates: None,
                    }
                ),
                (
                    "verifier".to_string(),
                    AgentRoleConfig {
                        description: Some(r#"Use `verifier` for command-driven implementation and runtime verification.
Verifiers distrust code reading as proof and must run checks that exercise the implementation directly.
They report one check at a time with command, observed output, result, and end with `VERDICT: PASS`, `VERDICT: FAIL`, or `VERDICT: PARTIAL`.
They keep scope bounded to verification only, do not edit project files, do not take checkpoint ownership, and return verdict, evidence, blockers, and any residual risks to root."#.to_string()),
                        config_file: Some("verifier.toml".to_string().parse().unwrap_or_default()),
                        nickname_candidates: None,
                    }
                ),
                // Awaiter is temp removed
//                 (
//                     "awaiter".to_string(),
//                     AgentRoleConfig {
//                         description: Some(r#"Use an `awaiter` agent EVERY TIME you must run a command that will take some very long time.
// This includes, but not only:
// * testing
// * monitoring of a long running process
// * explicit ask to wait for something
//
// Rules:
// - When an awaiter is running, you can work on something else. If you need to wait for its completion, use the largest possible timeout.
// - Be patient with the `awaiter`.
// - Do not use an awaiter for every compilation/test if it won't take time. Only use if for long running commands.
// - Close the awaiter when you're done with it."#.to_string()),
//                         config_file: Some("awaiter.toml".to_string().parse().unwrap_or_default()),
//                     }
//                 )
            ])
        });
        &CONFIG
    }

    pub(super) fn root_config() -> &'static AgentRoleConfig {
        static ROOT_CONFIG: LazyLock<AgentRoleConfig> = LazyLock::new(|| AgentRoleConfig {
            description: None,
            config_file: Some("orchestrator.toml".to_string().parse().unwrap_or_default()),
            nickname_candidates: None,
        });
        &ROOT_CONFIG
    }

    /// Resolves a built-in role `config_file` path to embedded content.
    pub(super) fn config_file_contents(path: &Path) -> Option<&'static str> {
        const EXPLORER: &str = include_str!("builtins/explorer.toml");
        const AWAITER: &str = include_str!("builtins/awaiter.toml");
        const AUDITOR: &str = include_str!("builtins/auditor.toml");
        const ORCHESTRATOR: &str = include_str!("builtins/orchestrator.toml");
        const RECON: &str = include_str!("builtins/recon.toml");
        const TOOLSMITH: &str = include_str!("builtins/toolsmith.toml");
        const VALIDATOR: &str = include_str!("builtins/validator.toml");
        const VERIFIER: &str = include_str!("builtins/verifier.toml");
        const WORKER: &str = include_str!("builtins/worker.toml");
        match path.to_str()? {
            "explorer.toml" => Some(EXPLORER),
            "awaiter.toml" => Some(AWAITER),
            "auditor.toml" => Some(AUDITOR),
            "orchestrator.toml" => Some(ORCHESTRATOR),
            "recon.toml" => Some(RECON),
            "toolsmith.toml" => Some(TOOLSMITH),
            "validator.toml" => Some(VALIDATOR),
            "verifier.toml" => Some(VERIFIER),
            "worker.toml" => Some(WORKER),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "role_tests.rs"]
mod tests;
