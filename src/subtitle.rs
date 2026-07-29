use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};

use crate::files::{FileEntry, FileFingerprint};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SubtitleFormat {
    SubRip,
    Ass,
    WebVtt,
    Ttml,
    MovText,
    Pgs,
    VobSub,
}

impl SubtitleFormat {
    pub const COMMON_TARGETS: [Self; 7] = [
        Self::SubRip,
        Self::Ass,
        Self::WebVtt,
        Self::Ttml,
        Self::MovText,
        Self::Pgs,
        Self::VobSub,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::SubRip => "SubRip / SRT",
            Self::Ass => "ASS",
            Self::WebVtt => "WebVTT",
            Self::Ttml => "TTML",
            Self::MovText => "MOV Text",
            Self::Pgs => "PGS / SUP",
            Self::VobSub => "VobSub",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::SubRip => "srt",
            Self::Ass => "ass",
            Self::WebVtt => "vtt",
            Self::Ttml => "ttml",
            Self::MovText => "mp4",
            Self::Pgs => "sup",
            Self::VobSub => "sub",
        }
    }

    pub fn ffmpeg_encoder(self) -> Option<&'static str> {
        match self {
            Self::SubRip => Some("subrip"),
            Self::Ass => Some("ass"),
            Self::WebVtt => Some("webvtt"),
            Self::Ttml => Some("ttml"),
            Self::MovText => Some("mov_text"),
            Self::Pgs | Self::VobSub => None,
        }
    }

    pub fn ffmpeg_codec(self) -> &'static str {
        match self {
            Self::SubRip => "subrip",
            Self::Ass => "ass",
            Self::WebVtt => "webvtt",
            Self::Ttml => "ttml",
            Self::MovText => "mov_text",
            Self::Pgs => "hdmv_pgs_subtitle",
            Self::VobSub => "dvd_subtitle",
        }
    }

    pub fn seconv_name(self) -> &'static str {
        match self {
            Self::SubRip => "subrip",
            Self::Ass => "assa",
            Self::WebVtt => "webvtt",
            Self::Ttml => "ttml",
            Self::MovText => "subrip",
            Self::Pgs => "bluraysup",
            Self::VobSub => "vobsub",
        }
    }

    pub fn is_text(self) -> bool {
        matches!(
            self,
            Self::SubRip | Self::Ass | Self::WebVtt | Self::Ttml | Self::MovText
        )
    }

    pub fn is_image(self) -> bool {
        matches!(self, Self::Pgs | Self::VobSub)
    }

    pub fn from_codec(codec: &str) -> Option<Self> {
        match codec {
            "subrip" | "srt" | "text" => Some(Self::SubRip),
            "ass" | "ssa" => Some(Self::Ass),
            "webvtt" => Some(Self::WebVtt),
            "ttml" => Some(Self::Ttml),
            "mov_text" => Some(Self::MovText),
            "hdmv_pgs_subtitle" => Some(Self::Pgs),
            "dvd_subtitle" => Some(Self::VobSub),
            _ => None,
        }
    }

    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "srt" => Some(Self::SubRip),
            "ass" | "ssa" => Some(Self::Ass),
            "vtt" => Some(Self::WebVtt),
            "ttml" => Some(Self::Ttml),
            "sup" => Some(Self::Pgs),
            "sub" => Some(Self::VobSub),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SubtitleSource {
    Embedded(u64),
    Sidecar(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtitleChange {
    pub source: SubtitleSource,
    pub source_format: SubtitleFormat,
    pub embedded_target: Option<SubtitleFormat>,
    pub export_target: Option<SubtitleFormat>,
    pub import_into_media: bool,
    pub ocr_language: Option<String>,
}

impl SubtitleChange {
    pub fn removes_from_media(&self) -> bool {
        matches!(self.source, SubtitleSource::Embedded(_))
            && self.embedded_target.is_none()
            && self.export_target.is_some()
    }

    pub fn changes_media(&self) -> bool {
        match self.source {
            SubtitleSource::Embedded(_) => {
                self.removes_from_media()
                    || self
                        .embedded_target
                        .is_some_and(|target| target != self.source_format)
            }
            SubtitleSource::Sidecar(_) => self.import_into_media,
        }
    }

    pub fn has_effect(&self) -> bool {
        match self.source {
            SubtitleSource::Embedded(_) => self.changes_media() || self.export_target.is_some(),
            SubtitleSource::Sidecar(_) => {
                self.import_into_media
                    || self
                        .embedded_target
                        .is_some_and(|target| target != self.source_format)
            }
        }
    }

    pub fn needs_ocr(&self) -> bool {
        self.source_format.is_image()
            && self
                .embedded_target
                .into_iter()
                .chain(self.export_target)
                .any(SubtitleFormat::is_text)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SidecarEntry {
    pub path: PathBuf,
    pub companion: Option<PathBuf>,
    pub display_name: String,
    pub format: SubtitleFormat,
    pub language: String,
    pub forced: bool,
    pub cc: bool,
    pub number: Option<usize>,
    pub fingerprint: FileFingerprint,
    pub companion_fingerprint: Option<FileFingerprint>,
}

impl SidecarEntry {
    pub fn source_paths(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.path).chain(self.companion.iter())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatChoice {
    pub format: SubtitleFormat,
    pub value: Option<SubtitleFormat>,
    pub label: String,
    pub enabled: bool,
    pub reason: Option<String>,
    pub current: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCapabilities {
    pub ffmpeg: bool,
    pub ffmpeg_encoders: BTreeSet<String>,
    pub ffmpeg_muxers: BTreeSet<String>,
    pub seconv: bool,
    pub tesseract_languages: Vec<String>,
}

impl ToolCapabilities {
    pub fn detect_cached() -> Self {
        static CAPABILITIES: OnceLock<ToolCapabilities> = OnceLock::new();
        CAPABILITIES.get_or_init(Self::detect).clone()
    }

    fn detect() -> Self {
        let encoders = command_stdout("ffmpeg", &["-hide_banner", "-encoders"]);
        let muxers = command_stdout("ffmpeg", &["-hide_banner", "-muxers"]);
        let ffmpeg = encoders.is_some() && muxers.is_some();
        let seconv = command_succeeds("seconv", &["--version"]);
        let tesseract_languages = command_stdout("tesseract", &["--list-langs"])
            .map(|output| {
                output
                    .lines()
                    .skip_while(|line| !line.starts_with("List of available languages"))
                    .skip(1)
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && *line != "osd")
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        Self {
            ffmpeg,
            ffmpeg_encoders: encoders
                .as_deref()
                .map(parse_capability_names)
                .unwrap_or_default(),
            ffmpeg_muxers: muxers
                .as_deref()
                .map(parse_capability_names)
                .unwrap_or_default(),
            seconv,
            tesseract_languages,
        }
    }

    pub fn format_choices(
        &self,
        source: SubtitleFormat,
        container_extension: Option<&str>,
        sidecar_output: bool,
        extracting_embedded: bool,
    ) -> Vec<FormatChoice> {
        SubtitleFormat::COMMON_TARGETS
            .into_iter()
            .map(|target| {
                let current = target == source;
                let reason = if current && !extracting_embedded {
                    None
                } else {
                    self.disabled_reason(
                        source,
                        target,
                        container_extension,
                        sidecar_output,
                        extracting_embedded,
                    )
                };
                FormatChoice {
                    format: target,
                    value: (!current).then_some(target),
                    label: target.label().to_string(),
                    enabled: reason.is_none(),
                    reason,
                    current,
                }
            })
            .collect()
    }

    fn disabled_reason(
        &self,
        source: SubtitleFormat,
        target: SubtitleFormat,
        container_extension: Option<&str>,
        sidecar_output: bool,
        extracting_embedded: bool,
    ) -> Option<String> {
        if target == SubtitleFormat::MovText && sidecar_output {
            return Some("MOV Text is only available inside MP4/MOV".to_string());
        }
        if source == SubtitleFormat::VobSub
            && target == SubtitleFormat::VobSub
            && extracting_embedded
            && !self.seconv
        {
            return Some("VobSub export requires seconv in PATH".to_string());
        }
        if source.is_image() && target.is_image() && source != target {
            return Some("cross-image conversion is not safely supported".to_string());
        }
        if source.is_image() && target.is_text() {
            if !self.seconv {
                return Some("requires seconv in PATH".to_string());
            }
            if self.tesseract_languages.is_empty() {
                return Some("requires Tesseract language data in PATH".to_string());
            }
        } else if source.is_text() && target.is_image() {
            if !self.seconv {
                return Some("requires seconv in PATH".to_string());
            }
        } else if source.is_text() && target.is_text() {
            if !self.ffmpeg {
                return Some("requires ffmpeg in PATH".to_string());
            }
            if target
                .ffmpeg_encoder()
                .is_some_and(|encoder| !self.ffmpeg_encoders.contains(encoder))
            {
                return Some("FFmpeg encoder is unavailable".to_string());
            }
        }
        if !sidecar_output && !container_supports(container_extension.unwrap_or_default(), target) {
            return Some(format!(
                "{} cannot embed {}",
                container_extension
                    .filter(|extension| !extension.is_empty())
                    .unwrap_or("this container")
                    .to_ascii_uppercase(),
                target.label()
            ));
        }
        None
    }
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn command_succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn parse_capability_names(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let flags = fields.next()?;
            let name = fields.next()?;
            (flags.contains('E') || flags.starts_with('S')).then(|| name.to_string())
        })
        .collect()
}

fn container_supports(extension: &str, format: SubtitleFormat) -> bool {
    match extension.to_ascii_lowercase().as_str() {
        "mkv" | "mks" => matches!(
            format,
            SubtitleFormat::SubRip
                | SubtitleFormat::Ass
                | SubtitleFormat::WebVtt
                | SubtitleFormat::Pgs
                | SubtitleFormat::VobSub
        ),
        "webm" => format == SubtitleFormat::WebVtt,
        "mp4" | "mov" | "m4v" | "3gp" => format == SubtitleFormat::MovText,
        _ => false,
    }
}

pub fn partition_sidecars(
    files: Vec<FileEntry>,
) -> (Vec<FileEntry>, HashMap<PathBuf, Vec<SidecarEntry>>) {
    let media_by_stem = media_paths_by_stem(&files);
    let entries_by_name = files
        .iter()
        .map(|entry| (entry.display_name.to_ascii_lowercase(), entry))
        .collect::<HashMap<_, _>>();
    let mut matched_paths = BTreeSet::new();
    let mut sidecars: HashMap<PathBuf, Vec<SidecarEntry>> = HashMap::new();

    for file in &files {
        let Some(format) = file
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(SubtitleFormat::from_extension)
        else {
            continue;
        };
        let Some((media_path, parsed)) =
            parse_sidecar_for_media(&file.display_name, format, &media_by_stem)
        else {
            continue;
        };
        let companion = (format == SubtitleFormat::VobSub).then(|| {
            let mut path = file.path.clone();
            path.set_extension("idx");
            path
        });
        let companion_entry = companion.as_ref().and_then(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| entries_by_name.get(&name.to_ascii_lowercase()))
                .copied()
        });
        if format == SubtitleFormat::VobSub && companion_entry.is_none() {
            continue;
        }
        matched_paths.insert(file.path.clone());
        if let Some(entry) = companion_entry {
            matched_paths.insert(entry.path.clone());
        }
        sidecars
            .entry(media_path.clone())
            .or_default()
            .push(SidecarEntry {
                path: file.path.clone(),
                companion: companion_entry.map(|entry| entry.path.clone()),
                display_name: file.display_name.clone(),
                format,
                language: parsed.language,
                forced: parsed.forced,
                cc: parsed.cc,
                number: parsed.number,
                fingerprint: file.fingerprint,
                companion_fingerprint: companion_entry.map(|entry| entry.fingerprint),
            });
    }

    for entries in sidecars.values_mut() {
        entries.sort_by(|left, right| {
            left.display_name
                .to_ascii_lowercase()
                .cmp(&right.display_name.to_ascii_lowercase())
        });
    }
    let visible = files
        .into_iter()
        .filter(|entry| !matched_paths.contains(&entry.path))
        .collect();
    (visible, sidecars)
}

#[derive(Debug)]
struct ParsedSidecar {
    language: String,
    forced: bool,
    cc: bool,
    number: Option<usize>,
}

fn media_paths_by_stem(files: &[FileEntry]) -> HashMap<String, Vec<PathBuf>> {
    let mut result: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for file in files {
        let extension = file
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(
            extension.as_str(),
            "mkv" | "mks" | "mp4" | "mov" | "m4v" | "webm" | "avi" | "ts" | "m2ts" | "mts"
        ) {
            continue;
        }
        if let Some(stem) = file.path.file_stem().and_then(|stem| stem.to_str()) {
            result
                .entry(stem.to_ascii_lowercase())
                .or_default()
                .push(file.path.clone());
        }
    }
    result
}

fn parse_sidecar_for_media<'a>(
    filename: &str,
    _format: SubtitleFormat,
    media_by_stem: &'a HashMap<String, Vec<PathBuf>>,
) -> Option<(&'a PathBuf, ParsedSidecar)> {
    let without_extension = filename.rsplit_once('.')?.0;
    let (media_stem, tail) = media_by_stem.iter().find_map(|(stem, paths)| {
        let prefix = format!("{stem}.");
        without_extension
            .to_ascii_lowercase()
            .strip_prefix(&prefix)
            .filter(|_| paths.len() == 1)
            .map(|tail| ((stem, &paths[0]), tail.to_string()))
    })?;
    let mut tokens = tail.split('.').collect::<Vec<_>>();
    if tokens.is_empty() {
        return None;
    }
    let language = tokens.remove(0).to_ascii_lowercase();
    if language.len() < 2
        || language.len() > 8
        || !language
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return None;
    }
    let mut forced = false;
    let mut cc = false;
    let mut number = None;
    for token in tokens {
        match token.to_ascii_lowercase().as_str() {
            "forced" if !forced => forced = true,
            "cc" if !cc => cc = true,
            value if number.is_none() => number = value.parse().ok(),
            _ => return None,
        }
    }
    Some((
        media_stem.1,
        ParsedSidecar {
            language,
            forced,
            cc,
            number,
        },
    ))
}

pub fn sidecar_filename(
    media_stem: &str,
    language: &str,
    forced: bool,
    cc: bool,
    number: Option<usize>,
    format: SubtitleFormat,
) -> String {
    let mut parts = vec![
        media_stem.to_string(),
        normalized_language(language).to_string(),
    ];
    if forced {
        parts.push("forced".to_string());
    }
    if cc {
        parts.push("cc".to_string());
    }
    if let Some(number) = number {
        parts.push(number.to_string());
    }
    parts.push(format.extension().to_string());
    parts.join(".")
}

pub fn normalized_language(language: &str) -> &str {
    let language = language.trim();
    if language.is_empty() || language.eq_ignore_ascii_case("und") {
        "und"
    } else {
        language
    }
}

pub fn path_extension(path: &Path) -> Option<&str> {
    path.extension().and_then(|extension| extension.to_str())
}

pub fn stream_language(stream: &BTreeMap<String, serde_json::Value>) -> String {
    stream
        .get("tags")
        .and_then(serde_json::Value::as_object)
        .and_then(|tags| tags.get("language"))
        .and_then(serde_json::Value::as_str)
        .map(|language| normalized_language(language).to_ascii_lowercase())
        .unwrap_or_else(|| "und".to_string())
}

pub fn stream_forced(stream: &BTreeMap<String, serde_json::Value>) -> bool {
    disposition(stream, "forced")
}

pub fn stream_cc(stream: &BTreeMap<String, serde_json::Value>) -> bool {
    disposition(stream, "hearing_impaired")
        || matches!(
            stream.get("codec_name").and_then(serde_json::Value::as_str),
            Some("eia_608" | "eia_708")
        )
}

fn disposition(stream: &BTreeMap<String, serde_json::Value>, name: &str) -> bool {
    stream
        .get("disposition")
        .and_then(serde_json::Value::as_object)
        .and_then(|disposition| disposition.get(name))
        .and_then(serde_json::Value::as_i64)
        == Some(1)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use kernal::prelude::*;

    use super::*;

    fn file(directory: &Path, name: &str) -> FileEntry {
        let path = directory.join(name);
        fs::write(&path, b"fixture").unwrap();
        FileEntry {
            fingerprint: FileFingerprint::for_path(&path).unwrap(),
            path,
            display_name: name.to_string(),
        }
    }

    #[test]
    fn partition_sidecars_should_attach_matching_files_when_name_contains_language_and_flags() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-subtitles-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let media = file(&directory, "movie.mkv");
        let subtitle = file(&directory, "movie.eng.forced.cc.2.srt");
        let unrelated = file(&directory, "other.eng.srt");

        // Act
        let (visible, sidecars) =
            partition_sidecars(vec![media.clone(), subtitle, unrelated.clone()]);

        // Assert
        assert_that!(
            visible
                .iter()
                .map(|entry| entry.display_name.as_str())
                .collect::<Vec<_>>()
        )
        .contains_exactly_in_given_order(["movie.mkv", "other.eng.srt"]);
        let entry = &sidecars[&media.path][0];
        assert_that!(entry.language.as_str()).is_equal_to("eng");
        assert_that!(entry.forced).is_true();
        assert_that!(entry.cc).is_true();
        assert_that!(entry.number).contains(2);

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn partition_sidecars_should_require_idx_companion_when_file_is_vobsub() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-vobsub-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let media = file(&directory, "movie.mkv");
        let subtitle = file(&directory, "movie.eng.sub");

        // Act
        let (visible, sidecars) = partition_sidecars(vec![media, subtitle]);

        // Assert
        assert_that!(sidecars).is_empty();
        assert_that!(visible).has_length(2);

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sidecar_filename_should_place_flags_and_duplicate_number_before_extension() {
        // Act
        let result = sidecar_filename("movie", "eng", true, true, Some(2), SubtitleFormat::SubRip);

        // Assert
        assert_that!(result).is_equal_to("movie.eng.forced.cc.2.srt".to_string());
    }

    #[test]
    fn format_choices_should_explain_missing_seconv_when_text_is_rendered_as_pgs() {
        // Arrange
        let capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from(["subrip".to_string()]),
            ..ToolCapabilities::default()
        };

        // Act
        let choices =
            capabilities.format_choices(SubtitleFormat::SubRip, Some("mkv"), false, false);
        let pgs = choices
            .iter()
            .find(|choice| choice.value == Some(SubtitleFormat::Pgs))
            .unwrap();

        // Assert
        assert_that!(pgs.enabled).is_false();
        assert_that!(pgs.reason.as_deref().unwrap()).contains("seconv");
    }

    #[test]
    fn format_choices_should_mark_actual_source_codec_without_synthetic_original_entry() {
        // Arrange
        let capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from([
                "subrip".to_string(),
                "ass".to_string(),
                "webvtt".to_string(),
                "ttml".to_string(),
                "mov_text".to_string(),
            ]),
            ffmpeg_muxers: BTreeSet::new(),
            seconv: false,
            tesseract_languages: Vec::new(),
        };

        // Act
        let choices =
            capabilities.format_choices(SubtitleFormat::SubRip, Some("mkv"), false, false);
        let current = choices.iter().find(|choice| choice.current).unwrap();

        // Assert
        assert_that!(choices.len()).is_equal_to(SubtitleFormat::COMMON_TARGETS.len());
        assert_that!(
            choices
                .iter()
                .any(|choice| choice.label.contains("Original"))
        )
        .is_false();
        assert_that!(current.label.as_str()).is_equal_to(SubtitleFormat::SubRip.label());
        assert_that!(current.value).is_none();
        assert_that!(current.enabled).is_true();
        assert_that!(current.reason.as_ref()).is_none();
    }

    #[test]
    fn export_choices_should_disable_current_mov_text_codec() {
        // Arrange
        let capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from(["subrip".to_string(), "mov_text".to_string()]),
            ..ToolCapabilities::default()
        };

        // Act
        let choices = capabilities.format_choices(SubtitleFormat::MovText, None, true, true);
        let current = choices.iter().find(|choice| choice.current).unwrap();

        // Assert
        assert_that!(current.format).is_equal_to(SubtitleFormat::MovText);
        assert_that!(current.enabled).is_false();
        assert_that!(current.reason.as_deref().unwrap()).contains("only available inside");
    }

    #[test]
    fn sidecar_choices_should_keep_the_current_codec_available_as_a_no_op() {
        // Arrange
        let capabilities = ToolCapabilities::default();

        // Act
        let choices = capabilities.format_choices(SubtitleFormat::VobSub, None, true, false);
        let current = choices.iter().find(|choice| choice.current).unwrap();

        // Assert
        assert_that!(current.enabled).is_true();
        assert_that!(current.reason.as_ref()).is_none();
    }

    #[test]
    fn image_to_text_choice_should_require_seconv_and_tesseract_language_data() {
        // Arrange
        let mut capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from(["subrip".to_string()]),
            ffmpeg_muxers: BTreeSet::new(),
            seconv: false,
            tesseract_languages: vec!["eng".to_string()],
        };

        // Act
        let without_seconv =
            capabilities.format_choices(SubtitleFormat::Pgs, Some("mkv"), false, false);
        capabilities.seconv = true;
        capabilities.tesseract_languages.clear();
        let without_language =
            capabilities.format_choices(SubtitleFormat::Pgs, Some("mkv"), false, false);

        // Assert
        let srt_without_seconv = without_seconv
            .iter()
            .find(|choice| choice.value == Some(SubtitleFormat::SubRip))
            .unwrap();
        let srt_without_language = without_language
            .iter()
            .find(|choice| choice.value == Some(SubtitleFormat::SubRip))
            .unwrap();
        assert_that!(srt_without_seconv.enabled).is_false();
        assert_that!(srt_without_seconv.reason.as_deref().unwrap()).contains("seconv");
        assert_that!(srt_without_language.enabled).is_false();
        assert_that!(srt_without_language.reason.as_deref().unwrap()).contains("Tesseract");
    }
}
