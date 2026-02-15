use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticFileDiff {
    pub path: String,
    pub language: String,
    pub parse_fallback: bool,
    pub changes: Vec<SemanticChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticChange {
    pub kind: SemanticChangeKind,
    pub symbol: String,
    pub risk: SemanticRisk,
    #[serde(default)]
    pub impact: SemanticImpact,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticImpact {
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub modules: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticChangeKind {
    Added,
    Removed,
    Modified,
    Renamed,
    Moved,
    #[serde(alias = "TextOnly")]
    StyleOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SemanticRisk {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SymbolKind {
    Function,
    Class,
    Method,
    Constructor,
    Field,
    Interface,
    TypeAlias,
    Enum,
    Export,
}

#[derive(Debug, Clone)]
struct SymbolInfo {
    kind: SymbolKind,
    name: String,
    body_hash: String,
    match_hash: String,
}

#[derive(Debug, Clone, Copy)]
enum SourceLanguage {
    JavaScript,
    Jsx,
    TypeScript,
    Tsx,
}

impl SourceLanguage {
    fn from_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("js") => Some(Self::JavaScript),
            Some("jsx") => Some(Self::Jsx),
            Some("ts") => Some(Self::TypeScript),
            Some("tsx") => Some(Self::Tsx),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::JavaScript => "javascript",
            Self::Jsx => "jsx",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
        }
    }
}

pub fn supported_source(path: &Path) -> bool {
    SourceLanguage::from_path(path).is_some()
}

pub fn diff(
    path: &Path,
    old_source: Option<&[u8]>,
    new_source: Option<&[u8]>,
) -> Result<Option<SemanticFileDiff>> {
    let Some(language) = SourceLanguage::from_path(path) else {
        return Ok(None);
    };

    let old_source = old_source.unwrap_or_default();
    let new_source = new_source.unwrap_or_default();

    if old_source == new_source {
        return Ok(None);
    }

    let old_symbols = parse_symbols(language, old_source);
    let new_symbols = parse_symbols(language, new_source);

    if let (Ok(old_symbols), Ok(new_symbols)) = (old_symbols, new_symbols) {
        let mut changes = Vec::new();

        let old_map = to_symbol_map(old_symbols);
        let new_map = to_symbol_map(new_symbols);
        let mut removed_candidates = BTreeSet::new();
        let mut added_candidates = BTreeSet::new();

        let keys: BTreeSet<String> = old_map.keys().chain(new_map.keys()).cloned().collect();

        for key in keys {
            match (old_map.get(&key), new_map.get(&key)) {
                (None, Some(_)) => {
                    added_candidates.insert(key);
                }
                (Some(_), None) => {
                    removed_candidates.insert(key);
                }
                (Some(old), Some(new)) if old.body_hash != new.body_hash => {
                    changes.push(SemanticChange {
                        kind: SemanticChangeKind::Modified,
                        symbol: key.clone(),
                        risk: score_risk(SemanticChangeKind::Modified, &key),
                        impact: base_impact(&key),
                    })
                }
                _ => {}
            }
        }

        let mut removed_by_fingerprint: BTreeMap<(SymbolKind, String), Vec<String>> =
            BTreeMap::new();
        for key in &removed_candidates {
            let Some(old_symbol) = old_map.get(key) else {
                continue;
            };

            removed_by_fingerprint
                .entry((old_symbol.kind, old_symbol.match_hash.clone()))
                .or_default()
                .push(key.clone());
        }

        let mut consumed_removed = BTreeSet::new();
        let mut consumed_added = BTreeSet::new();

        for key in &added_candidates {
            let Some(new_symbol) = new_map.get(key) else {
                continue;
            };

            let fingerprint = (new_symbol.kind, new_symbol.match_hash.clone());
            let mut remove_fingerprint = false;
            let old_symbol_key =
                removed_by_fingerprint
                    .get_mut(&fingerprint)
                    .and_then(|candidates| {
                        if candidates.is_empty() {
                            return None;
                        }

                        let moved_candidate_index = candidates.iter().position(|candidate| {
                            symbol_leaf_name(candidate) == symbol_leaf_name(key)
                        });
                        let selected_index = moved_candidate_index.unwrap_or(0);
                        let selected = candidates.remove(selected_index);
                        if candidates.is_empty() {
                            remove_fingerprint = true;
                        }
                        Some(selected)
                    });

            if remove_fingerprint {
                removed_by_fingerprint.remove(&fingerprint);
            }

            if let Some(old_key) = old_symbol_key {
                consumed_removed.insert(old_key.clone());
                consumed_added.insert(key.clone());
                changes.push(SemanticChange {
                    kind: relocation_kind(&old_key, key),
                    symbol: format!("{old_key} -> {key}"),
                    risk: score_relocation_risk(&old_key, key),
                    impact: base_impact(&format!("{old_key} -> {key}")),
                });
            }
        }

        for key in removed_candidates.difference(&consumed_removed) {
            changes.push(SemanticChange {
                kind: SemanticChangeKind::Removed,
                symbol: key.clone(),
                risk: score_risk(SemanticChangeKind::Removed, key),
                impact: base_impact(key),
            });
        }

        for key in added_candidates.difference(&consumed_added) {
            changes.push(SemanticChange {
                kind: SemanticChangeKind::Added,
                symbol: key.clone(),
                risk: score_risk(SemanticChangeKind::Added, key),
                impact: base_impact(key),
            });
        }

        if changes.is_empty() {
            changes.push(SemanticChange {
                kind: SemanticChangeKind::StyleOnly,
                symbol: "(style-only change)".to_string(),
                risk: SemanticRisk::Low,
                impact: base_impact("(style-only change)"),
            });
        }

        return Ok(Some(SemanticFileDiff {
            path: path.to_string_lossy().to_string(),
            language: language.name().to_string(),
            parse_fallback: false,
            changes,
        }));
    }

    Ok(Some(SemanticFileDiff {
        path: path.to_string_lossy().to_string(),
        language: language.name().to_string(),
        parse_fallback: true,
        changes: vec![SemanticChange {
            kind: SemanticChangeKind::StyleOnly,
            symbol: "(parser fallback)".to_string(),
            risk: SemanticRisk::High,
            impact: base_impact("(parser fallback)"),
        }],
    }))
}

fn base_impact(symbol: &str) -> SemanticImpact {
    SemanticImpact {
        symbols: impact_symbols(symbol),
        files: Vec::new(),
        modules: Vec::new(),
    }
}

fn impact_symbols(symbol: &str) -> Vec<String> {
    let mut symbols = BTreeSet::new();
    for part in symbol.split("->") {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = trimmed
            .split_once(':')
            .map(|(_, value)| value.trim())
            .unwrap_or(trimmed);
        if !value.is_empty() {
            symbols.insert(value.to_string());
        }
    }

    if symbols.is_empty() {
        symbols.insert(symbol.to_string());
    }

    symbols.into_iter().collect()
}

fn symbol_leaf_name(key: &str) -> &str {
    let (_, name) = key.split_once(':').unwrap_or(("", key));
    name.rsplit('.').next().unwrap_or(name)
}

fn relocation_kind(old_key: &str, new_key: &str) -> SemanticChangeKind {
    if symbol_leaf_name(old_key) == symbol_leaf_name(new_key) {
        SemanticChangeKind::Moved
    } else {
        SemanticChangeKind::Renamed
    }
}

fn score_relocation_risk(old_key: &str, new_key: &str) -> SemanticRisk {
    let kind = relocation_kind(old_key, new_key);
    std::cmp::max(score_risk(kind, old_key), score_risk(kind, new_key))
}

fn score_risk(kind: SemanticChangeKind, symbol: &str) -> SemanticRisk {
    let symbol_kind = symbol_kind_from_key(symbol);
    match kind {
        SemanticChangeKind::StyleOnly => SemanticRisk::Low,
        SemanticChangeKind::Added => match symbol_kind {
            Some(
                SymbolKind::Class
                | SymbolKind::Interface
                | SymbolKind::TypeAlias
                | SymbolKind::Enum
                | SymbolKind::Export,
            ) => SemanticRisk::Medium,
            Some(SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor) => {
                SemanticRisk::Medium
            }
            Some(SymbolKind::Field) => SemanticRisk::Low,
            None => SemanticRisk::Medium,
        },
        SemanticChangeKind::Removed => match symbol_kind {
            Some(SymbolKind::Field) => SemanticRisk::Medium,
            Some(_) => SemanticRisk::High,
            None => SemanticRisk::High,
        },
        SemanticChangeKind::Modified => match symbol_kind {
            Some(
                SymbolKind::Interface
                | SymbolKind::TypeAlias
                | SymbolKind::Enum
                | SymbolKind::Export,
            ) => SemanticRisk::High,
            Some(SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor) => {
                SemanticRisk::Medium
            }
            Some(SymbolKind::Class | SymbolKind::Field) => SemanticRisk::Medium,
            None => SemanticRisk::Medium,
        },
        SemanticChangeKind::Renamed | SemanticChangeKind::Moved => match symbol_kind {
            Some(SymbolKind::Field) => SemanticRisk::Low,
            Some(SymbolKind::Export) => SemanticRisk::High,
            Some(_) => SemanticRisk::Medium,
            None => SemanticRisk::Medium,
        },
    }
}

fn symbol_kind_from_key(symbol: &str) -> Option<SymbolKind> {
    let (prefix, _) = symbol.split_once(':')?;
    match prefix {
        "function" => Some(SymbolKind::Function),
        "class" => Some(SymbolKind::Class),
        "method" => Some(SymbolKind::Method),
        "constructor" => Some(SymbolKind::Constructor),
        "field" => Some(SymbolKind::Field),
        "interface" => Some(SymbolKind::Interface),
        "type" => Some(SymbolKind::TypeAlias),
        "enum" => Some(SymbolKind::Enum),
        "export" => Some(SymbolKind::Export),
        _ => None,
    }
}

fn to_symbol_map(symbols: Vec<SymbolInfo>) -> BTreeMap<String, SymbolInfo> {
    let mut map = BTreeMap::new();
    for symbol in symbols {
        let prefix = match symbol.kind {
            SymbolKind::Function => "function",
            SymbolKind::Class => "class",
            SymbolKind::Method => "method",
            SymbolKind::Constructor => "constructor",
            SymbolKind::Field => "field",
            SymbolKind::Interface => "interface",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Enum => "enum",
            SymbolKind::Export => "export",
        };

        let key = format!("{}:{}", prefix, symbol.name);
        map.insert(key, symbol);
    }

    map
}

fn parse_symbols(language: SourceLanguage, source: &[u8]) -> Result<Vec<SymbolInfo>> {
    let mut parser = Parser::new();
    match language {
        SourceLanguage::JavaScript => parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .context("failed to load JavaScript grammar")?,
        SourceLanguage::Jsx => parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .context("failed to load JSX grammar")?,
        SourceLanguage::TypeScript => parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .context("failed to load TypeScript grammar")?,
        SourceLanguage::Tsx => parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
            .context("failed to load TSX grammar")?,
    };

    if source.is_empty() {
        return Ok(Vec::new());
    }

    let tree = parser
        .parse(source, None)
        .context("tree-sitter parse returned no tree")?;

    let root = tree.root_node();
    let mut symbols = Vec::new();
    extract_symbols(root, source, None, &mut symbols)?;

    Ok(symbols)
}

fn extract_symbols(
    node: Node,
    source: &[u8],
    enclosing_class: Option<&str>,
    symbols: &mut Vec<SymbolInfo>,
) -> Result<()> {
    match node.kind() {
        "function_declaration" => {
            if let Some(name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Function,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                });
            }
        }
        "variable_declarator" => {
            if let Some(value) = node.child_by_field_name("value") {
                if matches!(value.kind(), "arrow_function" | "function_expression") {
                    if let Some(name) = symbol_name(node, source) {
                        symbols.push(SymbolInfo {
                            kind: SymbolKind::Function,
                            name,
                            body_hash: node_hash(node, source)?,
                            match_hash: node_match_hash(node, source)?,
                        });
                    }
                }
            }
        }
        "class_declaration" => {
            if let Some(class_name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Class,
                    name: class_name.clone(),
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                });

                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    extract_symbols(child, source, Some(&class_name), symbols)?;
                }
                return Ok(());
            }
        }
        "method_definition" => {
            if let Some(method_name) = symbol_name(node, source) {
                if method_name == "constructor" {
                    let scoped_name = match enclosing_class {
                        Some(class_name) => format!("{}.constructor", class_name),
                        None => "constructor".to_string(),
                    };
                    symbols.push(SymbolInfo {
                        kind: SymbolKind::Constructor,
                        name: scoped_name,
                        body_hash: node_hash(node, source)?,
                        match_hash: node_match_hash(node, source)?,
                    });
                } else {
                    let scoped_name = match enclosing_class {
                        Some(class_name) => format!("{}.{}", class_name, method_name),
                        None => method_name,
                    };

                    symbols.push(SymbolInfo {
                        kind: SymbolKind::Method,
                        name: scoped_name,
                        body_hash: node_hash(node, source)?,
                        match_hash: node_match_hash(node, source)?,
                    });
                }
            }
        }
        "public_field_definition" | "field_definition" => {
            if let Some(field_name) = symbol_name(node, source) {
                let scoped_name = match enclosing_class {
                    Some(class_name) => format!("{}.{}", class_name, field_name),
                    None => field_name,
                };
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Field,
                    name: scoped_name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                });
            }
        }
        "interface_declaration" => {
            if let Some(name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Interface,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                });
            }
        }
        "type_alias_declaration" => {
            if let Some(name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::TypeAlias,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                });
            }
        }
        "enum_declaration" => {
            if let Some(name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Enum,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                });
            }
        }
        "export_statement" => {
            if let Some(name) = export_symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Export,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                });
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        extract_symbols(child, source, enclosing_class, symbols)?;
    }

    Ok(())
}

fn symbol_name(node: Node, source: &[u8]) -> Option<String> {
    let name = node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;

    Some(name.to_string())
}

fn export_symbol_name(node: Node, source: &[u8]) -> Option<String> {
    if let Some(declaration) = node.child_by_field_name("declaration") {
        if let Some(name) = symbol_name(declaration, source) {
            return Some(format!("declaration:{}", name));
        }
        return Some(format!("declaration:{}", compact_text(declaration, source)));
    }

    if let Some(value) = node.child_by_field_name("value") {
        return Some(format!("value:{}", compact_text(value, source)));
    }

    if let Some(source_node) = node.child_by_field_name("source") {
        let src = compact_text(source_node, source);

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "export_clause" || child.kind() == "namespace_export" {
                let clause = compact_text(child, source);
                return Some(format!("{} from {}", clause, src));
            }
        }

        return Some(format!("from {}", src));
    }

    Some(compact_text(node, source))
}

fn compact_text(node: Node, source: &[u8]) -> String {
    let raw = node.utf8_text(source).unwrap_or_default();
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn node_hash(node: Node, source: &[u8]) -> Result<String> {
    let text = node
        .utf8_text(source)
        .context("node text was not valid UTF-8")?;
    Ok(blake3::hash(text.as_bytes()).to_hex().to_string())
}

fn node_match_hash(node: Node, source: &[u8]) -> Result<String> {
    let text = node
        .utf8_text(source)
        .context("node text was not valid UTF-8")?;

    let Some(name_node) = node.child_by_field_name("name") else {
        return Ok(blake3::hash(text.as_bytes()).to_hex().to_string());
    };

    let node_start = node.start_byte();
    let name_start = name_node.start_byte();
    let name_end = name_node.end_byte();
    if name_start < node_start || name_end > node.end_byte() {
        return Ok(blake3::hash(text.as_bytes()).to_hex().to_string());
    }

    let start = name_start - node_start;
    let end = name_end - node_start;
    if start > text.len() || end > text.len() || start > end {
        return Ok(blake3::hash(text.as_bytes()).to_hex().to_string());
    }

    let mut normalized = String::with_capacity(text.len() + "<name>".len());
    normalized.push_str(&text[..start]);
    normalized.push_str("<name>");
    normalized.push_str(&text[end..]);

    Ok(blake3::hash(normalized.as_bytes()).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_modified_function() {
        let old = b"function add(a, b) { return a + b; }";
        let new = b"function add(a, b) { return a - b; }";

        let diff = diff(Path::new("math.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(
            diff.changes
                .iter()
                .any(|c| c.kind == SemanticChangeKind::Modified)
        );
        assert!(
            diff.changes.iter().any(|c| {
                c.kind == SemanticChangeKind::Modified && c.risk == SemanticRisk::Medium
            })
        );
    }

    #[test]
    fn detects_added_method() {
        let old = b"class A { x() {} }";
        let new = b"class A { x() {} y() {} }";

        let diff = diff(Path::new("a.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(
            diff.changes
                .iter()
                .any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "method:A.y")
        );
    }

    #[test]
    fn detects_arrow_function_and_type_nodes() {
        let old = b"const add = (a, b) => a + b;";
        let new = b"const add = (a, b) => a + b; interface User { id: string }; type Id = string; enum Kind { A, B }";

        let diff = diff(Path::new("symbols.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(
            diff.changes
                .iter()
                .any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "interface:User")
        );
        assert!(
            diff.changes
                .iter()
                .any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "type:Id")
        );
        assert!(
            diff.changes
                .iter()
                .any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "enum:Kind")
        );
    }

    #[test]
    fn detects_re_export_changes() {
        let old = b"export { foo } from './a';";
        let new = b"export { foo, bar } from './a';";

        let diff = diff(Path::new("reexport.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| c.symbol.starts_with("export:")));
    }

    #[test]
    fn detects_renamed_function() {
        let old = b"function add(a, b) { return a + b; }";
        let new = b"function sum(a, b) { return a + b; }";

        let diff = diff(Path::new("rename.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| {
            c.kind == SemanticChangeKind::Renamed && c.symbol == "function:add -> function:sum"
        }));
        assert!(diff.changes.iter().any(|c| {
            c.kind == SemanticChangeKind::Renamed
                && c.impact.symbols.iter().any(|value| value == "add")
                && c.impact.symbols.iter().any(|value| value == "sum")
        }));
    }

    #[test]
    fn detects_moved_method() {
        let old = b"class A { x() {} } class B {}";
        let new = b"class A {} class B { x() {} }";

        let diff = diff(Path::new("move.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| {
            c.kind == SemanticChangeKind::Moved && c.symbol == "method:A.x -> method:B.x"
        }));
    }

    #[test]
    fn detects_style_only_file_change() {
        let old = b"function add(a, b) { return a + b; }\n";
        let new = b"function add(a, b) { return a + b; }\n\n// style-only comment\n";

        let diff = diff(Path::new("style.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| {
            c.kind == SemanticChangeKind::StyleOnly
                && c.symbol == "(style-only change)"
                && c.risk == SemanticRisk::Low
        }));
    }

    #[test]
    fn scores_removed_exports_as_high_risk() {
        let old = b"export { foo } from './a';";
        let new = b"";

        let diff = diff(Path::new("exports.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| {
            c.kind == SemanticChangeKind::Removed
                && c.symbol.starts_with("export:")
                && c.risk == SemanticRisk::High
        }));
    }
}
