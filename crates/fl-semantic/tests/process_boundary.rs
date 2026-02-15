use std::path::Path;

use fl_semantic::{
    AnalyzerRegistry, ProcessAnalyzerConfig, ProcessSemanticAnalyzer, SemanticChangeKind,
    SemanticConflictClassification,
};

fn process_registry() -> AnalyzerRegistry {
    let mut registry = AnalyzerRegistry::new();
    registry.register(ProcessSemanticAnalyzer::new(
        ProcessAnalyzerConfig::new("python-process", env!("CARGO_BIN_EXE_fl_semantic_analyzer"))
            .with_extensions(["py"]),
    ));
    registry
}

#[test]
fn process_boundary_diff_and_merge_contract() {
    let registry = process_registry();

    let diff = registry
        .diff(
            Path::new("example.py"),
            Some(b"print('hello')\n"),
            Some(b"print('hi')\n"),
        )
        .expect("diff should succeed")
        .expect("diff should exist");
    assert_eq!(diff.language, "python");
    assert!(diff.parse_fallback);
    assert!(
        diff.changes
            .iter()
            .any(|change| change.kind == SemanticChangeKind::StyleOnly)
    );

    let merge = registry
        .merge(
            Path::new("example.py"),
            Some(b"value = 1\n"),
            Some(b"value = 2\n"),
            Some(b"value = 3\n"),
        )
        .expect("merge should succeed")
        .expect("merge should exist");
    assert!(merge.parse_fallback);
    assert!(merge.conflicts.iter().any(|conflict| {
        conflict.classification == SemanticConflictClassification::TextFallback
    }));
}

#[test]
fn process_boundary_lifecycle_restart_after_shutdown() {
    let registry = process_registry();

    let first_diff = registry
        .diff(Path::new("service.py"), Some(b"x = 1\n"), Some(b"x = 2\n"))
        .expect("initial diff should succeed");
    assert!(first_diff.is_some());

    registry
        .shutdown_all()
        .expect("shutdown should stop analyzer process");

    let second_diff = registry
        .diff(Path::new("service.py"), Some(b"x = 2\n"), Some(b"x = 3\n"))
        .expect("diff should restart analyzer process");
    assert!(second_diff.is_some());

    registry
        .shutdown_all()
        .expect("shutdown should be idempotent");
}
