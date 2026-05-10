//! ADR-0057 — `lint` and `migrate` ship as clap-visible stubs.
//! Verify they exit 2 with the documented heads-up message so CI scripts
//! can rely on the contract.

use assert_cmd::Command;

#[test]
fn lint_stub_exits_2_with_followup_note() {
    Command::cargo_bin("ork")
        .expect("ork bin")
        .arg("lint")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("planned for a follow-up ADR"));
}

#[test]
fn migrate_stub_exits_2_with_followup_note() {
    Command::cargo_bin("ork")
        .expect("ork bin")
        .arg("migrate")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("planned for a follow-up ADR"));
}
