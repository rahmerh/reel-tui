use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use ratatui::widgets::ListState;

use crate::{
    edit::{EditEvent, EditOutcome, EditRequest, stream_index, validate_edit},
    files::{FileEntry, scan_directory},
    probe::{MediaInfo, ProbeOutcome, ProbeRequest, ProbeResponse},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Layer {
    #[default]
    Files,
    Streams,
    StreamDetails,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dialog {
    Keybindings,
    ConfirmSave,
    Processing,
    Error,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    path: PathBuf,
    length: u64,
    modified: Option<std::time::SystemTime>,
}

impl CacheKey {
    fn for_path(path: PathBuf) -> Option<Self> {
        let metadata = fs::metadata(&path).ok()?;
        Some(Self {
            path,
            length: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

pub struct App {
    pub directory: PathBuf,
    pub files: Vec<FileEntry>,
    pub list_state: ListState,
    pub outcome: Option<ProbeOutcome>,
    pub loading: bool,
    pub details_scroll: u16,
    pub details_max_scroll: u16,
    pub keybindings_scroll: u16,
    pub keybindings_max_scroll: u16,
    pub layer: Layer,
    pub selected_stream: usize,
    pub stream_order: Vec<u64>,
    pub deleted_streams: BTreeSet<u64>,
    pub default_streams: BTreeSet<u64>,
    pub dialog: Option<Dialog>,
    pub notice: Option<String>,
    pub edit_error: Option<String>,
    pub edit_progress: Option<f64>,
    pub edit_started: Option<Instant>,
    pub scan_error: Option<String>,
    request_tx: Sender<ProbeRequest>,
    edit_tx: Sender<EditRequest>,
    edit_cancel: Option<Arc<AtomicBool>>,
    generation: u64,
    pending_since: Option<Instant>,
    cache: HashMap<CacheKey, ProbeOutcome>,
    original_stream_order: Vec<u64>,
    original_default_streams: BTreeSet<u64>,
}

impl App {
    pub fn new(
        directory: PathBuf,
        request_tx: Sender<ProbeRequest>,
        edit_tx: Sender<EditRequest>,
    ) -> Result<Self> {
        let mut app = Self {
            directory,
            files: Vec::new(),
            list_state: ListState::default(),
            outcome: None,
            loading: false,
            details_scroll: 0,
            details_max_scroll: 0,
            keybindings_scroll: 0,
            keybindings_max_scroll: 0,
            layer: Layer::Files,
            selected_stream: 0,
            stream_order: Vec::new(),
            deleted_streams: BTreeSet::new(),
            default_streams: BTreeSet::new(),
            dialog: None,
            notice: None,
            edit_error: None,
            edit_progress: None,
            edit_started: None,
            scan_error: None,
            request_tx,
            edit_tx,
            edit_cancel: None,
            generation: 0,
            pending_since: None,
            cache: HashMap::new(),
            original_stream_order: Vec::new(),
            original_default_streams: BTreeSet::new(),
        };
        app.refresh()?;
        Ok(app)
    }

    pub fn refresh(&mut self) -> Result<()> {
        self.clear_edit_state();
        let selected_path = self.selected_file().map(|file| file.path.clone());
        match scan_directory(&self.directory) {
            Ok(files) => {
                self.files = files;
                self.scan_error = None;
            }
            Err(error) => {
                self.files.clear();
                self.scan_error = Some(error.to_string());
            }
        }

        let selection = selected_path
            .and_then(|path| self.files.iter().position(|file| file.path == path))
            .or_else(|| (!self.files.is_empty()).then_some(0));
        self.list_state.select(selection);
        self.queue_probe();
        Ok(())
    }

    pub fn selected_file(&self) -> Option<&FileEntry> {
        self.list_state
            .selected()
            .and_then(|index| self.files.get(index))
    }

    pub fn select_next(&mut self) {
        self.notice = None;
        if self.layer == Layer::Streams {
            let count = self.stream_count();
            if count > 0 {
                self.selected_stream = (self.selected_stream + 1).min(count - 1);
            }
            return;
        }
        if self.layer == Layer::StreamDetails {
            self.scroll_down();
            return;
        }
        if self.files.is_empty() {
            return;
        }
        let next = self
            .list_state
            .selected()
            .map(|index| (index + 1).min(self.files.len() - 1))
            .unwrap_or(0);
        self.select(next);
    }

    pub fn select_previous(&mut self) {
        self.notice = None;
        if self.layer == Layer::Streams {
            self.selected_stream = self.selected_stream.saturating_sub(1);
            return;
        }
        if self.layer == Layer::StreamDetails {
            self.scroll_up();
            return;
        }
        let previous = self
            .list_state
            .selected()
            .map(|index| index.saturating_sub(1))
            .unwrap_or(0);
        self.select(previous);
    }

    pub fn select_first(&mut self) {
        self.notice = None;
        if self.layer == Layer::Streams {
            self.selected_stream = 0;
            return;
        }
        if !self.files.is_empty() {
            self.select(0);
        }
    }

    pub fn select_last(&mut self) {
        self.notice = None;
        if self.layer == Layer::Streams {
            self.selected_stream = self.stream_count().saturating_sub(1);
            return;
        }
        if !self.files.is_empty() {
            self.select(self.files.len() - 1);
        }
    }

    fn select(&mut self, index: usize) {
        if self.list_state.selected() != Some(index) {
            self.clear_edit_state();
            self.list_state.select(Some(index));
            self.queue_probe();
        }
    }

    fn queue_probe(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.details_scroll = 0;
        self.details_max_scroll = 0;
        self.layer = Layer::Files;
        self.selected_stream = 0;
        self.outcome = None;
        self.loading = self.selected_file().is_some();
        self.pending_since = self.loading.then(Instant::now);

        if let Some(key) = self
            .selected_file()
            .and_then(|file| CacheKey::for_path(file.path.clone()))
            && let Some(cached) = self.cache.get(&key)
        {
            self.outcome = Some(cached.clone());
            self.loading = false;
            self.pending_since = None;
            self.reset_track_edits();
        }
    }

    pub fn start_pending_probe(&mut self) {
        let Some(since) = self.pending_since else {
            return;
        };
        if since.elapsed() < Duration::from_millis(120) {
            return;
        }
        let Some(path) = self.selected_file().map(|file| file.path.clone()) else {
            self.pending_since = None;
            return;
        };

        let _ = self.request_tx.send(ProbeRequest {
            generation: self.generation,
            path,
        });
        self.pending_since = None;
    }

    pub fn receive_probe_results(&mut self, receiver: &Receiver<ProbeResponse>) {
        while let Ok(response) = receiver.try_recv() {
            if let Some(key) = CacheKey::for_path(response.path.clone()) {
                self.cache.insert(key, response.outcome.clone());
            }
            if response.generation == self.generation
                && self
                    .selected_file()
                    .is_some_and(|file| file.path == response.path)
            {
                self.outcome = Some(response.outcome);
                self.loading = false;
                self.selected_stream = 0;
                self.reset_track_edits();
            }
        }
    }

    pub fn receive_edit_results(&mut self, receiver: &Receiver<EditEvent>) {
        while let Ok(event) = receiver.try_recv() {
            if self.dialog != Some(Dialog::Processing) {
                continue;
            }
            match event {
                EditEvent::Progress(progress) => self.edit_progress = progress,
                EditEvent::Finished { path, outcome } => match outcome {
                    EditOutcome::Completed => {
                        self.edit_cancel = None;
                        self.clear_track_edits();
                        self.dialog = None;
                        self.edit_error = None;
                        self.edit_progress = None;
                        self.edit_started = None;
                        self.notice = Some("Track edits saved.".to_string());
                        self.cache.retain(|key, _| key.path != path);
                        self.queue_probe();
                        self.layer = Layer::Streams;
                    }
                    EditOutcome::Cancelled => {
                        self.edit_cancel = None;
                    }
                    EditOutcome::Failed(error) => {
                        self.edit_cancel = None;
                        self.dialog = Some(Dialog::Error);
                        self.edit_error = Some(error);
                        self.edit_progress = None;
                        self.edit_started = None;
                    }
                },
            }
        }
    }

    pub fn enter(&mut self) {
        if self.dialog.is_some() {
            return;
        }
        match self.layer {
            Layer::Files if self.stream_count() > 0 => {
                self.layer = Layer::Streams;
                self.selected_stream = 0;
            }
            Layer::Streams if self.selected_stream_info().is_some() => {
                self.layer = Layer::StreamDetails;
                self.details_scroll = 0;
                self.details_max_scroll = 0;
            }
            _ => {}
        }
    }

    pub fn back(&mut self) -> bool {
        match self.layer {
            Layer::StreamDetails => {
                self.layer = Layer::Streams;
                self.details_scroll = 0;
                self.details_max_scroll = 0;
                true
            }
            Layer::Streams => {
                self.layer = Layer::Files;
                true
            }
            Layer::Files => false,
        }
    }

    pub fn media_info(&self) -> Option<&MediaInfo> {
        match &self.outcome {
            Some(ProbeOutcome::Video(info)) => Some(info),
            _ => None,
        }
    }

    pub fn stream_count(&self) -> usize {
        self.stream_order.len()
    }

    pub fn selected_stream_info(
        &self,
    ) -> Option<&std::collections::BTreeMap<String, serde_json::Value>> {
        let info = self.media_info()?;
        let index = self.stream_order.get(self.selected_stream)?;
        stream_by_index(info, *index)
    }

    pub fn selected_stream_index(&self) -> Option<u64> {
        self.selected_stream_info().and_then(stream_index)
    }

    pub fn toggle_delete_selected_stream(&mut self) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        let Some(index) = self.selected_stream_index() else {
            self.show_error("This track has no usable stream index.");
            return;
        };
        if self.deleted_streams.remove(&index) {
            self.notice = None;
            return;
        }

        self.deleted_streams.insert(index);
        self.notice = None;
        self.selected_stream =
            (self.selected_stream + 1).min(self.stream_count().saturating_sub(1));
    }

    pub fn move_selected_stream(&mut self, direction: isize) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        let Some(index) = self.selected_stream_index() else {
            return;
        };
        if self.deleted_streams.contains(&index) {
            self.notice = Some("Unmark this track for deletion before moving it.".to_string());
            return;
        }
        let Some(target) = self.selected_stream.checked_add_signed(direction) else {
            return;
        };
        if target >= self.stream_order.len() {
            return;
        }
        let same_group = self.media_info().is_some_and(|info| {
            let current = stream_by_index(info, self.stream_order[self.selected_stream]);
            let target_stream = stream_by_index(info, self.stream_order[target]);
            current
                .zip(target_stream)
                .is_some_and(|(current, target)| stream_group(current) == stream_group(target))
        });
        if !same_group {
            return;
        }
        self.stream_order.swap(self.selected_stream, target);
        self.selected_stream = target;
        self.notice = None;
    }

    pub fn set_selected_stream_default(&mut self) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        let Some(index) = self.selected_stream_index() else {
            return;
        };
        if self.deleted_streams.contains(&index) {
            self.notice =
                Some("Unmark this track for deletion before making it default.".to_string());
            return;
        }
        let Some((kind, eligible)) = self.media_info().and_then(|info| {
            stream_by_index(info, index).map(|stream| {
                let kind = stream_kind(stream).unwrap_or("other").to_string();
                let eligible = matches!(kind.as_str(), "video" | "audio" | "subtitle")
                    && !(kind == "video" && crate::probe::is_attached_picture(stream));
                (kind, eligible)
            })
        }) else {
            return;
        };
        if !eligible {
            self.notice =
                Some("Only video, audio, and subtitle tracks can be default.".to_string());
            return;
        }
        let same_kind: Vec<_> = self
            .media_info()
            .into_iter()
            .flat_map(|info| &info.streams)
            .filter(|stream| stream_kind(stream) == Some(kind.as_str()))
            .filter_map(stream_index)
            .collect();
        for stream_index in same_kind {
            self.default_streams.remove(&stream_index);
        }
        self.default_streams.insert(index);
        self.notice = None;
    }

    pub fn request_save(&mut self) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        if !self.has_track_edits() {
            self.notice = Some("No track changes to save.".to_string());
            return;
        }
        let Some(info) = self.media_info() else {
            return;
        };
        let order = final_stream_order(info, &self.stream_order, &self.deleted_streams);
        let defaults = self
            .default_streams
            .difference(&self.deleted_streams)
            .copied()
            .collect();
        if let Err(error) = validate_edit(info, &order, &self.deleted_streams, &defaults) {
            self.show_error(error);
            return;
        }
        self.notice = None;
        self.dialog = Some(Dialog::ConfirmSave);
    }

    pub fn confirm_save(&mut self) {
        if self.dialog != Some(Dialog::ConfirmSave) {
            return;
        }
        let Some(path) = self.selected_file().map(|file| file.path.clone()) else {
            self.dialog = Some(Dialog::Error);
            self.edit_error = Some("The selected file is no longer available.".to_string());
            return;
        };
        let Some(info) = self.media_info() else {
            self.show_error("The selected file no longer has track information.");
            return;
        };
        let stream_order = final_stream_order(info, &self.stream_order, &self.deleted_streams);
        let default_streams = self
            .default_streams
            .difference(&self.deleted_streams)
            .copied()
            .collect();
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = EditRequest {
            path,
            stream_order,
            deleted_streams: self.deleted_streams.clone(),
            default_streams,
            cancelled: cancelled.clone(),
        };
        match self.edit_tx.send(request) {
            Ok(()) => {
                self.dialog = Some(Dialog::Processing);
                self.edit_error = None;
                self.edit_progress = None;
                self.edit_started = Some(Instant::now());
                self.edit_cancel = Some(cancelled);
            }
            Err(error) => {
                self.dialog = Some(Dialog::Error);
                self.edit_error = Some(format!("Could not start the edit worker: {error}"));
            }
        }
    }

    pub fn cancel_edit(&mut self) {
        if self.dialog != Some(Dialog::Processing) {
            return;
        }
        if let Some(cancelled) = self.edit_cancel.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
        self.clear_track_edits();
        self.dialog = None;
        self.edit_error = None;
        self.edit_progress = None;
        self.edit_started = None;
        self.notice = Some("Remux cancelled.".to_string());
        self.layer = Layer::Files;
    }

    pub fn dismiss_dialog(&mut self) {
        if self.dialog == Some(Dialog::Processing) {
            return;
        }
        self.dialog = None;
        self.edit_error = None;
        self.edit_progress = None;
        self.edit_started = None;
        self.edit_cancel = None;
    }

    pub fn show_keybindings(&mut self) {
        if self.dialog.is_none() {
            self.keybindings_scroll = 0;
            self.keybindings_max_scroll = 0;
            self.dialog = Some(Dialog::Keybindings);
        }
    }

    pub fn scroll_keybindings_down(&mut self, amount: u16) {
        self.keybindings_scroll =
            scroll_forward(self.keybindings_scroll, self.keybindings_max_scroll, amount);
    }

    pub fn scroll_keybindings_up(&mut self, amount: u16) {
        self.keybindings_scroll = scroll_backward(self.keybindings_scroll, amount);
    }

    pub fn set_keybindings_max_scroll(&mut self, maximum: u16) {
        self.keybindings_max_scroll = maximum;
        self.keybindings_scroll = self.keybindings_scroll.min(maximum);
    }

    pub fn scroll_down(&mut self) {
        self.details_scroll = scroll_forward(self.details_scroll, self.details_max_scroll, 10);
    }

    pub fn scroll_up(&mut self) {
        self.details_scroll = scroll_backward(self.details_scroll, 10);
    }

    pub fn set_details_max_scroll(&mut self, maximum: u16) {
        self.details_max_scroll = maximum;
        self.details_scroll = self.details_scroll.min(maximum);
    }

    fn clear_edit_state(&mut self) {
        self.clear_track_edits();
        self.dialog = None;
        self.notice = None;
        self.edit_error = None;
        self.edit_progress = None;
        self.edit_started = None;
    }

    fn reset_track_edits(&mut self) {
        let Some(info) = self.media_info() else {
            self.clear_track_edits();
            return;
        };
        let order = grouped_stream_indices(info)
            .into_iter()
            .filter_map(|position| info.streams.get(position).and_then(stream_index))
            .collect::<Vec<_>>();
        let defaults = info
            .streams
            .iter()
            .filter(|stream| is_default(stream))
            .filter_map(stream_index)
            .collect::<BTreeSet<_>>();
        self.stream_order = order.clone();
        self.original_stream_order = order;
        self.default_streams = defaults.clone();
        self.original_default_streams = defaults;
        self.deleted_streams.clear();
    }

    fn clear_track_edits(&mut self) {
        self.stream_order.clear();
        self.original_stream_order.clear();
        self.deleted_streams.clear();
        self.default_streams.clear();
        self.original_default_streams.clear();
    }

    pub fn changed_streams(&self) -> BTreeSet<u64> {
        changed_streams(
            &self.original_stream_order,
            &self.stream_order,
            &self.deleted_streams,
            &self.original_default_streams,
            &self.default_streams,
            self.media_info(),
        )
    }

    pub fn has_track_edits(&self) -> bool {
        !self.deleted_streams.is_empty() || !self.changed_streams().is_empty()
    }

    pub fn save_summary(&self) -> Vec<String> {
        let Some(info) = self.media_info() else {
            return Vec::new();
        };
        edit_summary(
            info,
            &self.original_stream_order,
            &self.stream_order,
            &self.deleted_streams,
            &self.original_default_streams,
            &self.default_streams,
        )
    }

    fn show_error(&mut self, error: impl Into<String>) {
        self.notice = None;
        self.edit_error = Some(error.into());
        self.dialog = Some(Dialog::Error);
    }
}

fn scroll_forward(current: u16, maximum: u16, amount: u16) -> u16 {
    current.saturating_add(amount).min(maximum)
}

fn scroll_backward(current: u16, amount: u16) -> u16 {
    current.saturating_sub(amount)
}

pub fn grouped_stream_indices(info: &MediaInfo) -> Vec<usize> {
    ["video", "audio", "subtitle"]
        .into_iter()
        .flat_map(|kind| {
            info.streams
                .iter()
                .enumerate()
                .filter_map(move |(index, stream)| {
                    (stream.get("codec_type").and_then(serde_json::Value::as_str) == Some(kind))
                        .then_some(index)
                })
        })
        .chain(
            info.streams
                .iter()
                .enumerate()
                .filter_map(|(index, stream)| {
                    (!matches!(
                        stream.get("codec_type").and_then(serde_json::Value::as_str),
                        Some("video" | "audio" | "subtitle")
                    ))
                    .then_some(index)
                }),
        )
        .collect()
}

fn stream_by_index(
    info: &MediaInfo,
    index: u64,
) -> Option<&std::collections::BTreeMap<String, serde_json::Value>> {
    info.streams
        .iter()
        .find(|stream| stream_index(stream) == Some(index))
}

fn stream_kind(stream: &std::collections::BTreeMap<String, serde_json::Value>) -> Option<&str> {
    stream.get("codec_type").and_then(serde_json::Value::as_str)
}

pub(crate) fn stream_group(
    stream: &std::collections::BTreeMap<String, serde_json::Value>,
) -> &'static str {
    match stream_kind(stream) {
        Some("video") => "video",
        Some("audio") => "audio",
        Some("subtitle") => "subtitle",
        _ => "other",
    }
}

fn is_default(stream: &std::collections::BTreeMap<String, serde_json::Value>) -> bool {
    stream
        .get("disposition")
        .and_then(serde_json::Value::as_object)
        .and_then(|disposition| disposition.get("default"))
        .and_then(serde_json::Value::as_i64)
        == Some(1)
}

pub(crate) fn final_stream_order(
    info: &MediaInfo,
    staged_order: &[u64],
    deleted: &BTreeSet<u64>,
) -> Vec<u64> {
    let mut queues: BTreeMap<&'static str, VecDeque<u64>> = BTreeMap::new();
    for index in staged_order.iter().filter(|index| !deleted.contains(index)) {
        if let Some(stream) = stream_by_index(info, *index) {
            queues
                .entry(stream_group(stream))
                .or_default()
                .push_back(*index);
        }
    }

    info.streams
        .iter()
        .filter_map(|stream| {
            let index = stream_index(stream)?;
            if deleted.contains(&index) {
                return None;
            }
            queues.get_mut(stream_group(stream))?.pop_front()
        })
        .collect()
}

fn effective_group_order(
    info: &MediaInfo,
    order: &[u64],
    deleted: &BTreeSet<u64>,
    group: &str,
) -> Vec<u64> {
    order
        .iter()
        .filter(|index| !deleted.contains(index))
        .filter(|index| {
            stream_by_index(info, **index).is_some_and(|stream| stream_group(stream) == group)
        })
        .copied()
        .collect()
}

fn changed_streams(
    original_order: &[u64],
    staged_order: &[u64],
    deleted: &BTreeSet<u64>,
    original_defaults: &BTreeSet<u64>,
    staged_defaults: &BTreeSet<u64>,
    info: Option<&MediaInfo>,
) -> BTreeSet<u64> {
    let Some(info) = info else {
        return BTreeSet::new();
    };
    let mut changed = BTreeSet::new();
    for group in ["video", "audio", "subtitle", "other"] {
        let original = effective_group_order(info, original_order, deleted, group);
        let staged = effective_group_order(info, staged_order, deleted, group);
        for (position, index) in staged.iter().enumerate() {
            if original.get(position) != Some(index) {
                changed.insert(*index);
                if let Some(original_index) = original.get(position) {
                    changed.insert(*original_index);
                }
            }
        }
    }
    for index in original_order
        .iter()
        .filter(|index| !deleted.contains(index))
    {
        if original_defaults.contains(index) != staged_defaults.contains(index) {
            changed.insert(*index);
        }
    }
    changed
}

fn edit_summary(
    info: &MediaInfo,
    original_order: &[u64],
    staged_order: &[u64],
    deleted: &BTreeSet<u64>,
    original_defaults: &BTreeSet<u64>,
    staged_defaults: &BTreeSet<u64>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for group in ["video", "audio", "subtitle", "other"] {
        let original = effective_group_order(info, original_order, deleted, group);
        let staged = effective_group_order(info, staged_order, deleted, group);
        let moved = staged
            .iter()
            .enumerate()
            .filter(|(position, index)| original.get(*position) != Some(*index))
            .count();
        if moved > 0 {
            lines.push(format!(
                "Moving {moved} {}",
                track_count_label(group, moved)
            ));
        }
    }

    for group in ["video", "audio", "subtitle", "other"] {
        let count = deleted
            .iter()
            .filter(|index| {
                stream_by_index(info, **index).is_some_and(|stream| stream_group(stream) == group)
            })
            .count();
        if count > 0 {
            lines.push(format!(
                "Deleting {count} {}",
                track_count_label(group, count)
            ));
        }
    }

    for kind in ["video", "audio", "subtitle"] {
        let original = info
            .streams
            .iter()
            .filter(|stream| stream_kind(stream) == Some(kind))
            .filter_map(stream_index)
            .filter(|index| !deleted.contains(index) && original_defaults.contains(index))
            .collect::<BTreeSet<_>>();
        let staged = info
            .streams
            .iter()
            .filter(|stream| stream_kind(stream) == Some(kind))
            .filter_map(stream_index)
            .filter(|index| !deleted.contains(index) && staged_defaults.contains(index))
            .collect::<BTreeSet<_>>();
        if original != staged {
            lines.push(format!("Changing the default {kind} track"));
        }
    }
    lines
}

fn track_count_label(group: &str, count: usize) -> String {
    format!("{group} track{}", if count == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;

    fn media(streams: serde_json::Value) -> MediaInfo {
        MediaInfo::from_json(serde_json::json!({"streams": streams})).unwrap()
    }

    fn test_app(info: MediaInfo) -> App {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-app-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel::<ProbeRequest>();
        let (edit_tx, _) = std::sync::mpsc::channel::<EditRequest>();
        let mut app = App::new(directory, probe_tx, edit_tx).unwrap();
        app.outcome = Some(ProbeOutcome::Video(info));
        app.loading = false;
        app.reset_track_edits();
        app.layer = Layer::Streams;
        app
    }

    #[test]
    fn scroll_forward_should_add_amount_when_result_is_below_maximum() {
        // Arrange
        let current = 4;
        let maximum = 10;
        let amount = 3;

        // Act
        let result = scroll_forward(current, maximum, amount);

        // Assert
        assert_that!(result).is_equal_to(7);
    }

    #[test]
    fn scroll_forward_should_clamp_to_maximum_when_amount_exceeds_remaining_range() {
        // Arrange
        let current = 7;
        let maximum = 10;
        let amount = 10;

        // Act
        let result = scroll_forward(current, maximum, amount);

        // Assert
        assert_that!(result).is_equal_to(10);
    }

    #[test]
    fn scroll_forward_should_clamp_to_maximum_when_current_value_is_above_maximum() {
        // Arrange
        let current = u16::MAX;
        let maximum = 10;
        let amount = 1;

        // Act
        let result = scroll_forward(current, maximum, amount);

        // Assert
        assert_that!(result).is_equal_to(10);
    }

    #[test]
    fn scroll_backward_should_subtract_amount_when_result_is_above_zero() {
        // Arrange
        let current = 7;
        let amount = 3;

        // Act
        let result = scroll_backward(current, amount);

        // Assert
        assert_that!(result).is_equal_to(4);
    }

    #[test]
    fn scroll_backward_should_return_zero_when_amount_exceeds_current_value() {
        // Arrange
        let current = 2;
        let amount = 10;

        // Act
        let result = scroll_backward(current, amount);

        // Assert
        assert_that!(result).is_equal_to(0);
    }

    #[test]
    fn final_stream_order_should_change_only_same_group_positions_when_tracks_are_reordered() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"},
            {"index": 3, "codec_type": "audio"},
            {"index": 4, "codec_type": "attachment"}
        ]));
        let staged = [0, 3, 1, 2, 4];

        // Act
        let result = final_stream_order(&info, &staged, &BTreeSet::new());

        // Assert
        assert_that!(result).contains_exactly_in_given_order([0, 3, 2, 1, 4]);
    }

    #[test]
    fn final_stream_order_should_preserve_surviving_group_positions_when_reordered_track_is_deleted()
     {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"},
            {"index": 3, "codec_type": "audio"},
            {"index": 4, "codec_type": "attachment"}
        ]));
        let staged = [0, 3, 1, 2, 4];
        let deleted = BTreeSet::from([3]);

        // Act
        let result = final_stream_order(&info, &staged, &deleted);

        // Assert
        assert_that!(result).contains_exactly_in_given_order([0, 1, 2, 4]);
    }

    #[test]
    fn edit_summary_should_group_actions_by_track_type_when_multiple_edits_are_staged() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video"},
            {"index": 1, "codec_type": "audio"},
            {"index": 2, "codec_type": "subtitle"},
            {"index": 3, "codec_type": "audio"}
        ]));

        // Act
        let lines = edit_summary(
            &info,
            &[0, 1, 3, 2],
            &[0, 3, 1, 2],
            &BTreeSet::from([2]),
            &BTreeSet::from([1]),
            &BTreeSet::from([3]),
        );

        // Assert
        assert_that!(lines).contains_exactly_in_given_order([
            "Moving 2 audio tracks".to_string(),
            "Deleting 1 subtitle track".to_string(),
            "Changing the default audio track".to_string(),
        ]);
    }

    #[test]
    fn move_selected_stream_should_reorder_tracks_and_follow_selection_when_neighbor_has_same_type()
    {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "disposition": {"default": 0}},
            {"index": 3, "codec_type": "subtitle", "disposition": {"default": 0}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.selected_stream = 1;

        // Act
        app.move_selected_stream(1);

        // Assert
        assert_that!(&app.stream_order).contains_exactly_in_given_order([0, 2, 1, 3]);
        assert_that!(app.selected_stream).is_equal_to(2);
        assert_that!(app.changed_streams()).is_equal_to(BTreeSet::from([1, 2]));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn move_selected_stream_should_clear_order_changes_when_track_is_moved_to_original_position() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "disposition": {"default": 0}},
            {"index": 3, "codec_type": "subtitle", "disposition": {"default": 0}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.selected_stream = 1;
        app.move_selected_stream(1);

        // Act
        app.move_selected_stream(-1);

        // Assert
        assert_that!(&app.stream_order).contains_exactly_in_given_order([0, 1, 2, 3]);
        assert_that!(app.changed_streams()).is_empty();

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn set_selected_stream_default_should_replace_existing_default_when_track_has_same_type() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "disposition": {"default": 0}},
            {"index": 3, "codec_type": "subtitle", "disposition": {"default": 0}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.selected_stream = 2;

        // Act
        app.set_selected_stream_default();

        // Assert
        assert_that!(app.default_streams.clone()).is_equal_to(BTreeSet::from([0, 2]));
        assert_that!(app.changed_streams()).is_equal_to(BTreeSet::from([1, 2]));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn toggle_delete_selected_stream_should_mark_track_when_track_is_not_already_marked() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "disposition": {"default": 0}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.selected_stream = 2;

        // Act
        app.toggle_delete_selected_stream();

        // Assert
        assert_that!(app.deleted_streams).is_equal_to(BTreeSet::from([2]));

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn move_selected_stream_should_show_notice_when_selected_track_is_marked_for_deletion() {
        // Arrange
        let info = media(serde_json::json!([
            {"index": 0, "codec_type": "video", "disposition": {"default": 1}},
            {"index": 1, "codec_type": "audio", "disposition": {"default": 1}},
            {"index": 2, "codec_type": "audio", "disposition": {"default": 0}}
        ]));
        let mut app = test_app(info);
        let directory = app.directory.clone();
        app.selected_stream = 2;
        app.toggle_delete_selected_stream();
        app.selected_stream = 2;

        // Act
        app.move_selected_stream(-1);

        // Assert
        assert_that!(app.notice.as_deref().unwrap()).contains("Unmark");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }
}
