# reel-tui

A Rust 2024 TUI for inspecting and remuxing media files with `ffmpeg`/`ffprobe`, and for editing subtitle
tracks against the picture. The library `reel_tui` (`src/lib.rs`) holds everything; the binary `reel`
(`src/main.rs`) is a thin shell over it so integration tests drive the same code.

Rules in **bold** are load-bearing: each one exists because the obvious alternative was built and broke.
Do not undo one without re-reading why.

## Inbox

`INBOX.md` in this directory (git-ignored, may not exist) holds messages left for this project by other
sessions, mostly the workspace hub (`~/projects/workspace`) after it indexes the projects. At the start of a
session, if it exists, read it and tell Bas briefly what's in it. Act on a message only when Bas agrees;
once a message is handled or dropped, delete it, and delete the file when it's empty.

Other sessions only append to `INBOX.md`, never change anything else here. Each message gets a
`## YYYY-MM-DD, from <session>` heading, then a few plain lines: what, why, and where to look.

## Commands

- `cargo run` — launch against the current directory (needs `ffmpeg`/`ffprobe` in `PATH`).
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` — the ordinary checks.
  Clippy's `--all-targets` skips `tests/e2e.rs` (`test = false`); lint it with `cargo clippy --test e2e -- -D warnings`.
- `cargo test --test e2e <scenario> -- --exact` — one e2e scenario, only while writing or changing it.
- `cargo test --test e2e` — the full e2e suite, only as the final gate before merging a branch.
- **`cargo install --path .` after every change that passes**, so `~/.cargo/bin/reel` is current.
- `cargo publish --dry-run` — validate packaging.

## Modules

- `main.rs` — entry, worker spawning, terminal setup, event loop. Thin: anything testable goes in the library.
- `cli.rs` — `--help`, `--version`, target directory, handled before the terminal is touched.
- `config.rs` — `config.toml`: worker pools, notifications, `[preview] cache_tracks`/`cache_megabytes`,
  `[preview.prefetch] enabled`/`network`, `[preview.playback] fps`/`pad`. `pad` goes through
  `Duration::try_from_secs_f64` (`from_secs_f64` panics on a negative) and NaN is rejected *before* the clamp
  (`f64::min` returns the other operand for NaN). `MIN_PLAYBACK_FPS`/`MAX_PLAYBACK_FPS`/`MAX_PLAYBACK_PAD` are
  public so the preview-settings popup stops at exactly the same limits.
- `requirements.rs` — external tool floors (below).
- `input.rs` — `handle_key`: `KeyEvent` → `App` mutation.
- `app.rs` — state, layers, probe/edit event dispatch, probe cache, `PreviewSettings`.
- `staging.rs` — `StagedEdit`, `BatchState`.
- `ui.rs` — ratatui rendering.
- `files.rs` — directory scan, file tree, adaptive filesystem monitoring.
- `probe.rs` — async `ffprobe` worker, metadata parsing, probe flags, `is_chapter_track`.
- `edit.rs` — async `ffmpeg` remux, reordering, conversion, progress, cancellation, cross-filesystem publish
  (`move_or_copy_file`).
- `subtitle.rs` — subtitle inspection, sidecar matching, conversion, language tags, `ToolCapabilities`
  (decoders tracked apart from encoders: the edit page reads, the rest writes), `preview_blocked`,
  `CueChanges`/`CueEdit`/`CueInsert`/`CueSnapshot`, `rewrite_srt_cues`.
- `cue.rs` — SubRip/ASS parsing, folding, overlap groups, timeline lanes and windows, time formats. No `App`,
  `ratatui` or subprocesses.
- `subtitle_edit.rs` — `SubtitleEditState`: the subtitle edit page's cues, groups, selection, focus, frame
  window, background pass, playback.
- `preview.rs` — the page's four workers: cue extraction, still frames, background warm pass, scrub playback.
  Does **not** go through `edit.rs`, so a read-only preview never writes `edit_errors.log`.
- `framecache.rs` — on-disk preview frame cache.
- `audio.rs` — playback sound and the clock the picture follows.
- `notification.rs` — desktop notification when a save finishes unfocused.
- `mount.rs` — network mount detection (`/proc/mounts`), `REEL_NETWORK_MODE` override.
- `cache.rs` — probe cache, `$XDG_CACHE_HOME/reel-tui/probe_cache.json`.

Unit tests sit beside their code in `#[cfg(test)] mod tests`. E2E lives in `tests/e2e.rs` with helpers in
`tests/e2e/`. Releases: `.github/workflows/publish.yml`.

## Coding Style

rustfmt defaults, Clippy clean, standard Rust naming. Small focused functions; `anyhow::Result` for
recoverable failures. Keep terminal cleanup guaranteed and blocking `ffmpeg`/`ffprobe` work off the event loop.

**No inline control help text.** Never add a hint line ("↑↓ select · Enter apply") to any dialog, popup or
view. Keybinding help lives only in the global keybindings popup (`?`, `Dialog::Keybindings`/`keybindings_text()`
in `src/ui.rs`); extend `keybindings_text()` for keys it does not cover.

## Testing

**Tests never skip**: no `#[ignore]`, no early returns, no environment-based passes. Missing programs, codecs or
language data fail immediately with an actionable message. **Write tests that would notice**: assert the
observable result, not that something ran. Name tests by behaviour (`cancelled_edit_preserves_original`); use
temp paths and deterministic JSON fixtures.

**Never resolve the real cache directory in a test.** `DiskCache::cache_dir()` returns throwaway storage under
`cfg(test)`, and the e2e harness redirects `XDG_CACHE_HOME`. Both are load-bearing: without them a test run
rewrites `~/.cache/reel-tui/probe_cache.json` and fills `edit_errors.log` with deliberate failures — the log
you are told to read first.

### E2E suite

`tests/e2e.rs` replays `main.rs`'s loop in-process: real `handle_key`, real `ui::render` into a `TestBackend`,
real `ffprobe`/`ffmpeg` on real files. Nothing is mocked except crossterm's terminal setup and byte decoding.

- Separate target with `test = false`; see Commands for when it may run.
- **Every new or materially changed behaviour needs an overarching e2e scenario**, on top of unit tests.
  Pure styling/layout needs focused render/unit coverage instead. Missing coverage of an area you touch is debt
  to pay as part of that work.
- A regression from `~/.cache/reel-tui/edit_errors.log` quotes the log line in its doc comment. That log is the
  first place to look for edit regressions.
- Fixtures (`tests/e2e/fixtures.rs`) are `lavfi` sources parameterised by codec and container; the realism is
  the point (`subrip` into MP4, `mov_text` into Matroska).
- **Scenarios that render preview frames take `harness::frame_cache_lock()`.** The cache is process-global and
  every page prunes it to whole directories; without the lock scenarios evict each other's frames and hang.
- **Assert editing features on the file on disk** (and on the container for muxed tracks). Every layer short of
  the file agrees while such a feature is broken. Assert picture features on rendered bytes/pixels for the same
  reason.

### Coverage

- Project target ≥ 90% line coverage; watch branch coverage too.
- **New or changed production paths need 100% line and branch coverage from unit tests**; e2e does not count.
  Touching uncovered code means covering what you touched.
- Every bug fix gets a unit regression test that fails before and passes after, reproducing the whole failure
  including effects on neighbouring data; the area also needs an e2e scenario.
- Cover success, failure, cancellation, validation and rollback. Use small real media when a unit test cannot
  verify command semantics.
- **No inflation**: no assertion-free tests, broad exclusions, or tests for unreachable code — delete
  unreachable code instead and say so.
- Report the unit coverage method and totals with each change; merged unit+e2e only at the pre-merge gate. If
  it cannot be measured, say so.

Pre-merge measurement (runs the full e2e suite, so only then):

```sh
cargo +nightly llvm-cov clean --workspace
cargo +nightly llvm-cov --no-report --branch            # unit tests
cargo +nightly llvm-cov --no-report --branch --test e2e # e2e suite
cargo +nightly llvm-cov report --branch --summary-only
```

- Plain `cargo llvm-cov` silently drops the e2e target and understates `edit.rs`.
- Do not point `LLVM_COV`/`LLVM_PROFDATA` at `/usr/bin` (Arch's LLVM is older: `raw profile version mismatch`);
  rustup's `llvm-tools` match.
- **`report` can merge unexecuted objects** (`debug/reel`, `build/*/out/reel`, `deps/reel_tui-*`), halving every
  figure — the giveaway is twice the expected function count and zero branch counters. Check with `--verbose`,
  then run `llvm-cov report` directly on the `out/reel_tui-*` and `out/e2e-*` binaries with the same
  `-instr-profile` and `-ignore-filename-regex`.
- `#[cfg(test)]` branches count too; read per-file trends, not absolutes.

### Edit progress contract

Every blocking subprocess, slow filesystem step, validation, transaction phase, rollback and cleanup in a save
emits a short typed progress phase right before it runs. Measured progress only where the operation exposes
reliable units; otherwise the indeterminate loader, never an estimate. Short labels, basenames, compact codec
names, no "import/export" wording. Changed edit paths need tests for progress sequence, cancellation and
cleanup.

## External Tool Floors

Measured, not guessed (`src/requirements.rs`). Do not lower one without re-measuring.

- **FFmpeg 8.1** (`ffmpeg` and `ffprobe`), enforced at startup. Below it the `mov` demuxer cannot read the
  `name` atom, so MP4/MOV titles read as absent and a remux erases them while `validate_result` passes it.
  Suite verified green on 9.0.1.
- **seconv 5.1.0**, gating conversion/OCR. Its `--version` lies (says 5.0.0), so
  `subtitle::seconv_is_supported` looks for `--no-vobsub-isolate-colors` in `--help`. Needs `libicu`.
- **Tesseract 4.0**, gating OCR. 3.x is unreachable anyway (seconv's glibc).
- **`pkg-config` + ALSA headers** to build (`cpal` → `alsa-sys`). No sound device needed: `audio::open` falls
  back to `SilentOutput`.

CI installs tools from `.github/actions/media-tools`, never distro packages (`ubuntu-latest` has FFmpeg 6.1).
`ci.yml` runs both suites on pushes and PRs to `main`; `publish.yml` runs on release tags only.

## Media Handling

**Chapter tracks are not tracks.** MP4/MOV store chapter titles as a QuickTime `text` stream, which ffprobe
also lists as a `bin_data` stream. `probe::is_chapter_track` identifies it by container tag (not codec —
GoPro `gpmd` is also `bin_data`) plus a non-empty chapter list. It is never mapped (`-map_chapters 0` writes
chapters back itself; mapping it duplicates them and Matroska refuses it outright), and the muxer's own chapter
track is filtered from `validate_result`, `muxer_forced_default_positions` and `apply_edits`' count check. It
raises no container conflict. Real data streams still do (MP4/MOV take them, Matroska does not), and the advice
for `data`/`attachment` names the containers that would take them.

**Adaptive filesystem.** Network mounts (NFS, SMB, SSHFS, Rclone, Ceph…) are detected: reconcile every 10 s
instead of 1 s, probe with `-probesize 2000000 -analyzeduration 3000000`, remux into
`/tmp/reel-tui-scratch` and publish once. The probe cache is keyed by path, length and mtime.
`REEL_NETWORK_MODE=1`/`0` forces it.

## The Subtitle Edit Page

Opened on a subtitle track; judges a subtitle against the picture it is burned into. Two panes share one
cursor: the **cue panel** (a selection) and the **timeline** (a moment), plus the preview pane and one status row.

### Reading formats

Every format ends as a file `cue.rs` parses; `preview::prepare` dispatches, `ToolCapabilities::preview_blocked`
refuses before the page opens (so a missing tool reads as a refusal naming it, not a page that loads and fails).

| Format | Road |
| --- | --- |
| SubRip | `-c:s copy`, or the sidecar as is. `cue::parse_srt`. |
| ASS | `-c:s copy`, **never transcoded**. `cue::parse_ass`. |
| WebVTT, MOV Text | Transcoded to SubRip (`-c:s subrip`, or a second pass for a sidecar). Needs a decoder. |
| TTML, PGS, VobSub | Refused (TTML has no FFmpeg decoder; PGS/VobSub are pictures). |

ASS keeps what a transcode would discard: `Cue::dialogue` (original `Dialogue:` lines, a `Vec` because of
folding — `Cue::text` is the stripped reader's version), `CueStyle::Ass { header, events_format }` (field order
is per file, so parsing and `cue::retimed_dialogue` read the `Format:` line), and `CueSlot`, the staged
filename — **libass picks its reader by extension**, so ASS staged as `.srt` draws its tags as text. Likewise
**the staged filename and the extraction's `-c:s` must agree** (`extracted_format`), or the muxer refuses.

**One visible line is one cue** (`cue::collapse`). Typeset/karaoke lines are many events with identical start,
end and text; both parsers sort, fold *adjacent identical* events, then number. Nothing looser — real karaoke
differs per syllable. Every member's `Dialogue:` line is kept and staged. `Cue::events` counts members; the panel
shows `×4`, dropped entirely when it and the timing do not both fit (ratatui lets the right title paint over the
left).

### Frames

**A frame is the whole screen.** A burn stages every cue on screen: `cue::on_screen_at` (still) or
`on_screen_between` (playback). The anchor cue is always included (the still is grabbed just inside the
media's end, which can fall outside a last cue). The cache key covers the company (`FrameSource::key(cue,
on_screen)`). **The warm pass builds groups from the whole list, never its slice** (`WarmSlice::offset`), or
a slice boundary renders a different picture under the same key.

**A still is grabbed where the cue comes in** (`preview::seek_for`), not its midpoint — arriving late is only
visible at the start. Safe because the still retimes every staged cue across the burn window. Playback instead
shifts all cues by the same amount (`stage_span`), keeping their staggering; a cue starting before the span is
clamped to it.

Grab: `mjpeg -pix_fmt yuvj420p -q:v 2` (**the pixel format is required**: mjpeg refuses limited-range YUV), at a
size from the source capped by `MAX_FRAME_PIXELS` (1080p), not the pane's size.

**Warm workers each stage under their own name** (`warm_cue_file(n)`, separate from `CUE_FILE`); a shared file
burns another cue's line under the right key, visible only in the bytes. Worker count is half the cores, min 1,
max `MAX_WARM_WORKERS` (3).

**Frame cache** (`framecache.rs`): `$XDG_CACHE_HOME/reel-tui/preview_frames/{media_key}/{cue_key}.jpg`, 128-bit
FNV-1a, JPEG (~90 KB vs ~2 MB PNG). Directory via `DiskCache::cache_dir()`, never the environment.

- `media_key` = the video stream's identity (`preview::video_identity`: codec, tag, profile, geometry, pixel
  format, frame rate, duration, bit rate, extradata size), render size, `CACHE_FORMAT_VERSION`. **It asks "same
  picture", not "same file"**: a save is a remux, which copies all of these, so frames survive saves with no
  bookkeeping. **Duration is the stream's** (or its Matroska `DURATION` tag), never the file's — FFmpeg 8.1
  moves an AAC track's end by milliseconds on remux. It describes the chosen stream
  (`preview::video_stream_index`), and `pixels` follows that stream (`preview::video_resolution`).
  **`migrate`, `frames_survive_edit`, `App::move_frame_cache` and `pending_frame_moves` were deleted — do not
  reintroduce them.**
- `cue_key` = the staged file's bytes (what libass gets) plus timing — not the text, since two ASS cues with the
  same words draw differently. Edits simply miss. `moment_key` (tagged `MOMENT_TAG`) is the timeline cursor's
  key: instant plus staged on-screen file, `None` hashing like an empty file.
- **Eviction is by whole media directory**: `prune(tracks, bytes, open)`, LRU by the `.used` marker's mtime
  (`touch`), never the open one. `bytes` is a second bound spent the same way. **Never evict single frames** —
  a directory holds every track of a film, and a partial track is re-rendered on every visit.
- Bump `CACHE_FORMAT_VERSION` when a stored frame changes meaning.
- Defaults: 10 tracks, 2048 MB. Off by default on network mounts, and the status row says so. The panel border
  shows `[400/1200 · 400 new]`, rendered vs found.

**The in-memory window.** The selected cue and up to `NEARBY_FRAMES` either side are kept encoded, so moving
draws immediately. **`window_radius()` is budgeted** against `preview::FRAME_WINDOW_BUDGET` (a `Protocol` holds
terminal bytes: base64 RGBA for kitty/iTerm2), floored at 1. `prune_frames`, `nearby_frame_targets` and
`has_missing_nearby` must use the same radius. `FRAME_DEBOUNCE` holds back only a cue that must be rendered —
a cached one is asked for at once — and **only that cue, not the dispatch**, so its cache-only neighbours still
go (`clear_nearby_request` defers it, it is not answered). `apply_warming` refills only on `has_missing_nearby()`
(warm slices run in parallel, so `done` is not a prefix). `preview::newest_frame` carries a coalesced-away
`wanted`/`scrub` forward onto the request that replaced it, or the pane stays empty.

**Permanent vs per-cue failures are split.** Something that cannot change while open (no libass, no image
protocol) is a `PreviewSupport` reason in the pane; a single cue's failure is `frame_error()` on the status row,
keyed on the selected cue. The pane never shows the cue's text instead of a picture (it flashed on every move).

**Previews need a real image protocol.** `preview::drawing_picker` discards a `Halfblocks` picker in `main.rs`
(`Picker::from_query_stdio` reports halfblocks for every silent terminal), so no frame, warm or playback worker
runs and the pane says `PreviewSupport::NoImageProtocol`. Kitty, iTerm2 or sixel terminals work. The e2e harness
deliberately uses halfblocks anyway (`Harness::start`), the only protocol `TestBackend` records as cells;
`Harness::start_without_image_protocol` covers the refusal.

### Scrub playback

`p` plays a span. One `ffmpeg` decodes it to raw `rgb24` with the sound as a second output of the same pass.

- **`scale` is a literal `W:H`, no `force_original_aspect_ratio`**: output is sliced by `w*h*3`, and a short frame
  shears everything after it. Proportions come from `playback_cells`.
- The cue is staged at its place *inside* the span (`CueStyle::stage_between`).
- **Sound goes to a file, not a second pipe** (two pipes deadlock). A run with no sound (muted, no audio) must
  clear that file before and skip reading it after, or the previous cue's sound plays.
- Never cached. Frame rate gives first under `PLAYBACK_BUDGET` (`affordable_fps`).
- **Speed is applied in that one decode**: `setpts` *after* `subtitles` (libass uses source timestamps) and
  *before* `fps`; `atempo=0.5,atempo=0.5` for quarter speed (`atempo=0.25` is refused); the budget is charged
  `span / speed`. At normal speed neither filter is emitted.
- **`source_capped_fps` runs before the budget** (the `fps` filter duplicates frames above the source's rate),
  scaled by speed, skipped when ffprobe states no rate. The popup's list is capped the same way
  (`App::playback_fps_choices`, `effective_playback_fps` merged into it) and shortens as speed drops; the row is
  never disabled, and the *setting* is untouched by the cap.
- **Decoded at `playback_pixels`, the terminal's own cell size**, then drawn through the picker — the only way
  burned-in text is readable. No cell/colour fallback, ever.
- **`playback_cells` divides by the cell's aspect ratio and `playback_pixels` multiplies it back; they must
  cancel**, both from the font, not a constant 2. `pixels`, `cells` and `picker` travel together in `SpanShape`.
- `PlaybackSpeed` and every list in the settings popup run descending, data and display alike.

**Audio is the clock** (`audio.rs`): the frame index comes from where the device is (`frame_index_at`).
**Counting written samples is wrong** — it leads the sound by the device latency and looks like an early
subtitle. `PlaybackClock::anchor` uses cpal's `playback`−`callback` lead and re-anchors each callback.
`PlaybackClock` names no audio library so it is testable without hardware. `SilentOutput` is production (no
device, no audio track, muted). `AudioSource` exists for looping: a repeat opens a new output, no new `ffmpeg`.
`span_slice` is a function because `handed_over` counts device time and runs past the end, and `slice[n..n]`
panics there.

`Playback::position` multiplies by speed (frames are output time, the playhead media time). Loops restart at
frame zero; a span with no frames still ends.

**What stops a playback**: the selection moving to another cue, retiming under it (`set_cue_timing`), a new
track, the cursor changing pane. **A pane resize must not** — announcing a playback adds the status row, which
shrinks the preview pane, so it would stop itself; frames are re-encoded for the new size instead.

**No dialog over a running or decoding playback** (`App::playback_in_progress`): the span is drawn through the
image protocol, not the cell buffer, so a dialog is wiped by the next frame and left open, invisible, eating
keys. User-raised dialogs (`:`, `?`, `D`) are refused; one that raises itself (conflict notice) stops the
playback. **Likewise the still is not drawn while any dialog is up** (`render_edit_preview`): the image widget
marks its cells skipped, so a dialog that shrinks leaves its old border behind. Do not draw the image under a
dialog again; `TestBackend` cannot see the artifact, so
`a_dialog_over_the_preview_should_take_the_picture_down_rather_than_sit_on_it` asserts the rule.

### Preview settings popup (`:`)

Mutates `PreviewSettings` in place for the session and **never writes to disk**. `r`/`R` restore from
`preview_defaults` (what `main` loaded), not `Config::default()`. It holds only a cursor, so it always shows
what a playback would use. Reuses `SettingsDialog`: speed, padding, frame rate are `setting_line` +
`dropdown_line`; loop and sound are `toggle_line` (built on `action_option`; `Enter` flips, `h`/`l` *set* left
or right so key repeat settles — bound only there). `choice_style` is the shared "chosen/changed/inert" look.
`K` opens its help (`preview_field_help_text`). A row is marked changed when it differs from `config.toml`.

**Video, Audio and Subtitle track rows** come first, then a blank row (inserted where `PreviewSettingsField::ORDER`
leaves the track rows). Unlike the other five, they are **pending until the popup closes**
(`App::escape_preview_settings` → `apply_preview_tracks`), because a video or subtitle switch rebuilds the
page. **An audio switch rebuilds nothing** — the next `p` uses it (the e2e scenario asserts the generation is
unchanged).

- The choices live on `App`, not the page (the switch destroys the page); cleared in `queue_probe`; resolved via
  `preview::video_stream_index`/`App::preview_audio_stream` with fallback to the first stream.
- Reopen is `App::open_subtitle_edit_on`. The selected cue survives a video switch, not a subtitle switch.
- Unsaved cue edits + subtitle switch → `Dialog::ConfirmLeaveCues` ("switching tracks discards them"),
  `App::pending_track_switch`; staying clears the pending switch.
- `close_subtitle_edit` takes `PageExit::{Leaving, Reopening}`: reopening the *same* track must not delete a new
  empty sidecar; reopening a *different* one is leaving.
- Tracks the page would refuse are listed greyed with the reason (`App::subtitle_choice`,
  `preview_choice_enabled`), skipped by movement, refused by `set_preview_choice`. Cover art is excluded from
  video. An empty audio list reads `[ None ]` and does not open.
- Labels are built in `app.rs`, leading with the absolute index. **Every command maps streams by absolute
  index** (`-map 0:v:0` counted cover art, unlike `video_identity`). `PlaybackRequest::audio` is a
  `preview::AudioTrack`; `SyncRequest` carries the same stream.
- `playback_settings_badge` puts the state in the preview pane's title (`Preview · 0.5x · loop · muted`, led by
  a non-first audio track). Padding, frame rate and video track stay out.

### Cue panel

**Cues that share the screen are one row** (`cue::group_overlaps`, `<` at the boundary like `pack_lanes`; a
run reaches to its furthest end). A lone cue is three rows with full timing; a group is six rows: a fork into
**two blocks, never three** (`GROUP_COLUMNS`; a third could not hold a timing), the later-starting one a row
lower (level when they start together). The fork's crossbar running off the panel edge with a `┬` says there are
more; dropped below five rows of panel. `group_timing` picks compact span (`cue::format_compact`) or start
only, **once for the whole track**.

- `j`/`k` step groups, `h`/`l` move within one and stop at its ends (`select_within_group`). `j` from a later
  member of the last group is refused.
- **Groups are re-entered where they were left** (`group_memory`: cue and page). New groups enter on their
  first member; `gg`/`G` overwrite memory.
- Sideways movement **pages, it does not slide** (`group_window`, `group_page`): the pair holds still while the
  cursor crosses it. **The page is stored, not derived**, because the backed-up last page shares members.
- `cue_scroll` works in rows, not cues.
- **Row state** (`CueRowState`, carried by `CueMarks`): deleted outranks edited. Edited is yellow italic, deleted
  red strikethrough; on the cyan selected row only the modifier survives. No `×` glyph (that slot is the fold
  count).
- **The border's top right** holds the background pass's `[done/total]` while it runs, then
  `staged_cue_counts` (` 2 edited · 1 deleted `, each in its rows' colour, zeros dropped) — not the status row,
  which is for the key just pressed.
- **`App::notice` reaches this page through `edit_status_line`, not `render_footer`** (which this page never
  reaches); it ranks first there, every movement clears it, and `leave_subtitle_edit` clears it.

### Timeline

**Every cue is drawn the same way, whatever the density.** Crowding is fixed by shortening the window
(`TimelineWindow::fitted`, stepping `WINDOW_STEPS` down from a minute until cues fit `READABLE_CUE_COLUMNS`),
never by demoting cues — that was tried and rejected. Never shorter than the selected cue plus half again.
`TimelineWindow::around` is the cue-free twin for an empty track; both use `centred`.

- `axis_tick` scales the interval to the window (`TICKS`, `TICK_TARGET`). **`axis_columns` is the single answer
  to where ticks are**, read by the ruler and by `timeline_lines`' dashed `┊` gridlines (painted first; dashed so
  they differ from the solid playhead/cursor).
- Lanes (`LaneLayout`) are repacked after any retiming; **groups are not recomputed after a retiming** —
  recomputing under a held key reflows the list around a keypress resolved against the old one. They *are*
  recomputed on insert/remove.
- Colours: selection cyan, playhead yellow; in timing mode they swap (`selection_colour`, `playhead_colour`) and
  the `▲` marks follow the selection. At track scale every cue is yellow, the selection bold, and magenta overflow
  marking is suspended. Timeline cursor green.

**Timeline title.** In timing mode it reads the cue's live times plus `format_shift` (`+0.15s`, hundredths,
dropped at zero) and, if the length changed, `2.50s long`; at track scale `global +2.35s`. When it will not fit
(`title_width`), the cue's times go first. Selected grip is bracketed (`ui::cue_times_reading`). With the cursor
in the timeline it reads only `Timeline (00:00:07.20)` (`cue::format_precise`).

### The timeline cursor

`Ctrl+J` moves the cursor into the timeline (`EditFocus`, `App::focus_timeline`), `Ctrl+K` back. `h`/`l` step
`TIMELINE_STEP`, `H`/`L` ten, `Ctrl+H`/`Ctrl+L` `TIMELINE_FINE_STEP` (= `TIMING_STEP`, so the cursor can land on
any timing a nudge reaches). One `move_cursor` with the step passed in. Walking edits nothing; `i` here adds a cue.

- **While the timeline holds the cursor no cue is marked anywhere** — one `Option<usize>` from `state.cursor()`
  goes to both panes. `selected` still exists underneath for `Ctrl+K`, window fitting and the fallback still.
- **A focus, not a mode or layer.** The focus owns movement keys before the timing mode; `j`/`k`, `gg`/`G`,
  `Ctrl+D`/`Ctrl+U` are swallowed by one arm in `input.rs`. Focus changes leave the timing mode alone.
  `Esc`/`q` do not peel it: `App::back` stops a playback, clears a search filter, leaves the timing mode, then
  leaves the page.
- **Re-seeded on every entry** from `seek_for` of the selected cue (`cursor_seed`), so entering requests no grab.
  **The cue's still stands in only while the cursor is on exactly that moment** (`SubtitleEditState::still_frame`);
  elsewhere the pane is empty until the moment's frame lands, never the wrong picture.
- Moment frames: `ScrubTarget`, `FrameRequest::scrub`, `PreviewEvent::ScrubFrame`, `CueSlot::Scrub`; cached in the
  track's directory under `moment_key`; debounced only when uncached (`scrub_request_due`). A moment with nothing
  on screen gets **no `subtitles` filter** (`cue::on_screen_now`, unanchored), since libass rejects an empty
  file. The previous picture stays until the next lands; a failed grab clears it.
- **The window slides, not re-centres** (`TimelineWindow::containing`), and **its start is remembered while the
  timeline has the cursor** (`window_start`, `TimelineWindow::starting_at`, deliberately unclamped), written back
  by `render_edit_timeline` (hence `&mut`). `None` on arrival and on leaving.
- **`p` from the timeline plays the moment**: `CURSOR_PLAYBACK_REACH` either side plus `playback_pad`, burning
  `cue::on_screen_during`; no `subtitles` filter when empty (`video_filters` takes an `Option`). `PlaybackAnchor`
  identifies spans end to end; `anchor_is_current` holds both staleness rules. `focus_timeline` and
  `leave_timeline` stop a playback.

### Editing cues

All cue editing is **SubRip only**, refused as `CUE_EDITS_SUBRIP_ONLY` (ASS cues name styles and anchor `\move`,
`\fad`, karaoke to their timing). All of it is **staged, not written**: `SubtitleChange::cues` (a `CueChanges`)
travels through `staged_edits`, conflicts and the batch; `Ctrl+S` writes it.

- **Addressed by file position, checked against the whole cue.** `CueEdit` keys on position and carries a
  `CueSnapshot` of words *and* timing; `rewrite_srt_cues` refuses if the file's cue there differs. A second visit
  keeps the file's snapshot (`App::file_cue_snapshot`); editing back to the file stages nothing.
- **One entry per cue, storing the result, not the operation** (`App::stage_cue_change`). Insertions are the one
  separate map (they have no file position); `CueChanges::is_empty()` answers for both.
- **Page positions vs file positions**: `SubtitleEditState::origins` (`CueOrigin`, `origin`/`position_of`) maps
  between them once cues are added or removed; never renumber staged keys. `staged_cue_edits` /
  `staged_cue_deletions` answer in page positions.
- **`rewrite_srt_cues` order**: check deletions (against the file, before rewrites change it), apply rewrites,
  drop deletions, append insertions, sort, renumber (`cue::write_srt`). Positions address the file as read, so
  the sort comes last. `checked_cue` holds the three refusals.
- Sidecars are rewritten in place (original listed in `remove`, so it is backed up and not renumbered by
  `resolve_export_duplicates`); embedded tracks are copied out, rewritten, mapped back as a
  `SubtitleReplacement`. Cue edits apply before conversion/import.
- **Leaving the page discards cue edits after asking** (`Dialog::ConfirmLeaveCues`, default
  `LeaveCuesChoice::StayHere`; `App::discard_cue_edits` resets only `cues` and drops an empty change via
  `store_subtitle_change`). This page is the only place they are visible.
- The save confirmation counts cue work per track (`cue_change_summary`): moving (both ends), lengthening,
  shortening, rewording, adding, deleting; rewritten-and-deleted counts as deleted; listed before conversions.
- **A save returns to the page** (`SubtitleEditReopen`, `App::reopen_subtitle_edit`, fired from
  `receive_probe_results`), following a renamed file; dropped if the reader moved file, the track is gone, or a
  page is open.

**`i` edits** (`App::open_cue_editor`, `Dialog::EditCue`): its own multi-line buffer (line breaks matter), no
wrapping. `Esc` keeps the typing, `Ctrl+S` stages and saves; the editor never raises a second dialog.

**`i` in the timeline adds a cue** at the cursor (`CueTarget::New`), `INSERT_DURATION` (2 s) clamped to the
media's true duration. Nothing is staged unless the editor closes with non-whitespace text. It is inserted in
time order (groups recomputed), frames sharing its screen are dropped and the rest shifted, and the cursor
returns to the cue panel (`leave_timeline`).

**`d` marks a cue for deletion** (`App::toggle_delete_selected_cue`, `CueChanges::deletes`) — a toggle like the
track list's `d`. The row stays, and **stays burned into the preview** (judging a line means seeing it). A cue
added this session is un-added instead (`SubtitleEditState::remove_cue`, mirror of `insert_cue`). A staged
rewrite of a marked cue is kept; `i`, `t`, `r` refuse a marked cue (`CUE_MARKED_FOR_DELETION`). The last cue
cannot be marked (`CUE_TRACK_NEEDS_A_CUE`). Marking advances the cursor (`select_within_group(1)` then
`select(1)`); unmarking does not.

**`t` is the timing mode** (`TimingScope::Cue(CueGrip)`, `App::toggle_cue_timing_mode`): `h`/`l` move by
`TIMING_STEP` (50 ms), `H`/`L` ten, `r` resets the cue. A mode, not a dialog, so it works alongside playback;
sticky across `j`/`k`. A nudge stops a running playback (its frames show the old timing).

- **Both ends move together, even at 0:00** — it stops rather than shortening the cue. A press moving nothing
  returns `None` and stages nothing.
- `set_cue_timing` drops every encoded frame overlapping the old *or* new span (`cue::shares_screen`).
- `Esc` order in `App::back`: playback, then mode, then page. `h`/`l` lose their in-group meaning while on.

**`Ctrl+H`/`Ctrl+L` in timing mode select what `h`/`l` move**: start ← whole → end (`CueGrip`,
`App::move_cue_grip`, `SubtitleEditState::move_grabbed`), no wrap, forgotten on leaving the mode. In the
timeline the same keys are the fine step — the focus decides
(`the_pane_holding_the_cursor_should_decide_what_ctrl_h_moves`). This replaced a grow/shrink pair; **`Alt+H`/
`Alt+L` are bound to nothing — do not give them a resize meaning back.** The grip is shown in the title and as
reversed handle cells on the bar's moving ends (**both ends for the whole cue**), only when that end is in the
window. Edges cannot cross `MIN_CUE_LENGTH`; a cue already shorter is left alone. Start stops at zero; end is
not capped. `r` resets the whole cue.

**`D` types a length** (dialog, refused over playback): keeps the start, moves the end. Refused if longer than
the media (`CUE_LENGTH_LONGER_THAN_MEDIA` — an unbounded decode otherwise). A bad value keeps the dialog open
with the reason on the status row; `Esc` drops the typing.

**`T` is the same mode at track scale** (`TimingScope::Track`, `App::toggle_global_retiming`): `h`/`l`/`H`/`L`
shift every cue, `r` resets every staged timing (keeping words; inserted cues stay). One enum, not two flags;
`T` replaces `t`'s scope, one `Esc` leaves.

- **The floor is the earliest cue's** (`shift_all`, searched, not read off the front): the shift shortens rather
  than piling cues at zero.
- All encoded frames are cleared outright, with `frame_error`.
- **Filled in one pass** (`App::shift_whole_track`) — `stage_cue_change` per cue is quadratic — applying the same
  per-entry rules.
- Cues marked for deletion move too.
- `track_shift` is stored (nothing to derive it from), accumulated as applied, zeroed by `apply_prepared` and wide
  `r`.
- `A` is reserved for an automatic sync writing through the same map.

### Making and opening tracks

**`a` on the track list creates a subtitle track** (`Dialog::CreateTrack`, `App::open_create_track`) and opens the
page on it. **Every new track is a zero-byte sidecar written when the popup is answered** — there is no "new
track" `SubtitleSource`, and staging one would need phantom entries everywhere.

- "Internal" (default, `NewTrackPlacement`) just sets `import_into_media = true`, as `Ctrl+H` does.
- Nothing is written until every refusal has been raised. `subtitle::is_sidecar_host` is shared with
  `partition_sidecars`. No `preview_blocked` check until a second format exists.
- **Language starts empty and must be chosen** — never `und` (a save refuses `und` sidecars), never guessed from
  the audio. `Create` is refused until chosen (`App::confirm_create_track`); the picker is the subtitle
  settings' (`App::filtered_create_track_languages`).
- **An empty sidecar cannot be imported** (`empty_import_error_for`, and `edit::validate_subtitle_sources`) —
  checked in the `Ctrl+S` pre-flight; a staged insertion counts as content.
- **Leaving without a line deletes the file** (`App::discard_empty_new_track`, from `close_subtitle_edit`) only if
  it is this page's track, no save is in flight, nothing is staged in either map, and it is still zero bytes. The
  import mark does not count as work. The rescan is done by `leave_subtitle_edit`, not there.
- `reconcile_files` **remaps** staged subtitle changes (`App::remap_subtitle_changes`) by path and fingerprint,
  including `left_subtitle_order` positions, instead of clearing them; notice only when something dropped.

**An empty track is a working page.** There is no `LoadStatus::Empty`; emptiness is `cues.is_empty()`.
`apply_prepared` puts the cursor in the timeline (`cursor_seed` = zero), `remove_cue` does the same when the
last added cue goes, and the timeline still draws axis, ruler and cursor (`TimelineWindow::around`). Only
`Preparing` and `Failed` take the whole page.

### Finding a cue (`/`)

Same search bar as elsewhere (`SearchState`, `search_line`, `TextInputSite::CueSearch`).

- **A projection, not an edit**: `SubtitleEditState::visible` holds group indices; nothing else changes, and
  unfiltered `visible[i] == i`.
- **The timeline is not filtered.**
- Matches `cue.text` (staged words), trimmed case-insensitive substring; `match_count` counts cues.
- A group shows whole if any cue matches, on the page holding the match (`enter_group`, `group_window`;
  outranks `group_memory`).
- The cursor stays while its group matches; otherwise first visible group; no matches leaves it put.
- `Esc` restores `search_origin` (a `CueOrigin`); `Enter` keeps the filter and drops the origin. A back key with
  the bar closed clears the filter.
- The bar takes rows from the cue panel, never the status row (that would resize the preview).
- Matches are `Modifier::REVERSED` (`highlight_matches`); case folding per char (`cue::fold_case`) keeps offsets
  aligned for `cue::match_ranges`.
- `refilter` runs from `apply_prepared`, `insert_cue`, `remove_cue`, `edit_cue_text`, `App::after_text_edit`.
  The filter dies with the page.

## Commits and Releases

Short imperative subjects (`Add keybinds menu`), one coherent change per commit. PRs explain user-visible
behaviour, list verification commands, include a screenshot for layout changes and call out `ffmpeg`
requirements. Never commit `target/`.

Tags of exact semantic versions (`0.2.0`) publish to crates.io. Tags must be on `main` and only after CI is
green.
