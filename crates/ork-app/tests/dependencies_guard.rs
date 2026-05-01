//! ADR [`0049`](../../../docs/adrs/0049-orkapp-central-registry.md): `[dependencies]` of `ork-app` must remain hexagonal-only.

#[derive(Copy, Clone)]
enum Sec {
    None,
    Deps,
    DevDeps,
    Other,
}

#[test]
fn ork_app_dependencies_exclude_infra_crates_in_non_dev_section() {
    let cargo = include_str!("../Cargo.toml");
    let forbidden = ["axum", "sqlx", "reqwest", "rmcp", "rskafka"];

    let mut section = Sec::None;
    for line in cargo.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[") && trimmed.ends_with("]") {
            section = match trimmed {
                "[dependencies]" => Sec::Deps,
                "[dev-dependencies]" => Sec::DevDeps,
                _ => Sec::Other,
            };
            continue;
        }
        if matches!(section, Sec::Deps) && !trimmed.is_empty() && !trimmed.starts_with('#') {
            let first = trimmed
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches(':');
            for b in forbidden {
                if first == b {
                    panic!(
                        "forbidden direct dependency `{b}` in `[dependencies]` of ork-app/Cargo.toml"
                    );
                }
            }
        }
    }
}
