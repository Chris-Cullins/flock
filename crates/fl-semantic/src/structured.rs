use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use crate::{
    SemanticAnalyzerPlugin, SemanticChange, SemanticChangeKind, SemanticCompatibility,
    SemanticConflictClassification, SemanticFileDiff, SemanticImpact, SemanticMergeConflict,
    SemanticMergeResult, SemanticRisk,
};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A named structural unit extracted from a file.
#[derive(Debug, Clone)]
struct StructuralUnit {
    /// Human-readable name/path of the unit (e.g. "server.port", "h2: Installation").
    name: String,
    /// The raw text content of the unit.
    body: String,
}

/// Diff two sets of structural units, producing semantic changes.
fn diff_units(old_units: &[StructuralUnit], new_units: &[StructuralUnit]) -> Vec<SemanticChange> {
    let old_map: BTreeMap<&str, &str> = old_units.iter().map(|u| (u.name.as_str(), u.body.as_str())).collect();
    let new_map: BTreeMap<&str, &str> = new_units.iter().map(|u| (u.name.as_str(), u.body.as_str())).collect();

    let mut changes = Vec::new();

    // Removed or modified
    for (name, old_body) in &old_map {
        match new_map.get(name) {
            None => changes.push(SemanticChange {
                kind: SemanticChangeKind::Removed,
                symbol: name.to_string(),
                risk: SemanticRisk::Medium,
                impact: SemanticImpact::default(),
                compatibility: SemanticCompatibility::default(),
                old_source: Some(old_body.to_string()),
                new_source: None,
            }),
            Some(new_body) if new_body != old_body => changes.push(SemanticChange {
                kind: SemanticChangeKind::Modified,
                symbol: name.to_string(),
                risk: SemanticRisk::Low,
                impact: SemanticImpact::default(),
                compatibility: SemanticCompatibility::default(),
                old_source: Some(old_body.to_string()),
                new_source: Some(new_body.to_string()),
            }),
            _ => {}
        }
    }

    // Added
    for name in new_map.keys() {
        if !old_map.contains_key(name) {
            changes.push(SemanticChange {
                kind: SemanticChangeKind::Added,
                symbol: name.to_string(),
                risk: SemanticRisk::Low,
                impact: SemanticImpact::default(),
                compatibility: SemanticCompatibility::default(),
                old_source: None,
                new_source: new_map.get(name).map(|s| s.to_string()),
            });
        }
    }

    changes
}

/// Three-way merge of structural units.  Returns merged text and conflicts.
fn merge_units(
    base_units: &[StructuralUnit],
    left_units: &[StructuralUnit],
    right_units: &[StructuralUnit],
    separator: &str,
) -> (String, Vec<SemanticMergeConflict>) {
    let base_map: BTreeMap<String, String> = base_units.iter().map(|u| (u.name.clone(), u.body.clone())).collect();
    let left_map: BTreeMap<String, String> = left_units.iter().map(|u| (u.name.clone(), u.body.clone())).collect();
    let right_map: BTreeMap<String, String> = right_units.iter().map(|u| (u.name.clone(), u.body.clone())).collect();

    let mut all_keys: Vec<String> = Vec::new();
    // Preserve order: base keys first, then left-only additions, then right-only additions.
    for u in base_units {
        if !all_keys.contains(&u.name) {
            all_keys.push(u.name.clone());
        }
    }
    for u in left_units {
        if !all_keys.contains(&u.name) {
            all_keys.push(u.name.clone());
        }
    }
    for u in right_units {
        if !all_keys.contains(&u.name) {
            all_keys.push(u.name.clone());
        }
    }

    let mut merged_parts = Vec::new();
    let mut conflicts = Vec::new();

    for key in &all_keys {
        let base_val = base_map.get(key);
        let left_val = left_map.get(key);
        let right_val = right_map.get(key);

        match (base_val, left_val, right_val) {
            // Both sides deleted — omit.
            (Some(_), None, None) => {}
            // Only on one side (added or unchanged).
            (None, Some(v), None) | (None, None, Some(v)) => {
                merged_parts.push(v.clone());
            }
            // Both added the same thing.
            (None, Some(l), Some(r)) if l == r => {
                merged_parts.push(l.clone());
            }
            // Both added differently — conflict.
            (None, Some(l), Some(r)) => {
                merged_parts.push(l.clone());
                conflicts.push(SemanticMergeConflict {
                    symbol: key.clone(),
                    classification: SemanticConflictClassification::ConcurrentAddition,
                    explanation: format!("key `{}` added differently on both sides", key),
                    affected_files: Vec::new(),
                    impact: None,
                });
                let _ = r; // right side loses, reported as conflict
            }
            // One side deleted, other modified — conflict.
            (Some(_), None, Some(r)) => {
                merged_parts.push(r.clone());
                conflicts.push(SemanticMergeConflict {
                    symbol: key.clone(),
                    classification: SemanticConflictClassification::DeleteVsEdit,
                    explanation: format!("key `{}` deleted on left, modified on right", key),
                    affected_files: Vec::new(),
                    impact: None,
                });
            }
            (Some(_), Some(l), None) => {
                merged_parts.push(l.clone());
                conflicts.push(SemanticMergeConflict {
                    symbol: key.clone(),
                    classification: SemanticConflictClassification::DeleteVsEdit,
                    explanation: format!("key `{}` modified on left, deleted on right", key),
                    affected_files: Vec::new(),
                    impact: None,
                });
            }
            // Both unchanged.
            (Some(_), Some(l), Some(r)) if l == r => {
                merged_parts.push(l.clone());
            }
            // One side unchanged.
            (Some(b), Some(l), Some(r)) if l == b => {
                merged_parts.push(r.clone());
            }
            (Some(b), Some(l), Some(r)) if r == b => {
                merged_parts.push(l.clone());
            }
            // Both modified differently — conflict, pick left.
            (Some(_), Some(l), Some(_)) => {
                merged_parts.push(l.clone());
                conflicts.push(SemanticMergeConflict {
                    symbol: key.clone(),
                    classification: SemanticConflictClassification::DivergentEdit,
                    explanation: format!("key `{}` modified differently on both sides", key),
                    affected_files: Vec::new(),
                    impact: None,
                });
            }
            // Key exists nowhere — shouldn't happen.
            (None, None, None) => {}
        }
    }

    (merged_parts.join(separator), conflicts)
}

// ===========================================================================
// JSON / JSONL Analyzer
// ===========================================================================

pub struct JsonAnalyzer;

impl JsonAnalyzer {
    fn parse_json_units(source: &[u8]) -> Vec<StructuralUnit> {
        let text = String::from_utf8_lossy(source);
        // Try as JSON object first.
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(text.as_ref()) {
            return map
                .into_iter()
                .map(|(k, v)| StructuralUnit {
                    name: k.clone(),
                    body: serde_json::to_string_pretty(&v).unwrap_or_default(),
                })
                .collect();
        }
        // Try as JSON array.
        if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str(text.as_ref()) {
            return arr
                .into_iter()
                .enumerate()
                .map(|(i, v)| StructuralUnit {
                    name: format!("[{}]", i),
                    body: serde_json::to_string_pretty(&v).unwrap_or_default(),
                })
                .collect();
        }
        // JSONL: each line is an entry.
        text.lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(i, line)| StructuralUnit {
                name: format!("line:{}", i),
                body: line.to_string(),
            })
            .collect()
    }
}

impl SemanticAnalyzerPlugin for JsonAnalyzer {
    fn id(&self) -> &str {
        "structured-json"
    }

    fn supports_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("json" | "jsonl" | "ndjson")
        )
    }

    fn diff(
        &self,
        path: &Path,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> Result<Option<SemanticFileDiff>> {
        let old_source = old_source.unwrap_or_default();
        let new_source = new_source.unwrap_or_default();
        if old_source == new_source {
            return Ok(None);
        }

        let old_units = Self::parse_json_units(old_source);
        let new_units = Self::parse_json_units(new_source);
        let changes = diff_units(&old_units, &new_units);
        if changes.is_empty() {
            return Ok(None);
        }

        Ok(Some(SemanticFileDiff {
            path: path.to_string_lossy().to_string(),
            language: "JSON".to_string(),
            parse_fallback: false,
            changes,
        }))
    }

    fn merge(
        &self,
        path: &Path,
        base_source: Option<&[u8]>,
        left_source: Option<&[u8]>,
        right_source: Option<&[u8]>,
    ) -> Result<Option<SemanticMergeResult>> {
        let base_units = Self::parse_json_units(base_source.unwrap_or_default());
        let left_units = Self::parse_json_units(left_source.unwrap_or_default());
        let right_units = Self::parse_json_units(right_source.unwrap_or_default());
        let (merged, conflicts) = merge_units(&base_units, &left_units, &right_units, "\n");

        Ok(Some(SemanticMergeResult {
            path: path.to_string_lossy().to_string(),
            language: "JSON".to_string(),
            parse_fallback: false,
            merged_source: merged,
            conflicts,
        }))
    }
}

// ===========================================================================
// YAML / TOML Analyzer
// ===========================================================================

pub struct YamlTomlAnalyzer;

impl YamlTomlAnalyzer {
    /// Extract top-level key blocks from YAML by scanning for lines that start
    /// at column 0 with a key.  Each block runs until the next top-level key.
    fn parse_yaml_units(source: &[u8]) -> Vec<StructuralUnit> {
        let text = String::from_utf8_lossy(source);
        let mut units = Vec::new();
        let mut current_name: Option<String> = None;
        let mut current_lines: Vec<&str> = Vec::new();

        for line in text.lines() {
            // Top-level key: starts with a non-space, non-comment, non-dash character
            // and contains a colon.
            if !line.is_empty()
                && !line.starts_with(' ')
                && !line.starts_with('\t')
                && !line.starts_with('#')
                && !line.starts_with("---")
                && !line.starts_with("...")
            {
                if let Some(colon_pos) = line.find(':') {
                    // Flush previous block.
                    if let Some(name) = current_name.take() {
                        units.push(StructuralUnit {
                            name,
                            body: current_lines.join("\n"),
                        });
                        current_lines.clear();
                    }
                    current_name = Some(line[..colon_pos].trim().to_string());
                }
            }
            current_lines.push(line);
        }
        // Flush last block.
        if let Some(name) = current_name {
            units.push(StructuralUnit {
                name,
                body: current_lines.join("\n"),
            });
        } else if !current_lines.is_empty() {
            // No top-level keys found — treat entire file as one unit.
            units.push(StructuralUnit {
                name: "(document)".to_string(),
                body: current_lines.join("\n"),
            });
        }

        units
    }

    /// Extract TOML sections: `[section]` and `[[array]]` headers define blocks.
    /// Top-level keys before any header go under `(root)`.
    fn parse_toml_units(source: &[u8]) -> Vec<StructuralUnit> {
        let text = String::from_utf8_lossy(source);
        let mut units = Vec::new();
        let mut current_name = "(root)".to_string();
        let mut current_lines: Vec<&str> = Vec::new();

        for line in text.lines() {
            let trimmed = line.trim();
            if (trimmed.starts_with('[') && !trimmed.starts_with("[["))
                || trimmed.starts_with("[[")
            {
                // Flush previous section.
                let body = current_lines.join("\n");
                if !body.trim().is_empty() {
                    units.push(StructuralUnit {
                        name: current_name,
                        body,
                    });
                }
                current_name = trimmed
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .trim()
                    .to_string();
                current_lines = vec![line];
            } else {
                current_lines.push(line);
            }
        }
        let body = current_lines.join("\n");
        if !body.trim().is_empty() {
            units.push(StructuralUnit {
                name: current_name,
                body,
            });
        }

        units
    }

    fn parse_units(path: &Path, source: &[u8]) -> Vec<StructuralUnit> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("toml") => Self::parse_toml_units(source),
            _ => Self::parse_yaml_units(source),
        }
    }

    fn language(path: &Path) -> &'static str {
        match path.extension().and_then(|e| e.to_str()) {
            Some("toml") => "TOML",
            _ => "YAML",
        }
    }
}

impl SemanticAnalyzerPlugin for YamlTomlAnalyzer {
    fn id(&self) -> &str {
        "structured-yaml-toml"
    }

    fn supports_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yaml" | "yml" | "toml")
        )
    }

    fn diff(
        &self,
        path: &Path,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> Result<Option<SemanticFileDiff>> {
        let old_source = old_source.unwrap_or_default();
        let new_source = new_source.unwrap_or_default();
        if old_source == new_source {
            return Ok(None);
        }

        let old_units = Self::parse_units(path, old_source);
        let new_units = Self::parse_units(path, new_source);
        let changes = diff_units(&old_units, &new_units);
        if changes.is_empty() {
            return Ok(None);
        }

        Ok(Some(SemanticFileDiff {
            path: path.to_string_lossy().to_string(),
            language: Self::language(path).to_string(),
            parse_fallback: false,
            changes,
        }))
    }

    fn merge(
        &self,
        path: &Path,
        base_source: Option<&[u8]>,
        left_source: Option<&[u8]>,
        right_source: Option<&[u8]>,
    ) -> Result<Option<SemanticMergeResult>> {
        let base_units = Self::parse_units(path, base_source.unwrap_or_default());
        let left_units = Self::parse_units(path, left_source.unwrap_or_default());
        let right_units = Self::parse_units(path, right_source.unwrap_or_default());
        let (merged, conflicts) = merge_units(&base_units, &left_units, &right_units, "\n");

        Ok(Some(SemanticMergeResult {
            path: path.to_string_lossy().to_string(),
            language: Self::language(path).to_string(),
            parse_fallback: false,
            merged_source: merged,
            conflicts,
        }))
    }
}

// ===========================================================================
// XML / HTML Analyzer
// ===========================================================================

pub struct XmlHtmlAnalyzer;

impl XmlHtmlAnalyzer {
    /// Simple top-level element extraction.  Finds elements at the root level
    /// and uses their tag name (plus an index for duplicates) as the unit name.
    fn parse_units(source: &[u8]) -> Vec<StructuralUnit> {
        let text = String::from_utf8_lossy(source);
        let mut units = Vec::new();
        let mut tag_counts: BTreeMap<String, usize> = BTreeMap::new();

        // Simple state machine: track top-level tags by depth.
        let mut depth: i32 = 0;
        let mut current_tag: Option<String> = None;
        let mut current_lines: Vec<&str> = Vec::new();
        let mut preamble_lines: Vec<&str> = Vec::new();

        for line in text.lines() {
            let trimmed = line.trim();

            // Preamble (before any element).
            if depth == 0 && current_tag.is_none() {
                if trimmed.starts_with("<?")
                    || trimmed.starts_with("<!")
                    || trimmed.is_empty()
                {
                    preamble_lines.push(line);
                    continue;
                }
            }

            // Count opens and closes to track depth.
            let opens = trimmed.matches('<').count() as i32
                - trimmed.matches("</").count() as i32
                - trimmed.matches("/>").count() as i32
                - trimmed.matches("<?").count() as i32
                - trimmed.matches("<!").count() as i32;
            let closes = trimmed.matches("</").count() as i32
                + trimmed.matches("/>").count() as i32;

            if depth == 0 && current_tag.is_none() {
                // Starting a new top-level element.
                if let Some(tag) = extract_tag_name(trimmed) {
                    current_tag = Some(tag);
                    current_lines.push(line);
                    depth += opens - closes;
                    if depth <= 0 {
                        // Self-closing or single-line element.
                        let tag = current_tag.take().unwrap();
                        let count = tag_counts.entry(tag.clone()).or_insert(0);
                        *count += 1;
                        let name = if *count > 1 {
                            format!("{} #{}", tag, count)
                        } else {
                            tag
                        };
                        units.push(StructuralUnit {
                            name,
                            body: current_lines.join("\n"),
                        });
                        current_lines.clear();
                        depth = 0;
                    }
                } else {
                    preamble_lines.push(line);
                }
            } else {
                current_lines.push(line);
                depth += opens - closes;
                if depth <= 0 {
                    let tag = current_tag.take().unwrap();
                    let count = tag_counts.entry(tag.clone()).or_insert(0);
                    *count += 1;
                    let name = if *count > 1 {
                        format!("{} #{}", tag, count)
                    } else {
                        tag
                    };
                    units.push(StructuralUnit {
                        name,
                        body: current_lines.join("\n"),
                    });
                    current_lines.clear();
                    depth = 0;
                }
            }
        }

        // Flush remaining content.
        if !current_lines.is_empty() {
            let name = current_tag.unwrap_or_else(|| "(fragment)".to_string());
            units.push(StructuralUnit {
                name,
                body: current_lines.join("\n"),
            });
        }

        if !preamble_lines.is_empty() {
            let body = preamble_lines.join("\n");
            if !body.trim().is_empty() {
                units.insert(
                    0,
                    StructuralUnit {
                        name: "(preamble)".to_string(),
                        body,
                    },
                );
            }
        }

        units
    }

    fn language(path: &Path) -> &'static str {
        match path.extension().and_then(|e| e.to_str()) {
            Some("html" | "htm") => "HTML",
            Some("svg") => "SVG",
            _ => "XML",
        }
    }
}

fn extract_tag_name(line: &str) -> Option<String> {
    let line = line.trim();
    if !line.starts_with('<') || line.starts_with("</") || line.starts_with("<!") || line.starts_with("<?") {
        return None;
    }
    let rest = &line[1..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(rest.len());
    let tag = &rest[..end];
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

impl SemanticAnalyzerPlugin for XmlHtmlAnalyzer {
    fn id(&self) -> &str {
        "structured-xml-html"
    }

    fn supports_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("xml" | "html" | "htm" | "svg" | "xhtml")
        )
    }

    fn diff(
        &self,
        path: &Path,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> Result<Option<SemanticFileDiff>> {
        let old_source = old_source.unwrap_or_default();
        let new_source = new_source.unwrap_or_default();
        if old_source == new_source {
            return Ok(None);
        }

        let old_units = Self::parse_units(old_source);
        let new_units = Self::parse_units(new_source);
        let changes = diff_units(&old_units, &new_units);
        if changes.is_empty() {
            return Ok(None);
        }

        Ok(Some(SemanticFileDiff {
            path: path.to_string_lossy().to_string(),
            language: Self::language(path).to_string(),
            parse_fallback: false,
            changes,
        }))
    }

    fn merge(
        &self,
        path: &Path,
        base_source: Option<&[u8]>,
        left_source: Option<&[u8]>,
        right_source: Option<&[u8]>,
    ) -> Result<Option<SemanticMergeResult>> {
        let base_units = Self::parse_units(base_source.unwrap_or_default());
        let left_units = Self::parse_units(left_source.unwrap_or_default());
        let right_units = Self::parse_units(right_source.unwrap_or_default());
        let (merged, conflicts) = merge_units(&base_units, &left_units, &right_units, "\n");

        Ok(Some(SemanticMergeResult {
            path: path.to_string_lossy().to_string(),
            language: Self::language(path).to_string(),
            parse_fallback: false,
            merged_source: merged,
            conflicts,
        }))
    }
}

// ===========================================================================
// CSS / SCSS Analyzer
// ===========================================================================

pub struct CssAnalyzer;

impl CssAnalyzer {
    /// Parse CSS/SCSS into rule blocks.  Each top-level selector (or @-rule)
    /// with its body becomes one structural unit.
    fn parse_units(source: &[u8]) -> Vec<StructuralUnit> {
        let text = String::from_utf8_lossy(source);
        let mut units = Vec::new();
        let mut depth: i32 = 0;
        let mut current_selector: Option<String> = None;
        let mut current_lines: Vec<&str> = Vec::new();
        let mut selector_counts: BTreeMap<String, usize> = BTreeMap::new();

        for line in text.lines() {
            let trimmed = line.trim();

            // Skip empty lines / comments outside of rules.
            if depth == 0 && current_selector.is_none() {
                if trimmed.is_empty() || trimmed.starts_with("//") {
                    continue;
                }
                // Block comment that spans a single line.
                if trimmed.starts_with("/*") && trimmed.ends_with("*/") {
                    continue;
                }
            }

            let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
            let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;

            if depth == 0 && current_selector.is_none() && opens > 0 {
                // Start of a new rule.
                let selector = trimmed
                    .split('{')
                    .next()
                    .unwrap_or(trimmed)
                    .trim()
                    .to_string();
                current_selector = Some(selector);
                current_lines.push(line);
                depth += opens - closes;
                if depth <= 0 {
                    // Single-line rule.
                    let sel = current_selector.take().unwrap();
                    let count = selector_counts.entry(sel.clone()).or_insert(0);
                    *count += 1;
                    let name = if *count > 1 {
                        format!("{} #{}", sel, count)
                    } else {
                        sel
                    };
                    units.push(StructuralUnit {
                        name,
                        body: current_lines.join("\n"),
                    });
                    current_lines.clear();
                    depth = 0;
                }
            } else if current_selector.is_some() {
                current_lines.push(line);
                depth += opens - closes;
                if depth <= 0 {
                    let sel = current_selector.take().unwrap();
                    let count = selector_counts.entry(sel.clone()).or_insert(0);
                    *count += 1;
                    let name = if *count > 1 {
                        format!("{} #{}", sel, count)
                    } else {
                        sel
                    };
                    units.push(StructuralUnit {
                        name,
                        body: current_lines.join("\n"),
                    });
                    current_lines.clear();
                    depth = 0;
                }
            } else {
                // Top-level declaration without braces (e.g. @import, @charset).
                if !trimmed.is_empty() {
                    units.push(StructuralUnit {
                        name: trimmed.to_string(),
                        body: line.to_string(),
                    });
                }
            }
        }

        // Flush remaining content.
        if !current_lines.is_empty() {
            let name = current_selector.unwrap_or_else(|| "(fragment)".to_string());
            units.push(StructuralUnit {
                name,
                body: current_lines.join("\n"),
            });
        }

        units
    }

    fn language(path: &Path) -> &'static str {
        match path.extension().and_then(|e| e.to_str()) {
            Some("scss") => "SCSS",
            Some("less") => "Less",
            _ => "CSS",
        }
    }
}

impl SemanticAnalyzerPlugin for CssAnalyzer {
    fn id(&self) -> &str {
        "structured-css"
    }

    fn supports_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("css" | "scss" | "less")
        )
    }

    fn diff(
        &self,
        path: &Path,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> Result<Option<SemanticFileDiff>> {
        let old_source = old_source.unwrap_or_default();
        let new_source = new_source.unwrap_or_default();
        if old_source == new_source {
            return Ok(None);
        }

        let old_units = Self::parse_units(old_source);
        let new_units = Self::parse_units(new_source);
        let changes = diff_units(&old_units, &new_units);
        if changes.is_empty() {
            return Ok(None);
        }

        Ok(Some(SemanticFileDiff {
            path: path.to_string_lossy().to_string(),
            language: Self::language(path).to_string(),
            parse_fallback: false,
            changes,
        }))
    }

    fn merge(
        &self,
        path: &Path,
        base_source: Option<&[u8]>,
        left_source: Option<&[u8]>,
        right_source: Option<&[u8]>,
    ) -> Result<Option<SemanticMergeResult>> {
        let base_units = Self::parse_units(base_source.unwrap_or_default());
        let left_units = Self::parse_units(left_source.unwrap_or_default());
        let right_units = Self::parse_units(right_source.unwrap_or_default());
        let (merged, conflicts) = merge_units(&base_units, &left_units, &right_units, "\n\n");

        Ok(Some(SemanticMergeResult {
            path: path.to_string_lossy().to_string(),
            language: Self::language(path).to_string(),
            parse_fallback: false,
            merged_source: merged,
            conflicts,
        }))
    }
}

// ===========================================================================
// Markdown Analyzer
// ===========================================================================

pub struct MarkdownAnalyzer;

impl MarkdownAnalyzer {
    /// Parse Markdown into sections defined by headings.
    /// Each heading and its content until the next heading of equal or higher
    /// level forms one structural unit.
    fn parse_units(source: &[u8]) -> Vec<StructuralUnit> {
        let text = String::from_utf8_lossy(source);
        let mut units = Vec::new();
        let mut current_heading: Option<String> = None;
        let mut current_level: usize = 0;
        let mut current_lines: Vec<&str> = Vec::new();

        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(level) = heading_level(trimmed) {
                // Flush previous section.
                if current_heading.is_some() || !current_lines.is_empty() {
                    let name = current_heading
                        .take()
                        .unwrap_or_else(|| "(preamble)".to_string());
                    let body = current_lines.join("\n");
                    if !body.trim().is_empty() || name != "(preamble)" {
                        units.push(StructuralUnit { name, body });
                    }
                    current_lines.clear();
                }

                let heading_text = trimmed.trim_start_matches('#').trim();
                current_heading = Some(format!(
                    "h{}: {}",
                    level, heading_text
                ));
                current_level = level;
                current_lines.push(line);
            } else {
                current_lines.push(line);
            }
        }

        // Flush last section.
        let name = current_heading
            .take()
            .unwrap_or_else(|| "(preamble)".to_string());
        let body = current_lines.join("\n");
        if !body.trim().is_empty() || units.is_empty() {
            units.push(StructuralUnit { name, body });
        }

        let _ = current_level;
        units
    }
}

fn heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    if !trimmed.starts_with('#') {
        return None;
    }
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    // Must be followed by a space (or be just hashes at end of line).
    if hashes > 0 && hashes <= 6 {
        let rest = &trimmed[hashes..];
        if rest.is_empty() || rest.starts_with(' ') {
            return Some(hashes);
        }
    }
    None
}

impl SemanticAnalyzerPlugin for MarkdownAnalyzer {
    fn id(&self) -> &str {
        "structured-markdown"
    }

    fn supports_path(&self, path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("md" | "mdx" | "markdown")
        )
    }

    fn diff(
        &self,
        path: &Path,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> Result<Option<SemanticFileDiff>> {
        let old_source = old_source.unwrap_or_default();
        let new_source = new_source.unwrap_or_default();
        if old_source == new_source {
            return Ok(None);
        }

        let old_units = Self::parse_units(old_source);
        let new_units = Self::parse_units(new_source);
        let changes = diff_units(&old_units, &new_units);
        if changes.is_empty() {
            return Ok(None);
        }

        Ok(Some(SemanticFileDiff {
            path: path.to_string_lossy().to_string(),
            language: "Markdown".to_string(),
            parse_fallback: false,
            changes,
        }))
    }

    fn merge(
        &self,
        path: &Path,
        base_source: Option<&[u8]>,
        left_source: Option<&[u8]>,
        right_source: Option<&[u8]>,
    ) -> Result<Option<SemanticMergeResult>> {
        let base_units = Self::parse_units(base_source.unwrap_or_default());
        let left_units = Self::parse_units(left_source.unwrap_or_default());
        let right_units = Self::parse_units(right_source.unwrap_or_default());
        let (merged, conflicts) = merge_units(&base_units, &left_units, &right_units, "\n\n");

        Ok(Some(SemanticMergeResult {
            path: path.to_string_lossy().to_string(),
            language: "Markdown".to_string(),
            parse_fallback: false,
            merged_source: merged,
            conflicts,
        }))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- JSON tests --

    #[test]
    fn json_diff_added_key() {
        let analyzer = JsonAnalyzer;
        let old = br#"{"name": "flock"}"#;
        let new = br#"{"name": "flock", "version": "1.0"}"#;
        let diff = analyzer
            .diff(Path::new("package.json"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert_eq!(diff.language, "JSON");
        assert!(!diff.parse_fallback);
        assert!(diff.changes.iter().any(|c| c.symbol == "version" && c.kind == SemanticChangeKind::Added));
    }

    #[test]
    fn json_diff_removed_key() {
        let analyzer = JsonAnalyzer;
        let old = br#"{"name": "flock", "version": "1.0"}"#;
        let new = br#"{"name": "flock"}"#;
        let diff = analyzer
            .diff(Path::new("config.json"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert!(diff.changes.iter().any(|c| c.symbol == "version" && c.kind == SemanticChangeKind::Removed));
    }

    #[test]
    fn json_diff_modified_key() {
        let analyzer = JsonAnalyzer;
        let old = br#"{"name": "flock", "version": "1.0"}"#;
        let new = br#"{"name": "flock", "version": "2.0"}"#;
        let diff = analyzer
            .diff(Path::new("config.json"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert!(diff.changes.iter().any(|c| c.symbol == "version" && c.kind == SemanticChangeKind::Modified));
    }

    #[test]
    fn json_diff_no_change() {
        let analyzer = JsonAnalyzer;
        let src = br#"{"name": "flock"}"#;
        assert!(analyzer.diff(Path::new("x.json"), Some(src), Some(src)).unwrap().is_none());
    }

    #[test]
    fn json_merge_concurrent_add() {
        let analyzer = JsonAnalyzer;
        let base = br#"{"name": "flock"}"#;
        let left = br#"{"name": "flock", "a": 1}"#;
        let right = br#"{"name": "flock", "b": 2}"#;
        let result = analyzer
            .merge(Path::new("x.json"), Some(base), Some(left), Some(right))
            .unwrap()
            .unwrap();
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn json_merge_divergent_edit() {
        let analyzer = JsonAnalyzer;
        let base = br#"{"name": "flock"}"#;
        let left = br#"{"name": "foo"}"#;
        let right = br#"{"name": "bar"}"#;
        let result = analyzer
            .merge(Path::new("x.json"), Some(base), Some(left), Some(right))
            .unwrap()
            .unwrap();
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].classification, SemanticConflictClassification::DivergentEdit);
    }

    #[test]
    fn json_supports_extensions() {
        let analyzer = JsonAnalyzer;
        assert!(analyzer.supports_path(Path::new("x.json")));
        assert!(analyzer.supports_path(Path::new("x.jsonl")));
        assert!(analyzer.supports_path(Path::new("x.ndjson")));
        assert!(!analyzer.supports_path(Path::new("x.yaml")));
    }

    // -- YAML / TOML tests --

    #[test]
    fn yaml_diff_modified_key() {
        let analyzer = YamlTomlAnalyzer;
        let old = b"name: flock\nversion: 1.0\n";
        let new = b"name: flock\nversion: 2.0\n";
        let diff = analyzer
            .diff(Path::new("config.yaml"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert_eq!(diff.language, "YAML");
        assert!(diff.changes.iter().any(|c| c.symbol == "version" && c.kind == SemanticChangeKind::Modified));
    }

    #[test]
    fn yaml_diff_added_key() {
        let analyzer = YamlTomlAnalyzer;
        let old = b"name: flock\n";
        let new = b"name: flock\nversion: 1.0\n";
        let diff = analyzer
            .diff(Path::new("config.yml"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert!(diff.changes.iter().any(|c| c.symbol == "version" && c.kind == SemanticChangeKind::Added));
    }

    #[test]
    fn toml_diff_sections() {
        let analyzer = YamlTomlAnalyzer;
        let old = b"[package]\nname = \"flock\"\n\n[dependencies]\nserde = \"1\"\n";
        let new = b"[package]\nname = \"flock\"\nversion = \"0.1\"\n\n[dependencies]\nserde = \"1\"\n";
        let diff = analyzer
            .diff(Path::new("Cargo.toml"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert_eq!(diff.language, "TOML");
        assert!(diff.changes.iter().any(|c| c.symbol == "package" && c.kind == SemanticChangeKind::Modified));
    }

    #[test]
    fn toml_diff_added_section() {
        let analyzer = YamlTomlAnalyzer;
        let old = b"[package]\nname = \"flock\"\n";
        let new = b"[package]\nname = \"flock\"\n\n[dependencies]\nserde = \"1\"\n";
        let diff = analyzer
            .diff(Path::new("Cargo.toml"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert!(diff.changes.iter().any(|c| c.symbol == "dependencies" && c.kind == SemanticChangeKind::Added));
    }

    #[test]
    fn yaml_toml_supports_extensions() {
        let analyzer = YamlTomlAnalyzer;
        assert!(analyzer.supports_path(Path::new("x.yaml")));
        assert!(analyzer.supports_path(Path::new("x.yml")));
        assert!(analyzer.supports_path(Path::new("x.toml")));
        assert!(!analyzer.supports_path(Path::new("x.json")));
    }

    // -- XML / HTML tests --

    #[test]
    fn xml_diff_modified_element() {
        let analyzer = XmlHtmlAnalyzer;
        let old = b"<root>\n  <name>flock</name>\n</root>\n";
        let new = b"<root>\n  <name>flock v2</name>\n</root>\n";
        let diff = analyzer
            .diff(Path::new("config.xml"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert_eq!(diff.language, "XML");
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Modified));
    }

    #[test]
    fn html_diff_added_element() {
        let analyzer = XmlHtmlAnalyzer;
        let old = b"<div>hello</div>\n";
        let new = b"<div>hello</div>\n<footer>bye</footer>\n";
        let diff = analyzer
            .diff(Path::new("index.html"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert_eq!(diff.language, "HTML");
        assert!(diff.changes.iter().any(|c| c.symbol == "footer" && c.kind == SemanticChangeKind::Added));
    }

    #[test]
    fn xml_html_supports_extensions() {
        let analyzer = XmlHtmlAnalyzer;
        assert!(analyzer.supports_path(Path::new("x.xml")));
        assert!(analyzer.supports_path(Path::new("x.html")));
        assert!(analyzer.supports_path(Path::new("x.htm")));
        assert!(analyzer.supports_path(Path::new("x.svg")));
        assert!(analyzer.supports_path(Path::new("x.xhtml")));
        assert!(!analyzer.supports_path(Path::new("x.json")));
    }

    // -- CSS tests --

    #[test]
    fn css_diff_modified_rule() {
        let analyzer = CssAnalyzer;
        let old = b"body {\n  color: red;\n}\n";
        let new = b"body {\n  color: blue;\n}\n";
        let diff = analyzer
            .diff(Path::new("style.css"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert_eq!(diff.language, "CSS");
        assert!(diff.changes.iter().any(|c| c.symbol == "body" && c.kind == SemanticChangeKind::Modified));
    }

    #[test]
    fn css_diff_added_rule() {
        let analyzer = CssAnalyzer;
        let old = b"body {\n  color: red;\n}\n";
        let new = b"body {\n  color: red;\n}\n\n.header {\n  font-size: 16px;\n}\n";
        let diff = analyzer
            .diff(Path::new("style.css"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert!(diff.changes.iter().any(|c| c.symbol == ".header" && c.kind == SemanticChangeKind::Added));
    }

    #[test]
    fn css_diff_removed_rule() {
        let analyzer = CssAnalyzer;
        let old = b"body {\n  color: red;\n}\n\n.header {\n  font-size: 16px;\n}\n";
        let new = b"body {\n  color: red;\n}\n";
        let diff = analyzer
            .diff(Path::new("style.scss"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert_eq!(diff.language, "SCSS");
        assert!(diff.changes.iter().any(|c| c.symbol == ".header" && c.kind == SemanticChangeKind::Removed));
    }

    #[test]
    fn css_supports_extensions() {
        let analyzer = CssAnalyzer;
        assert!(analyzer.supports_path(Path::new("x.css")));
        assert!(analyzer.supports_path(Path::new("x.scss")));
        assert!(analyzer.supports_path(Path::new("x.less")));
        assert!(!analyzer.supports_path(Path::new("x.js")));
    }

    // -- Markdown tests --

    #[test]
    fn markdown_diff_modified_section() {
        let analyzer = MarkdownAnalyzer;
        let old = b"# Title\n\nHello world.\n\n## Section A\n\nContent A.\n";
        let new = b"# Title\n\nHello world.\n\n## Section A\n\nUpdated content.\n";
        let diff = analyzer
            .diff(Path::new("README.md"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert_eq!(diff.language, "Markdown");
        assert!(diff.changes.iter().any(|c| c.symbol == "h2: Section A" && c.kind == SemanticChangeKind::Modified));
    }

    #[test]
    fn markdown_diff_added_section() {
        let analyzer = MarkdownAnalyzer;
        let old = b"# Title\n\nHello.\n";
        let new = b"# Title\n\nHello.\n\n## New Section\n\nNew content.\n";
        let diff = analyzer
            .diff(Path::new("README.md"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert!(diff.changes.iter().any(|c| c.symbol == "h2: New Section" && c.kind == SemanticChangeKind::Added));
    }

    #[test]
    fn markdown_diff_removed_section() {
        let analyzer = MarkdownAnalyzer;
        let old = b"# Title\n\nHello.\n\n## Old Section\n\nOld content.\n";
        let new = b"# Title\n\nHello.\n";
        let diff = analyzer
            .diff(Path::new("guide.md"), Some(old), Some(new))
            .unwrap()
            .unwrap();
        assert!(diff.changes.iter().any(|c| c.symbol == "h2: Old Section" && c.kind == SemanticChangeKind::Removed));
    }

    #[test]
    fn markdown_merge_independent_sections() {
        let analyzer = MarkdownAnalyzer;
        let base = b"# Title\n\nIntro.\n\n## A\n\nContent A.\n\n## B\n\nContent B.\n";
        let left = b"# Title\n\nIntro.\n\n## A\n\nUpdated A.\n\n## B\n\nContent B.\n";
        let right = b"# Title\n\nIntro.\n\n## A\n\nContent A.\n\n## B\n\nUpdated B.\n";
        let result = analyzer
            .merge(Path::new("x.md"), Some(base), Some(left), Some(right))
            .unwrap()
            .unwrap();
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn markdown_merge_conflict() {
        let analyzer = MarkdownAnalyzer;
        let base = b"# Title\n\n## A\n\nOriginal.\n";
        let left = b"# Title\n\n## A\n\nLeft version.\n";
        let right = b"# Title\n\n## A\n\nRight version.\n";
        let result = analyzer
            .merge(Path::new("x.md"), Some(base), Some(left), Some(right))
            .unwrap()
            .unwrap();
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].classification, SemanticConflictClassification::DivergentEdit);
    }

    #[test]
    fn markdown_supports_extensions() {
        let analyzer = MarkdownAnalyzer;
        assert!(analyzer.supports_path(Path::new("x.md")));
        assert!(analyzer.supports_path(Path::new("x.mdx")));
        assert!(analyzer.supports_path(Path::new("x.markdown")));
        assert!(!analyzer.supports_path(Path::new("x.txt")));
    }

    // -- Shared helpers --

    #[test]
    fn diff_units_empty() {
        let changes = diff_units(&[], &[]);
        assert!(changes.is_empty());
    }

    #[test]
    fn merge_units_no_conflict() {
        let base = vec![
            StructuralUnit { name: "a".into(), body: "1".into() },
            StructuralUnit { name: "b".into(), body: "2".into() },
        ];
        let left = vec![
            StructuralUnit { name: "a".into(), body: "1-left".into() },
            StructuralUnit { name: "b".into(), body: "2".into() },
        ];
        let right = vec![
            StructuralUnit { name: "a".into(), body: "1".into() },
            StructuralUnit { name: "b".into(), body: "2-right".into() },
        ];
        let (merged, conflicts) = merge_units(&base, &left, &right, "\n");
        assert!(conflicts.is_empty());
        assert!(merged.contains("1-left"));
        assert!(merged.contains("2-right"));
    }
}
