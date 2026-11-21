//! What the keybindings popup says, and which keys are live where.
//!
//! The popup used to draw one flat list of every binding in the application, which had two
//! defects that got worse with every feature: it listed keys that do nothing where the
//! reader is standing, and — because `i`, `d`, `c`, `/`, `h`/`l`, `Ctrl-h`/`Ctrl-l` and
//! `Ctrl-j`/`Ctrl-k` each mean two or three different things — its descriptions became
//! run-on sentences naming every mode a key could be pressed in. A reader looking for what
//! `h` does had to work out which clause applied to them.
//!
//! So the catalogue is indexed by [`KeyContext`], which is the page or popup the reader is
//! in *plus* the sub-state that changes what a key means: which pane of the subtitle edit
//! page holds the cursor, whether a timing mode is on, and whether a search bar is taking
//! keys. [`sections`] answers with what is live right now and nothing else, so each key
//! gets one plain description.
//!
//! **This module is pure data.** No `ratatui`, no `App`, no subprocesses — so the whole
//! catalogue is testable without a `Frame`, and `ui.rs` does not grow another thousand
//! lines. [`App::key_context`](crate::app::App::key_context) is the resolver that maps the
//! application's state onto a context; this module is what that context is worth.

use crate::subtitle_edit::{EditFocus, TimingScope};

/// One row of the popup: the key, what it does here, and the paragraph `K` opens.
///
/// `description` is the one-line answer on the row itself; `explanation` is what the help
/// panel says when the cursor is on it. They are different lengths on purpose — the row
/// has to fit a column and the panel has a page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Keybinding {
    pub keys: &'static str,
    pub description: &'static str,
    /// What the key is *for*, in the house style of the settings dialogs' help panels: the
    /// trade the reader is making, not the mechanism. Paragraphs are split on `\n\n`.
    pub explanation: &'static str,
}

/// A named group of rows, drawn with a cyan heading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeySection {
    pub name: &'static str,
    pub bindings: Vec<Keybinding>,
}

impl KeySection {
    fn new(name: &'static str, bindings: &[Keybinding]) -> Self {
        Self {
            name,
            bindings: bindings.to_vec(),
        }
    }
}

const fn key(
    keys: &'static str,
    description: &'static str,
    explanation: &'static str,
) -> Keybinding {
    Keybinding {
        keys,
        description,
        explanation,
    }
}

/// Which shape a settings popup's keys are in.
///
/// The four settings dialogs (container, audio, video, subtitle) all route through the same
/// five states, so they share one enum rather than four parallel ones. `CustomResolution`
/// is reached only from the video dialog; naming it here keeps [`KeyContext`] flat instead
/// of giving video a variant of its own that differs in one arm.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SettingsKeyMode {
    /// A text field is taking keys — a title, or the container's metadata.
    Editing,
    /// The language dropdown's search bar is taking keys.
    LanguageSearch,
    /// The language dropdown is open but its search bar is closed.
    LanguageList,
    /// Walking the rows, or a non-language dropdown.
    #[default]
    Browsing,
    /// The video dialog's typed width and height.
    CustomResolution,
}

/// Where the reader is, at the granularity that decides what a key does.
///
/// The order of the variants follows
/// [`handle_key`](crate::input::handle_key)'s own routing — every dialog first, then the
/// layers — because [`App::key_context`](crate::app::App::key_context) is a guard chain
/// mirroring that function and the two are read side by side.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyContext {
    /// The cue editor's multi-line buffer.
    EditCue,
    /// Typing a cue's on-screen length.
    CueLength,
    /// "Leaving discards them."
    ConfirmLeaveCues,
    /// "What should I make?" — the new-track popup.
    CreateTrack {
        searching: bool,
    },
    /// Confirm-cancel over a running batch.
    ConfirmCancel,
    ContainerSettings {
        editing: bool,
    },
    PreviewSettings,
    AudioSettings {
        mode: SettingsKeyMode,
    },
    VideoSettings {
        mode: SettingsKeyMode,
    },
    SubtitleSettings {
        mode: SettingsKeyMode,
    },
    /// The popup itself. Reachable by the resolver, but the popup stores the context it was
    /// opened *from*, so this is what it would say about its own keys if ever asked.
    Keybindings {
        searching: bool,
    },
    Error,
    ConfirmProcessAll,
    BatchProcessing,
    ConfirmReset,
    ResolveConflicts,
    /// The modal cover over automatic sync, which takes no key at all.
    AutoSyncing,
    Files {
        searching: bool,
    },
    Streams,
    StreamDetails,
    SubtitleEdit {
        searching: bool,
        focus: EditFocus,
        timing: TimingScope,
    },
}

impl KeyContext {
    /// Every context, for the tests that assert none of them ships empty.
    ///
    /// Written out rather than derived: there is no crate-wide enumeration trait here, and
    /// a hand-written list that a new variant is forgotten from is caught by
    /// `every_context_should_be_listed_in_all`, which matches exhaustively.
    pub const ALL: &'static [Self] = &[
        Self::EditCue,
        Self::CueLength,
        Self::ConfirmLeaveCues,
        Self::CreateTrack { searching: false },
        Self::CreateTrack { searching: true },
        Self::ConfirmCancel,
        Self::ContainerSettings { editing: false },
        Self::ContainerSettings { editing: true },
        Self::PreviewSettings,
        Self::AudioSettings {
            mode: SettingsKeyMode::Editing,
        },
        Self::AudioSettings {
            mode: SettingsKeyMode::LanguageSearch,
        },
        Self::AudioSettings {
            mode: SettingsKeyMode::LanguageList,
        },
        Self::AudioSettings {
            mode: SettingsKeyMode::Browsing,
        },
        Self::VideoSettings {
            mode: SettingsKeyMode::Editing,
        },
        Self::VideoSettings {
            mode: SettingsKeyMode::LanguageSearch,
        },
        Self::VideoSettings {
            mode: SettingsKeyMode::LanguageList,
        },
        Self::VideoSettings {
            mode: SettingsKeyMode::Browsing,
        },
        Self::VideoSettings {
            mode: SettingsKeyMode::CustomResolution,
        },
        Self::SubtitleSettings {
            mode: SettingsKeyMode::Editing,
        },
        Self::SubtitleSettings {
            mode: SettingsKeyMode::LanguageSearch,
        },
        Self::SubtitleSettings {
            mode: SettingsKeyMode::LanguageList,
        },
        Self::SubtitleSettings {
            mode: SettingsKeyMode::Browsing,
        },
        Self::Keybindings { searching: false },
        Self::Keybindings { searching: true },
        Self::Error,
        Self::ConfirmProcessAll,
        Self::BatchProcessing,
        Self::ConfirmReset,
        Self::ResolveConflicts,
        Self::AutoSyncing,
        Self::Files { searching: false },
        Self::Files { searching: true },
        Self::Streams,
        Self::StreamDetails,
        Self::SubtitleEdit {
            searching: true,
            focus: EditFocus::Cues,
            timing: TimingScope::Off,
        },
        Self::SubtitleEdit {
            searching: false,
            focus: EditFocus::Timeline,
            timing: TimingScope::Off,
        },
        Self::SubtitleEdit {
            searching: false,
            focus: EditFocus::Cues,
            timing: TimingScope::Off,
        },
        Self::SubtitleEdit {
            searching: false,
            focus: EditFocus::Cues,
            timing: TimingScope::Cue(crate::subtitle_edit::CueGrip::Whole),
        },
        Self::SubtitleEdit {
            searching: false,
            focus: EditFocus::Cues,
            timing: TimingScope::Track,
        },
    ];

    /// What this context is called on the popup's title, so the reader can tell at a glance
    /// that the list narrowed to where they actually are.
    pub fn title(self) -> &'static str {
        match self {
            Self::EditCue => "Cue editor",
            Self::CueLength => "Cue length",
            Self::ConfirmLeaveCues => "Leaving the page",
            Self::CreateTrack { .. } => "New subtitle track",
            Self::ConfirmCancel => "Cancel processing",
            Self::ContainerSettings { .. } => "Container settings",
            Self::PreviewSettings => "Preview settings",
            Self::AudioSettings { .. } => "Audio track settings",
            Self::VideoSettings { .. } => "Video track settings",
            Self::SubtitleSettings { .. } => "Subtitle track settings",
            Self::Keybindings { .. } => "Keybindings",
            Self::Error => "Error",
            Self::ConfirmProcessAll => "Save",
            Self::BatchProcessing => "Processing",
            Self::ConfirmReset => "Reset",
            Self::ResolveConflicts => "Changed on disk",
            Self::AutoSyncing => "Automatic sync",
            Self::Files { .. } => "File list",
            Self::Streams => "Track list",
            Self::StreamDetails => "Track information",
            Self::SubtitleEdit {
                focus: EditFocus::Timeline,
                ..
            } => "Subtitle edit · timeline",
            Self::SubtitleEdit {
                timing: TimingScope::Cue(_),
                ..
            } => "Subtitle edit · timing a cue",
            Self::SubtitleEdit {
                timing: TimingScope::Track,
                ..
            } => "Subtitle edit · retiming the track",
            Self::SubtitleEdit { .. } => "Subtitle edit",
        }
    }
}

// ---------------------------------------------------------------------------
// Shared groups
//
// The keys several contexts answer identically are written once here. A context that
// answers a *different* thing with the same key states it in its own arm instead — that
// divergence is the whole reason this module exists.
// ---------------------------------------------------------------------------

const MOVE_ROWS: Keybinding = key(
    "j / k",
    "Move down or up a row",
    "Moves the cursor a row at a time. The arrow keys do the same thing.",
);

const MOVE_ENDS: Keybinding = key(
    "gg / G",
    "Go to the first or last row",
    "Jumps to either end of the list without holding a movement key down.",
);

const SCROLL_PAGE: Keybinding = key(
    "Ctrl-d / Ctrl-u",
    "Scroll ten rows",
    "Moves through a long list roughly a screenful at a time.",
);

const CONFIRM: Keybinding = key(
    "Enter",
    "Open, select, or confirm",
    "Takes the row the cursor is on: opens what can be opened, chooses what can be chosen, and confirms a prompt.",
);

const CLOSE: Keybinding = key(
    "Esc / q",
    "Close and go back",
    "Leaves whatever is on top. On the file list, with nothing above it, this quits the application.",
);

const SEARCH: Keybinding = key(
    "/",
    "Search",
    "Opens a search bar that narrows this list to what matches. Enter keeps the filter and hands the keys back; Esc drops it.",
);

const SAVE: Keybinding = key(
    "Ctrl-s",
    "Review and save staged edits",
    "Opens the save confirmation, which lists everything staged across every file before anything is written. Nothing reaches the disk until it is confirmed.",
);

const HELP_FIELD: Keybinding = key(
    "K",
    "Explain the highlighted field",
    "Opens a panel beside the dialog describing the row the cursor is on. It stays open as the cursor moves, so walking the rows reads as a tour of the dialog.",
);

/// The keys a search bar answers while it is taking input, in any of the places one opens.
fn search_bar_section() -> KeySection {
    KeySection::new(
        "Search",
        &[
            key(
                "Esc",
                "Discard the query and leave the bar",
                "Abandons the search and puts the cursor back where it was opened, as though the bar had never been up.",
            ),
            key(
                "Enter",
                "Keep the filter and leave the bar",
                "Accepts the search: the list stays narrowed and the movement keys work against it again.",
            ),
            key(
                "Ctrl-n / Ctrl-p",
                "Move through the matches",
                "Walks the results without leaving the bar, so a query can be refined and its hits checked in one pass. The arrow keys do the same thing.",
            ),
        ],
    )
}

/// Editing keys shared by every text field in the application.
fn text_input_section() -> KeySection {
    KeySection::new(
        "Text",
        &[
            key(
                "Left / Right",
                "Move the text cursor",
                "Moves a character at a time within the field.",
            ),
            key(
                "Home / End",
                "Go to the start or end",
                "Jumps to either end of the field. Ctrl-a and Ctrl-e do the same thing.",
            ),
            key(
                "Backspace / Delete",
                "Delete around the cursor",
                "Backspace removes the character before the cursor, Delete the one after it.",
            ),
            key(
                "Ctrl-w",
                "Delete the word before the cursor",
                "Removes back to the previous word boundary, for correcting a long value without holding Backspace.",
            ),
            key(
                "Ctrl-u",
                "Delete everything before the cursor",
                "Clears the field back to its start.",
            ),
            key(
                "Paste",
                "Insert the clipboard",
                "A paste arrives as one edit rather than as a burst of keystrokes, so a long value lands intact.",
            ),
        ],
    )
}

/// The three keys a settings dialog answers wherever its cursor is.
fn settings_common() -> Vec<Keybinding> {
    vec![HELP_FIELD, SAVE, CLOSE]
}

// ---------------------------------------------------------------------------
// The catalogue
// ---------------------------------------------------------------------------

/// What is live in `context`, as the popup draws it.
///
/// Matched exhaustively so a [`KeyContext`] added later cannot compile without being given
/// a list — the same guarantee `render_dialog` takes for `Dialog`.
pub fn sections(context: KeyContext) -> Vec<KeySection> {
    match context {
        KeyContext::EditCue => vec![
            KeySection::new(
                "Cue editor",
                &[
                    key(
                        "Enter",
                        "Start a new line",
                        "A subtitle cue is routinely two lines, and where it breaks is part of what the reader is judging — so Enter breaks the line rather than closing the editor.",
                    ),
                    key(
                        "Esc",
                        "Leave, keeping what was typed",
                        "Closes the editor the way leaving vim's insert mode does: the text stays staged. Nothing is discarded here — that is asked about once, on the way off the page.",
                    ),
                    key(
                        "Ctrl-s",
                        "Stage the cue and save",
                        "Closes the editor and opens the save confirmation, writing the cue into its file.",
                    ),
                ],
            ),
            text_input_section(),
        ],
        KeyContext::CueLength => vec![
            KeySection::new(
                "Cue length",
                &[
                    key(
                        "Enter",
                        "Use this length",
                        "Keeps the cue's start and moves its end, because the start is the half already judged against the picture. A value the media cannot hold keeps the dialog open and says why.",
                    ),
                    key(
                        "Esc",
                        "Discard and close",
                        "Drops what was typed. A half-typed length is quicker to retype than to find again, so nothing is kept across a visit.",
                    ),
                ],
            ),
            text_input_section(),
        ],
        KeyContext::ConfirmLeaveCues => vec![KeySection::new(
            "Leaving the page",
            &[
                key(
                    "h / l",
                    "Choose Stay or Discard",
                    "Moves between the two buttons. A bare Enter takes the safe half, because the other one throws away cue work that exists nowhere else.",
                ),
                CONFIRM,
                key(
                    "Esc / q",
                    "Stay here",
                    "Backing out of this question means staying on the page, so nothing is lost by pressing it.",
                ),
            ],
        )],
        KeyContext::CreateTrack { searching: true } => {
            vec![search_bar_section(), text_input_section()]
        }
        KeyContext::CreateTrack { searching: false } => vec![KeySection::new(
            "New subtitle track",
            &[
                MOVE_ROWS,
                key(
                    "h / l",
                    "Change this row's answer",
                    "Picks between the answers on a two-state row, such as whether the track goes inside the file or beside it.",
                ),
                key(
                    "Enter",
                    "Open a list, or press the button",
                    "Opens the row's choices, or on the button row creates the track.",
                ),
                SEARCH,
                key(
                    "c / b",
                    "Create, or Back out",
                    "The mnemonic letters picked out in cyan on the two buttons, reachable from any row so the track can be created without walking down to them.",
                ),
                MOVE_ENDS,
                CLOSE,
            ],
        )],
        KeyContext::ConfirmCancel => vec![KeySection::new(
            "Cancel processing",
            &[
                key(
                    "h / l",
                    "Choose Keep going or Cancel",
                    "Moves between the two buttons. Cancelling stops the batch and rolls back the file being written.",
                ),
                CONFIRM,
                key(
                    "Esc / q",
                    "Keep processing",
                    "Backing out of this question leaves the batch running.",
                ),
            ],
        )],
        KeyContext::ContainerSettings { editing: true } => vec![
            KeySection::new(
                "Editing a field",
                &[
                    key(
                        "Esc / Enter",
                        "Finish editing",
                        "Both accept what was typed and hand the keys back to the rows.",
                    ),
                    SAVE,
                ],
            ),
            text_input_section(),
        ],
        KeyContext::ContainerSettings { editing: false } => vec![KeySection::new(
            "Container settings",
            &[
                MOVE_ROWS,
                CONFIRM,
                key(
                    "i",
                    "Edit this text field",
                    "Starts typing into the highlighted metadata field, such as the title or the comment.",
                ),
                key(
                    "r",
                    "Reset this field",
                    "Puts the highlighted row back to what the file itself says, dropping the staged change to it.",
                ),
                MOVE_ENDS,
                HELP_FIELD,
                SAVE,
                CLOSE,
            ],
        )],
        KeyContext::PreviewSettings => vec![KeySection::new(
            "Preview settings",
            &[
                MOVE_ROWS,
                key(
                    "h / l",
                    "Choose Yes or No on a switch",
                    "Picks the left or right button on the loop and sound rows. Both answers fit on the row, so there is no list to open.",
                ),
                key(
                    "Enter",
                    "Open a list and choose from it",
                    "Opens the row's choices — speed, padding, frame rate, or which track the page previews against.",
                ),
                key(
                    "r",
                    "Reset this setting",
                    "Puts one row back to what config.toml asked for, rather than to a built-in default.",
                ),
                key(
                    "R",
                    "Reset every setting",
                    "Puts the whole popup back to what config.toml asked for.",
                ),
                MOVE_ENDS,
                HELP_FIELD,
                key(
                    "Esc / q",
                    "Close and apply the tracks",
                    "The playback settings are already in force; closing is what puts a changed video or subtitle track into effect, which re-opens the page on it.",
                ),
            ],
        )],
        KeyContext::AudioSettings { mode }
        | KeyContext::VideoSettings { mode }
        | KeyContext::SubtitleSettings { mode } => settings_sections(context, mode),
        KeyContext::Keybindings { searching: true } => {
            vec![search_bar_section(), text_input_section()]
        }
        KeyContext::Keybindings { searching: false } => vec![KeySection::new(
            "Keybindings",
            &[
                MOVE_ROWS,
                SCROLL_PAGE,
                MOVE_ENDS,
                SEARCH,
                key(
                    "?",
                    "Switch between this page and every key",
                    "Swaps between the keys this page answers and the whole application's list.",
                ),
                key(
                    "K",
                    "Explain the highlighted key",
                    "Opens a panel describing the row the cursor is on.",
                ),
                CLOSE,
            ],
        )],
        KeyContext::Error => vec![KeySection::new(
            "Error",
            &[key(
                "Enter / Esc",
                "Dismiss",
                "Closes the message. The full failure is also written to the edit error log.",
            )],
        )],
        KeyContext::ConfirmProcessAll => vec![KeySection::new(
            "Save",
            &[
                key(
                    "h / l",
                    "Choose Start or Back",
                    "Moves between the two buttons. Start dispatches every staged edit listed above.",
                ),
                MOVE_ROWS,
                SCROLL_PAGE,
                MOVE_ENDS,
                CONFIRM,
                key(
                    "Esc / q",
                    "Back out without saving",
                    "Closes the confirmation with everything still staged and nothing written.",
                ),
            ],
        )],
        KeyContext::BatchProcessing => vec![KeySection::new(
            "Processing",
            &[
                MOVE_ROWS,
                SCROLL_PAGE,
                key(
                    "Esc / q",
                    "Ask to cancel",
                    "Opens the cancel prompt. The batch keeps running until that prompt is answered, so this cannot stop a write by accident.",
                ),
            ],
        )],
        KeyContext::ConfirmReset => vec![KeySection::new(
            "Reset",
            &[
                key(
                    "h / l",
                    "Choose Cancel or Reset",
                    "Moves between the two buttons. Resetting discards staged work that cannot be recovered.",
                ),
                CONFIRM,
                key(
                    "Esc / q",
                    "Keep the staged work",
                    "Backing out of this question changes nothing.",
                ),
            ],
        )],
        KeyContext::ResolveConflicts => vec![KeySection::new(
            "Changed on disk",
            &[
                MOVE_ROWS,
                key(
                    "Enter / Space",
                    "Keep or discard this file's edits",
                    "Answers one row. Every file changed on disk since it was staged needs an answer, which is why this notice has no back key.",
                ),
            ],
        )],
        KeyContext::AutoSyncing => vec![KeySection::new(
            "Automatic sync",
            &[key(
                "—",
                "Nothing, until the measurement lands",
                "The decode is a background ffmpeg run of a few seconds and closes itself the moment it answers, whatever the answer was.",
            )],
        )],
        KeyContext::Files { searching: true } => vec![search_bar_section(), text_input_section()],
        KeyContext::Files { searching: false } => vec![
            KeySection::new(
                "File list",
                &[
                    MOVE_ROWS,
                    key(
                        "l / Enter",
                        "Open this file's tracks",
                        "Probes the file if it has not been read yet and moves to its track list.",
                    ),
                    SCROLL_PAGE,
                    MOVE_ENDS,
                    SEARCH,
                    SAVE,
                    key(
                        "q / Esc",
                        "Quit",
                        "There is nothing above the file list, so backing out of it leaves the application.",
                    ),
                ],
            ),
            KeySection::new(
                "Sidecar folds",
                &[
                    key(
                        "za / zo / zc",
                        "Toggle, open, or close this fold",
                        "A media file with subtitle files beside it draws them as a fold. These open and close one.",
                    ),
                    key(
                        "zM / zR",
                        "Close or open every fold",
                        "Collapses the whole list to one row per media file, or expands every sidecar at once.",
                    ),
                ],
            ),
            KeySection::new(
                "Staged work",
                &[
                    key(
                        "r",
                        "Unstage this file",
                        "Drops every staged edit for the file the cursor is on, after asking.",
                    ),
                    key(
                        "R",
                        "Unstage every file",
                        "Drops all staged work in the directory, after asking. There is no undo.",
                    ),
                ],
            ),
        ],
        KeyContext::Streams => vec![
            KeySection::new(
                "Track list",
                &[
                    MOVE_ROWS,
                    key(
                        "Enter",
                        "Edit this track's settings",
                        "Opens the container, video, audio or subtitle dialog, whichever the cursor is on.",
                    ),
                    key(
                        "i",
                        "Show this track's information",
                        "Opens the raw stream details ffprobe reported — codec, geometry, tags and the rest.",
                    ),
                    key(
                        "h",
                        "Move left, or back to the files",
                        "Where the subtitle columns are drawn side by side this hops between them; otherwise it leaves for the file list.",
                    ),
                    SCROLL_PAGE,
                    MOVE_ENDS,
                    CLOSE,
                ],
            ),
            KeySection::new(
                "Editing tracks",
                &[
                    key(
                        "Ctrl-j / Ctrl-k",
                        "Move this track down or up",
                        "Reorders the track within its own kind. The order is staged, not written, until the save is confirmed.",
                    ),
                    key(
                        "Ctrl-h / Ctrl-l",
                        "Mark for import or export",
                        "Marks a subtitle to be muxed into the file, or written out beside it as its own sidecar.",
                    ),
                    key(
                        "d",
                        "Mark this track for deletion",
                        "A toggle: the row stays listed and marked red until a save carries it out, so it can be taken back.",
                    ),
                    key(
                        "a",
                        "Add a subtitle track",
                        "Asks what to create, writes an empty subtitle file, and opens it for editing. Leaving without typing a line takes the file back off the disk.",
                    ),
                    key(
                        "c",
                        "Edit a subtitle track",
                        "Opens the subtitle edit page: the track's cues, their timing, and the frames they land on.",
                    ),
                    key(
                        "r",
                        "Reset this field",
                        "Puts the highlighted row back to what the file says.",
                    ),
                    key(
                        "R",
                        "Unstage this whole file",
                        "Drops every staged edit for the open file, after asking.",
                    ),
                    SAVE,
                ],
            ),
        ],
        KeyContext::StreamDetails => vec![KeySection::new(
            "Track information",
            &[
                MOVE_ROWS,
                SCROLL_PAGE,
                MOVE_ENDS,
                key(
                    "i",
                    "Close this popup",
                    "The same key that opened it closes it again.",
                ),
                CLOSE,
            ],
        )],
        KeyContext::SubtitleEdit {
            searching: true, ..
        } => vec![search_bar_section(), text_input_section()],
        KeyContext::SubtitleEdit {
            focus: EditFocus::Timeline,
            ..
        } => vec![
            KeySection::new(
                "Timeline cursor",
                &[
                    key(
                        "h / l",
                        "Move the cursor half a second",
                        "Walks the cursor through the track. The preview shows that moment with whatever a viewer would see there burned into it — which is how you find where a line actually belongs.",
                    ),
                    key(
                        "H / L",
                        "Move the cursor five seconds",
                        "The coarse step, for finding a shot rather than a frame.",
                    ),
                    key(
                        "Ctrl-h / Ctrl-l",
                        "Move the cursor a twentieth of a second",
                        "The fine step, exactly the size a cue is nudged by — so the cursor can never stand on a moment a nudged cue cannot reach.",
                    ),
                    key(
                        "Ctrl-k",
                        "Put the cursor back in the cue list",
                        "Hands the keys back to the cues. While the timeline holds the cursor no cue is marked anywhere, because the selection is parked in the other pane.",
                    ),
                ],
            ),
            KeySection::new(
                "Here",
                &[
                    key(
                        "i",
                        "Add a cue at the cursor",
                        "Opens the editor on a new cue two seconds long, starting where the cursor stands. Nothing is staged unless words are typed into it.",
                    ),
                    key(
                        "p",
                        "Play around the cursor",
                        "Plays a second of media centred on the moment, plus whatever padding the preview settings ask for.",
                    ),
                    SEARCH,
                    key(
                        ":",
                        "Preview settings",
                        "Speed, loop, sound, padding and frame rate for this session, and which video, audio and subtitle track the page is pointed at.",
                    ),
                    SAVE,
                    key(
                        "Esc / q",
                        "Stop, then leave the page",
                        "Peels a running playback first, then the page. The two panes are one page, so the cursor being here is not a level to back out of.",
                    ),
                ],
            ),
        ],
        KeyContext::SubtitleEdit {
            timing: TimingScope::Track,
            ..
        } => vec![
            KeySection::new(
                "Retiming the whole track",
                &[
                    key(
                        "h / l",
                        "Move every cue a twentieth of a second",
                        "Shifts the entire track, for a file that is out of sync from end to end. A backward shift stops at what the earliest cue can spare, so the opening lines never pile up at zero.",
                    ),
                    key(
                        "H / L",
                        "Move every cue half a second",
                        "The same shift, ten steps at a time.",
                    ),
                    key(
                        "r",
                        "Put the whole track back",
                        "Returns every cue to the timing the file gives it, keeping the words. This is the one key here that can discard a lot of work in a single press, and there is no undo.",
                    ),
                    key(
                        "T / Esc",
                        "Leave global retiming",
                        "Turns the mode off. The shift stays staged until it is saved or reset.",
                    ),
                ],
            ),
            subtitle_edit_page_section(),
        ],
        KeyContext::SubtitleEdit {
            timing: TimingScope::Cue(_),
            ..
        } => vec![
            KeySection::new(
                "Timing this cue",
                &[
                    key(
                        "h / l",
                        "Move the selected part a twentieth of a second",
                        "Moves whichever part of the cue is selected — the whole cue, or one of its ends. Both ends move together even at zero, so a nudge never quietly changes how long the line is up.",
                    ),
                    key(
                        "H / L",
                        "Move the selected part half a second",
                        "The same move, ten steps at a time.",
                    ),
                    key(
                        "Ctrl-h / Ctrl-l",
                        "Select the start, the whole cue, or the end",
                        "Steps the selection along start ← whole → end. With an end selected, h makes the line leave earlier — which is what changes how long it is on screen.",
                    ),
                    key(
                        "D",
                        "Type how long this cue is on screen",
                        "For a length stated outright rather than nudged: a sign that should be up for eight seconds is a value, not a gesture.",
                    ),
                    key(
                        "r",
                        "Put this cue back",
                        "Returns the cue to the timing the file gives it, whichever part is selected.",
                    ),
                    key(
                        "t / Esc",
                        "Leave the timing mode",
                        "Turns the mode off and gives h and l their other meaning back. What was nudged stays staged.",
                    ),
                ],
            ),
            subtitle_edit_page_section(),
        ],
        KeyContext::SubtitleEdit { .. } => vec![
            KeySection::new(
                "Cues",
                &[
                    MOVE_ROWS,
                    key(
                        "h / l",
                        "Move between cues sharing a moment",
                        "Where two lines are on screen together the panel draws them side by side; this moves between them and stops at the group's ends.",
                    ),
                    key(
                        "i",
                        "Edit this cue's text",
                        "Opens the cue editor. Esc keeps what was typed; it is staged, not written, until a save.",
                    ),
                    key(
                        "d",
                        "Mark this cue for deletion",
                        "A toggle: the row stays listed with a line through it, and the cue stays burned into the preview so it can still be judged before it goes.",
                    ),
                    SCROLL_PAGE,
                    MOVE_ENDS,
                ],
            ),
            KeySection::new(
                "Timing",
                &[
                    key(
                        "t",
                        "Time this cue",
                        "Turns on the cue-scale timing mode, where h and l move the selected cue rather than the cursor. Sticky, so a whole file can be retimed without turning it on again at every line.",
                    ),
                    key(
                        "T",
                        "Retime the whole track",
                        "Turns on the track-scale mode, where every cue moves together — for a file that is out of sync throughout rather than in one place.",
                    ),
                    key(
                        "A",
                        "Measure the offset automatically",
                        "Decodes the track's own audio, measures how far the subtitles are out, and stages the correction for checking before it is saved.",
                    ),
                ],
            ),
            subtitle_edit_page_section(),
        ],
    }
}

/// The keys the subtitle edit page answers wherever its cursor is and whatever mode is on.
fn subtitle_edit_page_section() -> KeySection {
    KeySection::new(
        "This page",
        &[
            key(
                "p",
                "Play this cue",
                "Plays the cue's span plus the padding the preview settings ask for, with the line burned in and the sound it is meant to match.",
            ),
            key(
                "Ctrl-j",
                "Put the cursor in the timeline",
                "Hands the keys to the timeline, where the cursor names a moment rather than a line — which is how you find where a cue belongs.",
            ),
            SEARCH,
            key(
                ":",
                "Preview settings",
                "Speed, loop, sound, padding and frame rate for this session, and which video, audio and subtitle track the page is pointed at.",
            ),
            SAVE,
            key(
                "Esc / q",
                "Leave the page",
                "Peels a running playback, then a filter, then a timing mode, then the page itself. Unsaved cue work is asked about before it goes.",
            ),
        ],
    )
}

/// The four settings dialogs share one shape, differing only in which rows they carry.
fn settings_sections(context: KeyContext, mode: SettingsKeyMode) -> Vec<KeySection> {
    let what = match context {
        KeyContext::AudioSettings { .. } => "Audio track settings",
        KeyContext::VideoSettings { .. } => "Video track settings",
        _ => "Subtitle track settings",
    };
    match mode {
        SettingsKeyMode::Editing => vec![
            KeySection::new(
                "Editing a field",
                &[
                    key(
                        "Esc / Enter",
                        "Finish editing",
                        "Both accept what was typed and hand the keys back to the rows.",
                    ),
                    SAVE,
                ],
            ),
            text_input_section(),
        ],
        SettingsKeyMode::LanguageSearch => vec![search_bar_section(), text_input_section()],
        SettingsKeyMode::LanguageList => vec![KeySection::new(
            "Choosing a language",
            &[
                MOVE_ROWS,
                key(
                    "Enter",
                    "Choose this language",
                    "Takes the highlighted entry and closes the list.",
                ),
                SEARCH,
                MOVE_ENDS,
                HELP_FIELD,
                SAVE,
                key(
                    "Esc / q",
                    "Close the list",
                    "Backs out one level, to the dialog's rows.",
                ),
            ],
        )],
        SettingsKeyMode::CustomResolution => vec![KeySection::new(
            "Custom resolution",
            &[
                MOVE_ROWS,
                key(
                    "i",
                    "Edit this number",
                    "Starts typing into the highlighted width or height.",
                ),
                CONFIRM,
                MOVE_ENDS,
                SAVE,
                key(
                    "Esc / q",
                    "Back out",
                    "Returns to the resolution choices without taking the typed size.",
                ),
            ],
        )],
        SettingsKeyMode::Browsing => {
            let mut bindings = vec![
                MOVE_ROWS,
                key(
                    "Enter",
                    "Open this row's choices",
                    "Opens the dropdown for the highlighted row, or takes a value from one already open.",
                ),
                key(
                    "i",
                    "Edit this text field",
                    "Starts typing into the highlighted field, such as the track's title.",
                ),
                key(
                    "r",
                    "Reset this field",
                    "Puts the highlighted row back to what the file itself says, dropping the staged change to it.",
                ),
                MOVE_ENDS,
            ];
            bindings.extend(settings_common());
            vec![KeySection::new(what, &bindings)]
        }
    }
}

/// Every binding in the application, which is what `?` from inside the popup switches to.
///
/// Kept as a flat list of the same four sections it has always had: the reader who asks for
/// this one is asking "what else is there", and grouping it by context would answer a
/// question they did not ask.
pub fn global_sections() -> Vec<KeySection> {
    vec![
        KeySection::new(
            "General",
            &[
                key(
                    "?",
                    "Open or close keybindings",
                    "Opens the keys the current page answers. From inside, ? switches to this list and back.",
                ),
                SEARCH,
                CLOSE,
                MOVE_ROWS,
                key(
                    "h / l",
                    "Change a horizontal choice",
                    "Moves between buttons, columns, or the cues that share a moment — whichever the page in front of you has.",
                ),
                MOVE_ENDS,
                SCROLL_PAGE,
                key(
                    "Ctrl-n / Ctrl-p",
                    "Move through search results",
                    "Walks the matches without leaving a search bar.",
                ),
                key(
                    "za / zo / zc",
                    "Toggle, open, or close a sidecar fold",
                    "On the file list, where subtitle files beside a media file are drawn as a fold.",
                ),
                key(
                    "zM / zR",
                    "Close or open every sidecar fold",
                    "Collapses the file list to one row per media file, or expands them all.",
                ),
                CONFIRM,
            ],
        ),
        KeySection::new(
            "Track editing",
            &[
                key(
                    "Ctrl-j / Ctrl-k",
                    "Move a track down or up within its type",
                    "Reorders a track. Staged, not written, until a save is confirmed.",
                ),
                key(
                    "Ctrl-h / Ctrl-l",
                    "Mark a subtitle for import or export",
                    "Marks it to be muxed into the file, or written out beside it as a sidecar.",
                ),
                key(
                    "Enter",
                    "Edit container, video, audio, or subtitle settings",
                    "Opens whichever dialog the highlighted row belongs to.",
                ),
                HELP_FIELD,
                key(
                    "i",
                    "Toggle container or stream information",
                    "Shows the raw details ffprobe reported for the highlighted track.",
                ),
                key(
                    "d",
                    "Mark or unmark a track for deletion",
                    "A toggle: the row stays listed and marked until a save carries it out.",
                ),
                key(
                    "a",
                    "Add a subtitle track to this file",
                    "Asks what to create, writes an empty subtitle file, and opens it for editing.",
                ),
                key(
                    "c",
                    "Edit a subtitle track",
                    "Opens the subtitle edit page: its text, its timing, and the frames they land on.",
                ),
                SAVE,
            ],
        ),
        KeySection::new(
            "Subtitle editing",
            &[
                key(
                    "/",
                    "Filter the cue list",
                    "Narrows the list to the lines whose words match, with the match highlighted. Esc drops the filter.",
                ),
                key(
                    "p",
                    "Play around the cue or the cursor",
                    "Plays a few seconds with the line burned in and the sound it should match.",
                ),
                key(
                    ":",
                    "Preview settings",
                    "Speed, loop, sound, padding and frame rate for this session, and which tracks the page previews against.",
                ),
                key(
                    "t",
                    "Time the selected cue",
                    "Turns on the cue-scale timing mode. SubRip tracks only.",
                ),
                key(
                    "T",
                    "Retime the whole track",
                    "Moves every cue together, for a track out of sync throughout. SubRip tracks only.",
                ),
                key(
                    "A",
                    "Measure the offset automatically",
                    "Measures the track against its own audio and stages the correction. SubRip tracks only.",
                ),
                key(
                    "Ctrl-j / Ctrl-k",
                    "Move the cursor to the timeline or back",
                    "The timeline's cursor names a moment rather than a line.",
                ),
                key(
                    "h / l",
                    "Move the cursor, a cue, or between cues",
                    "What this moves depends on which pane holds the cursor and whether a timing mode is on — press ? on the page itself to see the one that applies.",
                ),
                key(
                    "H / L",
                    "The same move, ten steps at a time",
                    "The leap, wherever h and l have a meaning.",
                ),
                key(
                    "Ctrl-h / Ctrl-l",
                    "Move the cursor finely, or select part of a cue",
                    "In the timeline, the fine step; in the cue list, which part of the cue the timing mode moves.",
                ),
                key(
                    "D",
                    "Type how long a cue is on screen",
                    "As mm:ss.mmm, while timing a cue.",
                ),
                key(
                    "r",
                    "Put a cue, or the track, back to the file's timing",
                    "Whichever scale the timing mode is at.",
                ),
                key(
                    "i",
                    "Edit a cue, or add one at the cursor",
                    "The cue editor. Esc keeps the edit; a save writes it. SubRip tracks only.",
                ),
                key(
                    "d",
                    "Mark or unmark a cue for deletion",
                    "A toggle, like marking a track. SubRip tracks only.",
                ),
                key(
                    "R",
                    "Reset every preview setting",
                    "In the preview settings dialog.",
                ),
            ],
        ),
        text_input_section(),
    ]
}

/// Narrows `sections` to the rows matching `query`, keeping each hit's heading.
///
/// The old filter flattened rendered lines back into strings and re-detected entries by
/// their leading whitespace, which meant a filtered view dropped every non-matching
/// heading — so the reader lost all context for the hits they were looking at. Filtering
/// the structure keeps the heading of any section that still has a row in it.
///
/// Matched on the keys *and* the description, trimmed and case-insensitively, which is the
/// rule the file list and the cue list already follow.
pub fn filter(sections: Vec<KeySection>, query: &str) -> Vec<KeySection> {
    let clean = query.trim().to_lowercase();
    if clean.is_empty() {
        return sections;
    }
    sections
        .into_iter()
        .filter_map(|section| {
            let bindings: Vec<Keybinding> = section
                .bindings
                .into_iter()
                .filter(|binding| {
                    binding.keys.to_lowercase().contains(&clean)
                        || binding.description.to_lowercase().contains(&clean)
                })
                .collect();
            (!bindings.is_empty()).then_some(KeySection {
                name: section.name,
                bindings,
            })
        })
        .collect()
}

/// How many rows are drawn, which is what the search bar counts and what the cursor is
/// clamped against. Headings are not rows.
pub fn count(sections: &[KeySection]) -> usize {
    sections.iter().map(|section| section.bindings.len()).sum()
}

/// The `index`-th row across every section, for the explanation panel.
pub fn nth(sections: &[KeySection], index: usize) -> Option<Keybinding> {
    sections
        .iter()
        .flat_map(|section| section.bindings.iter())
        .nth(index)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle_edit::CueGrip;

    /// A context that ships with nothing in it is a popup that opens on a blank page, which
    /// is worse than the flat list it replaced.
    #[test]
    fn every_context_should_offer_at_least_one_binding() {
        for context in KeyContext::ALL {
            let sections = sections(*context);
            assert!(!sections.is_empty(), "{context:?} has no sections at all",);
            let bindings: usize = sections.iter().map(|s| s.bindings.len()).sum();
            assert!(bindings > 0, "{context:?} lists no bindings");
            for section in &sections {
                assert!(
                    !section.name.is_empty(),
                    "{context:?} has a nameless section"
                );
            }
        }
    }

    /// `ALL` is hand-written, so this is what catches a variant added without being added
    /// to it: the match is exhaustive, and a new variant fails to compile here.
    #[test]
    fn every_context_should_be_listed_in_all() {
        for context in KeyContext::ALL {
            // Exhaustive by construction: a new variant makes this match fail to compile,
            // and the fix is to add it here *and* to `ALL`.
            let named = match context {
                KeyContext::EditCue
                | KeyContext::CueLength
                | KeyContext::ConfirmLeaveCues
                | KeyContext::CreateTrack { .. }
                | KeyContext::ConfirmCancel
                | KeyContext::ContainerSettings { .. }
                | KeyContext::PreviewSettings
                | KeyContext::AudioSettings { .. }
                | KeyContext::VideoSettings { .. }
                | KeyContext::SubtitleSettings { .. }
                | KeyContext::Keybindings { .. }
                | KeyContext::Error
                | KeyContext::ConfirmProcessAll
                | KeyContext::BatchProcessing
                | KeyContext::ConfirmReset
                | KeyContext::ResolveConflicts
                | KeyContext::AutoSyncing
                | KeyContext::Files { .. }
                | KeyContext::Streams
                | KeyContext::StreamDetails
                | KeyContext::SubtitleEdit { .. } => context.title(),
            };
            assert!(!named.is_empty());
        }
    }

    /// The point of the whole module: a key that does nothing here is not listed here.
    #[test]
    fn the_timeline_should_not_list_the_vertical_movement_keys_it_swallows() {
        let timeline = sections(KeyContext::SubtitleEdit {
            searching: false,
            focus: EditFocus::Timeline,
            timing: TimingScope::Off,
        });
        let listed = keys_of(&timeline);

        assert!(!listed.iter().any(|keys| keys.contains("j / k")));
        assert!(!listed.iter().any(|keys| keys.contains("gg / G")));
        assert!(listed.iter().any(|keys| keys.contains("Ctrl-k")));
    }

    /// The file list's fold chords exist nowhere else, which is exactly the kind of entry
    /// the flat list made every other page carry.
    #[test]
    fn only_the_file_list_should_list_the_sidecar_fold_chords() {
        let files = keys_of(&sections(KeyContext::Files { searching: false }));
        let streams = keys_of(&sections(KeyContext::Streams));

        assert!(files.iter().any(|keys| keys.contains("za")));
        assert!(!streams.iter().any(|keys| keys.contains("za")));
    }

    /// One key, one meaning: the three timing scopes each describe `h`/`l` differently, and
    /// none of them is the run-on sentence the flat list had to carry.
    #[test]
    fn each_timing_scope_should_give_h_and_l_its_own_description() {
        let describe = |timing| {
            sections(KeyContext::SubtitleEdit {
                searching: false,
                focus: EditFocus::Cues,
                timing,
            })
            .into_iter()
            .flat_map(|section| section.bindings)
            .find(|binding| binding.keys == "h / l")
            .map(|binding| binding.description)
            .expect("h / l should be listed at every cue-pane scope")
        };

        let off = describe(TimingScope::Off);
        let cue = describe(TimingScope::Cue(CueGrip::Whole));
        let track = describe(TimingScope::Track);

        assert_ne!(off, cue);
        assert_ne!(cue, track);
        assert_ne!(off, track);
        for description in [off, cue, track] {
            assert!(
                !description.contains("otherwise"),
                "a contextual description should not need to name another mode: {description}"
            );
        }
    }

    /// Whatever else changes, the global list stays the whole application's — that is what
    /// the reader switched to it for.
    #[test]
    fn the_global_list_should_keep_its_four_sections() {
        let names: Vec<_> = global_sections()
            .iter()
            .map(|section| section.name)
            .collect();

        assert_eq!(
            names,
            vec!["General", "Track editing", "Subtitle editing", "Text"]
        );
    }

    /// Every explanation is drawn in a panel that does not scroll, so an empty one is a
    /// blank box and a missing one is a feature that looks broken.
    #[test]
    fn every_binding_should_carry_a_description_and_an_explanation() {
        let all = KeyContext::ALL
            .iter()
            .flat_map(|context| sections(*context))
            .chain(global_sections());

        for section in all {
            for binding in section.bindings {
                assert!(!binding.keys.is_empty(), "a binding has no keys");
                assert!(
                    !binding.description.is_empty(),
                    "{} has no description",
                    binding.keys
                );
                assert!(
                    !binding.explanation.is_empty(),
                    "{} has no explanation",
                    binding.keys
                );
            }
        }
    }

    /// A query is matched against the keys as well as the description.
    ///
    /// Every other filter test here matches on description text, which left the first half
    /// of that `||` never taken: a reader searching for the key they half-remember would
    /// have got nothing back, and nothing in this module would have noticed.
    #[test]
    fn filtering_should_match_on_the_keys_as_well_as_the_description() {
        // Arrange: a key whose own description does not contain the query, so only the
        // keys half of the condition can produce this hit.
        let query = "Ctrl-w";

        // Act
        let sections = filter(global_sections(), query);
        let found: Vec<&str> = sections
            .iter()
            .flat_map(|section| section.bindings.iter().map(|binding| binding.keys))
            .collect();

        // Assert
        assert_eq!(found, vec!["Ctrl-w"]);
        assert_eq!(count(&sections), 1);
        assert!(
            !global_sections()
                .iter()
                .flat_map(|section| section.bindings.iter())
                .any(|binding| binding.description.to_lowercase().contains("ctrl-w")),
            "the query must not match any description, or this would not exercise the \
             keys half of the match"
        );
    }

    fn keys_of(sections: &[KeySection]) -> Vec<&'static str> {
        sections
            .iter()
            .flat_map(|section| section.bindings.iter().map(|binding| binding.keys))
            .collect()
    }
}
