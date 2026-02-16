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

    PolicyDecision {
        policy_name: "budget".to_string(),
        category: PolicyCategory::Budget,
        verdict: PolicyVerdict::Allow,
        operation: PolicyOperation::Checkpoint,
        actor: String::new(),
        task_id: Some(usage.task_id),
        exploration_id: usage.exploration_id,
        affected_files: Vec::new(),
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
        };
    }

    // Check explorations per task.
    if let Some(max) = limits.max_explorations_per_task {
        if usage.explorations_started > max {
            return make_rate_limit_decision(
                limits,
                usage,
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
    }
}

fn make_rate_limit_decision(
    limits: &RateLimits,
    usage: &RateLimitUsage,
    reason: String,
    operation: PolicyOperation,
) -> PolicyDecision {
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

    PolicyDecision {
        policy_name: "rate_limit".to_string(),
        category: PolicyCategory::RateLimit,
        verdict,
        operation,
        actor: String::new(),
        task_id: Some(usage.task_id),
        exploration_id: usage.exploration_id,
        affected_files: Vec::new(),
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
            on_exceed: BudgetExceedAction::Block,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 5,
            lines_changed: 200,
            exploration_id: None,
            exploration_files_modified: 0,
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
            on_exceed: BudgetExceedAction::Block,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 6,
            lines_changed: 0,
            exploration_id: None,
            exploration_files_modified: 0,
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
            on_exceed: BudgetExceedAction::Warn,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 10,
            lines_changed: 0,
            exploration_id: None,
            exploration_files_modified: 0,
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
            on_exceed: BudgetExceedAction::Block,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 2,
            lines_changed: 0,
            exploration_id: Some(test_exploration_id()),
            exploration_files_modified: 4,
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
            on_exceed: BudgetExceedAction::PauseAndFlag,
        };
        let usage = BudgetUsage {
            task_id: test_task_id(),
            files_modified: 0,
            lines_changed: 150,
            exploration_id: None,
            exploration_files_modified: 0,
        };
        let decision = check_budget_limits(&limits, &usage);
        assert!(decision.is_gated());
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
        };
        assert!(!gated.is_blocked());
        assert!(gated.is_gated());
    }
}
