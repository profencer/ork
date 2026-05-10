//! `ork` CLI library surface — exposes the verb modules so integration
//! tests under `tests/` can exercise the dispatch entry points directly,
//! in addition to driving the `ork` binary via `assert_cmd`. ADR-0057.

pub mod build_cmd;
pub mod dev;
pub mod eval;
pub mod init;
pub mod inspect;
pub mod legacy;
pub mod lint;
pub mod migrate;
pub mod start;
