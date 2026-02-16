pub mod ast_cache;
pub mod structured;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};
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
pub struct SemanticMergeResult {
    pub path: String,
    pub language: String,
    pub parse_fallback: bool,
    pub merged_source: String,
    #[serde(default)]
    pub conflicts: Vec<SemanticMergeConflict>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticMergeConflict {
    pub symbol: String,
    #[serde(default)]
    pub classification: SemanticConflictClassification,
    #[serde(default, alias = "detail")]
    pub explanation: String,
    #[serde(default)]
    pub affected_files: Vec<String>,
    #[serde(default)]
    pub impact: Option<SemanticImpact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticConflictClassification {
    #[default]
    Unclassified,
    DivergentEdit,
    DeleteVsEdit,
    ConcurrentAddition,
    KindMismatch,
    TextFallback,
    CrossFileBreakage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticChange {
    pub kind: SemanticChangeKind,
    pub symbol: String,
    pub risk: SemanticRisk,
    #[serde(default)]
    pub impact: SemanticImpact,
    #[serde(default)]
    pub compatibility: SemanticCompatibility,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticCompatibility {
    #[serde(default)]
    pub status: SemanticCompatibilityStatus,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SemanticCompatibilityStatus {
    #[default]
    Compatible,
    PotentiallyBreaking,
    Breaking,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SymbolKind {
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

impl SymbolKind {
    fn label(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Class => "class",
            SymbolKind::Method => "method",
            SymbolKind::Constructor => "constructor",
            SymbolKind::Field => "field",
            SymbolKind::Interface => "interface",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Enum => "enum",
            SymbolKind::Export => "export",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub kind: SymbolKind,
    pub name: String,
    pub body_hash: String,
    pub match_hash: String,
    pub signature: Option<CallableSignature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallableSignature {
    pub parameters: Vec<ParameterSignature>,
    pub return_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterSignature {
    pub name: String,
    pub type_hint: Option<String>,
    pub optional: bool,
    pub has_default: bool,
    pub is_rest: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceLanguage {
    JavaScript,
    Jsx,
    TypeScript,
    Tsx,
    Python,
    Go,
    Rust,
    CSharp,
}

impl SourceLanguage {
    fn from_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("js") => Some(Self::JavaScript),
            Some("jsx") => Some(Self::Jsx),
            Some("ts") => Some(Self::TypeScript),
            Some("tsx") => Some(Self::Tsx),
            Some("py") => Some(Self::Python),
            Some("go") => Some(Self::Go),
            Some("rs") => Some(Self::Rust),
            Some("cs") => Some(Self::CSharp),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::JavaScript => "javascript",
            Self::Jsx => "jsx",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Python => "python",
            Self::Go => "go",
            Self::Rust => "rust",
            Self::CSharp => "csharp",
        }
    }
}

pub const ANALYZER_PROCESS_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct ProcessAnalyzerConfig {
    pub id: String,
    pub command: String,
    pub args: Vec<String>,
    pub extensions: Vec<String>,
}

impl ProcessAnalyzerConfig {
    pub fn new(id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args: Vec::new(),
            extensions: Vec::new(),
        }
    }

    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_extensions(
        mut self,
        extensions: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.extensions = extensions.into_iter().map(Into::into).collect();
        self
    }

    fn supports_path(&self, path: &Path) -> bool {
        if self.extensions.is_empty() {
            return true;
        }

        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        self.extensions
            .iter()
            .any(|candidate| candidate == extension)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnalyzerProcessRequest {
    Initialize {
        protocol_version: u32,
    },
    SupportsPath {
        path: String,
    },
    Diff {
        path: String,
        old_source: Option<Vec<u8>>,
        new_source: Option<Vec<u8>>,
    },
    Merge {
        path: String,
        base_source: Option<Vec<u8>>,
        left_source: Option<Vec<u8>>,
        right_source: Option<Vec<u8>>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnalyzerProcessResponse {
    Initialized {
        protocol_version: u32,
        analyzer_ids: Vec<String>,
    },
    SupportsPath {
        supported: bool,
    },
    Diff {
        diff: Option<SemanticFileDiff>,
    },
    Merge {
        merge: Option<SemanticMergeResult>,
    },
    ShutdownAck,
    Error {
        message: String,
    },
}

pub fn serve_analyzer_process(registry: AnalyzerRegistry) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());
    let mut line = String::new();
    let mut initialized = false;

    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .context("failed to read analyzer process request")?
            == 0
        {
            break;
        }

        if line.trim().is_empty() {
            continue;
        }

        let request: AnalyzerProcessRequest = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                write_process_response(
                    &mut writer,
                    &AnalyzerProcessResponse::Error {
                        message: format!("invalid request payload: {error}"),
                    },
                )?;
                continue;
            }
        };

        let mut should_shutdown = false;
        let response = match request {
            AnalyzerProcessRequest::Initialize { protocol_version } => {
                if protocol_version != ANALYZER_PROCESS_PROTOCOL_VERSION {
                    AnalyzerProcessResponse::Error {
                        message: format!(
                            "protocol mismatch: expected {}, got {}",
                            ANALYZER_PROCESS_PROTOCOL_VERSION, protocol_version
                        ),
                    }
                } else {
                    initialized = true;
                    AnalyzerProcessResponse::Initialized {
                        protocol_version,
                        analyzer_ids: registry
                            .analyzers()
                            .iter()
                            .map(|analyzer| analyzer.id().to_string())
                            .collect(),
                    }
                }
            }
            AnalyzerProcessRequest::Shutdown => {
                should_shutdown = true;
                AnalyzerProcessResponse::ShutdownAck
            }
            _ if !initialized => AnalyzerProcessResponse::Error {
                message: "analyzer process must be initialized first".to_string(),
            },
            AnalyzerProcessRequest::SupportsPath { path } => {
                AnalyzerProcessResponse::SupportsPath {
                    supported: registry.supported_source(Path::new(&path)),
                }
            }
            AnalyzerProcessRequest::Diff {
                path,
                old_source,
                new_source,
            } => match registry.diff(
                Path::new(&path),
                old_source.as_deref(),
                new_source.as_deref(),
            ) {
                Ok(diff) => AnalyzerProcessResponse::Diff { diff },
                Err(error) => AnalyzerProcessResponse::Error {
                    message: error.to_string(),
                },
            },
            AnalyzerProcessRequest::Merge {
                path,
                base_source,
                left_source,
                right_source,
            } => match registry.merge(
                Path::new(&path),
                base_source.as_deref(),
                left_source.as_deref(),
                right_source.as_deref(),
            ) {
                Ok(merge) => AnalyzerProcessResponse::Merge { merge },
                Err(error) => AnalyzerProcessResponse::Error {
                    message: error.to_string(),
                },
            },
        };

        write_process_response(&mut writer, &response)?;
        if should_shutdown {
            break;
        }
    }

    Ok(())
}

fn write_process_response(
    writer: &mut impl Write,
    response: &AnalyzerProcessResponse,
) -> Result<()> {
    serde_json::to_writer(&mut *writer, response)
        .context("failed to serialize process response")?;
    writer
        .write_all(b"\n")
        .context("failed to write process response newline")?;
    writer
        .flush()
        .context("failed to flush process response output")
}

pub trait SemanticAnalyzerPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn supports_path(&self, path: &Path) -> bool;
    fn diff(
        &self,
        path: &Path,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> Result<Option<SemanticFileDiff>>;
    fn merge(
        &self,
        path: &Path,
        base_source: Option<&[u8]>,
        left_source: Option<&[u8]>,
        right_source: Option<&[u8]>,
    ) -> Result<Option<SemanticMergeResult>>;

    fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Default, Clone)]
pub struct AnalyzerRegistry {
    analyzers: Vec<Arc<dyn SemanticAnalyzerPlugin>>,
}

impl AnalyzerRegistry {
    pub fn new() -> Self {
        Self {
            analyzers: Vec::new(),
        }
    }

    pub fn with_builtin() -> Self {
        let mut registry = Self::new();
        registry.register(TreeSitterAnalyzer::default());
        registry.register(structured::JsonAnalyzer);
        registry.register(structured::YamlTomlAnalyzer);
        registry.register(structured::XmlHtmlAnalyzer);
        registry.register(structured::CssAnalyzer);
        registry.register(structured::MarkdownAnalyzer);
        registry
    }

    pub fn register<A>(&mut self, analyzer: A)
    where
        A: SemanticAnalyzerPlugin + 'static,
    {
        self.register_arc(Arc::new(analyzer));
    }

    pub fn register_arc(&mut self, analyzer: Arc<dyn SemanticAnalyzerPlugin>) {
        self.analyzers.push(analyzer);
    }

    pub fn analyzers(&self) -> &[Arc<dyn SemanticAnalyzerPlugin>] {
        &self.analyzers
    }

    pub fn shutdown_all(&self) -> Result<()> {
        for analyzer in &self.analyzers {
            analyzer
                .shutdown()
                .with_context(|| format!("failed to shutdown analyzer `{}`", analyzer.id()))?;
        }
        Ok(())
    }

    pub fn supported_source(&self, path: &Path) -> bool {
        self.resolve(path).is_some()
    }

    pub fn diff(
        &self,
        path: &Path,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> Result<Option<SemanticFileDiff>> {
        let Some(analyzer) = self.resolve(path) else {
            return Ok(None);
        };

        analyzer.diff(path, old_source, new_source)
    }

    pub fn merge(
        &self,
        path: &Path,
        base_source: Option<&[u8]>,
        left_source: Option<&[u8]>,
        right_source: Option<&[u8]>,
    ) -> Result<Option<SemanticMergeResult>> {
        let Some(analyzer) = self.resolve(path) else {
            return Ok(None);
        };

        analyzer.merge(path, base_source, left_source, right_source)
    }

    fn resolve(&self, path: &Path) -> Option<&Arc<dyn SemanticAnalyzerPlugin>> {
        // Latest registration wins so custom analyzers can override defaults.
        self.analyzers
            .iter()
            .rev()
            .find(|analyzer| analyzer.supports_path(path))
    }
}

#[derive(Debug, Default)]
pub struct TreeSitterAnalyzer;

impl SemanticAnalyzerPlugin for TreeSitterAnalyzer {
    fn id(&self) -> &str {
        "tree-sitter-multi"
    }

    fn supports_path(&self, path: &Path) -> bool {
        SourceLanguage::from_path(path).is_some()
    }

    fn diff(
        &self,
        path: &Path,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> Result<Option<SemanticFileDiff>> {
        tree_sitter_diff(path, old_source, new_source)
    }

    fn merge(
        &self,
        path: &Path,
        base_source: Option<&[u8]>,
        left_source: Option<&[u8]>,
        right_source: Option<&[u8]>,
    ) -> Result<Option<SemanticMergeResult>> {
        tree_sitter_merge(path, base_source, left_source, right_source)
    }
}

pub struct ProcessSemanticAnalyzer {
    config: ProcessAnalyzerConfig,
    state: Mutex<ProcessAnalyzerState>,
}

impl ProcessSemanticAnalyzer {
    pub fn new(config: ProcessAnalyzerConfig) -> Self {
        Self {
            config,
            state: Mutex::new(ProcessAnalyzerState::Stopped),
        }
    }

    pub fn shutdown_process(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("process analyzer state lock poisoned"))?;
        Self::shutdown_locked(&mut state)
    }

    fn send_with_retry(&self, request: AnalyzerProcessRequest) -> Result<AnalyzerProcessResponse> {
        let mut last_error = None;
        for _ in 0..2 {
            match self.send_once(&request) {
                Ok(response) => return Ok(response),
                Err(error) => {
                    last_error = Some(error);
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("process analyzer state lock poisoned"))?;
                    Self::shutdown_locked(&mut state)?;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("process analyzer request failed")))
    }

    fn send_once(&self, request: &AnalyzerProcessRequest) -> Result<AnalyzerProcessResponse> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("process analyzer state lock poisoned"))?;

        if matches!(*state, ProcessAnalyzerState::Stopped) {
            *state = ProcessAnalyzerState::Running(AnalyzerProcessHandle::spawn(&self.config)?);
        }

        let ProcessAnalyzerState::Running(handle) = &mut *state else {
            bail!("process analyzer did not enter running state");
        };
        handle.request(request)
    }

    fn shutdown_locked(state: &mut ProcessAnalyzerState) -> Result<()> {
        let ProcessAnalyzerState::Running(handle) = state else {
            return Ok(());
        };

        let shutdown_result = handle.shutdown();
        *state = ProcessAnalyzerState::Stopped;
        shutdown_result
    }
}

impl SemanticAnalyzerPlugin for ProcessSemanticAnalyzer {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn supports_path(&self, path: &Path) -> bool {
        self.config.supports_path(path)
    }

    fn diff(
        &self,
        path: &Path,
        old_source: Option<&[u8]>,
        new_source: Option<&[u8]>,
    ) -> Result<Option<SemanticFileDiff>> {
        if !self.supports_path(path) {
            return Ok(None);
        }

        let response = self.send_with_retry(AnalyzerProcessRequest::Diff {
            path: path.to_string_lossy().to_string(),
            old_source: old_source.map(|value| value.to_vec()),
            new_source: new_source.map(|value| value.to_vec()),
        })?;

        match response {
            AnalyzerProcessResponse::Diff { diff } => Ok(diff),
            AnalyzerProcessResponse::Error { message } => {
                bail!("process analyzer `{}` diff failed: {message}", self.id())
            }
            other => bail!(
                "process analyzer `{}` returned unexpected diff response: {:?}",
                self.id(),
                other
            ),
        }
    }

    fn merge(
        &self,
        path: &Path,
        base_source: Option<&[u8]>,
        left_source: Option<&[u8]>,
        right_source: Option<&[u8]>,
    ) -> Result<Option<SemanticMergeResult>> {
        if !self.supports_path(path) {
            return Ok(None);
        }

        let response = self.send_with_retry(AnalyzerProcessRequest::Merge {
            path: path.to_string_lossy().to_string(),
            base_source: base_source.map(|value| value.to_vec()),
            left_source: left_source.map(|value| value.to_vec()),
            right_source: right_source.map(|value| value.to_vec()),
        })?;

        match response {
            AnalyzerProcessResponse::Merge { merge } => Ok(merge),
            AnalyzerProcessResponse::Error { message } => {
                bail!("process analyzer `{}` merge failed: {message}", self.id())
            }
            other => bail!(
                "process analyzer `{}` returned unexpected merge response: {:?}",
                self.id(),
                other
            ),
        }
    }

    fn shutdown(&self) -> Result<()> {
        self.shutdown_process()
    }
}

enum ProcessAnalyzerState {
    Stopped,
    Running(AnalyzerProcessHandle),
}

struct AnalyzerProcessHandle {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl AnalyzerProcessHandle {
    fn spawn(config: &ProcessAnalyzerConfig) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn analyzer process `{}`", config.command))?;
        let stdin = child
            .stdin
            .take()
            .context("analyzer process stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("analyzer process stdout was not piped")?;
        let mut handle = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };

        let response = handle.request(&AnalyzerProcessRequest::Initialize {
            protocol_version: ANALYZER_PROCESS_PROTOCOL_VERSION,
        })?;
        match response {
            AnalyzerProcessResponse::Initialized {
                protocol_version, ..
            } if protocol_version == ANALYZER_PROCESS_PROTOCOL_VERSION => Ok(handle),
            AnalyzerProcessResponse::Initialized {
                protocol_version, ..
            } => bail!(
                "analyzer process protocol mismatch: expected {}, got {}",
                ANALYZER_PROCESS_PROTOCOL_VERSION,
                protocol_version
            ),
            AnalyzerProcessResponse::Error { message } => {
                bail!("analyzer process initialization failed: {message}")
            }
            other => bail!(
                "analyzer process returned unexpected initialization response: {:?}",
                other
            ),
        }
    }

    fn request(&mut self, request: &AnalyzerProcessRequest) -> Result<AnalyzerProcessResponse> {
        serde_json::to_writer(&mut self.stdin, request)
            .context("failed to write analyzer process request")?;
        self.stdin
            .write_all(b"\n")
            .context("failed to write analyzer process request newline")?;
        self.stdin
            .flush()
            .context("failed to flush analyzer process request")?;

        let mut line = String::new();
        if self
            .stdout
            .read_line(&mut line)
            .context("failed to read analyzer process response")?
            == 0
        {
            let status = self
                .child
                .try_wait()
                .context("failed to read analyzer process status")?
                .map(|value| value.to_string())
                .unwrap_or_else(|| "running".to_string());
            bail!("analyzer process closed stdout unexpectedly (status: {status})");
        }

        serde_json::from_str(&line).context("failed to parse analyzer process response")
    }

    fn shutdown(&mut self) -> Result<()> {
        let _ = self.request(&AnalyzerProcessRequest::Shutdown);
        self.terminate()
    }

    fn terminate(&mut self) -> Result<()> {
        if self
            .child
            .try_wait()
            .context("failed to query analyzer process state")?
            .is_none()
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        Ok(())
    }
}

impl Drop for AnalyzerProcessHandle {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[derive(Debug, Clone)]
pub struct FallbackTextAnalyzer {
    id: String,
    language: String,
    extensions: BTreeSet<String>,
}

impl FallbackTextAnalyzer {
    pub fn new(
        id: impl Into<String>,
        language: impl Into<String>,
        extensions: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            id: id.into(),
            language: language.into(),
            extensions: extensions.into_iter().map(Into::into).collect(),
        }
    }
}

impl SemanticAnalyzerPlugin for FallbackTextAnalyzer {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports_path(&self, path: &Path) -> bool {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        self.extensions.contains(extension)
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

        Ok(Some(SemanticFileDiff {
            path: path.to_string_lossy().to_string(),
            language: self.language.clone(),
            parse_fallback: true,
            changes: vec![SemanticChange {
                kind: SemanticChangeKind::StyleOnly,
                symbol: "(fallback text change)".to_string(),
                risk: SemanticRisk::Medium,
                impact: SemanticImpact::default(),
                compatibility: SemanticCompatibility::default(),
            }],
        }))
    }

    fn merge(
        &self,
        path: &Path,
        base_source: Option<&[u8]>,
        left_source: Option<&[u8]>,
        right_source: Option<&[u8]>,
    ) -> Result<Option<SemanticMergeResult>> {
        let base_source = base_source.unwrap_or_default();
        let left_source = left_source.unwrap_or_default();
        let right_source = right_source.unwrap_or_default();
        let (merged_source, has_conflicts) = text_merge(base_source, left_source, right_source);
        let conflicts = if has_conflicts {
            vec![SemanticMergeConflict {
                symbol: "(fallback text merge)".to_string(),
                classification: SemanticConflictClassification::TextFallback,
                explanation: "fallback text merge produced unresolved conflicts".to_string(),
                affected_files: Vec::new(),
                impact: None,
            }]
        } else {
            Vec::new()
        };

        Ok(Some(SemanticMergeResult {
            path: path.to_string_lossy().to_string(),
            language: self.language.clone(),
            parse_fallback: true,
            merged_source,
            conflicts,
        }))
    }
}

static DEFAULT_ANALYZER_REGISTRY: OnceLock<AnalyzerRegistry> = OnceLock::new();

pub fn default_analyzer_registry() -> &'static AnalyzerRegistry {
    DEFAULT_ANALYZER_REGISTRY.get_or_init(AnalyzerRegistry::with_builtin)
}

pub fn supported_source(path: &Path) -> bool {
    default_analyzer_registry().supported_source(path)
}

pub fn diff(
    path: &Path,
    old_source: Option<&[u8]>,
    new_source: Option<&[u8]>,
) -> Result<Option<SemanticFileDiff>> {
    default_analyzer_registry().diff(path, old_source, new_source)
}

pub fn merge(
    path: &Path,
    base_source: Option<&[u8]>,
    left_source: Option<&[u8]>,
    right_source: Option<&[u8]>,
) -> Result<Option<SemanticMergeResult>> {
    default_analyzer_registry().merge(path, base_source, left_source, right_source)
}

fn tree_sitter_diff(
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
                    let compatibility = check_symbol_compatibility(old, new);
                    changes.push(SemanticChange {
                        kind: SemanticChangeKind::Modified,
                        symbol: key.clone(),
                        risk: score_modified_risk(&key, compatibility.status),
                        impact: base_impact(&key),
                        compatibility,
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
                    compatibility: SemanticCompatibility::default(),
                });
            }
        }

        for key in removed_candidates.difference(&consumed_removed) {
            changes.push(SemanticChange {
                kind: SemanticChangeKind::Removed,
                symbol: key.clone(),
                risk: score_risk(SemanticChangeKind::Removed, key),
                impact: base_impact(key),
                compatibility: SemanticCompatibility::default(),
            });
        }

        for key in added_candidates.difference(&consumed_added) {
            changes.push(SemanticChange {
                kind: SemanticChangeKind::Added,
                symbol: key.clone(),
                risk: score_risk(SemanticChangeKind::Added, key),
                impact: base_impact(key),
                compatibility: SemanticCompatibility::default(),
            });
        }

        if changes.is_empty() {
            changes.push(SemanticChange {
                kind: SemanticChangeKind::StyleOnly,
                symbol: "(style-only change)".to_string(),
                risk: SemanticRisk::Low,
                impact: base_impact("(style-only change)"),
                compatibility: SemanticCompatibility::default(),
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
            compatibility: SemanticCompatibility::default(),
        }],
    }))
}

fn tree_sitter_merge(
    path: &Path,
    base_source: Option<&[u8]>,
    left_source: Option<&[u8]>,
    right_source: Option<&[u8]>,
) -> Result<Option<SemanticMergeResult>> {
    let Some(language) = SourceLanguage::from_path(path) else {
        return Ok(None);
    };

    let base_source = base_source.unwrap_or_default();
    let left_source = left_source.unwrap_or_default();
    let right_source = right_source.unwrap_or_default();

    if left_source == right_source {
        return Ok(Some(SemanticMergeResult {
            path: path.to_string_lossy().to_string(),
            language: language.name().to_string(),
            parse_fallback: false,
            merged_source: String::from_utf8_lossy(left_source).to_string(),
            conflicts: Vec::new(),
        }));
    }

    if left_source == base_source {
        return Ok(Some(SemanticMergeResult {
            path: path.to_string_lossy().to_string(),
            language: language.name().to_string(),
            parse_fallback: false,
            merged_source: String::from_utf8_lossy(right_source).to_string(),
            conflicts: Vec::new(),
        }));
    }

    if right_source == base_source {
        return Ok(Some(SemanticMergeResult {
            path: path.to_string_lossy().to_string(),
            language: language.name().to_string(),
            parse_fallback: false,
            merged_source: String::from_utf8_lossy(left_source).to_string(),
            conflicts: Vec::new(),
        }));
    }

    let left_diff = tree_sitter_diff(path, Some(base_source), Some(left_source))?;
    let right_diff = tree_sitter_diff(path, Some(base_source), Some(right_source))?;
    let parse_fallback =
        parse_fallback_used(left_diff.as_ref()) || parse_fallback_used(right_diff.as_ref());

    if parse_fallback {
        let (merged_source, has_text_conflict) = text_merge(base_source, left_source, right_source);
        let mut conflicts = Vec::new();
        if has_text_conflict {
            conflicts.push(SemanticMergeConflict {
                symbol: "(text-merge)".to_string(),
                classification: SemanticConflictClassification::TextFallback,
                explanation: "text fallback produced conflict markers".to_string(),
                affected_files: Vec::new(),
                impact: None,
            });
        }

        return Ok(Some(SemanticMergeResult {
            path: path.to_string_lossy().to_string(),
            language: language.name().to_string(),
            parse_fallback: true,
            merged_source,
            conflicts,
        }));
    }

    let base_map = to_symbol_map(parse_symbols(language, base_source)?);
    let left_map = to_symbol_map(parse_symbols(language, left_source)?);
    let right_map = to_symbol_map(parse_symbols(language, right_source)?);
    let mut conflicts = detect_symbol_conflicts(&base_map, &left_map, &right_map);

    let (mut merged_source, has_text_conflict) = text_merge(base_source, left_source, right_source);
    if has_text_conflict {
        if is_style_only(left_diff.as_ref()) && !is_style_only(right_diff.as_ref()) {
            merged_source = String::from_utf8_lossy(right_source).to_string();
        } else if is_style_only(right_diff.as_ref()) && !is_style_only(left_diff.as_ref()) {
            merged_source = String::from_utf8_lossy(left_source).to_string();
        } else {
            conflicts.push(SemanticMergeConflict {
                symbol: "(text-merge)".to_string(),
                classification: SemanticConflictClassification::TextFallback,
                explanation: "text merge produced unresolved conflicts".to_string(),
                affected_files: Vec::new(),
                impact: None,
            });
        }
    }

    Ok(Some(SemanticMergeResult {
        path: path.to_string_lossy().to_string(),
        language: language.name().to_string(),
        parse_fallback: false,
        merged_source,
        conflicts,
    }))
}

fn parse_fallback_used(diff: Option<&SemanticFileDiff>) -> bool {
    diff.map(|value| value.parse_fallback).unwrap_or(false)
}

fn is_style_only(diff: Option<&SemanticFileDiff>) -> bool {
    let Some(diff) = diff else {
        return false;
    };

    !diff.parse_fallback
        && !diff.changes.is_empty()
        && diff
            .changes
            .iter()
            .all(|change| change.kind == SemanticChangeKind::StyleOnly)
}

fn text_merge(base_source: &[u8], left_source: &[u8], right_source: &[u8]) -> (String, bool) {
    match diffy::merge_bytes(base_source, left_source, right_source) {
        Ok(merged) => (String::from_utf8_lossy(&merged).to_string(), false),
        Err(conflicted) => (String::from_utf8_lossy(&conflicted).to_string(), true),
    }
}

fn detect_symbol_conflicts(
    base_map: &BTreeMap<String, SymbolInfo>,
    left_map: &BTreeMap<String, SymbolInfo>,
    right_map: &BTreeMap<String, SymbolInfo>,
) -> Vec<SemanticMergeConflict> {
    let keys: BTreeSet<String> = base_map
        .keys()
        .chain(left_map.keys())
        .chain(right_map.keys())
        .cloned()
        .collect();

    let mut conflicts = Vec::new();
    for key in keys {
        let base_state = base_map
            .get(&key)
            .map(|symbol| (symbol.kind, symbol.body_hash.as_str()));
        let left_state = left_map
            .get(&key)
            .map(|symbol| (symbol.kind, symbol.body_hash.as_str()));
        let right_state = right_map
            .get(&key)
            .map(|symbol| (symbol.kind, symbol.body_hash.as_str()));

        let left_changed = left_state != base_state;
        let right_changed = right_state != base_state;
        if left_changed && right_changed && left_state != right_state {
            conflicts.push(build_symbol_conflict(
                &key,
                base_state,
                left_state,
                right_state,
            ));
        }
    }

    conflicts
}

fn build_symbol_conflict(
    key: &str,
    base_state: Option<(SymbolKind, &str)>,
    left_state: Option<(SymbolKind, &str)>,
    right_state: Option<(SymbolKind, &str)>,
) -> SemanticMergeConflict {
    let left_change = describe_symbol_change(base_state, left_state);
    let right_change = describe_symbol_change(base_state, right_state);

    let (classification, explanation) = if base_state.is_none()
        && left_state.is_some()
        && right_state.is_some()
    {
        (
            SemanticConflictClassification::ConcurrentAddition,
            format!(
                "both branches introduced `{}` differently (left: {}; right: {}).",
                key, left_change, right_change
            ),
        )
    } else if left_state.is_none() || right_state.is_none() {
        (
            SemanticConflictClassification::DeleteVsEdit,
            format!(
                "one branch removed `{}` while the other changed it (left: {}; right: {}).",
                key, left_change, right_change
            ),
        )
    } else if let (Some((left_kind, _)), Some((right_kind, _))) = (left_state, right_state) {
        if left_kind != right_kind {
            (
                SemanticConflictClassification::KindMismatch,
                format!(
                    "branches changed `{}` into different semantic kinds (left: {}; right: {}).",
                    key, left_change, right_change
                ),
            )
        } else {
            (
                SemanticConflictClassification::DivergentEdit,
                format!(
                    "both branches changed `{}` in different ways (left: {}; right: {}).",
                    key, left_change, right_change
                ),
            )
        }
    } else {
        (
            SemanticConflictClassification::DivergentEdit,
            format!(
                "both branches changed `{}` in different ways (left: {}; right: {}).",
                key, left_change, right_change
            ),
        )
    };

    SemanticMergeConflict {
        symbol: key.to_string(),
        classification,
        explanation,
        affected_files: Vec::new(),
        impact: None,
    }
}

fn describe_symbol_change(
    base_state: Option<(SymbolKind, &str)>,
    side_state: Option<(SymbolKind, &str)>,
) -> String {
    match (base_state, side_state) {
        (None, None) => "unchanged (absent)".to_string(),
        (None, Some((kind, _))) => format!("added {}", kind.label()),
        (Some((_base_kind, _)), None) => "removed symbol".to_string(),
        (Some((base_kind, base_hash)), Some((side_kind, side_hash))) => {
            if base_kind != side_kind {
                format!(
                    "changed kind {} -> {}",
                    base_kind.label(),
                    side_kind.label()
                )
            } else if base_hash != side_hash {
                format!("modified {}", side_kind.label())
            } else {
                format!("kept {}", side_kind.label())
            }
        }
    }
}

fn base_impact(symbol: &str) -> SemanticImpact {
    SemanticImpact {
        symbols: impact_symbols(symbol),
        files: Vec::new(),
        modules: Vec::new(),
    }
}

pub fn impact_symbols(symbol: &str) -> Vec<String> {
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

fn score_modified_risk(symbol: &str, compatibility: SemanticCompatibilityStatus) -> SemanticRisk {
    let baseline = score_risk(SemanticChangeKind::Modified, symbol);
    match compatibility {
        SemanticCompatibilityStatus::Breaking => SemanticRisk::High,
        SemanticCompatibilityStatus::PotentiallyBreaking => {
            std::cmp::max(baseline, SemanticRisk::Medium)
        }
        SemanticCompatibilityStatus::Compatible => baseline,
    }
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

fn check_symbol_compatibility(old: &SymbolInfo, new: &SymbolInfo) -> SemanticCompatibility {
    let (Some(old_signature), Some(new_signature)) = (&old.signature, &new.signature) else {
        return SemanticCompatibility::default();
    };

    check_callable_compatibility(old_signature, new_signature)
}

fn check_callable_compatibility(
    old_signature: &CallableSignature,
    new_signature: &CallableSignature,
) -> SemanticCompatibility {
    if old_signature == new_signature {
        return SemanticCompatibility::default();
    }

    let mut breaking_notes = Vec::new();
    let mut potential_notes = Vec::new();

    let old_required = required_parameter_count(&old_signature.parameters);
    let new_required = required_parameter_count(&new_signature.parameters);
    if new_required > old_required {
        breaking_notes.push(format!(
            "required parameter count increased ({} -> {})",
            old_required, new_required
        ));
    }

    if new_signature.parameters.len() < old_signature.parameters.len() {
        breaking_notes.push(format!(
            "parameter count decreased ({} -> {})",
            old_signature.parameters.len(),
            new_signature.parameters.len()
        ));
    }

    let shared = std::cmp::min(
        old_signature.parameters.len(),
        new_signature.parameters.len(),
    );
    for index in 0..shared {
        let old_param = &old_signature.parameters[index];
        let new_param = &new_signature.parameters[index];

        if old_param.is_rest != new_param.is_rest {
            breaking_notes.push(format!(
                "parameter {} rest marker changed ({} -> {})",
                index + 1,
                old_param.is_rest,
                new_param.is_rest
            ));
        }

        if old_param.optional && !new_param.optional && !new_param.has_default {
            breaking_notes.push(format!("parameter {} became required", index + 1));
        }

        if old_param.type_hint != new_param.type_hint
            && old_param.type_hint.is_some()
            && new_param.type_hint.is_some()
        {
            potential_notes.push(format!(
                "parameter {} type changed ({} -> {})",
                index + 1,
                old_param.type_hint.clone().unwrap_or_default(),
                new_param.type_hint.clone().unwrap_or_default()
            ));
        }
    }

    if old_signature.return_type != new_signature.return_type
        && old_signature.return_type.is_some()
        && new_signature.return_type.is_some()
    {
        potential_notes.push(format!(
            "return type changed ({} -> {})",
            old_signature.return_type.clone().unwrap_or_default(),
            new_signature.return_type.clone().unwrap_or_default()
        ));
    }

    if !breaking_notes.is_empty() {
        breaking_notes.extend(potential_notes);
        return SemanticCompatibility {
            status: SemanticCompatibilityStatus::Breaking,
            notes: breaking_notes,
        };
    }

    if !potential_notes.is_empty() {
        return SemanticCompatibility {
            status: SemanticCompatibilityStatus::PotentiallyBreaking,
            notes: potential_notes,
        };
    }

    if is_backward_compatible_extension(old_signature, new_signature) {
        return SemanticCompatibility::default();
    }

    SemanticCompatibility {
        status: SemanticCompatibilityStatus::PotentiallyBreaking,
        notes: vec!["callable signature changed".to_string()],
    }
}

fn required_parameter_count(parameters: &[ParameterSignature]) -> usize {
    parameters
        .iter()
        .filter(|param| !param.optional && !param.has_default && !param.is_rest)
        .count()
}

fn is_backward_compatible_extension(
    old_signature: &CallableSignature,
    new_signature: &CallableSignature,
) -> bool {
    if old_signature.return_type != new_signature.return_type {
        return false;
    }

    if new_signature.parameters.len() < old_signature.parameters.len() {
        return false;
    }

    for (index, old_param) in old_signature.parameters.iter().enumerate() {
        let Some(new_param) = new_signature.parameters.get(index) else {
            return false;
        };
        if old_param != new_param {
            return false;
        }
    }

    new_signature
        .parameters
        .iter()
        .skip(old_signature.parameters.len())
        .all(|param| param.optional || param.has_default || param.is_rest)
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

fn ast_cache() -> &'static Mutex<HashMap<String, Vec<SymbolInfo>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<SymbolInfo>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn persistent_cache() -> &'static Mutex<Option<ast_cache::PersistentASTCache>> {
    static PCACHE: OnceLock<Mutex<Option<ast_cache::PersistentASTCache>>> = OnceLock::new();
    PCACHE.get_or_init(|| Mutex::new(None))
}

/// Initialize the persistent AST cache rooted at the given repo root.
/// Must be called before `parse_symbols` will use disk caching.
pub fn set_cache_root(root: &Path) {
    if let Ok(mut guard) = persistent_cache().lock() {
        *guard = Some(ast_cache::PersistentASTCache::new(root));
    }
}

/// Clear the persistent AST cache on disk.
pub fn clear_cache(root: &Path) -> Result<()> {
    ast_cache::PersistentASTCache::new(root).clear()
}

/// Return the path used for the persistent cache directory.
pub fn cache_root_for(root: &Path) -> PathBuf {
    root.join(".flock/semantic-cache")
}

fn parse_symbols(language: SourceLanguage, source: &[u8]) -> Result<Vec<SymbolInfo>> {
    if source.is_empty() {
        return Ok(Vec::new());
    }

    let cache_key = blake3::hash(source).to_hex().to_string();

    // 1. Check in-memory cache
    if let Ok(cache) = ast_cache().lock() {
        if let Some(cached) = cache.get(&cache_key) {
            return Ok(cached.clone());
        }
    }

    // 2. Check persistent disk cache
    if let Ok(guard) = persistent_cache().lock() {
        if let Some(ref pcache) = *guard {
            if let Some(entry) = pcache.get(&cache_key) {
                // Populate in-memory cache for future lookups
                if let Ok(mut mem) = ast_cache().lock() {
                    mem.insert(cache_key, entry.symbols.clone());
                }
                return Ok(entry.symbols);
            }
        }
    }

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
        SourceLanguage::Python => parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .context("failed to load Python grammar")?,
        SourceLanguage::Go => parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .context("failed to load Go grammar")?,
        SourceLanguage::Rust => parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .context("failed to load Rust grammar")?,
        SourceLanguage::CSharp => parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .context("failed to load C# grammar")?,
    };

    let tree = parser
        .parse(source, None)
        .context("tree-sitter parse returned no tree")?;

    let root = tree.root_node();
    if root.has_error() {
        bail!("tree-sitter parser reported syntax errors");
    }

    let mut symbols = Vec::new();
    match language {
        SourceLanguage::JavaScript
        | SourceLanguage::Jsx
        | SourceLanguage::TypeScript
        | SourceLanguage::Tsx => extract_symbols(root, source, None, &mut symbols)?,
        SourceLanguage::Python => extract_symbols_python(root, source, None, &mut symbols)?,
        SourceLanguage::Go => extract_symbols_go(root, source, None, &mut symbols)?,
        SourceLanguage::Rust => extract_symbols_rust(root, source, None, &mut symbols)?,
        SourceLanguage::CSharp => extract_symbols_csharp(root, source, None, &mut symbols)?,
    }

    // Store in in-memory cache
    if let Ok(mut cache) = ast_cache().lock() {
        cache.insert(cache_key.clone(), symbols.clone());
    }

    // Store in persistent disk cache
    if let Ok(guard) = persistent_cache().lock() {
        if let Some(ref pcache) = *guard {
            let _ = pcache.put(&ast_cache::CachedSymbols {
                content_hash: cache_key,
                language: language.name().to_string(),
                symbols: symbols.clone(),
            });
        }
    }

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
                    signature: extract_callable_signature(node, source),
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
                            signature: extract_callable_signature(value, source),
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
                    signature: None,
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
                        signature: extract_callable_signature(node, source),
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
                        signature: extract_callable_signature(node, source),
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
                    signature: None,
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
                    signature: None,
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
                    signature: None,
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
                    signature: None,
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
                    signature: None,
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

// --- Python symbol extraction ---

fn extract_symbols_python(
    node: Node,
    source: &[u8],
    enclosing_class: Option<&str>,
    symbols: &mut Vec<SymbolInfo>,
) -> Result<()> {
    match node.kind() {
        "function_definition" => {
            if let Some(fn_name) = symbol_name(node, source) {
                let scoped_name = match enclosing_class {
                    Some(class_name) => format!("{}.{}", class_name, fn_name),
                    None => fn_name.clone(),
                };
                let kind = if enclosing_class.is_some() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                symbols.push(SymbolInfo {
                    kind,
                    name: scoped_name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: extract_callable_signature(node, source),
                });
            }
        }
        "decorated_definition" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "function_definition" || child.kind() == "class_definition" {
                    extract_symbols_python(child, source, enclosing_class, symbols)?;
                }
            }
            return Ok(());
        }
        "class_definition" => {
            if let Some(class_name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Class,
                    name: class_name.clone(),
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: None,
                });
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "block" {
                        let mut block_cursor = child.walk();
                        for block_child in child.named_children(&mut block_cursor) {
                            extract_symbols_python(
                                block_child,
                                source,
                                Some(&class_name),
                                symbols,
                            )?;
                        }
                    }
                }
                return Ok(());
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        extract_symbols_python(child, source, enclosing_class, symbols)?;
    }
    Ok(())
}

// --- Go symbol extraction ---

fn extract_symbols_go(
    node: Node,
    source: &[u8],
    _enclosing_class: Option<&str>,
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
                    signature: extract_callable_signature(node, source),
                });
            }
        }
        "method_declaration" => {
            if let Some(fn_name) = symbol_name(node, source) {
                let receiver = node
                    .child_by_field_name("receiver")
                    .and_then(|r| {
                        let mut cursor = r.walk();
                        r.named_children(&mut cursor)
                            .next()
                            .and_then(|param| param.child_by_field_name("type"))
                            .map(|t| {
                                let text = compact_text(t, source);
                                text.trim_start_matches('*').to_string()
                            })
                    });
                let scoped_name = match receiver {
                    Some(ref recv) => format!("{}.{}", recv, fn_name),
                    None => fn_name,
                };
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Method,
                    name: scoped_name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: extract_callable_signature(node, source),
                });
            }
        }
        "type_declaration" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "type_spec" {
                    if let Some(name) = symbol_name(child, source) {
                        let type_node = child.child_by_field_name("type");
                        let kind = match type_node.map(|n| n.kind()) {
                            Some("struct_type") => SymbolKind::Class,
                            Some("interface_type") => SymbolKind::Interface,
                            _ => SymbolKind::TypeAlias,
                        };
                        symbols.push(SymbolInfo {
                            kind,
                            name,
                            body_hash: node_hash(child, source)?,
                            match_hash: node_match_hash(child, source)?,
                            signature: None,
                        });
                    }
                }
            }
            return Ok(());
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        extract_symbols_go(child, source, None, symbols)?;
    }
    Ok(())
}

// --- Rust symbol extraction ---

fn extract_symbols_rust(
    node: Node,
    source: &[u8],
    enclosing_type: Option<&str>,
    symbols: &mut Vec<SymbolInfo>,
) -> Result<()> {
    match node.kind() {
        "function_item" => {
            if let Some(fn_name) = symbol_name(node, source) {
                let scoped_name = match enclosing_type {
                    Some(type_name) => format!("{}.{}", type_name, fn_name),
                    None => fn_name,
                };
                let kind = if enclosing_type.is_some() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                symbols.push(SymbolInfo {
                    kind,
                    name: scoped_name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: extract_callable_signature(node, source),
                });
            }
        }
        "impl_item" => {
            let type_name = node
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(source).ok())
                .map(|s| s.trim().to_string());
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "declaration_list" {
                    let mut inner_cursor = child.walk();
                    for inner in child.named_children(&mut inner_cursor) {
                        extract_symbols_rust(
                            inner,
                            source,
                            type_name.as_deref(),
                            symbols,
                        )?;
                    }
                }
            }
            return Ok(());
        }
        "trait_item" => {
            if let Some(name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Interface,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: None,
                });
            }
        }
        "struct_item" => {
            if let Some(name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Class,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: None,
                });
            }
        }
        "enum_item" => {
            if let Some(name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Enum,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: None,
                });
            }
        }
        "type_item" => {
            if let Some(name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::TypeAlias,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: None,
                });
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        extract_symbols_rust(child, source, enclosing_type, symbols)?;
    }
    Ok(())
}

// --- C# symbol extraction ---

fn extract_symbols_csharp(
    node: Node,
    source: &[u8],
    enclosing_class: Option<&str>,
    symbols: &mut Vec<SymbolInfo>,
) -> Result<()> {
    match node.kind() {
        "method_declaration" => {
            if let Some(method_name) = symbol_name(node, source) {
                let scoped_name = match enclosing_class {
                    Some(class_name) => format!("{}.{}", class_name, method_name),
                    None => method_name,
                };
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Method,
                    name: scoped_name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: extract_callable_signature(node, source),
                });
            }
        }
        "class_declaration" => {
            if let Some(class_name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Class,
                    name: class_name.clone(),
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: None,
                });
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "declaration_list" {
                        let mut inner_cursor = child.walk();
                        for inner in child.named_children(&mut inner_cursor) {
                            extract_symbols_csharp(
                                inner,
                                source,
                                Some(&class_name),
                                symbols,
                            )?;
                        }
                    }
                }
                return Ok(());
            }
        }
        "interface_declaration" => {
            if let Some(name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Interface,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: None,
                });
            }
        }
        "struct_declaration" => {
            if let Some(name) = symbol_name(node, source) {
                symbols.push(SymbolInfo {
                    kind: SymbolKind::Class,
                    name,
                    body_hash: node_hash(node, source)?,
                    match_hash: node_match_hash(node, source)?,
                    signature: None,
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
                    signature: None,
                });
            }
        }
        "property_declaration" => {
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
                    signature: None,
                });
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        extract_symbols_csharp(child, source, enclosing_class, symbols)?;
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

fn extract_callable_signature(node: Node, source: &[u8]) -> Option<CallableSignature> {
    let parameters = node.child_by_field_name("parameters")?;
    let mut parsed_parameters = Vec::new();
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.kind() == "comment" {
            continue;
        }

        parsed_parameters.push(parse_parameter_signature(parameter, source));
    }

    let return_type = node
        .child_by_field_name("return_type")
        .map(|annotation| compact_text(annotation, source))
        .filter(|text| !text.is_empty());

    Some(CallableSignature {
        parameters: parsed_parameters,
        return_type,
    })
}

fn parse_parameter_signature(node: Node, source: &[u8]) -> ParameterSignature {
    let kind = node.kind();
    let is_rest = kind == "rest_parameter";
    let has_default = kind == "assignment_pattern";
    let optional =
        kind == "optional_parameter" || has_default || compact_text(node, source).contains('?');
    let type_hint = node
        .child_by_field_name("type")
        .or_else(|| node.child_by_field_name("type_annotation"))
        .map(|annotation| compact_text(annotation, source))
        .filter(|text| !text.is_empty());

    ParameterSignature {
        name: parameter_name(node, source),
        type_hint,
        optional,
        has_default,
        is_rest,
    }
}

fn parameter_name(node: Node, source: &[u8]) -> String {
    if let Some(name_node) = node.child_by_field_name("name") {
        let text = compact_text(name_node, source);
        if !text.is_empty() {
            return text;
        }
    }

    if let Some(pattern_node) = node.child_by_field_name("pattern") {
        let text = compact_text(pattern_node, source);
        if !text.is_empty() {
            return text;
        }
    }

    if let Some(left_node) = node.child_by_field_name("left") {
        let text = compact_text(left_node, source);
        if !text.is_empty() {
            return text;
        }
    }

    compact_text(node, source)
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

    #[derive(Default)]
    struct StubAnalyzer {
        extension: &'static str,
        language: &'static str,
    }

    impl SemanticAnalyzerPlugin for StubAnalyzer {
        fn id(&self) -> &str {
            self.language
        }

        fn supports_path(&self, path: &Path) -> bool {
            path.extension()
                .and_then(|value| value.to_str())
                .map(|value| value == self.extension)
                .unwrap_or(false)
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

            Ok(Some(SemanticFileDiff {
                path: path.to_string_lossy().to_string(),
                language: self.language.to_string(),
                parse_fallback: false,
                changes: vec![SemanticChange {
                    kind: SemanticChangeKind::Modified,
                    symbol: "stub:symbol".to_string(),
                    risk: SemanticRisk::Low,
                    impact: SemanticImpact::default(),
                    compatibility: SemanticCompatibility::default(),
                }],
            }))
        }

        fn merge(
            &self,
            path: &Path,
            _base_source: Option<&[u8]>,
            _left_source: Option<&[u8]>,
            _right_source: Option<&[u8]>,
        ) -> Result<Option<SemanticMergeResult>> {
            Ok(Some(SemanticMergeResult {
                path: path.to_string_lossy().to_string(),
                language: self.language.to_string(),
                parse_fallback: false,
                merged_source: "stub-merge".to_string(),
                conflicts: Vec::new(),
            }))
        }
    }

    #[test]
    fn plugin_registry_dispatches_custom_analyzer() {
        let mut registry = AnalyzerRegistry::new();
        registry.register(StubAnalyzer {
            extension: "py",
            language: "python-stub",
        });

        let diff = registry
            .diff(Path::new("main.py"), Some(b"old"), Some(b"new"))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert_eq!(diff.language, "python-stub");
        assert!(registry.supported_source(Path::new("main.py")));
        assert!(!registry.supported_source(Path::new("main.go")));
    }

    #[test]
    fn latest_plugin_registration_overrides_default_analyzer() {
        let mut registry = AnalyzerRegistry::with_builtin();
        registry.register(StubAnalyzer {
            extension: "ts",
            language: "override-ts",
        });

        let diff = registry
            .diff(
                Path::new("override.ts"),
                Some(b"function oldName() {}"),
                Some(b"function newName() {}"),
            )
            .expect("diff should succeed")
            .expect("diff should exist");

        assert_eq!(diff.language, "override-ts");
        assert!(
            diff.changes
                .iter()
                .any(|change| change.symbol == "stub:symbol")
        );
    }

    #[test]
    fn fallback_analyzer_contract_diff_for_python() {
        let analyzer = FallbackTextAnalyzer::new("python-fallback", "python", ["py"]);
        assert!(analyzer.supports_path(Path::new("main.py")));
        assert!(!analyzer.supports_path(Path::new("main.ts")));

        let unchanged = analyzer
            .diff(
                Path::new("main.py"),
                Some(b"print('hello')\n"),
                Some(b"print('hello')\n"),
            )
            .expect("diff should succeed");
        assert!(unchanged.is_none());

        let changed = analyzer
            .diff(
                Path::new("main.py"),
                Some(b"print('hello')\n"),
                Some(b"print('hi')\n"),
            )
            .expect("diff should succeed")
            .expect("diff should exist");
        assert!(changed.parse_fallback);
        assert_eq!(changed.language, "python");
        assert!(changed.changes.iter().any(|change| {
            change.kind == SemanticChangeKind::StyleOnly
                && change.symbol == "(fallback text change)"
                && change.risk == SemanticRisk::Medium
        }));
    }

    #[test]
    fn fallback_analyzer_contract_merge_for_python() {
        let analyzer = FallbackTextAnalyzer::new("python-fallback", "python", ["py"]);
        let base = b"value = 1\n";
        let left = b"value = 2\n";
        let right = b"value = 3\n";

        let merged = analyzer
            .merge(Path::new("main.py"), Some(base), Some(left), Some(right))
            .expect("merge should succeed")
            .expect("merge result should exist");
        assert!(merged.parse_fallback);
        assert_eq!(merged.language, "python");
        assert!(merged.conflicts.iter().any(|conflict| {
            conflict.classification == SemanticConflictClassification::TextFallback
                && conflict.explanation.contains("fallback text merge")
        }));
    }

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
    fn flags_added_required_parameter_as_breaking_signature_change() {
        let old = b"function calculate(amount: number): number { return amount; }";
        let new =
            b"function calculate(amount: number, currency: string): number { return amount; }";

        let diff = diff(Path::new("signature.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| {
            c.kind == SemanticChangeKind::Modified
                && c.symbol == "function:calculate"
                && c.risk == SemanticRisk::High
                && c.compatibility.status == SemanticCompatibilityStatus::Breaking
                && c.compatibility
                    .notes
                    .iter()
                    .any(|note| note.contains("required parameter count increased"))
        }));
    }

    #[test]
    fn treats_added_optional_parameter_as_compatible() {
        let old = b"function calculate(amount: number): number { return amount; }";
        let new =
            b"function calculate(amount: number, currency?: string): number { return amount; }";

        let diff = diff(Path::new("optional-signature.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| {
            c.kind == SemanticChangeKind::Modified
                && c.symbol == "function:calculate"
                && c.compatibility.status == SemanticCompatibilityStatus::Compatible
        }));
    }

    #[test]
    fn marks_return_type_change_as_potentially_breaking() {
        let old = b"function lookup(id: string): string { return id; }";
        let new = b"function lookup(id: string): number { return Number(id); }";

        let diff = diff(Path::new("return-type.ts"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| {
            c.kind == SemanticChangeKind::Modified
                && c.symbol == "function:lookup"
                && c.compatibility.status == SemanticCompatibilityStatus::PotentiallyBreaking
                && c.compatibility
                    .notes
                    .iter()
                    .any(|note| note.contains("return type changed"))
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

    #[test]
    fn semantic_merge_combines_disjoint_symbol_changes() {
        let base = b"function left() {\n  return 1;\n}\n\nfunction right() {\n  return 1;\n}\n";
        let left = b"function left() {\n  return 2;\n}\n\nfunction right() {\n  return 1;\n}\n";
        let right = b"function left() {\n  return 1;\n}\n\nfunction right() {\n  return 3;\n}\n";

        let merged = merge(Path::new("merge.ts"), Some(base), Some(left), Some(right))
            .expect("merge should succeed")
            .expect("merge result should exist");

        assert!(!merged.parse_fallback);
        assert!(merged.conflicts.is_empty());
        assert!(merged.merged_source.contains("return 2;"));
        assert!(merged.merged_source.contains("return 3;"));
    }

    #[test]
    fn semantic_merge_flags_divergent_edits_to_same_symbol() {
        let base = b"function calculate(amount: number): number { return amount; }\n";
        let left =
            b"function calculate(amount: number, currency: string): number { return amount; }\n";
        let right = b"function calculate(amount: number): number { return amount * 2; }\n";

        let merged = merge(
            Path::new("conflict.ts"),
            Some(base),
            Some(left),
            Some(right),
        )
        .expect("merge should succeed")
        .expect("merge result should exist");

        assert!(!merged.conflicts.is_empty());
        assert!(merged.conflicts.iter().any(|conflict| {
            conflict.symbol == "function:calculate"
                && conflict.classification == SemanticConflictClassification::DivergentEdit
                && conflict.explanation.contains("both branches changed")
        }));
    }

    #[test]
    fn semantic_merge_classifies_delete_vs_edit_conflict() {
        let base = b"function calculate(amount: number): number { return amount; }\n";
        let left = b"";
        let right = b"function calculate(amount: number): number { return amount * 2; }\n";

        let merged = merge(
            Path::new("delete-vs-edit.ts"),
            Some(base),
            Some(left),
            Some(right),
        )
        .expect("merge should succeed")
        .expect("merge result should exist");

        assert!(merged.conflicts.iter().any(|conflict| {
            conflict.symbol == "function:calculate"
                && conflict.classification == SemanticConflictClassification::DeleteVsEdit
                && conflict.explanation.contains("removed")
        }));
    }

    #[test]
    fn semantic_merge_classifies_concurrent_addition_conflict() {
        let base = b"";
        let left = b"const fn = (amount: number) => amount + 1;\n";
        let right = b"const fn = (amount: number) => amount + 2;\n";

        let merged = merge(
            Path::new("concurrent-add.ts"),
            Some(base),
            Some(left),
            Some(right),
        )
        .expect("merge should succeed")
        .expect("merge result should exist");

        assert!(merged.conflicts.iter().any(|conflict| {
            conflict.symbol == "function:fn"
                && conflict.classification == SemanticConflictClassification::ConcurrentAddition
                && conflict.explanation.contains("introduced")
        }));
    }

    #[test]
    fn semantic_merge_uses_text_fallback_when_parse_fails() {
        let base = b"function add(a, b) { return a + b; }\n";
        let left = b"function add(a, b) {\n";
        let right = b"function add(a, b) { return a - b; }\n";

        let merged = merge(
            Path::new("fallback.ts"),
            Some(base),
            Some(left),
            Some(right),
        )
        .expect("merge should succeed")
        .expect("merge result should exist");

        assert!(merged.parse_fallback);
        assert!(!merged.merged_source.is_empty());
        assert!(merged.conflicts.iter().any(|conflict| {
            conflict.classification == SemanticConflictClassification::TextFallback
                && conflict.explanation.contains("text fallback")
        }));
    }

    #[test]
    fn semantic_merge_auto_resolves_style_only_vs_logic() {
        // Base has a function
        let base = b"function add(a, b) { return a + b; }\n";
        // Left side only adds whitespace/comments (style-only)
        let left = b"function add(a, b) { return a + b; }\n\n";
        // Right side makes a logic change
        let right = b"function add(a, b) { return a - b; }\n";

        let merged = merge(
            Path::new("resolve.ts"),
            Some(base),
            Some(left),
            Some(right),
        )
        .expect("merge should succeed")
        .expect("merge result should exist");

        // Should auto-pick the logic change (right side) since left is style-only
        assert_eq!(
            merged.merged_source,
            String::from_utf8_lossy(right).to_string()
        );
        // Should have no text-merge conflict since it was auto-resolved
        assert!(
            !merged
                .conflicts
                .iter()
                .any(|c| c.classification == SemanticConflictClassification::TextFallback)
        );
    }

    #[test]
    fn semantic_merge_auto_resolves_logic_vs_style_only() {
        let base = b"function add(a, b) { return a + b; }\n";
        // Left side makes a logic change
        let left = b"function add(a, b) { return a * b; }\n";
        // Right side only adds whitespace (style-only)
        let right = b"function add(a, b) { return a + b; }\n\n";

        let merged = merge(
            Path::new("resolve2.ts"),
            Some(base),
            Some(left),
            Some(right),
        )
        .expect("merge should succeed")
        .expect("merge result should exist");

        // Should auto-pick the logic change (left side)
        assert_eq!(
            merged.merged_source,
            String::from_utf8_lossy(left).to_string()
        );
        assert!(
            !merged
                .conflicts
                .iter()
                .any(|c| c.classification == SemanticConflictClassification::TextFallback)
        );
    }

    // --- Python analyzer tests ---

    #[test]
    fn detects_python_function_and_class() {
        let old = b"def greet(name):\n    return f'hello {name}'\n";
        let new = b"def greet(name):\n    return f'hi {name}'\n\nclass User:\n    def __init__(self, name):\n        self.name = name\n";

        let diff = diff(Path::new("main.py"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert_eq!(diff.language, "python");
        assert!(!diff.parse_fallback);
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Modified && c.symbol == "function:greet"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "class:User"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "method:User.__init__"));
    }

    #[test]
    fn detects_python_decorated_function() {
        let old = b"";
        let new = b"import functools\n\n@functools.lru_cache\ndef expensive(n):\n    return n * 2\n";

        let diff = diff(Path::new("cached.py"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "function:expensive"));
    }

    // --- Go analyzer tests ---

    #[test]
    fn detects_go_function_and_struct() {
        let old = b"package main\n\nfunc Add(a, b int) int {\n\treturn a + b\n}\n";
        let new = b"package main\n\nfunc Add(a, b int) int {\n\treturn a - b\n}\n\ntype User struct {\n\tName string\n}\n\nfunc (u *User) Greet() string {\n\treturn u.Name\n}\n";

        let diff = diff(Path::new("main.go"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert_eq!(diff.language, "go");
        assert!(!diff.parse_fallback);
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Modified && c.symbol == "function:Add"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "class:User"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "method:User.Greet"));
    }

    #[test]
    fn detects_go_interface() {
        let old = b"package main\n";
        let new = b"package main\n\ntype Reader interface {\n\tRead(p []byte) (n int, err error)\n}\n";

        let diff = diff(Path::new("iface.go"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "interface:Reader"));
    }

    // --- Rust analyzer tests ---

    #[test]
    fn detects_rust_function_and_struct() {
        let old_src = b"fn add(a: i32, b: i32) -> i32 { a + b }";
        let new_src = b"fn add(a: i32, b: i32) -> i32 { a - b }
struct User {
    name: String,
}
impl User {
    fn greet(&self) -> String {
        self.name.clone()
    }
}";

        let diff = diff(Path::new("lib.rs"), Some(old_src), Some(new_src))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert_eq!(diff.language, "rust");
        assert!(!diff.parse_fallback);
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Modified && c.symbol == "function:add"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "class:User"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "method:User.greet"));
    }

    #[test]
    fn detects_rust_trait_and_enum() {
        let old = b"";
        let new = b"trait Drawable {
    fn draw(&self);
}
enum Color {
    Red,
    Green,
    Blue,
}
type Coordinate = (f64, f64);";

        let diff = diff(Path::new("types.rs"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "interface:Drawable"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "enum:Color"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "type:Coordinate"));
    }

    // --- C# analyzer tests ---

    #[test]
    fn detects_csharp_class_and_method() {
        let old = b"";
        let new = b"class User {\n    public string Name { get; set; }\n    public string Greet() {\n        return Name;\n    }\n}\n";

        let diff = diff(Path::new("User.cs"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert_eq!(diff.language, "csharp");
        assert!(!diff.parse_fallback);
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "class:User"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "method:User.Greet"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "field:User.Name"));
    }

    #[test]
    fn detects_csharp_interface_and_enum() {
        let old = b"";
        let new = b"interface IDrawable {\n    void Draw();\n}\n\nenum Color {\n    Red,\n    Green,\n    Blue\n}\n\nstruct Point {\n    public int X { get; set; }\n    public int Y { get; set; }\n}\n";

        let diff = diff(Path::new("Types.cs"), Some(old), Some(new))
            .expect("diff should succeed")
            .expect("diff should exist");

        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "interface:IDrawable"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "enum:Color"));
        assert!(diff.changes.iter().any(|c| c.kind == SemanticChangeKind::Added && c.symbol == "class:Point"));
    }

    // --- AST cache tests ---

    #[test]
    fn ast_cache_returns_consistent_results() {
        let source = b"function cached() { return 1; }";
        let symbols1 = parse_symbols(SourceLanguage::JavaScript, source)
            .expect("first parse should succeed");
        let symbols2 = parse_symbols(SourceLanguage::JavaScript, source)
            .expect("second parse should succeed");

        assert_eq!(symbols1.len(), symbols2.len());
        for (a, b) in symbols1.iter().zip(symbols2.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.body_hash, b.body_hash);
        }
    }
}
