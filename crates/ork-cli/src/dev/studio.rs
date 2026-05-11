//! Studio open-browser hook for `ork dev` (ADR-0055, ADR-0057 §`ork dev`).
//!
//! v1 ships the open-browser call. The user binary chooses whether to
//! mount Studio via [`ServerConfig::studio`](ork_app::types::ServerConfig);
//! `ork dev` only nudges the browser at the right moment. ADR-0055's
//! reverse-proxy follow-up (HMR + token forwarding) lives in a later
//! ADR.

/// Open the user's default browser at `http://127.0.0.1:{port}/studio`.
///
/// * `no_studio` — operator passed `--no-studio`; skip entirely.
/// * `no_open` — operator passed `--no-open`; mention the URL but
///   don't launch a browser process.
/// * `port` — the listener the user binary serves Studio on; the
///   default in `ork dev` is 4111.
pub fn open_browser_if_enabled(no_studio: bool, no_open: bool, port: u16) {
    if no_studio {
        eprintln!("ork dev: Studio disabled (--no-studio)");
        return;
    }
    let url = format!("http://127.0.0.1:{port}/studio");
    if no_open {
        eprintln!("ork dev: open {url} (--no-open suppressed launch)");
        return;
    }
    eprintln!("ork dev: opening {url}");
    if let Err(e) = open::that(&url) {
        eprintln!("ork dev: open(\"{url}\") failed: {e}; copy/paste into a browser.");
    }
}
