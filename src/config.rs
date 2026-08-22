use std::{fs, path::PathBuf, time::Duration};

use serde::Deserialize;

/// Worker-pool sizing loaded from `~/.config/reel/config.toml`. See the "Parallelism
/// analysis" in the multi-file-staging design for why transcode and remux work get
/// independent limits, and why network mounts get their own (lower) pair: transcode
/// work already saturates multiple CPU cores per file, so running several at once
/// contends rather than speeds anything up, while remux (`-c copy`) work is I/O-bound
/// and benefits from real concurrency — except over a network mount, where this
/// codebase already treats concurrent I/O to the same share as something to avoid
/// (adaptive polling interval, local-scratch remuxing).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    pub notifications: bool,
    pub transcode_workers: usize,
    pub remux_workers: usize,
    pub network_transcode_workers: usize,
    pub network_remux_workers: usize,
    /// How many media files' preview frames the timing page's cache may hold before the
    /// least recently used one is dropped.
    pub preview_cache_tracks: usize,
    /// Whether opening the timing page renders every cue's frame in the background.
    pub preview_prefetch: bool,
    /// The same, for media on a network mount. Off by default: a feature-length track is
    /// a thousand-odd accurate seeks, and doing that across NFS or SMB unasked is not a
    /// trade the user made.
    pub network_preview_prefetch: bool,
    /// How many frames a second the timing page's scrub playback aims for.
    ///
    /// The escape hatch for a terminal that cannot keep up with the picture: over ssh, or
    /// inside tmux, the bytes for a halfblocks frame have further to travel than they do
    /// locally, and lowering this is what turns a playback that stutters into one that is
    /// merely chunky. A ceiling rather than a promise — a span too large to hold in memory
    /// at this rate is decoded at a lower one, and so is one whose source holds fewer frames
    /// a second than this (see `preview::source_capped_fps`).
    pub playback_fps: u32,
    /// How much of the media either side of the cue that playback covers.
    pub playback_pad: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            notifications: true,
            transcode_workers: 1,
            remux_workers: 5,
            network_transcode_workers: 1,
            network_remux_workers: 1,
            preview_cache_tracks: DEFAULT_PREVIEW_CACHE_TRACKS,
            preview_prefetch: true,
            network_preview_prefetch: false,
            playback_fps: DEFAULT_PLAYBACK_FPS,
            playback_pad: DEFAULT_PLAYBACK_PAD,
        }
    }
}

/// Frames a second a scrub playback aims for.
///
/// Thirty rather than something cheaper because the point of the playback is judging
/// whether a line lands with the speech, and below about twenty the gaps between frames
/// become the thing you are measuring against instead of the picture.
const DEFAULT_PLAYBACK_FPS: u32 = 30;

/// The floor and ceiling a mistyped rate is held between. Five is where consecutive frames
/// stop reading as motion at all; sixty is past what any terminal keeps up with, and
/// nothing above it would be drawn anyway.
///
/// Public because the timing page's preview-settings popup adjusts the same value for the
/// session, and it must stop at exactly the limits a config file is held to rather than at
/// a second copy of them that can drift.
pub const MIN_PLAYBACK_FPS: u32 = 5;
pub const MAX_PLAYBACK_FPS: u32 = 60;

/// How much of the media either side of the cue a scrub playback covers.
///
/// A second is long enough to hear the speech start before the line is due and to hear it
/// finish after, which is the judgement being made; much more and the span takes longer to
/// decode and longer to sit through for the same answer.
///
/// Lowered from two when playbacks started being decoded at the terminal's own pixel
/// resolution. The span's length multiplies its cost — see `preview::affordable_fps` — so
/// two seconds either side was buying run-up nobody watches at the price of the frame rate
/// during the part they do.
const DEFAULT_PLAYBACK_PAD: Duration = Duration::from_secs(1);

/// A pad of zero is meaningful — play exactly the cue and nothing else — so only the top
/// is capped. Ten seconds either side is already a twenty-second span.
///
/// A `Duration` rather than the seconds it is read from, so the popup and the config loader
/// share one number instead of one holding a float and the other a conversion of it. The
/// loader converts *to* seconds at the single point it compares against a parsed float.
pub const MAX_PLAYBACK_PAD: Duration = Duration::from_secs(10);

/// A typo'd or malicious huge value in the config file must not spawn an unreasonable
/// number of threads.
const MAX_WORKERS: usize = 16;

/// Whole media files rather than megabytes, because a track is what gets used and half a
/// track is nearly worthless — see `framecache`'s module docs for why the unit of eviction
/// has to be the unit of use.
///
/// Ten is the last ten films you opened the timing page on. Disk follows from the content
/// rather than from a number: a feature-length track is a thousand or two cues at under a
/// hundred kilobytes a frame, so ten of them is on the order of a gigabyte, and a set of
/// unusually long and densely subtitled ones perhaps three.
const DEFAULT_PREVIEW_CACHE_TRACKS: usize = 10;

/// A ceiling a mistyped value cannot cross. Zero is allowed and means the cache keeps
/// nothing beyond the page that is open, which is a real answer for a machine short on
/// disk — the open track is never evicted, since the pass is about to render into it.
const MAX_PREVIEW_CACHE_TRACKS: usize = 1024;

#[derive(Deserialize, Default)]
struct RawConfig {
    notifications: Option<RawNotifications>,
    workers: Option<RawWorkers>,
    preview: Option<RawPreview>,
}

#[derive(Deserialize, Default)]
struct RawNotifications {
    enabled: Option<bool>,
}

#[derive(Deserialize, Default)]
struct RawWorkers {
    transcode: Option<usize>,
    remux: Option<usize>,
    network: Option<RawNetworkWorkers>,
}

#[derive(Deserialize, Default)]
struct RawPreview {
    cache_tracks: Option<usize>,
    prefetch: Option<RawPrefetch>,
    playback: Option<RawPlayback>,
}

#[derive(Deserialize, Default)]
struct RawPlayback {
    fps: Option<u32>,
    /// Seconds, as a float, so half a second is expressible. `Duration` itself is not
    /// deserialised directly — TOML has no duration type, and the field being plainly a
    /// number of seconds is what makes the config file readable.
    pad: Option<f64>,
}

#[derive(Deserialize, Default)]
struct RawPrefetch {
    enabled: Option<bool>,
    network: Option<bool>,
}

#[derive(Deserialize, Default)]
struct RawNetworkWorkers {
    transcode: Option<usize>,
    remux: Option<usize>,
}

impl Config {
    /// Mirrors `DiskCache::cache_dir()` (cache.rs): check `XDG_CONFIG_HOME`, else fall
    /// back to `$HOME/.config`, and return `None` (never panic) if neither is set.
    /// Deliberately `reel`, not `reel-tui` like the cache directory — the user asked
    /// for this exact path.
    pub fn config_dir() -> Option<PathBuf> {
        Self::config_dir_from(
            std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }

    /// Resolves the config directory from the two environment variables that can name
    /// it. Split out from `config_dir` for the same reason as
    /// `DiskCache::cache_dir_from`: mutating the process environment from a test is racy
    /// under the threaded test runner and `unsafe` in this edition. An empty value is
    /// treated as unset rather than as the relative path it literally is.
    fn config_dir_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
        if let Some(path) = xdg_config_home.filter(|path| !path.is_empty()) {
            return Some(PathBuf::from(path).join("reel"));
        }
        if let Some(home) = home.filter(|home| !home.is_empty()) {
            return Some(PathBuf::from(home).join(".config").join("reel"));
        }
        None
    }

    pub fn config_file_path() -> Option<PathBuf> {
        Self::config_dir().map(|dir| dir.join("config.toml"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::config_file_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    /// Reads a config file, falling back to defaults for anything missing, malformed,
    /// or unreadable — a bad config is never worth failing a launch over, mirroring
    /// `DiskCache::load_from`'s forgiving pattern. Fields omitted from the TOML (down
    /// to individual leaf values) keep their own default rather than resetting the
    /// whole file to defaults, so a user can set just one value.
    pub fn load_from(path: &std::path::Path) -> Self {
        let defaults = Self::default();
        let Ok(contents) = fs::read_to_string(path) else {
            return defaults;
        };
        let raw: RawConfig = toml::from_str(&contents).unwrap_or_default();
        let notifications = raw.notifications.unwrap_or_default();
        let workers = raw.workers.unwrap_or_default();
        let network = workers.network.unwrap_or_default();
        let preview = raw.preview.unwrap_or_default();
        let prefetch = preview.prefetch.unwrap_or_default();
        let playback = preview.playback.unwrap_or_default();
        Self {
            notifications: notifications.enabled.unwrap_or(defaults.notifications),
            transcode_workers: clamp(workers.transcode.unwrap_or(defaults.transcode_workers)),
            remux_workers: clamp(workers.remux.unwrap_or(defaults.remux_workers)),
            network_transcode_workers: clamp(
                network
                    .transcode
                    .unwrap_or(defaults.network_transcode_workers),
            ),
            network_remux_workers: clamp(network.remux.unwrap_or(defaults.network_remux_workers)),
            preview_cache_tracks: preview
                .cache_tracks
                .unwrap_or(defaults.preview_cache_tracks)
                .min(MAX_PREVIEW_CACHE_TRACKS),
            preview_prefetch: prefetch.enabled.unwrap_or(defaults.preview_prefetch),
            network_preview_prefetch: prefetch
                .network
                .unwrap_or(defaults.network_preview_prefetch),
            playback_fps: playback
                .fps
                .unwrap_or(defaults.playback_fps)
                .clamp(MIN_PLAYBACK_FPS, MAX_PLAYBACK_FPS),
            playback_pad: playback
                .pad
                // NaN first, because `f64::min` answers with the *other* operand for it —
                // so a `pad = nan` would clamp to the maximum and give a twenty-second
                // playback rather than falling back to the default.
                .filter(|pad| !pad.is_nan())
                // `try_from_secs_f64` rather than `from_secs_f64`, which panics outright on
                // a negative — which a hand-written config file can perfectly well hold,
                // and which must not take the process down at launch.
                .and_then(|pad| {
                    Duration::try_from_secs_f64(pad.min(MAX_PLAYBACK_PAD.as_secs_f64())).ok()
                })
                .unwrap_or(defaults.playback_pad),
        }
    }

    /// Whether to render a whole track's frames in the background, given whether the
    /// target directory is on a network mount.
    pub fn effective_prefetch(&self, is_network_mount: bool) -> bool {
        if is_network_mount {
            self.network_preview_prefetch
        } else {
            self.preview_prefetch
        }
    }

    /// The worker counts to actually pass to `spawn_edit_worker_pools`, given whether
    /// the target directory is on a network mount.
    pub fn effective_workers(&self, is_network_mount: bool) -> (usize, usize) {
        if is_network_mount {
            (self.network_transcode_workers, self.network_remux_workers)
        } else {
            (self.transcode_workers, self.remux_workers)
        }
    }
}

fn clamp(value: usize) -> usize {
    value.clamp(1, MAX_WORKERS)
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-config-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn config_should_default_when_the_file_is_missing() {
        let directory = scratch("missing");
        let config = Config::load_from(&directory.join("config.toml"));
        assert_eq!(config, Config::default());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn config_should_default_when_the_file_is_malformed() {
        let directory = scratch("malformed");
        let path = directory.join("config.toml");
        fs::write(&path, b"this is not [ valid toml").unwrap();
        let config = Config::load_from(&path);
        assert_eq!(config, Config::default());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn config_should_parse_every_configured_value() {
        let directory = scratch("full");
        let path = directory.join("config.toml");
        fs::write(
            &path,
            b"[notifications]\nenabled = false\n\n[workers]\ntranscode = 2\nremux = 8\n\n[workers.network]\ntranscode = 3\nremux = 4\n\n[preview]\ncache_tracks = 4\n\n[preview.prefetch]\nenabled = false\nnetwork = true\n\n[preview.playback]\nfps = 24\npad = 1.5\n",
        )
        .unwrap();
        let config = Config::load_from(&path);
        assert_eq!(
            config,
            Config {
                notifications: false,
                transcode_workers: 2,
                remux_workers: 8,
                network_transcode_workers: 3,
                network_remux_workers: 4,
                preview_cache_tracks: 4,
                preview_prefetch: false,
                network_preview_prefetch: true,
                playback_fps: 24,
                playback_pad: Duration::from_millis(1500),
            }
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn config_should_keep_individual_defaults_for_unspecified_fields() {
        // Only `workers.remux` is set; every other value (including the whole
        // `[workers.network]` table) must keep its own default rather than the file's
        // mere presence resetting everything else to some blanket fallback.
        let directory = scratch("partial");
        let path = directory.join("config.toml");
        fs::write(&path, b"[workers]\nremux = 3\n").unwrap();
        let config = Config::load_from(&path);
        assert_eq!(
            config,
            Config {
                remux_workers: 3,
                ..Config::default()
            }
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn config_should_clamp_an_unreasonably_large_worker_count() {
        let directory = scratch("clamp");
        let path = directory.join("config.toml");
        fs::write(&path, b"[workers]\ntranscode = 999999\nremux = 0\n").unwrap();
        let config = Config::load_from(&path);
        assert_eq!(config.transcode_workers, MAX_WORKERS);
        // Zero is clamped up to 1: a pool with zero workers would never process
        // anything sent to it.
        assert_eq!(config.remux_workers, 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn config_dir_should_prefer_xdg_config_home_and_fall_back_to_home() {
        // Arrange / Act / Assert: deliberately `reel`, not `reel-tui` like the cache
        // directory — the user asked for this exact path, so a "consistency" fix that
        // renamed it would silently orphan their existing config file.
        assert_eq!(
            Config::config_dir_from(Some("/xdg"), Some("/home/bas")),
            Some(PathBuf::from("/xdg/reel")),
        );
        assert_eq!(
            Config::config_dir_from(None, Some("/home/bas")),
            Some(PathBuf::from("/home/bas/.config/reel")),
        );
        assert_eq!(Config::config_dir_from(None, None), None);
    }

    #[test]
    fn config_dir_should_treat_an_empty_variable_as_unset() {
        // Arrange / Act / Assert: `XDG_CONFIG_HOME=` from a shell profile must not
        // resolve the config to a relative `reel/` inside the launch directory.
        assert_eq!(
            Config::config_dir_from(Some(""), Some("/home/bas")),
            Some(PathBuf::from("/home/bas/.config/reel")),
        );
        assert_eq!(Config::config_dir_from(Some(""), Some("")), None);
    }

    #[test]
    fn the_config_file_should_sit_inside_the_config_directory() {
        // Arrange / Act / Assert: the pairing `load()` depends on.
        assert_eq!(
            Config::config_dir_from(Some("/xdg"), None).map(|dir| dir.join("config.toml")),
            Some(PathBuf::from("/xdg/reel/config.toml")),
        );
    }

    #[test]
    fn live_config_paths_should_resolve_from_the_process_environment() {
        let directory = Config::config_dir().expect("the test process must have a config home");

        assert_eq!(
            directory.file_name().and_then(|name| name.to_str()),
            Some("reel")
        );
        assert_eq!(
            Config::config_file_path(),
            Some(directory.join("config.toml"))
        );
    }

    #[test]
    fn config_should_default_when_the_file_is_valid_toml_but_holds_no_worker_settings() {
        // Arrange: a config file the user created for some other purpose, or one whose
        // `[workers]` table they commented out. Valid TOML with nothing relevant in it
        // must leave every worker count at its default rather than collapsing to zero.
        let directory = scratch("empty-toml");
        let path = directory.join("config.toml");
        fs::write(&path, b"# nothing configured yet\n").unwrap();

        // Act
        let config = Config::load_from(&path);

        // Assert
        assert_eq!(config, Config::default());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn config_should_keep_non_network_defaults_when_only_the_network_table_is_set() {
        // Arrange: the mirror of `config_should_keep_individual_defaults_for_unspecified_fields`
        // — setting only the nested network table must not reset the top-level pair, which
        // is the pairing most likely to break if the nested defaults were folded in wrongly.
        let directory = scratch("network-only");
        let path = directory.join("config.toml");
        fs::write(&path, b"[workers.network]\nremux = 2\n").unwrap();

        // Act
        let config = Config::load_from(&path);

        // Assert
        assert_eq!(
            config,
            Config {
                network_remux_workers: 2,
                ..Config::default()
            }
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn config_should_clamp_the_network_worker_counts_too() {
        // Arrange: the clamp is applied per field, so the network pair needs its own
        // proof — an unclamped 999999 there would spawn the thread flood the limit exists
        // to prevent, on the mount type least able to absorb it.
        let directory = scratch("clamp-network");
        let path = directory.join("config.toml");
        fs::write(&path, b"[workers.network]\ntranscode = 999999\nremux = 0\n").unwrap();

        // Act
        let config = Config::load_from(&path);

        // Assert
        assert_eq!(config.network_transcode_workers, MAX_WORKERS);
        assert_eq!(config.network_remux_workers, 1);
        fs::remove_dir_all(directory).unwrap();
    }

    /// The rate is the escape hatch for a terminal that cannot keep up with the picture —
    /// over ssh, or inside tmux — so a value that is missing, absurd, or nonsense has to
    /// leave a playback that still works rather than one that divides by zero or asks for
    /// a thousand frames a second.
    #[test]
    fn playback_settings_should_fall_back_and_stay_inside_their_limits() {
        let directory = scratch("playback");
        let load = |body: &[u8]| {
            let path = directory.join("config.toml");
            fs::write(&path, body).unwrap();
            Config::load_from(&path)
        };

        // Act / Assert: an absent section keeps the defaults.
        let defaults = load(b"[preview]\ncache_tracks = 4\n");
        assert_eq!(defaults.playback_fps, DEFAULT_PLAYBACK_FPS);
        assert_eq!(defaults.playback_pad, DEFAULT_PLAYBACK_PAD);

        // Act / Assert: one value set leaves the other alone, down to the leaf.
        let partial = load(b"[preview.playback]\nfps = 15\n");
        assert_eq!(partial.playback_fps, 15);
        assert_eq!(partial.playback_pad, DEFAULT_PLAYBACK_PAD);

        // Act / Assert: a rate nothing could draw, and one nothing could watch.
        assert_eq!(
            load(b"[preview.playback]\nfps = 9000\n").playback_fps,
            MAX_PLAYBACK_FPS
        );
        assert_eq!(
            load(b"[preview.playback]\nfps = 0\n").playback_fps,
            MIN_PLAYBACK_FPS
        );

        // Act / Assert: a pad of nothing is meaningful — play exactly the cue — so only
        // the top is capped.
        assert_eq!(
            load(b"[preview.playback]\npad = 0\n").playback_pad,
            Duration::ZERO
        );
        assert_eq!(
            load(b"[preview.playback]\npad = 600.0\n").playback_pad,
            MAX_PLAYBACK_PAD
        );

        // Act / Assert: and a negative or a non-finite pad, both of which `from_secs_f64`
        // panics on outright, fall back rather than take the process down at launch.
        assert_eq!(
            load(b"[preview.playback]\npad = -3.0\n").playback_pad,
            DEFAULT_PLAYBACK_PAD
        );
        assert_eq!(
            load(b"[preview.playback]\npad = nan\n").playback_pad,
            DEFAULT_PLAYBACK_PAD
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn effective_workers_should_use_the_network_limits_only_on_a_network_mount() {
        let config = Config {
            notifications: false,
            transcode_workers: 1,
            remux_workers: 5,
            network_transcode_workers: 1,
            network_remux_workers: 1,
            preview_cache_tracks: DEFAULT_PREVIEW_CACHE_TRACKS,
            preview_prefetch: true,
            network_preview_prefetch: false,
            playback_fps: DEFAULT_PLAYBACK_FPS,
            playback_pad: DEFAULT_PLAYBACK_PAD,
        };
        assert_eq!(config.effective_workers(false), (1, 5));
        assert_eq!(config.effective_workers(true), (1, 1));
    }

    #[test]
    fn notifications_should_be_enabled_by_default_and_individually_configurable() {
        let directory = scratch("notifications");
        let path = directory.join("config.toml");

        assert!(Config::default().notifications);
        fs::write(&path, b"[notifications]\nenabled = false\n").unwrap();
        let config = Config::load_from(&path);

        assert!(!config.notifications);
        assert_eq!(
            config,
            Config {
                notifications: false,
                ..Config::default()
            }
        );
        fs::remove_dir_all(directory).unwrap();
    }

    /// Rendering a frame per cue means a thousand accurate seeks, which is fine on a
    /// local disk and is not something to start unasked across NFS or SMB.
    #[test]
    fn prefetching_should_follow_the_mount_the_directory_is_on() {
        let defaults = Config::default();
        assert!(defaults.effective_prefetch(false));
        assert!(!defaults.effective_prefetch(true));

        // Both are settable, so a fast share can opt in and a slow laptop can opt out.
        let configured = Config {
            preview_prefetch: false,
            network_preview_prefetch: true,
            ..defaults
        };
        assert!(!configured.effective_prefetch(false));
        assert!(configured.effective_prefetch(true));
    }

    /// The cache is counted in whole media files rather than megabytes, because a track
    /// is what gets used and half a track is nearly worthless — see `framecache`.
    #[test]
    fn the_frame_cache_limit_should_be_counted_in_tracks() {
        assert_eq!(
            Config::default().preview_cache_tracks,
            DEFAULT_PREVIEW_CACHE_TRACKS
        );
        // Zero is a real answer for a machine short on disk: nothing is kept beyond the
        // page that is open, which the pass is rendering into and so never evicts.
        let none = Config {
            preview_cache_tracks: 0,
            ..Config::default()
        };
        assert_eq!(none.preview_cache_tracks, 0);
    }

    /// A mistyped cache size must not let the frame cache eat the disk, the same way a
    /// mistyped worker count must not spawn a hundred threads.
    #[test]
    fn an_absurd_cache_size_should_be_capped() {
        let directory = scratch("cache-size");
        let path = directory.join("config.toml");
        fs::write(&path, b"[preview]\ncache_tracks = 99999999\n").unwrap();

        let config = Config::load_from(&path);

        assert_eq!(config.preview_cache_tracks, MAX_PREVIEW_CACHE_TRACKS);
        fs::remove_dir_all(directory).unwrap();
    }

    /// One preview key set in isolation must not reset the others, the same as every
    /// other leaf in this file.
    #[test]
    fn a_lone_preview_key_should_keep_the_other_preview_defaults() {
        let directory = scratch("preview-partial");
        let path = directory.join("config.toml");
        fs::write(&path, b"[preview.prefetch]\nnetwork = true\n").unwrap();

        let config = Config::load_from(&path);

        assert_eq!(
            config,
            Config {
                network_preview_prefetch: true,
                ..Config::default()
            }
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
