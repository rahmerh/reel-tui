# Subtitle sync preview — remaining work

> **Delete this file before merging to `main`.** It is a working handoff for an
> in-progress branch, not documentation of the finished feature.

Handoff for the `subtitle-sync-preview` branch. Steps 0–5 are done, and step 6's gate has
been run once — but the branch is **not** merging yet, so this file stays until it is. See
"Step 6" for what has been run and what has to be re-run at the actual merge. The full original design is at
`~/.claude/plans/okay-so-let-s-create-cryptic-ember.md`.

## What this feature is

A read-only, full-screen page for inspecting one subtitle track's timing, opened with `c`
on a subtitle track in the Streams layer. Right pane: scrollable cue list (text +
timestamps), the only interactive element — `j`/`k` move the selection. Bottom: full-width
timeline of cues as `|<──>|` spans, stacked into lanes where cues overlap. Top-left: the
video frame at the selected cue, with the subtitle burned in by ffmpeg's libass
`subtitles` filter.

**SRT only.** The track must already *be* SubRip — a `.srt` sidecar or an embedded
`subrip` stream. Every other format is refused with "…is not implemented yet. Only SRT is
supported." No conversion, no OCR anywhere in this feature.

v1 is inspection-only: no timing edits, no saving.

## Current state — DONE

### Step 0 — `select_*` refactor (`src/app.rs:1611`+)
`select_next`/`select_previous`/`select_first`/`select_last` converted from leading
`if self.layer == …` chains into exhaustive `match self.layer`, matching `scroll_down`
(`:6570`) and `scroll_up` (`:6590`). Pure refactor. **This is load-bearing**: it turned the
file-list fall-through hazard into a compile error, and adding the `Layer` variant then
produced exactly 7 compiler errors pointing at every site that needed a decision.

### Step 1 — `src/cue.rs` (new, ~740 lines with tests)
Pure module, no crate dependencies.
- `Cue { index, start, end, text }` + `midpoint()` (frame-grab moment; midpoint not start,
  because a cue's start often lands on a scene cut).
- `parse_srt(&str) -> Vec<Cue>` — returns `Vec` not `Result`; driven off the `-->` timing
  line, never off index lines or blank separators. Handles BOM, missing/absent/wrong index
  lines, missing blank separators, `-->` inside cue text, trailing position tags, `,`/`.`
  separators, short/long fractional fields, missing hours, out-of-order cues, `end < start`
  (clamped), empty text, and integer overflow on every multiply.
- `read_srt(&Path)` — lossy UTF-8 decode (Windows-1252 files are common).
- `pack_lanes(&[Cue], max_lanes) -> LaneLayout` — greedy **first-fit** interval packing.
  `MAX_LANES = 4`. Overflow crowds onto the last lane, never drops. `<=` when testing lane
  availability (touching cues share a lane).
- `TimelineWindow` — fixed 60 s window centered on the selection, clamped both ends;
  `column()` and `span()` map times to columns.

**Coverage: 100% line, 100% branch, 100% function.** 24 deliberate breaks all proven to
fail the corresponding test (plus 3 more for the overflow multiply sites).

Two pieces of dead code were found by the proof exercise and deleted rather than tested:
a redundant `trim_end_matches('\r')` (`lines()`/`trim_end()`/`trim()`/`split_whitespace`
already cover every path) and a `span.is_zero()` guard in `column()` (unreachable from
`centered`, and NaN→0 saturating casts make it unobservable anyway).

### Step 2 — layer, page state, `App` plumbing
- **`src/sync.rs`** (new): `SubtitleSyncState`, `SyncStatus{Preparing,Ready,Empty,Failed}`,
  and `PreviewWorkspace(PathBuf)` with a `Drop` that `remove_dir_all`s. Workspace lives at
  `temp_dir()/reel-tui-preview/{pid}-{nanos}/` — deliberately *not* beside the media,
  because a name we control is what lets the frame worker hand ffmpeg a bare relative
  `cue.srt` instead of escaping a user path through filter-graph syntax.
  `select()`/`select_first()`/`select_last()` return `bool` "did it move" — Step 5 needs
  that to avoid one ffmpeg per key repeat at the list ends. `sync_scroll(rows)` is called
  from the renderer.
- **`Layer::SubtitleSync`** added (`src/app.rs:41`).
- **`App`**: `subtitle_sync: Option<SubtitleSyncState>`, `sync_generation: u64`.
  `open_subtitle_sync()` (guards in order: layer/dialog → is-it-a-subtitle → marked for
  deletion → **format must be SubRip**), `close_subtitle_sync()` (the *only* place the
  state is cleared; called from `back()` and `queue_probe()`), `selected_subtitle_source()`,
  `move_sync_cue()`. `is_animating()` extended.
- **`src/input.rs`**: `c` bound in `handle_layer_key`. **Fixed a real latent bug**: `R`
  (`request_reset_current_file`, which discards staged edits) was guarded by
  `app.layer != Layer::Files`, so it would have fired on the new page. Now
  `matches!(app.layer, Layer::Streams | Layer::StreamDetails)`.
- **`src/ui.rs`**: full-frame early return dispatching to `render_subtitle_sync`, a
  first-cut cue list plus the Preparing/Empty/Failed states, `details_selected_stream`
  tightened off `!= Layer::Files`, and a `c` entry in `keybindings_text()`.
- `edit::media_duration` made `pub(crate)`.

**Coverage: `sync.rs` 100% line + 100% branch.** 14 deliberate breaks all proven.
840 unit tests pass; `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`
are clean; `cargo install --path .` has been run.

### Known incomplete behavior right now
Nothing is stubbed. `c` opens the page, the cues load, `j`/`k` move the selection, the
timeline stacks overlapping cues, the preview shows the video frame at the selected cue
with that cue burned in, and Esc releases the page with its workspace. What is left is
step 6's pre-merge gate.

---

## Step 3 — render the page — DONE

`render_subtitle_sync` in `src/ui.rs` draws three panes: `Preview` (left), `Cues` (right),
`Timeline` (full width, bottom). Non-Ready statuses short-circuit to one centred message.

- **Track height is `lane_count + 2`**, so an overlapping region costs the preview pane
  exactly the rows it gains (3..6 rows total).
- **Cue panel width is `(width * 35 / 100).clamp(28, 48)`**, not a plain percentage: its
  content is fixed-format, and at the 50-column minimum a proportional split truncated the
  timestamps away. Preview takes the remainder.
- `timeline_lines` / `cue_glyphs` / `runs` are **pure functions**, so the column arithmetic
  is asserted directly rather than through a rendered frame. Glyphs degrade 1 `|`, 2 `||`,
  3 `|─|`, 4 `|<>|`, ≥5 `|<─…─>|`. Selected cyan+bold, overflowed magenta, rest dark gray;
  the selected cue is painted **last** so it survives a crowded lane.
- The preview pane shows the selected cue's text for now. That is not scaffolding — it is
  the same fallback used when the terminal has no image protocol or FFmpeg lacks libass.
- `state.preview_cells` is written back from the renderer (Step 5's worker needs it).
- **No custom `Widget` impl**: the track is `Vec<Line>` in a `Paragraph`. Repo still has
  zero `Buffer` manipulation.

Three dead paths were found by the proof exercise and deleted rather than tested: a
minimum-size guard (unreachable — `render` already enforces 50×10 and four lanes need only
nine rows), a `column < width` bounds check in `timeline_lines` (`span` clamps and
`cue_glyphs` returns exactly its argument, so it cannot fire), and a `.max(1)` on the lane
count (`pack_lanes` already guarantees ≥1).

One real bug was caught by looking at a rendered dump: `render_sync_cues` keyed the
selection marker off `Cue::index` rather than row position, so nothing was ever marked —
`apply_prepared` does not renumber the cues it is handed. It is positional now.

**15 deliberate breaks all proven.** `ui.rs` branch coverage 88.53% → 89.02%; every branch
in the new page code is covered.

## Step 4 — prepare worker — DONE

`src/preview.rs` (new, ~730 lines with tests) plus the `main.rs` and harness wiring. The
page now fills in for real; this is the first `cargo run`-verifiable milestone and is
shippable on its own with the text preview pane.

- `PrepareRequest{generation,input,stream_index,workspace}` / `PrepareOutcome{Ready,Failed}`
  / `PreviewEvent{generation,outcome}` as designed. A sidecar is `cue::read_srt` with **no
  ffmpeg at all**; an embedded track is
  `ffmpeg -v error -nostdin -y -i {media} -map 0:{index} -c:s copy {workspace}/cues.srt`,
  the **absolute** ffprobe index, asserted against the full argument vector.
- **`PreviewHandles` bundles the request channel with a shared `Arc<AtomicU64>`** instead of
  handing `App` a bare `Sender`. Sending *is* what makes a generation live, and every other
  write to the cell means "abandon". That is what lets the worker early-out before spawning
  and kill a running extraction the moment the page closes — a full container demux nobody
  is waiting for otherwise. Step 5's frame worker wants the same cell.
- Worker is FIFO (`spawn_probe_worker`'s shape minus the coalescing, which has nothing to
  collapse at one request per page opening), and shuts down when its results have nowhere
  left to go.
- `run_cancellable` is the pared-down `edit::run_cancellable_output`: same piped-reader
  threads and 25 ms poll, no progress reporting, and **no `EditError`/seconv/OCR reach** —
  a read-only preview must never write `~/.cache/reel-tui/edit_errors.log`.
  Its `try_wait` error arm was folded into the give-up path rather than given an
  untestable message of its own: `try_wait` fails only when something else reaped the
  child, and nothing here installs a `SIGCHLD` handler.
- `App::new`'s signature is unchanged: `set_preview_handles(Option<PreviewHandles>)`, in
  the `set_completion_notification_sender` style. `main.rs` drains with
  `dirty |= app.receive_preview_events(&preview_rx);`; the harness `pump` does the same —
  and dropping that line from the harness was one of the proven breaks.

**Coverage: `preview.rs` 100% line + 100% branch on production code** (every remaining
uncovered line and branch in the file is a `panic!` arm inside its own `#[cfg(test)]`
module). All new `app.rs` branches covered both ways. **25 deliberate breaks all proven.**
874 unit tests pass; fmt and clippy clean; `cargo install --path .` has been run.

Both e2e scenarios landed here rather than one, since Step 4 is what makes cue loading a
user-visible behavior. Scenario 1 asserts everything except the frame; Step 5 extends it.

## Step 5 — frame preview (`ratatui-image`) — DONE

`Protocol`, `Picker` and `DynamicImage` are all `Send` (asserted, permanently, in
`preview.rs`), so the protocol is built on the worker thread as designed and the fallback
of shipping a `DynamicImage` to the UI thread was never needed.

- **Dependencies** exactly as planned, `default-features = false` — `cargo publish
  --dry-run` passes, which is what proves no `chafa-dyn`/pkg-config crept in.
- **`Picker::from_query_stdio()` in `main.rs` before `ratatui::run`**, falling back to
  `Picker::halfblocks()` rather than the planned `from_fontsize((8, 16))`: that
  constructor is `#[deprecated]` since 9.0.0 and would fail `-D warnings`. Halfblocks is
  its recommended replacement and is what the e2e harness uses.
- **Frame command** as planned: `-ss` before `-i`, `subtitles` before `scale`,
  `current_dir(workspace)` plus the bare `cue.srt`, one retimed cue covering
  `00:00:00,000 --> 00:10:00,000`. No escaper exists anywhere, and a test asserts the
  working directory and the whole argument vector together.
- **Coalescing** is `fn newest<T>(request, &Receiver<T>) -> T`, extracted so the
  discard-all-but-the-last behavior is asserted directly rather than raced against a
  worker thread.
- **`FrameOutcome`/`PreviewEvent`** share the prepare worker's single channel, so
  `main.rs` and the harness keep one drain and cannot pump one worker and not the other.
- **Debounce** is `SubtitleSyncState::take_due_frame_request`, which *consumes* the
  request — "asked for" and "waiting to ask" can never both be true, so one settled
  selection is exactly one `ffmpeg`. Everything that invalidates a frame (cues arriving,
  selection moving, the pane being resized) goes through the same 120 ms gate.
- **libass probe** as planned: `ToolCapabilities::ffmpeg_filters` from `ffmpeg -filters`
  with its own parser (the filter listing's flag column carries none of the letters the
  encoder parser keys on, and its legend is printed in entry shape), plus
  `can_burn_subtitles()`. Only 6 struct-literal sites needed touching, all in tests.
- **The frame is keyed to its cue**: `state.frame()` returns `None` the instant the
  selection moves, so a stale picture never sits under a fresh cue — which on a *timing*
  page would read as the burn-in being wrong.

Three things the plan did not anticipate, all found by tests:

1. **A missing duration made every seek zero.** `open_subtitle_sync` deliberately tolerates
   an unparseable duration, and clamping the midpoint against that zero previewed the
   media's first frame for every cue in the track. The clamp now applies only when the
   duration is actually known.
2. **`Image` draws nothing at all — not even clipped — when the protocol is larger than
   its area.** A frame encoded before the pane shrank would therefore blank the preview
   rather than fall back, so the renderer checks the size and lets the text path take it.
3. **Encoding for a zero-cell area trips a `debug_assert!` inside `ratatui-image`** and
   takes the worker thread with it. `start_pending_preview`'s "has the renderer measured
   the pane yet" guard is load-bearing, not merely an optimization.

**Coverage: `sync.rs` 100% line + 100% branch; `preview.rs` 97.14% line, and every
uncovered line and branch in it is inside its own `#[cfg(test)]` module except one arm** —
`picker.new_protocol`'s `Err`, which only the sixel/kitty/iTerm2 encoders can produce and
no test can reach, since halfblocks is the only protocol usable without a real terminal
and it does not fail. **19 deliberate breaks all proven** (C1–C19), including the two the
plan named: the frame worker ignoring `cue_index`, and `start_pending_preview` dropped
from the harness `pump`. 904 unit tests pass; fmt, clippy, `cargo publish --dry-run` and
`cargo install --path .` are all clean.

## Step 6 — final gate

Run once at the end of the preview work. **Everything here that runs a check has to run
again immediately before the branch actually merges** — the gate is only worth what the
tree it ran against is.

- `keybindings_text()` entry — **already done in Step 2**.
- `AGENTS.md` module list — **done**: `cue.rs`, `preview.rs` and `sync.rs` added, and the
  five it already omitted (`cli.rs`, `config.rs`, `notification.rs`, `requirements.rs`,
  `staging.rs`) backfilled.
- **Delete this file** — *not* done, and deliberately: more work is planned on this branch,
  and this is its handoff. Delete it in the commit that makes the branch merge-ready.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test` — clean.
- `cargo publish --dry-run` — passes; this is what proves the chafa-free feature set.
- Full e2e suite — **31 scenarios, all passing**. The run that matters: every pre-existing
  scenario now spawns the preview workers and drains their channel through
  `Harness::pump`, and this was the first time all of them exercised that together.
- Merged coverage:
  ```sh
  cargo +nightly llvm-cov clean --workspace
  cargo +nightly llvm-cov --no-report --branch
  cargo +nightly llvm-cov --no-report --branch --test e2e
  cargo +nightly llvm-cov report --branch --summary-only
  ```
- `cargo install --path .`

## E2E scenarios — both written in Step 4

Both live in `tests/e2e.rs` and pass. One fixture addition, additive:
`SubtitleSpec::cues(&'static str)` overrides the builder's one-cue-per-track default
(`fixtures.rs`). The sidecar is written by the scenario itself with `fs::write` rather than
kept back from the builder — the scenario needs to control its cue text anyway, and that is
less machinery than teaching `write_media` to persist one.

1. **`the_subtitle_timing_page_should_load_cues_for_embedded_and_sidecar_srt_tracks`** —
   one fixture, 3 cues with one overlap, carrying both an embedded `subrip` track and a
   `clip.eng.srt` sidecar, so a single build covers both prepare paths. Open → `c` → wait
   for cues → assert count, `lane_count == 2`, text + timestamp + `▸` marker on screen →
   wait for the frame → assert the pane holds more than one `Rgb` colour → `j` → marker
   follows, and so does the frame → Esc → layer, state cleared, workspace gone → `l` →
   `c` on the sidecar. The fixture is 320x240 and six seconds long because a seek past the
   end of the media produces no frame at all.
   Note `l`, not `j`: with an embedded track *and* a sidecar the subtitle rows are drawn as
   two columns, and `j` deliberately stays inside its column — walking with `j` spins
   forever, which is what the first run of this scenario did.
   Proven by: `MAX_LANES = 1`; removing `close_subtitle_sync` from `back()`; dropping
   `receive_preview_events` from the harness `pump` (times out, exactly the trap the plan
   called out).
2. **`the_subtitle_timing_page_should_refuse_formats_other_than_srt`** — `c` on an `ass`
   track and on the VobSub fixture; asserts layer still `Streams`, state `None`, and the
   notice naming the format both in state and on screen. Files are visited in alphabetical
   order with an Esc between them, because `Harness::open` only ever walks the file panel
   downward. Proven by dropping the format guard.

Harness: `pump` gained `receive_preview_events` and `start_pending_preview`;
`Harness::start` calls `spawn_preview_workers(Some(Picker::halfblocks()))` and
`set_preview_handles`. `Harness::preview_shades` collects the distinct `Rgb` backgrounds on
screen — nothing else in this UI paints one, so more than one means a decoded picture
rather than a blank pane or a solid fill.

**What `TestBackend` cannot cover** — state this in the final change summary rather than
papering over it: its `Buffer` stores symbol+style per cell and does not expose the backend
writer, so **kitty/sixel/iTerm2 escape sequences are unobservable**. The halfblocks path
*is* meaningfully assertable — it writes `▀` cells with real fg/bg colors, so tests can
prove the pane is not blank, that more than one color appears (a decoded image rather than
a solid fill), and that scaling preserves aspect.

## Remaining risks

1. ~~**`Protocol: Send`**~~ — confirmed `Send`; asserted permanently in `preview.rs`.
2. ~~**`Picker::from_query_stdio()` before raw mode**~~ — done in `main.rs` before
   `ratatui::run`. Never call it from a test; it queries the real terminal and hangs.
3. ~~**`chafa-dyn`**~~ — `default-features = false` is in place and `cargo publish
   --dry-run` passes.
4. **Frame-grab latency on network mounts** — accurate seek over NFS is a full read from
   the preceding keyframe. Debounce, coalescing, early-out and kill-on-exit all help;
   beyond that the honest answer is an indeterminate loader. This is the one place the
   feature can *feel* broken.
5. **`Layer` comparison semantics** — the two known landmines (`input.rs` `R`,
   `ui.rs:324`) are fixed. If another `layer != Layer::Files` gets added later, re-audit.
6. **Subtitle column navigation in e2e** — `Harness::select_track_row` walks with `j`/`k`
   and loops forever on a row `j` refuses to leave. With side-by-side subtitle columns that
   is any subtitle row, so cross-column moves must use `l`/`h`.
