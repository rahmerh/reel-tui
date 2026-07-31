# Repository Guidelines

## Project Structure & Module Organization

This repository builds one Rust 2024 binary, `reel`, from `src/main.rs`. Keep responsibilities aligned across modules:

- `src/main.rs`: Entry point, worker spawning, and top-level terminal event loop.
- `src/app.rs`: Application state, navigation layer management, probe/edit event dispatching, and persistent cache integration.
- `src/ui.rs`: Ratatui rendering, split layout rendering, stream detail popups, and status indicators (including `[NET]` badge).
- `src/files.rs`: Directory scanning, file tree nesting, and adaptive filesystem monitoring.
- `src/probe.rs`: Asynchronous `ffprobe` worker thread, media metadata parsing, and tuned probing flags (`-probesize`, `-analyzeduration`).
- `src/edit.rs`: Asynchronous `ffmpeg` remuxing, track reordering, container conversion, progress reporting, cancellation, and cross-filesystem transaction publishing (`move_or_copy_file`).
- `src/subtitle.rs`: Subtitle stream inspection, sidecar file matching, format conversion, language tag translation, and OCR capability detection.
- `src/mount.rs`: Network mount detection via `/proc/mounts` (NFS, SMB/CIFS, SSHFS, Rclone, Ceph, etc.) and `REEL_NETWORK_MODE` environment variable overrides.
- `src/cache.rs`: Persistent on-disk probe metadata cache (`DiskCache`) stored at `$XDG_CACHE_HOME/reel-tui/probe_cache.json`.

Unit tests live beside their implementation in `#[cfg(test)] mod tests` blocks. Release publishing is defined in `.github/workflows/publish.yml`.

## Adaptive Filesystem & Performance Architecture

`reel-tui` automatically detects whether the target directory resides on a local disk or a remote network mount (NFS, SMB, etc.):

- **Directory Monitoring**: Local directories use a 1-second reconciliation interval; network mounts use an adaptive 10-second interval to eliminate network stat thrashing.
- **Persistent Probe Cache**: Media metadata is cached on disk in `$XDG_CACHE_HOME/reel-tui/probe_cache.json` indexed by path, file length, and mtime. Opening `reel` on network shares loads previously probed files instantly.
- **Fast Probe Flags**: Remote media probing uses `-probesize 2000000 -analyzeduration 3000000` to prevent deep network seeking.
- **Local Scratch Remuxing**: When remuxing on network mounts, intermediate work files write to local fast storage (`/tmp/reel-tui-scratch`) before performing a single final stream publish to destination.
- **Environment Overrides**: Set `REEL_NETWORK_MODE=1` (or `0`) to manually force or disable network performance tuning.

## Build, Test, and Development Commands

- `cargo run`: launch the TUI against the current directory. Ensure `ffmpeg` and `ffprobe` are available in `PATH`.
- `cargo build`: compile a debug binary.
- `cargo build --release`: compile an optimized release binary.
- `cargo install --path .`: install or replace the `reel` executable in `~/.cargo/bin/reel`.
- `cargo test`: run all module-local unit tests.
- `cargo fmt --check`: verify standard Rust formatting.
- `cargo clippy --all-targets -- -D warnings`: run strict linting used by CI.
- `cargo publish --dry-run`: validate packaging without publishing.

## Coding Style & Naming Conventions

Use `rustfmt` defaults (four-space indentation) and keep Clippy warning-free. Follow Rust conventions: `snake_case` for functions, variables, and modules; `PascalCase` for structs and enums; `SCREAMING_SNAKE_CASE` for constants. Prefer small, responsibility-focused functions and propagate recoverable failures with `anyhow::Result`. Preserve terminal cleanup guarantees and keep blocking `ffmpeg`/`ffprobe` work off the UI event loop.

## Testing Guidelines

Add tests in the module affected by the change, using descriptive behavior names such as `cancelled_edit_preserves_original`. Use temporary paths for filesystem cases and deterministic JSON fixtures for probe parsing. Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` before submitting.

## Commit & Pull Request Guidelines

Recent history uses short, imperative, feature-focused subjects, for example `Add keybinds menu` and `Be able to cancel processing file`. Keep each commit scoped to one coherent change. Pull requests should explain user-visible behavior, list verification commands, and link relevant issues. Include a terminal screenshot or recording when layout or interaction changes. Call out required `ffmpeg` behavior and any compatibility implications. Do not commit generated `target/` contents.

## Release Notes

Tags matching exact semantic versions (for example, `0.2.0`) trigger crates.io publishing. Release tags must point to commits reachable from `main`; avoid creating or pushing a tag until all CI checks pass.
