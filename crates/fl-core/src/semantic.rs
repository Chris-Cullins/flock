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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticChangeKind {
    Added,
    Removed,
    Modified,
    TextOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SymbolKind {
    Function,
    Class,
    Method,
}

#[derive(Debug, Clone)]
struct SymbolInfo {
    kind: SymbolKind,
    name: String,
    body_hash: String,
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

        let keys: BTreeSet<String> = old_map.keys().chain(new_map.keys()).cloned().collect();

        for key in keys {
            match (old_map.get(&key), new_map.get(&key)) {
                (None, Some(_)) => changes.push(SemanticChange {
                    kind: SemanticChangeKind::Added,
                    symbol: key,
                }),
                (Some(_), None) => changes.push(SemanticChange {
                    kind: SemanticChangeKind::Removed,
                    symbol: key,
                }),
                (Some(old), Some(new)) if old.body_hash != new.body_hash => {
                    changes.push(SemanticChange {
                        kind: SemanticChangeKind::Modified,
                        symbol: key,
                    })
                }
                _ => {}
            }
        }

        if changes.is_empty() {
            changes.push(SemanticChange {
                kind: SemanticChangeKind::TextOnly,
                symbol: "(non-semantic change)".to_string(),
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
            kind: SemanticChangeKind::TextOnly,
            symbol: "(parser fallback)".to_string(),
        }],
    }))
}

fn to_symbol_map(symbols: Vec<SymbolInfo>) -> BTreeMap<String, SymbolInfo> {
    let mut map = BTreeMap::new();
    for symbol in symbols {
        let prefix = match symbol.kind {
            SymbolKind::Function => "function",
            SymbolKind::Class => "class",
            SymbolKind::Method => "method",
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
                });
            }
        }
        "class_declaration" => {
            if let Some(class_name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Class,
                    name: class_name.clone(),
                    body_hash: node_hash(node, source)?,
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
                let scoped_name = match enclosing_class {
                    Some(class_name) => format!("{}.{}", class_name, method_name),
                    None => method_name,
                };

                symbols.push(SymbolInfo {
                    kind: SymbolKind::Method,
                    name: scoped_name,
                    body_hash: node_hash(node, source)?,
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

fn node_hash(node: Node, source: &[u8]) -> Result<String> {
    let text = node
        .utf8_text(source)
        .context("node text was not valid UTF-8")?;
    Ok(blake3::hash(text.as_bytes()).to_hex().to_string())
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
}
