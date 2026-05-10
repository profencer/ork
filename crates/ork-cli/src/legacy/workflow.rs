//! `ork legacy workflow` — workflow file utilities.
//! ADR-0057 §`Legacy subcommands`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use ork_core::models::workflow::WorkflowStep;

use super::change_plan::WorkflowYaml;

#[derive(Subcommand)]
pub enum WorkflowCmd {
    /// Migrate legacy step tools into prompt hints for ADR-0011.
    MigrateTools {
        /// Workflow YAML file, or directory to scan recursively for *.yaml/*.yml.
        path: PathBuf,
        /// Rewrite files in place instead of printing a diff.
        #[arg(long)]
        in_place: bool,
    },
}

pub fn run(cmd: WorkflowCmd) -> Result<()> {
    match cmd {
        WorkflowCmd::MigrateTools { path, in_place } => migrate_tools(path, in_place),
    }
}

const TOOL_MIGRATION_MARKER: &str = "Use the following tools as needed:";

fn migrate_step(step: &mut WorkflowStep) -> bool {
    if step.tools.is_empty() || step.prompt_template.contains(TOOL_MIGRATION_MARKER) {
        return false;
    }
    let hint = format!("{TOOL_MIGRATION_MARKER} {}.\n\n", step.tools.join(", "));
    step.prompt_template = format!("{hint}{}", step.prompt_template);
    true
}

fn collect_yaml_files(path: &std::path::Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    if !path.is_dir() {
        bail!("{} is neither a file nor a directory", path.display());
    }
    for entry in std::fs::read_dir(path).with_context(|| format!("read dir {}", path.display()))? {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            collect_yaml_files(&child, out)?;
        } else if matches!(
            child.extension().and_then(|e| e.to_str()),
            Some("yaml" | "yml")
        ) {
            out.push(child);
        }
    }
    Ok(())
}

fn simple_unified_diff(path: &std::path::Path, before: &str, after: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("--- {}\n", path.display()));
    out.push_str(&format!("+++ {}\n", path.display()));
    out.push_str("@@\n");
    for line in before.lines() {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in after.lines() {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn migrate_tools(path: PathBuf, in_place: bool) -> Result<()> {
    let mut files = Vec::new();
    collect_yaml_files(&path, &mut files)?;
    files.sort();

    let mut migrated_steps = 0usize;
    let mut changed_files = 0usize;
    for file in files {
        let before = std::fs::read_to_string(&file)
            .with_context(|| format!("read workflow file {}", file.display()))?;
        let mut wf: WorkflowYaml = serde_yaml::from_str(&before)
            .with_context(|| format!("parse YAML {}", file.display()))?;
        let mut changed_steps = 0usize;
        for step in &mut wf.steps {
            if migrate_step(step) {
                changed_steps += 1;
            }
        }
        if changed_steps == 0 {
            continue;
        }
        migrated_steps += changed_steps;
        changed_files += 1;
        let after = serde_yaml::to_string(&wf)
            .with_context(|| format!("serialize YAML {}", file.display()))?;
        if in_place {
            std::fs::write(&file, after)
                .with_context(|| format!("write workflow file {}", file.display()))?;
        } else {
            print!("{}", simple_unified_diff(&file, &before, &after));
        }
    }
    eprintln!("migrated {migrated_steps} step(s) across {changed_files} file(s)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(tools: Vec<&str>, prompt: &str) -> WorkflowStep {
        WorkflowStep {
            id: "s".into(),
            agent: "writer".into(),
            tools: tools.into_iter().map(String::from).collect(),
            prompt_template: prompt.into(),
            provider: None,
            model: None,
            depends_on: Vec::new(),
            condition: None,
            for_each: None,
            iteration_var: None,
            delegate_to: None,
        }
    }

    #[test]
    fn migrate_step_prepends_tool_hint() {
        let mut step = step(vec!["read_file", "code_search"], "Do work.");
        assert!(migrate_step(&mut step));
        assert_eq!(
            step.prompt_template,
            "Use the following tools as needed: read_file, code_search.\n\nDo work."
        );
    }

    #[test]
    fn migrate_step_is_idempotent() {
        let mut step = step(
            vec!["read_file"],
            "Use the following tools as needed: read_file.\n\nDo work.",
        );
        assert!(!migrate_step(&mut step));
    }

    #[test]
    fn migrate_step_skips_empty_tool_list() {
        let mut step = step(vec![], "Do work.");
        assert!(!migrate_step(&mut step));
        assert_eq!(step.prompt_template, "Do work.");
    }

    #[test]
    fn migrate_change_plan_template_diff_contains_tool_hints() {
        let before = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../workflow-templates/change-plan.yaml"
        ))
        .unwrap();
        let mut wf: WorkflowYaml = serde_yaml::from_str(&before).unwrap();
        let mut changed = 0usize;
        for step in &mut wf.steps {
            if migrate_step(step) {
                changed += 1;
            }
        }
        let after = serde_yaml::to_string(&wf).unwrap();
        let diff = simple_unified_diff(
            std::path::Path::new("workflow-templates/change-plan.yaml"),
            &before,
            &after,
        );
        assert_eq!(changed, 2);
        assert!(diff.contains("Use the following tools as needed: list_repos."));
        assert!(
            diff.contains("Use the following tools as needed: code_search, read_file, list_tree.")
        );
    }
}
