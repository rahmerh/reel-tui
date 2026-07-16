use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    time::{Duration, Instant},
};

use anyhow::Result;
use ratatui::widgets::ListState;

use crate::{
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
    pub layer: Layer,
    pub selected_stream: usize,
    pub scan_error: Option<String>,
    request_tx: Sender<ProbeRequest>,
    generation: u64,
    pending_since: Option<Instant>,
    cache: HashMap<CacheKey, ProbeOutcome>,
}

impl App {
    pub fn new(directory: PathBuf, request_tx: Sender<ProbeRequest>) -> Result<Self> {
        let mut app = Self {
            directory,
            files: Vec::new(),
            list_state: ListState::default(),
            outcome: None,
            loading: false,
            details_scroll: 0,
            details_max_scroll: 0,
            layer: Layer::Files,
            selected_stream: 0,
            scan_error: None,
            request_tx,
            generation: 0,
            pending_since: None,
            cache: HashMap::new(),
        };
        app.refresh()?;
        Ok(app)
    }

    pub fn refresh(&mut self) -> Result<()> {
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
        if self.layer == Layer::Streams {
            self.selected_stream = 0;
            return;
        }
        if !self.files.is_empty() {
            self.select(0);
        }
    }

    pub fn select_last(&mut self) {
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

    pub fn enter(&mut self) {
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

    pub fn scroll_down(&mut self) {
        self.details_scroll = self
            .details_scroll
            .saturating_add(10)
            .min(self.details_max_scroll);
    }

    pub fn scroll_up(&mut self) {
        self.details_scroll = self.details_scroll.saturating_sub(10);
    }

    pub fn set_details_max_scroll(&mut self, maximum: u16) {
        self.details_max_scroll = maximum;
        self.details_scroll = self.details_scroll.min(maximum);
    }
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
