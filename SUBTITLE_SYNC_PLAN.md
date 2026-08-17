# Subtitle sync preview — remaining work

> **Delete this file before merging to `main`.** It is a working handoff for an
> in-progress branch, not documentation of the finished feature.

Handoff for the `subtitle-sync-preview` branch. Steps 0–3 are done and sitting in the
working tree (uncommitted); steps 4–6 are not started. The full original design is at
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
Pressing `c` on an SRT track opens the page and it sits on **"Reading cues…" forever** —
nothing populates the cues yet. That is Step 4. The page is otherwise correct: Esc/`q`
leaves it, the workspace is created and deleted, and non-SRT tracks are refused properly.

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

## Step 4 — prepare worker (`src/preview.rs` new, `main.rs`, harness)

First `cargo run`-verifiable milestone, and **shippable on its own** with a text-only
preview pane if Step 5 has to be cut.

```rust
pub struct PrepareRequest {
    pub generation: u64,
    pub input: PathBuf,             // media file, or the sidecar itself
    pub stream_index: Option<u64>,  // Some => embedded; None => .srt sidecar, read directly
    pub workspace: PathBuf,
}
pub enum PrepareOutcome { Ready(Vec<Cue>), Failed(String) }
```

Two cases only:
| Source | Action |
|---|---|
| `.srt` sidecar | `cue::read_srt(path)` — **no ffmpeg at all** |
| embedded `subrip` | `ffmpeg -v error -nostdin -y -i {media} -map 0:{index} -c:s copy {workspace}/cues.srt` |

Note `-map 0:{index}` uses the **absolute** ffprobe stream index (`SubtitleSource::Embedded`
carries that), matching `edit.rs`'s convention — never `0:s:N`.

- **Must be async.** `-c:s copy` still demuxes to EOF: 1–3 s on a local 8 GB MKV, tens of
  seconds over NFS. Blocking `handle_key` freezes the terminal with no way to cancel.
- Worker shape: copy `spawn_probe_worker` (`src/probe.rs:254-279`). Prepare thread is FIFO.
- **Do not reuse `edit.rs`'s private `extract_subtitle`/`convert_subtitle`** — they drag in
  `SubtitleChange`/`ProgressReporter`/`EditError`/seconv/OCR, and routing a read-only
  preview through the Save pipeline risks preview failures landing in
  `~/.cache/reel-tui/edit_errors.log`, which AGENTS.md designates as the first place to
  look for *edit* regressions. Write ~30 lines of dedicated command construction plus a
  local `run_cancellable` mirroring `run_cancellable_output` (`edit.rs:4457`) minus
  progress reporting (~45 lines). Bounded, deliberate duplication.
- `App::receive_preview_events(&Receiver<PreviewEvent>) -> bool`, following
  `receive_probe_results` (`app.rs:1777`). Drop events whose `generation` ≠ the live
  state's.
- **Do not change `App::new`'s signature** — 10+ test sites construct it positionally.
  Make the sender `Option<Sender<_>>` defaulting to `None` plus a
  `set_preview_senders(...)` setter, in the `set_completion_notification_sender` style
  (`app.rs:1245`, used at `main.rs:37`).
- `main.rs`: `dirty |= app.receive_preview_events(&preview_rx);` beside the other drains.
  **The `dirty |=` trap**: omit it in either `main.rs` or `tests/e2e/harness.rs::pump` and
  results arrive but never paint, and the e2e times out on a screen dump that looks fine.
  Precedent regression test at `app.rs:9236`.
- **Progress reporting**: AGENTS.md's Edit Progress Contract is scoped to Save workflows.
  This is not one. Indeterminate loader only; **add no `EditPhase` variants**.

Run e2e scenario 2 here (it needs no frame grabbing).

## Step 5 — frame preview (`ratatui-image`)

Highest risk, deliberately last and fully isolated.

**First action:** `fn assert_send<T: Send>(){} assert_send::<Protocol>();` If `Protocol`
isn't `Send`, send `DynamicImage` from the worker and build the protocol on the UI thread
instead (trivial for halfblocks, a few ms of base64 for kitty).

```toml
ratatui-image = { version = "11.0.6", default-features = false, features = ["crossterm", "image-defaults"] }
image = { version = "0.25", default-features = false, features = ["png"] }
```
Verified: 11.0.6 depends on `ratatui ^0.30.1` (this project is on 0.30.2) and
`crossterm ^0.29` (matches). **`default-features = false` is non-negotiable** — the default
set includes `chafa-dyn`, which pulls a system C library via pkg-config and breaks CI,
`cargo publish --dry-run`, and any user without libchafa. Sixel/Kitty/iTerm2/halfblocks are
all built in without it.

`main.rs`, beside `check_ffmpeg_suite()` and **before** `ratatui::run` enters raw mode:
```rust
// Queries the terminal for its graphics protocol, which needs cooked mode and a clean
// stdin — so it must run before `ratatui::run` enters raw mode and the alternate screen.
let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)));
```
Never call `from_query_stdio` in a test — it queries the real terminal and hangs.

The frame command, run with `Command::current_dir(&workspace)`:
```
ffmpeg -v error -nostdin -y -ss {midpoint} -i {media}
       -map 0:v:0 -frames:v 1
       -vf "subtitles=cue.srt,scale={W}:{H}:force_original_aspect_ratio=decrease"
       -f image2pipe -vcodec png -
```
- **Escaping is eliminated, not solved.** `subtitles=` values need three layers of quoting
  (filtergraph `[],;`, filter-arg `:`, option-parser `'` and `\`). Running in the workspace
  and passing the bare relative name makes the value a constant safe string. Write no
  escaper; write a test asserting `current_dir` + bare filename.
- **A retimed one-cue SRT guarantees the burn is visible.** Write `workspace/cue.srt` with
  just the selected cue at `00:00:00,000 --> 00:10:00,000`. With `-ss` *before* `-i`,
  ffmpeg seeks accurately and resets output timestamps to ≈0, so the cue burns onto
  whatever frame emerges regardless of frame rounding or container `start_time`.
  Reject `-copyts`: it breaks on non-zero `start_time` (MPEG-TS routinely starts at 1.4 s
  or 10 s) and makes libass re-index the whole SRT every grab.
- Seek to `cue.midpoint()`, clamped to `duration - 200ms`.
- Filter order `subtitles` then `scale` — burn at source resolution so libass lays out
  against the source `PlayRes`, then downscale.
- Build the `Protocol` **on the worker thread**; render with the stateless `Image` widget.
  `StatefulImage` blocks on resize/encode inside `ui::render`.
- Coalescing worker (`while let Ok(newer) = rx.try_recv() { request = newer; }`), 120 ms
  debounce via `start_pending_preview` mirroring `start_pending_probe` (`app.rs:1754`),
  shared `Arc<AtomicU64>` generation early-out before spawning ffmpeg, and an `AtomicBool`
  polled in `run_cancellable`'s 25 ms loop so leaving the page kills a slow network seek.
- **libass probe**: extend `ToolCapabilities` (`subtitle.rs:393`) with
  `ffmpeg_filters: BTreeSet<String>` from `ffmpeg -hide_banner -filters`, memoized by the
  existing `detect_cached()` `OnceLock`, plus `can_burn_subtitles()`. 17 struct-literal
  sites but 8 use `..Default::default()`, so churn is small. Verified present locally
  (n8.1.2, `--enable-libass`) and in CI (BtbN GPL static builds). The no-libass fallback
  shares its path with the no-`Picker` fallback, so it stays exercised.

Run e2e scenario 1 here.

## Step 6 — final gate

- `keybindings_text()` entry — **already done in Step 2**.
- `AGENTS.md` module list: add `cue.rs`, `preview.rs`, `sync.rs`, and backfill the five it
  already omits (`cli.rs`, `config.rs`, `notification.rs`, `requirements.rs`, `staging.rs`).
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- `cargo publish --dry-run` — specifically proves the chafa-free feature set.
- Full e2e suite **once**, as the pre-merge gate.
- Merged coverage:
  ```sh
  cargo +nightly llvm-cov clean --workspace
  cargo +nightly llvm-cov --no-report --branch
  cargo +nightly llvm-cov --no-report --branch --test e2e
  cargo +nightly llvm-cov report --branch --summary-only
  ```
- `cargo install --path .`

## E2E scenarios (Steps 4 and 5)

Two fixture additions, both additive so no existing scenario changes:
- `srt_body` (`tests/e2e/fixtures.rs:407`) writes exactly one cue spanning the whole
  duration, so it cannot exercise lanes or `j`. Add an optional `SubtitleSpec.cues`
  defaulting to today's single cue.
- The builder deletes its temp `.srt` files after muxing (`fixtures.rs:262-263`); scenario
  1 needs one *kept* as a persisted `{stem}.{lang}.srt` sidecar matching
  `parse_sidecar_for_media`'s naming rules (`subtitle.rs:734`).

1. **`subtitle_sync_page_previews_cue_timing_for_srt_tracks`** — one fixture, 3 cues with
   one overlap, carrying both an embedded `subrip` track and a `.srt` sidecar so a single
   build covers both prepare paths. Open → `c` → wait for Ready → assert cue count,
   `lane_count == 2`, text + timestamp on screen → `j` → assert frame and highlight
   followed → Esc → assert layer, state cleared, workspace gone → repeat `c` on the sidecar
   row. Prove by: removing `close_subtitle_sync` from `back()`; making the frame worker
   ignore `cue_index`; setting `MAX_LANES = 1`.
2. **`subtitle_sync_page_refuses_formats_other_than_srt`** — press `c` on an `ass` track
   and on the existing VobSub fixture (`fixtures.rs:293`); assert layer still `Streams`,
   state `None`, notice names the format. Runs no ffmpeg, nearly free. Prove by dropping
   the format guard.

Harness: `pump` gains `receive_preview_events` + `start_pending_preview`; `Harness::start`
calls `spawn_preview_workers(Some(Picker::halfblocks()), …)`.

**What `TestBackend` cannot cover** — state this in the final change summary rather than
papering over it: its `Buffer` stores symbol+style per cell and does not expose the backend
writer, so **kitty/sixel/iTerm2 escape sequences are unobservable**. The halfblocks path
*is* meaningfully assertable — it writes `▀` cells with real fg/bg colors, so tests can
prove the pane is not blank, that more than one color appears (a decoded image rather than
a solid fill), and that scaling preserves aspect.

## Remaining risks

1. **`Protocol: Send`** — verify first thing in Step 5; contained fallback exists.
2. **`Picker::from_query_stdio()` before raw mode** — inside `ratatui::run`'s closure it
   hangs or corrupts the first frame.
3. **`chafa-dyn`** — forgetting `default-features = false` breaks CI and `cargo publish`.
4. **Frame-grab latency on network mounts** — accurate seek over NFS is a full read from
   the preceding keyframe. Debounce, coalescing, early-out and kill-on-exit all help;
   beyond that the honest answer is an indeterminate loader. This is the one place the
   feature can *feel* broken.
5. **`Layer` comparison semantics** — the two known landmines (`input.rs` `R`,
   `ui.rs:324`) are fixed. If another `layer != Layer::Files` gets added later, re-audit.
