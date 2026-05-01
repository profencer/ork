//! YAML template desugaring smoke (ADR-0050).

use std::path::PathBuf;

use ork_core::ports::workflow_def::WorkflowDef;

use ork_workflow::Workflow;

#[test]
fn load_all_repo_workflow_templates() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../workflow-templates");
    let mut n = 0usize;
    for e in std::fs::read_dir(&root).unwrap_or_else(|e| panic!("read_dir({root:?}): {e}")) {
        let e = e.expect("dir entry");
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
            continue;
        }
        let w = Workflow::from_template_path(&path).unwrap_or_else(|err| {
            panic!("from_template_path({path:?}): {err:?}");
        });
        assert!(!w.id().is_empty(), "{path:?}");
        n += 1;
    }
    assert!(n >= 5, "expected workflow-templates/*.yaml");
}
