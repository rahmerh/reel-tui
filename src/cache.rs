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

        let Ok(file) = File::open(&path) else {
            return Self::default();
        };

        let reader = BufReader::new(file);
        serde_json::from_reader(reader).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(dir) = Self::cache_dir() else {
            return Ok(());
        };
        fs::create_dir_all(&dir)?;
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
            Some(entry.outcome.clone())
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

    #[test]
    fn disk_cache_should_store_retrieve_and_invalidate_on_length_mismatch() {
        let mut cache = DiskCache::default();
        let path = PathBuf::from("/media/video.mkv");
        let now = SystemTime::now();

        let info = MediaInfo {
            format: BTreeMap::from([("duration".to_string(), json!("120.0"))]),
            streams: vec![],
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
