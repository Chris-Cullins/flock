use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::Result;

/// A single hook definition from `.flock/hooks.toml`.
#[derive(Debug, Clone)]
pub struct HookDef {
    pub name: String,
    pub hook_point: String,
    pub command: String,
    pub block_on_failure: bool,
    pub timeout_secs: u64,
}

/// Configuration for hooks, parsed from `.flock/hooks.toml`.
#[derive(Debug, Clone)]
pub struct HooksConfig {
    pub hooks: Vec<HookDef>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self { hooks: Vec::new() }
    }
}

/// Result of running a single hook.
#[derive(Debug, Clone)]
pub struct HookResult {
    pub name: String,
    pub hook_point: String,
    pub command: String,
    pub success: bool,
    pub duration_ms: u64,
    pub output: Option<String>,
    pub structured_output: Option<serde_json::Value>,
}

/// Result of running all hooks for a given hook point.
#[derive(Debug, Clone)]
pub struct HookReport {
    pub hook_point: String,
    pub results: Vec<HookResult>,
    pub blocked: bool,
}

impl HookReport {
    pub fn has_failures(&self) -> bool {
        self.results.iter().any(|r| !r.success)
    }
}

/// Default content for a new `.flock/hooks.toml` file.
pub const DEFAULT_HOOKS_TOML: &str = r#"# Hook configuration for Flock.
# Hooks run automatically during operations — no manual setup required.
# Everyone who clones this repo gets the same hooks — no installation step.
#
# Hook points:
#   pre-commit, post-commit, pre-push, post-push,
#   pre-merge, post-merge, pre-explore, post-explore-promote
#
# Environment variables available to hooks:
#   FL_ACTOR    — the current actor (user or agent name)
#   FL_ROOT     — the repository root path
#   FL_HOOK     — the hook point being executed (e.g. "pre-commit")
#
# Hooks can emit JSON to stdout for structured error messages:
#   {"message": "lint failed", "details": ["src/main.rs:10: unused variable"]}
#
# To skip hooks for a single operation, use --skip-hooks (recorded in audit log).
# There is no --no-verify escape hatch.
#
# Example:
# [[hooks]]
# name = "lint"
# hook_point = "pre-commit"
# command = "npm run lint"
# block_on_failure = true
# timeout_secs = 60
#
# [[hooks]]
# name = "tests"
# hook_point = "pre-commit"
# command = "cargo test"
# block_on_failure = true
# timeout_secs = 300
#
# [[hooks]]
# name = "notify"
# hook_point = "post-commit"
# command = "scripts/notify.sh"
# block_on_failure = false
# timeout_secs = 10
"#;

/// Parse a `hooks.toml` file into a `HooksConfig`.
pub fn parse_hooks_config(content: &str) -> HooksConfig {
    let mut config = HooksConfig::default();

    let mut in_hook = false;
    let mut current_name: Option<String> = None;
    let mut current_hook_point: Option<String> = None;
    let mut current_command: Option<String> = None;
    let mut current_block_on_failure: bool = true;
    let mut current_timeout_secs: u64 = 60;

    let flush = |config: &mut HooksConfig,
                 name: &mut Option<String>,
                 hook_point: &mut Option<String>,
                 command: &mut Option<String>,
                 block: &mut bool,
                 timeout: &mut u64| {
        if let (Some(n), Some(hp), Some(cmd)) = (name.take(), hook_point.take(), command.take()) {
            config.hooks.push(HookDef {
                name: n,
                hook_point: hp,
                command: cmd,
                block_on_failure: *block,
                timeout_secs: *timeout,
            });
        }
        *block = true;
        *timeout = 60;
    };

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed == "[[hooks]]" {
            flush(
                &mut config,
                &mut current_name,
                &mut current_hook_point,
                &mut current_command,
                &mut current_block_on_failure,
                &mut current_timeout_secs,
            );
            in_hook = true;
            continue;
        }

        if trimmed.starts_with('[') {
            // New section that isn't [[hooks]] — flush and stop hook parsing
            flush(
                &mut config,
                &mut current_name,
                &mut current_hook_point,
                &mut current_command,
                &mut current_block_on_failure,
                &mut current_timeout_secs,
            );
            in_hook = false;
            continue;
        }

        if !in_hook {
            continue;
        }

        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        match key {
            "name" => {
                current_name = Some(parse_toml_string(value));
            }
            "hook_point" => {
                current_hook_point = Some(parse_toml_string(value));
            }
            "command" => {
                current_command = Some(parse_toml_string(value));
            }
            "block_on_failure" => {
                current_block_on_failure = value == "true";
            }
            "timeout_secs" => {
                current_timeout_secs = value.parse().unwrap_or(60);
            }
            _ => {}
        }
    }

    // Flush last hook if any.
    flush(
        &mut config,
        &mut current_name,
        &mut current_hook_point,
        &mut current_command,
        &mut current_block_on_failure,
        &mut current_timeout_secs,
    );

    config
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

/// Load hooks config from `.flock/hooks.toml`, falling back to defaults (no hooks).
pub fn load_hooks_config(root: &Path) -> HooksConfig {
    let config_path = root.join(".flock/hooks.toml");
    match fs::read_to_string(&config_path) {
        Ok(content) => parse_hooks_config(&content),
        Err(_) => HooksConfig::default(),
    }
}

/// Execute all hooks matching the given hook point.
///
/// Returns a `HookReport` with results for each hook.
/// If any blocking hook fails, `report.blocked` is true.
pub fn execute_hooks(
    root: &Path,
    hook_point: &str,
    config: &HooksConfig,
) -> Result<HookReport> {
    let matching: Vec<&HookDef> = config
        .hooks
        .iter()
        .filter(|h| h.hook_point == hook_point)
        .collect();

    let mut results = Vec::new();
    let mut blocked = false;

    let actor = std::env::var("FL_ACTOR")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());

    for hook in &matching {
        let start = Instant::now();

        let output = Command::new("sh")
            .arg("-c")
            .arg(&hook.command)
            .current_dir(root)
            .env("FL_ACTOR", &actor)
            .env("FL_ROOT", root.to_string_lossy().as_ref())
            .env("FL_HOOK", hook_point)
            .output();

        let duration_ms = start.elapsed().as_millis() as u64;

        match output {
            Ok(out) => {
                let success = out.status.success();
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();

                let combined = if stderr.is_empty() {
                    if stdout.is_empty() { None } else { Some(stdout.clone()) }
                } else if stdout.is_empty() {
                    Some(stderr)
                } else {
                    Some(format!("{}\n{}", stdout, stderr))
                };

                // Try to parse stdout as JSON for structured output.
                let structured = serde_json::from_str::<serde_json::Value>(&stdout).ok();

                if !success && hook.block_on_failure {
                    blocked = true;
                }

                results.push(HookResult {
                    name: hook.name.clone(),
                    hook_point: hook_point.to_string(),
                    command: hook.command.clone(),
                    success,
                    duration_ms,
                    output: combined,
                    structured_output: structured,
                });
            }
            Err(e) => {
                if hook.block_on_failure {
                    blocked = true;
                }
                results.push(HookResult {
                    name: hook.name.clone(),
                    hook_point: hook_point.to_string(),
                    command: hook.command.clone(),
                    success: false,
                    duration_ms,
                    output: Some(format!("failed to execute hook: {}", e)),
                    structured_output: None,
                });
            }
        }
    }

    Ok(HookReport {
        hook_point: hook_point.to_string(),
        results,
        blocked,
    })
}

/// Format a hook report into a human-readable message.
pub fn format_hook_report(report: &HookReport) -> String {
    if report.results.is_empty() {
        return String::new();
    }

    let mut msg = String::new();
    for result in &report.results {
        let status = if result.success { "pass" } else { "FAIL" };
        msg.push_str(&format!(
            "  hook [{}] {} — {} ({}ms)\n",
            result.hook_point, result.name, status, result.duration_ms
        ));

        if !result.success {
            if let Some(structured) = &result.structured_output {
                // Show structured JSON message if available.
                if let Some(message) = structured.get("message").and_then(|v| v.as_str()) {
                    msg.push_str(&format!("    message: {}\n", message));
                }
                if let Some(details) = structured.get("details").and_then(|v| v.as_array()) {
                    for detail in details {
                        if let Some(s) = detail.as_str() {
                            msg.push_str(&format!("    - {}\n", s));
                        }
                    }
                }
            } else if let Some(output) = &result.output {
                // Show raw output, indented.
                for line in output.lines().take(20) {
                    msg.push_str(&format!("    {}\n", line));
                }
                let total_lines = output.lines().count();
                if total_lines > 20 {
                    msg.push_str(&format!("    ... ({} more lines)\n", total_lines - 20));
                }
            }
        }
    }

    if report.blocked {
        msg.push_str("\nHook failure blocked the operation. To skip hooks, use --skip-hooks (recorded in audit log).\n");
    }

    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let config = HooksConfig::default();
        assert!(config.hooks.is_empty());
    }

    #[test]
    fn test_parse_hooks_config_empty() {
        let toml = "# no hooks configured\n";
        let config = parse_hooks_config(toml);
        assert!(config.hooks.is_empty());
    }

    #[test]
    fn test_parse_hooks_config_single_hook() {
        let toml = r#"
[[hooks]]
name = "lint"
hook_point = "pre-commit"
command = "npm run lint"
block_on_failure = true
timeout_secs = 120
"#;
        let config = parse_hooks_config(toml);
        assert_eq!(config.hooks.len(), 1);
        let hook = &config.hooks[0];
        assert_eq!(hook.name, "lint");
        assert_eq!(hook.hook_point, "pre-commit");
        assert_eq!(hook.command, "npm run lint");
        assert!(hook.block_on_failure);
        assert_eq!(hook.timeout_secs, 120);
    }

    #[test]
    fn test_parse_hooks_config_multiple_hooks() {
        let toml = r#"
[[hooks]]
name = "lint"
hook_point = "pre-commit"
command = "npm run lint"
block_on_failure = true
timeout_secs = 60

[[hooks]]
name = "tests"
hook_point = "pre-commit"
command = "cargo test"
block_on_failure = true
timeout_secs = 300

[[hooks]]
name = "notify"
hook_point = "post-commit"
command = "scripts/notify.sh"
block_on_failure = false
timeout_secs = 10
"#;
        let config = parse_hooks_config(toml);
        assert_eq!(config.hooks.len(), 3);
        assert_eq!(config.hooks[0].name, "lint");
        assert_eq!(config.hooks[1].name, "tests");
        assert_eq!(config.hooks[2].name, "notify");
        assert_eq!(config.hooks[2].hook_point, "post-commit");
        assert!(!config.hooks[2].block_on_failure);
    }

    #[test]
    fn test_parse_hooks_config_defaults() {
        let toml = r#"
[[hooks]]
name = "check"
hook_point = "pre-commit"
command = "make check"
"#;
        let config = parse_hooks_config(toml);
        assert_eq!(config.hooks.len(), 1);
        let hook = &config.hooks[0];
        // block_on_failure defaults to true
        assert!(hook.block_on_failure);
        // timeout defaults to 60
        assert_eq!(hook.timeout_secs, 60);
    }

    #[test]
    fn test_execute_hooks_passing() {
        let dir = TempDir::new().unwrap();
        let config = HooksConfig {
            hooks: vec![HookDef {
                name: "echo-test".to_string(),
                hook_point: "pre-commit".to_string(),
                command: "echo hello".to_string(),
                block_on_failure: true,
                timeout_secs: 10,
            }],
        };

        let report = execute_hooks(dir.path(), "pre-commit", &config).unwrap();
        assert_eq!(report.results.len(), 1);
        assert!(report.results[0].success);
        assert!(!report.blocked);
    }

    #[test]
    fn test_execute_hooks_failing_blocks() {
        let dir = TempDir::new().unwrap();
        let config = HooksConfig {
            hooks: vec![HookDef {
                name: "fail-test".to_string(),
                hook_point: "pre-commit".to_string(),
                command: "exit 1".to_string(),
                block_on_failure: true,
                timeout_secs: 10,
            }],
        };

        let report = execute_hooks(dir.path(), "pre-commit", &config).unwrap();
        assert_eq!(report.results.len(), 1);
        assert!(!report.results[0].success);
        assert!(report.blocked);
    }

    #[test]
    fn test_execute_hooks_failing_non_blocking() {
        let dir = TempDir::new().unwrap();
        let config = HooksConfig {
            hooks: vec![HookDef {
                name: "warn-test".to_string(),
                hook_point: "post-commit".to_string(),
                command: "exit 1".to_string(),
                block_on_failure: false,
                timeout_secs: 10,
            }],
        };

        let report = execute_hooks(dir.path(), "post-commit", &config).unwrap();
        assert_eq!(report.results.len(), 1);
        assert!(!report.results[0].success);
        assert!(!report.blocked);
    }

    #[test]
    fn test_execute_hooks_filters_by_hook_point() {
        let dir = TempDir::new().unwrap();
        let config = HooksConfig {
            hooks: vec![
                HookDef {
                    name: "pre-hook".to_string(),
                    hook_point: "pre-commit".to_string(),
                    command: "echo pre".to_string(),
                    block_on_failure: true,
                    timeout_secs: 10,
                },
                HookDef {
                    name: "post-hook".to_string(),
                    hook_point: "post-commit".to_string(),
                    command: "echo post".to_string(),
                    block_on_failure: false,
                    timeout_secs: 10,
                },
            ],
        };

        let report = execute_hooks(dir.path(), "pre-commit", &config).unwrap();
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].name, "pre-hook");
    }

    #[test]
    fn test_execute_hooks_structured_json_output() {
        let dir = TempDir::new().unwrap();
        let config = HooksConfig {
            hooks: vec![HookDef {
                name: "json-hook".to_string(),
                hook_point: "pre-commit".to_string(),
                command: r#"echo '{"message":"lint failed","details":["src/main.rs:10: unused"]}'"#
                    .to_string(),
                block_on_failure: false,
                timeout_secs: 10,
            }],
        };

        let report = execute_hooks(dir.path(), "pre-commit", &config).unwrap();
        assert_eq!(report.results.len(), 1);
        assert!(report.results[0].structured_output.is_some());
        let json = report.results[0].structured_output.as_ref().unwrap();
        assert_eq!(json["message"], "lint failed");
    }

    #[test]
    fn test_execute_hooks_environment_variables() {
        let dir = TempDir::new().unwrap();
        let config = HooksConfig {
            hooks: vec![HookDef {
                name: "env-test".to_string(),
                hook_point: "pre-commit".to_string(),
                command: "echo $FL_HOOK".to_string(),
                block_on_failure: true,
                timeout_secs: 10,
            }],
        };

        let report = execute_hooks(dir.path(), "pre-commit", &config).unwrap();
        assert!(report.results[0].success);
        let output = report.results[0].output.as_ref().unwrap();
        assert!(output.trim().contains("pre-commit"));
    }

    #[test]
    fn test_load_hooks_config_missing_file() {
        let dir = TempDir::new().unwrap();
        let config = load_hooks_config(dir.path());
        assert!(config.hooks.is_empty());
    }

    #[test]
    fn test_load_hooks_config_from_file() {
        let dir = TempDir::new().unwrap();
        let flock_dir = dir.path().join(".flock");
        fs::create_dir_all(&flock_dir).unwrap();
        let hooks_path = flock_dir.join("hooks.toml");
        fs::write(
            &hooks_path,
            r#"
[[hooks]]
name = "check"
hook_point = "pre-commit"
command = "make check"
block_on_failure = true
timeout_secs = 30
"#,
        )
        .unwrap();

        let config = load_hooks_config(dir.path());
        assert_eq!(config.hooks.len(), 1);
        assert_eq!(config.hooks[0].name, "check");
    }

    #[test]
    fn test_format_hook_report_empty() {
        let report = HookReport {
            hook_point: "pre-commit".to_string(),
            results: Vec::new(),
            blocked: false,
        };
        assert!(format_hook_report(&report).is_empty());
    }

    #[test]
    fn test_format_hook_report_with_failure() {
        let report = HookReport {
            hook_point: "pre-commit".to_string(),
            results: vec![HookResult {
                name: "lint".to_string(),
                hook_point: "pre-commit".to_string(),
                command: "npm run lint".to_string(),
                success: false,
                duration_ms: 1500,
                output: Some("error: unused variable\n".to_string()),
                structured_output: None,
            }],
            blocked: true,
        };
        let msg = format_hook_report(&report);
        assert!(msg.contains("FAIL"));
        assert!(msg.contains("lint"));
        assert!(msg.contains("--skip-hooks"));
    }

    #[test]
    fn test_format_hook_report_structured_output() {
        let report = HookReport {
            hook_point: "pre-commit".to_string(),
            results: vec![HookResult {
                name: "lint".to_string(),
                hook_point: "pre-commit".to_string(),
                command: "npm run lint".to_string(),
                success: false,
                duration_ms: 500,
                output: None,
                structured_output: Some(serde_json::json!({
                    "message": "lint failed",
                    "details": ["src/main.rs:10: unused variable"]
                })),
            }],
            blocked: true,
        };
        let msg = format_hook_report(&report);
        assert!(msg.contains("lint failed"));
        assert!(msg.contains("src/main.rs:10"));
    }
}
