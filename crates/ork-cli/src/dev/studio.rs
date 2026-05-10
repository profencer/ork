//! Studio open-browser hook for `ork dev` (ADR-0057 §`ork dev`).
//!
//! v1 is plumbing only: prints what *would* happen if Studio were
//! bundled. The actual `pnpm build` orchestration and `--features
//! ork-webui/embed-spa` lands with ADR-0055.

pub fn open_browser_if_enabled(no_studio: bool, no_open: bool, port: u16) {
    if no_studio {
        eprintln!("ork dev: Studio disabled (--no-studio)");
        return;
    }
    eprintln!(
        "ork dev: Studio not bundled in this build — see ADR-0055 (the `ork-studio` crate has \
         not landed yet). The dev server is still serving the auto-generated REST/SSE surface \
         on http://127.0.0.1:{port}."
    );
    if no_open {
        // Intentionally no `open::that(...)` call here even when
        // `--no-open` is unset; the bundled Studio doesn't exist and
        // pointing the browser at `/studio` would 404. Once ADR-0055
        // lands, this is where the open-browser hook lives.
    }
}
