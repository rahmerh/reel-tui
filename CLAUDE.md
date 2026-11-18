# reel-tui

Rust 2024 TUI for inspecting and remuxing media with `ffmpeg`/`ffprobe`, and editing subtitle tracks against the
picture. Library `reel_tui` holds everything; binary `reel` (`src/main.rs`) stays thin so tests drive the same code.

This file holds only what the code does not say: how to work here, and traps where the obvious change has
already been tried and broke. Do not undo one of these without re-reading its reason.

## Inbox

`INBOX.md` in this directory (git-ignored, may not exist) holds messages left for this project by other
sessions, mostly the workspace hub (`~/projects/workspace`) after it indexes the projects. At the start of a
session, if it exists, read it and tell Bas briefly what's in it. Act on a message only when Bas agrees;
once a message is handled or dropped, delete it, and delete the file when it's empty.

Other sessions only append to `INBOX.md`, never change anything else here. Each message gets a
`## YYYY-MM-DD, from <session>` heading, then a few plain lines: what, why, and where to look.

## Workflow

A change is done when all of these pass, in this order:

1. `cargo fmt --check`
2. `cargo clippy --all-targets -- -D warnings` (this skips `tests/e2e.rs`; if you touched it, also
   `cargo clippy --test e2e -- -D warnings`)
3. `cargo test`
4. `cargo install --path .` — always, so `~/.cargo/bin/reel` is current.
5. Report unit coverage method and totals, or say it could not be measured.

**E2E runs are restricted**: `cargo test --test e2e <scenario> -- --exact` only for a scenario you are writing or
changing; the full `cargo test --test e2e` only once, as the final gate before merging a branch.

Commits: short imperative subjects (`Add keybinds menu`), one coherent change each. Tags are exact semver
(`0.2.0`), publish to crates.io, must be on `main`, and only after CI is green.

## Code rules

- Keep blocking `ffmpeg`/`ffprobe` work off the event loop; keep terminal cleanup guaranteed; `anyhow::Result`
  for recoverable failures.
- **No inline control help text** in any dialog, popup or view. Key help lives only in the keybindings popup
  (`?`, rows from `src/keybindings.rs`). Its own footer naming `?` and `K` is the single exception;
  `the_edit_page_should_not_carry_inline_control_hints` guards the rest.
- **Save-path progress**: every blocking subprocess, slow filesystem step, validation, transaction phase,
  rollback and cleanup emits a short typed progress phase right before it runs. Measured progress only when the
  operation reports reliable units, otherwise the indeterminate loader, never an estimate. Changed edit paths
  need tests for progress sequence, cancellation and cleanup.
- **Never resolve the real cache directory in a test.** `DiskCache::cache_dir()` is throwaway under `cfg(test)`
  and the e2e harness redirects `XDG_CACHE_HOME`. Without both, tests overwrite the user's probe cache and fill
  `~/.cache/reel-tui/edit_errors.log` — the log to read first when hunting edit regressions.
- `preview.rs` does not go through `edit.rs`, so a read-only preview never writes `edit_errors.log`.

## Testing policy

- **Tests never skip** — no `#[ignore]`, early returns or environment-based passes. A missing program, codec or
  language pack fails with an actionable message.
- **Assert the observable result.** For editing features that means reading the file back off disk (and
  re-probing the container for muxed tracks); for picture features, comparing rendered bytes. Every layer
  short of those agrees while these features are broken.
- **Every new or materially changed behaviour needs an e2e scenario** in `tests/e2e.rs` as well as unit tests.
  Pure styling needs focused render/unit tests instead.
- **Every bug fix needs a unit regression test** that fails before and passes after, covering effects on
  neighbouring data too. A regression found in `edit_errors.log` quotes the log line in its doc comment.
- **New or changed production paths need 100% line and branch coverage from unit tests** (e2e does not count);
  project floor 90% lines. No inflation: no assertion-free tests, no exclusions, and no tests for unreachable
  code — delete that code and say so.
- Unit tests live in `#[cfg(test)] mod tests` beside the code. E2E fixtures (`tests/e2e/fixtures.rs`) are
  `lavfi` sources over real codecs and containers; that realism is the point.
- **E2E scenarios that render preview frames take `harness::frame_cache_lock()`**, or parallel scenarios evict
  each other's frames and hang.
- The e2e harness uses a halfblocks picker on purpose: it is the only image protocol `TestBackend` records.

### Pre-merge coverage

Only at the final gate (it runs the full e2e suite):

```sh
cargo +nightly llvm-cov clean --workspace
cargo +nightly llvm-cov --no-report --branch
cargo +nightly llvm-cov --no-report --branch --test e2e
cargo +nightly llvm-cov report --branch --summary-only
```

- Plain `cargo llvm-cov` silently drops the e2e target.
- Do not set `LLVM_COV`/`LLVM_PROFDATA` to `/usr/bin` tools (`raw profile version mismatch`).
- **If figures look halved**, `report` merged unexecuted objects (`debug/reel`, `build/*/out/reel`,
  `deps/reel_tui-*`): about twice the expected function count, zero branch counters. Find the object list with
  `--verbose` and run `llvm-cov report` on only the `out/reel_tui-*` and `out/e2e-*` binaries.

## Tool floors

Measured; do not lower one without re-measuring (`src/requirements.rs`). CI installs from
`.github/actions/media-tools`, never distro packages.

- **FFmpeg 8.1**: below it, MP4/MOV track titles read as absent and a remux erases them while validation passes.
- **seconv 5.1.0**: its `--version` says 5.0.0, so detection probes `--help` for `--no-vobsub-isolate-colors`.
- **Tesseract 4.0** for OCR.
- **`pkg-config` + ALSA headers** to build (`cpal`). No sound device is needed: `SilentOutput` is the
  production fallback.

## Traps

### Remuxing

- **MP4/MOV chapter tracks** (`probe::is_chapter_track`, by container tag, not codec — GoPro `gpmd` is also
  `bin_data`) are never mapped, since `-map_chapters 0` writes them itself and Matroska refuses the duplicate.
  The muxer's own chapter track must be filtered from every output count (`validate_result`,
  `muxer_forced_default_positions`, `apply_edits`), or correct output fails as misordered.
- `reconcile_files` remaps staged subtitle changes by path and fingerprint (`App::remap_subtitle_changes`),
  including `left_subtitle_order` positions. Do not go back to clearing them: writing a sidecar would discard
  every other staged change on the file.

### Subtitle formats

- **ASS is never transcoded** (it loses styles and positioning). **libass picks its reader by file extension**,
  so staged files must keep the right extension (`CueSlot`), and the staged filename and extraction `-c:s` must
  agree (`extracted_format`).
- Cue editing is SubRip only: ASS timing anchors `\move`/`\fad`/karaoke tags.
- `cue::collapse` folds only *adjacent* events with identical start, end and text. Do not loosen it — karaoke
  syllables differ per timing on purpose.

### Frame cache (`framecache.rs`)

- **`media_key` describes the video stream** (`preview::video_identity`), not the file, so a remuxing save keeps
  every frame. Use the stream's duration (or its Matroska `DURATION` tag), never the file's — FFmpeg 8.1 shifts
  audio ends on remux. **Do not reintroduce `migrate`, `frames_survive_edit`, `App::move_frame_cache` or
  `pending_frame_moves`.**
- **`cue_key` hashes the staged file handed to libass plus timing**, not the cue text, and covers every cue on
  screen with it.
- **Evict whole media directories only**, never single frames, never the open one.
- Bump `CACHE_FORMAT_VERSION` when a stored frame changes meaning.

### Frame rendering

- A burn stages every cue on screen at that moment, and the anchor cue is always included.
- **The warm pass builds groups from the whole cue list, not its slice**, and **each warm worker stages under its
  own filename** (`warm_cue_file(n)`). Either mistake files a wrong picture under the right key; only the bytes
  show it.
- Still grab needs `-pix_fmt yuvj420p` (mjpeg refuses limited-range YUV).
- When nothing is on screen, emit **no `subtitles` filter** (libass rejects an empty file), and don't leave a
  dangling comma in the filter chain.
- `newest_frame` must carry a coalesced-away `wanted`/`scrub` forward, or the pane stays empty.
- `FRAME_DEBOUNCE` holds back only a cue that must be rendered, not the whole dispatch.
- `window_radius()`, `prune_frames`, `nearby_frame_targets` and `has_missing_nearby` must use the same radius.
- **Halfblocks is refused for real use** (`preview::drawing_picker`); never add a cells-and-colours fallback.

### Scrub playback

- `scale` is a literal `W:H` without `force_original_aspect_ratio`: output is sliced by `w*h*3`, so a frame one
  row short shears all the rest.
- **Sound goes to a file, not a second pipe** (two pipes deadlock). A run with no sound must delete that file
  first and not read it after, or the previous cue's audio plays.
- Speed: `setpts` goes *after* `subtitles` and *before* `fps`; quarter speed is `atempo=0.5,atempo=0.5`
  (`atempo=0.25` is refused); budget the stretched span (`span / speed`); cap fps at the source's rate
  (`source_capped_fps`) before budgeting.
- **`playback_cells` and `playback_pixels` apply the font's cell aspect ratio in opposite directions and must
  cancel.** Never use a constant 2. Both errors distort the picture and no count or key notices.
- **The audio device is the clock**; counting written samples leads by the device latency and looks like an early
  subtitle. `span_slice` must not index past the end (`handed_over` overruns).
- **A pane resize must not stop a playback**: starting one adds a status row, which resizes the pane.
- **No dialog over a running or decoding playback, and no still image under any dialog.** Image protocols
  bypass the cell buffer, so either way you get an invisible dialog that eats keys, or a stale border.
  `TestBackend` cannot see it; the tests assert the rule.

### Subtitle edit page state

- **Staged cue edits are keyed by file position** and checked against a snapshot of words *and* timing. Page
  positions differ once cues are added or removed; translate through `SubtitleEditState::origins` and never
  renumber staged keys.
- **`rewrite_srt_cues` order is fixed**: check deletions against the file, apply rewrites, drop deletions, append
  insertions, then sort and renumber. Positions address the file as read.
- One staged entry per cue, storing the result, not the operation; insertions are the only second map.
- **Do not recompute overlap groups after a retiming** (it reflows the list under a held key); do repack lanes.
  Groups are recomputed on insert/remove.
- Global retiming fills the map in one pass (`App::shift_whole_track`); calling `stage_cue_change` per cue is
  quadratic.
- Retiming invalidates frames of every cue overlapping the old *or* new span, not just its own.
- Draw every timeline cue the same way. Crowding is fixed by shortening the window, never by demoting cues
  (tried, rejected). `axis_columns` is the single source of tick positions.
- `App::notice` reaches this page through `edit_status_line`, not `render_footer`.
- Which preview streams are chosen lives on `App`, not the page (switching destroys the page). Every command maps
  streams by absolute index.
- `close_subtitle_edit(PageExit::Reopening)` must not delete a new empty sidecar that the reopened page is about to read.
- New subtitle tracks are zero-byte sidecars written immediately. Never default their language to `und` (saves
  refuse it); an empty sidecar cannot be imported.
- The cue search is a projection (`visible`). Never renumber the list for it, and don't filter the timeline.
- `Alt+H`/`Alt+L` are deliberately unbound; do not give them a resize meaning back.

### Keybindings popup

- `App::key_context()` must mirror `handle_key`'s routing order exactly. The context is captured when `?` is
  pressed, not at render time (by then the dialog is the popup).
- Its cursor uses `choice_style`, never `Modifier::REVERSED`: the e2e harness treats reversed cells as search
  hits and timing handles only.
