use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

use crate::app::TrackRef;
use crate::edit::{ContainerFormat, ContainerMetadata, VideoSettings};
use crate::files::FileFingerprint;
use crate::subtitle::{SubtitleChange, SubtitleSource};

/// A per-file snapshot of staged (not-yet-processed) track/container/subtitle edits,
/// keyed by path on `App::staged_edits`. This is what lets a user edit file A, switch
/// to file B, edit it too, and process both — rather than edits being tied to
/// whichever file happens to be currently selected, which is what `App`'s live
/// `stream_order`/`deleted_streams`/etc. fields represent for the file currently open.
#[derive(Clone, Debug)]
pub struct StagedEdit {
    /// The file's on-disk fingerprint when these edits were staged. Compared against
    /// the live fingerprint on every reconcile to detect the file changing underneath
    /// a staged (not currently open) edit — see `stale`.
    pub fingerprint: FileFingerprint,
    /// Set once the file's on-disk fingerprint no longer matches `fingerprint`. Kept
    /// rather than discarded, so the user can inspect or manually reset the file
    /// instead of silently losing staged work, but this blocks processing until
    /// resolved.
    pub stale: bool,
    /// Which `app::stream_group` labels ("video"/"audio"/"subtitle"/"other") a
    /// background re-probe found to be *structurally* conflicted: this edit stages
    /// changes to tracks of that type, and that type's set of tracks on disk is no
    /// longer the one that was staged against. Empty means no conflict.
    ///
    /// Deliberately narrower than "the staged edit no longer validates". A pure
    /// metadata/mtime bump, a change to a type nothing is staged for, and an edit that
    /// merely became *invalid* against the new content (say a staged MP4 conversion
    /// after a SubRip track appeared) all leave this empty and auto-resolve quietly —
    /// the last of those then surfaces through the ordinary
    /// `App::staged_file_status` `Invalid` path, with the compatibility markers and
    /// Ctrl+S block that already exist for it. Only this set forces the unskippable
    /// `Dialog::ResolveConflicts` notice, because only this means the tracks the user
    /// edited are not the tracks that are now there.
    ///
    /// Always empty while `stale` is false, and empty while a check is still queued or
    /// in flight (a file can be `stale` without a conflict being confirmed yet). Drives
    /// `App::conflicting_paths`/`App::acknowledge_conflicts`.
    pub conflict_groups: BTreeSet<&'static str>,
    pub stream_order: Vec<u64>,
    pub moved_streams: BTreeSet<u64>,
    pub deleted_streams: BTreeSet<u64>,
    pub default_streams: BTreeSet<u64>,
    pub default_sidecars: BTreeSet<usize>,
    pub video_settings: BTreeMap<u64, VideoSettings>,
    pub subtitle_changes: BTreeMap<SubtitleSource, SubtitleChange>,
    pub left_subtitle_order: Vec<TrackRef>,
    pub container_target: Option<ContainerFormat>,
    pub container_metadata: Option<ContainerMetadata>,
    /// Baseline `stream_order` before any staged reordering, needed both for the
    /// "changed tracks" diff and for `App::reset_track_position` to restore a single
    /// track's position without disturbing other tracks' staged moves.
    pub original_stream_order: Vec<u64>,
    pub original_default_streams: BTreeSet<u64>,
    /// Which stream-type group (`app::stream_group`'s "video"/"audio"/"subtitle"/
    /// "other") each index in `original_stream_order` belonged to when editing began
    /// — covers tracks later marked for deletion too, since those can still vanish
    /// from disk independently. This is the minimal snapshot `App::reconcile_untouched_groups`
    /// needs to tell "a group nobody edited grew or shrank a track" (auto-resolve)
    /// apart from "a group the edit actually touches changed" (always a conflict) —
    /// deliberately just a type label per index rather than a full `MediaInfo`
    /// snapshot, since a type label doesn't flap on ordinary metadata/tag changes the
    /// way whole-stream equality did.
    pub track_groups: BTreeMap<u64, &'static str>,
}

/// The lifecycle of one file within an in-flight `BatchState`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatchItemStatus {
    /// Sent to a worker pool, not yet picked up by a thread.
    Pending,
    /// A worker is actively processing it (at least one `Progress` event received).
    Running,
    Completed,
    Failed(String),
    Cancelled,
}

/// One file's row in the batch progress dialog.
#[derive(Clone, Debug)]
pub struct BatchItem {
    pub path: PathBuf,
    pub label: Option<String>,
    pub fraction: Option<f64>,
    pub status: BatchItemStatus,
    /// Set once the item completes — may differ from `path` (e.g. a container
    /// conversion renaming `movie.mkv` to `movie.mp4`), so the file list can reselect
    /// what the source file actually became.
    pub output_path: Option<PathBuf>,
}

/// State for an in-flight "process all staged files" run (`Dialog::BatchProcessing`),
/// built by `App::confirm_process_all`. `cancelled` is shared across every request in
/// the batch — both already-running and still-queued — so cancelling the batch stops
/// everything not yet finished with a single flag flip, rather than needing a
/// per-request cancel handle.
pub struct BatchState {
    pub cancelled: Arc<AtomicBool>,
    pub items: Vec<BatchItem>,
    pub started: Instant,
}

impl BatchState {
    pub fn item_mut(&mut self, path: &std::path::Path) -> Option<&mut BatchItem> {
        self.items.iter_mut().find(|item| item.path == path)
    }

    /// Whether every item has reached a terminal state — the batch dialog auto-closes
    /// once this is true.
    pub fn is_finished(&self) -> bool {
        self.items.iter().all(|item| {
            !matches!(
                item.status,
                BatchItemStatus::Pending | BatchItemStatus::Running
            )
        })
    }
}
