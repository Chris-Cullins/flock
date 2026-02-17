use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Verdict & Decision types
// ---------------------------------------------------------------------------

/// Outcome of a policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyVerdict {
    /// Operation is allowed.
    Allow,
    /// Operation requires justification / human approval before proceeding.
    Gate {
        reason: String,
        justification_required: bool,
    },
    /// Operation is blocked.
    Block {
        reason: String,
        fix_suggestion: Option<String>,
    },
}

/// Category of the policy that produced this decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyCategory {
    Scope,
    Budget,
    RateLimit,
    TestRequirement,
    ArchitectureRule,
    AntiPattern,
}

/// The operation being checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyOperation {
    Checkpoint,
    ExplorationStart,
    ExplorationPromote,
    Undo,
    TaskClaim,
}

/// Structured context provided when a rate limit triggers PauseAndEscalate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationContext {
    /// What the agent was trying to do.
    pub agent_action: String,
    /// Which limit was hit.
    pub limit_name: String,
    /// Current counter value.
    pub current_value: u32,
    /// Configured limit value.
    pub limit_value: u32,
    /// Exploration where the limit was hit.
    pub exploration_id: Option<Uuid>,
    /// Task associated with the limit.
    pub task_id: Option<Uuid>,
    /// Summary of recent exploration history for context.
    pub exploration_history: Vec<String>,
}

/// A fully-resolved policy evaluation result with context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub policy_name: String,
    pub category: PolicyCategory,
    pub verdict: PolicyVerdict,
    pub operation: PolicyOperation,
    pub actor: String,
    pub task_id: Option<Uuid>,
    pub exploration_id: Option<Uuid>,
    pub affected_files: Vec<String>,
    /// Populated when a rate limit triggers PauseAndEscalate.
    #[serde(default)]
    pub escalation_context: Option<EscalationContext>,
}

impl PolicyDecision {
    pub fn is_blocked(&self) -> bool {
        matches!(self.verdict, PolicyVerdict::Block { .. })
    }

    pub fn is_gated(&self) -> bool {
        matches!(self.verdict, PolicyVerdict::Gate { .. })
    }
}

// ---------------------------------------------------------------------------
// Scope policy types
// ---------------------------------------------------------------------------

/// How scope violations are enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeEnforceMode {
    /// Block the operation entirely.
    Block,
    /// Create a gate requiring human approval.
    Gate,
    /// Split changes: allow in-scope, gate out-of-scope.
    Split,
}

impl Default for ScopeEnforceMode {
    fn default() -> Self {
        Self::Block
    }
}

/// How scope boundaries are defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeMode {
    /// File path glob matching.
    Path,
    /// Semantic symbol matching (future).
    Semantic,
    /// Module-level matching (future).
    Module,
}

impl Default for ScopeMode {
    fn default() -> Self {
        Self::Path
    }
}

/// Policy for constraining which files a task may modify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopePolicy {
    pub enabled: bool,
    pub enforce_mode: ScopeEnforceMode,
    pub scope_mode: ScopeMode,
}

impl Default for ScopePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            enforce_mode: ScopeEnforceMode::default(),
            scope_mode: ScopeMode::default(),
        }
    }
}

/// The allowed scope for a specific task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskScope {
    pub task_id: Uuid,
    /// Glob patterns for allowed file paths.
    pub allowed_paths: Vec<String>,
}

// ---------------------------------------------------------------------------
// Budget types
// ---------------------------------------------------------------------------

/// What happens when a budget limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetExceedAction {
    PauseAndFlag,
    Block,
    Warn,
}

impl Default for BudgetExceedAction {
    fn default() -> Self {
        Self::Block
    }
}

/// Limits on how much a task or exploration may change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetLimits {
    pub enabled: bool,
    /// Max files modified per task.
    pub max_files_per_task: Option<u32>,
    /// Max files modified per exploration.
    pub max_files_per_exploration: Option<u32>,
    /// Max total line changes per task.
    pub max_lines_per_task: Option<u32>,
    /// Max semantic changes (symbol-level) per exploration.
    pub max_semantic_changes_per_exploration: Option<u32>,
    /// Action when budget is exceeded.
    pub on_exceed: BudgetExceedAction,
}

impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            enabled: false,
            max_files_per_task: None,
            max_files_per_exploration: None,
            max_lines_per_task: None,
            max_semantic_changes_per_exploration: None,
            on_exceed: BudgetExceedAction::default(),
        }
    }
}

/// Tracked budget usage for a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetUsage {
    pub task_id: Uuid,
    pub files_modified: u32,
    pub lines_changed: u32,
    pub exploration_id: Option<Uuid>,
    /// Files modified in the current exploration only.
    pub exploration_files_modified: u32,
    /// Semantic changes (symbol-level) accumulated globally.
    #[serde(default)]
    pub semantic_changes: u32,
    /// Semantic changes in the current exploration only.
    #[serde(default)]
    pub exploration_semantic_changes: u32,
}

// ---------------------------------------------------------------------------
// Rate limit types
// ---------------------------------------------------------------------------

/// What happens when a rate limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateLimitExceedAction {
    PauseAndEscalate,
    Warn,
    Block,
}

impl Default for RateLimitExceedAction {
    fn default() -> Self {
        Self::Block
    }
}

/// Limits on operation frequency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimits {
    pub enabled: bool,
    /// Max explorations a task may start.
    pub max_explorations_per_task: Option<u32>,
    /// Max undo operations per exploration.
    pub max_undos_per_exploration: Option<u32>,
    /// Max checkpoints within a time window (window_secs).
    pub max_checkpoints_per_window: Option<u32>,
    /// Time window in seconds for checkpoint rate limiting.
    pub window_secs: u64,
    /// Action when rate limit is exceeded.
    pub on_exceed: RateLimitExceedAction,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            enabled: false,
            max_explorations_per_task: None,
            max_undos_per_exploration: None,
            max_checkpoints_per_window: None,
            window_secs: 3600,
            on_exceed: RateLimitExceedAction::default(),
        }
    }
}

/// Tracked rate limit usage for a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitUsage {
    pub task_id: Uuid,
    pub explorations_started: u32,
    pub exploration_id: Option<Uuid>,
    /// Undo count in the current exploration.
    pub undos_in_exploration: u32,
    /// Number of checkpoints in the current window.
    pub checkpoints_in_window: u32,
}

// ---------------------------------------------------------------------------
// Test requirement types
// ---------------------------------------------------------------------------

/// What happens when test requirements fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestFailureAction {
    Block,
    Warn,
    Gate,
}

impl Default for TestFailureAction {
    fn default() -> Self {
        Self::Block
    }
}

/// Policy requiring tests to pass before exploration promotion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRequirements {
    pub enabled: bool,
    /// Tests must pass before promotion.
    pub require_passing: bool,
    /// Command to run tests.
    pub test_command: String,
    /// Require test files for new source files.
    pub require_new_tests: bool,
    /// Action when test requirements fail.
    pub on_failure: TestFailureAction,
}

impl Default for TestRequirements {
    fn default() -> Self {
        Self {
            enabled: false,
            require_passing: true,
            test_command: "cargo test".to_string(),
            require_new_tests: false,
            on_failure: TestFailureAction::default(),
        }
    }
}

/// Result of running the test command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResult {
    pub passed: bool,
    pub exit_code: i32,
    pub output_summary: String,
}

// ---------------------------------------------------------------------------
// Pure evaluation functions
// ---------------------------------------------------------------------------

/// Check whether a file path matches a task's scope.
pub fn file_matches_scope(path: &str, scope: &TaskScope) -> bool {
    for pattern in &scope.allowed_paths {
        if let Ok(glob) = glob::Pattern::new(pattern) {
            if glob.matches(path) {
                return true;
            }
        }
        // Prefix fallback (e.g. "src/" matches "src/foo.rs").
        let prefix = pattern.trim_end_matches("**").trim_end_matches('/');
        if !prefix.is_empty() && path.starts_with(prefix) {
            return true;
        }
    }
    false
}

/// Evaluate scope policy for a set of files against a task scope.
pub fn check_scope_policy(
    policy: &ScopePolicy,
    task_scope: &TaskScope,
    files: &[String],
) -> PolicyDecision {
    if !policy.enabled {
        return PolicyDecision {
            policy_name: "scope".to_string(),
            category: PolicyCategory::Scope,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: Some(task_scope.task_id),
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    let out_of_scope: Vec<String> = files
        .iter()
        .filter(|f| !file_matches_scope(f, task_scope))
        .cloned()
        .collect();

    if out_of_scope.is_empty() {
        return PolicyDecision {
            policy_name: "scope".to_string(),
            category: PolicyCategory::Scope,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: Some(task_scope.task_id),
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    let verdict = match policy.enforce_mode {
        ScopeEnforceMode::Block => PolicyVerdict::Block {
            reason: format!(
                "{} file(s) outside task scope: {}",
                out_of_scope.len(),
                out_of_scope.join(", ")
            ),
            fix_suggestion: Some(
                "Move changes to an exploration with a broader scope, or update the task scope in policies.toml".to_string(),
            ),
        },
        ScopeEnforceMode::Gate | ScopeEnforceMode::Split => PolicyVerdict::Gate {
            reason: format!(
                "{} file(s) outside task scope: {}",
                out_of_scope.len(),
                out_of_scope.join(", ")
            ),
            justification_required: true,
        },
    };

    PolicyDecision {
        policy_name: "scope".to_string(),
        category: PolicyCategory::Scope,
        verdict,
        operation: PolicyOperation::Checkpoint,
        actor: String::new(),
        task_id: Some(task_scope.task_id),
        exploration_id: None,
        affected_files: out_of_scope,
        escalation_context: None,
    }
}

/// Evaluate budget limits against current usage.
pub fn check_budget_limits(limits: &BudgetLimits, usage: &BudgetUsage) -> PolicyDecision {
    if !limits.enabled {
        return PolicyDecision {
            policy_name: "budget".to_string(),
            category: PolicyCategory::Budget,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: Some(usage.task_id),
            exploration_id: usage.exploration_id,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    // Check per-task file limit.
    if let Some(max) = limits.max_files_per_task {
        if usage.files_modified > max {
            return make_budget_decision(
                limits,
                usage,
                format!(
                    "Task has modified {} files (limit: {})",
                    usage.files_modified, max
                ),
            );
        }
    }

    // Check per-exploration file limit.
    if let Some(max) = limits.max_files_per_exploration {
        if usage.exploration_files_modified > max {
            return make_budget_decision(
                limits,
                usage,
                format!(
                    "Exploration has modified {} files (limit: {})",
                    usage.exploration_files_modified, max
                ),
            );
        }
    }

    // Check per-task line limit.
    if let Some(max) = limits.max_lines_per_task {
        if usage.lines_changed > max {
            return make_budget_decision(
                limits,
                usage,
                format!(
                    "Task has changed {} lines (limit: {})",
                    usage.lines_changed, max
                ),
            );
        }
    }

    // Check per-exploration semantic change limit.
    if let Some(max) = limits.max_semantic_changes_per_exploration {
        if usage.exploration_semantic_changes > max {
            return make_budget_decision(
                limits,
                usage,
                format!(
                    "Exploration has {} semantic changes (limit: {})",
                    usage.exploration_semantic_changes, max
                ),
            );
        }
    }

    PolicyDecision {
        policy_name: "budget".to_string(),
        category: PolicyCategory::Budget,
        verdict: PolicyVerdict::Allow,
        operation: PolicyOperation::Checkpoint,
        actor: String::new(),
        task_id: Some(usage.task_id),
        exploration_id: usage.exploration_id,
        affected_files: Vec::new(),
        escalation_context: None,
    }
}

fn make_budget_decision(
    limits: &BudgetLimits,
    usage: &BudgetUsage,
    reason: String,
) -> PolicyDecision {
    let verdict = match limits.on_exceed {
        BudgetExceedAction::Block => PolicyVerdict::Block {
            reason: reason.clone(),
            fix_suggestion: Some(
                "Split work into smaller tasks, or increase budget limits in policies.toml"
                    .to_string(),
            ),
        },
        BudgetExceedAction::Warn => PolicyVerdict::Allow,
        BudgetExceedAction::PauseAndFlag => PolicyVerdict::Gate {
            reason: reason.clone(),
            justification_required: true,
        },
    };

    PolicyDecision {
        policy_name: "budget".to_string(),
        category: PolicyCategory::Budget,
        verdict,
        operation: PolicyOperation::Checkpoint,
        actor: String::new(),
        task_id: Some(usage.task_id),
        exploration_id: usage.exploration_id,
        affected_files: Vec::new(),
        escalation_context: None,
    }
}

/// Evaluate rate limits against current usage.
pub fn check_rate_limits(limits: &RateLimits, usage: &RateLimitUsage) -> PolicyDecision {
    if !limits.enabled {
        return PolicyDecision {
            policy_name: "rate_limit".to_string(),
            category: PolicyCategory::RateLimit,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: Some(usage.task_id),
            exploration_id: usage.exploration_id,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    // Check explorations per task.
    if let Some(max) = limits.max_explorations_per_task {
        if usage.explorations_started > max {
            return make_rate_limit_decision(
                limits,
                usage,
                "max_explorations_per_task",
                usage.explorations_started,
                max,
                format!(
                    "Task has started {} explorations (limit: {})",
                    usage.explorations_started, max
                ),
                PolicyOperation::ExplorationStart,
            );
        }
    }

    // Check undos per exploration.
    if let Some(max) = limits.max_undos_per_exploration {
        if usage.undos_in_exploration > max {
            return make_rate_limit_decision(
                limits,
                usage,
                "max_undos_per_exploration",
                usage.undos_in_exploration,
                max,
                format!(
                    "Exploration has {} undos (limit: {})",
                    usage.undos_in_exploration, max
                ),
                PolicyOperation::Undo,
            );
        }
    }

    // Check checkpoints per window.
    if let Some(max) = limits.max_checkpoints_per_window {
        if usage.checkpoints_in_window > max {
            return make_rate_limit_decision(
                limits,
                usage,
                "max_checkpoints_per_window",
                usage.checkpoints_in_window,
                max,
                format!(
                    "{} checkpoints in current window (limit: {})",
                    usage.checkpoints_in_window, max
                ),
                PolicyOperation::Checkpoint,
            );
        }
    }

    PolicyDecision {
        policy_name: "rate_limit".to_string(),
        category: PolicyCategory::RateLimit,
        verdict: PolicyVerdict::Allow,
        operation: PolicyOperation::Checkpoint,
        actor: String::new(),
        task_id: Some(usage.task_id),
        exploration_id: usage.exploration_id,
        affected_files: Vec::new(),
        escalation_context: None,
    }
}

fn make_rate_limit_decision(
    limits: &RateLimits,
    usage: &RateLimitUsage,
    limit_name: &str,
    current_value: u32,
    limit_value: u32,
    reason: String,
    operation: PolicyOperation,
) -> PolicyDecision {
    let is_escalation = matches!(limits.on_exceed, RateLimitExceedAction::PauseAndEscalate);

    let verdict = match limits.on_exceed {
        RateLimitExceedAction::Block => PolicyVerdict::Block {
            reason: reason.clone(),
            fix_suggestion: Some(
                "Wait before retrying, or increase rate limits in policies.toml".to_string(),
            ),
        },
        RateLimitExceedAction::Warn => PolicyVerdict::Allow,
        RateLimitExceedAction::PauseAndEscalate => PolicyVerdict::Gate {
            reason: reason.clone(),
            justification_required: true,
        },
    };

    let escalation_context = if is_escalation {
        Some(EscalationContext {
            agent_action: format!("{:?}", operation),
            limit_name: limit_name.to_string(),
            current_value,
            limit_value,
            exploration_id: usage.exploration_id,
            task_id: Some(usage.task_id),
            exploration_history: Vec::new(),
        })
    } else {
        None
    };

    PolicyDecision {
        policy_name: "rate_limit".to_string(),
        category: PolicyCategory::RateLimit,
        verdict,
        operation,
        actor: String::new(),
        task_id: Some(usage.task_id),
        exploration_id: usage.exploration_id,
        affected_files: Vec::new(),
        escalation_context,
    }
}

/// Evaluate test requirements for exploration promotion.
pub fn check_test_requirements(
    requirements: &TestRequirements,
    test_result: Option<&TestResult>,
    new_source_files: &[String],
    new_test_files: &[String],
) -> PolicyDecision {
    if !requirements.enabled {
        return PolicyDecision {
            policy_name: "test_requirements".to_string(),
            category: PolicyCategory::TestRequirement,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::ExplorationPromote,
            actor: String::new(),
            task_id: None,
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    // Check test passing requirement.
    if requirements.require_passing {
        match test_result {
            None => {
                return make_test_decision(
                    requirements,
                    "No test results available — tests were not run".to_string(),
                );
            }
            Some(result) if !result.passed => {
                return make_test_decision(
                    requirements,
                    format!(
                        "Tests failed (exit code {}): {}",
                        result.exit_code, result.output_summary
                    ),
                );
            }
            _ => {}
        }
    }

    // Check new test coverage requirement.
    if requirements.require_new_tests && !new_source_files.is_empty() {
        let uncovered: Vec<&String> = new_source_files
            .iter()
            .filter(|src| !has_matching_test_file(src, new_test_files))
            .collect();

        if !uncovered.is_empty() {
            let files: Vec<String> = uncovered.iter().map(|s| (*s).clone()).collect();
            return PolicyDecision {
                policy_name: "test_requirements".to_string(),
                category: PolicyCategory::TestRequirement,
                verdict: match requirements.on_failure {
                    TestFailureAction::Block => PolicyVerdict::Block {
                        reason: format!(
                            "New source files without corresponding test files: {}",
                            files.join(", ")
                        ),
                        fix_suggestion: Some(
                            "Add test files for the new source files".to_string(),
                        ),
                    },
                    TestFailureAction::Warn => PolicyVerdict::Allow,
                    TestFailureAction::Gate => PolicyVerdict::Gate {
                        reason: format!(
                            "New source files without corresponding test files: {}",
                            files.join(", ")
                        ),
                        justification_required: true,
                    },
                },
                operation: PolicyOperation::ExplorationPromote,
                actor: String::new(),
                task_id: None,
                exploration_id: None,
                affected_files: files,
                escalation_context: None,
            };
        }
    }

    PolicyDecision {
        policy_name: "test_requirements".to_string(),
        category: PolicyCategory::TestRequirement,
        verdict: PolicyVerdict::Allow,
        operation: PolicyOperation::ExplorationPromote,
        actor: String::new(),
        task_id: None,
        exploration_id: None,
        affected_files: Vec::new(),
        escalation_context: None,
    }
}

fn make_test_decision(requirements: &TestRequirements, reason: String) -> PolicyDecision {
    let verdict = match requirements.on_failure {
        TestFailureAction::Block => PolicyVerdict::Block {
            reason,
            fix_suggestion: Some(
                "Fix failing tests before promoting the exploration".to_string(),
            ),
        },
        TestFailureAction::Warn => PolicyVerdict::Allow,
        TestFailureAction::Gate => PolicyVerdict::Gate {
            reason,
            justification_required: true,
        },
    };

    PolicyDecision {
        policy_name: "test_requirements".to_string(),
        category: PolicyCategory::TestRequirement,
        verdict,
        operation: PolicyOperation::ExplorationPromote,
        actor: String::new(),
        task_id: None,
        exploration_id: None,
        affected_files: Vec::new(),
        escalation_context: None,
    }
}

/// Check if a source file has a matching test file.
fn has_matching_test_file(source_path: &str, test_files: &[String]) -> bool {
    // Extract the stem (filename without extension).
    let source_stem = std::path::Path::new(source_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if source_stem.is_empty() {
        return false;
    }

    // Common test file naming conventions.
    let test_patterns = [
        format!("{}_test", source_stem),
        format!("test_{}", source_stem),
        format!("{}.test", source_stem),
        format!("{}_spec", source_stem),
    ];

    test_files.iter().any(|test_path| {
        let test_stem = std::path::Path::new(test_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        test_patterns.iter().any(|pattern| test_stem == pattern)
    })
}

// ---------------------------------------------------------------------------
// Architecture rules types (12.5f)
// ---------------------------------------------------------------------------

/// Enforcement level for architecture rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchRuleEnforce {
    Block,
    Gate,
    Warn,
}

impl Default for ArchRuleEnforce {
    fn default() -> Self {
        Self::Block
    }
}

/// A layer boundary rule — files matching `layer_pattern` may only import
/// from layers listed in `allowed_deps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerBoundaryRule {
    /// Human-readable layer name (e.g. "presentation", "domain", "infra").
    pub layer: String,
    /// Glob pattern matching files in this layer (e.g. "src/ui/**").
    pub file_pattern: String,
    /// Layers this layer is allowed to depend on.
    pub allowed_deps: Vec<String>,
}

/// A dependency direction rule — files matching `from_pattern` must not
/// import files matching `forbidden_pattern`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyDirectionRule {
    /// Description for error messages.
    pub description: String,
    /// Glob pattern for the source (importing) files.
    pub from_pattern: String,
    /// Glob pattern for forbidden dependencies.
    pub forbidden_pattern: String,
}

/// A namespace convention rule — files matching `file_pattern` must declare
/// symbols whose names match `name_pattern` (regex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceConventionRule {
    /// Description for error messages.
    pub description: String,
    /// Glob pattern for files this rule applies to.
    pub file_pattern: String,
    /// Required module/namespace prefix (e.g. "MyApp.Services.").
    pub required_prefix: String,
}

/// Top-level architecture rules config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchitectureRules {
    pub enabled: bool,
    pub enforce: ArchRuleEnforce,
    pub layer_boundaries: Vec<LayerBoundaryRule>,
    pub dependency_direction: Vec<DependencyDirectionRule>,
    pub namespace_conventions: Vec<NamespaceConventionRule>,
}

impl Default for ArchitectureRules {
    fn default() -> Self {
        Self {
            enabled: false,
            enforce: ArchRuleEnforce::default(),
            layer_boundaries: Vec::new(),
            dependency_direction: Vec::new(),
            namespace_conventions: Vec::new(),
        }
    }
}

/// Info about a detected import for architecture rule checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportInfo {
    /// The file containing the import.
    pub source_file: String,
    /// The path or module being imported.
    pub imported_path: String,
}

/// Check architecture dependency direction rules for a set of imports.
pub fn check_architecture_rules(
    rules: &ArchitectureRules,
    imports: &[ImportInfo],
    files_changed: &[String],
) -> PolicyDecision {
    if !rules.enabled {
        return PolicyDecision {
            policy_name: "architecture".to_string(),
            category: PolicyCategory::ArchitectureRule,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: None,
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    let mut violations: Vec<String> = Vec::new();
    let mut affected: Vec<String> = Vec::new();

    // Check dependency direction rules.
    for rule in &rules.dependency_direction {
        let from_glob = match glob::Pattern::new(&rule.from_pattern) {
            Ok(g) => g,
            Err(_) => continue,
        };
        let forbidden_glob = match glob::Pattern::new(&rule.forbidden_pattern) {
            Ok(g) => g,
            Err(_) => continue,
        };

        for import in imports {
            if from_glob.matches(&import.source_file)
                && forbidden_glob.matches(&import.imported_path)
            {
                violations.push(format!(
                    "{}: {} imports {} — {}",
                    import.source_file, import.source_file,
                    import.imported_path, rule.description
                ));
                if !affected.contains(&import.source_file) {
                    affected.push(import.source_file.clone());
                }
            }
        }
    }

    // Check layer boundary rules.
    for rule in &rules.layer_boundaries {
        let layer_glob = match glob::Pattern::new(&rule.file_pattern) {
            Ok(g) => g,
            Err(_) => continue,
        };

        for import in imports {
            if !layer_glob.matches(&import.source_file) {
                continue;
            }
            // Check if the imported path belongs to an allowed layer.
            let import_in_allowed_layer = rules.layer_boundaries.iter().any(|other_rule| {
                rule.allowed_deps.contains(&other_rule.layer)
                    && glob::Pattern::new(&other_rule.file_pattern)
                        .map(|g| g.matches(&import.imported_path))
                        .unwrap_or(false)
            });
            // Also allow importing from the same layer.
            let import_in_same_layer = layer_glob.matches(&import.imported_path);
            // Allow imports that don't match any defined layer (external deps).
            let import_in_any_layer = rules.layer_boundaries.iter().any(|other_rule| {
                glob::Pattern::new(&other_rule.file_pattern)
                    .map(|g| g.matches(&import.imported_path))
                    .unwrap_or(false)
            });

            if import_in_any_layer && !import_in_allowed_layer && !import_in_same_layer {
                let target_layer = rules
                    .layer_boundaries
                    .iter()
                    .find(|r| {
                        glob::Pattern::new(&r.file_pattern)
                            .map(|g| g.matches(&import.imported_path))
                            .unwrap_or(false)
                    })
                    .map(|r| r.layer.as_str())
                    .unwrap_or("unknown");
                violations.push(format!(
                    "{}: layer \"{}\" cannot depend on layer \"{}\" (imported {})",
                    import.source_file, rule.layer, target_layer, import.imported_path
                ));
                if !affected.contains(&import.source_file) {
                    affected.push(import.source_file.clone());
                }
            }
        }
    }

    // Check namespace conventions.
    for rule in &rules.namespace_conventions {
        let file_glob = match glob::Pattern::new(&rule.file_pattern) {
            Ok(g) => g,
            Err(_) => continue,
        };

        for path in files_changed {
            if !file_glob.matches(path) {
                continue;
            }
            // Extract the file stem as a proxy for the primary symbol name.
            let stem = std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !stem.is_empty() && !stem.starts_with(&rule.required_prefix) {
                violations.push(format!(
                    "{}: file does not follow namespace convention (expected prefix \"{}\"): {}",
                    path, rule.required_prefix, rule.description
                ));
                if !affected.contains(path) {
                    affected.push(path.clone());
                }
            }
        }
    }

    if violations.is_empty() {
        return PolicyDecision {
            policy_name: "architecture".to_string(),
            category: PolicyCategory::ArchitectureRule,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: None,
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    let reason = format!(
        "{} architecture rule violation(s):\n  {}",
        violations.len(),
        violations.join("\n  ")
    );

    let verdict = match rules.enforce {
        ArchRuleEnforce::Block => PolicyVerdict::Block {
            reason,
            fix_suggestion: Some(
                "Restructure imports to respect layer boundaries and dependency direction rules"
                    .to_string(),
            ),
        },
        ArchRuleEnforce::Gate => PolicyVerdict::Gate {
            reason,
            justification_required: true,
        },
        ArchRuleEnforce::Warn => PolicyVerdict::Allow,
    };

    PolicyDecision {
        policy_name: "architecture".to_string(),
        category: PolicyCategory::ArchitectureRule,
        verdict,
        operation: PolicyOperation::Checkpoint,
        actor: String::new(),
        task_id: None,
        exploration_id: None,
        affected_files: affected,
        escalation_context: None,
    }
}

// ---------------------------------------------------------------------------
// Anti-pattern detection types (12.5g)
// ---------------------------------------------------------------------------

/// Enforcement level for anti-pattern rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AntiPatternEnforce {
    BlockWithExplanation,
    Gate,
    Warn,
}

impl Default for AntiPatternEnforce {
    fn default() -> Self {
        Self::BlockWithExplanation
    }
}

/// A single anti-pattern rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AntiPatternRule {
    /// Unique rule ID (e.g. "no-float-currency").
    pub id: String,
    /// Human-readable description of what this rule catches.
    pub description: String,
    /// Glob pattern for files to check (e.g. "src/finance/**").
    pub file_pattern: String,
    /// Text pattern to search for (simple substring or regex-like).
    pub pattern: String,
    /// Explanation of why this is an anti-pattern.
    pub explanation: String,
    /// Suggested fix.
    pub fix_suggestion: String,
}

/// Top-level anti-pattern detection config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AntiPatternConfig {
    pub enabled: bool,
    pub enforce: AntiPatternEnforce,
    pub rules: Vec<AntiPatternRule>,
}

impl Default for AntiPatternConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            enforce: AntiPatternEnforce::default(),
            rules: Vec::new(),
        }
    }
}

/// A detected anti-pattern match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntiPatternMatch {
    pub rule_id: String,
    pub file: String,
    pub line_number: Option<u32>,
    pub matched_text: String,
}

/// Check file contents against anti-pattern rules.
pub fn check_anti_patterns(
    config: &AntiPatternConfig,
    file_contents: &[(String, String)], // (path, content)
) -> PolicyDecision {
    if !config.enabled {
        return PolicyDecision {
            policy_name: "anti_patterns".to_string(),
            category: PolicyCategory::AntiPattern,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: None,
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    let mut matches: Vec<(String, &AntiPatternRule)> = Vec::new();
    let mut affected: Vec<String> = Vec::new();

    for rule in &config.rules {
        let file_glob = match glob::Pattern::new(&rule.file_pattern) {
            Ok(g) => g,
            Err(_) => continue,
        };

        for (path, content) in file_contents {
            if !file_glob.matches(path) {
                continue;
            }

            // Simple substring match against file content lines.
            for (line_idx, line) in content.lines().enumerate() {
                if line.contains(&rule.pattern) {
                    matches.push((
                        format!(
                            "{}:{}: [{}] {} — {}",
                            path,
                            line_idx + 1,
                            rule.id,
                            rule.description,
                            rule.explanation
                        ),
                        rule,
                    ));
                    if !affected.contains(path) {
                        affected.push(path.clone());
                    }
                    break; // One match per rule per file is sufficient.
                }
            }
        }
    }

    if matches.is_empty() {
        return PolicyDecision {
            policy_name: "anti_patterns".to_string(),
            category: PolicyCategory::AntiPattern,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: None,
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    let violations: Vec<String> = matches.iter().map(|(desc, _)| desc.clone()).collect();
    let fix_suggestions: Vec<String> = matches
        .iter()
        .map(|(_, rule)| format!("[{}] {}", rule.id, rule.fix_suggestion))
        .collect();

    let reason = format!(
        "{} anti-pattern(s) detected:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );

    let verdict = match config.enforce {
        AntiPatternEnforce::BlockWithExplanation => PolicyVerdict::Block {
            reason,
            fix_suggestion: Some(fix_suggestions.join("\n")),
        },
        AntiPatternEnforce::Gate => PolicyVerdict::Gate {
            reason,
            justification_required: true,
        },
        AntiPatternEnforce::Warn => PolicyVerdict::Allow,
    };

    PolicyDecision {
        policy_name: "anti_patterns".to_string(),
        category: PolicyCategory::AntiPattern,
        verdict,
        operation: PolicyOperation::Checkpoint,
        actor: String::new(),
        task_id: None,
        exploration_id: None,
        affected_files: affected,
        escalation_context: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_task_id() -> Uuid {
        Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
    }

    fn test_exploration_id() -> Uuid {
        Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap()
    }

    // --- Scope tests ---

    #[test]
    fn scope_disabled_allows_everything() {
        let policy = ScopePolicy::default(); // enabled: false
        let scope = TaskScope {
            task_id: test_task_id(),
            allowed_paths: vec!["src/**".to_string()],
        };
        let decision = check_scope_policy(&policy, &scope, &["lib/foo.rs".to_string()]);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn scope_in_scope_allows() {
        let policy = ScopePolicy {
            enabled: true,
            enforce_mode: ScopeEnforceMode::Block,
            scope_mode: ScopeMode::Path,
        };
        let scope = TaskScope {
            task_id: test_task_id(),
            allowed_paths: vec!["src/**".to_string()],
        };
        let decision = check_scope_policy(
            &policy,
            &scope,
            &["src/main.rs".to_string(), "src/lib.rs".to_string()],
        );
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn scope_out_of_scope_blocks() {
        let policy = ScopePolicy {
            enabled: true,
            enforce_mode: ScopeEnforceMode::Block,
            scope_mode: ScopeMode::Path,
        };
        let scope = TaskScope {
            task_id: test_task_id(),
            allowed_paths: vec!["src/**".to_string()],
        };
        let decision = check_scope_policy(
            &policy,
            &scope,
            &["src/main.rs".to_string(), "tests/integration.rs".to_string()],
        );
        assert!(decision.is_blocked());
        assert_eq!(decision.affected_files, vec!["tests/integration.rs"]);
    }

    #[test]
    fn scope_out_of_scope_gates() {
        let policy = ScopePolicy {
            enabled: true,
            enforce_mode: ScopeEnforceMode::Gate,
            scope_mode: ScopeMode::Path,
        };
        let scope = TaskScope {
            task_id: test_task_id(),
            allowed_paths: vec!["src/**".to_string()],
        };
        let decision =
            check_scope_policy(&policy, &scope, &["docs/README.md".to_string()]);
        assert!(decision.is_gated());
    }

    #[test]
    fn file_matches_scope_glob_patterns() {
        let scope = TaskScope {
            task_id: test_task_id(),
            allowed_paths: vec![
                "src/**".to_string(),
                "Cargo.toml".to_string(),
            ],
        };
        assert!(file_matches_scope("src/main.rs", &scope));
        assert!(file_matches_scope("src/nested/deep.rs", &scope));
        assert!(file_matches_scope("Cargo.toml", &scope));
        assert!(!file_matches_scope("tests/foo.rs", &scope));
    }

    // --- Budget tests ---

    #[test]
    fn budget_disabled_allows_everything() {
        let limits = BudgetLimits::default(); // enabled: false
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 999,
            lines_changed: 999999,
            exploration_id: None,
            exploration_files_modified: 0,
            semantic_changes: 0,
            exploration_semantic_changes: 0,
        };
        let decision = check_budget_limits(&limits, &usage);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn budget_within_limits_allows() {
        let limits = BudgetLimits {
            enabled: true,
            max_files_per_task: Some(10),
            max_lines_per_task: Some(500),
            max_files_per_exploration: None,
            max_semantic_changes_per_exploration: None,
            on_exceed: BudgetExceedAction::Block,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 5,
            lines_changed: 200,
            exploration_id: None,
            exploration_files_modified: 0,
            semantic_changes: 0,
            exploration_semantic_changes: 0,
        };
        let decision = check_budget_limits(&limits, &usage);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn budget_exceeded_blocks() {
        let limits = BudgetLimits {
            enabled: true,
            max_files_per_task: Some(5),
            max_lines_per_task: None,
            max_files_per_exploration: None,
            max_semantic_changes_per_exploration: None,
            on_exceed: BudgetExceedAction::Block,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 6,
            lines_changed: 0,
            exploration_id: None,
            exploration_files_modified: 0,
            semantic_changes: 0,
            exploration_semantic_changes: 0,
        };
        let decision = check_budget_limits(&limits, &usage);
        assert!(decision.is_blocked());
    }

    #[test]
    fn budget_exceeded_warn_allows() {
        let limits = BudgetLimits {
            enabled: true,
            max_files_per_task: Some(5),
            max_lines_per_task: None,
            max_files_per_exploration: None,
            max_semantic_changes_per_exploration: None,
            on_exceed: BudgetExceedAction::Warn,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 10,
            lines_changed: 0,
            exploration_id: None,
            exploration_files_modified: 0,
            semantic_changes: 0,
            exploration_semantic_changes: 0,
        };
        let decision = check_budget_limits(&limits, &usage);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn budget_exploration_files_exceeded() {
        let limits = BudgetLimits {
            enabled: true,
            max_files_per_task: None,
            max_lines_per_task: None,
            max_files_per_exploration: Some(3),
            max_semantic_changes_per_exploration: None,
            on_exceed: BudgetExceedAction::Block,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 2,
            lines_changed: 0,
            exploration_id: Some(test_exploration_id()),
            exploration_files_modified: 4,
            semantic_changes: 0,
            exploration_semantic_changes: 0,
        };
        let decision = check_budget_limits(&limits, &usage);
        assert!(decision.is_blocked());
    }

    #[test]
    fn budget_lines_exceeded_pause_and_flag() {
        let limits = BudgetLimits {
            enabled: true,
            max_files_per_task: None,
            max_lines_per_task: Some(100),
            max_files_per_exploration: None,
            max_semantic_changes_per_exploration: None,
            on_exceed: BudgetExceedAction::PauseAndFlag,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 0,
            lines_changed: 150,
            exploration_id: None,
            exploration_files_modified: 0,
            semantic_changes: 0,
            exploration_semantic_changes: 0,
        };
        let decision = check_budget_limits(&limits, &usage);
        assert!(decision.is_gated());
    }

    #[test]
    fn budget_semantic_changes_exceeded_blocks() {
        let limits = BudgetLimits {
            enabled: true,
            max_files_per_task: None,
            max_lines_per_task: None,
            max_files_per_exploration: None,
            max_semantic_changes_per_exploration: Some(10),
            on_exceed: BudgetExceedAction::Block,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 0,
            lines_changed: 0,
            exploration_id: Some(test_exploration_id()),
            exploration_files_modified: 0,
            semantic_changes: 15,
            exploration_semantic_changes: 12,
        };
        let decision = check_budget_limits(&limits, &usage);
        assert!(decision.is_blocked());
    }

    #[test]
    fn budget_semantic_changes_within_limit_allows() {
        let limits = BudgetLimits {
            enabled: true,
            max_files_per_task: None,
            max_lines_per_task: None,
            max_files_per_exploration: None,
            max_semantic_changes_per_exploration: Some(20),
            on_exceed: BudgetExceedAction::Block,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 0,
            lines_changed: 0,
            exploration_id: Some(test_exploration_id()),
            exploration_files_modified: 0,
            semantic_changes: 15,
            exploration_semantic_changes: 8,
        };
        let decision = check_budget_limits(&limits, &usage);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    // --- Rate limit tests ---

    #[test]
    fn rate_limit_disabled_allows_everything() {
        let limits = RateLimits::default(); // enabled: false
        let usage = RateLimitUsage {
            task_id: test_task_id(),
            explorations_started: 999,
            exploration_id: None,
            undos_in_exploration: 999,
            checkpoints_in_window: 999,
        };
        let decision = check_rate_limits(&limits, &usage);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn rate_limit_within_limits_allows() {
        let limits = RateLimits {
            enabled: true,
            max_explorations_per_task: Some(10),
            max_undos_per_exploration: Some(5),
            max_checkpoints_per_window: Some(20),
            window_secs: 3600,
            on_exceed: RateLimitExceedAction::Block,
        };
        let usage = RateLimitUsage {
            task_id: test_task_id(),
            explorations_started: 3,
            exploration_id: Some(test_exploration_id()),
            undos_in_exploration: 1,
            checkpoints_in_window: 5,
        };
        let decision = check_rate_limits(&limits, &usage);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn rate_limit_explorations_exceeded_blocks() {
        let limits = RateLimits {
            enabled: true,
            max_explorations_per_task: Some(3),
            max_undos_per_exploration: None,
            max_checkpoints_per_window: None,
            window_secs: 3600,
            on_exceed: RateLimitExceedAction::Block,
        };
        let usage = RateLimitUsage {
            task_id: test_task_id(),
            explorations_started: 4,
            exploration_id: None,
            undos_in_exploration: 0,
            checkpoints_in_window: 0,
        };
        let decision = check_rate_limits(&limits, &usage);
        assert!(decision.is_blocked());
        assert_eq!(decision.operation, PolicyOperation::ExplorationStart);
    }

    #[test]
    fn rate_limit_undos_exceeded_warns() {
        let limits = RateLimits {
            enabled: true,
            max_explorations_per_task: None,
            max_undos_per_exploration: Some(3),
            max_checkpoints_per_window: None,
            window_secs: 3600,
            on_exceed: RateLimitExceedAction::Warn,
        };
        let usage = RateLimitUsage {
            task_id: test_task_id(),
            explorations_started: 0,
            exploration_id: Some(test_exploration_id()),
            undos_in_exploration: 5,
            checkpoints_in_window: 0,
        };
        let decision = check_rate_limits(&limits, &usage);
        // Warn mode still returns Allow
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn rate_limit_checkpoints_exceeded_escalates() {
        let limits = RateLimits {
            enabled: true,
            max_explorations_per_task: None,
            max_undos_per_exploration: None,
            max_checkpoints_per_window: Some(10),
            window_secs: 3600,
            on_exceed: RateLimitExceedAction::PauseAndEscalate,
        };
        let usage = RateLimitUsage {
            task_id: test_task_id(),
            explorations_started: 0,
            exploration_id: None,
            undos_in_exploration: 0,
            checkpoints_in_window: 15,
        };
        let decision = check_rate_limits(&limits, &usage);
        assert!(decision.is_gated());
        assert!(decision.escalation_context.is_some());
        let ctx = decision.escalation_context.unwrap();
        assert_eq!(ctx.limit_name, "max_checkpoints_per_window");
        assert_eq!(ctx.current_value, 15);
        assert_eq!(ctx.limit_value, 10);
    }

    #[test]
    fn rate_limit_escalation_context_not_present_on_block() {
        let limits = RateLimits {
            enabled: true,
            max_explorations_per_task: Some(3),
            max_undos_per_exploration: None,
            max_checkpoints_per_window: None,
            window_secs: 3600,
            on_exceed: RateLimitExceedAction::Block,
        };
        let usage = RateLimitUsage {
            task_id: test_task_id(),
            explorations_started: 4,
            exploration_id: None,
            undos_in_exploration: 0,
            checkpoints_in_window: 0,
        };
        let decision = check_rate_limits(&limits, &usage);
        assert!(decision.is_blocked());
        assert!(decision.escalation_context.is_none());
    }

    // --- Test requirement tests ---

    #[test]
    fn test_requirements_disabled_allows() {
        let req = TestRequirements::default(); // enabled: false
        let decision = check_test_requirements(&req, None, &[], &[]);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn test_requirements_passing_tests_allows() {
        let req = TestRequirements {
            enabled: true,
            require_passing: true,
            test_command: "cargo test".to_string(),
            require_new_tests: false,
            on_failure: TestFailureAction::Block,
        };
        let result = TestResult {
            passed: true,
            exit_code: 0,
            output_summary: "all tests passed".to_string(),
        };
        let decision = check_test_requirements(&req, Some(&result), &[], &[]);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn test_requirements_failing_tests_blocks() {
        let req = TestRequirements {
            enabled: true,
            require_passing: true,
            test_command: "cargo test".to_string(),
            require_new_tests: false,
            on_failure: TestFailureAction::Block,
        };
        let result = TestResult {
            passed: false,
            exit_code: 1,
            output_summary: "2 tests failed".to_string(),
        };
        let decision = check_test_requirements(&req, Some(&result), &[], &[]);
        assert!(decision.is_blocked());
    }

    #[test]
    fn test_requirements_no_results_blocks() {
        let req = TestRequirements {
            enabled: true,
            require_passing: true,
            test_command: "cargo test".to_string(),
            require_new_tests: false,
            on_failure: TestFailureAction::Block,
        };
        let decision = check_test_requirements(&req, None, &[], &[]);
        assert!(decision.is_blocked());
    }

    #[test]
    fn test_requirements_failing_tests_gates() {
        let req = TestRequirements {
            enabled: true,
            require_passing: true,
            test_command: "cargo test".to_string(),
            require_new_tests: false,
            on_failure: TestFailureAction::Gate,
        };
        let result = TestResult {
            passed: false,
            exit_code: 1,
            output_summary: "1 test failed".to_string(),
        };
        let decision = check_test_requirements(&req, Some(&result), &[], &[]);
        assert!(decision.is_gated());
    }

    #[test]
    fn test_requirements_new_tests_required_with_coverage() {
        let req = TestRequirements {
            enabled: true,
            require_passing: false,
            test_command: "cargo test".to_string(),
            require_new_tests: true,
            on_failure: TestFailureAction::Block,
        };
        let new_source = vec!["src/auth.rs".to_string()];
        let new_tests = vec!["tests/auth_test.rs".to_string()];
        let decision = check_test_requirements(&req, None, &new_source, &new_tests);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn test_requirements_new_tests_required_without_coverage() {
        let req = TestRequirements {
            enabled: true,
            require_passing: false,
            test_command: "cargo test".to_string(),
            require_new_tests: true,
            on_failure: TestFailureAction::Block,
        };
        let new_source = vec!["src/auth.rs".to_string(), "src/db.rs".to_string()];
        let new_tests = vec!["tests/auth_test.rs".to_string()];
        let decision = check_test_requirements(&req, None, &new_source, &new_tests);
        assert!(decision.is_blocked());
        assert_eq!(decision.affected_files, vec!["src/db.rs"]);
    }

    #[test]
    fn has_matching_test_file_patterns() {
        assert!(has_matching_test_file(
            "src/auth.rs",
            &["tests/auth_test.rs".to_string()]
        ));
        assert!(has_matching_test_file(
            "src/auth.rs",
            &["tests/test_auth.rs".to_string()]
        ));
        assert!(has_matching_test_file(
            "src/auth.rs",
            &["tests/auth_spec.rs".to_string()]
        ));
        assert!(!has_matching_test_file(
            "src/auth.rs",
            &["tests/login_test.rs".to_string()]
        ));
    }

    // --- PolicyDecision helpers ---

    #[test]
    fn decision_is_blocked_and_gated() {
        let blocked = PolicyDecision {
            policy_name: "test".to_string(),
            category: PolicyCategory::Budget,
            verdict: PolicyVerdict::Block {
                reason: "too many".to_string(),
                fix_suggestion: None,
            },
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: None,
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
        assert!(blocked.is_blocked());
        assert!(!blocked.is_gated());

        let gated = PolicyDecision {
            policy_name: "test".to_string(),
            category: PolicyCategory::Scope,
            verdict: PolicyVerdict::Gate {
                reason: "needs approval".to_string(),
                justification_required: true,
            },
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: None,
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
        assert!(!gated.is_blocked());
        assert!(gated.is_gated());
    }

    // --- Architecture rules tests ---

    #[test]
    fn arch_rules_disabled_allows() {
        let rules = ArchitectureRules::default();
        let decision = check_architecture_rules(&rules, &[], &[]);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn arch_dependency_direction_blocks_violation() {
        let rules = ArchitectureRules {
            enabled: true,
            enforce: ArchRuleEnforce::Block,
            layer_boundaries: Vec::new(),
            dependency_direction: vec![DependencyDirectionRule {
                description: "UI must not import from infra".to_string(),
                from_pattern: "src/ui/**".to_string(),
                forbidden_pattern: "src/infra/**".to_string(),
            }],
            namespace_conventions: Vec::new(),
        };
        let imports = vec![ImportInfo {
            source_file: "src/ui/dashboard.ts".to_string(),
            imported_path: "src/infra/database.ts".to_string(),
        }];
        let decision = check_architecture_rules(&rules, &imports, &[]);
        assert!(decision.is_blocked());
        assert_eq!(decision.affected_files, vec!["src/ui/dashboard.ts"]);
    }

    #[test]
    fn arch_dependency_direction_allows_valid_import() {
        let rules = ArchitectureRules {
            enabled: true,
            enforce: ArchRuleEnforce::Block,
            layer_boundaries: Vec::new(),
            dependency_direction: vec![DependencyDirectionRule {
                description: "UI must not import from infra".to_string(),
                from_pattern: "src/ui/**".to_string(),
                forbidden_pattern: "src/infra/**".to_string(),
            }],
            namespace_conventions: Vec::new(),
        };
        let imports = vec![ImportInfo {
            source_file: "src/ui/dashboard.ts".to_string(),
            imported_path: "src/domain/user.ts".to_string(),
        }];
        let decision = check_architecture_rules(&rules, &imports, &[]);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn arch_layer_boundary_blocks_forbidden_dep() {
        let rules = ArchitectureRules {
            enabled: true,
            enforce: ArchRuleEnforce::Block,
            layer_boundaries: vec![
                LayerBoundaryRule {
                    layer: "presentation".to_string(),
                    file_pattern: "src/ui/**".to_string(),
                    allowed_deps: vec!["domain".to_string()],
                },
                LayerBoundaryRule {
                    layer: "domain".to_string(),
                    file_pattern: "src/domain/**".to_string(),
                    allowed_deps: vec![],
                },
                LayerBoundaryRule {
                    layer: "infra".to_string(),
                    file_pattern: "src/infra/**".to_string(),
                    allowed_deps: vec!["domain".to_string()],
                },
            ],
            dependency_direction: Vec::new(),
            namespace_conventions: Vec::new(),
        };
        // Presentation importing from infra — not allowed.
        let imports = vec![ImportInfo {
            source_file: "src/ui/page.ts".to_string(),
            imported_path: "src/infra/db.ts".to_string(),
        }];
        let decision = check_architecture_rules(&rules, &imports, &[]);
        assert!(decision.is_blocked());
    }

    #[test]
    fn arch_layer_boundary_allows_valid_dep() {
        let rules = ArchitectureRules {
            enabled: true,
            enforce: ArchRuleEnforce::Block,
            layer_boundaries: vec![
                LayerBoundaryRule {
                    layer: "presentation".to_string(),
                    file_pattern: "src/ui/**".to_string(),
                    allowed_deps: vec!["domain".to_string()],
                },
                LayerBoundaryRule {
                    layer: "domain".to_string(),
                    file_pattern: "src/domain/**".to_string(),
                    allowed_deps: vec![],
                },
            ],
            dependency_direction: Vec::new(),
            namespace_conventions: Vec::new(),
        };
        // Presentation importing from domain — allowed.
        let imports = vec![ImportInfo {
            source_file: "src/ui/page.ts".to_string(),
            imported_path: "src/domain/user.ts".to_string(),
        }];
        let decision = check_architecture_rules(&rules, &imports, &[]);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn arch_rules_gate_mode() {
        let rules = ArchitectureRules {
            enabled: true,
            enforce: ArchRuleEnforce::Gate,
            layer_boundaries: Vec::new(),
            dependency_direction: vec![DependencyDirectionRule {
                description: "no cross-boundary".to_string(),
                from_pattern: "src/a/**".to_string(),
                forbidden_pattern: "src/b/**".to_string(),
            }],
            namespace_conventions: Vec::new(),
        };
        let imports = vec![ImportInfo {
            source_file: "src/a/foo.rs".to_string(),
            imported_path: "src/b/bar.rs".to_string(),
        }];
        let decision = check_architecture_rules(&rules, &imports, &[]);
        assert!(decision.is_gated());
    }

    // --- Anti-pattern tests ---

    #[test]
    fn anti_pattern_disabled_allows() {
        let config = AntiPatternConfig::default();
        let decision = check_anti_patterns(&config, &[]);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn anti_pattern_detects_match() {
        let config = AntiPatternConfig {
            enabled: true,
            enforce: AntiPatternEnforce::BlockWithExplanation,
            rules: vec![AntiPatternRule {
                id: "no-float-currency".to_string(),
                description: "Float used for currency".to_string(),
                file_pattern: "src/finance/**".to_string(),
                pattern: "f64".to_string(),
                explanation: "Floating point arithmetic causes rounding errors in financial calculations".to_string(),
                fix_suggestion: "Use a Decimal type or integer cents instead".to_string(),
            }],
        };
        let files = vec![(
            "src/finance/billing.rs".to_string(),
            "let total: f64 = 19.99;".to_string(),
        )];
        let decision = check_anti_patterns(&config, &files);
        assert!(decision.is_blocked());
        assert_eq!(decision.affected_files, vec!["src/finance/billing.rs"]);
    }

    #[test]
    fn anti_pattern_no_match_allows() {
        let config = AntiPatternConfig {
            enabled: true,
            enforce: AntiPatternEnforce::BlockWithExplanation,
            rules: vec![AntiPatternRule {
                id: "no-float-currency".to_string(),
                description: "Float used for currency".to_string(),
                file_pattern: "src/finance/**".to_string(),
                pattern: "f64".to_string(),
                explanation: "Rounding errors".to_string(),
                fix_suggestion: "Use Decimal".to_string(),
            }],
        };
        let files = vec![(
            "src/finance/billing.rs".to_string(),
            "let total: Decimal = Decimal::new(1999, 2);".to_string(),
        )];
        let decision = check_anti_patterns(&config, &files);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn anti_pattern_ignores_non_matching_files() {
        let config = AntiPatternConfig {
            enabled: true,
            enforce: AntiPatternEnforce::BlockWithExplanation,
            rules: vec![AntiPatternRule {
                id: "no-float-currency".to_string(),
                description: "Float used for currency".to_string(),
                file_pattern: "src/finance/**".to_string(),
                pattern: "f64".to_string(),
                explanation: "Rounding errors".to_string(),
                fix_suggestion: "Use Decimal".to_string(),
            }],
        };
        // File is not in src/finance/
        let files = vec![(
            "src/graphics/renderer.rs".to_string(),
            "let scale: f64 = 1.5;".to_string(),
        )];
        let decision = check_anti_patterns(&config, &files);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn anti_pattern_warn_mode_allows() {
        let config = AntiPatternConfig {
            enabled: true,
            enforce: AntiPatternEnforce::Warn,
            rules: vec![AntiPatternRule {
                id: "no-todo".to_string(),
                description: "TODO comment".to_string(),
                file_pattern: "**".to_string(),
                pattern: "TODO".to_string(),
                explanation: "TODOs should be tracked as tasks".to_string(),
                fix_suggestion: "Create a task instead".to_string(),
            }],
        };
        let files = vec![(
            "src/main.rs".to_string(),
            "// TODO: fix this later".to_string(),
        )];
        let decision = check_anti_patterns(&config, &files);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }
}
