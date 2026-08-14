use std::{fs, path::PathBuf};

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            notifications: true,
            transcode_workers: 1,
            remux_workers: 5,
            network_transcode_workers: 1,
            network_remux_workers: 1,
        }
    }
}

/// A typo'd or malicious huge value in the config file must not spawn an unreasonable
/// number of threads.
const MAX_WORKERS: usize = 16;

#[derive(Deserialize, Default)]
struct RawConfig {
    notifications: Option<RawNotifications>,
    workers: Option<RawWorkers>,
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
            b"[notifications]\nenabled = false\n\n[workers]\ntranscode = 2\nremux = 8\n\n[workers.network]\ntranscode = 3\nremux = 4\n",
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

    #[test]
    fn effective_workers_should_use_the_network_limits_only_on_a_network_mount() {
        let config = Config {
            notifications: false,
            transcode_workers: 1,
            remux_workers: 5,
            network_transcode_workers: 1,
            network_remux_workers: 1,
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
}
