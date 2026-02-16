use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Represents an overlapping symbol between local and remote changes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlappingSymbol {
    pub path: String,
    pub symbol: String,
    pub local_change: String,
    pub remote_change: String,
    pub remote_actor: String,
}

/// Represents a presence overlap - when a remote actor is working on the same symbol
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceOverlap {
    pub actor: String,
    pub symbol: String,
    pub path: String,
    pub intent: Option<String>,
}

/// Conflict forecaster that compares local semantic changes against remote announcements
/// to detect potential conflicts before they happen
pub struct ConflictForecaster {
    /// Map of (path, symbol) -> change_kind for local changes
    local_changes: HashMap<(String, String), String>,
}

impl ConflictForecaster {
    /// Create a new empty forecaster
    pub fn new() -> Self {
        Self {
            local_changes: HashMap::new(),
        }
    }

    /// Update local changes, replacing any existing entries
    /// Each tuple is (path, symbol, change_kind)
    pub fn update_local(&mut self, changes: Vec<(String, String, String)>) {
        self.local_changes.clear();
        for (path, symbol, change_kind) in changes {
            self.local_changes.insert((path, symbol), change_kind);
        }
    }

    /// Check for overlapping changes between local and remote
    /// Returns all symbols that are being modified both locally and remotely
    pub fn check_overlap(
        &self,
        remote_actor: &str,
        remote_paths: &[String],
        remote_symbols: &[String],
        remote_change_kinds: &[String],
    ) -> Vec<OverlappingSymbol> {
        let mut overlaps = Vec::new();

        for ((path, symbol), local_change) in &self.local_changes {
            // Check if this path and symbol appear in the remote changes
            let path_matches = remote_paths.iter().any(|p| p == path);
            let symbol_matches = remote_symbols.iter().any(|s| s == symbol);

            if path_matches && symbol_matches {
                // Found an overlap - use first remote change kind or "Unknown"
                let remote_change = remote_change_kinds
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "Unknown".to_string());

                overlaps.push(OverlappingSymbol {
                    path: path.clone(),
                    symbol: symbol.clone(),
                    local_change: local_change.clone(),
                    remote_change,
                    remote_actor: remote_actor.to_string(),
                });
            }
        }

        overlaps
    }

    /// Check for presence overlap - when a remote actor is working on files/symbols
    /// that we also have local changes for
    pub fn check_presence_overlap(
        &self,
        actor: &str,
        files: &[String],
        symbols: &[String],
        intent: Option<&str>,
    ) -> Vec<PresenceOverlap> {
        let mut overlaps = Vec::new();

        for ((path, symbol), _) in &self.local_changes {
            let path_matches = files.iter().any(|f| f == path);
            let symbol_matches = symbols.iter().any(|s| s == symbol);

            if path_matches && symbol_matches {
                overlaps.push(PresenceOverlap {
                    actor: actor.to_string(),
                    symbol: symbol.clone(),
                    path: path.clone(),
                    intent: intent.map(|i| i.to_string()),
                });
            }
        }

        overlaps
    }
}

impl Default for ConflictForecaster {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_forecaster() {
        let forecaster = ConflictForecaster::new();
        let overlaps = forecaster.check_overlap(
            "actor1",
            &["src/main.rs".to_string()],
            &["main".to_string()],
            &["Modified".to_string()],
        );
        assert!(overlaps.is_empty());
    }

    #[test]
    fn test_overlap_detected() {
        let mut forecaster = ConflictForecaster::new();
        forecaster.update_local(vec![
            ("src/main.rs".to_string(), "main".to_string(), "Modified".to_string()),
            ("src/lib.rs".to_string(), "parse".to_string(), "Added".to_string()),
        ]);

        let overlaps = forecaster.check_overlap(
            "alice",
            &["src/main.rs".to_string(), "src/other.rs".to_string()],
            &["main".to_string(), "helper".to_string()],
            &["Modified".to_string()],
        );

        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].path, "src/main.rs");
        assert_eq!(overlaps[0].symbol, "main");
        assert_eq!(overlaps[0].local_change, "Modified");
        assert_eq!(overlaps[0].remote_change, "Modified");
        assert_eq!(overlaps[0].remote_actor, "alice");
    }

    #[test]
    fn test_no_overlap() {
        let mut forecaster = ConflictForecaster::new();
        forecaster.update_local(vec![
            ("src/main.rs".to_string(), "main".to_string(), "Modified".to_string()),
        ]);

        // Different path
        let overlaps = forecaster.check_overlap(
            "bob",
            &["src/other.rs".to_string()],
            &["main".to_string()],
            &["Modified".to_string()],
        );
        assert!(overlaps.is_empty());

        // Different symbol
        let overlaps = forecaster.check_overlap(
            "bob",
            &["src/main.rs".to_string()],
            &["other_function".to_string()],
            &["Modified".to_string()],
        );
        assert!(overlaps.is_empty());
    }

    #[test]
    fn test_presence_overlap() {
        let mut forecaster = ConflictForecaster::new();
        forecaster.update_local(vec![
            ("src/main.rs".to_string(), "main".to_string(), "Modified".to_string()),
            ("src/lib.rs".to_string(), "parse".to_string(), "Added".to_string()),
        ]);

        let overlaps = forecaster.check_presence_overlap(
            "carol",
            &["src/main.rs".to_string()],
            &["main".to_string()],
            Some("refactoring"),
        );

        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].actor, "carol");
        assert_eq!(overlaps[0].symbol, "main");
        assert_eq!(overlaps[0].path, "src/main.rs");
        assert_eq!(overlaps[0].intent, Some("refactoring".to_string()));
    }
}
