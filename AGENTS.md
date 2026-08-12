# Repository Guidelines

## Project Structure & Module Organization

This repository builds a Rust 2024 library, `reel_tui` (`src/lib.rs`), and one binary, `reel` (`src/main.rs`), on top of it. The library exists so integration tests under `tests/` can drive the same code the binary does. Keep responsibilities aligned across modules:

- `src/lib.rs`: Module declarations only; the crate root shared by the binary and the integration tests.
- `src/main.rs`: Entry point, worker spawning, terminal setup, and the top-level event loop. Deliberately thin — anything testable belongs in the library.
- `src/input.rs`: Key handling (`handle_key` and friends): the mapping from a `KeyEvent` to an `App` mutation.
- `src/app.rs`: Application state, navigation layer management, probe/edit event dispatching, and persistent cache integration.
- `src/ui.rs`: Ratatui rendering, split layout rendering, stream detail popups, and status indicators (including `[NET]` badge).
- `src/files.rs`: Directory scanning, file tree nesting, and adaptive filesystem monitoring.
- `src/probe.rs`: Asynchronous `ffprobe` worker thread, media metadata parsing, and tuned probing flags (`-probesize`, `-analyzeduration`).
- `src/edit.rs`: Asynchronous `ffmpeg` remuxing, track reordering, container conversion, progress reporting, cancellation, and cross-filesystem transaction publishing (`move_or_copy_file`).
- `src/subtitle.rs`: Subtitle stream inspection, sidecar file matching, format conversion, language tag translation, and OCR capability detection.
- `src/mount.rs`: Network mount detection via `/proc/mounts` (NFS, SMB/CIFS, SSHFS, Rclone, Ceph, etc.) and `REEL_NETWORK_MODE` environment variable overrides.
- `src/cache.rs`: Persistent on-disk probe metadata cache (`DiskCache`) stored at `$XDG_CACHE_HOME/reel-tui/probe_cache.json`.

Unit tests live beside their implementation in `#[cfg(test)] mod tests` blocks. End-to-end tests live in `tests/e2e.rs` with helpers in `tests/e2e/`. Release publishing is defined in `.github/workflows/publish.yml`.

## End-to-End Test Suite

`tests/e2e.rs` drives the real application: it replays the `main.rs` event loop in-process, feeds synthetic `KeyEvent`s through the same `handle_key`, renders through the same `ui::render` into a `TestBackend`, and runs genuine `ffprobe`/`ffmpeg` subprocesses against real files. Nothing in the crate is mocked; only crossterm's terminal setup and byte decoding are bypassed.

- It is a **separate cargo test target with `test = false`**, so `cargo test` does not run it. Run it with `cargo test --test e2e`.
- **E2E scenarios are allowed to be slow.** They are run individually while working on one, and as a whole suite before merging — never on every edit. Minutes of wall clock for the full suite is fine. Buy realism with that budget: real codecs, real containers, real subprocesses, real OCR. Do not trade a scenario's fidelity for speed, and do not skip covering a path here because "it would need a real encode" — that is precisely what this target is for.
- Every scenario reproduces a failure that actually reached `~/.cache/reel-tui/edit_errors.log` in real use, and quotes that log line in its doc comment. That log is the first place to look when hunting for a regression worth locking in.
- Fixtures are built by `tests/e2e/fixtures.rs` over `lavfi` sources, parameterised by codec and container. Codec/container realism is the point: the bugs this suite exists for come from combinations like `subrip` into MP4 or `mov_text` into Matroska, which the simpler `ffv1`/`pcm_s16le` unit fixtures cannot express.
- The harness redirects `XDG_CACHE_HOME` to throwaway storage, so runs never touch the user's real probe cache or failure log. The unit-test binary is covered separately: `DiskCache::cache_dir()` returns throwaway storage under `cfg(test)` (`src/cache.rs`). **Both halves are load-bearing** — without them a test run rewrites `~/.cache/reel-tui/probe_cache.json` with `/tmp` fixture paths and fills `edit_errors.log` with deliberately-failing test edits, which is exactly the log this file tells you to read first. Never add a test that resolves the real cache directory.
- When adding a scenario, verify it actually catches the regression by reintroducing the bug and watching it fail. A test that passes against broken code is worse than no test.

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
- `cargo install --path .`: install or replace the `reel` executable in `~/.cargo/bin/reel`. Always run this after making changes.
- `cargo test`: run all module-local unit tests. Excludes the e2e suite.
- `cargo test --test e2e`: run the end-to-end suite (requires `ffmpeg`/`ffprobe`; individual tests self-skip when an encoder is missing).
- `cargo fmt --check`: verify standard Rust formatting.
- `cargo clippy --all-targets -- -D warnings`: run strict linting used by CI.
- `cargo publish --dry-run`: validate packaging without publishing.

## Coding Style & Naming Conventions

Use `rustfmt` defaults (four-space indentation) and keep Clippy warning-free. Follow Rust conventions: `snake_case` for functions, variables, and modules; `PascalCase` for structs and enums; `SCREAMING_SNAKE_CASE` for constants. Prefer small, responsibility-focused functions and propagate recoverable failures with `anyhow::Result`. Preserve terminal cleanup guarantees and keep blocking `ffmpeg`/`ffprobe` work off the UI event loop.

**No inline control help text**: never add a hint/help line (e.g. "↑↓ select · Enter apply") to a dialog, popup, or view's render function, no matter how new or small. All keybinding help lives solely in the global keybindings popup (`?`, `Dialog::Keybindings`/`keybindings_text()` in `src/ui.rs`). If a new view's keys aren't already covered by that popup's generic entries, extend `keybindings_text()` rather than adding local hint text.

## Testing & Reinstallation Guidelines

Add tests in the module affected by the change, using descriptive behavior names such as `cancelled_edit_preserves_original`. Use temporary paths for filesystem cases and deterministic JSON fixtures for probe parsing. Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` before submitting.

### Coverage and Regression Policy

- Maintain a project target of at least **90% measured line coverage**, and prefer coverage comfortably above 90%. Measure branch coverage as well when the installed Rust coverage tooling supports it, and improve weak branch coverage rather than relying on the line percentage alone.
- Until the repository reaches 90%, changes must not reduce overall coverage. New or modified production paths should be covered comprehensively and should improve coverage where practical, especially in modules below the target.
- Every bug fix must include a focused regression test that fails before the fix and passes afterward. Reproduce the complete failure mode, including effects on neighboring or untouched data; do not test only the value directly changed by the user.
- Cover meaningful success, failure, cancellation, validation, and transaction/rollback paths. For FFmpeg and ffprobe behavior, include integration tests using small deterministic media fixtures when unit tests cannot verify the real command semantics.
- Treat coverage as a diagnostic floor, not proof of correctness. Do not inflate it with assertion-free tests, broad exclusions, or tests that execute code without validating observable behavior.
- Report the measured coverage method and totals with completed changes. If coverage cannot be measured in the current environment, state that explicitly rather than estimating it.
- **Measuring coverage.** Branch coverage needs `-Z coverage-options=branch`, which only nightly `rustc` accepts, so it runs under `cargo +nightly llvm-cov --branch`. The unit and e2e suites are separate targets and each produces only its own profile, so measure them together and report the merged result:

  ```sh
  cargo +nightly llvm-cov clean --workspace
  cargo +nightly llvm-cov --no-report --branch            # unit tests
  cargo +nightly llvm-cov --no-report --branch --test e2e # e2e suite
  cargo +nightly llvm-cov report --branch --summary-only
  ```

  A plain `cargo llvm-cov` run silently excludes the e2e suite (its target sets `test = false`), which understates `edit.rs` in particular — the remux paths it covers are the ones only e2e reaches.

  Do **not** set `LLVM_COV`/`LLVM_PROFDATA` to the system `/usr/bin` binaries for these runs: the Arch system LLVM is older than nightly's and fails with `raw profile version mismatch`. Rustup supplies matching `llvm-tools` for the toolchain automatically. (That override was only needed back when the machine had no rustup at all.)

  Note that `llvm-cov` counts branches inside `#[cfg(test)]` modules as well as production code, so a test containing loops or `match`es raises the denominator along with the numerator. Treat the per-file trend, not the absolute percentage, as the signal.

**Mandatory Reinstall Step**: After making changes and successfully passing tests, ALWAYS run `cargo install --path .` so the updated `reel` binary is immediately installed to `~/.cargo/bin/reel`.

### Edit Progress Contract

Treat accurate progress reporting as a required part of every Save workflow change. Every blocking subprocess, potentially slow filesystem operation, validation pass, transaction phase, rollback, and cleanup path must emit a concise typed progress phase immediately before doing the work. Use measured phase-local progress only when the underlying operation exposes reliable units; otherwise show the indeterminate loader rather than estimating. Keep labels short enough for the Save dialog, use basenames and compact codec names, and omit redundant import/export wording. New or changed edit paths must include regression tests for their progress sequence, cancellation, and cleanup behavior.

## Commit & Pull Request Guidelines

Recent history uses short, imperative, feature-focused subjects, for example `Add keybinds menu` and `Be able to cancel processing file`. Keep each commit scoped to one coherent change. Pull requests should explain user-visible behavior, list verification commands, and link relevant issues. Include a terminal screenshot or recording when layout or interaction changes. Call out required `ffmpeg` behavior and any compatibility implications. Do not commit generated `target/` contents.

## Release Notes

Tags matching exact semantic versions (for example, `0.2.0`) trigger crates.io publishing. Release tags must point to commits reachable from `main`; avoid creating or pushing a tag until all CI checks pass.
