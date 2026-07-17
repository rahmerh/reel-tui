use std::{
    collections::{BTreeSet, HashMap},
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
    edit::{EditEvent, EditOutcome, EditRequest, stream_index, validate_deletion},
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
    ConfirmDelete,
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
    pub marked_streams: BTreeSet<u64>,
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
            marked_streams: BTreeSet::new(),
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
                        self.marked_streams.clear();
                        self.dialog = None;
                        self.edit_error = None;
                        self.edit_progress = None;
                        self.edit_started = None;
                        self.notice = Some("Selected tracks deleted.".to_string());
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
        self.media_info().map_or(0, |info| info.streams.len())
    }

    pub fn selected_stream_info(
        &self,
    ) -> Option<&std::collections::BTreeMap<String, serde_json::Value>> {
        let info = self.media_info()?;
        grouped_stream_indices(info)
            .get(self.selected_stream)
            .and_then(|index| info.streams.get(*index))
    }

    pub fn selected_stream_index(&self) -> Option<u64> {
        self.selected_stream_info().and_then(stream_index)
    }

    pub fn toggle_selected_stream(&mut self) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        let Some(index) = self.selected_stream_index() else {
            self.show_error("This track has no usable stream index.");
            return;
        };
        if self.marked_streams.remove(&index) {
            self.notice = None;
            return;
        }

        self.marked_streams.insert(index);
        self.notice = None;
        self.selected_stream =
            (self.selected_stream + 1).min(self.stream_count().saturating_sub(1));
    }

    pub fn request_delete(&mut self) {
        if self.layer != Layer::Streams || self.dialog.is_some() {
            return;
        }
        if self.marked_streams.is_empty() {
            self.show_error("Select one or more tracks with Space first.");
            return;
        }
        if let Some(info) = self.media_info()
            && let Err(error) = validate_deletion(info, &self.marked_streams)
        {
            self.show_error(error);
            return;
        }
        self.notice = None;
        self.dialog = Some(Dialog::ConfirmDelete);
    }

    pub fn confirm_delete(&mut self) {
        if self.dialog != Some(Dialog::ConfirmDelete) {
            return;
        }
        let Some(path) = self.selected_file().map(|file| file.path.clone()) else {
            self.dialog = Some(Dialog::Error);
            self.edit_error = Some("The selected file is no longer available.".to_string());
            return;
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = EditRequest {
            path,
            stream_indices: self.marked_streams.clone(),
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
        self.marked_streams.clear();
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
        self.marked_streams.clear();
        self.dialog = None;
        self.notice = None;
        self.edit_error = None;
        self.edit_progress = None;
        self.edit_started = None;
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

#[cfg(test)]
mod tests {
    use super::{scroll_backward, scroll_forward};

    #[test]
    fn scrolling_stays_within_its_bounds() {
        assert_eq!(scroll_forward(4, 10, 3), 7);
        assert_eq!(scroll_forward(7, 10, 10), 10);
        assert_eq!(scroll_forward(u16::MAX, 10, 1), 10);
        assert_eq!(scroll_backward(7, 3), 4);
        assert_eq!(scroll_backward(2, 10), 0);
    }
}
