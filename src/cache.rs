use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use crate::probe::ProbeOutcome;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskCacheEntry {
    pub length: u64,
    pub modified: Option<SystemTime>,
    pub outcome: ProbeOutcome,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiskCache {
    pub entries: HashMap<PathBuf, DiskCacheEntry>,
}

impl DiskCache {
    pub fn cache_dir() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("XDG_CACHE_HOME") {
            let path = PathBuf::from(path);
            if !path.as_os_str().is_empty() {
                return Some(path.join("reel-tui"));
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let path = PathBuf::from(home);
            if !path.as_os_str().is_empty() {
                return Some(path.join(".cache").join("reel-tui"));
            }
        }
        None
    }

    pub fn cache_file_path() -> Option<PathBuf> {
        Self::cache_dir().map(|dir| dir.join("probe_cache.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::cache_file_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    /// Reads a cache file, discarding anything unreadable or malformed — a stale cache is
    /// never worth failing a launch over. Entries that no longer describe a real video
    /// are dropped so a file that once probed as video is re-probed instead of trusted.
    pub fn load_from(path: &Path) -> Self {
        let Ok(file) = File::open(path) else {
            return Self::default();
        };

        let reader = BufReader::new(file);
        let mut cache: DiskCache = serde_json::from_reader(reader).unwrap_or_default();
        cache.entries.retain(|_, entry| match &entry.outcome {
            ProbeOutcome::Video(info) => info.streams.iter().any(|stream| {
                stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("video")
                    && !crate::probe::is_attached_picture(stream)
                    && !crate::probe::is_still_image(stream, &info.format)
            }),
            _ => true,
        });
        cache
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(dir) = Self::cache_dir() else {
            return Ok(());
        };
        self.save_to(&dir)
    }

    /// Writes the cache into `dir`, via a temporary file so a crash mid-write leaves the
    /// previous cache intact rather than a truncated one.
    pub fn save_to(&self, dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        let file_path = dir.join("probe_cache.json");
        let temp_path = dir.join(".probe_cache.json.tmp");

        let file = File::create(&temp_path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer(writer, self)?;
        fs::rename(temp_path, file_path)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get(
        &self,
        path: &Path,
        length: u64,
        modified: Option<SystemTime>,
    ) -> Option<ProbeOutcome> {
        let entry = self.entries.get(path)?;
        if entry.length == length && entry.modified == modified {
            match &entry.outcome {
                ProbeOutcome::Video(info) => {
                    let valid_video = info.streams.iter().any(|stream| {
                        stream.get("codec_type").and_then(serde_json::Value::as_str)
                            == Some("video")
                            && !crate::probe::is_attached_picture(stream)
                            && !crate::probe::is_still_image(stream, &info.format)
                    });
                    if valid_video {
                        Some(entry.outcome.clone())
                    } else {
                        None
                    }
                }
                other => Some(other.clone()),
            }
        } else {
            None
        }
    }

    pub fn insert(
        &mut self,
        path: PathBuf,
        length: u64,
        modified: Option<SystemTime>,
        outcome: ProbeOutcome,
    ) {
        self.entries.insert(
            path,
            DiskCacheEntry {
                length,
                modified,
                outcome,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use serde_json::json;

    use super::*;
    use crate::probe::MediaInfo;

    fn scratch(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-cache-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn video(codec_type: &str) -> ProbeOutcome {
        ProbeOutcome::Video(MediaInfo {
            format: BTreeMap::from([("format_name".to_string(), json!("matroska"))]),
            streams: vec![BTreeMap::from([
                ("codec_type".to_string(), json!(codec_type)),
                ("codec_name".to_string(), json!("h264")),
            ])],
            chapters: vec![],
        })
    }

    #[test]
    fn disk_cache_should_survive_a_round_trip_through_a_file() {
        // Arrange
        let directory = scratch("round-trip");
        let path = PathBuf::from("/media/video.mkv");
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut cache = DiskCache::default();
        cache.insert(path.clone(), 1024, Some(modified), video("video"));

        // Act
        cache.save_to(&directory).unwrap();
        let loaded = DiskCache::load_from(&directory.join("probe_cache.json"));

        // Assert: the fingerprint survives the trip, so a reload still serves the entry
        // and still rejects a file whose length has moved on.
        assert!(loaded.get(&path, 1024, Some(modified)).is_some());
        assert!(loaded.get(&path, 2048, Some(modified)).is_none());

        // And the temporary file is not left behind next to the real one.
        assert!(!directory.join(".probe_cache.json.tmp").exists());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_cache_should_start_empty_when_the_file_is_missing_or_unreadable() {
        // Arrange
        let directory = scratch("unreadable");
        let path = directory.join("probe_cache.json");

        // Act / Assert: a missing cache and a corrupt one are both recoverable states —
        // `reel` must launch either way rather than propagate the error.
        assert!(DiskCache::load_from(&path).entries.is_empty());

        fs::write(&path, b"{ this is not json").unwrap();
        assert!(DiskCache::load_from(&path).entries.is_empty());

        fs::write(&path, b"{\"entries\":{}}").unwrap();
        assert!(DiskCache::load_from(&path).entries.is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_cache_should_drop_a_stored_entry_that_no_longer_describes_a_video() {
        // Arrange: an entry that was cached as a video but holds no video stream — the
        // shape a still image or an audio file leaves behind after a probe-rule change.
        let directory = scratch("not-a-video");
        let path = PathBuf::from("/media/song.mkv");
        let mut cache = DiskCache::default();
        cache.insert(path.clone(), 10, None, video("audio"));

        // Act
        cache.save_to(&directory).unwrap();
        let loaded = DiskCache::load_from(&directory.join("probe_cache.json"));

        // Assert: dropped on load, so the file is re-probed instead of misreported.
        assert!(loaded.entries.is_empty());
        // `get` refuses it too, in case it is still in a cache held in memory.
        assert!(cache.get(&path, 10, None).is_none());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_cache_should_keep_a_non_video_outcome_verbatim() {
        // Arrange: `NotVideo` and `Error` are answers in their own right, not stale
        // video entries, and re-probing them every launch is what the cache exists to
        // avoid on network mounts.
        let directory = scratch("not-video-outcome");
        let mut cache = DiskCache::default();
        cache.insert(
            PathBuf::from("/media/notes.txt"),
            4,
            None,
            ProbeOutcome::NotVideo("no video stream".to_string()),
        );
        cache.insert(
            PathBuf::from("/media/broken.mkv"),
            4,
            None,
            ProbeOutcome::Error("ffprobe failed".to_string()),
        );

        // Act
        cache.save_to(&directory).unwrap();
        let loaded = DiskCache::load_from(&directory.join("probe_cache.json"));

        // Assert
        assert_eq!(loaded.entries.len(), 2);
        assert!(matches!(
            loaded.get(&PathBuf::from("/media/notes.txt"), 4, None),
            Some(ProbeOutcome::NotVideo(_)),
        ));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disk_cache_should_store_retrieve_and_invalidate_on_length_mismatch() {
        let mut cache = DiskCache::default();
        let path = PathBuf::from("/media/video.mkv");
        let now = SystemTime::now();

        let info = MediaInfo {
            format: BTreeMap::from([
                ("format_name".to_string(), json!("matroska")),
                ("duration".to_string(), json!("120.0")),
            ]),
            streams: vec![BTreeMap::from([
                ("codec_type".to_string(), json!("video")),
                ("codec_name".to_string(), json!("h264")),
            ])],
            chapters: vec![],
        };
        let outcome = ProbeOutcome::Video(info);

        cache.insert(path.clone(), 1024, Some(now), outcome);

        // Retrieve valid entry
        let hit = cache.get(&path, 1024, Some(now));
        assert!(hit.is_some());

        // Invalidate on size mismatch
        let miss_size = cache.get(&path, 2048, Some(now));
        assert!(miss_size.is_none());

        // Invalidate on time mismatch
        let miss_time = cache.get(&path, 1024, Some(now + Duration::from_secs(10)));
        assert!(miss_time.is_none());
    }
}
