use std::fs;
use std::path::Path;

use fl_policy::{
    AntiPatternConfig, AntiPatternEnforce, AntiPatternRule, ArchRuleEnforce, ArchitectureRules,
    BudgetExceedAction, BudgetLimits, DependencyDirectionRule, DependencyEnforce,
    DependencyPolicy, LayerBoundaryRule, RateLimitExceedAction, RateLimits, RegressionConfig,
    ReuseEnforce, ReusePolicy, RollbackConfig, ScopeEnforceMode, ScopeMode, ScopePolicy,
    TestFailureAction, TestRequirements,
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
    pub test_requirements: TestRequirements,
    pub architecture: ArchitectureRules,
    pub anti_patterns: AntiPatternConfig,
    pub reuse: ReusePolicy,
    pub dependencies: DependencyPolicy,
    pub regression: RegressionConfig,
    pub rollback: RollbackConfig,
}

impl Default for PoliciesConfig {
    fn default() -> Self {
        Self {
            scope: ScopePolicy::default(),
            budget: BudgetLimits::default(),
            rate_limits: RateLimits::default(),
            commit_hygiene: CommitHygieneConfig::default(),
            test_requirements: TestRequirements::default(),
            architecture: ArchitectureRules::default(),
            anti_patterns: AntiPatternConfig::default(),
            reuse: ReusePolicy::default(),
            dependencies: DependencyPolicy::default(),
            regression: RegressionConfig::default(),
            rollback: RollbackConfig::default(),
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
# max_semantic_changes_per_exploration = 50
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

# --- Test requirements ---
# Require tests to pass before exploration promotion.
[test_requirements]
enabled = false
# require_passing = true
# test_command = "cargo test"
# require_new_tests = false
# on_failure = "block"

# --- Architecture rules ---
# Enforce layer boundaries and dependency direction.
# Rules are defined in .flock/arch-rules.toml (see below for inline format).
[architecture]
enabled = false
# enforce = "block"  # "block", "gate", or "warn"

# --- Anti-pattern detection ---
# Detect domain-specific anti-patterns in file contents.
# Rules are defined in .flock/anti-patterns.toml (see below for inline format).
[anti_patterns]
enabled = false
# enforce = "block_with_explanation"  # "block_with_explanation", "gate", or "warn"

# --- DRY / Duplication prevention ---
# Detect duplicate code by comparing new symbols against existing ones.
# Protected domains with stricter thresholds are defined in .flock/reuse-domains.toml.
[reuse]
enabled = false
# enforce = "gate"  # "block", "gate", or "warn"
# similarity_threshold = 0.8
# check_signatures = true
# check_bodies = true
# check_patterns = true

# --- Dependency & compatibility checks ---
# Enforce an approved package allowlist, block forbidden licenses, and scan for vulnerabilities.
# Approved packages are defined in .flock/approved-deps.toml.
[dependencies]
enabled = false
# enforce = "block"  # "block", "gate", or "warn"
# license_blocklist = ["GPL-3.0", "AGPL-3.0"]
# vuln_check = true
# consumer_test_command = ""

# --- Regression detection ---
# Monitor for test failures and benchmark regressions after merge/promotion.
[regression]
enabled = false
# monitor_after_merge = true
# monitor_window = "1h"
# benchmark_threshold = 0.1

# --- Automatic rollback ---
# Automatically revert merged changes when regressions are detected.
[rollback]
enabled = false
# auto_rollback = true
# rollback_on_test_failure = true
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
            "test_requirements" => parse_test_requirements_field(&mut config.test_requirements, key, value),
            "architecture" => parse_architecture_field(&mut config.architecture, key, value),
            "anti_patterns" => parse_anti_patterns_field(&mut config.anti_patterns, key, value),
            "reuse" => parse_reuse_field(&mut config.reuse, key, value),
            "dependencies" => parse_dependency_field(&mut config.dependencies, key, value),
            "regression" => parse_regression_field(&mut config.regression, key, value),
            "rollback" => parse_rollback_field(&mut config.rollback, key, value),
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
        "max_semantic_changes_per_exploration" => budget.max_semantic_changes_per_exploration = value.parse().ok(),
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

fn parse_test_requirements_field(config: &mut TestRequirements, key: &str, value: &str) {
    match key {
        "enabled" => config.enabled = value == "true",
        "require_passing" => config.require_passing = value == "true",
        "test_command" => config.test_command = parse_toml_string(value),
        "require_new_tests" => config.require_new_tests = value == "true",
        "min_coverage_percent" => config.min_coverage_percent = value.parse().ok(),
        "on_failure" => {
            config.on_failure = match parse_toml_string(value).as_str() {
                "warn" => TestFailureAction::Warn,
                "gate" => TestFailureAction::Gate,
                _ => TestFailureAction::Block,
            };
        }
        _ => {}
    }
}

fn parse_architecture_field(config: &mut ArchitectureRules, key: &str, value: &str) {
    match key {
        "enabled" => config.enabled = value == "true",
        "enforce" => {
            config.enforce = match parse_toml_string(value).as_str() {
                "gate" => ArchRuleEnforce::Gate,
                "warn" => ArchRuleEnforce::Warn,
                _ => ArchRuleEnforce::Block,
            };
        }
        _ => {}
    }
}

fn parse_anti_patterns_field(config: &mut AntiPatternConfig, key: &str, value: &str) {
    match key {
        "enabled" => config.enabled = value == "true",
        "enforce" => {
            config.enforce = match parse_toml_string(value).as_str() {
                "gate" => AntiPatternEnforce::Gate,
                "warn" => AntiPatternEnforce::Warn,
                _ => AntiPatternEnforce::BlockWithExplanation,
            };
        }
        _ => {}
    }
}

fn parse_reuse_field(config: &mut ReusePolicy, key: &str, value: &str) {
    match key {
        "enabled" => config.enabled = value == "true",
        "enforce" => {
            config.enforce = match parse_toml_string(value).as_str() {
                "block" => ReuseEnforce::Block,
                "warn" => ReuseEnforce::Warn,
                _ => ReuseEnforce::Gate,
            };
        }
        "similarity_threshold" => {
            if let Ok(v) = value.parse::<f64>() {
                config.similarity_threshold = v.clamp(0.0, 1.0);
            }
        }
        "check_signatures" => config.check_signatures = value == "true",
        "check_bodies" => config.check_bodies = value == "true",
        "check_patterns" => config.check_patterns = value == "true",
        _ => {}
    }
}

/// Parse `.flock/reuse-domains.toml` — protected domains with stricter thresholds:
///
/// ```toml
/// [domain.finance]
/// file_pattern = "src/finance/**"
/// similarity_threshold = 0.5
///
/// [domain.compliance]
/// file_pattern = "src/compliance/**"
/// similarity_threshold = 0.4
/// ```
pub fn parse_reuse_domains(content: &str) -> Vec<fl_policy::ProtectedDomain> {
    let mut domains = Vec::new();
    let mut in_domain = false;
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    let flush =
        |fields: &std::collections::HashMap<String, String>,
         domains: &mut Vec<fl_policy::ProtectedDomain>| {
            let file_pattern = match fields.get("file_pattern") {
                Some(p) if !p.is_empty() => p.clone(),
                _ => return,
            };
            let threshold = fields
                .get("similarity_threshold")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.5)
                .clamp(0.0, 1.0);
            domains.push(fl_policy::ProtectedDomain {
                file_pattern,
                similarity_threshold: threshold,
            });
        };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_domain {
                flush(&fields, &mut domains);
            }
            let section = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            in_domain = section.starts_with("domain.");
            fields.clear();
            continue;
        }

        if in_domain {
            if let Some((key, value)) = trimmed.split_once('=') {
                fields.insert(key.trim().to_string(), parse_toml_string(value.trim()));
            }
        }
    }
    if in_domain {
        flush(&fields, &mut domains);
    }

    domains
}

fn parse_dependency_field(config: &mut DependencyPolicy, key: &str, value: &str) {
    match key {
        "enabled" => config.enabled = value == "true",
        "enforce" => {
            config.enforce = match parse_toml_string(value).as_str() {
                "gate" => DependencyEnforce::Gate,
                "warn" => DependencyEnforce::Warn,
                _ => DependencyEnforce::Block,
            };
        }
        "vuln_check" => config.vuln_check = value == "true",
        "consumer_test_command" => config.consumer_test_command = parse_toml_string(value),
        "license_blocklist" => {
            // Parse as comma-separated or TOML array-like: ["GPL-3.0", "AGPL-3.0"]
            let s = parse_toml_string(value);
            let s = s.trim_start_matches('[').trim_end_matches(']');
            config.license_blocklist = s
                .split(',')
                .map(|item| {
                    let item = item.trim();
                    let item = item.trim_matches('"').trim_matches('\'');
                    item.to_string()
                })
                .filter(|s| !s.is_empty())
                .collect();
        }
        _ => {}
    }
}

fn parse_regression_field(config: &mut RegressionConfig, key: &str, value: &str) {
    match key {
        "enabled" => config.enabled = value == "true",
        "monitor_after_merge" => config.monitor_after_merge = value == "true",
        "monitor_window" => {
            if let Some(secs) = parse_duration_to_secs(value) {
                config.monitor_window_secs = secs;
            }
        }
        "benchmark_threshold" => {
            if let Ok(v) = parse_toml_string(value).parse::<f64>() {
                config.benchmark_threshold = v.clamp(0.0, 1.0);
            }
        }
        _ => {}
    }
}

fn parse_rollback_field(config: &mut RollbackConfig, key: &str, value: &str) {
    match key {
        "enabled" => config.enabled = value == "true",
        "auto_rollback" => config.auto_rollback = value == "true",
        "rollback_on_test_failure" => config.rollback_on_test_failure = value == "true",
        _ => {}
    }
}

/// Parse `.flock/approved-deps.toml` — approved package definitions:
///
/// ```toml
/// [package.serde]
/// version_range = "^1.0"
///
/// [package.tokio]
/// version_range = ">=1.0"
/// ```
pub fn parse_approved_deps(content: &str) -> Vec<fl_policy::ApprovedPackage> {
    let mut packages = Vec::new();
    let mut current_name = String::new();
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut in_package = false;

    let flush = |name: &str,
                 fields: &std::collections::HashMap<String, String>,
                 packages: &mut Vec<fl_policy::ApprovedPackage>| {
        if name.is_empty() {
            return;
        }
        let version_range = fields
            .get("version_range")
            .cloned()
            .unwrap_or_else(|| "*".to_string());
        packages.push(fl_policy::ApprovedPackage {
            name: name.to_string(),
            version_range,
        });
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_package {
                flush(&current_name, &fields, &mut packages);
            }
            let section = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            if let Some(name) = section.strip_prefix("package.") {
                current_name = name.to_string();
                in_package = true;
            } else {
                current_name.clear();
                in_package = false;
            }
            fields.clear();
            continue;
        }

        if in_package {
            if let Some((key, value)) = trimmed.split_once('=') {
                fields.insert(key.trim().to_string(), parse_toml_string(value.trim()));
            }
        }
    }
    if in_package {
        flush(&current_name, &fields, &mut packages);
    }

    packages
}

/// Parse `.flock/arch-rules.toml` — a simple line-oriented format:
///
/// ```toml
/// # Layer boundary rules
/// [layer.presentation]
/// file_pattern = "src/ui/**"
/// allowed_deps = "domain"
///
/// [layer.domain]
/// file_pattern = "src/domain/**"
/// allowed_deps = ""
///
/// # Dependency direction rules
/// [deny.no-ui-to-infra]
/// from_pattern = "src/ui/**"
/// forbidden_pattern = "src/infra/**"
/// ```
pub fn parse_arch_rules(content: &str) -> (Vec<LayerBoundaryRule>, Vec<DependencyDirectionRule>) {
    let mut layers = Vec::new();
    let mut denies = Vec::new();
    let mut current_section = "";
    let mut current_name = String::new();
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    let flush =
        |section: &str,
         name: &str,
         fields: &std::collections::HashMap<String, String>,
         layers: &mut Vec<LayerBoundaryRule>,
         denies: &mut Vec<DependencyDirectionRule>| {
            if section.starts_with("layer.") && !name.is_empty() {
                let file_pattern = fields.get("file_pattern").cloned().unwrap_or_default();
                let allowed_deps: Vec<String> = fields
                    .get("allowed_deps")
                    .map(|s| {
                        s.split(',')
                            .map(|d| d.trim().to_string())
                            .filter(|d| !d.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                layers.push(LayerBoundaryRule {
                    layer: name.to_string(),
                    file_pattern,
                    allowed_deps,
                });
            } else if section.starts_with("deny.") && !name.is_empty() {
                let from_pattern = fields.get("from_pattern").cloned().unwrap_or_default();
                let forbidden_pattern =
                    fields.get("forbidden_pattern").cloned().unwrap_or_default();
                let description = fields
                    .get("description")
                    .cloned()
                    .unwrap_or_else(|| name.to_string());
                denies.push(DependencyDirectionRule {
                    description,
                    from_pattern,
                    forbidden_pattern,
                });
            }
        };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // Flush previous section.
            flush(current_section, &current_name, &fields, &mut layers, &mut denies);
            let section_str = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            current_section = if section_str.starts_with("layer.") {
                current_name = section_str.strip_prefix("layer.").unwrap_or("").to_string();
                "layer."
            } else if section_str.starts_with("deny.") {
                current_name = section_str.strip_prefix("deny.").unwrap_or("").to_string();
                "deny."
            } else {
                current_name.clear();
                ""
            };
            fields.clear();
            continue;
        }

        if let Some((key, value)) = trimmed.split_once('=') {
            fields.insert(key.trim().to_string(), parse_toml_string(value.trim()));
        }
    }
    // Flush last section.
    flush(current_section, &current_name, &fields, &mut layers, &mut denies);

    (layers, denies)
}

/// Parse `.flock/anti-patterns.toml` — a simple line-oriented format:
///
/// ```toml
/// [rule.no-float-currency]
/// file_pattern = "src/finance/**"
/// pattern = "f64"
/// description = "Float used for currency"
/// explanation = "Floating point causes rounding errors"
/// fix_suggestion = "Use Decimal type instead"
/// ```
pub fn parse_anti_pattern_rules(content: &str) -> Vec<AntiPatternRule> {
    let mut rules = Vec::new();
    let mut current_id = String::new();
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut in_rule = false;

    let flush =
        |id: &str, fields: &std::collections::HashMap<String, String>, rules: &mut Vec<AntiPatternRule>| {
            if id.is_empty() {
                return;
            }
            rules.push(AntiPatternRule {
                id: id.to_string(),
                description: fields.get("description").cloned().unwrap_or_else(|| id.to_string()),
                file_pattern: fields.get("file_pattern").cloned().unwrap_or_else(|| "**".to_string()),
                pattern: fields.get("pattern").cloned().unwrap_or_default(),
                explanation: fields.get("explanation").cloned().unwrap_or_default(),
                fix_suggestion: fields.get("fix_suggestion").cloned().unwrap_or_default(),
            });
        };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // Flush previous rule.
            if in_rule {
                flush(&current_id, &fields, &mut rules);
            }
            let section = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            if let Some(id) = section.strip_prefix("rule.") {
                current_id = id.to_string();
                in_rule = true;
            } else {
                current_id.clear();
                in_rule = false;
            }
            fields.clear();
            continue;
        }

        if in_rule {
            if let Some((key, value)) = trimmed.split_once('=') {
                fields.insert(key.trim().to_string(), parse_toml_string(value.trim()));
            }
        }
    }
    // Flush last rule.
    if in_rule {
        flush(&current_id, &fields, &mut rules);
    }

    rules
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
///
/// Also loads architecture rules from `.flock/arch-rules.toml` and
/// anti-pattern rules from `.flock/anti-patterns.toml` if those files exist.
pub fn load_policies_config(root: &Path) -> PoliciesConfig {
    let config_path = root.join(".flock/policies.toml");
    let mut config = match fs::read_to_string(&config_path) {
        Ok(content) => parse_policies_config(&content),
        Err(_) => PoliciesConfig::default(),
    };

    // Load architecture rules from external file.
    let arch_rules_path = root.join(".flock/arch-rules.toml");
    if let Ok(content) = fs::read_to_string(&arch_rules_path) {
        let (layers, denies) = parse_arch_rules(&content);
        config.architecture.layer_boundaries = layers;
        config.architecture.dependency_direction = denies;
    }

    // Load anti-pattern rules from external file.
    let anti_patterns_path = root.join(".flock/anti-patterns.toml");
    if let Ok(content) = fs::read_to_string(&anti_patterns_path) {
        let rules = parse_anti_pattern_rules(&content);
        config.anti_patterns.rules = rules;
    }

    // Load reuse protected domains from external file.
    let reuse_domains_path = root.join(".flock/reuse-domains.toml");
    if let Ok(content) = fs::read_to_string(&reuse_domains_path) {
        let domains = parse_reuse_domains(&content);
        config.reuse.protected_domains = domains;
    }

    // Load approved dependencies from external file.
    let approved_deps_path = root.join(".flock/approved-deps.toml");
    if let Ok(content) = fs::read_to_string(&approved_deps_path) {
        let packages = parse_approved_deps(&content);
        config.dependencies.approved_packages = packages;
    }

    config
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

    #[test]
    fn test_parse_test_requirements() {
        let toml = r#"
[test_requirements]
enabled = true
require_passing = true
test_command = "npm test"
require_new_tests = true
on_failure = "gate"
"#;
        let config = parse_policies_config(toml);
        assert!(config.test_requirements.enabled);
        assert!(config.test_requirements.require_passing);
        assert_eq!(config.test_requirements.test_command, "npm test");
        assert!(config.test_requirements.require_new_tests);
        assert_eq!(config.test_requirements.on_failure, TestFailureAction::Gate);
    }

    #[test]
    fn test_parse_budget_with_semantic_limit() {
        let toml = r#"
[budget]
enabled = true
max_semantic_changes_per_exploration = 25
on_exceed = "block"
"#;
        let config = parse_policies_config(toml);
        assert!(config.budget.enabled);
        assert_eq!(config.budget.max_semantic_changes_per_exploration, Some(25));
    }

    #[test]
    fn test_default_test_requirements() {
        let config = PoliciesConfig::default();
        assert!(!config.test_requirements.enabled);
        assert!(config.test_requirements.require_passing);
        assert_eq!(config.test_requirements.test_command, "cargo test");
        assert!(!config.test_requirements.require_new_tests);
        assert_eq!(config.test_requirements.on_failure, TestFailureAction::Block);
    }

    #[test]
    fn test_parse_architecture_config() {
        let toml = r#"
[architecture]
enabled = true
enforce = "gate"
"#;
        let config = parse_policies_config(toml);
        assert!(config.architecture.enabled);
        assert_eq!(config.architecture.enforce, ArchRuleEnforce::Gate);
    }

    #[test]
    fn test_parse_anti_patterns_config() {
        let toml = r#"
[anti_patterns]
enabled = true
enforce = "warn"
"#;
        let config = parse_policies_config(toml);
        assert!(config.anti_patterns.enabled);
        assert_eq!(config.anti_patterns.enforce, AntiPatternEnforce::Warn);
    }

    #[test]
    fn test_parse_arch_rules_file() {
        let content = r#"
[layer.presentation]
file_pattern = "src/ui/**"
allowed_deps = "domain,shared"

[layer.domain]
file_pattern = "src/domain/**"
allowed_deps = ""

[deny.no-ui-to-infra]
from_pattern = "src/ui/**"
forbidden_pattern = "src/infra/**"
description = "UI must not import from infra"
"#;
        let (layers, denies) = parse_arch_rules(content);
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].layer, "presentation");
        assert_eq!(layers[0].file_pattern, "src/ui/**");
        assert_eq!(layers[0].allowed_deps, vec!["domain", "shared"]);
        assert_eq!(layers[1].layer, "domain");
        assert!(layers[1].allowed_deps.is_empty());
        assert_eq!(denies.len(), 1);
        assert_eq!(denies[0].from_pattern, "src/ui/**");
        assert_eq!(denies[0].forbidden_pattern, "src/infra/**");
    }

    #[test]
    fn test_parse_anti_pattern_rules_file() {
        let content = r#"
[rule.no-float-currency]
file_pattern = "src/finance/**"
pattern = "f64"
description = "Float used for currency"
explanation = "Floating point causes rounding errors"
fix_suggestion = "Use Decimal type instead"

[rule.no-hardcoded-rates]
file_pattern = "src/**"
pattern = "exchange_rate ="
description = "Hardcoded exchange rate"
explanation = "Exchange rates change frequently"
fix_suggestion = "Use a rate service or config"
"#;
        let rules = parse_anti_pattern_rules(content);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].id, "no-float-currency");
        assert_eq!(rules[0].pattern, "f64");
        assert_eq!(rules[1].id, "no-hardcoded-rates");
        assert_eq!(rules[1].pattern, "exchange_rate =");
    }

    #[test]
    fn test_default_architecture_and_anti_patterns() {
        let config = PoliciesConfig::default();
        assert!(!config.architecture.enabled);
        assert_eq!(config.architecture.enforce, ArchRuleEnforce::Block);
        assert!(config.architecture.layer_boundaries.is_empty());
        assert!(!config.anti_patterns.enabled);
        assert_eq!(config.anti_patterns.enforce, AntiPatternEnforce::BlockWithExplanation);
        assert!(config.anti_patterns.rules.is_empty());
    }

    #[test]
    fn test_parse_reuse_config() {
        let toml = r#"
[reuse]
enabled = true
enforce = "block"
similarity_threshold = 0.75
check_signatures = true
check_bodies = true
check_patterns = false
"#;
        let config = parse_policies_config(toml);
        assert!(config.reuse.enabled);
        assert_eq!(config.reuse.enforce, ReuseEnforce::Block);
        assert!((config.reuse.similarity_threshold - 0.75).abs() < 0.001);
        assert!(config.reuse.check_signatures);
        assert!(config.reuse.check_bodies);
        assert!(!config.reuse.check_patterns);
    }

    #[test]
    fn test_default_reuse_config() {
        let config = PoliciesConfig::default();
        assert!(!config.reuse.enabled);
        assert_eq!(config.reuse.enforce, ReuseEnforce::Gate);
        assert!((config.reuse.similarity_threshold - 0.8).abs() < 0.001);
        assert!(config.reuse.check_signatures);
        assert!(config.reuse.check_bodies);
        assert!(config.reuse.check_patterns);
    }

    #[test]
    fn test_parse_reuse_domains() {
        let content = r#"
[domain.finance]
file_pattern = "src/finance/**"
similarity_threshold = 0.5

[domain.compliance]
file_pattern = "src/compliance/**"
similarity_threshold = 0.4
"#;
        let domains = parse_reuse_domains(content);
        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].file_pattern, "src/finance/**");
        assert!((domains[0].similarity_threshold - 0.5).abs() < 0.001);
        assert_eq!(domains[1].file_pattern, "src/compliance/**");
        assert!((domains[1].similarity_threshold - 0.4).abs() < 0.001);
    }

    #[test]
    fn test_parse_dependency_config() {
        let toml = r#"
[dependencies]
enabled = true
enforce = "gate"
vuln_check = true
license_blocklist = ["GPL-3.0", "AGPL-3.0"]
consumer_test_command = "npm run test:consumers"
"#;
        let config = parse_policies_config(toml);
        assert!(config.dependencies.enabled);
        assert_eq!(config.dependencies.enforce, DependencyEnforce::Gate);
        assert!(config.dependencies.vuln_check);
        assert_eq!(
            config.dependencies.license_blocklist,
            vec!["GPL-3.0", "AGPL-3.0"]
        );
        assert_eq!(config.dependencies.consumer_test_command, "npm run test:consumers");
    }

    #[test]
    fn test_parse_approved_deps() {
        let content = r#"
[package.serde]
version_range = "^1.0"

[package.tokio]
version_range = ">=1.0"

[package.uuid]
version_range = "*"
"#;
        let packages = parse_approved_deps(content);
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].name, "serde");
        assert_eq!(packages[0].version_range, "^1.0");
        assert_eq!(packages[1].name, "tokio");
        assert_eq!(packages[1].version_range, ">=1.0");
        assert_eq!(packages[2].name, "uuid");
        assert_eq!(packages[2].version_range, "*");
    }

    #[test]
    fn test_parse_regression_config() {
        let toml = r#"
[regression]
enabled = true
monitor_after_merge = true
monitor_window = "2h"
benchmark_threshold = 0.15
"#;
        let config = parse_policies_config(toml);
        assert!(config.regression.enabled);
        assert!(config.regression.monitor_after_merge);
        assert_eq!(config.regression.monitor_window_secs, 7200);
        assert!((config.regression.benchmark_threshold - 0.15).abs() < 0.001);
    }

    #[test]
    fn test_parse_rollback_config() {
        let toml = r#"
[rollback]
enabled = true
auto_rollback = true
rollback_on_test_failure = false
"#;
        let config = parse_policies_config(toml);
        assert!(config.rollback.enabled);
        assert!(config.rollback.auto_rollback);
        assert!(!config.rollback.rollback_on_test_failure);
    }

    #[test]
    fn test_default_dependency_config() {
        let config = PoliciesConfig::default();
        assert!(!config.dependencies.enabled);
        assert_eq!(config.dependencies.enforce, DependencyEnforce::Block);
        assert!(config.dependencies.approved_packages.is_empty());
        assert!(config.dependencies.license_blocklist.is_empty());
        assert!(!config.dependencies.vuln_check);
    }

    #[test]
    fn test_default_regression_rollback_config() {
        let config = PoliciesConfig::default();
        assert!(!config.regression.enabled);
        assert!(config.regression.monitor_after_merge);
        assert_eq!(config.regression.monitor_window_secs, 3600);
        assert!(!config.rollback.enabled);
        assert!(config.rollback.auto_rollback);
        assert!(config.rollback.rollback_on_test_failure);
    }

    #[test]
    fn test_parse_test_requirements_with_coverage() {
        let toml = r#"
[test_requirements]
enabled = true
require_passing = true
min_coverage_percent = 80
on_failure = "block"
"#;
        let config = parse_policies_config(toml);
        assert!(config.test_requirements.enabled);
        assert_eq!(config.test_requirements.min_coverage_percent, Some(80));
    }
}
