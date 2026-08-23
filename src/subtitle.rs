use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};

use isolang::{Language, languages};

use crate::files::{FileEntry, FileFingerprint};
use crate::requirements::{MINIMUM_SECONV, MINIMUM_TESSERACT, parse_tesseract_version};

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

    pub fn overview_label(self) -> &'static str {
        match self {
            Self::SubRip => "SRT",
            Self::Ass => "ASS",
            Self::WebVtt => "VTT",
            Self::Ttml => "TTML",
            Self::MovText => "MOVTXT",
            Self::Pgs => "PGS",
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
pub struct SubtitleMetadata {
    pub language: String,
    pub title: Option<String>,
    pub forced: bool,
    pub cc: bool,
    pub hearing_impaired: bool,
    pub original: bool,
    pub commentary: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubtitleFlag {
    Forced,
    Cc,
    HearingImpaired,
    Original,
    Commentary,
}

impl SubtitleFlag {
    pub const ALL: [Self; 5] = [
        Self::Forced,
        Self::Cc,
        Self::HearingImpaired,
        Self::Original,
        Self::Commentary,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Forced => "Forced",
            Self::Cc => "CC",
            Self::HearingImpaired => "Hearing impaired",
            Self::Original => "Original",
            Self::Commentary => "Commentary",
        }
    }
}

impl SubtitleMetadata {
    pub fn get_flag(&self, flag: SubtitleFlag) -> bool {
        match flag {
            SubtitleFlag::Forced => self.forced,
            SubtitleFlag::Cc => self.cc,
            SubtitleFlag::HearingImpaired => self.hearing_impaired,
            SubtitleFlag::Original => self.original,
            SubtitleFlag::Commentary => self.commentary,
        }
    }

    pub fn set_flag(&mut self, flag: SubtitleFlag, value: bool) {
        match flag {
            SubtitleFlag::Forced => self.forced = value,
            SubtitleFlag::Cc => self.cc = value,
            SubtitleFlag::HearingImpaired => self.hearing_impaired = value,
            SubtitleFlag::Original => self.original = value,
            SubtitleFlag::Commentary => self.commentary = value,
        }
    }
}

/// One cue's text as the timing page's editor rewrote it.
///
/// **`original` is what makes applying this safe.** The edit is addressed by the cue's
/// *position* in the parsed list, since a cue has no identity of its own — no id in the
/// file, and its text is exactly what is being changed. A position is only meaningful
/// against the list it was taken from, so the writer re-parses the file it is about to
/// rewrite and refuses when the cue standing at that position no longer says what the reader
/// was looking at. Without the check, a sidecar edited in another program between staging
/// and saving would have this text land on whichever line happened to move into that slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CueTextEdit {
    /// What the cue said when the editor opened, verbatim.
    pub original: String,
    /// What it should say instead.
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtitleChange {
    pub source: SubtitleSource,
    pub source_format: SubtitleFormat,
    pub embedded_target: Option<SubtitleFormat>,
    pub export_target: Option<SubtitleFormat>,
    pub import_into_media: bool,
    pub ocr_language: Option<String>,
    pub metadata: Option<SubtitleMetadata>,
    /// Cue text rewritten on the timing page, keyed by the cue's position in the parsed
    /// list. Empty for every change staged from the track list, which is every change that
    /// existed before the editor did.
    pub cue_text: BTreeMap<usize, CueTextEdit>,
}

impl SubtitleChange {
    pub fn removes_from_media(&self) -> bool {
        matches!(self.source, SubtitleSource::Embedded(_)) && self.export_target.is_some()
    }

    pub fn changes_media(&self) -> bool {
        match self.source {
            // Rewritten cue text reaches an embedded track only through the container, so
            // it is a remux like any other change to what the file holds.
            SubtitleSource::Embedded(_) => {
                self.removes_from_media()
                    || self.metadata.is_some()
                    || !self.cue_text.is_empty()
                    || self
                        .embedded_target
                        .is_some_and(|target| target != self.source_format)
            }
            // A sidecar's cues are rewritten in the sidecar itself, so they change the media
            // only when the track is also being imported into it.
            SubtitleSource::Sidecar(_) => self.import_into_media,
        }
    }

    pub fn has_effect(&self) -> bool {
        match self.source {
            SubtitleSource::Embedded(_) => self.changes_media() || self.export_target.is_some(),
            SubtitleSource::Sidecar(_) => {
                self.import_into_media
                    || self.metadata.is_some()
                    || !self.cue_text.is_empty()
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

/// Applies the timing page's cue edits to a SubRip file's text.
///
/// The file is re-parsed here rather than the page's cue list being written out, and the two
/// are not the same thing: the list was parsed when the page opened, and what is being
/// rewritten is whatever is on disk when the save runs. Every edit's `original` is checked
/// against the cue standing at its position, so an edit lands on the line the reader was
/// looking at or the save fails saying so — a sidecar rewritten by another program between
/// staging and saving must not have this text dropped onto whichever line moved into the
/// slot.
///
/// The whole file is rewritten rather than patched in place, because SubRip's counters are
/// positional: a cue whose text gained or lost a line leaves every byte offset after it
/// wrong, and `cue::write_srt` renumbers from one for free.
pub fn rewrite_srt_cues(
    source: &str,
    edits: &BTreeMap<usize, CueTextEdit>,
) -> Result<String, String> {
    let mut cues = crate::cue::parse_srt(source);
    for (position, edit) in edits {
        let Some(cue) = cues.get_mut(*position) else {
            return Err(format!(
                "This track no longer has a cue #{}; it may have been edited elsewhere.",
                position + 1
            ));
        };
        if cue.text != edit.original {
            return Err(format!(
                "Cue #{} no longer reads the way it did when it was edited; \
                 it may have been changed elsewhere.",
                position + 1
            ));
        }
        cue.text = edit.text.clone();
    }
    Ok(crate::cue::write_srt(&cues))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LanguageChoice {
    pub code: String,
    pub two_letter: String,
    pub name: String,
}

impl LanguageChoice {
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.code)
    }

    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_ascii_lowercase();
        query.is_empty()
            || self.name.to_ascii_lowercase().contains(&query)
            || self.code.contains(&query)
            || self.two_letter.contains(&query)
    }
}

pub fn common_language_choices() -> Vec<LanguageChoice> {
    let mut choices = languages()
        .filter_map(|language| {
            let two_letter = language.to_639_1()?;
            Some(LanguageChoice {
                code: language.to_639_3().to_string(),
                two_letter: two_letter.to_string(),
                name: language.to_name().to_string(),
            })
        })
        .collect::<Vec<_>>();
    choices.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.code.cmp(&right.code))
    });
    choices.dedup_by(|left, right| left.code == right.code);
    choices
}

pub fn canonical_language_code(code: &str) -> Option<String> {
    let normalized = code.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == "und" {
        return None;
    }
    let (base, suffix) = normalized
        .split_once(['-', '_'])
        .map_or((normalized.as_str(), None), |(base, suffix)| {
            (base, (!suffix.is_empty()).then_some(suffix))
        });
    let canonical_base = match base {
        "alb" => "sqi",
        "arm" => "hye",
        "baq" => "eus",
        "bur" => "mya",
        "chi" => "zho",
        "cze" => "ces",
        "dut" => "nld",
        "fre" => "fra",
        "geo" => "kat",
        "ger" => "deu",
        "gre" => "ell",
        "ice" => "isl",
        "mac" => "mkd",
        "mao" => "mri",
        "may" => "msa",
        "per" => "fas",
        "rum" => "ron",
        "slo" => "slk",
        "tib" => "bod",
        "wel" => "cym",
        _ => base,
    };
    let language = Language::from_639_1(canonical_base)
        .or_else(|| Language::from_639_3(canonical_base))
        .or_else(|| canonical_base.parse().ok())?;
    let canonical = language.to_639_3();
    Some(suffix.map_or_else(
        || canonical.to_string(),
        |suffix| format!("{canonical}-{suffix}"),
    ))
}

pub fn language_choice(code: &str) -> Option<LanguageChoice> {
    let canonical = canonical_language_code(code)?;
    let base = canonical
        .split_once(['-', '_'])
        .map_or(canonical.as_str(), |(base, _)| base);
    let language = Language::from_639_1(base)
        .or_else(|| Language::from_639_3(base))
        .or_else(|| base.parse().ok())?;
    Some(LanguageChoice {
        code: canonical,
        two_letter: language.to_639_1().unwrap_or_default().to_string(),
        name: language.to_name().to_string(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SidecarEntry {
    pub path: PathBuf,
    pub companion: Option<PathBuf>,
    pub display_name: String,
    pub format: SubtitleFormat,
    pub language: String,
    pub forced: bool,
    pub hearing_impaired: bool,
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
    /// Filter names from `ffmpeg -filters`, consulted only by the subtitle timing page's
    /// frame preview. A build without libass has no `subtitles` filter, and asking one to
    /// burn a cue in fails per keypress rather than once.
    pub ffmpeg_filters: BTreeSet<String>,
    /// Decoder names from `ffmpeg -decoders`, consulted by the subtitle timing page.
    ///
    /// Separate from `ffmpeg_encoders` because the page reads where the rest of the
    /// program writes: previewing a WebVTT or MOV Text track transcodes it *to* SubRip on
    /// the way out, so what matters is whether this build can read the source format, and
    /// a build can perfectly well encode a format it cannot decode.
    pub ffmpeg_decoders: BTreeSet<String>,
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
        let filters = command_stdout("ffmpeg", &["-hide_banner", "-filters"]);
        let decoders = command_stdout("ffmpeg", &["-hide_banner", "-decoders"]);
        let ffmpeg = encoders.is_some() && muxers.is_some();
        let seconv = command_stdout("seconv", &["--help"])
            .as_deref()
            .is_some_and(seconv_is_supported);
        let tesseract_languages = detect_tesseract_languages(
            || command_stdout("tesseract", &["--version"]),
            || command_stdout("tesseract", &["--list-langs"]),
        );

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
            ffmpeg_filters: filters
                .as_deref()
                .map(parse_filter_names)
                .unwrap_or_default(),
            ffmpeg_decoders: decoders
                .as_deref()
                .map(parse_capability_names)
                .unwrap_or_default(),
            seconv,
            tesseract_languages,
        }
    }

    /// Whether `ffmpeg` can draw a subtitle onto a video frame.
    ///
    /// `subtitles` is the libass one, present only in a build configured with it;
    /// `scale` is what fits the result to the preview pane. Without both, the timing page
    /// asks for no frames at all and leaves its preview pane empty, which is also what a
    /// terminal with no image protocol gets.
    pub fn can_burn_subtitles(&self) -> bool {
        self.ffmpeg_filters.contains("subtitles") && self.ffmpeg_filters.contains("scale")
    }

    /// Why the timing page cannot be opened on a track of this format, if it cannot.
    ///
    /// Checked before the page opens rather than reported by a worker afterwards, so a
    /// missing tool reads as a refusal with a reason instead of a page that loads, sits on
    /// its loader, and fails. The same reason `format_choices` gates conversion up front.
    ///
    /// Each format reaches the cue list by a different road, and it is the road that
    /// decides what has to be installed:
    ///
    /// - **SubRip and ASS** are parsed by [`crate::cue`] straight off a `-c:s copy`
    ///   extraction, so no codec is involved and nothing can be missing.
    /// - **WebVTT and MOV Text** are transcoded to SubRip on the way out, which needs a
    ///   build that can *decode* them — a different question from the encoder list the
    ///   rest of the program asks about.
    /// - **PGS and VobSub** carry pictures rather than text, so there is nothing for any
    ///   of this to read.
    pub fn preview_blocked(&self, format: SubtitleFormat) -> Option<String> {
        match format {
            SubtitleFormat::SubRip | SubtitleFormat::Ass => None,
            SubtitleFormat::WebVtt | SubtitleFormat::MovText => {
                (!self.ffmpeg_decoders.contains(format.ffmpeg_codec())).then(|| {
                    format!(
                        "{} previewing needs an FFmpeg build that can decode it.",
                        format.overview_label()
                    )
                })
            }
            SubtitleFormat::Ttml | SubtitleFormat::Pgs | SubtitleFormat::VobSub => Some(format!(
                "{} subtitle previewing is not implemented yet.",
                format.overview_label()
            )),
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
            return Some(format!(
                "VobSub export requires seconv {MINIMUM_SECONV}+ in PATH"
            ));
        }
        if source.is_image() && target.is_image() && source != target {
            return Some("cross-image conversion is not safely supported".to_string());
        }
        if source.is_image() && target.is_text() {
            if !self.seconv {
                return Some(format!("requires seconv {MINIMUM_SECONV}+ in PATH"));
            }
            if self.tesseract_languages.is_empty() {
                return Some(format!(
                    "requires Tesseract {MINIMUM_TESSERACT}+ language data in PATH"
                ));
            }
        } else if source.is_text() && target.is_image() {
            if !self.seconv {
                return Some(format!("requires seconv {MINIMUM_SECONV}+ in PATH"));
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

fn parse_capability_names(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let flags = fields.next()?;
            let name = fields.next()?;
            // `ffmpeg -encoders` prints its flag legend in the same shape as its entries
            // (" V..... = Video"), so without this the legend contributes a capability
            // literally named "=".
            if name == "=" {
                return None;
            }
            (flags.contains('E')
                || flags.starts_with('V')
                || flags.starts_with('A')
                || flags.starts_with('S'))
            .then(|| name.to_string())
        })
        .collect()
}

/// Filter names from `ffmpeg -filters`.
///
/// A separate parser from `parse_capability_names` because the listings differ where it
/// matters: a filter's flag column is `TSC`-style with no `E`, and its third field is the
/// `V->V` signature. Keying on that signature is what separates the entries from the
/// legend above them, which is printed in the same shape (`T.. = Timeline support`).
fn parse_filter_names(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _flags = fields.next()?;
            let name = fields.next()?;
            fields.next()?.contains("->").then(|| name.to_string())
        })
        .collect()
}

/// The OCR languages `reel` will offer, or none when Tesseract is missing or too old.
///
/// The version is consulted before the language list so an ancient Tesseract reports as
/// having no languages rather than as having languages that OCR badly: `reel` shows the
/// resulting choice as unavailable, which is recoverable, instead of running the
/// conversion and writing a subtitle full of garbage, which is not.
fn detect_tesseract_languages(
    version: impl FnOnce() -> Option<String>,
    languages: impl FnOnce() -> Option<String>,
) -> Vec<String> {
    if !version().as_deref().is_some_and(tesseract_is_supported) {
        return Vec::new();
    }
    languages()
        .as_deref()
        .map(parse_tesseract_languages)
        .unwrap_or_default()
}

fn tesseract_is_supported(banner: &str) -> bool {
    parse_tesseract_version(banner).is_some_and(|version| version >= MINIMUM_TESSERACT)
}

/// Whether a `seconv --help` listing belongs to a build new enough to use.
///
/// `--version` cannot answer this: the 5.1.0 release reports `5.0.0`, so gating on it
/// rejects the very build `reel` needs. The help listing is the only thing that
/// distinguishes them, and `--no-vobsub-isolate-colors` is the flag to look for —
/// 5.1.0 added it as part of the VobSub OCR rework that also taught `seconv` to read a
/// `.idx` at all, which is what 5.0.0 refuses with "Unable to determine subtitle
/// format". A newer release that drops the flag would read as unsupported, which fails
/// closed rather than silently producing empty subtitles.
fn seconv_is_supported(help: &str) -> bool {
    help.contains("--no-vobsub-isolate-colors")
}

fn parse_tesseract_languages(output: &str) -> Vec<String> {
    output
        .lines()
        .skip_while(|line| !line.starts_with("List of available languages"))
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "osd")
        .map(str::to_string)
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
        let Some((media_paths, parsed)) =
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
        for media_path in media_paths {
            sidecars
                .entry(media_path.clone())
                .or_default()
                .push(SidecarEntry {
                    path: file.path.clone(),
                    companion: companion_entry.map(|entry| entry.path.clone()),
                    display_name: file.display_name.clone(),
                    format,
                    language: parsed.language.clone(),
                    forced: parsed.forced,
                    hearing_impaired: parsed.hearing_impaired,
                    number: parsed.number,
                    fingerprint: file.fingerprint,
                    companion_fingerprint: companion_entry.map(|entry| entry.fingerprint),
                });
        }
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
    hearing_impaired: bool,
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
) -> Option<(&'a [PathBuf], ParsedSidecar)> {
    let without_extension = filename.rsplit_once('.')?.0;
    // Lowercased once outside the loop: it doesn't depend on `stem`, so recomputing it
    // per candidate media stem was an O(media_by_stem.len()) redundant allocation for
    // every sidecar file matched.
    let lower_without_extension = without_extension.to_ascii_lowercase();
    let (_media_stem, media_paths, tail) = media_by_stem.iter().find_map(|(stem, paths)| {
        let prefix = format!("{stem}.");
        lower_without_extension
            .strip_prefix(&prefix)
            .map(|tail| (stem, paths.as_slice(), tail.to_string()))
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
    let mut hearing_impaired = false;
    let mut number = None;
    for token in tokens {
        match token.to_ascii_lowercase().as_str() {
            "forced" if !forced => forced = true,
            "cc" | "sdh" | "hi" => hearing_impaired = true,
            value if number.is_none() => number = value.parse().ok(),
            _ => return None,
        }
    }
    Some((
        media_paths,
        ParsedSidecar {
            language: canonical_language_code(&language).unwrap_or(language),
            forced,
            hearing_impaired,
            number,
        },
    ))
}

pub fn sidecar_filename(
    media_stem: &str,
    language: &str,
    forced: bool,
    hearing_impaired: bool,
    number: Option<usize>,
    format: SubtitleFormat,
) -> String {
    let mut parts = vec![
        media_stem.to_string(),
        canonical_language_code(language).unwrap_or_else(|| "und".to_string()),
    ];
    if forced {
        parts.push("forced".to_string());
    }
    if hearing_impaired {
        parts.push("sdh".to_string());
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
        .and_then(canonical_language_code)
        .unwrap_or_else(|| "und".to_string())
}

/// A track's human-readable name.
///
/// Matroska stores it as `title`, but ISO-BMFF stores it in the track's `name` atom and
/// ffprobe reports it under that key — so an MP4/MOV title reads as absent unless both
/// are consulted, which made a title survive a remux into MP4 yet fail verification.
pub fn stream_title(stream: &BTreeMap<String, serde_json::Value>) -> Option<String> {
    let tags = stream.get("tags").and_then(serde_json::Value::as_object)?;
    ["title", "name"]
        .into_iter()
        .filter_map(|key| tags.get(key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .find(|title| !title.is_empty())
        .map(str::to_string)
}

pub fn stream_forced(stream: &BTreeMap<String, serde_json::Value>) -> bool {
    disposition(stream, "forced")
}

pub fn stream_cc(stream: &BTreeMap<String, serde_json::Value>) -> bool {
    disposition(stream, "captions")
        || matches!(
            stream.get("codec_name").and_then(serde_json::Value::as_str),
            Some("eia_608" | "eia_708")
        )
}

pub fn stream_hearing_impaired(stream: &BTreeMap<String, serde_json::Value>) -> bool {
    disposition(stream, "hearing_impaired")
}

pub fn stream_original(stream: &BTreeMap<String, serde_json::Value>) -> bool {
    disposition(stream, "original")
}

pub fn stream_commentary(stream: &BTreeMap<String, serde_json::Value>) -> bool {
    disposition(stream, "comment")
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
    fn embedded_subtitle_state_matrix_should_remove_every_exported_combination() {
        let targets = std::iter::once(None)
            .chain(SubtitleFormat::COMMON_TARGETS.map(Some))
            .collect::<Vec<_>>();

        for embedded_target in &targets {
            for export_target in &targets {
                let change = SubtitleChange {
                    cue_text: Default::default(),
                    source: SubtitleSource::Embedded(7),
                    source_format: SubtitleFormat::SubRip,
                    embedded_target: *embedded_target,
                    export_target: *export_target,
                    import_into_media: false,
                    ocr_language: None,
                    metadata: None,
                };
                let converted_in_media =
                    embedded_target.is_some_and(|target| target != SubtitleFormat::SubRip);
                let exported = export_target.is_some();

                assert_eq!(
                    change.removes_from_media(),
                    exported,
                    "embedded={embedded_target:?}, export={export_target:?}"
                );
                assert_eq!(
                    change.changes_media(),
                    exported || converted_in_media,
                    "embedded={embedded_target:?}, export={export_target:?}"
                );
                assert_eq!(
                    change.has_effect(),
                    exported || converted_in_media,
                    "embedded={embedded_target:?}, export={export_target:?}"
                );
            }
        }
    }

    #[test]
    fn sidecar_subtitle_state_matrix_should_distinguish_conversion_from_import() {
        let targets = std::iter::once(None)
            .chain(SubtitleFormat::COMMON_TARGETS.map(Some))
            .collect::<Vec<_>>();

        for embedded_target in targets {
            for import_into_media in [false, true] {
                let change = SubtitleChange {
                    cue_text: Default::default(),
                    source: SubtitleSource::Sidecar(PathBuf::from("/media/movie.eng.srt")),
                    source_format: SubtitleFormat::SubRip,
                    embedded_target,
                    export_target: None,
                    import_into_media,
                    ocr_language: None,
                    metadata: None,
                };
                let converted =
                    embedded_target.is_some_and(|target| target != SubtitleFormat::SubRip);

                assert_that!(change.removes_from_media()).is_false();
                assert_eq!(change.changes_media(), import_into_media);
                assert_eq!(change.has_effect(), import_into_media || converted);
            }
        }
    }

    #[test]
    fn overview_label_should_return_compact_labels_when_format_is_known() {
        // Arrange
        let formats = SubtitleFormat::COMMON_TARGETS;

        // Act
        let labels = formats
            .into_iter()
            .map(SubtitleFormat::overview_label)
            .collect::<Vec<_>>();

        // Assert
        assert_that!(labels).contains_exactly_in_given_order([
            "SRT", "ASS", "VTT", "TTML", "MOVTXT", "PGS", "VobSub",
        ]);
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
        let subtitle = file(&directory, "movie.eng.forced.cc.sdh.2.srt");
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
        assert_that!(entry.hearing_impaired).is_true();
        assert_that!(entry.number).contains(2);

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn partition_sidecars_should_match_sidecars_for_multiple_media_files_sharing_the_same_stem() {
        let directory = std::env::temp_dir().join(format!(
            "reel-multi-stem-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let media_mkv = file(&directory, "movie.mkv");
        let media_mp4 = file(&directory, "movie.mp4");
        let subtitle = file(&directory, "movie.eng.srt");

        let (visible, sidecars) =
            partition_sidecars(vec![media_mkv.clone(), media_mp4.clone(), subtitle]);

        assert_that!(visible).has_length(2);
        assert_that!(&sidecars[&media_mkv.path]).has_length(1);
        assert_that!(&sidecars[&media_mp4.path]).has_length(1);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sidecar_accessibility_aliases_should_collapse_to_hearing_impaired() {
        let directory = std::env::temp_dir().join(format!(
            "reel-sidecar-aliases-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let media = file(&directory, "movie.mkv");
        let media_by_stem = media_paths_by_stem(std::slice::from_ref(&media));

        for filename in [
            "movie.eng.cc.srt",
            "movie.eng.sdh.srt",
            "movie.eng.hi.srt",
            "movie.eng.cc.sdh.srt",
        ] {
            let (_, parsed) =
                parse_sidecar_for_media(filename, SubtitleFormat::SubRip, &media_by_stem).unwrap();
            assert_that!(parsed.hearing_impaired).is_true();
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_sidecar_whose_language_token_is_not_a_language_should_not_be_matched() {
        // Arrange: the token after the media stem is only a language if it looks like one.
        // Accepting anything here would attach unrelated files — `movie.backup.srt`,
        // `movie.2024-01-01.srt` — to the film as subtitle tracks the user never added,
        // and then offer to mux them in.
        let directory = std::env::temp_dir().join(format!(
            "reel-sidecar-bad-language-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let media = file(&directory, "movie.mkv");
        let media_by_stem = media_paths_by_stem(std::slice::from_ref(&media));

        // Act / Assert
        for filename in [
            // Too short to be a language tag.
            "movie.e.srt",
            // Too long.
            "movie.abcdefghi.srt",
            // Characters a language tag never contains.
            "movie.en_us.srt",
            "movie.en us.srt",
            "movie.en+gb.srt",
        ] {
            assert!(
                parse_sidecar_for_media(filename, SubtitleFormat::SubRip, &media_by_stem).is_none(),
                "{filename} must not be matched as a subtitle for the film",
            );
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_sidecar_for_a_different_film_should_not_be_matched() {
        // Arrange: name matching is by stem prefix, so a file that merely starts with
        // similar text must not attach. `movie2.eng.srt` belongs to another film.
        let directory = std::env::temp_dir().join(format!(
            "reel-sidecar-other-film-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let media = file(&directory, "movie.mkv");
        let media_by_stem = media_paths_by_stem(std::slice::from_ref(&media));

        // Act / Assert
        for filename in [
            "movie2.eng.srt",
            "other.eng.srt",
            // The stem alone, with no language token at all.
            "movie.srt",
            // No extension to strip.
            "movie",
        ] {
            assert!(
                parse_sidecar_for_media(filename, SubtitleFormat::SubRip, &media_by_stem).is_none(),
                "{filename} must not be matched",
            );
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_duplicate_number_should_be_read_but_junk_after_it_should_reject_the_file() {
        // Arrange: `movie.eng.2.srt` is the second English subtitle — the numbering this
        // codebase writes itself when exporting duplicates, so it must round-trip. A
        // token that is neither a flag nor a number after that is not a name this scheme
        // produces, and attaching it anyway would misreport what the file is.
        let directory = std::env::temp_dir().join(format!(
            "reel-sidecar-number-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let media = file(&directory, "movie.mkv");
        let media_by_stem = media_paths_by_stem(std::slice::from_ref(&media));

        // Act / Assert: the number is read.
        let (_, parsed) =
            parse_sidecar_for_media("movie.eng.2.srt", SubtitleFormat::SubRip, &media_by_stem)
                .unwrap();
        assert_that!(parsed.number).is_equal_to(Some(2));
        assert_that!(parsed.forced).is_false();
        assert_that!(parsed.hearing_impaired).is_false();

        // Act / Assert: a flag and a number together still parse, in either order.
        let (_, parsed) = parse_sidecar_for_media(
            "movie.eng.forced.3.srt",
            SubtitleFormat::SubRip,
            &media_by_stem,
        )
        .unwrap();
        assert_that!(parsed.number).is_equal_to(Some(3));
        assert_that!(parsed.forced).is_true();

        // Act / Assert: an unrecognised token after the number is rejected outright.
        assert!(
            parse_sidecar_for_media(
                "movie.eng.2.backup.srt",
                SubtitleFormat::SubRip,
                &media_by_stem,
            )
            .is_none(),
            "a trailing unknown token after the number must reject the file",
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_undetermined_language_tag_should_not_be_treated_as_a_real_language() {
        // Arrange / Act / Assert: `und` is ffmpeg's placeholder for "no language set",
        // not a language. Canonicalising it into a real code would label every untagged
        // track as if the user had chosen that language.
        assert_that!(canonical_language_code("und")).is_none();
        assert_that!(canonical_language_code("UND")).is_none();
        assert_that!(canonical_language_code("")).is_none();
        assert_that!(canonical_language_code("   ")).is_none();
        // A real tag still resolves, so the guard is not over-broad.
        assert_that!(canonical_language_code("en").as_deref()).contains("eng");
    }

    /// The `.idx` is half the subtitle, so pairing it up is what keeps it out of the
    /// file list — otherwise it shows as a media file the user can open.
    #[test]
    fn partition_sidecars_should_claim_the_idx_companion_alongside_its_sub() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-vobsub-pair-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let media = file(&directory, "movie.mkv");
        let subtitle = file(&directory, "movie.eng.sub");
        let index = file(&directory, "movie.eng.idx");

        // Act
        let (visible, sidecars) =
            partition_sidecars(vec![media.clone(), subtitle.clone(), index.clone()]);

        // Assert
        assert_that!(visible.len()).is_equal_to(1);
        assert_that!(visible[0].path.clone()).is_equal_to(media.path.clone());
        let entries = sidecars.get(&media.path).unwrap();
        assert_that!(entries).has_length(1);
        assert_that!(entries[0].format).is_equal_to(SubtitleFormat::VobSub);
        assert_that!(entries[0].companion.clone()).contains(index.path);

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_repeated_flag_token_should_not_be_read_as_a_duplicate_number() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-sidecar-tokens-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let media = file(&directory, "movie.mkv");
        let repeated = file(&directory, "movie.eng.forced.forced.srt");

        // Act
        let (_, sidecars) = partition_sidecars(vec![media.clone(), repeated]);

        // Assert: the second `forced` is consumed as the (unparseable) number slot
        // rather than rejecting the file outright.
        let entries = sidecars.get(&media.path).unwrap();
        assert_that!(entries[0].forced).is_true();
        assert_that!(entries[0].number).is_none();

        // Cleanup
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_blank_or_undetermined_language_should_normalize_to_und() {
        // Act / Assert
        assert_that!(normalized_language("")).is_equal_to("und");
        assert_that!(normalized_language("   ")).is_equal_to("und");
        assert_that!(normalized_language("und")).is_equal_to("und");
        assert_that!(normalized_language("UND")).is_equal_to("und");
        assert_that!(normalized_language("  eng  ")).is_equal_to("eng");
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
        assert_that!(result).is_equal_to("movie.eng.forced.sdh.2.srt".to_string());
    }

    #[test]
    fn caption_and_hearing_impaired_dispositions_should_remain_independent() {
        let captions = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"disposition": {"captions": 1, "hearing_impaired": 0}}),
        )
        .unwrap();
        let hearing_impaired = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"disposition": {"captions": 0, "hearing_impaired": 1}}),
        )
        .unwrap();

        assert_that!(stream_cc(&captions)).is_true();
        assert_that!(stream_hearing_impaired(&captions)).is_false();
        assert_that!(stream_cc(&hearing_impaired)).is_false();
        assert_that!(stream_hearing_impaired(&hearing_impaired)).is_true();
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
            ffmpeg_filters: BTreeSet::new(),
            ffmpeg_decoders: BTreeSet::new(),
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
            ffmpeg_filters: BTreeSet::new(),
            ffmpeg_decoders: BTreeSet::new(),
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

    #[test]
    fn converting_between_two_different_image_formats_should_never_be_offered() {
        // Arrange: PGS and VobSub are both bitmap formats, so "converting" one to the
        // other means OCR to text and re-rendering back to bitmaps — two lossy steps that
        // produce visibly worse subtitles than the source. It is refused regardless of
        // which tools are installed, so a fully-equipped machine is used here.
        let capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from(["subrip".to_string(), "dvdsub".to_string()]),
            ffmpeg_muxers: BTreeSet::new(),
            ffmpeg_filters: BTreeSet::new(),
            ffmpeg_decoders: BTreeSet::new(),
            seconv: true,
            tesseract_languages: vec!["eng".to_string()],
        };

        // Act
        let from_pgs = capabilities.format_choices(SubtitleFormat::Pgs, Some("mkv"), false, false);
        let vobsub = from_pgs
            .iter()
            .find(|choice| choice.value == Some(SubtitleFormat::VobSub))
            .unwrap();

        // Assert
        assert_that!(vobsub.enabled).is_false();
        assert_that!(vobsub.reason.as_deref().unwrap()).contains("cross-image");
    }

    #[test]
    fn seconv_should_be_judged_by_its_help_listing_because_its_version_lies() {
        // Arrange: the v5.1.0 release reports "5.0.0" from `--version`, so a version
        // gate would reject the exact build reel needs. These are the two listings that
        // have to be told apart, trimmed to the line that distinguishes them: 5.0.0
        // cannot read a VobSub `.idx` at all, and 5.1.0's VobSub OCR rework is what
        // added the colour-isolation flag.
        let five_zero = "  --ocr-engine:[engine]    OCR engine: tesseract | nocr | \n  \
                         --time-codes-only        Image sources (.sup/VobSub/PGS/DVB)\n";
        let five_one = "  --ocr-engine:[engine]    OCR engine: tesseract | nocr | \n  \
                        --time-codes-only        Image sources (.sup/VobSub/PGS/DVB)\n  \
                        --no-vobsub-isolate-colors  Disable VobSub OCR colour isolation\n";

        // Act / Assert: mentioning VobSub is not enough — both listings do. Only the
        // flag itself may count, or 5.0.0 reads as supported and every image-subtitle
        // conversion fails after the user commits to it.
        assert_that!(seconv_is_supported(five_zero)).is_false();
        assert_that!(seconv_is_supported(five_one)).is_true();
        assert_that!(seconv_is_supported("")).is_false();
    }

    #[test]
    fn tesseract_should_be_offered_only_when_it_is_new_enough_to_ocr_well() {
        // Arrange: the banners of the Tesseract builds reel was measured against, plus
        // the 3.x that predates the LSTM engine and the flags seconv drives it with.
        let list = || Some("List of available languages (2):\neng\nosd\nnld\n".to_string());

        // Act
        let modern = detect_tesseract_languages(|| Some("tesseract 5.5.3\n".to_string()), list);
        let oldest_supported =
            detect_tesseract_languages(|| Some("tesseract 4.0.0\n".to_string()), list);
        let too_old = detect_tesseract_languages(|| Some("tesseract 3.04.01\n".to_string()), list);
        let missing = detect_tesseract_languages(|| None, list);
        let unreadable = detect_tesseract_languages(|| Some("tesseract\n".to_string()), list);
        // A supported Tesseract with no language data installed is still no languages.
        let no_data = detect_tesseract_languages(|| Some("tesseract 5.5.3\n".to_string()), || None);

        // Assert
        assert_that!(modern)
            .contains_exactly_in_given_order(["eng".to_string(), "nld".to_string()]);
        assert_that!(oldest_supported)
            .contains_exactly_in_given_order(["eng".to_string(), "nld".to_string()]);
        assert_that!(too_old).is_empty();
        assert_that!(missing).is_empty();
        assert_that!(unreadable).is_empty();
        assert_that!(no_data).is_empty();
    }

    #[test]
    fn a_too_old_tesseract_should_not_be_consulted_for_languages_at_all() {
        // Arrange: the version has to be checked *before* the language list is read, or
        // a 3.x install reports usable languages and reel offers an OCR conversion that
        // silently produces garbage text instead of an unavailable choice.
        let mut listed = false;

        // Act
        let languages = detect_tesseract_languages(
            || Some("tesseract 3.04.01\n".to_string()),
            || {
                listed = true;
                Some("List of available languages (1):\neng\n".to_string())
            },
        );

        // Assert
        assert_that!(languages).is_empty();
        assert_that!(listed).is_false();
    }

    #[test]
    fn extracting_an_embedded_vobsub_unchanged_should_still_require_seconv() {
        // Arrange: pulling VobSub out of a container to a sidecar is not a no-op even
        // though the format is unchanged — the `.sub`/`.idx` pair has to be written, which
        // is seconv's job. Treating "same format" as always-available would offer an
        // export that fails once the user commits to it.
        let mut capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from(["dvdsub".to_string()]),
            ffmpeg_muxers: BTreeSet::new(),
            ffmpeg_filters: BTreeSet::new(),
            ffmpeg_decoders: BTreeSet::new(),
            seconv: false,
            tesseract_languages: Vec::new(),
        };

        // Act: extracting_embedded is what makes the same-format entry a real conversion.
        let without_seconv = capabilities.format_choices(SubtitleFormat::VobSub, None, true, true);
        capabilities.seconv = true;
        let with_seconv = capabilities.format_choices(SubtitleFormat::VobSub, None, true, true);
        let find = |choices: &[FormatChoice]| {
            choices
                .iter()
                .find(|choice| choice.format == SubtitleFormat::VobSub)
                .cloned()
                .unwrap()
        };

        // Assert
        let blocked = find(&without_seconv);
        assert_that!(blocked.enabled).is_false();
        assert_that!(blocked.reason.as_deref().unwrap()).contains("seconv");
        assert_that!(find(&with_seconv).enabled).is_true();
    }

    #[test]
    fn text_conversions_should_be_refused_outright_when_ffmpeg_is_missing() {
        // Arrange: without ffmpeg there is nothing to convert text subtitles with. The
        // reason must name ffmpeg specifically — "FFmpeg encoder is unavailable" would
        // send the user looking for a codec when the whole binary is absent.
        let capabilities = ToolCapabilities {
            ffmpeg: false,
            ffmpeg_encoders: BTreeSet::new(),
            ffmpeg_muxers: BTreeSet::new(),
            ffmpeg_filters: BTreeSet::new(),
            ffmpeg_decoders: BTreeSet::new(),
            seconv: true,
            tesseract_languages: vec!["eng".to_string()],
        };

        // Act
        let choices =
            capabilities.format_choices(SubtitleFormat::SubRip, Some("mkv"), false, false);
        let ass = choices
            .iter()
            .find(|choice| choice.value == Some(SubtitleFormat::Ass))
            .unwrap();

        // Assert
        assert_that!(ass.enabled).is_false();
        assert_that!(ass.reason.as_deref().unwrap()).contains("requires ffmpeg in PATH");
    }

    #[test]
    fn common_languages_should_be_searchable_and_exclude_undetermined() {
        let choices = common_language_choices();

        assert_that!(choices.iter().any(|choice| choice.code == "und")).is_false();
        assert_that!(
            choices
                .iter()
                .find(|choice| choice.code == "nld")
                .unwrap()
                .matches("dut")
        )
        .is_true();
        assert_that!(
            choices
                .iter()
                .find(|choice| choice.code == "nld")
                .unwrap()
                .matches("nl")
        )
        .is_true();
        assert_that!(language_choice("und")).is_none();
        assert_that!(language_choice("en-US").unwrap().code.as_str()).is_equal_to("eng-us");
    }

    #[test]
    fn legacy_language_codes_should_be_read_but_canonicalized_for_output() {
        for (legacy, canonical) in [
            ("alb", "sqi"),
            ("arm", "hye"),
            ("baq", "eus"),
            ("bur", "mya"),
            ("chi", "zho"),
            ("cze", "ces"),
            ("dut", "nld"),
            ("fre", "fra"),
            ("geo", "kat"),
            ("ger", "deu"),
            ("gre", "ell"),
            ("ice", "isl"),
            ("mac", "mkd"),
            ("mao", "mri"),
            ("may", "msa"),
            ("per", "fas"),
            ("rum", "ron"),
            ("slo", "slk"),
            ("tib", "bod"),
            ("wel", "cym"),
        ] {
            assert_that!(canonical_language_code(legacy).as_deref()).contains(canonical);
            assert_that!(language_choice(legacy).unwrap().code.as_str()).is_equal_to(canonical);
        }
        assert_that!(canonical_language_code("cze-CZ").as_deref()).contains("ces-cz");
        assert_that!(canonical_language_code("und")).is_none();
        assert_that!(canonical_language_code("invalid-language-tag")).is_none();
    }

    #[test]
    fn embedded_and_sidecar_languages_should_emit_canonical_codes() {
        let stream = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"tags": {"language": "cze"}}),
        )
        .unwrap();

        assert_that!(stream_language(&stream)).is_equal_to("ces".to_string());
        assert_that!(sidecar_filename(
            "movie",
            "dut",
            false,
            false,
            None,
            SubtitleFormat::SubRip,
        ))
        .is_equal_to("movie.nld.srt".to_string());
    }

    #[test]
    fn stream_title_should_fall_back_to_the_iso_bmff_name_tag() {
        // Arrange: ffprobe reports an MP4/MOV track's name under `name`, so a title that
        // survived a remux into MP4 reads as absent unless both keys are consulted.
        let mp4 = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"tags": {"language": "kor", "name": "Forced"}}),
        )
        .unwrap();
        let both = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"tags": {"title": "Matroska", "name": "Ignored"}}),
        )
        .unwrap();
        let blank_title = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"tags": {"title": "  ", "name": "Forced"}}),
        )
        .unwrap();

        // Act / Assert: `title` still wins when present, and an empty one does not
        // shadow a real `name`.
        assert_that!(stream_title(&mp4)).is_equal_to(Some("Forced".to_string()));
        assert_that!(stream_title(&both)).is_equal_to(Some("Matroska".to_string()));
        assert_that!(stream_title(&blank_title)).is_equal_to(Some("Forced".to_string()));
    }

    #[test]
    fn stream_title_should_extract_and_trim_title_tag() {
        let present = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"tags": {"title": "  My Title  "}}),
        )
        .unwrap();
        let empty = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"tags": {"title": ""}}),
        )
        .unwrap();
        let whitespace = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"tags": {"title": "   "}}),
        )
        .unwrap();
        let missing_title = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"tags": {"language": "eng"}}),
        )
        .unwrap();
        let missing_tags = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"codec_name": "srt"}),
        )
        .unwrap();

        assert_that!(stream_title(&present)).is_equal_to(Some("My Title".to_string()));
        assert_that!(stream_title(&empty)).is_none();
        assert_that!(stream_title(&whitespace)).is_none();
        assert_that!(stream_title(&missing_title)).is_none();
        assert_that!(stream_title(&missing_tags)).is_none();
    }

    #[test]
    fn stream_disposition_flags_should_be_parsed_correctly() {
        let forced = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"disposition": {"forced": 1, "original": 0, "comment": 0}}),
        )
        .unwrap();
        let original = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"disposition": {"forced": 0, "original": 1, "comment": 0}}),
        )
        .unwrap();
        let comment = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"disposition": {"forced": 0, "original": 0, "comment": 1}}),
        )
        .unwrap();
        let missing =
            serde_json::from_value::<BTreeMap<String, serde_json::Value>>(serde_json::json!({}))
                .unwrap();

        assert_that!(stream_forced(&forced)).is_true();
        assert_that!(stream_forced(&original)).is_false();
        assert_that!(stream_forced(&missing)).is_false();

        assert_that!(stream_original(&original)).is_true();
        assert_that!(stream_original(&forced)).is_false();
        assert_that!(stream_original(&missing)).is_false();

        assert_that!(stream_commentary(&comment)).is_true();
        assert_that!(stream_commentary(&forced)).is_false();
        assert_that!(stream_commentary(&missing)).is_false();

        let hi = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"disposition": {"hearing_impaired": 1}}),
        )
        .unwrap();
        assert_that!(stream_hearing_impaired(&hi)).is_true();
        let not_hi = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"disposition": {"hearing_impaired": 0}}),
        )
        .unwrap();
        assert_that!(stream_hearing_impaired(&not_hi)).is_false();
    }

    #[test]
    fn stream_cc_should_detect_captions_disposition_or_eia_codecs() {
        let captions_disp = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"disposition": {"captions": 1}}),
        )
        .unwrap();
        let eia_608 = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"codec_name": "eia_608"}),
        )
        .unwrap();
        let eia_708 = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"codec_name": "eia_708"}),
        )
        .unwrap();
        let neither = serde_json::from_value::<BTreeMap<String, serde_json::Value>>(
            serde_json::json!({"disposition": {"captions": 0}, "codec_name": "subrip"}),
        )
        .unwrap();

        assert_that!(stream_cc(&captions_disp)).is_true();
        assert_that!(stream_cc(&eia_608)).is_true();
        assert_that!(stream_cc(&eia_708)).is_true();
        assert_that!(stream_cc(&neither)).is_false();
    }

    /// The filter listing prints its legend in the same shape as its entries, and its
    /// flag column carries none of the letters the encoder parser keys on — so this needs
    /// its own rule, and getting it wrong means either no filters or a filter named "=".
    #[test]
    fn parse_filter_names_should_take_the_entries_and_leave_the_legend() {
        // Arrange: the real shape of `ffmpeg -filters`.
        let output = "Filters:
  T.. = Timeline support
  .S. = Slice threading
  A = Audio input/output
  ------
 .. acompressor    A->A       Audio compressor.
 .. scale          V->V       Scale the input video size.
 ..C subtitles     V->V       Render text subtitles using the libass library.
 T.. overlay       VV->V      Overlay a video source on top of the input.
        ";

        // Act
        let filters = parse_filter_names(output);

        // Assert
        assert_that!(filters.contains("subtitles")).is_true();
        assert_that!(filters.contains("scale")).is_true();
        assert_that!(filters.contains("overlay")).is_true();
        assert_that!(filters.contains("=")).is_false();
        assert_that!(filters.contains("Timeline")).is_false();
        assert_that!(filters.len()).is_equal_to(4);
    }

    /// The timing page reads where the rest of the program writes, so it has to ask about
    /// decoders rather than encoders. FFmpeg ships plenty of one without the other — TTML
    /// most notably, which is why that format does not take this road at all.
    #[test]
    fn preview_should_be_blocked_by_a_missing_decoder_rather_than_a_missing_encoder() {
        // Arrange: a build that can write every text format and read only SubRip, which a
        // check against `ffmpeg_encoders` would wave straight through.
        let capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_encoders: BTreeSet::from([
                "subrip".to_string(),
                "webvtt".to_string(),
                "mov_text".to_string(),
            ]),
            ffmpeg_decoders: BTreeSet::from(["subrip".to_string()]),
            ..ToolCapabilities::default()
        };

        // Act / Assert
        assert_that!(capabilities.preview_blocked(SubtitleFormat::SubRip)).is_none();
        // ASS too: this crate parses it, off a `-c:s copy` that decodes nothing. A build
        // with no `ass` decoder still previews one, which is the point of asking per
        // format rather than asking once.
        assert_that!(capabilities.preview_blocked(SubtitleFormat::Ass)).is_none();
        let vtt = capabilities
            .preview_blocked(SubtitleFormat::WebVtt)
            .expect("a build that cannot read WebVTT should refuse it");
        assert_that!(vtt.as_str()).contains("VTT");
        assert_that!(vtt.as_str()).contains("decode it");
        assert_that!(
            capabilities
                .preview_blocked(SubtitleFormat::MovText)
                .is_some()
        )
        .is_true();

        // Act / Assert: and a build that can read them lets them through.
        let readable = ToolCapabilities {
            ffmpeg_decoders: BTreeSet::from([
                "subrip".to_string(),
                "webvtt".to_string(),
                "mov_text".to_string(),
            ]),
            ..capabilities
        };
        assert_that!(readable.preview_blocked(SubtitleFormat::WebVtt)).is_none();
        assert_that!(readable.preview_blocked(SubtitleFormat::MovText)).is_none();
    }

    /// The formats with no road to a cue list say so whatever is installed, since no
    /// amount of tooling changes that there is nothing yet to read them with.
    #[test]
    fn preview_should_refuse_the_formats_with_no_road_to_a_cue_list() {
        // Arrange: everything installed.
        let capabilities = ToolCapabilities {
            ffmpeg: true,
            ffmpeg_decoders: BTreeSet::from([
                "subrip".to_string(),
                "ass".to_string(),
                "webvtt".to_string(),
                "mov_text".to_string(),
                "hdmv_pgs_subtitle".to_string(),
                "dvd_subtitle".to_string(),
            ]),
            seconv: true,
            tesseract_languages: vec!["eng".to_string()],
            ..ToolCapabilities::default()
        };

        // Act / Assert
        for format in [
            SubtitleFormat::Ttml,
            SubtitleFormat::Pgs,
            SubtitleFormat::VobSub,
        ] {
            let reason = capabilities
                .preview_blocked(format)
                .unwrap_or_else(|| panic!("{format:?} should be refused"));
            assert_that!(reason.as_str()).contains(format.overview_label());
            assert_that!(reason.as_str()).contains("not implemented yet");
        }
    }

    /// A build without libass has no `subtitles` filter at all, and the timing page has
    /// to notice before it starts one doomed `ffmpeg` per settled selection.
    #[test]
    fn can_burn_subtitles_should_require_both_filters_the_frame_grab_uses() {
        // Arrange
        let with = |names: [&str; 2]| ToolCapabilities {
            ffmpeg_filters: names.iter().map(|name| name.to_string()).collect(),
            ..ToolCapabilities::default()
        };

        // Act / Assert
        assert_that!(with(["subtitles", "scale"]).can_burn_subtitles()).is_true();
        assert_that!(with(["scale", "overlay"]).can_burn_subtitles()).is_false();
        assert_that!(with(["subtitles", "overlay"]).can_burn_subtitles()).is_false();
        assert_that!(ToolCapabilities::default().can_burn_subtitles()).is_false();
    }

    #[test]
    fn parse_capability_names_should_extract_encoders_and_muxers() {
        let output = " Encoders:
  V..... = Video
  A..... = Audio
  S..... = Subtitle
  .E.... = Frame-level multithreading
  ..S... = Slice-level multithreading
  ...X.. = Codec is experimental
  ....B. = Supports draw_horiz_band
  .....D = Supports direct rendering method 1
  ------
  V..... libx264              libx264 H.264
  S..... srt                  SubRip subtitle
  S..... webvtt               WebVTT subtitle
  A..X.. aac                  AAC
        ";

        let capabilities = parse_capability_names(output);

        assert_that!(capabilities.contains("srt")).is_true();
        assert_that!(capabilities.contains("webvtt")).is_true();
        assert_that!(capabilities.contains("libx264")).is_true();
        assert_that!(capabilities.contains("aac")).is_true();
        // The flag legend is printed in the same shape as a real entry, so it must not
        // contribute a capability of its own.
        assert_that!(capabilities.contains("=")).is_false();

        let muxer_output = " File formats:
  D. = Demuxing supported
  .E = Muxing supported
  --
  E  3g2             3GP2 (3GPP2 file format)
  .E mp4             MP4 (MPEG-4 Part 14)
  D  matroska        Matroska
        ";
        let muxers = parse_capability_names(muxer_output);
        assert_that!(muxers.contains("3g2")).is_true();
        assert_that!(muxers.contains("mp4")).is_true();
        assert_that!(muxers.contains("matroska")).is_false();
    }

    #[test]
    fn parse_tesseract_languages_should_extract_languages_and_ignore_osd() {
        let output = "List of available languages (3):
eng
osd
fra
";
        let langs = parse_tesseract_languages(output);
        assert_that!(langs).contains_exactly_in_given_order(["eng".to_string(), "fra".to_string()]);
    }

    fn cue_edit(original: &str, text: &str) -> CueTextEdit {
        CueTextEdit {
            original: original.to_string(),
            text: text.to_string(),
        }
    }

    const THREE_CUES: &str = "1\n00:00:01,000 --> 00:00:02,000\none\n\n\
                              2\n00:00:03,000 --> 00:00:04,000\ntwo\n\n\
                              3\n00:00:05,000 --> 00:00:06,000\nthree\n\n";

    /// The edited cue changes and every other one is left exactly as it was — including
    /// its timing, which the editor never touches and a rewrite must not round.
    #[test]
    fn rewrite_srt_cues_should_change_the_edited_cue_and_nothing_else() {
        // Arrange
        let edits = BTreeMap::from([(1, cue_edit("two", "two, rewritten\nover two lines"))]);

        // Act
        let written = rewrite_srt_cues(THREE_CUES, &edits).expect("the edit should apply");

        // Assert
        let cues = crate::cue::parse_srt(&written);
        let texts: Vec<&str> = cues.iter().map(|cue| cue.text.as_str()).collect();
        assert_that!(texts).contains_exactly_in_given_order([
            "one",
            "two, rewritten\nover two lines",
            "three",
        ]);
        assert_that!(cues[2].start).is_equal_to(std::time::Duration::from_secs(5));
    }

    /// A cue has no identity in the file, so an edit is addressed by position — which is
    /// only meaningful against the list it was taken from. A file changed elsewhere between
    /// staging and saving must stop the save rather than have this text land on whichever
    /// line moved into the slot.
    #[test]
    fn rewrite_srt_cues_should_refuse_when_the_file_no_longer_matches() {
        // Act / Assert: the cue at that position now says something else.
        let moved = rewrite_srt_cues(
            THREE_CUES,
            &BTreeMap::from([(1, cue_edit("SOMETHING ELSE", "x"))]),
        );
        assert_that!(moved.clone().unwrap_err().as_str()).contains("changed elsewhere");

        // Act / Assert: and the cue is gone from the file entirely.
        let missing = rewrite_srt_cues(THREE_CUES, &BTreeMap::from([(9, cue_edit("nine", "x"))]));
        assert_that!(missing.unwrap_err().as_str()).contains("no longer has a cue");
    }
}
