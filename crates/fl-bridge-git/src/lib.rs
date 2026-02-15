use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn run_git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stdout.is_empty() {
        stderr.clone()
    } else if stderr.is_empty() {
        stdout.clone()
    } else {
        format!("{}\n{}", stdout, stderr)
    };

    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            if detail.is_empty() {
                "(no output)"
            } else {
                detail.as_str()
            }
        )
    }

    Ok(detail)
}
