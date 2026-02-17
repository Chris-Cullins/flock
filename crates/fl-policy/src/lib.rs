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
    DuplicationReuse,
    DependencyCheck,
    Regression,
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
    /// Minimum coverage percentage for modified modules (0--100).
    pub min_coverage_percent: Option<u32>,
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
            min_coverage_percent: None,
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

/// Coverage data for modified modules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoverageResult {
    /// Per-module coverage percentages (module_path → coverage %).
    pub module_coverage: Vec<(String, f64)>,
    /// Overall coverage percentage.
    pub overall_percent: f64,
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
// DRY / Duplication prevention types (12.5e)
// ---------------------------------------------------------------------------

/// Enforcement level for reuse / duplication prevention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReuseEnforce {
    Block,
    Gate,
    Warn,
}

impl Default for ReuseEnforce {
    fn default() -> Self {
        Self::Gate
    }
}

/// Which duplication detection layer matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DuplicationLayer {
    /// Layer 1: Parameter types, return type, name similarity.
    Signature,
    /// Layer 2: AST structural comparison of method bodies.
    Body,
    /// Layer 3: New code should implement existing interfaces/patterns.
    Pattern,
}

/// A protected domain with stricter enforcement thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtectedDomain {
    /// Glob pattern for files in this domain.
    pub file_pattern: String,
    /// Override similarity threshold for this domain (lower = stricter).
    pub similarity_threshold: f64,
}

/// Top-level reuse / duplication prevention config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReusePolicy {
    pub enabled: bool,
    pub enforce: ReuseEnforce,
    /// Similarity threshold (0.0--1.0). Matches above this are flagged.
    pub similarity_threshold: f64,
    /// Enable Layer 1: signature matching.
    pub check_signatures: bool,
    /// Enable Layer 2: body structural comparison.
    pub check_bodies: bool,
    /// Enable Layer 3: pattern/interface conformance.
    pub check_patterns: bool,
    /// Domains where stricter thresholds apply.
    pub protected_domains: Vec<ProtectedDomain>,
}

impl Default for ReusePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            enforce: ReuseEnforce::default(),
            similarity_threshold: 0.8,
            check_signatures: true,
            check_bodies: true,
            check_patterns: true,
            protected_domains: Vec::new(),
        }
    }
}

/// A detected duplication match between a new symbol and an existing one.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicationMatch {
    /// Fully-qualified name of the new symbol (e.g. "function:processPayment").
    pub new_symbol: String,
    /// File containing the new symbol.
    pub new_file: String,
    /// Fully-qualified name of the existing symbol it duplicates.
    pub existing_symbol: String,
    /// File containing the existing symbol.
    pub existing_file: String,
    /// How similar the two symbols are (0.0--1.0).
    pub similarity: f64,
    /// Which detection layer found this match.
    pub layer: DuplicationLayer,
    /// Human-readable suggestion for reusing the existing code.
    pub reuse_suggestion: String,
}

/// Check new symbols against existing symbols for duplication.
///
/// `matches` should be pre-computed by the caller using signature matching,
/// body hash comparison, and pattern conformance checks.
pub fn check_reuse_policy(
    config: &ReusePolicy,
    matches: &[DuplicationMatch],
) -> PolicyDecision {
    if !config.enabled {
        return PolicyDecision {
            policy_name: "reuse".to_string(),
            category: PolicyCategory::DuplicationReuse,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::Checkpoint,
            actor: String::new(),
            task_id: None,
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    if matches.is_empty() {
        return PolicyDecision {
            policy_name: "reuse".to_string(),
            category: PolicyCategory::DuplicationReuse,
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
    let mut suggestions: Vec<String> = Vec::new();

    for m in matches {
        let layer_label = match m.layer {
            DuplicationLayer::Signature => "signature",
            DuplicationLayer::Body => "body",
            DuplicationLayer::Pattern => "pattern",
        };
        violations.push(format!(
            "{}: {} is {:.0}% similar to {} in {} ({} match)",
            m.new_file,
            m.new_symbol,
            m.similarity * 100.0,
            m.existing_symbol,
            m.existing_file,
            layer_label,
        ));
        if !affected.contains(&m.new_file) {
            affected.push(m.new_file.clone());
        }
        if !m.reuse_suggestion.is_empty() {
            suggestions.push(format!(
                "[{}] {}",
                m.new_symbol, m.reuse_suggestion
            ));
        }
    }

    let reason = format!(
        "{} potential duplication(s) detected:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );

    let fix = if suggestions.is_empty() {
        "Consider reusing existing code instead of duplicating functionality".to_string()
    } else {
        suggestions.join("\n")
    };

    let verdict = match config.enforce {
        ReuseEnforce::Block => PolicyVerdict::Block {
            reason,
            fix_suggestion: Some(fix),
        },
        ReuseEnforce::Gate => PolicyVerdict::Gate {
            reason,
            justification_required: true,
        },
        ReuseEnforce::Warn => PolicyVerdict::Allow,
    };

    PolicyDecision {
        policy_name: "reuse".to_string(),
        category: PolicyCategory::DuplicationReuse,
        verdict,
        operation: PolicyOperation::Checkpoint,
        actor: String::new(),
        task_id: None,
        exploration_id: None,
        affected_files: affected,
        escalation_context: None,
    }
}

/// Compute name similarity using normalized Levenshtein distance.
pub fn name_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let max_len = a.len().max(b.len());
    let distance = levenshtein_distance(a, b);
    1.0 - (distance as f64 / max_len as f64)
}

/// Compute signature similarity between two callable signatures.
///
/// Compares parameter count, parameter types, and return types.
/// Returns a similarity score from 0.0 to 1.0.
pub fn signature_similarity(
    a: &(Vec<(Option<String>, bool)>, Option<String>), // (params: [(type, optional)], return_type)
    b: &(Vec<(Option<String>, bool)>, Option<String>),
) -> f64 {
    let mut score = 0.0;
    let mut total = 0.0;

    // Parameter count similarity.
    let max_params = a.0.len().max(b.0.len());
    if max_params == 0 {
        score += 1.0;
    } else {
        let min_params = a.0.len().min(b.0.len());
        score += min_params as f64 / max_params as f64;
    }
    total += 1.0;

    // Parameter type matching (positional).
    let pairs = a.0.len().min(b.0.len());
    if pairs > 0 {
        let mut type_matches = 0.0;
        for i in 0..pairs {
            match (&a.0[i].0, &b.0[i].0) {
                (Some(ta), Some(tb)) if ta == tb => type_matches += 1.0,
                (Some(ta), Some(tb)) => type_matches += name_similarity(ta, tb) * 0.5,
                (None, None) => type_matches += 0.5, // both untyped
                _ => {}
            }
        }
        score += type_matches / max_params as f64;
    } else if max_params == 0 {
        score += 1.0; // both have zero params — fully matching
    }
    total += 1.0;

    // Return type matching.
    match (&a.1, &b.1) {
        (Some(ra), Some(rb)) if ra == rb => score += 1.0,
        (Some(ra), Some(rb)) => score += name_similarity(ra, rb) * 0.5,
        (None, None) => score += 0.5,
        _ => {}
    }
    total += 1.0;

    score / total
}

// ---------------------------------------------------------------------------
// Dependency & Compatibility types (12.5h)
// ---------------------------------------------------------------------------

/// Enforcement level for dependency checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DependencyEnforce {
    Block,
    Gate,
    Warn,
}

impl Default for DependencyEnforce {
    fn default() -> Self {
        Self::Block
    }
}

/// An approved package with allowed version range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedPackage {
    /// Package name (e.g. "serde", "react").
    pub name: String,
    /// Allowed version range (e.g. ">=1.0,<2.0" or "*" for any).
    pub version_range: String,
}

/// A detected dependency in a manifest file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedDependency {
    /// Package name.
    pub name: String,
    /// Declared version.
    pub version: String,
    /// Which manifest file declares it.
    pub manifest_file: String,
    /// License identifier (if detected).
    pub license: Option<String>,
    /// Known vulnerabilities (CVE IDs).
    pub vulnerabilities: Vec<String>,
}

/// Top-level dependency policy config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyPolicy {
    pub enabled: bool,
    pub enforce: DependencyEnforce,
    /// Approved packages (from .flock/approved-deps.toml).
    pub approved_packages: Vec<ApprovedPackage>,
    /// Blocked license identifiers (e.g. "GPL-3.0", "AGPL-3.0").
    pub license_blocklist: Vec<String>,
    /// Whether to check for known vulnerabilities.
    pub vuln_check: bool,
    /// Command to run consumer test suites when shared libraries are modified.
    pub consumer_test_command: String,
}

impl Default for DependencyPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            enforce: DependencyEnforce::default(),
            approved_packages: Vec::new(),
            license_blocklist: Vec::new(),
            vuln_check: false,
            consumer_test_command: String::new(),
        }
    }
}

/// Check detected dependencies against the dependency policy.
pub fn check_dependency_policy(
    config: &DependencyPolicy,
    dependencies: &[DetectedDependency],
) -> PolicyDecision {
    if !config.enabled {
        return PolicyDecision {
            policy_name: "dependencies".to_string(),
            category: PolicyCategory::DependencyCheck,
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

    for dep in dependencies {
        // Check against approved packages allowlist (if non-empty).
        if !config.approved_packages.is_empty() {
            let approved = config
                .approved_packages
                .iter()
                .any(|a| a.name == dep.name && version_matches(&dep.version, &a.version_range));
            if !approved {
                violations.push(format!(
                    "{}: package '{}@{}' is not in the approved packages list",
                    dep.manifest_file, dep.name, dep.version
                ));
                if !affected.contains(&dep.manifest_file) {
                    affected.push(dep.manifest_file.clone());
                }
            }
        }

        // Check license blocklist.
        if let Some(license) = &dep.license {
            if config
                .license_blocklist
                .iter()
                .any(|blocked| license.contains(blocked))
            {
                violations.push(format!(
                    "{}: package '{}' has blocked license '{}'",
                    dep.manifest_file, dep.name, license
                ));
                if !affected.contains(&dep.manifest_file) {
                    affected.push(dep.manifest_file.clone());
                }
            }
        }

        // Check for known vulnerabilities.
        if config.vuln_check && !dep.vulnerabilities.is_empty() {
            violations.push(format!(
                "{}: package '{}@{}' has known vulnerabilities: {}",
                dep.manifest_file,
                dep.name,
                dep.version,
                dep.vulnerabilities.join(", ")
            ));
            if !affected.contains(&dep.manifest_file) {
                affected.push(dep.manifest_file.clone());
            }
        }
    }

    if violations.is_empty() {
        return PolicyDecision {
            policy_name: "dependencies".to_string(),
            category: PolicyCategory::DependencyCheck,
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
        "{} dependency violation(s):\n  {}",
        violations.len(),
        violations.join("\n  ")
    );

    let verdict = match config.enforce {
        DependencyEnforce::Block => PolicyVerdict::Block {
            reason,
            fix_suggestion: Some(
                "Update .flock/approved-deps.toml or remove the offending dependency".to_string(),
            ),
        },
        DependencyEnforce::Gate => PolicyVerdict::Gate {
            reason,
            justification_required: true,
        },
        DependencyEnforce::Warn => PolicyVerdict::Allow,
    };

    PolicyDecision {
        policy_name: "dependencies".to_string(),
        category: PolicyCategory::DependencyCheck,
        verdict,
        operation: PolicyOperation::Checkpoint,
        actor: String::new(),
        task_id: None,
        exploration_id: None,
        affected_files: affected,
        escalation_context: None,
    }
}

/// Simple version matching. Supports "*" (any), exact match, and basic range
/// prefixes like ">=", "^", "~".
fn version_matches(actual: &str, range: &str) -> bool {
    if range == "*" || range.is_empty() {
        return true;
    }
    if range == actual {
        return true;
    }
    // Caret range: ^1.2.3 means >=1.2.3,<2.0.0 (simplified: major must match).
    if let Some(prefix) = range.strip_prefix('^') {
        let range_major = prefix.split('.').next().unwrap_or("");
        let actual_major = actual.split('.').next().unwrap_or("");
        return range_major == actual_major;
    }
    // Tilde range: ~1.2.3 means >=1.2.3,<1.3.0 (simplified: major.minor must match).
    if let Some(prefix) = range.strip_prefix('~') {
        let range_parts: Vec<&str> = prefix.split('.').collect();
        let actual_parts: Vec<&str> = actual.split('.').collect();
        return range_parts.first() == actual_parts.first()
            && range_parts.get(1) == actual_parts.get(1);
    }
    // >= range.
    if let Some(min) = range.strip_prefix(">=") {
        return actual >= min.trim();
    }
    false
}

/// Check coverage thresholds for modified modules.
pub fn check_coverage_threshold(
    requirements: &TestRequirements,
    coverage: Option<&CoverageResult>,
    modified_files: &[String],
) -> PolicyDecision {
    if !requirements.enabled {
        return PolicyDecision {
            policy_name: "coverage_threshold".to_string(),
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

    let min_pct = match requirements.min_coverage_percent {
        Some(pct) => pct as f64,
        None => {
            return PolicyDecision {
                policy_name: "coverage_threshold".to_string(),
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
    };

    let coverage = match coverage {
        Some(c) => c,
        None => {
            return make_test_decision(
                requirements,
                format!(
                    "Coverage data not available — min_coverage_percent={} required",
                    min_pct
                ),
            );
        }
    };

    let mut below_threshold: Vec<(String, f64)> = Vec::new();
    for (module, pct) in &coverage.module_coverage {
        // Only check modules that have modified files.
        let module_relevant = modified_files
            .iter()
            .any(|f| f.starts_with(module) || module.starts_with(f));
        if module_relevant && *pct < min_pct {
            below_threshold.push((module.clone(), *pct));
        }
    }

    if below_threshold.is_empty() {
        return PolicyDecision {
            policy_name: "coverage_threshold".to_string(),
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

    let details: Vec<String> = below_threshold
        .iter()
        .map(|(m, pct)| format!("{}: {:.1}% (min {}%)", m, pct, min_pct))
        .collect();
    let affected: Vec<String> = below_threshold.iter().map(|(m, _)| m.clone()).collect();

    let reason = format!(
        "Coverage below threshold for modified modules:\n  {}",
        details.join("\n  ")
    );

    let verdict = match requirements.on_failure {
        TestFailureAction::Block => PolicyVerdict::Block {
            reason,
            fix_suggestion: Some("Add tests to increase coverage for modified modules".to_string()),
        },
        TestFailureAction::Warn => PolicyVerdict::Allow,
        TestFailureAction::Gate => PolicyVerdict::Gate {
            reason,
            justification_required: true,
        },
    };

    PolicyDecision {
        policy_name: "coverage_threshold".to_string(),
        category: PolicyCategory::TestRequirement,
        verdict,
        operation: PolicyOperation::ExplorationPromote,
        actor: String::new(),
        task_id: None,
        exploration_id: None,
        affected_files: affected,
        escalation_context: None,
    }
}

// ---------------------------------------------------------------------------
// Regression detection & rollback types (12.5k)
// ---------------------------------------------------------------------------

/// Configuration for post-merge regression monitoring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegressionConfig {
    pub enabled: bool,
    /// Whether to monitor test results after merge/promotion.
    pub monitor_after_merge: bool,
    /// How long to monitor after merge (in seconds).
    pub monitor_window_secs: u64,
    /// Performance regression threshold (fractional, e.g. 0.1 = 10% slower).
    pub benchmark_threshold: f64,
}

impl Default for RegressionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            monitor_after_merge: true,
            monitor_window_secs: 3600,
            benchmark_threshold: 0.1,
        }
    }
}

/// Configuration for automatic rollback on regression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackConfig {
    pub enabled: bool,
    /// Whether to auto-rollback on detected regression.
    pub auto_rollback: bool,
    /// Whether test failures trigger rollback.
    pub rollback_on_test_failure: bool,
}

impl Default for RollbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_rollback: true,
            rollback_on_test_failure: true,
        }
    }
}

/// Detected regression after a merge/promotion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegressionDetection {
    /// The event (checkpoint/merge) that introduced the regression.
    pub source_event_id: Uuid,
    /// What kind of regression was detected.
    pub kind: RegressionKind,
    /// Human-readable description.
    pub description: String,
    /// Files implicated in the regression.
    pub affected_files: Vec<String>,
    /// Actor who made the change.
    pub originating_actor: String,
}

/// What type of regression was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegressionKind {
    /// Test suite failures.
    TestFailure,
    /// Performance benchmark regression.
    BenchmarkRegression,
}

/// Check whether a regression should trigger a rollback.
pub fn check_regression_policy(
    regression_config: &RegressionConfig,
    rollback_config: &RollbackConfig,
    regression: &RegressionDetection,
) -> PolicyDecision {
    if !regression_config.enabled {
        return PolicyDecision {
            policy_name: "regression".to_string(),
            category: PolicyCategory::Regression,
            verdict: PolicyVerdict::Allow,
            operation: PolicyOperation::ExplorationPromote,
            actor: String::new(),
            task_id: None,
            exploration_id: None,
            affected_files: Vec::new(),
            escalation_context: None,
        };
    }

    let should_rollback = rollback_config.enabled
        && rollback_config.auto_rollback
        && match regression.kind {
            RegressionKind::TestFailure => rollback_config.rollback_on_test_failure,
            RegressionKind::BenchmarkRegression => true,
        };

    let reason = format!(
        "Regression detected after merge: {} (source event: {}, actor: {})",
        regression.description,
        regression.source_event_id,
        regression.originating_actor
    );

    let verdict = if should_rollback {
        PolicyVerdict::Block {
            reason,
            fix_suggestion: Some(format!(
                "Automatic rollback will revert event {}. Originating agent will be notified for re-exploration.",
                regression.source_event_id
            )),
        }
    } else {
        PolicyVerdict::Gate {
            reason,
            justification_required: true,
        }
    };

    PolicyDecision {
        policy_name: "regression".to_string(),
        category: PolicyCategory::Regression,
        verdict,
        operation: PolicyOperation::ExplorationPromote,
        actor: regression.originating_actor.clone(),
        task_id: None,
        exploration_id: None,
        affected_files: regression.affected_files.clone(),
        escalation_context: None,
    }
}

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    let mut prev = (0..=n).collect::<Vec<_>>();
    let mut curr = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1)
                .min(curr[j - 1] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
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
            min_coverage_percent: None,
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
            min_coverage_percent: None,
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
            min_coverage_percent: None,
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
            min_coverage_percent: None,
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
            min_coverage_percent: None,
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
            min_coverage_percent: None,
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

    // --- Reuse / duplication prevention tests ---

    #[test]
    fn reuse_disabled_allows() {
        let config = ReusePolicy::default(); // enabled: false
        let matches = vec![DuplicationMatch {
            new_symbol: "function:foo".to_string(),
            new_file: "src/a.rs".to_string(),
            existing_symbol: "function:foo".to_string(),
            existing_file: "src/b.rs".to_string(),
            similarity: 0.95,
            layer: DuplicationLayer::Body,
            reuse_suggestion: "Import foo from src/b.rs".to_string(),
        }];
        let decision = check_reuse_policy(&config, &matches);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn reuse_no_matches_allows() {
        let config = ReusePolicy {
            enabled: true,
            ..ReusePolicy::default()
        };
        let decision = check_reuse_policy(&config, &[]);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn reuse_match_blocks() {
        let config = ReusePolicy {
            enabled: true,
            enforce: ReuseEnforce::Block,
            ..ReusePolicy::default()
        };
        let matches = vec![DuplicationMatch {
            new_symbol: "function:processPayment".to_string(),
            new_file: "src/checkout.rs".to_string(),
            existing_symbol: "function:handlePayment".to_string(),
            existing_file: "src/billing.rs".to_string(),
            similarity: 0.85,
            layer: DuplicationLayer::Signature,
            reuse_suggestion: "Reuse handlePayment from src/billing.rs".to_string(),
        }];
        let decision = check_reuse_policy(&config, &matches);
        assert!(decision.is_blocked());
        assert_eq!(decision.affected_files, vec!["src/checkout.rs"]);
    }

    #[test]
    fn reuse_match_gates() {
        let config = ReusePolicy {
            enabled: true,
            enforce: ReuseEnforce::Gate,
            ..ReusePolicy::default()
        };
        let matches = vec![DuplicationMatch {
            new_symbol: "function:calc".to_string(),
            new_file: "src/new.rs".to_string(),
            existing_symbol: "function:calculate".to_string(),
            existing_file: "src/old.rs".to_string(),
            similarity: 0.90,
            layer: DuplicationLayer::Body,
            reuse_suggestion: String::new(),
        }];
        let decision = check_reuse_policy(&config, &matches);
        assert!(decision.is_gated());
    }

    #[test]
    fn reuse_match_warns_allows() {
        let config = ReusePolicy {
            enabled: true,
            enforce: ReuseEnforce::Warn,
            ..ReusePolicy::default()
        };
        let matches = vec![DuplicationMatch {
            new_symbol: "function:foo".to_string(),
            new_file: "src/a.rs".to_string(),
            existing_symbol: "function:bar".to_string(),
            existing_file: "src/b.rs".to_string(),
            similarity: 0.95,
            layer: DuplicationLayer::Pattern,
            reuse_suggestion: "Use trait Bar".to_string(),
        }];
        let decision = check_reuse_policy(&config, &matches);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn name_similarity_identical() {
        assert_eq!(name_similarity("processPayment", "processPayment"), 1.0);
    }

    #[test]
    fn name_similarity_similar() {
        let sim = name_similarity("processPayment", "handlePayment");
        assert!(sim >= 0.4 && sim < 1.0, "similarity was {}", sim);
    }

    #[test]
    fn name_similarity_different() {
        let sim = name_similarity("processPayment", "renderDashboard");
        assert!(sim < 0.5, "similarity was {}", sim);
    }

    #[test]
    fn name_similarity_empty() {
        assert_eq!(name_similarity("", "anything"), 0.0);
        assert_eq!(name_similarity("anything", ""), 0.0);
    }

    #[test]
    fn signature_similarity_identical() {
        let sig_a = (
            vec![(Some("String".to_string()), false), (Some("i32".to_string()), false)],
            Some("bool".to_string()),
        );
        let sig_b = sig_a.clone();
        assert!((signature_similarity(&sig_a, &sig_b) - 1.0).abs() < 0.01);
    }

    #[test]
    fn signature_similarity_different_types() {
        let sig_a = (
            vec![(Some("String".to_string()), false)],
            Some("bool".to_string()),
        );
        let sig_b = (
            vec![(Some("i32".to_string()), false)],
            Some("Result".to_string()),
        );
        let sim = signature_similarity(&sig_a, &sig_b);
        assert!(sim < 0.8, "similarity was {}", sim);
    }

    #[test]
    fn signature_similarity_no_params() {
        let sig_a: (Vec<(Option<String>, bool)>, Option<String>) = (vec![], Some("String".to_string()));
        let sig_b: (Vec<(Option<String>, bool)>, Option<String>) = (vec![], Some("String".to_string()));
        assert!((signature_similarity(&sig_a, &sig_b) - 1.0).abs() < 0.01);
    }

    // --- Dependency policy tests ---

    #[test]
    fn dependency_disabled_allows() {
        let config = DependencyPolicy::default();
        let deps = vec![DetectedDependency {
            name: "evil-pkg".to_string(),
            version: "1.0.0".to_string(),
            manifest_file: "Cargo.toml".to_string(),
            license: Some("GPL-3.0".to_string()),
            vulnerabilities: vec!["CVE-2024-0001".to_string()],
        }];
        let decision = check_dependency_policy(&config, &deps);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn dependency_unapproved_package_blocks() {
        let config = DependencyPolicy {
            enabled: true,
            enforce: DependencyEnforce::Block,
            approved_packages: vec![ApprovedPackage {
                name: "serde".to_string(),
                version_range: "*".to_string(),
            }],
            license_blocklist: Vec::new(),
            vuln_check: false,
            consumer_test_command: String::new(),
        };
        let deps = vec![DetectedDependency {
            name: "unknown-pkg".to_string(),
            version: "1.0.0".to_string(),
            manifest_file: "Cargo.toml".to_string(),
            license: None,
            vulnerabilities: Vec::new(),
        }];
        let decision = check_dependency_policy(&config, &deps);
        assert!(decision.is_blocked());
    }

    #[test]
    fn dependency_approved_package_allows() {
        let config = DependencyPolicy {
            enabled: true,
            enforce: DependencyEnforce::Block,
            approved_packages: vec![ApprovedPackage {
                name: "serde".to_string(),
                version_range: "^1.0".to_string(),
            }],
            license_blocklist: Vec::new(),
            vuln_check: false,
            consumer_test_command: String::new(),
        };
        let deps = vec![DetectedDependency {
            name: "serde".to_string(),
            version: "1.0.200".to_string(),
            manifest_file: "Cargo.toml".to_string(),
            license: None,
            vulnerabilities: Vec::new(),
        }];
        let decision = check_dependency_policy(&config, &deps);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn dependency_blocked_license() {
        let config = DependencyPolicy {
            enabled: true,
            enforce: DependencyEnforce::Block,
            approved_packages: Vec::new(),
            license_blocklist: vec!["GPL-3.0".to_string(), "AGPL-3.0".to_string()],
            vuln_check: false,
            consumer_test_command: String::new(),
        };
        let deps = vec![DetectedDependency {
            name: "copyleft-lib".to_string(),
            version: "2.0".to_string(),
            manifest_file: "package.json".to_string(),
            license: Some("GPL-3.0-or-later".to_string()),
            vulnerabilities: Vec::new(),
        }];
        let decision = check_dependency_policy(&config, &deps);
        assert!(decision.is_blocked());
    }

    #[test]
    fn dependency_vulnerability_blocks() {
        let config = DependencyPolicy {
            enabled: true,
            enforce: DependencyEnforce::Block,
            approved_packages: Vec::new(),
            license_blocklist: Vec::new(),
            vuln_check: true,
            consumer_test_command: String::new(),
        };
        let deps = vec![DetectedDependency {
            name: "vuln-pkg".to_string(),
            version: "0.1.0".to_string(),
            manifest_file: "Cargo.toml".to_string(),
            license: None,
            vulnerabilities: vec!["CVE-2024-1234".to_string()],
        }];
        let decision = check_dependency_policy(&config, &deps);
        assert!(decision.is_blocked());
    }

    #[test]
    fn dependency_gate_mode() {
        let config = DependencyPolicy {
            enabled: true,
            enforce: DependencyEnforce::Gate,
            approved_packages: vec![ApprovedPackage {
                name: "allowed".to_string(),
                version_range: "*".to_string(),
            }],
            license_blocklist: Vec::new(),
            vuln_check: false,
            consumer_test_command: String::new(),
        };
        let deps = vec![DetectedDependency {
            name: "new-dep".to_string(),
            version: "1.0.0".to_string(),
            manifest_file: "Cargo.toml".to_string(),
            license: None,
            vulnerabilities: Vec::new(),
        }];
        let decision = check_dependency_policy(&config, &deps);
        assert!(decision.is_gated());
    }

    // --- Version matching tests ---

    #[test]
    fn version_match_wildcard() {
        assert!(version_matches("1.2.3", "*"));
        assert!(version_matches("0.0.1", ""));
    }

    #[test]
    fn version_match_exact() {
        assert!(version_matches("1.2.3", "1.2.3"));
        assert!(!version_matches("1.2.4", "1.2.3"));
    }

    #[test]
    fn version_match_caret() {
        assert!(version_matches("1.5.0", "^1.0"));
        assert!(!version_matches("2.0.0", "^1.0"));
    }

    #[test]
    fn version_match_tilde() {
        assert!(version_matches("1.2.9", "~1.2"));
        assert!(!version_matches("1.3.0", "~1.2"));
    }

    #[test]
    fn version_match_gte() {
        assert!(version_matches("2.0.0", ">=1.0"));
        assert!(!version_matches("0.9.0", ">=1.0"));
    }

    // --- Coverage threshold tests ---

    #[test]
    fn coverage_disabled_allows() {
        let req = TestRequirements::default();
        let decision = check_coverage_threshold(&req, None, &[]);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn coverage_no_threshold_allows() {
        let req = TestRequirements {
            enabled: true,
            min_coverage_percent: None,
            ..TestRequirements::default()
        };
        let decision = check_coverage_threshold(&req, None, &[]);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn coverage_below_threshold_blocks() {
        let req = TestRequirements {
            enabled: true,
            min_coverage_percent: Some(80),
            on_failure: TestFailureAction::Block,
            ..TestRequirements::default()
        };
        let coverage = CoverageResult {
            module_coverage: vec![("src/auth".to_string(), 45.0)],
            overall_percent: 45.0,
        };
        let modified = vec!["src/auth/login.rs".to_string()];
        let decision = check_coverage_threshold(&req, Some(&coverage), &modified);
        assert!(decision.is_blocked());
    }

    #[test]
    fn coverage_above_threshold_allows() {
        let req = TestRequirements {
            enabled: true,
            min_coverage_percent: Some(80),
            on_failure: TestFailureAction::Block,
            ..TestRequirements::default()
        };
        let coverage = CoverageResult {
            module_coverage: vec![("src/auth".to_string(), 90.0)],
            overall_percent: 90.0,
        };
        let modified = vec!["src/auth/login.rs".to_string()];
        let decision = check_coverage_threshold(&req, Some(&coverage), &modified);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn coverage_no_data_blocks() {
        let req = TestRequirements {
            enabled: true,
            min_coverage_percent: Some(80),
            on_failure: TestFailureAction::Block,
            ..TestRequirements::default()
        };
        let modified = vec!["src/auth/login.rs".to_string()];
        let decision = check_coverage_threshold(&req, None, &modified);
        assert!(decision.is_blocked());
    }

    // --- Regression policy tests ---

    #[test]
    fn regression_disabled_allows() {
        let reg_config = RegressionConfig::default();
        let rollback_config = RollbackConfig::default();
        let regression = RegressionDetection {
            source_event_id: test_task_id(),
            kind: RegressionKind::TestFailure,
            description: "tests failed".to_string(),
            affected_files: vec!["src/main.rs".to_string()],
            originating_actor: "agent-1".to_string(),
        };
        let decision = check_regression_policy(&reg_config, &rollback_config, &regression);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
    }

    #[test]
    fn regression_test_failure_with_rollback_blocks() {
        let reg_config = RegressionConfig {
            enabled: true,
            monitor_after_merge: true,
            monitor_window_secs: 3600,
            benchmark_threshold: 0.1,
        };
        let rollback_config = RollbackConfig {
            enabled: true,
            auto_rollback: true,
            rollback_on_test_failure: true,
        };
        let regression = RegressionDetection {
            source_event_id: test_task_id(),
            kind: RegressionKind::TestFailure,
            description: "3 tests failed".to_string(),
            affected_files: vec!["src/lib.rs".to_string()],
            originating_actor: "agent-2".to_string(),
        };
        let decision = check_regression_policy(&reg_config, &rollback_config, &regression);
        assert!(decision.is_blocked());
    }

    #[test]
    fn regression_without_rollback_gates() {
        let reg_config = RegressionConfig {
            enabled: true,
            monitor_after_merge: true,
            monitor_window_secs: 3600,
            benchmark_threshold: 0.1,
        };
        let rollback_config = RollbackConfig {
            enabled: false,
            auto_rollback: false,
            rollback_on_test_failure: false,
        };
        let regression = RegressionDetection {
            source_event_id: test_task_id(),
            kind: RegressionKind::TestFailure,
            description: "tests failed".to_string(),
            affected_files: Vec::new(),
            originating_actor: "agent-1".to_string(),
        };
        let decision = check_regression_policy(&reg_config, &rollback_config, &regression);
        assert!(decision.is_gated());
    }

    #[test]
    fn regression_benchmark_with_rollback_blocks() {
        let reg_config = RegressionConfig {
            enabled: true,
            monitor_after_merge: true,
            monitor_window_secs: 3600,
            benchmark_threshold: 0.1,
        };
        let rollback_config = RollbackConfig {
            enabled: true,
            auto_rollback: true,
            rollback_on_test_failure: false, // only test failures disabled
        };
        let regression = RegressionDetection {
            source_event_id: test_task_id(),
            kind: RegressionKind::BenchmarkRegression,
            description: "15% slower".to_string(),
            affected_files: vec!["src/engine.rs".to_string()],
            originating_actor: "agent-3".to_string(),
        };
        let decision = check_regression_policy(&reg_config, &rollback_config, &regression);
        assert!(decision.is_blocked());
    }
}
