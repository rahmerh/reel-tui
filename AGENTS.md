# Repository Guidelines

## Project Structure & Module Organization

This repository builds a Rust 2024 library, `reel_tui` (`src/lib.rs`), and one binary, `reel` (`src/main.rs`), on top of it. The library exists so integration tests under `tests/` can drive the same code the binary does. Keep responsibilities aligned across modules:

- `src/lib.rs`: Module declarations only; the crate root shared by the binary and the integration tests.
- `src/main.rs`: Entry point, worker spawning, terminal setup, and the top-level event loop. Deliberately thin — anything testable belongs in the library.
- `src/cli.rs`: Argument dispatch (`--help`, `--version`, the target directory) before the terminal is touched, so informational and usage-error output stays script-friendly.
- `src/config.rs`: The user's `config.toml` — worker-pool sizes, the desktop-notification switch, and the timing page's frame cache (`[preview] cache_tracks`, `[preview.prefetch] enabled`/`network`).
- `src/requirements.rs`: The external tool floors, enforced at startup. See **External Tool Floors** below.
- `src/input.rs`: Key handling (`handle_key` and friends): the mapping from a `KeyEvent` to an `App` mutation.
- `src/app.rs`: Application state, navigation layer management, probe/edit event dispatching, and persistent cache integration.
- `src/staging.rs`: Staged edits and in-flight batch state (`StagedEdit`, `BatchState`), the shape a save is dispatched and reported through.
- `src/ui.rs`: Ratatui rendering, split layout rendering, stream detail popups, and status indicators (including `[NET]` badge).
- `src/files.rs`: Directory scanning, file tree nesting, and adaptive filesystem monitoring.
- `src/framecache.rs`: The on-disk cache of rendered preview frames, at `$XDG_CACHE_HOME/reel-tui/preview_frames/{media_key}/{cue_key}.jpg`. Content-addressed with 128-bit FNV-1a: `media_key` covers the media file, its length and mtime, the size frames are rendered at, and `CACHE_FORMAT_VERSION`; `cue_key` covers the cue's text and timing. An edited cue simply misses and is re-rendered, with no invalidation logic anywhere. **The two-level path is the eviction policy, not tidiness.** What the user opens is a *track*, and a track missing some of its frames is re-rendered on every visit, so the unit of eviction has to be the unit of use: `prune(tracks, open)` deletes whole media directories, least recently used first, and never the open one. Ranking is by the `.used` marker's mtime (`touch`), so a fully cached track — the ideal case, where the pass renders nothing — does not age out under tracks rendered once. The directory *is* the index, which keeps the grouping without a side file to corrupt or a lock to take. Bump `CACHE_FORMAT_VERSION` when a stored frame stops meaning what it did; that orphans whole directories which then age out, instead of leaving files whose names collide with live keys. Frames are **JPEG** (`FRAME_EXTENSION`), not PNG: ~90 KB at 1080p against ~2 MB for a *smaller* PNG. Resolves its directory through `DiskCache::cache_dir()`, never from the environment, so both test redirections apply. Pure filesystem and hashing.
- `src/probe.rs`: Asynchronous `ffprobe` worker thread, media metadata parsing, and tuned probing flags (`-probesize`, `-analyzeduration`).
- `src/edit.rs`: Asynchronous `ffmpeg` remuxing, track reordering, container conversion, progress reporting, cancellation, and cross-filesystem transaction publishing (`move_or_copy_file`).
- `src/subtitle.rs`: Subtitle stream inspection, sidecar file matching, format conversion, language tag translation, and OCR/libass capability detection (`ToolCapabilities`).
- `src/cue.rs`: Cue-level subtitle data — SubRip parsing into individual cues, timeline lane packing, and the mapping from cue times to terminal columns. No dependency on `App`, `ratatui`, or subprocesses.
- `src/sync.rs`: State for the subtitle timing page (`SubtitleSyncState`): which cues a track holds, which is selected, the small window of frames encoded and ready to draw (the selected cue and `NEARBY_FRAMES` either side), how far the background frame pass has got (`WarmState`), and the scratch directory that is released when the page closes.
- `src/preview.rs`: The timing page's three background workers — reading a track's cues (`.srt` sidecar or embedded `subrip`), grabbing the video frame at the selected cue with that cue burned in plus encoding the cues either side of it from the cache (`FrameRequest`'s `wanted` and `nearby`), and rendering the rest of the track's frames behind them with scoped threads over contiguous slices — half the machine's cores, floored at 1 and capped at `MAX_WARM_WORKERS` (3), because a fixed three cost the *interactive* grab +157% latency on a dual core. **Each warm worker stages its cue under its own name** (`warm_cue_file(n)`, distinct from the interactive `CUE_FILE`): two sharing one staged `.srt` would overwrite each other between the write and the burn, and the frame would carry another cue's line under the right cache key — invisible to any assertion about keys, counts or files, which is why the tests for it compare rendered bytes. Frames are rendered at a fixed size derived from the source, capped at 1080p by `MAX_FRAME_PIXELS` (not the pane's size, which changes), and cached through `framecache.rs`. The grab is `mjpeg -pix_fmt yuvj420p -q:v 2`; the pixel format is load-bearing, since the mjpeg encoder refuses the limited-range YUV most video actually is. Deliberately does **not** route through `edit.rs`, so a read-only preview never writes `edit_errors.log`.
- `src/notification.rs`: Best-effort desktop notifications when a save finishes while the terminal is unfocused.
- `src/mount.rs`: Network mount detection via `/proc/mounts` (NFS, SMB/CIFS, SSHFS, Rclone, Ceph, etc.) and `REEL_NETWORK_MODE` environment variable overrides.
- `src/cache.rs`: Persistent on-disk probe metadata cache (`DiskCache`) stored at `$XDG_CACHE_HOME/reel-tui/probe_cache.json`.

Unit tests live beside their implementation in `#[cfg(test)] mod tests` blocks. End-to-end tests live in `tests/e2e.rs` with helpers in `tests/e2e/`. Release publishing is defined in `.github/workflows/publish.yml`.

## End-to-End Test Suite

`tests/e2e.rs` drives the real application: it replays the `main.rs` event loop in-process, feeds synthetic `KeyEvent`s through the same `handle_key`, renders through the same `ui::render` into a `TestBackend`, and runs genuine `ffprobe`/`ffmpeg` subprocesses against real files. Nothing in the crate is mocked; only crossterm's terminal setup and byte decoding are bypassed.

- It is a **separate cargo test target with `test = false`**, so ordinary `cargo test` does not run it; use the restricted execution rules below.
- **Comprehensive e2e coverage is the target, and the current gaps are testing debt to fill incrementally.** Whenever work touches a functional area that lacks an overarching scenario in `tests/e2e.rs`, add one as part of that work.
- **Every new or materially changed functional behavior must be covered by at least one overarching scenario in `tests/e2e.rs`.** The scenario must exercise the behavior through the real application workflow independently of module-local unit tests and make meaningful assertions about the observable result. Coverage overlap is explicitly welcome and does not waive either layer. Purely visual styling or layout changes do not require their own e2e scenario, but do require focused rendering/unit coverage.
- **Tests never skip.** Do not use `#[ignore]`, conditional early returns, environment-based success paths, or any other mechanism that reports an unexecuted test as passing. Required programs, codecs, language data, and other external capabilities are test-environment prerequisites; fail immediately with an actionable missing-prerequisite message.
- **E2E execution is deliberately restricted.** Run an individual scenario only while creating or directly changing that scenario. Run the complete e2e suite only once as the final verification immediately before merging a branch. Do not run e2e tests during ordinary implementation, routine verification, or on every edit. Minutes of wall clock for the final suite is fine: use real codecs, containers, subprocesses, and OCR rather than reducing fidelity for speed.
- When a regression scenario comes from a failure in `~/.cache/reel-tui/edit_errors.log`, quote that log line in its doc comment. That log is the first place to look when hunting for an edit regression worth locking in; UI and workflow regressions need not originate there.
- Fixtures are built by `tests/e2e/fixtures.rs` over `lavfi` sources, parameterised by codec and container. Codec/container realism is the point: the bugs this suite exists for come from combinations like `subrip` into MP4 or `mov_text` into Matroska, which the simpler `ffv1`/`pcm_s16le` unit fixtures cannot express.
- The harness redirects `XDG_CACHE_HOME` to throwaway storage, so runs never touch the user's real probe cache or failure log. The unit-test binary is covered separately: `DiskCache::cache_dir()` returns throwaway storage under `cfg(test)` (`src/cache.rs`). **Both halves are load-bearing** — without them a test run rewrites `~/.cache/reel-tui/probe_cache.json` with `/tmp` fixture paths and fills `edit_errors.log` with deliberately-failing test edits, which is exactly the log this file tells you to read first. Never add a test that resolves the real cache directory.
- **Write tests that would notice.** A test that passes against broken code converts an unchecked path into a checked-looking one, so assert the observable result rather than the fact that something ran. A feature-level e2e scenario need not reproduce every defect in that feature; focused regression coverage belongs in the unit suite.

## External Tool Floors

`src/requirements.rs` holds the minimum versions, each measured rather than guessed. Do not lower one without re-measuring; each is load-bearing.

- **FFmpeg 8.1** (`ffmpeg` *and* `ffprobe`) — enforced at startup, refusing to launch below it. `n8.1` is the first release containing FFmpeg commit `e59d964a3c`, which taught the `mov` demuxer to read the ISO-BMFF `name` atom. Below it `ffprobe` reports MP4/MOV track titles as absent, so a remux erases every title, `apply_edits` writes the erasure out as `title=`, and `validate_result` compares absent against absent and passes it. No flag works around it — the muxer never receives a title the demuxer did not parse. The e2e suite otherwise passes on 5.0.1 through 8.0.1, so the floor is entirely about this.
- **seconv 5.1.0** — gates subtitle conversion and OCR, not startup. 5.0.0 cannot read a VobSub `.idx` ("Unable to determine subtitle format"). **Cannot be version-gated**: the 5.1.0 build reports `5.0.0` from `--version`, so `subtitle::seconv_is_supported` probes the `--help` listing for `--no-vobsub-isolate-colors` instead. Also needs `libicu` present, or it aborts in `Bootstrap.Initialize`.
- **Tesseract 4.0** — gates OCR only. Verified working on 4.0.0, 4.1.1, and 5.3.x. 3.x is unreachable regardless: `seconv` needs a newer glibc than any distribution shipping it.

CI installs these from `.github/actions/media-tools`, never from the runner's distribution packages — `ubuntu-latest` ships FFmpeg 6.1.1, which is below the floor. `.github/workflows/ci.yml` runs both suites on every push and pull request to `main`; `publish.yml` runs on release tags only and is not a substitute for it.

## Adaptive Filesystem & Performance Architecture

`reel-tui` automatically detects whether the target directory resides on a local disk or a remote network mount (NFS, SMB, etc.):

- **Directory Monitoring**: Local directories use a 1-second reconciliation interval; network mounts use an adaptive 10-second interval to eliminate network stat thrashing.
- **Persistent Probe Cache**: Media metadata is cached on disk in `$XDG_CACHE_HOME/reel-tui/probe_cache.json` indexed by path, file length, and mtime. Opening `reel` on network shares loads previously probed files instantly.
- **Fast Probe Flags**: Remote media probing uses `-probesize 2000000 -analyzeduration 3000000` to prevent deep network seeking.
- **Local Scratch Remuxing**: When remuxing on network mounts, intermediate work files write to local fast storage (`/tmp/reel-tui-scratch`) before performing a single final stream publish to destination.
- **Preview Frame Cache**: Opening the subtitle timing page renders every cue's frame in the background and caches it on disk, so revisiting a cue — or re-opening the page — costs a decode rather than an `ffmpeg` seek. Bounded by whole media files (`cache_tracks`, default 10) rather than by bytes, because a byte budget can bisect a track and half a track is re-rendered on every visit. Off by default on network mounts, where a feature-length track is a thousand-odd accurate seeks across the network; the page says so in its status row rather than leaving the absence unexplained.
- **Ready Preview Frames**: On top of the disk cache, the page keeps the selected cue's frame and the ones either side of it *encoded* in memory, so moving the cursor draws a picture in the same pass that handled the keypress rather than after a round trip to the worker. `FRAME_DEBOUNCE` therefore applies only to a cue that would have to be rendered — one already in the frame cache is asked for immediately, since it costs a file read rather than an accurate seek. The preview pane draws nothing when it has no frame; it never substitutes the cue's text, which flashed on every cursor move.
- **Environment Overrides**: Set `REEL_NETWORK_MODE=1` (or `0`) to manually force or disable network performance tuning.

## Build, Test, and Development Commands

- `cargo run`: launch the TUI against the current directory. Ensure `ffmpeg` and `ffprobe` are available in `PATH`.
- `cargo build`: compile a debug binary.
- `cargo build --release`: compile an optimized release binary.
- `cargo install --path .`: install or replace the `reel` executable in `~/.cargo/bin/reel`. Always run this after making changes.
- `cargo test`: run all module-local unit tests. Excludes the e2e suite.
- `cargo test --test e2e <scenario> -- --exact`: run one scenario only while creating or directly changing that scenario.
- `cargo test --test e2e`: run the complete end-to-end suite only as the final pre-merge gate. Its required `ffmpeg`/`ffprobe` programs and encoders must be installed; missing prerequisites fail the suite.
- `cargo fmt --check`: verify standard Rust formatting.
- `cargo clippy --all-targets -- -D warnings`: run strict linting used by CI. Note this does **not** cover `tests/e2e.rs`, whose target sets `test = false`; lint that one with `cargo clippy --test e2e -- -D warnings`.
- `cargo publish --dry-run`: validate packaging without publishing.

## Coding Style & Naming Conventions

Use `rustfmt` defaults (four-space indentation) and keep Clippy warning-free. Follow Rust conventions: `snake_case` for functions, variables, and modules; `PascalCase` for structs and enums; `SCREAMING_SNAKE_CASE` for constants. Prefer small, responsibility-focused functions and propagate recoverable failures with `anyhow::Result`. Preserve terminal cleanup guarantees and keep blocking `ffmpeg`/`ffprobe` work off the UI event loop.

**No inline control help text**: never add a hint/help line (e.g. "↑↓ select · Enter apply") to a dialog, popup, or view's render function, no matter how new or small. All keybinding help lives solely in the global keybindings popup (`?`, `Dialog::Keybindings`/`keybindings_text()` in `src/ui.rs`). If a new view's keys aren't already covered by that popup's generic entries, extend `keybindings_text()` rather than adding local hint text.

## Testing & Reinstallation Guidelines

Add tests in the module affected by the change, using descriptive behavior names such as `cancelled_edit_preserves_original`. Use temporary paths for filesystem cases and deterministic JSON fixtures for probe parsing. No test in any suite may be ignored or conditionally skipped. During ordinary work, run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`; reserve the complete e2e suite for the final pre-merge gate.

### Coverage and Regression Policy

- Maintain a project target of at least **90% measured line coverage**, and prefer coverage comfortably above 90%. Measure branch coverage as well when the installed Rust coverage tooling supports it, and improve weak branch coverage rather than relying on the line percentage alone.
- New or modified production paths must have **100% line and branch coverage from module-local unit tests**. E2E coverage is a separate requirement and does not substitute for unit coverage. Existing uncovered code remains debt, but touching it means covering the affected paths completely.
- Every bug fix must include a focused module-local regression unit test that fails before the fix and passes afterward. Reproduce the complete failure mode, including effects on neighboring or untouched data; do not test only the value directly changed by the user. The affected functional area must also have an overarching e2e scenario; add one if it does not, but that scenario need not reproduce the exact defect.
- Cover meaningful success, failure, cancellation, validation, and transaction/rollback paths. For FFmpeg and ffprobe behavior, include integration tests using small deterministic media fixtures when unit tests cannot verify the real command semantics.
- Treat coverage as a diagnostic floor, not proof of correctness. Do not inflate it with assertion-free tests, broad exclusions, or tests that execute code without validating observable behavior. **Covering unreachable code is inflation too**: before writing a test to reach the coverage bar, check that the application can actually reach the path. If it cannot, delete the path instead of testing it, and say so.
- Report the unit-test coverage method and totals with completed ordinary changes. Report merged unit-and-e2e coverage only at the final pre-merge gate. If coverage cannot be measured in the current environment, state that explicitly rather than estimating it.
- **Measuring final pre-merge coverage.** Branch coverage needs `-Z coverage-options=branch`, which only nightly `rustc` accepts, so it runs under `cargo +nightly llvm-cov --branch`. Because the unit and e2e suites are separate targets, the merged commands below run the complete e2e suite and therefore must only be used immediately before merging a branch:

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
