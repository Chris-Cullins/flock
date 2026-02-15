use anyhow::Result;
use fl_semantic::{AnalyzerRegistry, FallbackTextAnalyzer, serve_analyzer_process};

fn main() -> Result<()> {
    let mut registry = AnalyzerRegistry::new();
    registry.register(FallbackTextAnalyzer::new(
        "python-fallback",
        "python",
        ["py"],
    ));
    serve_analyzer_process(registry)
}
