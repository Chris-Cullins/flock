use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use regex::Regex;

/// A single secret finding in a file.
#[derive(Debug, Clone)]
pub struct SecretFinding {
    pub file: PathBuf,
    pub line: usize,
    pub pattern_name: String,
    pub matched_text: String,
}

/// Result of scanning files for secrets.
#[derive(Debug, Clone)]
pub struct SecretScanReport {
    pub findings: Vec<SecretFinding>,
}

impl SecretScanReport {
    pub fn has_secrets(&self) -> bool {
        !self.findings.is_empty()
    }
}

/// Configuration for secret scanning, parsed from `.flock/secrets.toml`.
#[derive(Debug, Clone)]
pub struct SecretsConfig {
    /// Whether to block (true) or just warn (false) on detected secrets.
    pub block: bool,
    /// Custom patterns: name -> regex string.
    pub custom_patterns: HashMap<String, String>,
    /// Paths that are allowed to contain secrets (e.g. test fixtures).
    pub allowed_paths: Vec<String>,
    /// Built-in pattern names to disable.
    pub disabled_patterns: Vec<String>,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            block: true,
            custom_patterns: HashMap::new(),
            allowed_paths: Vec::new(),
            disabled_patterns: Vec::new(),
        }
    }
}

/// Default content for a new `.flock/secrets.toml` file.
pub const DEFAULT_SECRETS_TOML: &str = r#"# Secret detection configuration for Flock.
# Secrets are scanned during `fl checkpoint` and blocked by default.

# Set to false to warn instead of blocking commits with secrets.
block = true

# Paths allowed to contain secrets (glob patterns, relative to repo root).
# Example: allowed_paths = ["tests/fixtures/**", "docs/examples/**"]
allowed_paths = []

# Built-in pattern names to disable.
# Available: aws_access_key, aws_secret_key, gcp_api_key, azure_key,
#            openai_key, github_token, github_fine_grained_token,
#            private_key_pem, generic_secret_assignment, generic_password_assignment,
#            generic_token_assignment, slack_token, stripe_key, twilio_key,
#            sendgrid_key, database_url_with_password
# Example: disabled_patterns = ["generic_secret_assignment"]
disabled_patterns = []

# Custom patterns: each entry is a table with 'name' and 'pattern' (regex).
# [[custom_patterns]]
# name = "my_internal_key"
# pattern = "MY_KEY_[A-Za-z0-9]{32}"
"#;

/// Built-in secret patterns.
struct BuiltinPattern {
    name: &'static str,
    pattern: &'static str,
}

const BUILTIN_PATTERNS: &[BuiltinPattern] = &[
    // AWS
    BuiltinPattern {
        name: "aws_access_key",
        pattern: r"(?:^|[^A-Za-z0-9])AKIA[0-9A-Z]{16}(?:[^A-Za-z0-9]|$)",
    },
    BuiltinPattern {
        name: "aws_secret_key",
        pattern: r#"(?i)aws[_-]?secret[_-]?access[_-]?key[\s]*[=:]\s*['"]?[A-Za-z0-9/+=]{40}"#,
    },
    // GCP
    BuiltinPattern {
        name: "gcp_api_key",
        pattern: r"AIza[0-9A-Za-z_-]{35}",
    },
    // Azure
    BuiltinPattern {
        name: "azure_key",
        pattern: r#"(?i)(?:azure|az)[_-]?(?:storage|account|subscription)?[_-]?key[\s]*[=:]\s*['"]?[A-Za-z0-9+/]{44,88}={0,2}"#,
    },
    // OpenAI
    BuiltinPattern {
        name: "openai_key",
        pattern: r"sk-[A-Za-z0-9]{20,}T3BlbkFJ[A-Za-z0-9]{20,}",
    },
    // GitHub
    BuiltinPattern {
        name: "github_token",
        pattern: r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}",
    },
    BuiltinPattern {
        name: "github_fine_grained_token",
        pattern: r"github_pat_[A-Za-z0-9]{22}_[A-Za-z0-9]{59}",
    },
    // Private keys
    BuiltinPattern {
        name: "private_key_pem",
        pattern: r"-----BEGIN[A-Z ]*PRIVATE KEY-----",
    },
    // Generic secrets in assignments
    BuiltinPattern {
        name: "generic_secret_assignment",
        pattern: r#"(?i)(?:secret|api_secret|client_secret|app_secret)[\s]*[=:]\s*['\"][^\s'"]{8,}['\"]"#,
    },
    BuiltinPattern {
        name: "generic_password_assignment",
        pattern: r#"(?i)(?:password|passwd|pwd)[\s]*[=:]\s*['\"][^\s'"]{8,}['\"]"#,
    },
    BuiltinPattern {
        name: "generic_token_assignment",
        pattern: r#"(?i)(?:token|auth_token|access_token|bearer)[\s]*[=:]\s*['\"][^\s'"]{8,}['\"]"#,
    },
    // Slack
    BuiltinPattern {
        name: "slack_token",
        pattern: r"xox[bpors]-[0-9]{10,13}-[0-9]{10,13}[a-zA-Z0-9-]*",
    },
    // Stripe
    BuiltinPattern {
        name: "stripe_key",
        pattern: r"(?:sk|pk)_(?:live|test)_[0-9a-zA-Z]{24,}",
    },
    // Twilio
    BuiltinPattern {
        name: "twilio_key",
        pattern: r"SK[0-9a-fA-F]{32}",
    },
    // SendGrid
    BuiltinPattern {
        name: "sendgrid_key",
        pattern: r"SG\.[A-Za-z0-9_-]{22,}\.[A-Za-z0-9_-]{22,}",
    },
    // Database URL with password
    BuiltinPattern {
        name: "database_url_with_password",
        pattern: r"(?i)(?:mysql|postgres|postgresql|mongodb|redis)://[^:]+:[^@\s]{3,}@",
    },
];

/// Parse a `secrets.toml` file into a `SecretsConfig`.
pub fn parse_secrets_config(content: &str) -> SecretsConfig {
    let mut config = SecretsConfig::default();

    let mut in_custom_pattern = false;
    let mut current_name: Option<String> = None;
    let mut current_pattern: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed == "[[custom_patterns]]" {
            // Flush any previous custom pattern.
            if let (Some(n), Some(p)) = (current_name.take(), current_pattern.take()) {
                config.custom_patterns.insert(n, p);
            }
            in_custom_pattern = true;
            continue;
        }

        // A new top-level key resets custom_pattern context (unless it's a
        // custom_pattern field).
        if !trimmed.starts_with("name") && !trimmed.starts_with("pattern") && !trimmed.starts_with('[') {
            if in_custom_pattern {
                if let (Some(n), Some(p)) = (current_name.take(), current_pattern.take()) {
                    config.custom_patterns.insert(n, p);
                }
                in_custom_pattern = false;
            }
        }

        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        if in_custom_pattern {
            match key {
                "name" => {
                    current_name = Some(parse_toml_string(value));
                }
                "pattern" => {
                    current_pattern = Some(parse_toml_string(value));
                }
                _ => {}
            }
            continue;
        }

        match key {
            "block" => {
                config.block = value == "true";
            }
            "allowed_paths" => {
                config.allowed_paths = parse_toml_string_array(value);
            }
            "disabled_patterns" => {
                config.disabled_patterns = parse_toml_string_array(value);
            }
            _ => {}
        }
    }

    // Flush last custom pattern if any.
    if let (Some(n), Some(p)) = (current_name, current_pattern) {
        config.custom_patterns.insert(n, p);
    }

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

/// Parse a TOML array of strings like `["foo", "bar"]`.
fn parse_toml_string_array(s: &str) -> Vec<String> {
    let s = s.trim();
    if !s.starts_with('[') || !s.ends_with(']') {
        return Vec::new();
    }
    let inner = &s[1..s.len() - 1];
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|part| parse_toml_string(part.trim()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Check whether `rel_path` matches any of the allowed path globs.
fn is_path_allowed(rel_path: &str, allowed_paths: &[String]) -> bool {
    for pattern in allowed_paths {
        if let Ok(glob) = glob::Pattern::new(pattern) {
            if glob.matches(rel_path) {
                return true;
            }
        }
        // Simple prefix match as fallback.
        if rel_path.starts_with(pattern.trim_end_matches("**").trim_end_matches('/')) {
            return true;
        }
    }
    false
}

/// Scan files under `root` for secrets according to the provided config.
/// Only scans files that would be tracked (uses the provided file list).
pub fn scan_files_for_secrets(
    root: &Path,
    files: &[PathBuf],
    config: &SecretsConfig,
) -> Result<SecretScanReport> {
    let mut compiled_patterns: Vec<(String, Regex)> = Vec::new();

    // Add built-in patterns (unless disabled).
    for bp in BUILTIN_PATTERNS {
        if config.disabled_patterns.contains(&bp.name.to_string()) {
            continue;
        }
        let re = Regex::new(bp.pattern)
            .map_err(|e| anyhow::anyhow!("invalid built-in pattern '{}': {}", bp.name, e))?;
        compiled_patterns.push((bp.name.to_string(), re));
    }

    // Add custom patterns.
    for (name, pattern) in &config.custom_patterns {
        let re = Regex::new(pattern)
            .map_err(|e| anyhow::anyhow!("invalid custom pattern '{}': {}", name, e))?;
        compiled_patterns.push((name.clone(), re));
    }

    let mut findings = Vec::new();

    for file_path in files {
        let rel_path = file_path
            .strip_prefix(root)
            .unwrap_or(file_path)
            .to_string_lossy();

        // Skip allowed paths.
        if is_path_allowed(&rel_path, &config.allowed_paths) {
            continue;
        }

        // Skip binary files (heuristic: check first 8KB for null bytes).
        let contents = match fs::read(file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let check_len = contents.len().min(8192);
        if contents[..check_len].contains(&0) {
            continue;
        }

        let text = match std::str::from_utf8(&contents) {
            Ok(t) => t,
            Err(_) => continue,
        };

        for (line_num, line) in text.lines().enumerate() {
            for (pattern_name, re) in &compiled_patterns {
                if let Some(m) = re.find(line) {
                    let matched = m.as_str();
                    // Truncate display of matched text for safety.
                    let display = if matched.len() > 40 {
                        format!("{}...", &matched[..40])
                    } else {
                        matched.to_string()
                    };

                    findings.push(SecretFinding {
                        file: PathBuf::from(rel_path.as_ref()),
                        line: line_num + 1,
                        pattern_name: pattern_name.clone(),
                        matched_text: display,
                    });
                    // Only report first match per line to avoid noise.
                    break;
                }
            }
        }
    }

    Ok(SecretScanReport { findings })
}

/// Load secrets config from `.flock/secrets.toml`, falling back to defaults.
pub fn load_secrets_config(root: &Path) -> SecretsConfig {
    let config_path = root.join(".flock/secrets.toml");
    match fs::read_to_string(&config_path) {
        Ok(content) => parse_secrets_config(&content),
        Err(_) => SecretsConfig::default(),
    }
}

/// Format secret findings into a human-readable error message.
pub fn format_findings(findings: &[SecretFinding]) -> String {
    let mut msg = String::from("Secret detection found potential secrets in the following files:\n\n");
    for f in findings {
        msg.push_str(&format!(
            "  {}:{} — {} (matched: {})\n",
            f.file.display(),
            f.line,
            f.pattern_name,
            f.matched_text,
        ));
    }
    msg.push_str(
        "\nTo commit anyway, use: fl checkpoint --allow-secrets -m \"your message\"\n\
         To configure exceptions, edit .flock/secrets.toml",
    );
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_default_config() {
        let config = SecretsConfig::default();
        assert!(config.block);
        assert!(config.custom_patterns.is_empty());
        assert!(config.allowed_paths.is_empty());
        assert!(config.disabled_patterns.is_empty());
    }

    #[test]
    fn test_parse_secrets_config_basic() {
        let toml = r#"
block = false
allowed_paths = ["tests/fixtures/**"]
disabled_patterns = ["generic_password_assignment"]
"#;
        let config = parse_secrets_config(toml);
        assert!(!config.block);
        assert_eq!(config.allowed_paths, vec!["tests/fixtures/**"]);
        assert_eq!(config.disabled_patterns, vec!["generic_password_assignment"]);
    }

    #[test]
    fn test_parse_secrets_config_custom_patterns() {
        let toml = r#"
block = true
allowed_paths = []
disabled_patterns = []

[[custom_patterns]]
name = "my_key"
pattern = "MY_KEY_[A-Za-z0-9]{32}"

[[custom_patterns]]
name = "internal_token"
pattern = "INT_[0-9]{16}"
"#;
        let config = parse_secrets_config(toml);
        assert!(config.block);
        assert_eq!(config.custom_patterns.len(), 2);
        assert_eq!(
            config.custom_patterns.get("my_key").unwrap(),
            "MY_KEY_[A-Za-z0-9]{32}"
        );
        assert_eq!(
            config.custom_patterns.get("internal_token").unwrap(),
            "INT_[0-9]{16}"
        );
    }

    #[test]
    fn test_detect_aws_access_key() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("config.py");
        fs::write(&file_path, "AWS_KEY = 'AKIAIOSFODNN7EXAMPLE'\n").unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "aws_access_key");
    }

    #[test]
    fn test_detect_github_token() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("env.sh");
        fs::write(
            &file_path,
            &format!("export GITHUB_TOKEN={}_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn\n", "ghp"),
        )
        .unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "github_token");
    }

    #[test]
    fn test_detect_private_key() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("key.pem");
        fs::write(
            &file_path,
            "-----BEGIN RSA PRIVATE KEY-----\nMIIBogIBAAJ...\n-----END RSA PRIVATE KEY-----\n",
        )
        .unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "private_key_pem");
    }

    #[test]
    fn test_detect_generic_password() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("app.conf");
        fs::write(&file_path, "password = \"my_super_secret_password123\"\n").unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "generic_password_assignment");
    }

    #[test]
    fn test_detect_stripe_key() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("billing.js");
        fs::write(
            &file_path,
            &format!("const key = '{}_abcdefghijklmnopqrstuvwx';\n", "sk_live"),
        )
        .unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "stripe_key");
    }

    #[test]
    fn test_detect_database_url() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join(".env.local");
        fs::write(
            &file_path,
            "DATABASE_URL=postgres://user:s3cret@localhost:5432/mydb\n",
        )
        .unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "database_url_with_password");
    }

    #[test]
    fn test_detect_slack_token() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("notify.py");
        fs::write(
            &file_path,
            "SLACK = xoxb-1234567890-1234567890123-abcdef\n",
        )
        .unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "slack_token");
    }

    #[test]
    fn test_allowed_paths_skip_scanning() {
        let dir = TempDir::new().unwrap();
        let fixtures_dir = dir.path().join("tests/fixtures");
        fs::create_dir_all(&fixtures_dir).unwrap();
        let file_path = fixtures_dir.join("test_keys.py");
        fs::write(&file_path, "key = 'AKIAIOSFODNN7EXAMPLE'\n").unwrap();

        let config = SecretsConfig {
            allowed_paths: vec!["tests/fixtures/**".to_string()],
            ..Default::default()
        };
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(!report.has_secrets());
    }

    #[test]
    fn test_disabled_patterns() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("config.py");
        fs::write(&file_path, "AWS_KEY = 'AKIAIOSFODNN7EXAMPLE'\n").unwrap();

        let config = SecretsConfig {
            disabled_patterns: vec!["aws_access_key".to_string()],
            ..Default::default()
        };
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(!report.has_secrets());
    }

    #[test]
    fn test_custom_patterns() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("config.yaml");
        fs::write(&file_path, "key: CUSTOM_abcdef1234567890abcdef1234567890ab\n").unwrap();

        let mut custom = HashMap::new();
        custom.insert(
            "custom_key".to_string(),
            r"CUSTOM_[a-f0-9]{34}".to_string(),
        );
        let config = SecretsConfig {
            custom_patterns: custom,
            ..Default::default()
        };
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "custom_key");
    }

    #[test]
    fn test_no_secrets_clean_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("main.rs");
        fs::write(
            &file_path,
            "fn main() {\n    println!(\"Hello, world!\");\n}\n",
        )
        .unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(!report.has_secrets());
    }

    #[test]
    fn test_binary_files_skipped() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("image.png");
        let mut f = fs::File::create(&file_path).unwrap();
        // Write some bytes including null bytes to simulate binary.
        f.write_all(&[0x89, b'P', b'N', b'G', 0, 0, 0]).unwrap();
        f.write_all(b"AKIAIOSFODNN7EXAMPLE").unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(!report.has_secrets());
    }

    #[test]
    fn test_parse_toml_string_array() {
        assert_eq!(
            parse_toml_string_array(r#"["foo", "bar", "baz"]"#),
            vec!["foo", "bar", "baz"]
        );
        assert!(parse_toml_string_array("[]").is_empty());
    }

    #[test]
    fn test_format_findings() {
        let findings = vec![SecretFinding {
            file: PathBuf::from("src/config.py"),
            line: 5,
            pattern_name: "aws_access_key".to_string(),
            matched_text: "AKIAIOSFODNN7EXAMPLE".to_string(),
        }];
        let msg = format_findings(&findings);
        assert!(msg.contains("src/config.py:5"));
        assert!(msg.contains("aws_access_key"));
        assert!(msg.contains("--allow-secrets"));
    }

    #[test]
    fn test_detect_gcp_key() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("config.json");
        fs::write(
            &file_path,
            r#"{"api_key": "AIzaSyA1234567890abcdefghijklmnopqrstuv"}"#,
        )
        .unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "gcp_api_key");
    }

    #[test]
    fn test_detect_sendgrid_key() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("mailer.js");
        fs::write(
            &file_path,
            "const key = 'SG.abcdefghijklmnopqrstuv.wxyz1234567890abcdefghijkl';\n",
        )
        .unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "sendgrid_key");
    }

    #[test]
    fn test_detect_generic_token_assignment() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("auth.py");
        fs::write(
            &file_path,
            "access_token = \"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9\"\n",
        )
        .unwrap();

        let config = SecretsConfig::default();
        let report =
            scan_files_for_secrets(dir.path(), &[file_path], &config).unwrap();
        assert!(report.has_secrets());
        assert_eq!(report.findings[0].pattern_name, "generic_token_assignment");
    }
}
