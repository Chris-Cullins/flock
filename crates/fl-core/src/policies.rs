use std::fs;
use std::path::Path;

use fl_policy::{
    BudgetExceedAction, BudgetLimits, RateLimitExceedAction, RateLimits, ScopeEnforceMode,
    ScopeMode, ScopePolicy,
};

/// Configuration for commit hygiene enforcement.
#[derive(Debug, Clone)]
pub struct CommitHygieneConfig {
    pub enabled: bool,
    pub require_category: bool,
    pub require_scope: bool,
    pub require_confidence: bool,
    /// Maximum seconds between checkpoints before a warning is emitted.
    pub max_time_between_checkpoints: Option<u64>,
}

impl Default for CommitHygieneConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            require_category: false,
            require_scope: false,
            require_confidence: false,
            max_time_between_checkpoints: None,
        }
    }
}

/// Aggregated policy configuration loaded from `.flock/policies.toml`.
#[derive(Debug, Clone)]
pub struct PoliciesConfig {
    pub scope: ScopePolicy,
    pub budget: BudgetLimits,
    pub rate_limits: RateLimits,
    pub commit_hygiene: CommitHygieneConfig,
}

impl Default for PoliciesConfig {
    fn default() -> Self {
        Self {
            scope: ScopePolicy::default(),
            budget: BudgetLimits::default(),
            rate_limits: RateLimits::default(),
            commit_hygiene: CommitHygieneConfig::default(),
        }
    }
}

/// Default content for a new `.flock/policies.toml` file.
pub const DEFAULT_POLICIES_TOML: &str = r#"# Agent governance policies for Flock.
# These policies constrain what agents can do within the repository.

# --- Scope enforcement ---
# Restrict agents to only modify files within their task's allowed paths.
[scope]
enabled = false
# How scope violations are handled: "block", "gate", or "split"
enforce_mode = "block"
# How scope is defined: "path" (glob matching), "semantic", or "module"
scope_mode = "path"

# --- Change budget limits ---
# Limit how much an agent can change per task or exploration.
[budget]
enabled = false
# max_files_per_task = 20
# max_files_per_exploration = 10
# max_lines_per_task = 1000
# Action when budget is exceeded: "block", "warn", or "pause_and_flag"
on_exceed = "block"

# --- Rate limits & runaway prevention ---
# Prevent agents from running away with too many operations.
[rate_limits]
enabled = false
# max_explorations_per_task = 10
# max_undos_per_exploration = 5
# max_checkpoints_per_window = 50
# Time window in seconds (default: 1 hour)
# window_secs = 3600
# Action when rate limit is exceeded: "block", "warn", or "pause_and_escalate"
on_exceed = "block"

# --- Commit hygiene ---
# Require structured metadata on checkpoints for agent governance.
[commit_hygiene]
enabled = false
# require_category = true
# require_scope = true
# require_confidence = true
# max_time_between_checkpoints = 3600
"#;

/// Parse a `policies.toml` file into a `PoliciesConfig`.
pub fn parse_policies_config(content: &str) -> PoliciesConfig {
    let mut config = PoliciesConfig::default();
    let mut current_section = "";

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Section headers.
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current_section = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            continue;
        }

        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        match current_section {
            "scope" => parse_scope_field(&mut config.scope, key, value),
            "budget" => parse_budget_field(&mut config.budget, key, value),
            "rate_limits" => parse_rate_limit_field(&mut config.rate_limits, key, value),
            "commit_hygiene" => parse_commit_hygiene_field(&mut config.commit_hygiene, key, value),
            _ => {}
        }
    }

    config
}

fn parse_scope_field(scope: &mut ScopePolicy, key: &str, value: &str) {
    match key {
        "enabled" => scope.enabled = value == "true",
        "enforce_mode" => {
            scope.enforce_mode = match parse_toml_string(value).as_str() {
                "gate" => ScopeEnforceMode::Gate,
                "split" => ScopeEnforceMode::Split,
                _ => ScopeEnforceMode::Block,
            };
        }
        "scope_mode" => {
            scope.scope_mode = match parse_toml_string(value).as_str() {
                "semantic" => ScopeMode::Semantic,
                "module" => ScopeMode::Module,
                _ => ScopeMode::Path,
            };
        }
        _ => {}
    }
}

fn parse_budget_field(budget: &mut BudgetLimits, key: &str, value: &str) {
    match key {
        "enabled" => budget.enabled = value == "true",
        "max_files_per_task" => budget.max_files_per_task = value.parse().ok(),
        "max_files_per_exploration" => budget.max_files_per_exploration = value.parse().ok(),
        "max_lines_per_task" => budget.max_lines_per_task = value.parse().ok(),
        "on_exceed" => {
            budget.on_exceed = match parse_toml_string(value).as_str() {
                "warn" => BudgetExceedAction::Warn,
                "pause_and_flag" => BudgetExceedAction::PauseAndFlag,
                _ => BudgetExceedAction::Block,
            };
        }
        _ => {}
    }
}

fn parse_rate_limit_field(limits: &mut RateLimits, key: &str, value: &str) {
    match key {
        "enabled" => limits.enabled = value == "true",
        "max_explorations_per_task" => limits.max_explorations_per_task = value.parse().ok(),
        "max_undos_per_exploration" => limits.max_undos_per_exploration = value.parse().ok(),
        "max_checkpoints_per_window" => limits.max_checkpoints_per_window = value.parse().ok(),
        "window_secs" => {
            if let Ok(secs) = value.parse::<u64>() {
                limits.window_secs = secs;
            } else {
                // Try parsing duration strings like "30m", "1h".
                if let Some(secs) = parse_duration_to_secs(value) {
                    limits.window_secs = secs;
                }
            }
        }
        "on_exceed" => {
            limits.on_exceed = match parse_toml_string(value).as_str() {
                "warn" => RateLimitExceedAction::Warn,
                "pause_and_escalate" => RateLimitExceedAction::PauseAndEscalate,
                _ => RateLimitExceedAction::Block,
            };
        }
        _ => {}
    }
}

fn parse_commit_hygiene_field(config: &mut CommitHygieneConfig, key: &str, value: &str) {
    match key {
        "enabled" => config.enabled = value == "true",
        "require_category" => config.require_category = value == "true",
        "require_scope" => config.require_scope = value == "true",
        "require_confidence" => config.require_confidence = value == "true",
        "max_time_between_checkpoints" => {
            if let Ok(secs) = value.parse::<u64>() {
                config.max_time_between_checkpoints = Some(secs);
            } else if let Some(secs) = parse_duration_to_secs(value) {
                config.max_time_between_checkpoints = Some(secs);
            }
        }
        _ => {}
    }
}

/// Parse a TOML string value (strip surrounding quotes).
fn parse_toml_string(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Parse a duration string like "30m", "1h", "2d" into seconds.
pub fn parse_duration_to_secs(s: &str) -> Option<u64> {
    let s = parse_toml_string(s);
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let (num_str, suffix) = if s.ends_with('s') {
        (&s[..s.len() - 1], "s")
    } else if s.ends_with('m') {
        (&s[..s.len() - 1], "m")
    } else if s.ends_with('h') {
        (&s[..s.len() - 1], "h")
    } else if s.ends_with('d') {
        (&s[..s.len() - 1], "d")
    } else {
        return s.parse().ok();
    };

    let num: u64 = num_str.parse().ok()?;
    let multiplier = match suffix {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    Some(num * multiplier)
}

/// Load policies config from `.flock/policies.toml`, falling back to defaults.
pub fn load_policies_config(root: &Path) -> PoliciesConfig {
    let config_path = root.join(".flock/policies.toml");
    match fs::read_to_string(&config_path) {
        Ok(content) => parse_policies_config(&content),
        Err(_) => PoliciesConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = PoliciesConfig::default();
        assert!(!config.scope.enabled);
        assert!(!config.budget.enabled);
        assert!(!config.rate_limits.enabled);
    }

    #[test]
    fn test_parse_default_toml() {
        let config = parse_policies_config(DEFAULT_POLICIES_TOML);
        assert!(!config.scope.enabled);
        assert_eq!(config.scope.enforce_mode, ScopeEnforceMode::Block);
        assert!(!config.budget.enabled);
        assert_eq!(config.budget.on_exceed, BudgetExceedAction::Block);
        assert!(!config.rate_limits.enabled);
        assert_eq!(config.rate_limits.on_exceed, RateLimitExceedAction::Block);
    }

    #[test]
    fn test_parse_enabled_scope() {
        let toml = r#"
[scope]
enabled = true
enforce_mode = "gate"
scope_mode = "path"
"#;
        let config = parse_policies_config(toml);
        assert!(config.scope.enabled);
        assert_eq!(config.scope.enforce_mode, ScopeEnforceMode::Gate);
        assert_eq!(config.scope.scope_mode, ScopeMode::Path);
    }

    #[test]
    fn test_parse_budget_with_limits() {
        let toml = r#"
[budget]
enabled = true
max_files_per_task = 20
max_files_per_exploration = 10
max_lines_per_task = 1000
on_exceed = "warn"
"#;
        let config = parse_policies_config(toml);
        assert!(config.budget.enabled);
        assert_eq!(config.budget.max_files_per_task, Some(20));
        assert_eq!(config.budget.max_files_per_exploration, Some(10));
        assert_eq!(config.budget.max_lines_per_task, Some(1000));
        assert_eq!(config.budget.on_exceed, BudgetExceedAction::Warn);
    }

    #[test]
    fn test_parse_rate_limits() {
        let toml = r#"
[rate_limits]
enabled = true
max_explorations_per_task = 5
max_undos_per_exploration = 3
max_checkpoints_per_window = 50
window_secs = 7200
on_exceed = "pause_and_escalate"
"#;
        let config = parse_policies_config(toml);
        assert!(config.rate_limits.enabled);
        assert_eq!(config.rate_limits.max_explorations_per_task, Some(5));
        assert_eq!(config.rate_limits.max_undos_per_exploration, Some(3));
        assert_eq!(config.rate_limits.max_checkpoints_per_window, Some(50));
        assert_eq!(config.rate_limits.window_secs, 7200);
        assert_eq!(
            config.rate_limits.on_exceed,
            RateLimitExceedAction::PauseAndEscalate
        );
    }

    #[test]
    fn test_parse_duration_to_secs() {
        assert_eq!(parse_duration_to_secs("30m"), Some(1800));
        assert_eq!(parse_duration_to_secs("1h"), Some(3600));
        assert_eq!(parse_duration_to_secs("2d"), Some(172800));
        assert_eq!(parse_duration_to_secs("60s"), Some(60));
        assert_eq!(parse_duration_to_secs("3600"), Some(3600));
        assert_eq!(parse_duration_to_secs(""), None);
    }

    #[test]
    fn test_parse_commit_hygiene() {
        let toml = r#"
[commit_hygiene]
enabled = true
require_category = true
require_scope = true
require_confidence = false
max_time_between_checkpoints = 1800
"#;
        let config = parse_policies_config(toml);
        assert!(config.commit_hygiene.enabled);
        assert!(config.commit_hygiene.require_category);
        assert!(config.commit_hygiene.require_scope);
        assert!(!config.commit_hygiene.require_confidence);
        assert_eq!(config.commit_hygiene.max_time_between_checkpoints, Some(1800));
    }

    #[test]
    fn test_parse_combined_config() {
        let toml = r#"
[scope]
enabled = true
enforce_mode = "split"

[budget]
enabled = true
max_files_per_task = 15
on_exceed = "pause_and_flag"

[rate_limits]
enabled = false
"#;
        let config = parse_policies_config(toml);
        assert!(config.scope.enabled);
        assert_eq!(config.scope.enforce_mode, ScopeEnforceMode::Split);
        assert!(config.budget.enabled);
        assert_eq!(config.budget.max_files_per_task, Some(15));
        assert_eq!(config.budget.on_exceed, BudgetExceedAction::PauseAndFlag);
        assert!(!config.rate_limits.enabled);
    }
}
