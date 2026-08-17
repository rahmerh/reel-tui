//! State for the subtitle timing page: which cues a track holds, which one is selected,
//! and the scratch directory the preview worker stages files in.
//!
//! Kept out of `App` as one owned struct rather than a dozen loose fields, for the same
//! reason `staging::BatchState` is: the page's lifetime is a single `Option`, so every
//! way of leaving it — Esc, selecting another file, quitting — releases the whole thing
//! including its temp directory, without each exit path having to remember to.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cue::{Cue, LaneLayout, MAX_LANES, pack_lanes};
use crate::subtitle::SubtitleSource;

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

/// Everything the subtitle timing page draws and navigates.
#[derive(Debug)]
pub struct SubtitleSyncState {
    /// Which page opening this is. Echoed by every worker message so results belonging
    /// to a page the user has already left can be dropped rather than applied.
    pub generation: u64,
    /// The video frames are grabbed from — the container for an embedded track, or the
    /// sidecar's companion media.
    pub media: PathBuf,
    pub source: SubtitleSource,
    pub duration: Duration,
    pub status: SyncStatus,
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
    pub preview_cells: (u16, u16),
    workspace: PreviewWorkspace,
}

impl SubtitleSyncState {
    pub fn new(
        generation: u64,
        media: PathBuf,
        source: SubtitleSource,
        duration: Duration,
        workspace: PreviewWorkspace,
    ) -> Self {
        Self {
            generation,
            media,
            source,
            duration,
            status: SyncStatus::Preparing,
            cues: Vec::new(),
            layout: LaneLayout::default(),
            selected: 0,
            list_scroll: 0,
            list_rows: 0,
            preview_cells: (0, 0),
            workspace,
        }
    }

    pub fn workspace(&self) -> &Path {
        self.workspace.path()
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
    }

    pub fn fail(&mut self, message: String) {
        self.status = SyncStatus::Failed(message);
        self.cues.clear();
        self.layout = LaneLayout::default();
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
        true
    }

    pub fn select_first(&mut self) -> bool {
        if self.cues.is_empty() || self.selected == 0 {
            return false;
        }
        self.selected = 0;
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
            PathBuf::from("/media/show.mkv"),
            SubtitleSource::Embedded(2),
            Duration::from_secs(600),
            PreviewWorkspace::new().unwrap(),
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
}
