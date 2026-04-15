use super::*;
use crate::SkillsManager;
use crate::config::CONFIG_TOML_FILE;
use crate::config::ConfigBuilder;
use crate::config_loader::ConfigLayerStackOrdering;
use crate::plugins::PluginsManager;
use crate::skills_load_input_from_config;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::Verbosity;
use codex_protocol::openai_models::ReasoningEffort;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

async fn test_config_with_cli_overrides(
    cli_overrides: Vec<(String, TomlValue)>,
) -> (TempDir, Config) {
    let home = TempDir::new().expect("create temp dir");
    let home_path = home.path().to_path_buf();
    let config = ConfigBuilder::default()
        .codex_home(home_path.clone())
        .cli_overrides(cli_overrides)
        .fallback_cwd(Some(home_path))
        .build()
        .await
        .expect("load test config");
    (home, config)
}

async fn write_role_config(home: &TempDir, name: &str, contents: &str) -> PathBuf {
    let role_path = home.path().join(name);
    tokio::fs::write(&role_path, contents)
        .await
        .expect("write role config");
    role_path
}

fn session_flags_layer_count(config: &Config) -> usize {
    config
        .config_layer_stack
        .get_layers(
            ConfigLayerStackOrdering::LowestPrecedenceFirst,
            /*include_disabled*/ true,
        )
        .into_iter()
        .filter(|layer| layer.name == ConfigLayerSource::SessionFlags)
        .count()
}

#[test]
fn default_and_root_role_constants_match_expected_values() {
    assert_eq!(DEFAULT_ROLE_NAME, "worker");
    assert_eq!(ROOT_AGENT_ROLE_NAME, "orchestrator");
    assert!(!built_in::configs().contains_key("orchestrator"));
    assert!(built_in::configs().contains_key("worker"));
    assert!(built_in::configs().contains_key("default"));
}

#[tokio::test]
async fn apply_role_defaults_to_worker_and_sets_worker_prompt() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let before_layers = session_flags_layer_count(&config);

    apply_role_to_config(&mut config, /*role_name*/ None)
        .await
        .expect("default role should apply");

    assert_eq!(
        config.developer_instructions.as_deref(),
        Some(
            "You are a worker.\nExecute one bounded task at a time.\nDefault to bounded offensive support work that root already scoped: recon expansion, semantic tracing, exploit reduction, payload shaping, replay setup, tooling, or evidence production.\nStay inside the authorized target slice and task boundary root assigned.\nOptimize for concrete artifacts and reproducible proof, not broad planning or open-ended speculation.\nRespect ownership boundaries for files, tools, and target slices and adapt to concurrent edits rather than reverting them.\nEscalate back to root when scope is unclear, priorities changed, authorization is uncertain, or you discover a materially different lead than the one assigned.\nDo not take checkpoint ownership, session synthesis ownership, or delegation strategy ownership.\nReturn the concrete outcome, evidence, blockers, and the next justified action to root.\n"
        )
    );
    assert_eq!(session_flags_layer_count(&config), before_layers + 1);
}

#[tokio::test]
async fn apply_role_returns_error_for_unknown_role() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;

    let err = apply_role_to_config(&mut config, Some("missing-role"))
        .await
        .expect_err("unknown role should fail");

    assert_eq!(err, "unknown agent_type 'missing-role'");
}

#[tokio::test]
async fn apply_explorer_role_is_available() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;

    apply_role_to_config(&mut config, Some("explorer"))
        .await
        .expect("explorer role should apply");
}

#[tokio::test]
async fn apply_worker_role_is_available_and_sets_worker_prompt() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let before_layers = session_flags_layer_count(&config);

    apply_role_to_config(&mut config, Some("worker"))
        .await
        .expect("worker role should apply");

    assert_eq!(
        config.developer_instructions.as_deref(),
        Some(
            "You are a worker.\nExecute one bounded task at a time.\nDefault to bounded offensive support work that root already scoped: recon expansion, semantic tracing, exploit reduction, payload shaping, replay setup, tooling, or evidence production.\nStay inside the authorized target slice and task boundary root assigned.\nOptimize for concrete artifacts and reproducible proof, not broad planning or open-ended speculation.\nRespect ownership boundaries for files, tools, and target slices and adapt to concurrent edits rather than reverting them.\nEscalate back to root when scope is unclear, priorities changed, authorization is uncertain, or you discover a materially different lead than the one assigned.\nDo not take checkpoint ownership, session synthesis ownership, or delegation strategy ownership.\nReturn the concrete outcome, evidence, blockers, and the next justified action to root.\n"
        )
    );
    assert_eq!(session_flags_layer_count(&config), before_layers + 1);
}

#[tokio::test]
async fn apply_default_alias_matches_worker_role() {
    let (_home, mut default_alias_config) = test_config_with_cli_overrides(Vec::new()).await;
    let (_home, mut worker_config) = test_config_with_cli_overrides(Vec::new()).await;

    apply_role_to_config(&mut default_alias_config, Some("default"))
        .await
        .expect("legacy default alias should apply");
    apply_role_to_config(&mut worker_config, Some("worker"))
        .await
        .expect("worker role should apply");

    assert_eq!(
        default_alias_config.developer_instructions,
        worker_config.developer_instructions
    );
    assert_eq!(
        session_flags_layer_count(&default_alias_config),
        session_flags_layer_count(&worker_config)
    );
}

#[tokio::test]
async fn apply_default_spawn_role_preserves_runtime_fields_and_appends_worker_doctrine() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let before_layers = session_flags_layer_count(&config);
    let active_profile = config.active_profile.clone();
    config.base_instructions = Some("base instructions".to_string());
    config.user_instructions = Some("parent user instructions".to_string());
    config.developer_instructions = Some("Parent developer instructions.".to_string());
    config.model = Some("gpt-5.1".to_string());
    config.model_provider.base_url = Some("https://proxy.example.test/v1".to_string());
    config.model_reasoning_effort = Some(ReasoningEffort::High);
    config.model_reasoning_summary = Some(ReasoningSummary::Detailed);
    config.compact_prompt = Some("compact".to_string());
    config.agent_max_threads = Some(7);
    config.agent_job_max_runtime_seconds = Some(123);
    config.agent_max_depth = 3;

    apply_default_spawn_role_to_config(&mut config)
        .await
        .expect("default spawn role should apply");

    assert_eq!(config.active_profile, active_profile);
    assert_eq!(
        config.base_instructions.as_deref(),
        Some("base instructions")
    );
    assert_eq!(
        config.user_instructions.as_deref(),
        Some("parent user instructions")
    );
    assert_eq!(config.model.as_deref(), Some("gpt-5.1"));
    assert_eq!(
        config.model_provider.base_url.as_deref(),
        Some("https://proxy.example.test/v1")
    );
    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(
        config.model_reasoning_summary,
        Some(ReasoningSummary::Detailed)
    );
    assert_eq!(config.compact_prompt.as_deref(), Some("compact"));
    assert_eq!(config.agent_max_threads, Some(7));
    assert_eq!(config.agent_job_max_runtime_seconds, Some(123));
    assert_eq!(config.agent_max_depth, 3);
    assert_eq!(session_flags_layer_count(&config), before_layers + 1);
    let developer_instructions = config
        .developer_instructions
        .as_deref()
        .expect("developer instructions should be present");
    assert!(developer_instructions.contains("Parent developer instructions."));
    assert!(developer_instructions.contains("You are a worker."));
}

#[tokio::test]
async fn apply_root_role_preserves_existing_guidance_and_appends_orchestrator_doctrine() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let before_layers = session_flags_layer_count(&config);
    config.developer_instructions = Some("Repo-specific root guidance.".to_string());

    apply_root_role_to_config(&mut config)
        .await
        .expect("root role should apply");

    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(session_flags_layer_count(&config), before_layers + 1);
    let developer_instructions = config
        .developer_instructions
        .as_deref()
        .expect("developer instructions should be present");
    assert!(developer_instructions.contains("Repo-specific root guidance."));
    assert!(developer_instructions.contains("You are the canonical root orchestrator"));
    assert!(developer_instructions.contains("Use this root dispatch playbook by default"));
}

#[tokio::test]
async fn apply_recon_role_sets_reasoning_effort_and_adds_session_flags_layer() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let before_layers = session_flags_layer_count(&config);

    apply_role_to_config(&mut config, Some("recon"))
        .await
        .expect("recon role should apply");

    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::Medium));
    assert_eq!(session_flags_layer_count(&config), before_layers + 1);
}

#[tokio::test]
async fn apply_auditor_role_sets_high_reasoning_effort_and_adds_session_flags_layer() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let before_layers = session_flags_layer_count(&config);

    apply_role_to_config(&mut config, Some("auditor"))
        .await
        .expect("auditor role should apply");

    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(session_flags_layer_count(&config), before_layers + 1);
}

#[tokio::test]
async fn apply_validator_role_sets_medium_reasoning_effort_and_adds_session_flags_layer() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let before_layers = session_flags_layer_count(&config);

    apply_role_to_config(&mut config, Some("validator"))
        .await
        .expect("validator role should apply");

    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::Medium));
    assert_eq!(session_flags_layer_count(&config), before_layers + 1);
}

#[tokio::test]
async fn apply_toolsmith_role_sets_medium_reasoning_effort_and_adds_session_flags_layer() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let before_layers = session_flags_layer_count(&config);

    apply_role_to_config(&mut config, Some("toolsmith"))
        .await
        .expect("toolsmith role should apply");

    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::Medium));
    assert_eq!(session_flags_layer_count(&config), before_layers + 1);
}

#[tokio::test]
async fn apply_verifier_role_sets_medium_reasoning_effort_and_adds_session_flags_layer() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let before_layers = session_flags_layer_count(&config);

    apply_role_to_config(&mut config, Some("verifier"))
        .await
        .expect("verifier role should apply");

    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::Medium));
    assert_eq!(session_flags_layer_count(&config), before_layers + 1);
}

#[tokio::test]
async fn apply_empty_explorer_role_preserves_current_model_and_reasoning_effort() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let before_layers = session_flags_layer_count(&config);
    config.model = Some("gpt-5.4-mini".to_string());
    config.model_reasoning_effort = Some(ReasoningEffort::High);

    apply_role_to_config(&mut config, Some("explorer"))
        .await
        .expect("explorer role should apply");

    assert_eq!(config.model.as_deref(), Some("gpt-5.4-mini"));
    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(session_flags_layer_count(&config), before_layers);
}

#[tokio::test]
async fn apply_role_returns_unavailable_for_missing_user_role_file() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(PathBuf::from("/path/does/not/exist.toml")),
            nickname_candidates: None,
        },
    );

    let err = apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect_err("missing role file should fail");

    assert_eq!(err, AGENT_TYPE_UNAVAILABLE_ERROR);
}

#[tokio::test]
async fn apply_role_returns_unavailable_for_invalid_user_role_toml() {
    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let role_path = write_role_config(&home, "invalid-role.toml", "model = [").await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    let err = apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect_err("invalid role file should fail");

    assert_eq!(err, AGENT_TYPE_UNAVAILABLE_ERROR);
}

#[tokio::test]
async fn apply_role_ignores_agent_metadata_fields_in_user_role_file() {
    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let role_path = write_role_config(
        &home,
        "metadata-role.toml",
        r#"
name = "archivist"
description = "Role metadata"
nickname_candidates = ["Hypatia"]
developer_instructions = "Stay focused"
model = "role-model"
"#,
    )
    .await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect("custom role should apply");

    assert_eq!(config.model.as_deref(), Some("role-model"));
}

#[tokio::test]
async fn apply_role_preserves_unspecified_keys() {
    let (home, mut config) = test_config_with_cli_overrides(vec![(
        "model".to_string(),
        TomlValue::String("base-model".to_string()),
    )])
    .await;
    config.codex_linux_sandbox_exe = Some(PathBuf::from("/tmp/codex-linux-sandbox"));
    config.main_execve_wrapper_exe = Some(PathBuf::from("/tmp/codex-execve-wrapper"));
    let role_path = write_role_config(
        &home,
        "effort-only.toml",
        "developer_instructions = \"Stay focused\"\nmodel_reasoning_effort = \"high\"",
    )
    .await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect("custom role should apply");

    assert_eq!(config.model.as_deref(), Some("base-model"));
    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(
        config.codex_linux_sandbox_exe,
        Some(PathBuf::from("/tmp/codex-linux-sandbox"))
    );
    assert_eq!(
        config.main_execve_wrapper_exe,
        Some(PathBuf::from("/tmp/codex-execve-wrapper"))
    );
}

#[tokio::test]
async fn apply_role_preserves_active_profile_and_model_provider() {
    let home = TempDir::new().expect("create temp dir");
    tokio::fs::write(
        home.path().join(CONFIG_TOML_FILE),
        r#"
[model_providers.test-provider]
name = "Test Provider"
base_url = "https://example.com/v1"
env_key = "TEST_PROVIDER_API_KEY"
wire_api = "responses"

[profiles.test-profile]
model_provider = "test-provider"
"#,
    )
    .await
    .expect("write config.toml");
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            config_profile: Some("test-profile".to_string()),
            ..Default::default()
        })
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await
        .expect("load config");
    let role_path = write_role_config(
        &home,
        "empty-role.toml",
        "developer_instructions = \"Stay focused\"",
    )
    .await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect("custom role should apply");

    assert_eq!(config.active_profile.as_deref(), Some("test-profile"));
    assert_eq!(config.model_provider_id, "test-provider");
    assert_eq!(config.model_provider.name, "Test Provider");
}

#[tokio::test]
async fn apply_role_top_level_profile_settings_override_preserved_profile() {
    let home = TempDir::new().expect("create temp dir");
    tokio::fs::write(
        home.path().join(CONFIG_TOML_FILE),
        r#"
[profiles.base-profile]
model = "profile-model"
model_reasoning_effort = "low"
model_reasoning_summary = "concise"
model_verbosity = "low"
"#,
    )
    .await
    .expect("write config.toml");
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            config_profile: Some("base-profile".to_string()),
            ..Default::default()
        })
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await
        .expect("load config");
    let role_path = write_role_config(
        &home,
        "top-level-profile-settings-role.toml",
        r#"developer_instructions = "Stay focused"
model = "role-model"
model_reasoning_effort = "high"
model_reasoning_summary = "detailed"
model_verbosity = "high"
"#,
    )
    .await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect("custom role should apply");

    assert_eq!(config.active_profile.as_deref(), Some("base-profile"));
    assert_eq!(config.model.as_deref(), Some("role-model"));
    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(
        config.model_reasoning_summary,
        Some(ReasoningSummary::Detailed)
    );
    assert_eq!(config.model_verbosity, Some(Verbosity::High));
}

#[tokio::test]
async fn apply_role_uses_role_profile_instead_of_current_profile() {
    let home = TempDir::new().expect("create temp dir");
    tokio::fs::write(
        home.path().join(CONFIG_TOML_FILE),
        r#"
[model_providers.base-provider]
name = "Base Provider"
base_url = "https://base.example.com/v1"
env_key = "BASE_PROVIDER_API_KEY"
wire_api = "responses"

[model_providers.role-provider]
name = "Role Provider"
base_url = "https://role.example.com/v1"
env_key = "ROLE_PROVIDER_API_KEY"
wire_api = "responses"

[profiles.base-profile]
model_provider = "base-provider"

[profiles.role-profile]
model_provider = "role-provider"
"#,
    )
    .await
    .expect("write config.toml");
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            config_profile: Some("base-profile".to_string()),
            ..Default::default()
        })
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await
        .expect("load config");
    let role_path = write_role_config(
        &home,
        "profile-role.toml",
        "developer_instructions = \"Stay focused\"\nprofile = \"role-profile\"",
    )
    .await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect("custom role should apply");

    assert_eq!(config.active_profile.as_deref(), Some("role-profile"));
    assert_eq!(config.model_provider_id, "role-provider");
    assert_eq!(config.model_provider.name, "Role Provider");
}

#[tokio::test]
async fn apply_role_uses_role_model_provider_instead_of_current_profile_provider() {
    let home = TempDir::new().expect("create temp dir");
    tokio::fs::write(
        home.path().join(CONFIG_TOML_FILE),
        r#"
[model_providers.base-provider]
name = "Base Provider"
base_url = "https://base.example.com/v1"
env_key = "BASE_PROVIDER_API_KEY"
wire_api = "responses"

[model_providers.role-provider]
name = "Role Provider"
base_url = "https://role.example.com/v1"
env_key = "ROLE_PROVIDER_API_KEY"
wire_api = "responses"

[profiles.base-profile]
model_provider = "base-provider"
"#,
    )
    .await
    .expect("write config.toml");
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            config_profile: Some("base-profile".to_string()),
            ..Default::default()
        })
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await
        .expect("load config");
    let role_path = write_role_config(
        &home,
        "provider-role.toml",
        "developer_instructions = \"Stay focused\"\nmodel_provider = \"role-provider\"",
    )
    .await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect("custom role should apply");

    assert_eq!(config.active_profile, None);
    assert_eq!(config.model_provider_id, "role-provider");
    assert_eq!(config.model_provider.name, "Role Provider");
}

#[tokio::test]
async fn apply_role_uses_active_profile_model_provider_update() {
    let home = TempDir::new().expect("create temp dir");
    tokio::fs::write(
        home.path().join(CONFIG_TOML_FILE),
        r#"
[model_providers.base-provider]
name = "Base Provider"
base_url = "https://base.example.com/v1"
env_key = "BASE_PROVIDER_API_KEY"
wire_api = "responses"

[model_providers.role-provider]
name = "Role Provider"
base_url = "https://role.example.com/v1"
env_key = "ROLE_PROVIDER_API_KEY"
wire_api = "responses"

[profiles.base-profile]
model_provider = "base-provider"
model_reasoning_effort = "low"
"#,
    )
    .await
    .expect("write config.toml");
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            config_profile: Some("base-profile".to_string()),
            ..Default::default()
        })
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await
        .expect("load config");
    let role_path = write_role_config(
        &home,
        "profile-edit-role.toml",
        r#"developer_instructions = "Stay focused"

[profiles.base-profile]
model_provider = "role-provider"
model_reasoning_effort = "high"
"#,
    )
    .await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect("custom role should apply");

    assert_eq!(config.active_profile.as_deref(), Some("base-profile"));
    assert_eq!(config.model_provider_id, "role-provider");
    assert_eq!(config.model_provider.name, "Role Provider");
    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::High));
}

#[tokio::test]
#[cfg(not(windows))]
async fn apply_role_does_not_materialize_default_sandbox_workspace_write_fields() {
    use codex_protocol::protocol::SandboxPolicy;
    let (home, mut config) = test_config_with_cli_overrides(vec![
        (
            "sandbox_mode".to_string(),
            TomlValue::String("workspace-write".to_string()),
        ),
        (
            "sandbox_workspace_write.network_access".to_string(),
            TomlValue::Boolean(true),
        ),
    ])
    .await;
    let role_path = write_role_config(
        &home,
        "sandbox-role.toml",
        r#"developer_instructions = "Stay focused"

[sandbox_workspace_write]
writable_roots = ["./sandbox-root"]
"#,
    )
    .await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect("custom role should apply");

    let role_layer = config
        .config_layer_stack
        .get_layers(
            ConfigLayerStackOrdering::LowestPrecedenceFirst,
            /*include_disabled*/ true,
        )
        .into_iter()
        .rfind(|layer| layer.name == ConfigLayerSource::SessionFlags)
        .expect("expected a session flags layer");
    let sandbox_workspace_write = role_layer
        .config
        .get("sandbox_workspace_write")
        .and_then(TomlValue::as_table)
        .expect("role layer should include sandbox_workspace_write");
    assert_eq!(
        sandbox_workspace_write.contains_key("network_access"),
        false
    );
    assert_eq!(
        sandbox_workspace_write.contains_key("exclude_tmpdir_env_var"),
        false
    );
    assert_eq!(
        sandbox_workspace_write.contains_key("exclude_slash_tmp"),
        false
    );

    match &*config.permissions.sandbox_policy {
        SandboxPolicy::WorkspaceWrite { network_access, .. } => {
            assert_eq!(*network_access, true);
        }
        other => panic!("expected workspace-write sandbox policy, got {other:?}"),
    }
}

#[tokio::test]
async fn apply_role_takes_precedence_over_existing_session_flags_for_same_key() {
    let (home, mut config) = test_config_with_cli_overrides(vec![(
        "model".to_string(),
        TomlValue::String("cli-model".to_string()),
    )])
    .await;
    let before_layers = session_flags_layer_count(&config);
    let role_path = write_role_config(
        &home,
        "model-role.toml",
        "developer_instructions = \"Stay focused\"\nmodel = \"role-model\"",
    )
    .await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect("custom role should apply");

    assert_eq!(config.model.as_deref(), Some("role-model"));
    assert_eq!(session_flags_layer_count(&config), before_layers + 1);
}

#[cfg_attr(windows, ignore)]
#[tokio::test]
async fn apply_role_skills_config_disables_skill_for_spawned_agent() {
    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let skill_dir = home.path().join("skills").join("demo");
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    let skill_path = skill_dir.join("SKILL.md");
    fs::write(
        &skill_path,
        "---\nname: demo-skill\ndescription: demo description\n---\n\n# Body\n",
    )
    .expect("write skill");
    let role_path = write_role_config(
        &home,
        "skills-role.toml",
        &format!(
            r#"developer_instructions = "Stay focused"

[[skills.config]]
path = "{}"
enabled = false
"#,
            skill_path.display()
        ),
    )
    .await;
    config.agent_roles.insert(
        "custom".to_string(),
        AgentRoleConfig {
            description: None,
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );

    apply_role_to_config(&mut config, Some("custom"))
        .await
        .expect("custom role should apply");

    let plugins_manager = Arc::new(PluginsManager::new(home.path().to_path_buf()));
    let skills_manager =
        SkillsManager::new(home.path().abs(), /*bundled_skills_enabled*/ true);
    let plugin_outcome = plugins_manager.plugins_for_config(&config).await;
    let effective_skill_roots = plugin_outcome.effective_skill_roots();
    let skills_input = skills_load_input_from_config(&config, effective_skill_roots);
    let outcome = skills_manager
        .skills_for_config(&skills_input, Some(codex_exec_server::LOCAL_FS.clone()))
        .await;
    let skill = outcome
        .skills
        .iter()
        .find(|skill| skill.name == "demo-skill")
        .expect("demo skill should be discovered");

    assert_eq!(outcome.is_skill_enabled(skill), false);
}

#[test]
fn spawn_tool_spec_build_deduplicates_user_defined_built_in_roles() {
    let user_defined_roles = BTreeMap::from([
        (
            "explorer".to_string(),
            AgentRoleConfig {
                description: Some("user override".to_string()),
                config_file: None,
                nickname_candidates: None,
            },
        ),
        ("researcher".to_string(), AgentRoleConfig::default()),
    ]);

    let spec = spawn_tool_spec::build(&user_defined_roles);

    assert!(spec.contains("researcher: no description"));
    assert!(spec.contains("explorer: {\nuser override\n}"));
    assert!(spec.contains("default: {\nLegacy alias for `worker`; retained for compatibility.\n}"));
    assert!(!spec.contains("orchestrator: {"));
    assert!(!spec.contains("Explorers are fast and authoritative."));
}

#[test]
fn spawn_tool_spec_lists_user_defined_roles_before_built_ins() {
    let user_defined_roles = BTreeMap::from([(
        "aaa".to_string(),
        AgentRoleConfig {
            description: Some("first".to_string()),
            config_file: None,
            nickname_candidates: None,
        },
    )]);

    let spec = spawn_tool_spec::build(&user_defined_roles);
    let user_index = spec.find("aaa: {\nfirst\n}").expect("find user role");
    let built_in_index = spec
        .find("default: {\nLegacy alias for `worker`; retained for compatibility.\n}")
        .expect("find built-in role");

    assert!(user_index < built_in_index);
}

#[test]
fn spawn_tool_spec_lists_legacy_and_offensive_roles() {
    let spec = spawn_tool_spec::build(&BTreeMap::new());

    assert!(spec.contains("explorer: {\nUse `explorer` for specific codebase questions."));
    assert!(spec.contains("worker: {\nUse for execution and production work."));
    assert!(spec.contains("Optional type name for the new agent. If omitted, `worker` is used."));
    assert!(!spec.contains("orchestrator: {\nUse `orchestrator` as the default root role."));
    assert!(spec.contains("recon: {\nUse `recon` for coverage-first attack-surface mapping."));
    assert!(
        spec.contains(
            "auditor: {\nUse `auditor` for whitebox review of code, docs, JS, and schemas."
        )
    );
    assert!(spec.contains(
        "validator: {\nUse `validator` to confirm exploitability and reduce false positives."
    ));
    assert!(spec.contains(
        "verifier: {\nUse `verifier` for command-driven implementation and runtime verification."
    ));
    assert!(spec.contains(
        "toolsmith: {\nUse `toolsmith` to build offensive helpers that speed up investigation."
    ));
    assert!(spec.contains("plugin-backed browser or proxy discovery when available"));
    assert!(spec.contains("do not take checkpoint ownership or reprioritize the engagement"));
    assert!(spec.contains("prefer plugin-backed browser or proxy discovery when available"));
    assert!(spec.contains("receive bounded task context only"));
    assert!(spec.contains("return observed surface, uncertainty, blockers, exit status, and the next justified action to root"));
    assert!(spec.contains("return evidence, uncertainty, blockers, exit status, and a prove, chain, or drop recommendation to root"));
    assert!(spec.contains("They report one check at a time with command, observed output, result, and end with `VERDICT: PASS`, `VERDICT: FAIL`, or `VERDICT: PARTIAL`."));
    assert!(spec.contains("interoperate cleanly with plugin-native artifacts and refs"));
    assert!(spec.contains("Implement part of a feature"));
    assert!(spec.contains("Fix tests or bugs"));
}

#[test]
fn spawn_tool_spec_marks_role_locked_model_and_reasoning_effort() {
    let tempdir = TempDir::new().expect("create temp dir");
    let role_path = tempdir.path().join("researcher.toml");
    fs::write(
            &role_path,
            "developer_instructions = \"Research carefully\"\nmodel = \"gpt-5\"\nmodel_reasoning_effort = \"high\"\n",
        )
        .expect("write role config");
    let user_defined_roles = BTreeMap::from([(
        "researcher".to_string(),
        AgentRoleConfig {
            description: Some("Research carefully.".to_string()),
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    )]);

    let spec = spawn_tool_spec::build(&user_defined_roles);

    assert!(spec.contains(
            "Research carefully.\n- This role's model is set to `gpt-5` and its reasoning effort is set to `high`. These settings cannot be changed."
        ));
}

#[test]
fn spawn_tool_spec_marks_role_locked_reasoning_effort_only() {
    let tempdir = TempDir::new().expect("create temp dir");
    let role_path = tempdir.path().join("reviewer.toml");
    fs::write(
        &role_path,
        "developer_instructions = \"Review carefully\"\nmodel_reasoning_effort = \"medium\"\n",
    )
    .expect("write role config");
    let user_defined_roles = BTreeMap::from([(
        "reviewer".to_string(),
        AgentRoleConfig {
            description: Some("Review carefully.".to_string()),
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    )]);

    let spec = spawn_tool_spec::build(&user_defined_roles);

    assert!(spec.contains(
            "Review carefully.\n- This role's reasoning effort is set to `medium` and cannot be changed."
        ));
}

#[test]
fn built_in_config_file_contents_resolves_offensive_root_role_files() {
    let explorer = built_in::config_file_contents(Path::new("explorer.toml"))
        .expect("explorer role file should resolve");
    let auditor = built_in::config_file_contents(Path::new("auditor.toml"))
        .expect("auditor role file should resolve");
    let orchestrator = built_in::config_file_contents(Path::new("orchestrator.toml"))
        .expect("orchestrator role file should resolve");
    let recon =
        built_in::config_file_contents(Path::new("recon.toml")).expect("recon should resolve");
    let toolsmith = built_in::config_file_contents(Path::new("toolsmith.toml"))
        .expect("toolsmith should resolve");
    let validator = built_in::config_file_contents(Path::new("validator.toml"))
        .expect("validator should resolve");
    let verifier = built_in::config_file_contents(Path::new("verifier.toml"))
        .expect("verifier should resolve");
    let worker =
        built_in::config_file_contents(Path::new("worker.toml")).expect("worker should resolve");

    assert!(explorer.is_empty());
    assert!(auditor.contains("You are an auditor."));
    assert!(auditor.contains("live user prompt stream"));
    assert!(
        auditor.contains(
            "Do not broaden scope, own checkpoint state, or reprioritize the engagement."
        )
    );
    assert!(
        orchestrator.contains(
            "You are the canonical root orchestrator for authorized offensive appsec work."
        )
    );
    assert!(orchestrator.contains("owns canonical root-session coordination"));
    assert!(orchestrator.contains("Own session-level synthesis"));
    assert!(
        orchestrator
            .contains("Prefer the `reddex-plugin` offensive MCP stack when it is available.")
    );
    assert!(orchestrator.contains("browser actions through `bb-browser`"));
    assert!(orchestrator.contains("Use `validator` for exploitability proof"));
    assert!(
        orchestrator.contains(
            "Use `verifier` after non-trivial worker or toolsmith implementation changes"
        )
    );
    assert!(orchestrator.contains("Do not treat a forked child as a second root"));
    assert!(recon.contains("You are a recon specialist."));
    assert!(recon.contains("live user prompt stream"));
    assert!(recon.contains("`bb-browser`"));
    assert!(recon.contains("Return results in this contract when practical:"));
    assert!(recon.contains("- OBSERVED SURFACE"));
    assert!(recon.contains("- NEXT ACTION"));
    assert!(auditor.contains("Return results in this contract when practical:"));
    assert!(auditor.contains("- HYPOTHESES"));
    assert!(auditor.contains("- NEXT ACTION"));
    assert!(auditor.contains("`bb-codeintel`"));
    assert!(toolsmith.contains("You are a toolsmith."));
    assert!(toolsmith.contains("live user prompt stream"));
    assert!(toolsmith.contains("consume or emit plugin-native artifacts"));
    assert!(
        toolsmith.contains(
            "Do not broaden scope, own checkpoint state, or reprioritize the engagement."
        )
    );
    assert!(toolsmith.contains("Return results in this contract when practical:"));
    assert!(toolsmith.contains("- HELPER"));
    assert!(toolsmith.contains("- NEXT ACTION"));
    assert!(validator.contains("You are a validator."));
    assert!(validator.contains("live user prompt stream"));
    assert!(validator.contains("Return results in this contract when practical:"));
    assert!(validator.contains("- EVIDENCE"));
    assert!(validator.contains("- RECOMMENDATION"));
    assert!(verifier.contains("You are a verifier."));
    assert!(
        verifier.contains(
            "verify behavior by running checks, not by reading code and declaring success"
        )
    );
    assert!(worker.contains("You are a worker."));
    assert!(worker.contains("Execute one bounded task at a time."));
    assert!(worker.contains("Default to bounded offensive support work that root already scoped"));
    assert!(
        worker.contains("Stay inside the authorized target slice and task boundary root assigned.")
    );
    assert!(worker.contains("Do not take checkpoint ownership, session synthesis ownership, or delegation strategy ownership."));
    assert!(verifier.contains("Do not edit project files."));
    assert!(verifier.contains("VERDICT: PASS"));
    assert_eq!(
        built_in::config_file_contents(Path::new("missing.toml")),
        None
    );
}
