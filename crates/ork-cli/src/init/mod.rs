//! `ork init <name>` — scaffold a new ork project. ADR-0057 §`ork init`.
//!
//! v1 ships the `minimal` template only. The other named templates from
//! the ADR (`eval`, `multi-agent`, `mcp`) are reserved for follow-ups;
//! selecting them errors out clearly so scripts can downgrade
//! gracefully.
//!
//! Templates are embedded at compile time via [`include_dir!`] from
//! `crates/ork-cli/templates/<template>/`. `.tmpl` files have their
//! `{{name}}` placeholder substituted with the user-supplied project
//! name at write time.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use include_dir::{Dir, include_dir};

static MINIMAL: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/templates/minimal");

#[derive(Args)]
pub struct InitArgs {
    /// Project directory name. The new directory is created in the cwd.
    pub name: String,

    /// Project template. v1 ships `minimal`; other templates error out.
    #[arg(long, value_enum, default_value_t = Template::Minimal)]
    pub template: Template,

    /// Override the dependency source for `ork-app` / `ork-server`. Pass an
    /// absolute path to a local checkout of the ork workspace (the template
    /// will be wired with `path = "<value>/crates/ork-app"`). Without this
    /// flag the template uses git deps with a placeholder URL the user
    /// is expected to update before `cargo build`.
    ///
    /// Smoke tests pass `--ork-source $CARGO_MANIFEST_DIR/../..` so the
    /// scaffolded project compiles against the in-repo ork crates.
    #[arg(long)]
    pub ork_source: Option<PathBuf>,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum Template {
    Minimal,
    Eval,
    MultiAgent,
    Mcp,
}

pub fn run(args: InitArgs) -> Result<()> {
    validate_project_name(&args.name)?;
    let target = std::env::current_dir()?.join(&args.name);
    if target.exists() {
        bail!(
            "ork init: target directory {} already exists; choose a different name or remove it.",
            target.display()
        );
    }
    let dir = match args.template {
        Template::Minimal => &MINIMAL,
        other => bail!(
            "ork init: template `{other:?}` is not yet available. v1 ships `minimal` only; the \
             eval / multi-agent / mcp templates are reserved for a follow-up ADR."
        ),
    };

    std::fs::create_dir(&target).with_context(|| format!("create {}", target.display()))?;
    let subs = Substitutions::for_args(&args)?;
    write_dir(dir, &target, &subs)?;

    eprintln!(
        "ork init: scaffolded {} (template={:?})",
        target.display(),
        args.template
    );
    if args.ork_source.is_none() {
        eprintln!(
            "ork init: NOTE — the generated Cargo.toml uses a placeholder git URL for ork-app/\
             ork-server. Edit the [dependencies] section before running `cargo build`, or \
             re-run `ork init --ork-source <path-to-ork-checkout>`."
        );
    }
    eprintln!("Next steps:");
    eprintln!("  cd {}", args.name);
    eprintln!("  ork dev");
    Ok(())
}

struct Substitutions {
    name: String,
    ork_app_dep: String,
    ork_server_dep: String,
}

impl Substitutions {
    fn for_args(args: &InitArgs) -> Result<Self> {
        let (ork_app_dep, ork_server_dep) = match &args.ork_source {
            Some(p) => {
                let abs = p
                    .canonicalize()
                    .with_context(|| format!("--ork-source: canonicalise {}", p.display()))?;
                let app = abs.join("crates").join("ork-app");
                let server = abs.join("crates").join("ork-server");
                (
                    format!("path = \"{}\"", app.display()),
                    format!("path = \"{}\"", server.display()),
                )
            }
            None => (
                // Placeholder — the user must edit before `cargo build`. Documented in README.
                "git = \"https://github.com/your-org/ork\", branch = \"main\"".to_string(),
                "git = \"https://github.com/your-org/ork\", branch = \"main\"".to_string(),
            ),
        };
        Ok(Self {
            name: args.name.clone(),
            ork_app_dep,
            ork_server_dep,
        })
    }

    fn apply(&self, raw: &str) -> String {
        raw.replace("{{name}}", &self.name)
            .replace("{{ork_dep}}", &self.ork_app_dep)
            .replace("{{ork_dep_server}}", &self.ork_server_dep)
    }
}

fn write_dir(src: &Dir<'_>, dest_root: &Path, subs: &Substitutions) -> Result<()> {
    for entry in src.entries() {
        match entry {
            include_dir::DirEntry::Dir(d) => {
                let rel = d.path();
                let dest = dest_root.join(rel);
                std::fs::create_dir_all(&dest)
                    .with_context(|| format!("create {}", dest.display()))?;
                write_dir(d, dest_root, subs)?;
            }
            include_dir::DirEntry::File(f) => {
                let rel = f.path();
                let raw = f.contents_utf8().with_context(|| {
                    format!(
                        "template file {} is not valid UTF-8; templates must be text",
                        rel.display()
                    )
                })?;
                let substituted = subs.apply(raw);
                let dest_path = dest_path_for(dest_root, rel);
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
                std::fs::write(&dest_path, substituted)
                    .with_context(|| format!("write {}", dest_path.display()))?;
            }
        }
    }
    Ok(())
}

/// Reviewer m2: reject names that would escape the cwd, embed shell/path
/// metacharacters, or break the generated Cargo.toml / Rust source via
/// `{{name}}` substitution. Cargo's package-name rules are stricter than
/// what we need for the directory; we conservatively pick the
/// intersection so the scaffolded project also publishes cleanly.
fn validate_project_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("ork init: project name must not be empty");
    }
    if name.len() > 64 {
        bail!("ork init: project name must be ≤ 64 characters");
    }
    let first = name.chars().next().unwrap();
    if !first.is_ascii_alphabetic() {
        bail!("ork init: project name must start with an ASCII letter (got `{name}`)");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "ork init: project name `{name}` may only contain ASCII letters, digits, `-`, and `_`"
        );
    }
    Ok(())
}

fn dest_path_for(dest_root: &Path, rel: &Path) -> PathBuf {
    // Strip a single trailing `.tmpl` from the file name so e.g.
    // `Cargo.toml.tmpl` → `Cargo.toml`.
    let mut out = dest_root.join(rel);
    if rel.extension().and_then(|s| s.to_str()) == Some("tmpl") {
        out.set_extension("");
    }
    out
}
