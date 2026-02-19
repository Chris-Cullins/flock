use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fl_storage::{Event, EventKind, FLOCK_DIR};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const INTELLIGENCE_DIR: &str = "intelligence";
const INDEX_FILE: &str = "index.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelligenceConfig {
    pub enabled: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub confidence_threshold: u32,
}

impl Default for IntelligenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: None,
            model: None,
            endpoint: None,
            confidence_threshold: 70,
        }
    }
}

impl IntelligenceConfig {
    /// Load config from TOML `[intelligence]` section, with env var overrides.
    pub fn load(root: &Path) -> Self {
        let mut cfg = Self::load_from_toml(root).unwrap_or_default();

        if let Ok(endpoint) = env::var("FL_LLM_ENDPOINT") {
            cfg.endpoint = Some(endpoint);
        }
        if let Ok(model) = env::var("FL_LLM_MODEL") {
            cfg.model = Some(model);
        }
        // Provider detection: if we have an API key, auto-detect provider
        if cfg.provider.is_none() {
            if env::var("FL_LLM_API_KEY").is_ok() {
                // Default to openai-compatible if endpoint set, otherwise anthropic
                if cfg.endpoint.is_some() {
                    cfg.provider = Some("openai".to_string());
                } else {
                    cfg.provider = Some("anthropic".to_string());
                }
            }
        }
        cfg
    }

    fn load_from_toml(root: &Path) -> Option<Self> {
        let config_path = root.join(FLOCK_DIR).join("config.toml");
        let content = fs::read_to_string(&config_path).ok()?;

        // Simple TOML section parser — find [intelligence] section and read key=value pairs
        let mut in_section = false;
        let mut cfg = Self::default();
        let mut found = false;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_section = trimmed == "[intelligence]";
                if in_section {
                    found = true;
                }
                continue;
            }
            if !in_section {
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((key, value)) = trimmed.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            match key {
                "enabled" => cfg.enabled = value == "true",
                "provider" => cfg.provider = Some(value.to_string()),
                "model" => cfg.model = Some(value.to_string()),
                "endpoint" => cfg.endpoint = Some(value.to_string()),
                "confidence_threshold" => {
                    cfg.confidence_threshold = value.parse().unwrap_or(70);
                }
                _ => {}
            }
        }

        if found { Some(cfg) } else { None }
    }

    /// Returns true if an LLM API key is available.
    pub fn has_api_key(&self) -> bool {
        env::var("FL_LLM_API_KEY").is_ok()
    }

    /// Returns true if AI features can be used (enabled + has key).
    pub fn ai_available(&self) -> bool {
        self.enabled && self.has_api_key()
    }
}

// ---------------------------------------------------------------------------
// LLM Client
// ---------------------------------------------------------------------------

pub struct LlmClient {
    config: IntelligenceConfig,
    api_key: String,
}

impl LlmClient {
    pub fn new(config: &IntelligenceConfig) -> Result<Self> {
        let api_key =
            env::var("FL_LLM_API_KEY").context("FL_LLM_API_KEY not set")?;
        Ok(Self {
            config: config.clone(),
            api_key,
        })
    }

    /// Send a prompt to the configured LLM and return the response text.
    pub fn call(&self, prompt: &str, system: Option<&str>) -> Result<String> {
        let provider = self
            .config
            .provider
            .as_deref()
            .unwrap_or("anthropic");

        match provider {
            "anthropic" => self.call_anthropic(prompt, system),
            "openai" | _ => self.call_openai(prompt, system),
        }
    }

    fn call_anthropic(&self, prompt: &str, system: Option<&str>) -> Result<String> {
        let endpoint = self
            .config
            .endpoint
            .as_deref()
            .unwrap_or("https://api.anthropic.com/v1/messages");
        let model = self
            .config
            .model
            .as_deref()
            .unwrap_or("claude-sonnet-4-5-20250929");

        let messages = vec![serde_json::json!({
            "role": "user",
            "content": prompt,
        })];

        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": 1024,
            "messages": messages,
        });

        if let Some(sys) = system {
            body["system"] = serde_json::json!(sys);
        }

        let resp = ureq::post(endpoint)
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .send_json(body)
            .context("LLM API call failed")?;

        let text = resp.into_string().context("failed to read LLM response")?;
        let json: serde_json::Value =
            serde_json::from_str(&text).context("failed to parse LLM response")?;

        json["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|block| block["text"].as_str())
            .map(String::from)
            .ok_or_else(|| anyhow!("unexpected Anthropic response format"))
    }

    fn call_openai(&self, prompt: &str, system: Option<&str>) -> Result<String> {
        let endpoint = self
            .config
            .endpoint
            .as_deref()
            .unwrap_or("https://api.openai.com/v1/chat/completions");
        let model = self
            .config
            .model
            .as_deref()
            .unwrap_or("gpt-4o-mini");

        let mut messages = Vec::new();
        if let Some(sys) = system {
            messages.push(serde_json::json!({"role": "system", "content": sys}));
        }
        messages.push(serde_json::json!({"role": "user", "content": prompt}));

        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1024,
            "messages": messages,
        });

        let resp = ureq::post(endpoint)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .send_json(body)
            .context("LLM API call failed")?;

        let text = resp.into_string().context("failed to read LLM response")?;
        let json: serde_json::Value =
            serde_json::from_str(&text).context("failed to parse LLM response")?;

        json["choices"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|choice| choice["message"]["content"].as_str())
            .map(String::from)
            .ok_or_else(|| anyhow!("unexpected OpenAI response format"))
    }
}

// ---------------------------------------------------------------------------
// TF-IDF Search Index
// ---------------------------------------------------------------------------

/// A single indexed document (event).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexedDoc {
    event_id: Uuid,
    /// Combined searchable text for this event.
    text: String,
    /// Precomputed term frequencies.
    tf: HashMap<String, f64>,
}

/// TF-IDF search index stored in `.flock/intelligence/index.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchIndex {
    /// Number of documents indexed.
    pub doc_count: usize,
    /// Inverse document frequency for each term.
    idf: HashMap<String, f64>,
    /// Indexed documents.
    docs: Vec<IndexedDoc>,
}

impl SearchIndex {
    pub fn new() -> Self {
        Self {
            doc_count: 0,
            idf: HashMap::new(),
            docs: Vec::new(),
        }
    }

    fn index_path(root: &Path) -> PathBuf {
        root.join(FLOCK_DIR).join(INTELLIGENCE_DIR).join(INDEX_FILE)
    }

    /// Load an existing index or create a new one.
    pub fn load_or_create(root: &Path) -> Self {
        let path = Self::index_path(root);
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(idx) = serde_json::from_str(&data) {
                return idx;
            }
        }
        Self::new()
    }

    /// Rebuild the index from a list of events.
    pub fn rebuild(events: &[Event]) -> Self {
        let mut idx = Self::new();
        for event in events {
            idx.index_event(event);
        }
        idx.recompute_idf();
        idx
    }

    /// Add a single event to the index (call `recompute_idf` after batch adds).
    pub fn index_event(&mut self, event: &Event) {
        let text = extract_searchable_text(event);
        if text.is_empty() {
            return;
        }
        let tf = compute_tf(&text);
        self.docs.push(IndexedDoc {
            event_id: event.id,
            text,
            tf,
        });
        self.doc_count = self.docs.len();
    }

    /// Recompute IDF values from current documents.
    fn recompute_idf(&mut self) {
        let n = self.docs.len() as f64;
        if n == 0.0 {
            return;
        }
        let mut df: HashMap<String, usize> = HashMap::new();
        for doc in &self.docs {
            for term in doc.tf.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
        }
        self.idf = df
            .into_iter()
            .map(|(term, count)| (term, (n / count as f64).ln()))
            .collect();
    }

    /// Search for events matching the query, returning top `limit` results.
    pub fn search(&self, query: &str, limit: usize) -> Vec<QueryResult> {
        let query_terms = tokenize(query);
        if query_terms.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(Uuid, f64, &str)> = self
            .docs
            .iter()
            .filter_map(|doc| {
                let score: f64 = query_terms
                    .iter()
                    .map(|term| {
                        let tf = doc.tf.get(term.as_str()).copied().unwrap_or(0.0);
                        let idf = self.idf.get(term.as_str()).copied().unwrap_or(0.0);
                        tf * idf
                    })
                    .sum();
                if score > 0.0 {
                    Some((doc.event_id, score, doc.text.as_str()))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        scored
            .into_iter()
            .map(|(event_id, score, text)| QueryResult {
                event_id,
                relevance: score,
                snippet: text.chars().take(200).collect(),
                ai_explanation: None,
            })
            .collect()
    }

    /// Number of unique terms in the index.
    pub fn term_count(&self) -> usize {
        self.idf.len()
    }

    /// Persist the index to disk.
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::index_path(root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string(self)
            .context("failed to serialize search index")?;
        fs::write(&path, json).context("failed to write search index")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub event_id: Uuid,
    pub relevance: f64,
    pub snippet: String,
    pub ai_explanation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentExtractionResult {
    pub intent: String,
    pub confidence: u32,
    pub categories: Vec<String>,
    pub source: IntentSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IntentSource {
    Ai,
    Heuristic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictSuggestionResult {
    pub suggestion: String,
    pub code: Option<String>,
    pub confidence: u32,
    pub reasoning: String,
    pub source: IntentSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceScore {
    pub score: u32,
    pub factors: Vec<ConfidenceFactor>,
    pub recommendation: ConfidenceRecommendation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceFactor {
    pub name: String,
    pub score: u32,
    pub weight: f64,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConfidenceRecommendation {
    Proceed,
    Review,
    Block,
}

impl std::fmt::Display for ConfidenceRecommendation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proceed => f.write_str("proceed"),
            Self::Review => f.write_str("review"),
            Self::Block => f.write_str("block"),
        }
    }
}

/// Stats about the search index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    pub document_count: usize,
    pub term_count: usize,
    pub index_size_bytes: u64,
}

/// Report from a full index rebuild.
#[derive(Debug, Clone)]
pub struct IntelligenceIndexReport {
    pub events_indexed: usize,
    pub terms_indexed: usize,
}

// ---------------------------------------------------------------------------
// Core functions
// ---------------------------------------------------------------------------

/// Query event history using TF-IDF search, optionally enhanced with AI.
pub fn query_history(
    events: &[Event],
    query: &str,
    limit: usize,
    llm_client: Option<&LlmClient>,
    root: &Path,
) -> Result<Vec<QueryResult>> {
    let mut index = SearchIndex::load_or_create(root);

    // Auto-rebuild if index is empty but we have events
    if index.doc_count == 0 && !events.is_empty() {
        index = SearchIndex::rebuild(events);
        let _ = index.save(root);
    }

    let mut results = index.search(query, limit);

    // Optionally enhance results with AI explanation
    if let Some(client) = llm_client {
        if let Some(first) = results.first_mut() {
            let prompt = format!(
                "Given this version control query: \"{}\"\n\n\
                 And this matching event text: \"{}\"\n\n\
                 Explain in one sentence why this event is relevant to the query.",
                query, first.snippet
            );
            if let Ok(explanation) = client.call(&prompt, Some("You are a version control assistant. Be concise.")) {
                first.ai_explanation = Some(explanation);
            }
        }
    }

    Ok(results)
}

/// Extract intent from a diff and commit message using AI, falling back to heuristics.
pub fn extract_intent(
    changed_files: &[String],
    message: Option<&str>,
    llm_client: Option<&LlmClient>,
) -> IntentExtractionResult {
    // Try AI first
    if let Some(client) = llm_client {
        let files_summary = changed_files.join(", ");
        let msg = message.unwrap_or("(no message)");
        let prompt = format!(
            "Classify this code change intent in 1-3 words.\n\
             Changed files: {}\n\
             Commit message: {}\n\n\
             Respond with ONLY the intent classification (e.g. 'bug fix', 'new feature', 'refactor', 'documentation', 'test', 'performance', 'security fix').",
            files_summary, msg
        );
        if let Ok(intent) = client.call(&prompt, Some("You classify code changes. Respond with only the classification, nothing else.")) {
            let intent = intent.trim().to_string();
            let categories = categorize_intent(&intent);
            return IntentExtractionResult {
                intent,
                confidence: 85,
                categories,
                source: IntentSource::Ai,
            };
        }
    }

    // Heuristic fallback
    heuristic_intent(changed_files, message)
}

/// Suggest a conflict resolution using AI, falling back to rule-based suggestion.
pub fn suggest_conflict_resolution(
    classification: Option<&str>,
    path: Option<&str>,
    symbol: Option<&str>,
    llm_client: Option<&LlmClient>,
) -> ConflictSuggestionResult {
    // Try AI first
    if let Some(client) = llm_client {
        let prompt = format!(
            "Suggest how to resolve this merge conflict:\n\
             Classification: {}\n\
             File: {}\n\
             Symbol: {}\n\n\
             Provide a brief strategy (2-3 sentences).",
            classification.unwrap_or("unknown"),
            path.unwrap_or("unknown"),
            symbol.unwrap_or("unknown"),
        );
        if let Ok(suggestion) = client.call(
            &prompt,
            Some("You are a merge conflict resolution assistant. Be practical and concise."),
        ) {
            return ConflictSuggestionResult {
                suggestion: suggestion.trim().to_string(),
                code: None,
                confidence: 75,
                reasoning: "AI-generated suggestion based on conflict context.".to_string(),
                source: IntentSource::Ai,
            };
        }
    }

    // Heuristic fallback
    heuristic_conflict_suggestion(classification)
}

/// Calculate a confidence score for the current session state.
pub fn calculate_confidence(
    checkpoint_count: usize,
    has_tests: bool,
    conflict_count: usize,
    gate_pending_count: usize,
    high_risk_changes: usize,
    threshold: u32,
) -> ConfidenceScore {
    let mut factors = Vec::new();

    // Factor: checkpoint frequency
    let checkpoint_score = (checkpoint_count.min(10) as u32) * 10;
    factors.push(ConfidenceFactor {
        name: "checkpoint_coverage".to_string(),
        score: checkpoint_score,
        weight: 0.25,
        detail: format!("{} checkpoints", checkpoint_count),
    });

    // Factor: test presence
    let test_score = if has_tests { 90 } else { 40 };
    factors.push(ConfidenceFactor {
        name: "test_coverage".to_string(),
        score: test_score,
        weight: 0.25,
        detail: if has_tests {
            "tests present".to_string()
        } else {
            "no tests detected".to_string()
        },
    });

    // Factor: conflicts
    let conflict_score = if conflict_count == 0 {
        100
    } else {
        (100u32).saturating_sub(conflict_count as u32 * 20)
    };
    factors.push(ConfidenceFactor {
        name: "conflict_free".to_string(),
        score: conflict_score,
        weight: 0.25,
        detail: format!("{} unresolved conflicts", conflict_count),
    });

    // Factor: risk assessment
    let risk_score = if high_risk_changes == 0 {
        100
    } else {
        (100u32).saturating_sub(high_risk_changes as u32 * 15)
    };
    factors.push(ConfidenceFactor {
        name: "risk_level".to_string(),
        score: risk_score,
        weight: 0.15,
        detail: format!("{} high-risk changes", high_risk_changes),
    });

    // Factor: gate status
    let gate_score = if gate_pending_count == 0 { 100 } else { 30 };
    factors.push(ConfidenceFactor {
        name: "gate_status".to_string(),
        score: gate_score,
        weight: 0.10,
        detail: format!("{} pending gates", gate_pending_count),
    });

    // Weighted average
    let total_weight: f64 = factors.iter().map(|f| f.weight).sum();
    let weighted_sum: f64 = factors
        .iter()
        .map(|f| f.score as f64 * f.weight)
        .sum();
    let score = (weighted_sum / total_weight) as u32;

    let recommendation = if score >= threshold {
        ConfidenceRecommendation::Proceed
    } else if score >= threshold.saturating_sub(20) {
        ConfidenceRecommendation::Review
    } else {
        ConfidenceRecommendation::Block
    };

    ConfidenceScore {
        score,
        factors,
        recommendation,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract searchable text from an event for indexing.
fn extract_searchable_text(event: &Event) -> String {
    let mut parts = vec![event.actor.clone()];

    match &event.kind {
        EventKind::Checkpoint(cp) => {
            parts.push(cp.label.clone());
            if let Some(msg) = &cp.message {
                parts.push(msg.clone());
            }
            if let Some(intent) = &cp.ai_intent {
                parts.push(intent.clone());
            }
        }
        EventKind::Exploration(ex) => {
            parts.push(ex.title.clone());
        }
        EventKind::Task(task) => {
            parts.push(task.title.clone());
            if let Some(desc) = &task.description {
                parts.push(desc.clone());
            }
        }
        EventKind::Session(session) => {
            parts.push(session.agent.clone());
            if let Some(desc) = &session.task_description {
                parts.push(desc.clone());
            }
        }
        EventKind::ConflictResolution(cr) => {
            if let Some(path) = &cr.path {
                parts.push(path.clone());
            }
            if let Some(sym) = &cr.symbol {
                parts.push(sym.clone());
            }
            if let Some(cls) = &cr.classification {
                parts.push(cls.clone());
            }
        }
        EventKind::GitBridge(gb) => {
            parts.push(gb.detail.clone());
        }
        _ => {}
    }

    parts.join(" ")
}

/// Tokenize text into lowercase terms.
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| s.len() >= 2)
        .map(String::from)
        .collect()
}

/// Compute term frequency for a text.
fn compute_tf(text: &str) -> HashMap<String, f64> {
    let tokens = tokenize(text);
    let total = tokens.len() as f64;
    if total == 0.0 {
        return HashMap::new();
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for token in &tokens {
        *counts.entry(token.clone()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(term, count)| (term, count as f64 / total))
        .collect()
}

/// Heuristic intent classification based on file paths and message keywords.
fn heuristic_intent(changed_files: &[String], message: Option<&str>) -> IntentExtractionResult {
    let msg_lower = message.unwrap_or("").to_lowercase();
    let files_lower: Vec<String> = changed_files.iter().map(|f| f.to_lowercase()).collect();

    let intent = if msg_lower.contains("fix") || msg_lower.contains("bug") {
        "bug fix"
    } else if msg_lower.contains("feat") || msg_lower.contains("add") {
        "new feature"
    } else if msg_lower.contains("refactor") || msg_lower.contains("clean") {
        "refactor"
    } else if msg_lower.contains("doc") || msg_lower.contains("readme") {
        "documentation"
    } else if msg_lower.contains("test") || files_lower.iter().any(|f| f.contains("test")) {
        "test"
    } else if msg_lower.contains("perf") || msg_lower.contains("optim") {
        "performance"
    } else if msg_lower.contains("secur") || msg_lower.contains("vuln") {
        "security fix"
    } else if files_lower.iter().any(|f| f.contains("config") || f.contains(".toml") || f.contains(".yaml")) {
        "configuration"
    } else {
        "general change"
    };

    let categories = categorize_intent(intent);

    IntentExtractionResult {
        intent: intent.to_string(),
        confidence: 50,
        categories,
        source: IntentSource::Heuristic,
    }
}

fn categorize_intent(intent: &str) -> Vec<String> {
    let lower = intent.to_lowercase();
    let mut cats = Vec::new();
    if lower.contains("fix") || lower.contains("bug") {
        cats.push("bugfix".to_string());
    }
    if lower.contains("feat") || lower.contains("new") || lower.contains("add") {
        cats.push("feature".to_string());
    }
    if lower.contains("refactor") || lower.contains("clean") {
        cats.push("maintenance".to_string());
    }
    if lower.contains("doc") {
        cats.push("documentation".to_string());
    }
    if lower.contains("test") {
        cats.push("testing".to_string());
    }
    if lower.contains("perf") {
        cats.push("performance".to_string());
    }
    if lower.contains("secur") {
        cats.push("security".to_string());
    }
    if cats.is_empty() {
        cats.push("general".to_string());
    }
    cats
}

fn heuristic_conflict_suggestion(classification: Option<&str>) -> ConflictSuggestionResult {
    let suggestion = match classification {
        Some("DivergentEdit") => {
            "Both sides modified the same symbol. Review both versions and choose one, or manually merge the changes."
        }
        Some("DeleteVsEdit") => {
            "One side deleted a symbol while the other modified it. Decide whether the deletion or the modification should win."
        }
        Some("ConcurrentAddition") => {
            "Both sides added the same symbol with different implementations. Choose one implementation or combine them."
        }
        Some("KindMismatch") => {
            "The symbol was changed to different kinds on each side. Choose which kind is correct."
        }
        Some("TextFallback") => {
            "Could not perform semantic merge. Resolve conflict markers manually in the file."
        }
        _ => "Review the conflict and resolve manually.",
    };

    ConflictSuggestionResult {
        suggestion: suggestion.to_string(),
        code: None,
        confidence: 40,
        reasoning: "Rule-based suggestion from conflict classification.".to_string(),
        source: IntentSource::Heuristic,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use fl_storage::CheckpointEvent;

    fn make_checkpoint(id: u128, label: &str, message: &str) -> Event {
        Event {
            id: Uuid::from_u128(id),
            timestamp: "2026-02-16T12:00:00Z".to_string(),
            actor: "test-actor".to_string(),
            parent_id: None,
            signer_public_key: None,
            signature: None,
            prev_event_hash: None,
            exploration_id: None,
            session_id: None,
            workspace_name: None,
            kind: EventKind::Checkpoint(CheckpointEvent {
                label: label.to_string(),
                message: Some(message.to_string()),
                snapshot_id: Uuid::from_u128(id + 1000),
                parent_checkpoint_event: None,
                snapshot_merkle_root: None,
                ai_intent: None,
                intent_confidence: None,
                files_changed: None,
                category: None,
                scope_label: None,
                structured_description: None,
                git_commit_sha: None,
            }),
        }
    }

    #[test]
    fn test_tokenize() {
        let tokens = tokenize("Hello World, this is a test_case!");
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
        assert!(tokens.contains(&"test_case".to_string()));
        // Single-char tokens are filtered
        assert!(!tokens.contains(&"a".to_string()));
    }

    #[test]
    fn test_compute_tf() {
        let tf = compute_tf("hello world hello");
        assert!(tf["hello"] > tf["world"]);
    }

    #[test]
    fn test_search_index_build_and_search() {
        let events = vec![
            make_checkpoint(1, "auth-fix", "Fix authentication bug in login flow"),
            make_checkpoint(2, "add-cache", "Add Redis caching layer for API responses"),
            make_checkpoint(3, "refactor-db", "Refactor database connection pooling"),
        ];

        let index = SearchIndex::rebuild(&events);
        assert_eq!(index.doc_count, 3);

        let results = index.search("authentication login", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].event_id, Uuid::from_u128(1));

        let results = index.search("cache api", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].event_id, Uuid::from_u128(2));
    }

    #[test]
    fn test_search_index_no_results() {
        let events = vec![make_checkpoint(1, "test", "Simple test commit")];
        let index = SearchIndex::rebuild(&events);
        let results = index.search("nonexistent xyz", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_heuristic_intent_bugfix() {
        let result = heuristic_intent(&["src/auth.rs".to_string()], Some("Fix login bug"));
        assert_eq!(result.intent, "bug fix");
        assert_eq!(result.source, IntentSource::Heuristic);
        assert!(result.categories.contains(&"bugfix".to_string()));
    }

    #[test]
    fn test_heuristic_intent_feature() {
        let result = heuristic_intent(&["src/new.rs".to_string()], Some("Add user profiles"));
        assert_eq!(result.intent, "new feature");
        assert!(result.categories.contains(&"feature".to_string()));
    }

    #[test]
    fn test_heuristic_intent_test_files() {
        let result = heuristic_intent(&["tests/auth_test.rs".to_string()], Some("update checks"));
        assert_eq!(result.intent, "test");
    }

    #[test]
    fn test_heuristic_intent_general() {
        let result = heuristic_intent(&["src/main.rs".to_string()], Some("misc changes"));
        assert_eq!(result.intent, "general change");
    }

    #[test]
    fn test_heuristic_conflict_suggestion() {
        let result = heuristic_conflict_suggestion(Some("DivergentEdit"));
        assert!(result.suggestion.contains("Both sides modified"));
        assert_eq!(result.source, IntentSource::Heuristic);

        let result = heuristic_conflict_suggestion(None);
        assert!(result.suggestion.contains("manually"));
    }

    #[test]
    fn test_confidence_score_high() {
        let score = calculate_confidence(10, true, 0, 0, 0, 70);
        assert!(score.score >= 70);
        assert_eq!(score.recommendation, ConfidenceRecommendation::Proceed);
    }

    #[test]
    fn test_confidence_score_low() {
        let score = calculate_confidence(0, false, 5, 2, 5, 70);
        assert!(score.score < 70);
        assert!(matches!(
            score.recommendation,
            ConfidenceRecommendation::Review | ConfidenceRecommendation::Block
        ));
    }

    #[test]
    fn test_confidence_factors() {
        let score = calculate_confidence(5, true, 1, 0, 0, 70);
        assert_eq!(score.factors.len(), 5);
        assert!(score.factors.iter().any(|f| f.name == "checkpoint_coverage"));
        assert!(score.factors.iter().any(|f| f.name == "test_coverage"));
        assert!(score.factors.iter().any(|f| f.name == "conflict_free"));
        assert!(score.factors.iter().any(|f| f.name == "risk_level"));
        assert!(score.factors.iter().any(|f| f.name == "gate_status"));
    }

    #[test]
    fn test_intelligence_config_default() {
        let cfg = IntelligenceConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.confidence_threshold, 70);
        assert!(cfg.provider.is_none());
    }

    #[test]
    fn test_extract_searchable_text_checkpoint() {
        let event = make_checkpoint(1, "my-label", "Fix the login page");
        let text = extract_searchable_text(&event);
        assert!(text.contains("my-label"));
        assert!(text.contains("Fix the login page"));
        assert!(text.contains("test-actor"));
    }

    #[test]
    fn test_categorize_intent() {
        assert!(categorize_intent("bug fix").contains(&"bugfix".to_string()));
        assert!(categorize_intent("new feature").contains(&"feature".to_string()));
        assert!(categorize_intent("refactor code").contains(&"maintenance".to_string()));
        assert!(categorize_intent("xyz").contains(&"general".to_string()));
    }

    #[test]
    fn test_index_stats() {
        let events = vec![
            make_checkpoint(1, "a", "First commit"),
            make_checkpoint(2, "b", "Second commit"),
        ];
        let index = SearchIndex::rebuild(&events);
        assert_eq!(index.doc_count, 2);
        assert!(!index.idf.is_empty());
    }
}
