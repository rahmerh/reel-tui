//! State for the subtitle timing page: which cues a track holds, which one is selected,
//! and the scratch directory the preview worker stages files in.
//!
//! Kept out of `App` as one owned struct rather than a dozen loose fields, for the same
//! reason `staging::BatchState` is: the page's lifetime is a single `Option`, so every
//! way of leaving it — Esc, selecting another file, quitting — releases the whole thing
//! including its temp directory, without each exit path having to remember to.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::layout::Size;
use ratatui_image::protocol::Protocol;

use crate::cue::{Cue, LaneLayout, MAX_LANES, pack_lanes};
use crate::preview::FrameSource;
use crate::subtitle::SubtitleSource;

/// How long the selection has to stop moving before a frame is asked for.
///
/// Matches the file list's probe debounce (`App::start_pending_probe`). Walking a cue list
/// with `j` held down would otherwise start an `ffmpeg` per repeat, each one an accurate
/// seek that on a network mount reads from the preceding keyframe.
pub const FRAME_DEBOUNCE: Duration = Duration::from_millis(120);

/// How far the background pass has got in rendering the track's frames.
///
/// Its own enum rather than a pair of counters, because "not running" has three different
/// meanings on this page and only one of them is worth a word on screen: a network mount
/// is a decision the user did not make and would otherwise be left wondering about,
/// whereas a build that cannot draw images at all already shows text previews everywhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarmState {
    /// No background pass, and nothing to say about it: the terminal draws no images, the
    /// FFmpeg build cannot burn subtitles in, or the user turned prefetching off.
    Off,
    /// No background pass because the media is on a network mount, where rendering a
    /// frame per cue means a thousand accurate seeks across the network. Said out loud on
    /// the page, since the frames the user does land on still appear.
    OffForNetwork,
    /// `done` counts cues the pass has finished with, however it finished with them.
    Working {
        done: usize,
        total: usize,
    },
    Done,
}

/// How far the page has got in loading a track's cues.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncStatus {
    /// Cues are being extracted or read. The page is open and drawn during this.
    Preparing,
    Ready,
    /// The track parsed but holds no cues — an empty or wholly unparseable file.
    /// Distinct from `Failed`, because nothing went wrong, there is just nothing to see.
    Empty,
    Failed(String),
}

/// A scratch directory for one open page, removed when the page closes.
///
/// Lives under the system temp directory rather than beside the media: the name is then
/// entirely ours, which is what lets the preview worker hand `ffmpeg` a bare relative
/// filename instead of escaping a user path through the filter-graph syntax. It also
/// keeps the directory monitor from ever seeing these files.
#[derive(Debug)]
pub struct PreviewWorkspace(PathBuf);

impl PreviewWorkspace {
    pub fn new() -> std::io::Result<Self> {
        let path = std::env::temp_dir()
            .join("reel-tui-preview")
            .join(unique_name());
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PreviewWorkspace {
    fn drop(&mut self) {
        // Best-effort: a page closing is not a place to surface a failed unlink, and the
        // directory holds nothing but a copy of subtitles the user already has.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn unique_name() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

/// A drawn frame and the cue it belongs to.
///
/// A struct with its own `Debug` rather than a tuple in the state, because `Protocol`
/// holds encoded image data and has none of its own — and without this the whole page
/// state would lose its derived `Debug`, which every test failure message prints.
struct Frame {
    cue_index: usize,
    protocol: Box<Protocol>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Frame")
            .field("cue_index", &self.cue_index)
            .field("size", &self.protocol.size())
            .finish()
    }
}

/// Everything the subtitle timing page draws and navigates.
#[derive(Debug)]
pub struct SubtitleSyncState {
    /// Which page opening this is. Echoed by every worker message so results belonging
    /// to a page the user has already left can be dropped rather than applied.
    pub generation: u64,
    /// Everything a frame grab needs about the media: the file itself — the container for
    /// an embedded track, or the sidecar's companion media — how to tell it has changed,
    /// and the size its frames are rendered at.
    pub frames: FrameSource,
    pub source: SubtitleSource,
    pub duration: Duration,
    pub status: SyncStatus,
    /// How far the background frame pass has got, or why there is not one.
    pub warm: WarmState,
    pub cues: Vec<Cue>,
    pub layout: LaneLayout,
    pub selected: usize,
    /// First cue row drawn, moved only to keep `selected` on screen.
    pub list_scroll: usize,
    /// Rows the cue list can show, measured by the renderer.
    pub list_rows: usize,
    /// Size of the preview pane in terminal cells, measured by the renderer.
    ///
    /// Recorded here because the frame grab has to scale to a pane whose size only the
    /// renderer knows, and because a change to it is what tells the worker the frame it
    /// already produced is the wrong size now.
    pub preview_cells: Size,
    /// The frame on screen, once one has been drawn.
    frame: Option<Frame>,
    /// Why the last frame request produced nothing, shown under the text preview.
    pub frame_error: Option<String>,
    /// Set when something invalidated the frame — the selection moved, the cues arrived,
    /// the pane resized — and cleared when the request is actually sent, one debounce
    /// later.
    frame_pending_since: Option<Instant>,
    workspace: PreviewWorkspace,
}

impl SubtitleSyncState {
    pub fn new(
        generation: u64,
        frames: FrameSource,
        source: SubtitleSource,
        duration: Duration,
        workspace: PreviewWorkspace,
    ) -> Self {
        Self {
            generation,
            frames,
            source,
            duration,
            status: SyncStatus::Preparing,
            warm: WarmState::Off,
            cues: Vec::new(),
            layout: LaneLayout::default(),
            selected: 0,
            list_scroll: 0,
            list_rows: 0,
            preview_cells: Size::new(0, 0),
            frame: None,
            frame_error: None,
            frame_pending_since: None,
            workspace,
        }
    }

    pub fn workspace(&self) -> &Path {
        self.workspace.path()
    }

    pub fn media(&self) -> &Path {
        &self.frames.media
    }

    /// Takes the background pass's latest count.
    ///
    /// `done == total` is "the pass is over", not "every cue was rendered" — a cue it
    /// could not draw is finished with too. The status line only reports a count while
    /// there is still one to report.
    pub fn apply_warming(&mut self, done: usize, total: usize) {
        self.warm = if done >= total {
            WarmState::Done
        } else {
            WarmState::Working { done, total }
        };
    }

    /// Takes the cues a worker parsed and makes the page ready.
    ///
    /// Lanes are packed once, here, rather than per frame: the packing is over the whole
    /// track, so doing it while rendering would both repeat the work every draw and let
    /// the track's height change as the user scrolls through denser regions.
    pub fn apply_prepared(&mut self, cues: Vec<Cue>) {
        self.layout = pack_lanes(&cues, MAX_LANES);
        self.status = if cues.is_empty() {
            SyncStatus::Empty
        } else {
            SyncStatus::Ready
        };
        self.cues = cues;
        self.selected = 0;
        self.list_scroll = 0;
        self.request_frame();
    }

    pub fn fail(&mut self, message: String) {
        self.status = SyncStatus::Failed(message);
        // Nothing to render frames for, so nothing to count. A pass already running for
        // this page is stopped by the generation bump that closing it performs.
        self.warm = WarmState::Off;
        self.cues.clear();
        self.layout = LaneLayout::default();
        self.drop_frame();
        self.frame_pending_since = None;
    }

    /// The frame to draw, if there is one for the cue currently selected.
    ///
    /// Keyed on the selection rather than handed out unconditionally, so the moment the
    /// cursor moves the pane shows the new cue's text instead of the previous cue's
    /// picture — a stale frame under a fresh cue reads as the burn-in being wrong.
    pub fn frame(&self) -> Option<&Protocol> {
        self.frame
            .as_ref()
            .filter(|frame| frame.cue_index == self.selected)
            .map(|frame| frame.protocol.as_ref())
    }

    /// Takes a rendered frame, ignoring one for a cue that is no longer selected.
    pub fn apply_frame(&mut self, cue_index: usize, protocol: Box<Protocol>) {
        self.frame_error = None;
        self.frame = Some(Frame {
            cue_index,
            protocol,
        });
    }

    pub fn fail_frame(&mut self, message: String) {
        self.drop_frame();
        self.frame_error = Some(message);
    }

    fn drop_frame(&mut self) {
        self.frame = None;
    }

    /// Marks the frame on screen as no longer the right one, starting the debounce.
    pub fn request_frame(&mut self) {
        self.frame_pending_since = Some(Instant::now());
    }

    /// Whether a frame request has waited out its debounce, consuming it if so.
    ///
    /// Consuming here rather than in the caller keeps "asked for" and "waiting to ask"
    /// from ever both being true, which is what would let one settled selection start two
    /// `ffmpeg` processes.
    pub fn take_due_frame_request(&mut self) -> bool {
        let due = self
            .frame_pending_since
            .is_some_and(|since| since.elapsed() >= FRAME_DEBOUNCE);
        if due {
            self.frame_pending_since = None;
        }
        due
    }

    /// Records the pane size the renderer measured, asking for a new frame when it
    /// changed — the frame already drawn was encoded for the old size, and `Image` draws
    /// nothing at all rather than clipping when it no longer fits.
    pub fn set_preview_cells(&mut self, cells: Size) {
        if self.preview_cells != cells {
            self.preview_cells = cells;
            self.request_frame();
        }
    }

    pub fn selected_cue(&self) -> Option<&Cue> {
        self.cues.get(self.selected)
    }

    /// Whether the page is waiting on background work, so the loader keeps animating.
    pub fn is_busy(&self) -> bool {
        self.status == SyncStatus::Preparing
    }

    /// Moves the cue cursor, reporting whether it actually moved.
    ///
    /// The return value is what stops a held-down `j` at the end of the list from
    /// re-requesting the same preview frame on every repeat.
    pub fn select(&mut self, delta: isize) -> bool {
        if self.cues.is_empty() {
            return false;
        }
        let last = self.cues.len() - 1;
        let next = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(last)
        };
        if next == self.selected {
            return false;
        }
        self.selected = next;
        self.request_frame();
        true
    }

    pub fn select_first(&mut self) -> bool {
        if self.cues.is_empty() || self.selected == 0 {
            return false;
        }
        self.selected = 0;
        self.request_frame();
        true
    }

    pub fn select_last(&mut self) -> bool {
        if self.cues.is_empty() {
            return false;
        }
        let last = self.cues.len() - 1;
        if self.selected == last {
            return false;
        }
        self.selected = last;
        self.request_frame();
        true
    }

    /// Scrolls the cue list just far enough to keep the selection visible.
    ///
    /// Called from the renderer, which is the only place that knows how many rows the
    /// list actually got — the same arrangement `sync_batch_scroll` uses for the batch
    /// dialog.
    pub fn sync_scroll(&mut self, rows: usize) {
        self.list_rows = rows;
        if rows == 0 {
            return;
        }
        if self.selected < self.list_scroll {
            self.list_scroll = self.selected;
        } else if self.selected >= self.list_scroll + rows {
            self.list_scroll = self.selected + 1 - rows;
        }
        self.list_scroll = self.list_scroll.min(self.cues.len().saturating_sub(rows));
    }
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;

    fn cue(start: u64, end: u64, text: &str) -> Cue {
        Cue {
            index: 0,
            start: Duration::from_millis(start),
            end: Duration::from_millis(end),
            text: text.to_string(),
        }
    }

    fn state() -> SubtitleSyncState {
        SubtitleSyncState::new(
            1,
            FrameSource {
                media: PathBuf::from("/media/show.mkv"),
                media_length: 4096,
                media_modified: None,
                pixels: (960, 540),
                workspace: PathBuf::from("/tmp/reel-tui-preview/state"),
            },
            SubtitleSource::Embedded(2),
            Duration::from_secs(600),
            PreviewWorkspace::new().unwrap(),
        )
    }

    /// A protocol occupying exactly `width` x `height` cells.
    ///
    /// Sized in pixels from the halfblocks font size, because `Resize::Fit` takes the
    /// cell size from the image's own proportions rather than from what it was asked for.
    fn protocol(width: u16, height: u16) -> Box<Protocol> {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let font = picker.font_size();
        let image = image::DynamicImage::new_rgb8(
            u32::from(width) * u32::from(font.width),
            u32::from(height) * u32::from(font.height),
        );
        Box::new(
            picker
                .new_protocol(
                    image,
                    Size::new(width, height),
                    ratatui_image::Resize::Fit(None),
                )
                .expect("halfblocks should encode any image"),
        )
    }

    fn ready(count: usize) -> SubtitleSyncState {
        let mut state = state();
        let cues = (0..count)
            .map(|index| {
                let start = index as u64 * 2000;
                cue(start, start + 1000, &format!("line {index}"))
            })
            .collect();
        state.apply_prepared(cues);
        state
    }

    #[test]
    fn a_new_page_should_start_preparing_with_nothing_to_show() {
        // Act
        let state = state();

        // Assert
        assert_that!(state.status.clone()).is_equal_to(SyncStatus::Preparing);
        assert_that!(state.is_busy()).is_true();
        assert_that!(state.cues.as_slice()).is_empty();
        assert_that!(state.selected_cue()).is_none();
    }

    #[test]
    fn apply_prepared_should_pack_lanes_and_select_the_first_cue() {
        // Arrange: the middle cue overlaps the first, so the track needs two lanes.
        let mut state = state();
        state.selected = 5;

        // Act
        state.apply_prepared(vec![
            cue(0, 3000, "a"),
            cue(1000, 4000, "b"),
            cue(5000, 6000, "c"),
        ]);

        // Assert
        assert_that!(state.status.clone()).is_equal_to(SyncStatus::Ready);
        assert_that!(state.layout.lane_count).is_equal_to(2);
        assert_that!(state.layout.lanes.as_slice()).contains_exactly_in_given_order([0, 1, 0]);
        assert_that!(state.selected).is_equal_to(0);
        assert_that!(state.is_busy()).is_false();
    }

    /// An empty track is not a failure — the file was read, it simply holds no cues —
    /// and reporting it as one would send the user hunting for a problem that is not
    /// there.
    #[test]
    fn apply_prepared_should_report_an_empty_track_separately_from_a_failure() {
        // Arrange
        let mut state = state();

        // Act
        state.apply_prepared(Vec::new());

        // Assert
        assert_that!(state.status.clone()).is_equal_to(SyncStatus::Empty);
        assert_that!(state.is_busy()).is_false();
    }

    #[test]
    fn fail_should_report_the_message_and_drop_any_cues() {
        // Arrange
        let mut state = ready(3);

        // Act
        state.fail("ffmpeg exploded".to_string());

        // Assert
        assert_that!(state.status.clone())
            .is_equal_to(SyncStatus::Failed("ffmpeg exploded".to_string()));
        assert_that!(state.cues.as_slice()).is_empty();
        assert_that!(state.layout.lane_count).is_equal_to(0);
        assert_that!(state.is_busy()).is_false();
    }

    #[test]
    fn select_should_move_the_cursor_and_report_that_it_moved() {
        // Arrange
        let mut state = ready(3);

        // Act / Assert
        assert_that!(state.select(1)).is_true();
        assert_that!(state.selected).is_equal_to(1);
        assert_that!(state.select(-1)).is_true();
        assert_that!(state.selected).is_equal_to(0);
    }

    /// Holding `j` at the end of the list must not keep re-requesting the same preview
    /// frame, which is one `ffmpeg` process per key repeat.
    #[test]
    fn select_should_report_no_movement_at_either_end_of_the_list() {
        // Arrange
        let mut state = ready(3);

        // Act / Assert
        assert_that!(state.select(-1)).is_false();
        assert_that!(state.selected).is_equal_to(0);
        state.select(10);
        assert_that!(state.selected).is_equal_to(2);
        assert_that!(state.select(1)).is_false();
        assert_that!(state.selected).is_equal_to(2);
    }

    #[test]
    fn select_should_do_nothing_on_a_track_without_cues() {
        // Arrange
        let mut state = state();

        // Act / Assert
        assert_that!(state.select(1)).is_false();
        assert_that!(state.select_first()).is_false();
        assert_that!(state.select_last()).is_false();
    }

    #[test]
    fn select_first_and_last_should_jump_to_the_ends() {
        // Arrange
        let mut state = ready(5);

        // Act / Assert
        assert_that!(state.select_last()).is_true();
        assert_that!(state.selected).is_equal_to(4);
        assert_that!(state.select_last()).is_false();
        assert_that!(state.select_first()).is_true();
        assert_that!(state.selected).is_equal_to(0);
        assert_that!(state.select_first()).is_false();
    }

    #[test]
    fn sync_scroll_should_follow_the_selection_down_past_the_last_visible_row() {
        // Arrange
        let mut state = ready(10);
        state.sync_scroll(4);

        // Act
        state.select(5);
        state.sync_scroll(4);

        // Assert
        assert_that!(state.list_scroll).is_equal_to(2);
    }

    #[test]
    fn sync_scroll_should_follow_the_selection_back_up_above_the_first_visible_row() {
        // Arrange
        let mut state = ready(10);
        state.select(9);
        state.sync_scroll(4);

        // Act
        state.select(-9);
        state.sync_scroll(4);

        // Assert
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    #[test]
    fn sync_scroll_should_not_leave_blank_rows_below_a_short_list() {
        // Arrange: scrolled to the bottom, then given a taller pane.
        let mut state = ready(6);
        state.select(5);
        state.sync_scroll(2);
        assert_that!(state.list_scroll).is_equal_to(4);

        // Act
        state.sync_scroll(6);

        // Assert
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    #[test]
    fn sync_scroll_should_record_the_measured_row_count_and_tolerate_a_pane_with_none() {
        // Arrange
        let mut state = ready(10);
        state.select(9);

        // Act
        state.sync_scroll(0);

        // Assert
        assert_that!(state.list_rows).is_equal_to(0);
        assert_that!(state.list_scroll).is_equal_to(0);
    }

    #[test]
    fn selected_cue_should_return_the_cue_under_the_cursor() {
        // Arrange
        let mut state = ready(3);
        state.select(2);

        // Act / Assert
        assert_that!(state.selected_cue().map(|cue| cue.text.as_str())).is_equal_to(Some("line 2"));
    }

    /// The workspace exists for exactly as long as the page does. Leaking it would leave
    /// a copy of every previewed subtitle in the temp directory for the session's life.
    #[test]
    fn dropping_the_page_should_remove_its_workspace() {
        // Arrange
        let state = state();
        let path = state.workspace().to_path_buf();
        std::fs::write(path.join("cues.srt"), "1\n").unwrap();
        assert_that!(path.exists()).is_true();

        // Act
        drop(state);

        // Assert
        assert_that!(path.exists()).is_false();
    }

    #[test]
    fn two_workspaces_should_not_share_a_directory() {
        // Act
        let first = PreviewWorkspace::new().unwrap();
        let second = PreviewWorkspace::new().unwrap();

        // Assert
        assert_that!(first.path() == second.path()).is_false();
    }

    /// The page's state is printed whole in every test failure and in the harness's
    /// timeout dump, and `Protocol` contributes nothing to that on its own.
    #[test]
    fn a_stored_frame_should_describe_itself_by_cue_and_size() {
        // Arrange
        let mut state = ready(2);
        state.apply_frame(1, protocol(10, 5));

        // Act
        let described = format!("{:?}", state.frame);

        // Assert
        assert_that!(described.as_str()).contains("cue_index: 1");
        assert_that!(described.as_str()).contains("width: 10");
        assert_that!(described.as_str()).contains("height: 5");
    }

    /// A frame belongs to the cue it was drawn for. Showing it under a different cue
    /// would read as the burn-in being wrong, which is the one thing this page exists to
    /// let you judge.
    #[test]
    fn a_frame_should_only_be_drawn_for_the_cue_it_was_rendered_for() {
        // Arrange
        let mut state = ready(3);

        // Act
        state.apply_frame(0, protocol(10, 5));

        // Assert
        assert_that!(state.frame().is_some()).is_true();
        state.select(1);
        assert_that!(state.frame().is_some()).is_false();
        state.select(-1);
        assert_that!(state.frame().is_some()).is_true();
    }

    #[test]
    fn a_frame_that_could_not_be_drawn_should_replace_the_one_on_screen_with_its_reason() {
        // Arrange
        let mut state = ready(3);
        state.apply_frame(0, protocol(10, 5));

        // Act
        state.fail_frame("libass is missing".to_string());

        // Assert
        assert_that!(state.frame().is_some()).is_false();
        assert_that!(state.frame_error.clone()).is_equal_to(Some("libass is missing".to_string()));

        // Act / Assert: and a frame that does arrive clears the reason again.
        state.apply_frame(0, protocol(10, 5));
        assert_that!(state.frame_error.clone()).is_none();
    }

    /// Everything that invalidates the frame has to go through the same debounce, or a
    /// held-down `j` starts an ffmpeg per key repeat.
    #[test]
    fn moving_the_selection_should_ask_for_a_frame_only_once_the_movement_settles() {
        // Arrange
        let mut state = ready(5);
        state.take_due_frame_request();

        // Act
        state.select(1);

        // Assert: pending, but not yet due.
        assert_that!(state.take_due_frame_request()).is_false();
        std::thread::sleep(FRAME_DEBOUNCE + Duration::from_millis(20));
        assert_that!(state.take_due_frame_request()).is_true();
        // And consumed: one settled selection is one request.
        assert_that!(state.take_due_frame_request()).is_false();
    }

    #[test]
    fn every_way_the_frame_goes_stale_should_ask_for_a_new_one() {
        // Arrange
        let mut state = state();
        let due = |state: &mut SubtitleSyncState| {
            std::thread::sleep(FRAME_DEBOUNCE + Duration::from_millis(20));
            state.take_due_frame_request()
        };

        // Act / Assert: the cues arriving.
        state.apply_prepared(vec![cue(0, 1000, "a"), cue(2000, 3000, "b")]);
        assert_that!(due(&mut state)).is_true();

        // Act / Assert: the pane being measured, and then resized.
        state.set_preview_cells(Size::new(40, 20));
        assert_that!(due(&mut state)).is_true();
        state.set_preview_cells(Size::new(40, 20));
        assert_that!(due(&mut state)).is_false();
        state.set_preview_cells(Size::new(30, 20));
        assert_that!(due(&mut state)).is_true();

        // Act / Assert: jumping to either end of the list.
        assert_that!(state.select_last()).is_true();
        assert_that!(due(&mut state)).is_true();
        assert_that!(state.select_first()).is_true();
        assert_that!(due(&mut state)).is_true();

        // Act / Assert: and a refusal to move asks for nothing.
        assert_that!(state.select_first()).is_false();
        assert_that!(due(&mut state)).is_false();
    }

    /// A track that failed to load has no cue to draw, so a request left pending from
    /// opening the page would grab a frame for a cue list that does not exist.
    #[test]
    fn a_failed_track_should_drop_its_frame_and_stop_asking_for_one() {
        // Arrange
        let mut state = ready(3);
        state.apply_frame(0, protocol(10, 5));

        // Act
        state.fail("ffmpeg exploded".to_string());

        // Assert
        assert_that!(state.frame().is_some()).is_false();
        std::thread::sleep(FRAME_DEBOUNCE + Duration::from_millis(20));
        assert_that!(state.take_due_frame_request()).is_false();
    }

    /// The status line counts cues the pass is finished with, and stops counting when the
    /// pass is over — however it ended, since a cue it could not draw is finished with too.
    #[test]
    fn warming_should_count_up_and_then_stop_counting() {
        // Arrange
        let mut state = ready(4);
        assert_that!(state.warm).is_equal_to(WarmState::Off);

        // Act / Assert
        state.apply_warming(0, 4);
        assert_that!(state.warm).is_equal_to(WarmState::Working { done: 0, total: 4 });
        state.apply_warming(3, 4);
        assert_that!(state.warm).is_equal_to(WarmState::Working { done: 3, total: 4 });
        state.apply_warming(4, 4);
        assert_that!(state.warm).is_equal_to(WarmState::Done);
        // A pass that gave up early reports everything left as finished at once, which
        // must read as done rather than as a count past the end.
        state.apply_warming(9, 4);
        assert_that!(state.warm).is_equal_to(WarmState::Done);
        // An empty track has nothing to count and is over before it starts.
        state.apply_warming(0, 0);
        assert_that!(state.warm).is_equal_to(WarmState::Done);
    }

    /// A track that could not be read has no cues to render, so a count left on screen
    /// from before it failed would be counting towards nothing.
    #[test]
    fn a_failed_track_should_stop_reporting_a_background_pass() {
        // Arrange
        let mut state = ready(3);
        state.apply_warming(1, 3);

        // Act
        state.fail("ffmpeg exploded".to_string());

        // Assert
        assert_that!(state.warm).is_equal_to(WarmState::Off);
    }

    /// The media is named in every cache key and in every grab, so the page has to hand
    /// out the one its frames were rendered against rather than a second copy.
    #[test]
    fn the_page_should_name_the_media_its_frames_come_from() {
        // Act / Assert
        assert_that!(state().media()).is_equal_to(Path::new("/media/show.mkv"));
    }
}
