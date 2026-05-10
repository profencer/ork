//! `ork lint` — static checks against the user project.
//!
//! ADR-0057 ships this verb as a clap-visible **stub**. The full
//! rule set (id collisions, missing references, missing eval datasets,
//! migration ordering, `--fix` for trivial issues) is sized for its
//! own ADR; opening that work without first agreeing on the rule
//! taxonomy would create rule churn before any users depend on the
//! checks. Run today returns exit 2.

use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct LintArgs {
    /// Patch trivial issues (sort migrations, deduplicate imports). v1: no-op.
    #[arg(long)]
    pub fix: bool,
}

pub fn run(_args: LintArgs) -> Result<()> {
    eprintln!(
        "ork lint: planned for a follow-up ADR. The v1 of ADR-0057 lands the verb shape so \
         scripts can rely on its presence; the rule set (id collisions, missing references, \
         missing datasets, migration ordering, `--fix`) is sized for its own ADR. Exiting 2."
    );
    std::process::exit(2);
}
