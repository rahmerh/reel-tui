# Repository Guidelines

## Project Structure & Module Organization

This repository builds one Rust 2024 binary, `reel`, from `src/main.rs`. Keep responsibilities aligned with the existing modules:

- `src/app.rs`: application state, navigation, and edit orchestration.
- `src/ui.rs`: Ratatui rendering and layout.
- `src/files.rs`: directory scanning and media-file discovery.
- `src/probe.rs`: `ffprobe` execution and metadata parsing.
- `src/edit.rs`: `ffmpeg` remuxing, validation, progress, and cancellation.

Unit tests live beside their implementation in `#[cfg(test)] mod tests` blocks. There is no separate assets directory. Release publishing is defined in `.github/workflows/publish.yml`.

## Build, Test, and Development Commands

- `cargo run`: launch the TUI against the current directory. Ensure `ffmpeg` and `ffprobe` are available in `PATH`.
- `cargo build`: compile a debug binary.
- `cargo build --release --locked`: reproduce the optimized CI release build.
- `cargo test --locked`: run all module-local unit tests.
- `cargo fmt --check`: verify standard Rust formatting.
- `cargo clippy --locked --all-targets -- -D warnings`: run the same strict linting used by CI.
- `cargo publish --dry-run --locked`: validate packaging without publishing.

## Coding Style & Naming Conventions

Use `rustfmt` defaults (four-space indentation) and keep Clippy warning-free. Follow Rust conventions: `snake_case` for functions, variables, and modules; `PascalCase` for structs and enums; `SCREAMING_SNAKE_CASE` for constants. Prefer small, responsibility-focused functions and propagate recoverable failures with `anyhow::Result`. Preserve terminal cleanup guarantees and keep blocking `ffmpeg`/`ffprobe` work off the UI event loop.

## Testing Guidelines

Add tests in the module affected by the change, using descriptive behavior names such as `cancelled_edit_preserves_original`. Use temporary paths for filesystem cases and deterministic JSON fixtures for probe parsing. Changes to stream ordering, default flags, deletion, remux validation, or keyboard behavior should include regression coverage. Run formatting, Clippy, and the full test suite before submitting.

## Commit & Pull Request Guidelines

Recent history uses short, imperative, feature-focused subjects, for example `Add keybinds menu` and `Be able to cancel processing file`. Keep each commit scoped to one coherent change. Pull requests should explain user-visible behavior, list verification commands, and link relevant issues. Include a terminal screenshot or recording when layout or interaction changes. Call out required `ffmpeg` behavior and any compatibility implications. Do not commit generated `target/` contents.

## Release Notes

Tags matching exact semantic versions (for example, `0.2.0`) trigger crates.io publishing. Release tags must point to commits reachable from `main`; avoid creating or pushing a tag until all CI checks pass.
