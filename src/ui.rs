use std::{collections::BTreeMap, path::Path};

use isolang::Language;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Gauge, List, ListItem, Paragraph, Wrap},
};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{
        App, CancelEditChoice, CharClass, ConfirmProcessAllChoice, ContainerChoice,
        ContainerSettingsField, ContainerSettingsMode, ContainerSettingsPopup,
        CustomResolutionField, Dialog, InputReject, Layer, ResetChoice, SearchState,
        StagedFileStatus, SubtitleDisplayState, SubtitleSettingsField, SubtitleSettingsMode,
        SubtitleSettingsPopup, TextInputConfig, TextInputSite, TextInputState, TrackRef,
        VideoSettingsField, VideoSettingsMode, describe_track_groups,
    },
    edit::{ContainerFormat, stream_index},
    probe::{MediaInfo, ProbeOutcome},
    staging::BatchItemStatus,
    subtitle::{
        SidecarEntry, SubtitleChange, SubtitleFlag, SubtitleFormat, SubtitleSource,
        canonical_language_code, language_choice, stream_cc, stream_commentary, stream_forced,
        stream_hearing_impaired, stream_language, stream_original, stream_title,
    },
};

const SUBTITLE_COLUMN_GUTTER: u16 = 2;

/// Decides when the event loop repaints. The loop deliberately does *not* redraw
/// unconditionally at some frame rate — it draws when app state changed, or while the
/// UI is animating itself off wall-clock time (`App::is_animating`).
///
/// The subtlety this type exists to hold: when an animation stops, the frame showing
/// its finished state is by definition the first frame where `animating` is false, so
/// keying purely off `dirty || animating` never paints it. The conflict notice used to
/// freeze on "Understood (1)", dim, until some unrelated event forced a repaint —
/// while the button had in fact already armed. One trailing frame after `animating`
/// goes false fixes that, and the same applies to a batch gauge's final position.
///
/// Lives here rather than inline in `main` so the rule is testable; `main` owns only
/// the wiring.
#[derive(Debug, Default)]
pub struct RedrawState {
    was_animating: bool,
}

impl RedrawState {
    /// Whether to draw this tick. `dirty` is "app state changed since the last draw",
    /// `animating` is `App::is_animating`.
    pub fn tick(&mut self, dirty: bool, animating: bool) -> bool {
        let draw = dirty || animating || self.was_animating;
        self.was_animating = animating;
        draw
    }
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 50 || area.height < 10 {
        app.set_subtitle_columns_side_by_side(false);
        frame.render_widget(
            Paragraph::new("Terminal too small\nResize to at least 50×10")
                .centered()
                .block(Block::bordered().title(" reel-tui ")),
            area,
        );
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(rows[0]);

    render_files(frame, app, columns[0]);
    render_details(frame, app, columns[1]);
    render_footer(frame, app, rows[1]);
    if app.layer == Layer::StreamDetails {
        dim_backdrop(frame);
        render_details_popup(frame, app);
    }
    if let Some(dialog) = app.dialog {
        dim_backdrop(frame);
        render_dialog(frame, app, dialog);
    }
}

fn dim_backdrop(frame: &mut Frame) {
    let dim_block = Block::default().style(Style::default().fg(Color::DarkGray));
    frame.render_widget(dim_block, frame.area());
}

fn render_files(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.layer == Layer::Files;
    let entries = app.file_panel_entries();
    let filtering = app.file_search_has_query();
    let has_any_sidecars = entries
        .iter()
        .any(|entry| !entry.sidecar_indices.is_empty());

    let items: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let file = app.files.get(entry.file_index)?;
            let sidecars = app.sidecars_for_media(&file.path);
            Some(ListItem::new(file_tree_lines(
                &file.display_name,
                entry
                    .sidecar_indices
                    .iter()
                    .filter_map(|index| sidecars.get(*index))
                    .map(|sidecar| sidecar.display_name.as_str()),
                !filtering && app.is_file_folded(&file.path),
                has_any_sidecars,
                app.staged_file_status(&file.path),
            )))
        })
        .collect();
    app.file_search.match_count = entries.len();
    // Network mode is reported once, in the footer.
    let title = format!(" Files ({}) ", app.files.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border(app.layer == Layer::Files))
        .title(title);
    let inner = block.inner(area);
    let list = List::new(items)
        .highlight_style(if focused {
            focused_style(false)
        } else {
            Style::default().fg(Color::White).bold()
        })
        .highlight_symbol(if focused { "› " } else { "  " });
    frame.render_widget(block, area);

    if app.file_search.is_active || !app.file_search.value.is_empty() {
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
        frame.render_stateful_widget(list, chunks[0], &mut app.list_state);
        let reject = app.text_input_reject(TextInputSite::FileSearch);
        let search = search_line(&mut app.file_search, chunks[1], reject);
        frame.render_widget(Paragraph::new(search), chunks[1]);
    } else {
        frame.render_stateful_widget(list, inner, &mut app.list_state);
    }
}

/// `status` drives the same yellow/italic-for-changed and warning-triangle-for-invalid
/// visual language used for track rows in the details panel (`changed_style()`,
/// `warning_style()`), applied here to a whole file's row so staged (and possibly
/// stale/conflicting) files are distinguishable at a glance in the file list.
fn file_tree_lines<'a>(
    display_name: &str,
    sidecar_names: impl IntoIterator<Item = &'a str>,
    folded: bool,
    has_any_sidecars: bool,
    status: StagedFileStatus,
) -> Vec<Line<'static>> {
    let (name_style, marker) = match status {
        StagedFileStatus::Unstaged => (None, ""),
        StagedFileStatus::Valid => (Some(changed_style()), ""),
        StagedFileStatus::Invalid(_) => (Some(warning_style(true)), "⚠ "),
    };
    let styled_name = |text: String| match name_style {
        Some(style) => Line::styled(text, style),
        None => Line::from(text),
    };

    let sidecar_names = sidecar_names.into_iter().collect::<Vec<_>>();
    if sidecar_names.is_empty() {
        let prefix = if has_any_sidecars { "  " } else { "" };
        return vec![styled_name(format!("{prefix}{marker}{display_name}"))];
    }
    let prefix = if folded { "▹ " } else { "▿ " };
    let first_line = styled_name(format!("{prefix}{marker}{display_name}"));
    if folded {
        return vec![first_line];
    }
    let mut lines = Vec::with_capacity(sidecar_names.len() + 1);
    lines.push(first_line);
    let last = sidecar_names.len().saturating_sub(1);
    lines.extend(sidecar_names.into_iter().enumerate().map(|(index, name)| {
        Line::from(vec![
            Span::styled(
                if index == last {
                    "  └── "
                } else {
                    "  ├── "
                },
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(name.to_string(), Style::default().fg(Color::DarkGray)),
        ])
    }));
    lines
}

fn render_details(frame: &mut Frame, app: &mut App, area: Rect) {
    let filename = app
        .selected_file()
        .map(|file| file.display_name.as_str())
        .unwrap_or("Details");
    let title = format!(" {filename} ");
    let embedded_subtitles = app.media_info().map_or(0, |info| {
        info.streams
            .iter()
            .filter(|stream| string(stream, "codec_type") == Some("subtitle"))
            .count()
    });
    let subtitle_content_width = area.width.saturating_sub(2);
    let subtitle_columns_side_by_side = subtitle_columns_fit(
        subtitle_content_width,
        embedded_subtitles,
        app.sidecars.len(),
    );
    app.set_subtitle_columns_side_by_side(subtitle_columns_side_by_side);

    let text = if let Some(error) = &app.scan_error {
        message("Could not read directory", error, Color::Red)
    } else if app.files.is_empty() {
        Text::from("No regular files in this directory.")
    } else if app.loading {
        Text::from(vec![
            Line::styled("Loading metadata…", Style::default().fg(Color::Yellow)),
            Line::from(""),
            Line::from("You can continue navigating while ffprobe runs."),
        ])
    } else {
        match &app.outcome {
            Some(ProbeOutcome::Video(info)) => {
                let changed = app.changed_streams();
                let rows = app.track_rows();
                let container_conflicts = app.selected_container_conflicts();
                let conflicting_streams = app.selected_container_conflict_streams();
                let (text, selected_line) = media_text(
                    info,
                    details_selected_stream(app),
                    MediaTextState {
                        order: &app.stream_order,
                        rows: &rows,
                        sidecars: &app.sidecars,
                        deleted: &app.deleted_streams,
                        defaults: &app.default_streams,
                        default_sidecars: &app.default_sidecars,
                        changed: &changed,
                        subtitle_changes: &app.subtitle_changes,
                        source_container: app.source_container(),
                        container_target: app.container_target,
                        container_metadata_changed: app.container_metadata_changed(),
                        container_conflicts: container_conflicts.len(),
                        conflicting_streams: &conflicting_streams,
                        subtitle_columns_side_by_side,
                        subtitle_column_width: usize::from(
                            subtitle_content_width.saturating_sub(SUBTITLE_COLUMN_GUTTER) / 2,
                        ),
                    },
                );
                if app.layer == Layer::Streams
                    && let Some(selected_line) = selected_line
                {
                    app.details_scroll =
                        scroll_to_show_line(&text, area, selected_line, app.details_scroll);
                }
                text
            }
            Some(ProbeOutcome::NotVideo(reason)) => {
                message("Unsupported format", reason, Color::Yellow)
            }
            Some(ProbeOutcome::Error(error)) => message("Probe error", error, Color::Red),
            None => Text::from("Select a file to inspect it."),
        }
    };

    app.set_details_max_scroll(max_scroll(&text, area));
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(focus_border(app.layer == Layer::Streams))
                    .title(title),
            )
            .wrap(Wrap { trim: false })
            .scroll((app.details_scroll, 0)),
        area,
    );
}

fn details_selected_stream(app: &App) -> Option<usize> {
    (app.layer != Layer::Files && app.dialog.is_none()).then_some(app.selected_stream)
}

fn subtitle_columns_fit(_content_width: u16, embedded: usize, external: usize) -> bool {
    embedded > 0 || external > 0
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(notice) = &app.notice {
        frame.render_widget(
            Paragraph::new(Line::styled(
                truncate(notice, area.width as usize),
                Style::default().fg(Color::Yellow),
            )),
            area,
        );
        return;
    }
    let directory = app.directory.to_string_lossy();
    let net_tag = if app.is_network_mount {
        " [Network Mode]"
    } else {
        ""
    };
    let hint = " ? keybinds ";
    let available = area.width as usize;
    let directory_width = available.saturating_sub(hint.len() + net_tag.len());
    let directory = truncate(&directory, directory_width);
    let padding = " ".repeat(
        available
            .saturating_sub(directory.chars().count())
            .saturating_sub(hint.len())
            .saturating_sub(net_tag.len()),
    );
    let mut spans = vec![Span::styled(
        directory,
        Style::default().fg(Color::DarkGray),
    )];
    if app.is_network_mount {
        spans.push(Span::styled(net_tag, Style::default().fg(Color::Yellow)));
    }
    spans.push(Span::raw(padding));
    spans.push(Span::styled(hint, Style::default().fg(Color::Cyan)));
    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
}

fn message(heading: &str, detail: &str, color: Color) -> Text<'static> {
    Text::from(vec![
        Line::styled(heading.to_string(), Style::default().fg(color).bold()),
        Line::from(""),
        Line::from(detail.to_string()),
    ])
}

fn media_text(
    info: &MediaInfo,
    selected: Option<usize>,
    state: MediaTextState<'_>,
) -> (Text<'static>, Option<usize>) {
    let MediaTextState {
        order,
        rows,
        sidecars,
        deleted,
        defaults,
        default_sidecars,
        changed,
        subtitle_changes,
        source_container,
        container_target,
        container_metadata_changed,
        container_conflicts,
        conflicting_streams,
        subtitle_columns_side_by_side,
        subtitle_column_width,
    } = state;
    let mut lines = Vec::new();
    let mut selected_line = None;
    let effective_container = container_target.or(source_container);
    section(&mut lines, "Container");
    let container_index = rows
        .iter()
        .position(|row| *row == TrackRef::Container)
        .unwrap_or(0);
    if selected == Some(container_index) {
        selected_line = Some(lines.len());
    }
    lines.push(container_line(
        info,
        source_container,
        container_target,
        container_metadata_changed,
        container_conflicts,
        selected == Some(container_index),
    ));

    for (heading, kind) in [("Video", "video"), ("Audio", "audio")] {
        let streams: Vec<_> = order
            .iter()
            .filter_map(|index| {
                let stream = info
                    .streams
                    .iter()
                    .find(|stream| stream_index(stream) == Some(*index))?;
                let selection_index = rows
                    .iter()
                    .position(|row| *row == TrackRef::Embedded(*index))?;
                (string(stream, "codec_type") == Some(kind)).then_some((selection_index, stream))
            })
            .collect();
        if !streams.is_empty() {
            section(&mut lines, &format!("{heading} ({})", streams.len()));
            for (selection_index, stream) in streams {
                if selected == Some(selection_index) {
                    selected_line = Some(lines.len());
                }
                lines.push(stream_line(
                    stream,
                    selection_index,
                    selected == Some(selection_index),
                    stream_index(stream).is_some_and(|index| deleted.contains(&index)),
                    stream_index(stream).is_some_and(|index| changed.contains(&index)),
                    stream_index(stream).is_some_and(|index| conflicting_streams.contains(&index)),
                    stream_index(stream).is_some_and(|index| defaults.contains(&index)),
                ));
            }
        }
    }

    enum SubtitleRowItem<'a> {
        Stream(usize, &'a std::collections::BTreeMap<String, Value>),
        Sidecar(usize, usize, &'a SidecarEntry),
    }

    let is_exported = |index: u64| {
        subtitle_changes
            .get(&SubtitleSource::Embedded(index))
            .is_some_and(|c| c.export_target.is_some())
    };
    let is_imported = |sidecar: &SidecarEntry| {
        subtitle_changes
            .get(&SubtitleSource::Sidecar(sidecar.path.clone()))
            .is_some_and(|c| c.import_into_media)
    };

    let mut left_subtitles = Vec::new();
    let mut right_subtitles = Vec::new();

    for (selection_index, row) in rows.iter().enumerate() {
        match row {
            TrackRef::Embedded(index) => {
                if let Some(stream) = info
                    .streams
                    .iter()
                    .find(|stream| stream_index(stream) == Some(*index))
                    && string(stream, "codec_type") == Some("subtitle")
                {
                    if is_exported(*index) {
                        right_subtitles.push(SubtitleRowItem::Stream(selection_index, stream));
                    } else {
                        left_subtitles.push(SubtitleRowItem::Stream(selection_index, stream));
                    }
                }
            }
            TrackRef::Sidecar(sidecar_index) => {
                if let Some(sidecar) = sidecars.get(*sidecar_index) {
                    if is_imported(sidecar) {
                        left_subtitles.push(SubtitleRowItem::Sidecar(
                            selection_index,
                            *sidecar_index,
                            sidecar,
                        ));
                    } else {
                        right_subtitles.push(SubtitleRowItem::Sidecar(
                            selection_index,
                            *sidecar_index,
                            sidecar,
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    if subtitle_columns_side_by_side {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(subtitle_columns_line(
            Line::styled(
                format!("Embedded subtitles ({})", left_subtitles.len()),
                Style::default().fg(Color::Cyan).bold(),
            ),
            Line::styled(
                format!("External subtitles ({})", right_subtitles.len()),
                Style::default().fg(Color::Cyan).bold(),
            ),
            subtitle_column_width,
        ));
        let count = left_subtitles.len().max(right_subtitles.len());
        for row in 0..count {
            let left = left_subtitles.get(row).map(|item| match item {
                SubtitleRowItem::Stream(selection_index, stream) => {
                    if selected == Some(*selection_index) {
                        selected_line = Some(lines.len());
                    }
                    let change = stream_index(stream)
                        .and_then(|index| subtitle_changes.get(&SubtitleSource::Embedded(index)));
                    stream_line_with_subtitle_context(
                        stream,
                        *selection_index,
                        selected == Some(*selection_index),
                        stream_index(stream).is_some_and(|index| deleted.contains(&index)),
                        stream_index(stream).is_some_and(|index| changed.contains(&index)),
                        stream_index(stream)
                            .is_some_and(|index| conflicting_streams.contains(&index)),
                        stream_index(stream).is_some_and(|index| defaults.contains(&index)),
                        change,
                        effective_container,
                        Some(subtitle_column_width),
                    )
                }
                SubtitleRowItem::Sidecar(selection_index, sidecar_index, sidecar) => {
                    if selected == Some(*selection_index) {
                        selected_line = Some(lines.len());
                    }
                    let change =
                        subtitle_changes.get(&SubtitleSource::Sidecar(sidecar.path.clone()));
                    sidecar_line_with_subtitle_context(
                        sidecar,
                        selected == Some(*selection_index),
                        change.is_some(),
                        default_sidecars.contains(sidecar_index),
                        change,
                        effective_container,
                        Some(subtitle_column_width),
                    )
                }
            });
            let right = right_subtitles.get(row).map(|item| match item {
                SubtitleRowItem::Stream(selection_index, stream) => {
                    if selected == Some(*selection_index) {
                        selected_line = Some(lines.len());
                    }
                    let change = stream_index(stream)
                        .and_then(|index| subtitle_changes.get(&SubtitleSource::Embedded(index)));
                    stream_line_with_subtitle_context(
                        stream,
                        *selection_index,
                        selected == Some(*selection_index),
                        stream_index(stream).is_some_and(|index| deleted.contains(&index)),
                        stream_index(stream).is_some_and(|index| changed.contains(&index)),
                        stream_index(stream)
                            .is_some_and(|index| conflicting_streams.contains(&index)),
                        stream_index(stream).is_some_and(|index| defaults.contains(&index)),
                        change,
                        effective_container,
                        Some(subtitle_column_width),
                    )
                }
                SubtitleRowItem::Sidecar(selection_index, sidecar_index, sidecar) => {
                    if selected == Some(*selection_index) {
                        selected_line = Some(lines.len());
                    }
                    let change =
                        subtitle_changes.get(&SubtitleSource::Sidecar(sidecar.path.clone()));
                    sidecar_line_with_subtitle_context(
                        sidecar,
                        selected == Some(*selection_index),
                        change.is_some(),
                        default_sidecars.contains(sidecar_index),
                        change,
                        effective_container,
                        Some(subtitle_column_width),
                    )
                }
            });
            lines.push(subtitle_optional_columns_line(
                left,
                right,
                subtitle_column_width,
            ));
        }
    } else {
        if !left_subtitles.is_empty() {
            section(
                &mut lines,
                &format!("Embedded subtitles ({})", left_subtitles.len()),
            );
            for item in &left_subtitles {
                match item {
                    SubtitleRowItem::Stream(selection_index, stream) => {
                        if selected == Some(*selection_index) {
                            selected_line = Some(lines.len());
                        }
                        let change = stream_index(stream).and_then(|index| {
                            subtitle_changes.get(&SubtitleSource::Embedded(index))
                        });
                        lines.push(stream_line_with_subtitle_context(
                            stream,
                            *selection_index,
                            selected == Some(*selection_index),
                            stream_index(stream).is_some_and(|index| deleted.contains(&index)),
                            stream_index(stream).is_some_and(|index| changed.contains(&index)),
                            stream_index(stream)
                                .is_some_and(|index| conflicting_streams.contains(&index)),
                            stream_index(stream).is_some_and(|index| defaults.contains(&index)),
                            change,
                            effective_container,
                            None,
                        ));
                    }
                    SubtitleRowItem::Sidecar(selection_index, sidecar_index, sidecar) => {
                        if selected == Some(*selection_index) {
                            selected_line = Some(lines.len());
                        }
                        let change =
                            subtitle_changes.get(&SubtitleSource::Sidecar(sidecar.path.clone()));
                        lines.push(sidecar_line_with_subtitle_context(
                            sidecar,
                            selected == Some(*selection_index),
                            change.is_some(),
                            default_sidecars.contains(sidecar_index),
                            change,
                            effective_container,
                            None,
                        ));
                    }
                }
            }
        }
        if !right_subtitles.is_empty() {
            section(
                &mut lines,
                &format!("External subtitles ({})", right_subtitles.len()),
            );
            for item in &right_subtitles {
                match item {
                    SubtitleRowItem::Stream(selection_index, stream) => {
                        if selected == Some(*selection_index) {
                            selected_line = Some(lines.len());
                        }
                        let change = stream_index(stream).and_then(|index| {
                            subtitle_changes.get(&SubtitleSource::Embedded(index))
                        });
                        lines.push(stream_line_with_subtitle_context(
                            stream,
                            *selection_index,
                            selected == Some(*selection_index),
                            stream_index(stream).is_some_and(|index| deleted.contains(&index)),
                            stream_index(stream).is_some_and(|index| changed.contains(&index)),
                            stream_index(stream)
                                .is_some_and(|index| conflicting_streams.contains(&index)),
                            stream_index(stream).is_some_and(|index| defaults.contains(&index)),
                            change,
                            effective_container,
                            None,
                        ));
                    }
                    SubtitleRowItem::Sidecar(selection_index, sidecar_index, sidecar) => {
                        if selected == Some(*selection_index) {
                            selected_line = Some(lines.len());
                        }
                        let change =
                            subtitle_changes.get(&SubtitleSource::Sidecar(sidecar.path.clone()));
                        lines.push(sidecar_line_with_subtitle_context(
                            sidecar,
                            selected == Some(*selection_index),
                            change.is_some(),
                            default_sidecars.contains(sidecar_index),
                            change,
                            effective_container,
                            None,
                        ));
                    }
                }
            }
        }
    }

    // Only the container, video, audio and subtitle sections are listed. Attachments
    // and data streams have no editor, and the chapter section said only that chapter
    // support was still to come.

    (Text::from(lines), selected_line)
}

struct MediaTextState<'a> {
    order: &'a [u64],
    rows: &'a [TrackRef],
    sidecars: &'a [SidecarEntry],
    deleted: &'a std::collections::BTreeSet<u64>,
    defaults: &'a std::collections::BTreeSet<u64>,
    default_sidecars: &'a std::collections::BTreeSet<usize>,
    changed: &'a std::collections::BTreeSet<u64>,
    subtitle_changes: &'a std::collections::BTreeMap<
        crate::subtitle::SubtitleSource,
        crate::subtitle::SubtitleChange,
    >,
    source_container: Option<crate::edit::ContainerFormat>,
    container_target: Option<crate::edit::ContainerFormat>,
    container_metadata_changed: bool,
    container_conflicts: usize,
    conflicting_streams: &'a std::collections::BTreeSet<u64>,
    subtitle_columns_side_by_side: bool,
    subtitle_column_width: usize,
}

#[cfg(test)]
fn sidecar_line(
    sidecar: &SidecarEntry,
    selected: bool,
    changed: bool,
    default: bool,
) -> Line<'static> {
    sidecar_line_with_subtitle_context(sidecar, selected, changed, default, None, None, None)
}

fn sidecar_line_with_subtitle_context(
    sidecar: &SidecarEntry,
    selected: bool,
    changed: bool,
    default: bool,
    change: Option<&SubtitleChange>,
    container: Option<ContainerFormat>,
    max_width: Option<usize>,
) -> Line<'static> {
    let marker = if selected { "›" } else { " " };
    let prefix = format!("{marker}     ");
    let metadata = change.and_then(|change| change.metadata.as_ref());
    let language = metadata
        .map(|metadata| metadata.language.as_str())
        .unwrap_or(&sidecar.language);
    let title = metadata.and_then(|metadata| metadata.title.as_deref());
    // A sidecar has no closed-caption flag of its own, so a staged CC reads as hearing
    // impaired — that is what an import writes into the container.
    let mut flags = SubtitleOverviewFlags {
        default,
        forced: metadata.map_or(sidecar.forced, |metadata| metadata.forced),
        cc: false,
        hearing_impaired: metadata.map_or(sidecar.hearing_impaired, |metadata| {
            metadata.cc || metadata.hearing_impaired
        }),
        original: metadata.is_some_and(|metadata| metadata.original),
        commentary: metadata.is_some_and(|metadata| metadata.commentary),
    };
    // Only an imported sidecar has to live inside the container; one staying on disk
    // keeps every flag its own filename carries.
    if change.is_some_and(|change| change.import_into_media)
        && let Some(container) = container
    {
        flags = flags.supported_by(container);
    }
    let format =
        subtitle_output_format_label(change, || sidecar.format.overview_label().to_string());
    let details = subtitle_overview_details(
        &format,
        crate::subtitle::normalized_language(language),
        title,
        flags,
        max_width.map(|width| width.saturating_sub(prefix.chars().count())),
    );
    Line::from(format!("{prefix}{details}")).style(
        TrackRowState {
            selected,
            changed,
            ..TrackRowState::default()
        }
        .line_style(),
    )
}

fn subtitle_columns_line(
    left: Line<'static>,
    right: Line<'static>,
    column_width: usize,
) -> Line<'static> {
    subtitle_optional_columns_line(Some(left), Some(right), column_width)
}

fn subtitle_optional_columns_line(
    left: Option<Line<'static>>,
    right: Option<Line<'static>>,
    column_width: usize,
) -> Line<'static> {
    let column = |line: Option<Line<'static>>, pad: bool| {
        let (content, style) = line.map_or_else(
            || (String::new(), Style::default()),
            |line| (truncate_end(&line.to_string(), column_width), line.style),
        );
        Span::styled(
            if pad {
                format!("{content:<column_width$}")
            } else {
                content
            },
            style,
        )
    };
    Line::from(vec![
        column(left, true),
        Span::raw("  "),
        column(right, false),
    ])
}

fn section(lines: &mut Vec<Line<'static>>, name: &str) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::styled(
        name.to_string(),
        Style::default().fg(Color::Cyan).bold(),
    ));
}

fn container_line(
    info: &MediaInfo,
    source_container: Option<crate::edit::ContainerFormat>,
    target: Option<crate::edit::ContainerFormat>,
    metadata_changed: bool,
    conflicts: usize,
    selected: bool,
) -> Line<'static> {
    let source = source_container.map_or_else(
        || {
            string(&info.format, "format_long_name")
                .or_else(|| string(&info.format, "format_name"))
                .unwrap_or("Unknown container")
                .to_string()
        },
        |container| container.label().to_string(),
    );
    let format = target.map_or_else(
        || source.clone(),
        |target| format!("{source} → {}", target.label()),
    );
    // Format, duration, size — nothing else. The title and bit rate are in the `i`
    // panel, and the overview row is for telling files apart at a glance.
    let mut parts = vec![format];
    if let Some(duration) = number_string(&info.format, "duration").and_then(parse_number) {
        parts.push(format_duration(duration));
    }
    if let Some(size) = number_string(&info.format, "size").and_then(parse_number) {
        parts.push(format_bytes(size));
    }
    if conflicts > 0 {
        parts.push(format!(
            "⚠ {conflicts} compatibility conflict{}",
            if conflicts == 1 { "" } else { "s" }
        ));
    }
    let marker = if selected { "›" } else { " " };
    let changed = target.is_some() || metadata_changed;
    Line::from(format!("{marker}    {}", parts.join("  ·  "))).style(
        TrackRowState {
            selected,
            deleted: false,
            conflict: conflicts > 0,
            changed,
        }
        .line_style(),
    )
}

fn stream_line(
    stream: &std::collections::BTreeMap<String, Value>,
    fallback_index: usize,
    selected: bool,
    deleted: bool,
    changed: bool,
    conflict: bool,
    default: bool,
) -> Line<'static> {
    stream_line_with_subtitle_context(
        stream,
        fallback_index,
        selected,
        deleted,
        changed,
        conflict,
        default,
        None,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn stream_line_with_subtitle_context(
    stream: &std::collections::BTreeMap<String, Value>,
    fallback_index: usize,
    selected: bool,
    deleted: bool,
    changed: bool,
    conflict: bool,
    default: bool,
    subtitle_change: Option<&SubtitleChange>,
    container: Option<ContainerFormat>,
    max_width: Option<usize>,
) -> Line<'static> {
    let index = number_string(stream, "index").unwrap_or_else(|| fallback_index.to_string());
    let kind = string(stream, "codec_type").unwrap_or("unknown");
    let codec = string(stream, "codec_name").unwrap_or("unknown");
    let subtitle = kind == "subtitle";
    let marker = if deleted {
        "× "
    } else if conflict {
        "⚠ "
    } else if changed {
        "~ "
    } else {
        "  "
    };
    let index_text = format!("#{index:<2} ");
    let mut details = if subtitle {
        let metadata = subtitle_change.and_then(|change| change.metadata.as_ref());
        let original_title = stream_title(stream);
        let language = metadata
            .map(|metadata| metadata.language.as_str())
            .unwrap_or_else(|| tag(stream, "language").unwrap_or("und"));
        let title = metadata.map_or_else(
            || original_title.as_deref(),
            |metadata| metadata.title.as_deref(),
        );
        let mut flags = SubtitleOverviewFlags {
            default,
            forced: metadata.map_or_else(|| stream_forced(stream), |metadata| metadata.forced),
            cc: metadata.map_or_else(|| stream_cc(stream), |metadata| metadata.cc),
            hearing_impaired: metadata.map_or_else(
                || stream_hearing_impaired(stream),
                |metadata| metadata.hearing_impaired,
            ),
            original: metadata
                .map_or_else(|| stream_original(stream), |metadata| metadata.original),
            commentary: metadata
                .map_or_else(|| stream_commentary(stream), |metadata| metadata.commentary),
        };
        if subtitle_change.is_some_and(|change| change.export_target.is_some()) {
            // An exported track leaves the container entirely: no container can veto its
            // flags, but a sidecar filename cannot spell "closed captions".
            flags.hearing_impaired |= flags.cc;
            flags.cc = false;
        } else if let Some(container) = container {
            flags = flags.supported_by(container);
        }
        let format =
            subtitle_output_format_label(subtitle_change, || subtitle_format_overview_label(codec));
        vec![subtitle_overview_details(
            &format,
            crate::subtitle::normalized_language(language),
            title,
            flags,
            max_width.map(|width| {
                width.saturating_sub(marker.chars().count() + index_text.chars().count())
            }),
        )]
    } else {
        vec![codec.to_uppercase()]
    };

    // Video reads format, resolution, frame rate; audio reads format, channels,
    // language. Neither carries a title: the `i` panel has it, and the overview row is
    // for scanning.
    match kind {
        "video" => {
            if let (Some(width), Some(height)) = (
                number_string(stream, "width"),
                number_string(stream, "height"),
            ) {
                details.push(format!("{width}×{height}"));
            }
            if let Some(fps) = string(stream, "avg_frame_rate")
                .or_else(|| string(stream, "r_frame_rate"))
                .and_then(format_frame_rate)
            {
                details.push(format!("{fps} fps"));
            }
        }
        "audio" => {
            if let Some(layout) = string(stream, "channel_layout") {
                details.push(layout.to_string());
            } else if let Some(channels) = number_string(stream, "channels") {
                details.push(format!("{channels} ch"));
            }
            if let Some(language) = tag(stream, "language").filter(|code| *code != "und") {
                details.push(language_display_name(language));
            }
        }
        "subtitle" => {}
        _ => {
            if kind != "unknown" {
                details.push(kind.to_string());
            }
        }
    }

    if !subtitle && let Some(flags) = disposition_flag_tag(stream, default) {
        details.push(flags);
    }

    let state = TrackRowState {
        selected,
        deleted,
        conflict,
        changed,
    };
    Line::from(vec![
        if marker == "  " {
            Span::raw(marker)
        } else {
            Span::styled(marker, state.marker_style())
        },
        Span::styled(index_text, state.index_style()),
        Span::raw(details.join(if subtitle { " - " } else { "  ·  " })),
    ])
    .style(state.line_style())
}

fn subtitle_format_overview_label(codec: &str) -> String {
    crate::subtitle::SubtitleFormat::from_codec(codec).map_or_else(
        || codec.to_ascii_uppercase(),
        |format| format.overview_label().to_string(),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SubtitleOverviewFlags {
    default: bool,
    forced: bool,
    cc: bool,
    hearing_impaired: bool,
    original: bool,
    commentary: bool,
}

impl SubtitleOverviewFlags {
    /// Clears every flag `container` cannot store, so the overview promises only what
    /// will survive the remux. The single place a container's flag support is consulted
    /// for display — the embedded and sidecar rows must not drift apart on this.
    ///
    /// `default` is not a subtitle disposition the container can veto, so it is kept.
    fn supported_by(self, container: ContainerFormat) -> Self {
        let supported = |flag, value: bool| value && container.supports_subtitle_flag(flag);
        Self {
            default: self.default,
            forced: supported(SubtitleFlag::Forced, self.forced),
            cc: supported(SubtitleFlag::Cc, self.cc),
            hearing_impaired: supported(SubtitleFlag::HearingImpaired, self.hearing_impaired),
            original: supported(SubtitleFlag::Original, self.original),
            commentary: supported(SubtitleFlag::Commentary, self.commentary),
        }
    }
}

/// The format label an overview row should show: whatever the row is being converted to,
/// or `fallback` when it is staying as it is.
fn subtitle_output_format_label(
    change: Option<&SubtitleChange>,
    fallback: impl FnOnce() -> String,
) -> String {
    change
        .and_then(|change| change.export_target.or(change.embedded_target))
        .map_or_else(fallback, |format| format.overview_label().to_string())
}

fn subtitle_overview_details(
    format: &str,
    language: &str,
    title: Option<&str>,
    flags: SubtitleOverviewFlags,
    max_width: Option<usize>,
) -> String {
    const SEPARATOR: &str = " · ";
    const MIN_TITLE_WIDTH: usize = 4;

    let language = language.to_ascii_uppercase();
    let mut active = Vec::new();
    if flags.default {
        active.push("D");
    }
    if flags.forced {
        active.push("F");
    }
    if flags.cc {
        active.push("CC");
    }
    if flags.hearing_impaired {
        active.push("HI");
    }
    if flags.original {
        active.push("OG");
    }
    if flags.commentary {
        active.push("CM");
    }

    let flag_tag = (!active.is_empty()).then(|| format!("[{}]", active.join("/")));
    let mut parts = vec![format.to_string(), language];
    if let Some(flags) = flag_tag.as_ref() {
        parts.push(flags.clone());
    }
    let base = parts.join(SEPARATOR);

    if let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) {
        let title_width = max_width.map_or(title.chars().count(), |width| {
            width.saturating_sub(base.chars().count() + SEPARATOR.chars().count())
        });
        if title_width >= MIN_TITLE_WIDTH {
            let title = truncate_end(title, title_width);
            let title_index = 2;
            parts.insert(title_index, title);
        }
    }

    let details = parts.join(SEPARATOR);
    max_width.map_or(details.clone(), |width| truncate_end(&details, width))
}

fn render_dialog(frame: &mut Frame, app: &mut App, dialog: Dialog) {
    if dialog == Dialog::Keybindings {
        render_keybindings_dialog(frame, app);
        return;
    }
    if dialog == Dialog::ContainerSettings {
        render_container_settings_dialog(frame, app);
        return;
    }
    if dialog == Dialog::VideoSettings {
        render_video_settings_dialog(frame, app);
        return;
    }
    if dialog == Dialog::SubtitleSettings {
        render_subtitle_settings_dialog(frame, app);
        return;
    }
    if dialog == Dialog::ConfirmCancel {
        render_batch_progress_dialog(frame, app);
        render_cancel_edit_dialog(frame, app);
        return;
    }
    if dialog == Dialog::ConfirmProcessAll {
        render_confirm_process_all_dialog(frame, app);
        return;
    }
    if dialog == Dialog::BatchProcessing {
        render_batch_progress_dialog(frame, app);
        return;
    }
    if dialog == Dialog::ConfirmReset {
        render_confirm_reset_dialog(frame, app);
        return;
    }
    if dialog == Dialog::ResolveConflicts {
        render_resolve_conflicts_dialog(frame, app);
        return;
    }
    let (title, body, color) = match dialog {
        Dialog::Keybindings | Dialog::ContainerSettings | Dialog::VideoSettings => unreachable!(),
        Dialog::SubtitleSettings | Dialog::ConfirmCancel => unreachable!(),
        Dialog::ConfirmProcessAll | Dialog::BatchProcessing => unreachable!(),
        Dialog::ConfirmReset | Dialog::ResolveConflicts => unreachable!(),
        Dialog::Error => (
            " Error ",
            app.edit_error
                .clone()
                .unwrap_or_else(|| "An unknown editing error occurred.".to_string()),
            Color::Red,
        ),
    };
    let area = centered_fixed(frame.area(), 64, 9);
    let text = padded_popup_text(Text::from(body));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(color))
                    .title(title),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn container_field_help_title(field: ContainerSettingsField) -> String {
    format!(" Information about {} ", field.label())
}

fn container_field_help_text(_app: &App, popup: &ContainerSettingsPopup) -> Text<'static> {
    let description = match popup.field {
        ContainerSettingsField::Format => {
            "Sets the target container format written to the output file (e.g. MKV, MP4, MOV, WebM).\n\nIt's possible that a container doesn't support certain tracks, each conflict has to be resolved before you're able to save all edits."
        }
        ContainerSettingsField::Title => {
            "Title tag written to the container metadata. Displayed by media players as the main work or episode title."
        }
        ContainerSettingsField::Comment => {
            "Comment or synopsis tag written to the container metadata."
        }
        ContainerSettingsField::Date => {
            "Release year or creation timestamp written to the container metadata. Commonly in the 'yyyy-MM-dd' format."
        }
        ContainerSettingsField::Genre => {
            "Genre tag written to the container metadata.\n\nStored exactly as typed. Most players read a comma-separated list as multiple genres, for example \u{201c}Animation, Adventure, Comedy\u{201d}."
        }
        ContainerSettingsField::Artist => {
            "Creator, director, or artist tag written to the container metadata."
        }
    };
    help_paragraphs(vec![(
        description.to_string(),
        Style::default().fg(Color::White),
    )])
}

fn render_container_settings_dialog(frame: &mut Frame, app: &App) {
    let Some(popup) = app.container_settings_popup.as_ref() else {
        return;
    };
    let effective = app.effective_container_metadata().unwrap_or_default();
    let selected = |field| popup.field == field;
    let changed = |field| app.container_field_changed(field);

    let mut lines = Vec::new();
    let mut field_lines = Vec::new();

    // 1. Format Field
    field_lines.push((ContainerSettingsField::Format, lines.len()));
    let choices = app.container_choices();
    let format_label = choices
        .iter()
        .find(|choice| choice.value == app.container_target)
        .map(|choice| choice.label.clone())
        .unwrap_or_else(|| app.original_container_label());
    lines.push(setting_line(
        "Format",
        &format_label,
        selected(ContainerSettingsField::Format) && popup.mode == ContainerSettingsMode::Summary,
        changed(ContainerSettingsField::Format),
        popup.mode == ContainerSettingsMode::FormatDropdown,
    ));

    if popup.mode == ContainerSettingsMode::FormatDropdown {
        let last_index = choices.len().saturating_sub(1);
        for (position, choice) in choices.iter().enumerate() {
            lines.push(container_choice_line(
                choice,
                position == popup.format_cursor,
                position == last_index,
            ));
        }
    }

    // 2. Metadata Text Fields. The third column is the hint shown while the field is
    // empty: an example of the value wanted, not a restatement of the label.
    let text_fields = [
        (
            "Title",
            ContainerSettingsField::Title,
            effective.title.as_deref(),
            "e.g. Big Buck Bunny",
        ),
        (
            "Comment",
            ContainerSettingsField::Comment,
            effective.comment.as_deref(),
            "a note about this file",
        ),
        (
            "Date",
            ContainerSettingsField::Date,
            effective.date.as_deref(),
            "e.g. 2008-04-10",
        ),
        (
            "Genre",
            ContainerSettingsField::Genre,
            effective.genre.as_deref(),
            "e.g. Animation",
        ),
        (
            "Artist",
            ContainerSettingsField::Artist,
            effective.artist.as_deref(),
            "e.g. Blender Foundation",
        ),
    ];

    lines.push(Line::from(""));

    for (label, field, val, hint) in text_fields {
        field_lines.push((field, lines.len()));
        let editing = selected(field) && popup.mode == ContainerSettingsMode::TextEdit;
        let value = if editing {
            FieldValue::Editing(&popup.text_input)
        } else {
            FieldValue::Static(val.unwrap_or(""))
        };
        lines.push(text_field_line(
            TextField::new(label, value, TextInputConfig::CONTAINER_METADATA.width)
                .selected(selected(field))
                .changed(changed(field))
                .placeholder(hint)
                .reject(app.text_input_reject(TextInputSite::ContainerMetadata)),
        ));
    }

    let focus_line = match popup.mode {
        ContainerSettingsMode::FormatDropdown => 1 + popup.format_cursor,
        ContainerSettingsMode::Summary | ContainerSettingsMode::TextEdit => field_lines
            .iter()
            .find_map(|(field, line)| (*field == popup.field).then_some(*line))
            .unwrap_or(0),
    };

    render_settings_dialog(
        frame,
        SettingsDialog {
            text: padded_popup_text(Text::from(lines)),
            title: " Container settings ".to_string(),
            focus_line,
            help: popup.help_visible.then(|| {
                (
                    container_field_help_text(app, popup),
                    container_field_help_title(popup.field),
                )
            }),
            min_height: 10,
        },
    );
}

fn container_choice_line(choice: &ContainerChoice, cursor: bool, last: bool) -> Line<'static> {
    let changed = choice.staged && !choice.current;
    let mut line = subtitle_codec_line(&choice.label, cursor, changed, true, last);
    if let Some(warning) = choice.warning() {
        let target_prefix = format!("{} ", choice.label);
        let warning = warning.strip_prefix(&target_prefix).unwrap_or(&warning);
        line.spans.push(Span::styled(
            format!("  ⚠ {warning}"),
            if cursor {
                focused_style(false)
            } else {
                warning_style(changed)
            },
        ));
    }
    line
}

fn render_cancel_edit_dialog(frame: &mut Frame, app: &App) {
    let lines = vec![
        Line::from("Are you sure you want to cancel the current operation?").centered(),
        Line::from(""),
        Line::from(vec![
            action_option(
                " Keep processing ",
                app.cancel_edit_choice == CancelEditChoice::KeepProcessing,
            ),
            Span::raw("  "),
            action_option(
                " Cancel processing ",
                app.cancel_edit_choice == CancelEditChoice::CancelProcessing,
            ),
        ])
        .centered(),
    ];
    let text = padded_popup_text(Text::from(lines));
    let area = centered_fixed(frame.area(), 64, 7);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(" Confirm cancellation "),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The `r`/`R` "are you sure?" popup — mirrors `render_cancel_edit_dialog`'s
/// two-option layout, naming what's about to be discarded via `ResetScope::label`.
fn render_confirm_reset_dialog(frame: &mut Frame, app: &App) {
    let Some(scope) = app.pending_reset() else {
        return;
    };
    let lines = vec![
        Line::from(scope.label()).centered(),
        Line::from(""),
        Line::from(vec![
            action_option(" Keep edits ", app.reset_choice == ResetChoice::KeepEdits),
            Span::raw("  "),
            action_option(" Reset edits ", app.reset_choice == ResetChoice::ResetEdits),
        ])
        .centered(),
    ];
    let text = padded_popup_text(Text::from(lines));
    let area = centered_fixed(frame.area(), 64, 7);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(" Confirm reset "),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn action_option(label: impl Into<std::borrow::Cow<'static, str>>, focused: bool) -> Span<'static> {
    Span::styled(
        label.into(),
        if focused {
            focused_style(false)
        } else {
            Style::default().fg(Color::White)
        },
    )
}

pub fn filter_keybindings_text(text: Text<'static>, query: &str) -> (Text<'static>, usize) {
    let clean_query = query.trim().to_lowercase();
    if clean_query.is_empty() {
        let count = text.lines.iter().filter(|l| is_keybinding_entry(l)).count();
        return (text, count);
    }
    let mut match_count = 0;
    let filtered_lines: Vec<Line<'static>> = text
        .lines
        .into_iter()
        .filter(|line| {
            let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let matches = plain.to_lowercase().contains(&clean_query);
            if matches && is_keybinding_entry(line) {
                match_count += 1;
            }
            matches
        })
        .collect();
    (Text::from(filtered_lines), match_count)
}

fn is_keybinding_entry(line: &Line) -> bool {
    line.spans.iter().any(|s| s.content.starts_with("  "))
}

/// Columns a search bar gives up to its label, its frame and the widest match-count
/// suffix it might grow. Reserving the widest keeps the value window from shifting a
/// column each time the count crosses a digit boundary.
const SEARCH_BAR_OVERHEAD: usize = "  Search ".len() + FIELD_FRAME_COLUMNS + " (999 matches)".len();

/// Columns a frame costs a row: the opening glyph and its gutter, plus the closing
/// gutter and glyph. Counted rather than measured because the glyphs are multi-byte.
const FIELD_FRAME_COLUMNS: usize = 4;

/// Renders a pane-bottom search bar and records the width it was given, so the next
/// keystroke scrolls the caret against the width actually drawn.
fn search_line(search: &mut SearchState, area: Rect, reject: Option<InputReject>) -> Line<'static> {
    search.field_width = (area.width as usize).saturating_sub(SEARCH_BAR_OVERHEAD);
    text_field_line(
        TextField::new(
            "Search",
            FieldValue::Editing(&search.input),
            search.field_width,
        )
        .bar()
        .selected(search.is_active)
        .placeholder("type to filter")
        .suffix(match_suffix(search.match_count))
        .reject(reject),
    )
}

fn render_keybindings_dialog(frame: &mut Frame, app: &mut App) {
    let area = popup_area(frame.area(), 80, 80);
    let full_text = keybindings_text();
    let (filtered_text, count) = filter_keybindings_text(full_text, &app.keybindings_search.value);
    app.keybindings_search.match_count = count;
    let text = padded_popup_text(filtered_text);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Keybindings ");

    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    if app.keybindings_search.is_active || !app.keybindings_search.value.is_empty() {
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

        app.set_keybindings_max_scroll(max_scroll(&text, chunks[0]));

        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((app.keybindings_scroll, 0)),
            chunks[0],
        );

        let reject = app.text_input_reject(TextInputSite::KeybindingsSearch);
        let search = search_line(&mut app.keybindings_search, chunks[1], reject);
        frame.render_widget(Paragraph::new(search), chunks[1]);
    } else {
        app.set_keybindings_max_scroll(max_scroll(&text, inner));

        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((app.keybindings_scroll, 0)),
            inner,
        );
    }
}

fn keybindings_text() -> Text<'static> {
    let mut lines = Vec::new();
    keybindings_section(&mut lines, "General");
    keybinding(&mut lines, "?", "Open or close keybindings");
    keybinding(&mut lines, "/", "Search files, keybindings, or languages");
    keybinding(&mut lines, "Esc / q", "Close, go back, or quit");
    keybinding(&mut lines, "j/k / Up/Down", "Move or scroll vertically");
    keybinding(&mut lines, "h/l / Left/Right", "Change a horizontal choice");
    keybinding(&mut lines, "gg / G", "Go to the first/top or last/bottom");
    keybinding(&mut lines, "Ctrl-d / Ctrl-u", "Scroll ten lines");
    keybinding(&mut lines, "Ctrl-n / Ctrl-p", "Move through search results");
    keybinding(
        &mut lines,
        "za / zo / zc",
        "Toggle / open / close sidecar fold",
    );
    keybinding(&mut lines, "zM / zR", "Close / open all sidecar folds");
    keybinding(&mut lines, "Enter", "Open, select, or confirm");

    keybindings_section(&mut lines, "Track editing");
    keybinding(
        &mut lines,
        "Ctrl-j / Ctrl-k",
        "Move track down / up within its type",
    );
    keybinding(
        &mut lines,
        "Ctrl-h / Ctrl-l",
        "Mark subtitle for import / export",
    );
    keybinding(
        &mut lines,
        "Enter",
        "Edit container, video, or subtitle settings",
    );
    keybinding(&mut lines, "K", "Explain the highlighted subtitle field");
    keybinding(&mut lines, "i", "Toggle container or stream information");
    keybinding(&mut lines, "d", "Mark or unmark track for deletion");
    keybinding(&mut lines, "Ctrl-s", "Review and save pending edits");

    keybindings_section(&mut lines, "Text input");
    keybinding(
        &mut lines,
        "i",
        "Edit the selected text or number field in a settings dialog",
    );
    keybinding(
        &mut lines,
        "/",
        "Search the file list, keybindings, or languages",
    );
    keybinding(&mut lines, "Left/Right", "Move the text cursor");
    keybinding(
        &mut lines,
        "Home/End or Ctrl-a/Ctrl-e",
        "Move to the start or end of text",
    );
    keybinding(
        &mut lines,
        "Backspace/Delete",
        "Delete text around the cursor",
    );
    keybinding(&mut lines, "Ctrl-w", "Delete the word before the cursor");
    keybinding(&mut lines, "Ctrl-u", "Delete everything before the cursor");
    keybinding(&mut lines, "Paste", "Insert the clipboard as one edit");
    keybinding(&mut lines, "Esc / Enter", "Finish or accept text input");
    keybinding(
        &mut lines,
        "Esc",
        "Discard a search query and leave the bar",
    );
    Text::from(lines)
}

fn keybindings_section(lines: &mut Vec<Line<'static>>, name: &str) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::styled(
        name.to_string(),
        Style::default().fg(Color::Cyan).bold(),
    ));
}

fn keybinding(lines: &mut Vec<Line<'static>>, keys: &str, description: &str) {
    lines.push(Line::from(vec![
        Span::styled(format!("  {keys:<18}"), Style::default().fg(Color::Yellow)),
        Span::raw(description.to_string()),
    ]));
}

/// The original single-file design: one centered box, one label line, one gauge (or
/// indeterminate loader before the first measured progress arrives). Used whenever
/// `active_batch` has exactly one item — including the common case of processing
/// just one staged file, which never needs the multi-row table — via
/// `render_batch_progress_dialog`.
fn render_progress_dialog(frame: &mut Frame, app: &App) {
    let Some(batch) = app.active_batch.as_ref() else {
        return;
    };
    let Some(item) = batch.items.first() else {
        return;
    };
    let area = centered_fixed(frame.area(), 64, 9);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Saving media edits ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .margin(1)
    .split(inner);

    frame.render_widget(
        Paragraph::new("Processing...")
            .centered()
            .style(Style::default().fg(Color::Cyan).bold()),
        rows[0],
    );
    let label = item.label.clone().unwrap_or_else(|| {
        let name = item
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        format!("Processing {name}")
    });
    frame.render_widget(
        Paragraph::new(processing_info_line(truncate_end(
            &label,
            rows[2].width as usize,
        )))
        .centered(),
        rows[2],
    );

    let tick = (batch.started.elapsed().as_millis() / 80) as usize;
    let percent = item
        .fraction
        .map(|fraction| (fraction.clamp(0.0, 1.0) * 100.0).round() as u16);
    if let Some(percent) = percent {
        frame.render_widget(
            Gauge::default()
                .gauge_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .percent(percent)
                .label(format!("{percent}%")),
            rows[4],
        );
    } else {
        frame.render_widget(Paragraph::new(loader_line(tick)).centered(), rows[4]);
    }
}

/// Renders one bullet per file about to be processed, all sharing a single "Start" /
/// "Cancel" hint — the confirm step for `App::request_process_all`, gating on every
/// staged file already being valid (see `App::confirm_process_all`).
/// Lists every staged file with a summary of its own staged changes (`App::
/// staged_file_summary`), scrollable like the keybindings dialog once the content
/// exceeds the popup — replaces the old bare filename list, which grew unbounded
/// with the file count instead of scrolling. A fixed Start/Cancel button bar (mirrors
/// `render_cancel_edit_dialog`/`render_confirm_reset_dialog`'s `action_option` style)
/// stays pinned at the bottom, outside the scrollable area. Since the list scrolls
/// now, the popup itself can stay compact rather than growing to fit every file.
fn render_confirm_process_all_dialog(frame: &mut Frame, app: &mut App) {
    let mut paths: Vec<_> = app.staged_edits.keys().cloned().collect();
    paths.sort();
    let count = paths.len();
    let title = format!(
        " Process {count} file{} ",
        if count == 1 { "" } else { "s" }
    );
    let area = popup_area(frame.area(), 60, 50);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for path in &paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        lines.push(Line::styled(format!("• {name}"), changed_style()));
        for change in app.staged_file_summary(path) {
            lines.push(Line::from(format!("    {change}")));
        }
        lines.push(Line::from(""));
    }
    let text = padded_popup_text(Text::from(lines));

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    app.set_confirm_process_all_max_scroll(max_scroll(&text, chunks[0]));
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((app.confirm_process_all_scroll, 0)),
        chunks[0],
    );

    let buttons = Line::from(vec![
        action_option(
            " \u{25b6} Start ",
            app.confirm_process_all_choice == ConfirmProcessAllChoice::Start,
        ),
        Span::raw("  "),
        action_option(
            " Cancel ",
            app.confirm_process_all_choice == ConfirmProcessAllChoice::Cancel,
        ),
    ])
    .centered();
    frame.render_widget(Paragraph::new(buttons), chunks[1]);
}

/// The unskippable notice for files whose tracks moved out from under a staged edit —
/// see `App::conflicting_paths`. One section per affected file: its name and which
/// track types changed, then the staged changes for exactly those types, which
/// acknowledging will revert (`App::acknowledge_conflicts`). Everything else the file
/// stages is untouched and deliberately not listed.
///
/// There is only one button, because there is only one outcome. Keeping the staged
/// changes was previously offered and could never succeed — the tracks they name are
/// no longer the tracks in the file, so every path but reverting them ends at the same
/// blocked save. Escape is not handled either (see `input::handle_key`), making the
/// button the only way out. Borrows `render_confirm_process_all_dialog`'s
/// scrollable-list-plus-pinned-button-bar layout. No inline key hint — control help
/// lives solely in the keybindings popup (`?`).
/// Width the conflict notice's labels are padded to, so their values line up in one
/// column. Sized to the longest label (`Reverting`) plus its colon and a space.
const CONFLICT_LABEL_WIDTH: usize = "Reverting: ".len();

/// `Label:      value`, one label against however many values, each starting on its
/// own line and every continuation indented to the shared column — including the
/// continuations produced by wrapping a value too long for the popup, which is why
/// this pre-wraps rather than leaving it to `Wrap`: ratatui wraps back to column
/// zero, which would break the very alignment the column exists for.
///
/// Renders nothing at all for an empty list, so a file with nothing to keep simply
/// has no `Keeping` row rather than an empty one.
fn labelled_rows(
    label: &str,
    values: Vec<String>,
    style: Style,
    width: usize,
) -> Vec<Line<'static>> {
    let indent = " ".repeat(CONFLICT_LABEL_WIDTH);
    let mut lines = Vec::new();
    for value in values {
        for piece in wrap_value(&value, width.saturating_sub(CONFLICT_LABEL_WIDTH).max(1)) {
            let head = if lines.is_empty() {
                Span::styled(
                    format!("{:<CONFLICT_LABEL_WIDTH$}", format!("{label}:")),
                    Style::default().fg(Color::DarkGray),
                )
            } else {
                Span::raw(indent.clone())
            };
            lines.push(Line::from(vec![head, Span::styled(piece, style)]));
        }
    }
    lines
}

/// Word-wraps to `width` *display columns* rather than bytes or chars, so a wide
/// character or an accented filename can't overrun the popup. A single word longer
/// than the width is split rather than allowed to overflow.
fn wrap_value(value: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for word in value.split_whitespace() {
        let word_width = word.width();
        if current_width > 0 && current_width + 1 + word_width > width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if word_width > width {
            // Longer than a whole line on its own: fill what's left, then keep going.
            for character in word.chars() {
                let character_width = character.width().unwrap_or(0);
                if current_width + character_width > width && current_width > 0 {
                    lines.push(std::mem::take(&mut current));
                    current_width = 0;
                }
                current.push(character);
                current_width += character_width;
            }
            continue;
        }
        if current_width > 0 {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(word);
        current_width += word_width;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn render_resolve_conflicts_dialog(frame: &mut Frame, app: &mut App) {
    let paths = app.conflicting_paths();
    let single = paths.len() == 1;
    let area = popup_area(frame.area(), 60, 50);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(if single {
            " Source file changed "
        } else {
            " Source files changed "
        });
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);

    // An aligned label column per file, rather than prose: four labels each answering
    // one question — which file, what moved under it, what that costs, what it
    // doesn't. The popup title already says the source changed, so no line repeats it.
    // `Keeping` is the one that stops this reading as more destructive than it is:
    // only the conflicting track types are reverted, which is invisible if the notice
    // lists nothing but losses.
    // Matches `max_scroll`'s idea of the usable content width, so a value pre-wrapped
    // here is never re-wrapped by the `Wrap` below (which would lose the indent).
    let width = chunks[0].width.saturating_sub(2).max(1) as usize;
    let value_style = Style::default().fg(Color::White);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for path in &paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let groups = app.conflict_groups_for(path);
        lines.extend(labelled_rows(
            "File",
            vec![name.to_string()],
            changed_style(),
            width,
        ));
        lines.extend(labelled_rows(
            "Changed",
            vec![format!("{}, on disk", describe_track_groups(&groups))],
            value_style,
            width,
        ));
        let reverted = app.conflicting_change_summary(path);
        // A staged change can fail to describe itself — a deleted track that has since
        // vanished from the file has nothing left to name it by — and a bare "Reverting"
        // with nothing after it reads like a bug rather than an answer.
        let reverted = if reverted.is_empty() {
            vec![format!(
                "the staged {} changes",
                describe_track_groups(&groups)
                    .strip_suffix(" tracks")
                    .unwrap_or("track")
            )]
        } else {
            reverted
        };
        lines.extend(labelled_rows("Reverting", reverted, value_style, width));
        lines.extend(labelled_rows(
            "Keeping",
            app.kept_change_summary(path),
            value_style,
            width,
        ));
        lines.push(Line::from(""));
    }
    let text = padded_popup_text(Text::from(lines));

    app.set_conflict_max_scroll(max_scroll(&text, chunks[0]));
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((app.conflict_scroll, 0)),
        chunks[0],
    );

    // Inert for its first seconds, counting down inside its own label — the notice
    // appears unprompted, so an Enter already in flight must not acknowledge it. See
    // `App::conflict_countdown`.
    let button = match app.conflict_countdown() {
        Some(seconds) => action_option(format!(" Understood ({seconds}) "), false),
        None => action_option(" Understood ", true),
    };
    frame.render_widget(Paragraph::new(Line::from(button).centered()), chunks[1]);
}

/// One progress row per staged file being processed, table-like (a single column of
/// rows), each rendered like the single-file `render_progress_dialog` (a label line
/// plus a gauge once measured progress is known, or an indeterminate loader before
/// then). The row under `App::batch_cursor` carries a thick cyan left border, and the
/// viewport (`App::batch_scroll`, synced here where the visible row count is known)
/// follows that cursor when more files are in the batch than fit in the popup.
fn render_batch_progress_dialog(frame: &mut Frame, app: &mut App) {
    let Some(total) = app.active_batch.as_ref().map(|batch| batch.items.len()) else {
        return;
    };
    if total == 1 {
        render_progress_dialog(frame, app);
        return;
    }
    let area = centered_fixed(
        frame.area(),
        70,
        frame.area().height.saturating_sub(4).min(24),
    );
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" Processing {total} files "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    const ROW_HEIGHT: u16 = 3; // name/status line, gauge line, blank spacer
    let visible_rows = (inner.height / ROW_HEIGHT).max(1) as usize;
    app.sync_batch_scroll(visible_rows);
    let offset = app.batch_scroll as usize;
    let cursor = app.batch_cursor;
    let Some(batch) = app.active_batch.as_ref() else {
        return;
    };
    let tick = (batch.started.elapsed().as_millis() / 80) as usize;

    let mut constraints = vec![Constraint::Length(ROW_HEIGHT); visible_rows.min(total - offset)];
    if constraints.is_empty() {
        return;
    }
    let footer_needed = offset + constraints.len() < total || offset > 0;
    if footer_needed {
        constraints.push(Constraint::Length(1));
    }
    let rows = Layout::vertical(constraints).split(inner);

    for (row_index, item) in batch.items[offset..].iter().take(visible_rows).enumerate() {
        // The left gutter carries the cursor bar; every row is indented by it so the
        // text does not shift when the highlight moves.
        let columns =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(0)]).split(rows[row_index]);
        let sub_rows =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(columns[1]);
        if offset + row_index == cursor {
            let bar = Rect {
                height: sub_rows[1].bottom().saturating_sub(columns[0].y),
                ..columns[0]
            };
            frame.render_widget(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_type(BorderType::Thick)
                    .border_style(Style::default().fg(Color::Cyan)),
                bar,
            );
        }
        let name = item
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let (status_text, status_style) = match &item.status {
            BatchItemStatus::Pending => {
                ("Waiting…".to_string(), Style::default().fg(Color::DarkGray))
            }
            BatchItemStatus::Running => (
                item.label
                    .clone()
                    .unwrap_or_else(|| "Processing".to_string()),
                Style::default().fg(Color::Cyan),
            ),
            BatchItemStatus::Completed => ("Done".to_string(), Style::default().fg(Color::Green)),
            BatchItemStatus::Failed(error) => {
                (format!("Failed: {error}"), Style::default().fg(Color::Red))
            }
            BatchItemStatus::Cancelled => (
                "Cancelled".to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        };
        frame.render_widget(
            Line::from(vec![
                Span::styled(
                    truncate_end(name, sub_rows[0].width.saturating_sub(1) as usize / 2),
                    Style::default().bold(),
                ),
                Span::raw(" "),
                Span::styled(
                    truncate_end(&status_text, sub_rows[0].width as usize / 2),
                    status_style,
                ),
            ]),
            sub_rows[0],
        );
        match &item.status {
            BatchItemStatus::Completed | BatchItemStatus::Cancelled => {
                let is_completed = item.status == BatchItemStatus::Completed;
                frame.render_widget(
                    Gauge::default()
                        .gauge_style(
                            Style::default().fg(status_style.fg.unwrap_or(Color::DarkGray)),
                        )
                        .percent(if is_completed { 100 } else { 0 })
                        .label(status_text.clone()),
                    sub_rows[1],
                );
            }
            BatchItemStatus::Failed(_) => {
                frame.render_widget(Paragraph::new(""), sub_rows[1]);
            }
            BatchItemStatus::Pending | BatchItemStatus::Running => {
                if let Some(fraction) = item.fraction {
                    let percent = (fraction.clamp(0.0, 1.0) * 100.0).round() as u16;
                    frame.render_widget(
                        Gauge::default()
                            .gauge_style(
                                Style::default()
                                    .fg(Color::Cyan)
                                    .bg(Color::DarkGray)
                                    .add_modifier(Modifier::BOLD),
                            )
                            .percent(percent)
                            .label(format!("{percent}%")),
                        sub_rows[1],
                    );
                } else {
                    frame.render_widget(Paragraph::new(loader_line(tick)), sub_rows[1]);
                }
            }
        }
    }

    if footer_needed {
        let footer_area = rows[rows.len() - 1];
        let remaining_above = offset;
        let remaining_below = total.saturating_sub(offset + visible_rows.min(total - offset));
        let mut parts = Vec::new();
        if remaining_above > 0 {
            parts.push(format!("↑ {remaining_above} more"));
        }
        if remaining_below > 0 {
            parts.push(format!("↓ {remaining_below} more"));
        }
        frame.render_widget(
            Paragraph::new(processing_info_line(parts.join("   ·   "))).centered(),
            footer_area,
        );
    }
}

fn loader_line(tick: usize) -> Line<'static> {
    const POSITIONS: [usize; 4] = [0, 1, 2, 1];
    let active = POSITIONS[(tick / 2) % POSITIONS.len()];
    let mut spans = Vec::with_capacity(3);
    for position in 0..3 {
        spans.push(if position == active {
            Span::styled(
                "●",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("•", Style::default().fg(Color::DarkGray))
        });
    }
    Line::from(spans)
}

fn processing_info_line(description: String) -> Line<'static> {
    Line::styled(
        description,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )
}

fn render_video_settings_dialog(frame: &mut Frame, app: &App) {
    let Some(popup) = app.video_settings_popup.as_ref() else {
        return;
    };
    if popup.mode == VideoSettingsMode::CustomResolution {
        render_custom_resolution_dialog(frame, app);
        return;
    }
    let settings = app
        .video_settings
        .get(&popup.stream_index)
        .copied()
        .unwrap_or_default();
    let codec_choices = app.video_codec_choices(popup.stream_index);
    let codec_label = codec_choices
        .iter()
        .find(|choice| choice.value == settings.codec)
        .map(|choice| choice.label.as_str())
        .unwrap_or("Unknown");
    let resolution_choices = app.resolution_choices(popup.stream_index);
    let resolution_label = resolution_choices
        .iter()
        .find(|choice| choice.selected(settings.resolution))
        .map(|choice| choice.label.clone())
        .unwrap_or_else(|| settings.resolution.label());

    let codec_expanded =
        popup.mode == VideoSettingsMode::Dropdown && popup.field == VideoSettingsField::Codec;
    let resolution_expanded =
        popup.mode == VideoSettingsMode::Dropdown && popup.field == VideoSettingsField::Resolution;
    // Each field's options are pushed straight after that field's own row. Collecting
    // both rows first and appending the options at the end would hang an expanded
    // Codec list underneath Resolution.
    let mut lines = vec![setting_line(
        "Codec",
        codec_label,
        popup.field == VideoSettingsField::Codec,
        settings.codec != crate::edit::VideoCodec::Original,
        codec_expanded,
    )];
    if codec_expanded {
        let last_index = codec_choices.len().saturating_sub(1);
        for (position, choice) in codec_choices.iter().enumerate() {
            let label = match &choice.reason {
                Some(reason) => format!("{} — {reason}", choice.label),
                None => choice.label.clone(),
            };
            lines.push(subtitle_codec_line(
                &label,
                position == popup.codec_cursor,
                settings.codec != crate::edit::VideoCodec::Original
                    && choice.value == settings.codec,
                choice.enabled,
                position == last_index,
            ));
        }
    }
    lines.push(setting_line(
        "Resolution",
        &resolution_label,
        popup.field == VideoSettingsField::Resolution,
        settings.resolution != crate::edit::VideoResolution::Original,
        resolution_expanded,
    ));
    if resolution_expanded {
        let last_index = resolution_choices.len().saturating_sub(1);
        for (position, choice) in resolution_choices.iter().enumerate() {
            let selected = choice.selected(settings.resolution);
            lines.push(dropdown_line(
                &choice.label,
                position == popup.resolution_cursor,
                selected,
                choice.enabled,
                settings.resolution != crate::edit::VideoResolution::Original && selected,
                position == last_index,
            ));
        }
    }

    let text = padded_popup_text(Text::from(lines));
    let height = (text.lines.len() as u16 + 2).max(7);
    let area = centered_fixed(frame.area(), 58, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(format!(" Video track #{} settings ", popup.stream_index)),
        ),
        area,
    );
}

fn render_custom_resolution_dialog(frame: &mut Frame, app: &App) {
    let Some(popup) = app.video_settings_popup.as_ref() else {
        return;
    };
    let Some(draft) = popup.custom_resolution.as_ref() else {
        return;
    };
    let source = app
        .video_source_dimensions(popup.stream_index)
        .map(|(width, height)| format!("Original: {width}×{height}"))
        .unwrap_or_else(|| "Original resolution unavailable".to_string());
    let source_dimensions = app.video_source_dimensions(popup.stream_index);
    let width_changed = source_dimensions
        .is_some_and(|(width, _)| draft.width.value.parse::<u64>().ok() != Some(width));
    let height_changed = source_dimensions
        .is_some_and(|(_, height)| draft.height.value.parse::<u64>().ok() != Some(height));
    let mut lines = vec![
        Line::styled(source, Style::default().fg(Color::DarkGray)),
        Line::from(""),
    ];
    let reject = app.text_input_reject(TextInputSite::CustomResolution);
    lines.push(custom_input_line(
        "Width",
        &draft.width,
        draft.field == CustomResolutionField::Width,
        width_changed,
        "e.g. 1920",
        reject,
    ));
    lines.push(Line::from(""));
    lines.push(custom_input_line(
        "Height",
        &draft.height,
        draft.field == CustomResolutionField::Height,
        height_changed,
        "e.g. 1080",
        reject,
    ));
    lines.push(Line::from(""));
    if let Some(error) = app.custom_resolution_error() {
        lines.push(Line::styled(error, Style::default().fg(Color::Red)));
        lines.push(Line::from(""));
    }
    lines.extend(custom_scaling_lines(draft));

    let text = padded_popup_text(Text::from(lines));
    let height = (text.lines.len() as u16 + 2).max(10);
    let area = centered_fixed(frame.area(), 58, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(format!(
                        " Video track #{} custom resolution ",
                        popup.stream_index
                    )),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn custom_input_line(
    label: &str,
    input: &TextInputState,
    focused: bool,
    changed: bool,
    placeholder: &str,
    reject: Option<InputReject>,
) -> Line<'static> {
    text_field_line(
        TextField::new(
            label,
            FieldValue::Editing(input),
            TextInputConfig::RESOLUTION.width,
        )
        .selected(focused)
        .changed(changed)
        .placeholder(placeholder)
        .reject(reject),
    )
}

fn custom_scaling_lines(draft: &crate::app::CustomResolutionDraft) -> Vec<Line<'static>> {
    let focused = draft.field == CustomResolutionField::Scaling;
    let changed = draft.scaling != crate::edit::CustomScaling::FitPad;
    let mut lines = vec![setting_line(
        "Scaling",
        draft.scaling.label(),
        focused,
        changed,
        draft.scaling_dropdown_open,
    )];
    if draft.scaling_dropdown_open {
        let last_index = crate::edit::CustomScaling::OPTIONS.len().saturating_sub(1);
        for (position, scaling) in crate::edit::CustomScaling::OPTIONS.iter().enumerate() {
            lines.push(dropdown_line(
                scaling.label(),
                position == draft.scaling_cursor,
                *scaling == draft.scaling,
                true,
                changed && *scaling == draft.scaling,
                position == last_index,
            ));
        }
    }
    lines
}

fn render_subtitle_settings_dialog(frame: &mut Frame, app: &App) {
    let Some(popup) = app.subtitle_settings_popup.as_ref() else {
        return;
    };
    let codec_choices = app.subtitle_choices(&popup.source, popup.source_format);
    let selected_codec = app.subtitle_popup_codec().unwrap_or(popup.source_format);
    let codec_selected = |choice: &crate::subtitle::FormatChoice| choice.format == selected_codec;
    let codec_staged = |choice: &crate::subtitle::FormatChoice| {
        choice.format == selected_codec && selected_codec != popup.source_format
    };
    let codec_label = codec_choices
        .iter()
        .find(|choice| codec_selected(choice))
        .map(|choice| choice.label.as_str())
        .unwrap_or_else(|| popup.source_format.label());
    let metadata = app.subtitle_popup_metadata();
    let selected = |field| popup.field == field;
    let changed = |field| app.subtitle_popup_metadata_changed(field);
    let mut lines = Vec::new();
    let mut field_lines = Vec::new();
    field_lines.push((SubtitleSettingsField::Codec, lines.len()));
    lines.push(setting_line(
        "Codec",
        codec_label,
        selected(SubtitleSettingsField::Codec),
        changed(SubtitleSettingsField::Codec),
        popup.mode == SubtitleSettingsMode::CodecDropdown,
    ));
    let mut codec_dropdown_start = None;

    if popup.mode == SubtitleSettingsMode::CodecDropdown {
        codec_dropdown_start = Some(lines.len());
        let last_index = codec_choices.len().saturating_sub(1);
        for (position, choice) in codec_choices.iter().enumerate() {
            let label = match &choice.reason {
                Some(reason) => format!("{} — {reason}", choice.label),
                None => choice.label.clone(),
            };
            lines.push(subtitle_codec_line(
                &label,
                position == popup.codec_cursor,
                codec_staged(choice),
                choice.enabled,
                position == last_index,
            ));
        }
        lines.push(Line::from(""));
    }
    let language = metadata
        .as_ref()
        .and_then(|metadata| language_choice(&metadata.language))
        .map(|choice| choice.label())
        .unwrap_or_else(|| "Choose a language…".to_string());
    field_lines.push((SubtitleSettingsField::Language, lines.len()));
    lines.push(setting_line(
        "Language",
        &language,
        selected(SubtitleSettingsField::Language),
        changed(SubtitleSettingsField::Language),
        popup.mode == SubtitleSettingsMode::LanguageDropdown,
    ));
    let mut language_dropdown_start = None;
    if popup.mode == SubtitleSettingsMode::LanguageDropdown {
        let choices = app.filtered_subtitle_languages();
        lines.push(text_field_line(
            TextField::new(
                "Search",
                FieldValue::Editing(&popup.language_search.input),
                TextInputConfig::LANGUAGE_SEARCH.width,
            )
            .selected(popup.language_search.is_active)
            .placeholder("type to filter")
            .suffix(match_suffix(choices.len()))
            .reject(app.text_input_reject(TextInputSite::LanguageSearch)),
        ));
        language_dropdown_start = Some(lines.len());
        let start = popup.language_cursor.saturating_sub(5).min(choices.len());
        let end = (start + 10).min(choices.len());
        let last_index = end.saturating_sub(1);
        for (position, choice) in choices.iter().enumerate().take(end).skip(start) {
            lines.push(dropdown_line(
                &choice.label(),
                position == popup.language_cursor,
                metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.language == choice.code),
                true,
                changed(SubtitleSettingsField::Language)
                    && metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.language == choice.code),
                position == last_index,
            ));
        }
        if choices.is_empty() {
            lines.push(Line::from(vec![
                tree_guide_span(true),
                Span::styled(
                    "No matching languages",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        lines.push(Line::from(""));
    }
    if app.subtitle_field_visible(SubtitleSettingsField::Title) {
        field_lines.push((SubtitleSettingsField::Title, lines.len()));
        lines.push(text_field_line(
            TextField::new(
                "Title",
                FieldValue::Editing(&popup.title_input),
                TextInputConfig::SUBTITLE_TITLE.width,
            )
            .selected(selected(SubtitleSettingsField::Title))
            .changed(changed(SubtitleSettingsField::Title))
            .placeholder("name shown in player menus")
            .reason(
                app.subtitle_field_reason(SubtitleSettingsField::Title)
                    .as_deref(),
            )
            .reject(app.text_input_reject(TextInputSite::SubtitleTitle)),
        ));
    }

    if let Some(metadata) = metadata.as_ref() {
        let checkbox_groups = [
            vec![
                (
                    "Default",
                    SubtitleSettingsField::Default,
                    app.subtitle_popup_default(),
                ),
                ("Forced", SubtitleSettingsField::Forced, metadata.forced),
            ],
            vec![
                ("CC", SubtitleSettingsField::Cc, metadata.cc),
                (
                    "Hearing impaired",
                    SubtitleSettingsField::HearingImpaired,
                    metadata.hearing_impaired,
                ),
            ],
            vec![
                (
                    "Original",
                    SubtitleSettingsField::Original,
                    metadata.original,
                ),
                (
                    "Commentary",
                    SubtitleSettingsField::Commentary,
                    metadata.commentary,
                ),
            ],
        ];
        for group in checkbox_groups {
            let visible = group
                .into_iter()
                .filter(|(_, field, _)| app.subtitle_field_visible(*field))
                .collect::<Vec<_>>();
            if visible.is_empty() {
                continue;
            }
            lines.push(Line::from(""));
            for (label, field, checked) in visible {
                field_lines.push((field, lines.len()));
                lines.push(subtitle_checkbox_line(
                    label,
                    checked,
                    selected(field),
                    changed(field),
                    app.subtitle_field_reason(field).as_deref(),
                ));
            }
        }
    }
    let title = match &popup.source {
        SubtitleSource::Embedded(index) => format!(" Subtitle track #{index} "),
        SubtitleSource::Sidecar(path) => format!(
            " {} ",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Subtitle sidecar")
        ),
    };
    let focus_line = match popup.mode {
        SubtitleSettingsMode::CodecDropdown => {
            codec_dropdown_start.unwrap_or(0) + popup.codec_cursor
        }
        SubtitleSettingsMode::LanguageDropdown => {
            let choices = app.filtered_subtitle_languages();
            let start = popup.language_cursor.saturating_sub(5).min(choices.len());
            language_dropdown_start.unwrap_or(0) + popup.language_cursor.saturating_sub(start)
        }
        SubtitleSettingsMode::Summary | SubtitleSettingsMode::TitleEdit => field_lines
            .iter()
            .find_map(|(field, line)| (*field == popup.field).then_some(*line))
            .unwrap_or(0),
    };
    render_settings_dialog(
        frame,
        SettingsDialog {
            text: padded_popup_text(Text::from(lines)),
            title,
            focus_line,
            help: popup.help_visible.then(|| {
                (
                    subtitle_field_help_text(app, popup),
                    subtitle_field_help_title(popup.field),
                )
            }),
            min_height: 12,
        },
    );
}

/// A settings popup: a scrolling field list, optionally beside a help panel explaining
/// the focused field. The container, subtitle and any future settings dialog differ only
/// in what they put in these fields — the geometry, scrolling and chrome are shared.
struct SettingsDialog {
    text: Text<'static>,
    title: String,
    /// Line the focused field sits on, kept on screen by scrolling to it.
    focus_line: usize,
    /// Help text and its panel title, when the user has the panel open.
    help: Option<(Text<'static>, String)>,
    /// Floor for the popup's height, so a short dialog still reads as a dialog.
    min_height: u16,
}

fn render_settings_dialog(frame: &mut Frame, dialog: SettingsDialog) {
    let SettingsDialog {
        text,
        title,
        focus_line,
        help,
        min_height,
    } = dialog;
    let height = (text.lines.len() as u16 + 2).max(min_height);
    let (area, help_area) =
        subtitle_settings_dialog_areas(frame.area(), height, help.as_ref().map(|(text, _)| text));

    let visible_lines = area.height.saturating_sub(2) as usize;
    let scroll = (focus_line + 1).saturating_sub(visible_lines.saturating_sub(1)) as u16;

    frame.render_widget(Clear, combined_popup_area(area, help_area));
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(title),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
    if let (Some((help_text, help_title)), Some(help_area)) = (help, help_area) {
        frame.render_widget(
            Paragraph::new(help_text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Blue))
                        .title(help_title),
                )
                .wrap(Wrap { trim: false }),
            help_area,
        );
    }
}

/// Two columns for `setting_line`'s chevron, or its blank stand-in on checkbox and
/// text rows.
const FIELD_MARKER_WIDTH: usize = 2;
/// Column every row's value chrome lands on: a dropdown's `[`, a checkbox's `[x]`, or
/// a text field's frame. Sized for the marker plus the longest label ("Hearing
/// impaired", 16) plus a two-space gutter. One grid shared by all three row builders
/// is what keeps the settings popups aligned.
const FIELD_VALUE_COLUMN: usize = FIELD_MARKER_WIDTH + 18;
const SUBTITLE_SETTINGS_WIDTH: u16 = 86;
const SUBTITLE_HELP_WIDTH: u16 = 44;
const SUBTITLE_HELP_GAP: u16 = 2;

fn subtitle_field_help_title(field: SubtitleSettingsField) -> String {
    format!(" Information about {} ", field.label())
}

fn subtitle_field_help_text(app: &App, popup: &SubtitleSettingsPopup) -> Text<'static> {
    let mut paragraphs = vec![(
        match popup.field {
            SubtitleSettingsField::Codec => {
                "Sets the subtitle format written to the output. Text formats remain searchable and editable; image formats preserve rendered graphics. Converting an image subtitle to text requires seconv and tesseract to be in your PATH."
            }
            SubtitleSettingsField::Language => {
                "Identifies the subtitle language to players and media libraries. It changes metadata only; it does not translate the subtitle text."
            }
            SubtitleSettingsField::Title => {
                "An optional name shown by players, such as “English SDH” or “Director commentary.” Use it to distinguish subtitle tracks that otherwise look alike."
            }
            SubtitleSettingsField::Default => {
                "Marks this as the subtitle track a player should prefer automatically.\n\nReel only allows 1 default flag, although technically you're allowed multiple default flags. Reel will automatically toggle other default track(s) when you mark something as default."
            }
            SubtitleSettingsField::Forced => {
                "Marks subtitles containing essential dialogue that should appear when ordinary subtitles are off, such as foreign-language lines. It does not make the whole track permanently visible."
            }
            SubtitleSettingsField::Cc => {
                "Marks the track as closed captions: a transcription of speech and relevant sounds. This flag is metadata only, it does not add missing cues."
            }
            SubtitleSettingsField::HearingImpaired => {
                "Marks the track as intended for deaf or hard-of-hearing viewers, usually with speaker labels and sound cues. This flag is metadata only, it does not change the subtitle content."
            }
            SubtitleSettingsField::Original => {
                "Marks the subtitle as matching the work’s original language. This means it's the same language as the file was originally recorded in."
            }
            SubtitleSettingsField::Commentary => {
                "Marks the track as commentary or annotation rather than the main dialogue subtitles, for example director commentary."
            }
        }
        .to_string(),
        Style::default().fg(Color::White),
    )];
    let state = app.subtitle_display_state(&popup.source, popup.source_format);
    let external_sidecar = matches!(popup.source, SubtitleSource::Sidecar(_))
        && state.as_ref().is_some_and(|state| state.external);
    let context = match popup.field {
        SubtitleSettingsField::Language if external_sidecar => Some(
            "For an external sidecar, Reel also writes the canonical language code into its file name."
                .to_string(),
        ),
        SubtitleSettingsField::Forced if external_sidecar => Some(
            "For an external sidecar, Reel represents this choice with the .forced file name marker."
                .to_string(),
        ),
        SubtitleSettingsField::Cc if !external_sidecar => state.as_ref().map(|_| {
            "External subtitle files use SDH (hearing impaired) in place of CC. If this track is exported, CC will be stored as SDH.".to_string()
        }),
        SubtitleSettingsField::HearingImpaired if external_sidecar => {
            let cc_folded = state.as_ref().is_some_and(|state| {
                state.original_metadata().cc
            });
            Some(if cc_folded {
                "For an external sidecar, Reel represents this choice with the .sdh file name marker. The original CC flag has been folded into this field.".to_string()
            } else {
                "For an external sidecar, Reel represents this choice with the canonical .sdh file name marker.".to_string()
            })
        }
        _ => None,
    };
    if let Some(context) = context {
        paragraphs.push((context, Style::default().fg(Color::Gray)));
    }
    if let Some(reason) = app.subtitle_field_reason(popup.field) {
        paragraphs.push((
            format!("Unavailable: {reason}"),
            Style::default().fg(Color::Yellow),
        ));
    }

    help_paragraphs(paragraphs)
}

/// Lays help paragraphs out with a blank line between them. A paragraph may itself
/// contain a blank line, written `\n\n`, which reads as a break of its own — that is how
/// a field adds a caveat without its caller having to know how many paragraphs it has.
fn help_paragraphs(paragraphs: Vec<(String, Style)>) -> Text<'static> {
    let mut lines = Vec::new();
    for (text, style) in paragraphs {
        for paragraph in text.split("\n\n") {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::styled(paragraph.to_string(), style));
        }
    }
    padded_popup_text(Text::from(lines))
}

fn subtitle_settings_dialog_areas(
    frame_area: Rect,
    editor_height: u16,
    help_text: Option<&Text<'_>>,
) -> (Rect, Option<Rect>) {
    let editor = centered_fixed(frame_area, SUBTITLE_SETTINGS_WIDTH, editor_height);
    let Some(help_text) = help_text else {
        return (editor, None);
    };
    let help_x = editor
        .x
        .saturating_add(editor.width)
        .saturating_add(SUBTITLE_HELP_GAP);
    if help_x.saturating_add(SUBTITLE_HELP_WIDTH) <= frame_area.x.saturating_add(frame_area.width) {
        // The panel matches the dialog's height, but grows past it rather than clipping
        // a help text that wraps longer — it is its own box, so it may be the taller of
        // the two.
        let height = wrapped_popup_height(help_text, SUBTITLE_HELP_WIDTH)
            .max(editor.height)
            .min(
                frame_area
                    .height
                    .saturating_sub(editor.y.saturating_sub(frame_area.y))
                    .max(1),
            );
        let help = Rect::new(help_x, editor.y, SUBTITLE_HELP_WIDTH, height);
        return (editor, Some(help));
    }

    let width = SUBTITLE_SETTINGS_WIDTH
        .min(frame_area.width.saturating_sub(2))
        .max(1);
    let help_height = wrapped_popup_height(help_text, width);
    let available_height = frame_area.height.saturating_sub(2).max(1);
    let desired_height = editor_height
        .saturating_add(1)
        .saturating_add(help_height)
        .min(available_height);
    let compound = centered_fixed(frame_area, width, desired_height);
    let gap = u16::from(compound.height >= 3);
    let usable_height = compound.height.saturating_sub(gap);
    let minimum_help = 3.min(usable_height.saturating_sub(1));
    let minimum_editor = if usable_height >= 12 {
        9
    } else {
        usable_height.saturating_sub(minimum_help)
    };
    let maximum_help = usable_height.saturating_sub(minimum_editor);
    let help_height = help_height.min(maximum_help).max(minimum_help);
    let editor_height = usable_height.saturating_sub(help_height);
    let editor = Rect::new(compound.x, compound.y, compound.width, editor_height);
    let help = Rect::new(
        compound.x,
        compound.y + editor_height + gap,
        compound.width,
        help_height,
    );
    (editor, Some(help))
}

fn combined_popup_area(editor: Rect, help: Option<Rect>) -> Rect {
    let Some(help) = help else {
        return editor;
    };
    let x = editor.x.min(help.x);
    let y = editor.y.min(help.y);
    let right = editor
        .x
        .saturating_add(editor.width)
        .max(help.x.saturating_add(help.width));
    let bottom = editor
        .y
        .saturating_add(editor.height)
        .max(help.y.saturating_add(help.height));
    Rect::new(x, y, right.saturating_sub(x), bottom.saturating_sub(y))
}

fn wrapped_popup_height(text: &Text<'_>, width: u16) -> u16 {
    let content_width = width.saturating_sub(2).max(1) as usize;
    let height = text
        .lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(content_width))
        .sum::<usize>()
        .saturating_add(2);
    u16::try_from(height).unwrap_or(u16::MAX)
}

/// What a field shows: a live editor with caret and horizontal scroll, or a static
/// value for a field that is merely selected or idle.
enum FieldValue<'a> {
    Editing(&'a TextInputState),
    Static(&'a str),
}

/// Where a field's label sits. Both chromes draw the same frame; they differ only in
/// how much room the label is given, because a pane-bottom search bar cannot afford the
/// popups' twenty-column label grid on a narrow pane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FieldChrome {
    /// A settings-popup row, whose frame lands on [`FIELD_VALUE_COLUMN`] alongside
    /// `setting_line`'s `[` and `subtitle_checkbox_line`'s `[x]`.
    Row,
    /// A pane-bottom search bar: the label is followed immediately by the frame.
    Bar,
}

/// One text field. Every editable value in the application renders through this, so
/// the caret, the horizontal scroll and the focus treatment exist in exactly one place.
struct TextField<'a> {
    label: &'a str,
    value: FieldValue<'a>,
    /// Value cells, matching the site's `TextInputConfig::width` so what is drawn and
    /// what the cursor scrolls against can never disagree.
    width: usize,
    selected: bool,
    changed: bool,
    chrome: FieldChrome,
    /// Shown in place of an empty value while the field is idle.
    placeholder: Option<&'a str>,
    /// Trailing dim text, such as a match count.
    suffix: Option<String>,
    /// Why the field is unavailable; also renders it disabled.
    reason: Option<&'a str>,
    /// Why the last keystroke did not land, if it did not.
    reject: Option<InputReject>,
}

impl<'a> TextField<'a> {
    fn new(label: &'a str, value: FieldValue<'a>, width: usize) -> Self {
        Self {
            label,
            value,
            width,
            selected: false,
            changed: false,
            chrome: FieldChrome::Row,
            placeholder: None,
            suffix: None,
            reason: None,
            reject: None,
        }
    }

    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn changed(mut self, changed: bool) -> Self {
        self.changed = changed;
        self
    }

    fn bar(mut self) -> Self {
        self.chrome = FieldChrome::Bar;
        self
    }

    fn placeholder(mut self, placeholder: &'a str) -> Self {
        self.placeholder = Some(placeholder);
        self
    }

    fn suffix(mut self, suffix: String) -> Self {
        self.suffix = Some(suffix);
        self
    }

    fn reason(mut self, reason: Option<&'a str>) -> Self {
        self.reason = reason;
        self
    }

    fn reject(mut self, reject: Option<InputReject>) -> Self {
        self.reject = reject;
        self
    }
}

/// What to tell the user about a keystroke the field refused. Phrased as the rule that
/// was broken, since the character they typed is already gone from the screen.
fn reject_message(reject: InputReject) -> String {
    match reject {
        InputReject::Character(CharClass::Digits) => "digits only".to_string(),
        InputReject::Character(CharClass::Word) => "no spaces".to_string(),
        InputReject::Character(CharClass::Text) => "unsupported character".to_string(),
        InputReject::Full(max_len) => format!("{max_len} character limit"),
    }
}

/// The frame every field is drawn with. One glyph in every state: focus is carried by
/// colour alone, so a selected row does not change width or weight, and no terminal has
/// to agree with us about how wide a heavy box-drawing character is.
const FIELD_FRAME: &str = "│";
/// Caret glyph shown inside the field while it is being edited.
const FIELD_CARET: &str = "▏";
/// Replaces the frame on whichever side the value continues past, so a scrolled or
/// truncated value is visibly cut off rather than looking complete.
const FIELD_OVERFLOW: &str = "…";
/// Surface painted behind a value *only* while it is being edited. Idle fields stay
/// unfilled so a column of stacked fields reads as separate rows rather than one slab.
const FIELD_EDITING_SURFACE: Color = Color::Rgb(32, 32, 32);

/// The leading `marker + label` span shared by every settings row. Padding both parts
/// here is what puts each row's value chrome on [`FIELD_VALUE_COLUMN`], whether that
/// chrome is a dropdown's `[`, a checkbox's `[x]`, or a text field's frame.
fn field_label_span(marker: &str, label: &str, style: Style) -> Span<'static> {
    let prefix = format!(
        "{marker:<marker_width$}{label}",
        marker_width = FIELD_MARKER_WIDTH
    );
    let padding = FIELD_VALUE_COLUMN.saturating_sub(UnicodeWidthStr::width(prefix.as_str()));
    Span::styled(format!("{prefix}{blank:padding$}", blank = ""), style)
}

/// Takes characters from `start` while they fit in `budget` display columns. Returning
/// the columns used lets the caller pad the field to a fixed width even when the value
/// contains double-width glyphs, which is what keeps the closing frame in its column.
fn take_columns(characters: &[char], start: usize, budget: usize) -> (String, usize) {
    let mut taken = String::new();
    let mut columns = 0;
    for character in characters.iter().skip(start) {
        let width = UnicodeWidthChar::width(*character).unwrap_or(0);
        if columns + width > budget {
            break;
        }
        taken.push(*character);
        columns += width;
    }
    (taken, columns)
}

fn text_field_line(field: TextField<'_>) -> Line<'static> {
    let TextField {
        label,
        value,
        width,
        selected,
        changed,
        chrome,
        placeholder,
        suffix,
        reason,
        reject,
    } = field;
    let enabled = reason.is_none();
    let editing = matches!(&value, FieldValue::Editing(input) if input.is_active);
    let reject = reject.filter(|_| editing);

    let mut value_style = if !enabled {
        Style::default().fg(Color::DarkGray)
    } else if changed {
        changed_style()
    } else {
        Style::default().fg(Color::White)
    };
    if editing {
        value_style = value_style.bg(FIELD_EDITING_SURFACE);
    }

    // One column is always reserved for the caret so the value window does not shift
    // when editing starts.
    let budget = width.saturating_sub(1);
    let (before, after, used, overflow) = match &value {
        FieldValue::Editing(input) => {
            let characters = input.value.chars().collect::<Vec<_>>();
            let start = if input.is_active {
                input.view_offset
            } else {
                0
            }
            .min(characters.len());
            let (window, columns) = take_columns(&characters, start, budget);
            let window = window.chars().collect::<Vec<_>>();
            let caret = input.cursor.saturating_sub(start).min(window.len());
            (
                window[..caret].iter().collect::<String>(),
                window[caret..].iter().collect::<String>(),
                columns,
                Overflow {
                    before: start > 0,
                    after: start + window.len() < characters.len(),
                },
            )
        }
        FieldValue::Static(text) => {
            let characters = text.chars().collect::<Vec<_>>();
            let (window, columns) = take_columns(&characters, 0, budget);
            let overflow = Overflow {
                before: false,
                after: window.chars().count() < characters.len(),
            };
            (window, String::new(), columns, overflow)
        }
    };

    let placeholder = placeholder.filter(|_| before.is_empty() && after.is_empty());

    let mut spans = Vec::new();
    let label_style = Style::default().fg(if selected { Color::Cyan } else { Color::Gray });
    let mut frame_style = if reject.is_some() {
        Style::default().fg(Color::Red).bold()
    } else if selected && enabled {
        Style::default().fg(Color::Cyan).bold()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    // The frames sit *on* the editing surface rather than beside it, so the filled
    // band runs bar to bar with no unpainted cell at either end.
    if editing {
        frame_style = frame_style.bg(FIELD_EDITING_SURFACE);
    }
    let frame = FIELD_FRAME;
    // The overflow marker takes the frame's cell rather than a cell of its own, so a
    // value that starts scrolling does not shift the column the frame sits in.
    let opening = if overflow.before {
        FIELD_OVERFLOW
    } else {
        frame
    };
    let closing = if overflow.after {
        FIELD_OVERFLOW
    } else {
        frame
    };

    match chrome {
        FieldChrome::Row => spans.push(field_label_span("", label, label_style)),
        FieldChrome::Bar => spans.push(Span::styled(
            format!("  {label} "),
            Style::default().fg(Color::Cyan),
        )),
    }
    spans.push(Span::styled(opening.to_string(), frame_style));
    spans.push(Span::styled(" ".to_string(), value_style));

    let caret_columns = if editing { 1 } else { 0 };
    spans.push(Span::styled(before, value_style));
    if editing {
        spans.push(Span::styled(
            FIELD_CARET.to_string(),
            value_style.fg(Color::Cyan),
        ));
    }
    spans.push(Span::styled(after, value_style));
    let mut filled = used + caret_columns;
    if let Some(placeholder) = placeholder {
        let (text, columns) = take_columns(
            &placeholder.chars().collect::<Vec<_>>(),
            0,
            width.saturating_sub(filled),
        );
        filled += columns;
        spans.push(Span::styled(
            text,
            value_style.fg(Color::DarkGray).italic().not_bold(),
        ));
    }
    spans.push(Span::styled(
        " ".repeat(width.saturating_sub(filled)),
        value_style,
    ));
    spans.push(Span::styled(" ".to_string(), value_style));
    spans.push(Span::styled(closing.to_string(), frame_style));
    if let Some(suffix) = suffix {
        spans.push(Span::styled(suffix, Style::default().fg(Color::DarkGray)));
    }
    if let Some(reason) = reason {
        spans.push(Span::styled(
            format!("  {reason}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(reject) = reject {
        spans.push(Span::styled(
            format!("  {}", reject_message(reject)),
            Style::default().fg(Color::Red),
        ));
    }
    Line::from(spans)
}

/// Which sides of a field's value window have text the window does not show.
struct Overflow {
    before: bool,
    after: bool,
}

/// Match-count suffix, worded identically by all three search bars.
fn match_suffix(count: usize) -> String {
    match count {
        0 => " (no matches)".to_string(),
        1 => " (1 match)".to_string(),
        count => format!(" ({count} matches)"),
    }
}

fn subtitle_checkbox_line(
    label: &str,
    checked: bool,
    selected: bool,
    changed: bool,
    reason: Option<&str>,
) -> Line<'static> {
    let enabled = reason.is_none() || checked;
    let box_style = if !enabled {
        Style::default().fg(Color::DarkGray)
    } else if selected {
        focused_style(changed)
    } else if changed {
        changed_style()
    } else {
        Style::default().fg(Color::White)
    };
    let mut spans = vec![
        field_label_span(
            "",
            label,
            Style::default().fg(if selected { Color::Cyan } else { Color::Gray }),
        ),
        Span::styled(if checked { "[x]" } else { "[ ]" }, box_style),
    ];
    if let Some(reason) = reason {
        spans.push(Span::styled(
            format!("  {reason}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn setting_line(
    label: &str,
    value: &str,
    selected: bool,
    changed: bool,
    expanded: bool,
) -> Line<'static> {
    let value_style = if selected {
        focused_style(changed)
    } else if changed {
        changed_style()
    } else {
        Style::default()
    };
    let marker = if expanded { "▿" } else { "▹" };
    Line::from(vec![
        field_label_span(
            marker,
            label,
            Style::default().fg(if selected { Color::Cyan } else { Color::Gray }),
        ),
        Span::styled(format!("[ {value} ]"), value_style),
    ])
}

/// Tree-guide prefix for a row nested under an expanded dropdown field, matching the
/// fold connectors used by `file_tree_lines` for sidecar subtitles.
fn tree_guide_span(last: bool) -> Span<'static> {
    let guide = if last { "  └── " } else { "  ├── " };
    Span::styled(guide, Style::default().fg(Color::DarkGray))
}

fn subtitle_codec_line(
    label: &str,
    cursor: bool,
    staged: bool,
    enabled: bool,
    last: bool,
) -> Line<'static> {
    let label_style = choice_style(cursor, staged, enabled);
    let spans = vec![
        tree_guide_span(last),
        Span::styled(label.to_string(), label_style),
    ];
    // Base the line on the label's own style so the guide glyph inherits the cursor
    // row's highlight background rather than leaving a gap at the start of the bar.
    Line::from(spans).style(label_style)
}

fn dropdown_line(
    label: &str,
    cursor: bool,
    selected: bool,
    enabled: bool,
    staged: bool,
    last: bool,
) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    let line = Line::from(vec![
        tree_guide_span(last),
        Span::raw(format!("{marker} {label}")),
    ]);
    line.style(choice_style(cursor, staged, enabled))
}

fn render_details_popup(frame: &mut Frame, app: &mut App) {
    let Some((text, title)) = details_popup_content(app) else {
        return;
    };
    let text = padded_popup_text(text);
    let area = details_popup_area(frame.area(), &text, &title);
    app.set_details_max_scroll(max_scroll(&text, area));

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(title),
            )
            .wrap(Wrap { trim: false })
            .scroll((app.details_scroll, 0)),
        area,
    );
}

fn details_popup_content(app: &App) -> Option<(Text<'static>, String)> {
    let info = app.media_info()?;
    match app.selected_track()? {
        TrackRef::Container => {
            let file = app.selected_file()?;
            Some((
                Text::from(container_information_lines(
                    info,
                    &file.path,
                    file.fingerprint.length,
                    app.effective_container_metadata().as_ref(),
                    &|field| app.container_field_changed(field),
                )),
                " Container information ".to_string(),
            ))
        }
        TrackRef::Embedded(index) => {
            let stream = info
                .streams
                .iter()
                .find(|stream| stream_index(stream) == Some(index))?;
            let default = app.default_streams.contains(&index);
            let index_label = number_string(stream, "index").unwrap_or_else(|| index.to_string());
            let kind = string(stream, "codec_type").unwrap_or("unknown");
            if kind == "video" {
                return Some((
                    Text::from(video_information_lines(stream, default)),
                    format!(" Video #{index_label} "),
                ));
            }
            if kind == "audio" {
                return Some((
                    Text::from(audio_information_lines(stream, default)),
                    format!(" Audio #{index_label} "),
                ));
            }
            if kind == "subtitle" {
                let source = SubtitleSource::Embedded(index);
                let source_format =
                    SubtitleFormat::from_codec(string(stream, "codec_name").unwrap_or("unknown"));
                let state =
                    source_format.and_then(|format| app.subtitle_display_state(&source, format));
                return Some((
                    Text::from(embedded_subtitle_information_lines(
                        stream,
                        default,
                        state.as_ref(),
                    )),
                    format!(" Subtitle #{index_label} "),
                ));
            }
            // `track_rows` only ever offers video, audio and subtitle rows, so there is
            // no other kind to describe here.
            None
        }
        TrackRef::Sidecar(index) => {
            let sidecar = app.sidecars.get(index)?;
            let source = SubtitleSource::Sidecar(sidecar.path.clone());
            let state = app.subtitle_display_state(&source, sidecar.format);
            Some((
                Text::from(sidecar_subtitle_information_lines(sidecar, state.as_ref())),
                " External subtitle ".to_string(),
            ))
        }
    }
}

fn video_information_lines(stream: &BTreeMap<String, Value>, default: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let title = tag(stream, "title")
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Not provided");
    append_information_group(&mut lines, vec![field_line(0, "Title", title)]);

    let mut language_and_role = Vec::new();
    if let Some(language) = tag(stream, "language").and_then(language_name) {
        language_and_role.push(field_line(0, "Language", &language));
    }
    let roles = video_roles(stream, default);
    if !roles.is_empty() {
        language_and_role.push(field_line(0, "Role", &roles.join(" · ")));
    }
    append_information_group(&mut lines, language_and_role);

    let mut technical = vec![field_line(0, "Format", &video_format_description(stream))];
    if let Some(resolution) = video_resolution_description(stream) {
        technical.push(field_line(0, "Resolution", &resolution));
    }
    if let Some(frame_rate) = string(stream, "avg_frame_rate")
        .or_else(|| string(stream, "r_frame_rate"))
        .and_then(format_frame_rate)
    {
        technical.push(field_line(0, "Frame rate", &format!("{frame_rate} fps")));
    }

    let mut picture = Vec::new();
    if let Some(range) = video_dynamic_range(stream) {
        picture.push(range);
    }
    if let Some(depth) = video_bit_depth(stream) {
        picture.push(format!("{depth}-bit"));
    }
    if let Some(scan) = video_scan_type(stream) {
        picture.push(scan.to_string());
    }
    if !picture.is_empty() {
        technical.push(field_line(0, "Picture", &picture.join(" · ")));
    }

    if let Some(bit_rate) = number_string(stream, "bit_rate").and_then(parse_number) {
        technical.push(field_line(0, "Bitrate", &format_bitrate(bit_rate)));
    }
    append_information_group(&mut lines, technical);

    lines
}

fn video_format_description(stream: &BTreeMap<String, Value>) -> String {
    match string(stream, "codec_name").unwrap_or("unknown") {
        "h264" => "H.264 (AVC)".to_string(),
        "hevc" => "HEVC (H.265)".to_string(),
        "av1" => "AV1".to_string(),
        "vp9" => "VP9".to_string(),
        "vp8" => "VP8".to_string(),
        "mpeg4" => "MPEG-4 Visual".to_string(),
        "mpeg2video" => "MPEG-2 Video".to_string(),
        "prores" => "Apple ProRes".to_string(),
        "mjpeg" => "Motion JPEG".to_string(),
        "unknown" => "Unknown".to_string(),
        codec => codec.to_ascii_uppercase(),
    }
}

fn video_resolution_description(stream: &BTreeMap<String, Value>) -> Option<String> {
    let width = number_string(stream, "width")?.parse::<u64>().ok()?;
    let height = number_string(stream, "height")?.parse::<u64>().ok()?;
    let mut parts = vec![format!("{width}×{height}")];
    if let Some(aspect_ratio) = string(stream, "display_aspect_ratio")
        .filter(|aspect_ratio| !matches!(*aspect_ratio, "0:1" | "N/A"))
    {
        parts.push(aspect_ratio.to_string());
    }
    if let Some(tier) = match height {
        4320 => Some("8K"),
        2160 => Some("4K"),
        1440 => Some("1440p"),
        1080 => Some("1080p"),
        720 => Some("720p"),
        576 => Some("576p"),
        480 => Some("480p"),
        _ => None,
    } {
        parts.push(tier.to_string());
    }
    Some(parts.join(" · "))
}

fn video_dynamic_range(stream: &BTreeMap<String, Value>) -> Option<String> {
    match string(stream, "color_transfer")? {
        "smpte2084" => Some("HDR10".to_string()),
        "arib-std-b67" => Some("HLG HDR".to_string()),
        "bt709" | "gamma22" | "gamma28" | "smpte170m" | "bt470bg" => Some("SDR".to_string()),
        _ => None,
    }
}

fn video_bit_depth(stream: &BTreeMap<String, Value>) -> Option<u8> {
    if let Some(depth) = number_string(stream, "bits_per_raw_sample")
        .and_then(|depth| depth.parse::<u8>().ok())
        .filter(|depth| *depth > 0)
    {
        return Some(depth);
    }
    let pixel_format = string(stream, "pix_fmt")?;
    for (marker, depth) in [
        ("p016", 16),
        ("p014", 14),
        ("p012", 12),
        ("p010", 10),
        ("p16", 16),
        ("p14", 14),
        ("p12", 12),
        ("p10", 10),
        ("p9", 9),
    ] {
        if pixel_format.contains(marker) {
            return Some(depth);
        }
    }
    matches!(
        pixel_format,
        "yuv420p"
            | "yuv422p"
            | "yuv444p"
            | "yuvj420p"
            | "yuvj422p"
            | "yuvj444p"
            | "nv12"
            | "nv21"
            | "rgb24"
            | "bgr24"
            | "rgba"
            | "bgra"
            | "gray"
    )
    .then_some(8)
}

fn video_scan_type(stream: &BTreeMap<String, Value>) -> Option<&'static str> {
    match string(stream, "field_order")? {
        "progressive" => Some("Progressive"),
        "tt" | "bb" | "tb" | "bt" => Some("Interlaced"),
        _ => None,
    }
}

fn video_roles(stream: &BTreeMap<String, Value>, default: bool) -> Vec<String> {
    let mut roles = Vec::new();
    if default {
        roles.push("Default".to_string());
    }
    for (key, label) in [
        ("forced", "Forced"),
        ("hearing_impaired", "Hearing Impaired"),
        ("visual_impaired", "Visual Impaired"),
        ("comment", "Commentary"),
        ("dub", "Dub"),
    ] {
        if disposition_enabled(stream, key) {
            roles.push(label.to_string());
        }
    }
    if disposition_enabled(stream, "original") {
        roles.push("Original".to_string());
    }
    if crate::probe::is_attached_picture(stream) {
        roles.push("Cover art".to_string());
    }
    roles
}

fn language_name(value: &str) -> Option<String> {
    let value = value.trim();
    let normalized = value.to_ascii_lowercase();
    let code = normalized
        .split_once(['-', '_'])
        .map_or(normalized.as_str(), |(language, _)| language);
    if matches!(code, "" | "und") {
        return None;
    }
    if let Some(canonical) = canonical_language_code(code)
        && let Ok(language) = canonical.parse::<Language>()
    {
        return Some(language.to_name().to_string());
    }
    if code.len() <= 3
        && code
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        return Some(format!("Unknown language ({})", code.to_ascii_uppercase()));
    }
    Some(value.to_string())
}

fn audio_information_lines(stream: &BTreeMap<String, Value>, default: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let title = tag(stream, "title")
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Not provided");
    append_information_group(&mut lines, vec![field_line(0, "Title", title)]);

    let mut language_and_role = Vec::new();
    if let Some(language) = tag(stream, "language").and_then(language_name) {
        language_and_role.push(field_line(0, "Language", &language));
    }
    let roles = audio_roles(stream, default);
    if !roles.is_empty() {
        language_and_role.push(field_line(0, "Role", &roles.join(" · ")));
    }
    append_information_group(&mut lines, language_and_role);

    let mut technical = vec![field_line(0, "Format", &audio_format_description(stream))];
    if let Some(channels) = audio_channel_description(stream) {
        technical.push(field_line(0, "Channels", &channels));
    }
    if let Some(bit_rate) = number_string(stream, "bit_rate").and_then(parse_number) {
        technical.push(field_line(0, "Bitrate", &format_bitrate(bit_rate)));
    }
    if let Some(sample_rate) = number_string(stream, "sample_rate").and_then(parse_number) {
        technical.push(field_line(
            0,
            "Sample rate",
            &format_sample_rate(sample_rate),
        ));
    }
    append_information_group(&mut lines, technical);

    lines
}

fn audio_format_description(stream: &BTreeMap<String, Value>) -> String {
    let codec = string(stream, "codec_name").unwrap_or("unknown");
    match codec {
        "aac" => "AAC".to_string(),
        "ac3" => "Dolby Digital (AC-3)".to_string(),
        "eac3" => "Dolby Digital Plus (E-AC-3)".to_string(),
        "truehd" => "Dolby TrueHD".to_string(),
        "dts" => match string(stream, "profile") {
            Some("DTS-HD MA") => "DTS-HD Master Audio".to_string(),
            Some("DTS-HD HRA") => "DTS-HD High Resolution Audio".to_string(),
            _ => "DTS".to_string(),
        },
        "opus" => "Opus".to_string(),
        "vorbis" => "Vorbis".to_string(),
        "flac" => "FLAC · Lossless".to_string(),
        "alac" => "ALAC · Lossless".to_string(),
        "mp3" => "MP3".to_string(),
        "unknown" => "Unknown".to_string(),
        codec if codec.starts_with("pcm_") => "PCM · Uncompressed".to_string(),
        codec => codec.to_ascii_uppercase(),
    }
}

fn audio_channel_description(stream: &BTreeMap<String, Value>) -> Option<String> {
    if let Some(layout) = string(stream, "channel_layout") {
        let base = layout.split_once('(').map_or(layout, |(base, _)| base);
        let description = match base {
            "mono" => Some("Mono".to_string()),
            "stereo" => Some("Stereo".to_string()),
            "quad" => Some("Quadraphonic".to_string()),
            value
                if value
                    .chars()
                    .all(|character| character.is_ascii_digit() || character == '.') =>
            {
                Some(format!("{value} surround"))
            }
            _ => None,
        };
        if description.is_some() {
            return description;
        }
    }

    match number_string(stream, "channels")?.parse::<u64>().ok()? {
        1 => Some("Mono".to_string()),
        2 => Some("Stereo".to_string()),
        channels => Some(format!("{channels} channels")),
    }
}

fn audio_roles(stream: &BTreeMap<String, Value>, default: bool) -> Vec<String> {
    let mut roles = Vec::new();
    if default {
        roles.push("Default".to_string());
    }
    for (key, label) in [
        ("forced", "Forced"),
        ("hearing_impaired", "Hearing impaired"),
        ("visual_impaired", "Audio description"),
        ("comment", "Commentary"),
        ("dub", "Dubbed"),
        ("original", "Original"),
    ] {
        if disposition_enabled(stream, key) {
            roles.push(label.to_string());
        }
    }
    roles
}

fn embedded_subtitle_information_lines(
    stream: &BTreeMap<String, Value>,
    default: bool,
    state: Option<&SubtitleDisplayState>,
) -> Vec<Line<'static>> {
    let codec = string(stream, "codec_name").unwrap_or("unknown");
    let metadata = state.map_or_else(
        || crate::subtitle::SubtitleMetadata {
            language: stream_language(stream),
            title: stream_title(stream),
            forced: stream_forced(stream),
            cc: stream_cc(stream),
            hearing_impaired: stream_hearing_impaired(stream),
            original: stream_original(stream),
            commentary: stream_commentary(stream),
        },
        |state| state.metadata.clone(),
    );
    let format = state
        .map(|state| state.format)
        .or_else(|| SubtitleFormat::from_codec(codec));
    subtitle_information_lines(
        None,
        &metadata,
        state.map_or(default, |state| state.default),
        state.is_some_and(|state| state.external),
        format,
        codec,
        state,
    )
}

fn sidecar_subtitle_information_lines(
    sidecar: &SidecarEntry,
    state: Option<&SubtitleDisplayState>,
) -> Vec<Line<'static>> {
    let metadata = state.map_or_else(
        || crate::subtitle::SubtitleMetadata {
            language: sidecar.language.clone(),
            title: None,
            forced: sidecar.forced,
            cc: false,
            hearing_impaired: sidecar.hearing_impaired,
            original: false,
            commentary: false,
        },
        |state| state.metadata.clone(),
    );
    subtitle_information_lines(
        Some(&sidecar.display_name),
        &metadata,
        state.is_some_and(|state| state.default),
        state.is_none_or(|state| state.external),
        Some(state.map_or(sidecar.format, |state| state.format)),
        sidecar.format.ffmpeg_codec(),
        state,
    )
}

#[allow(clippy::too_many_arguments)]
fn subtitle_information_lines(
    file: Option<&str>,
    metadata: &crate::subtitle::SubtitleMetadata,
    default: bool,
    external: bool,
    format: Option<SubtitleFormat>,
    codec: &str,
    state: Option<&SubtitleDisplayState>,
) -> Vec<Line<'static>> {
    let changed = |field| state.is_some_and(|state| state.field_changed(field));
    let visible = |field| {
        state.map_or_else(
            || {
                !external
                    || !matches!(
                        field,
                        SubtitleSettingsField::Title
                            | SubtitleSettingsField::Default
                            | SubtitleSettingsField::Cc
                            | SubtitleSettingsField::Original
                            | SubtitleSettingsField::Commentary
                    )
            },
            |state| state.field_visible(field),
        )
    };
    let mut lines = Vec::new();

    if let Some(file) = file {
        append_information_group(&mut lines, vec![field_line(0, "File", file)]);
    }
    if visible(SubtitleSettingsField::Title) {
        append_information_group(
            &mut lines,
            vec![information_field_line(
                "Title",
                metadata.title.as_deref().unwrap_or("Not provided"),
                changed(SubtitleSettingsField::Title),
            )],
        );
    }

    let language = subtitle_language_description(&metadata.language)
        .unwrap_or_else(|| "Not provided".to_string());
    let mut language_and_flags = vec![information_field_line(
        "Language",
        &language,
        changed(SubtitleSettingsField::Language),
    )];
    // Every flag states itself, set or not, in the order the settings dialog lists
    // them. A run of names told you what was on but never what was off, so "no
    // Commentary" and "Commentary not applicable here" looked identical.
    for (field, label, enabled) in [
        (SubtitleSettingsField::Default, "Default", default),
        (SubtitleSettingsField::Forced, "Forced", metadata.forced),
        (SubtitleSettingsField::Cc, "Closed captions", metadata.cc),
        (
            SubtitleSettingsField::HearingImpaired,
            "Hearing impaired",
            metadata.hearing_impaired,
        ),
        (
            SubtitleSettingsField::Original,
            "Original",
            metadata.original,
        ),
        (
            SubtitleSettingsField::Commentary,
            "Commentary",
            metadata.commentary,
        ),
    ] {
        if visible(field) {
            language_and_flags.push(information_field_line(
                label,
                if enabled { "Yes" } else { "No" },
                changed(field),
            ));
        }
    }
    append_information_group(&mut lines, language_and_flags);

    let mut format_and_type = vec![information_field_line(
        "Format",
        &subtitle_information_format(format, codec),
        changed(SubtitleSettingsField::Codec),
    )];
    if let Some(format) = format {
        format_and_type.push(field_line(
            0,
            "Type",
            if format.is_text() {
                "Text-based"
            } else {
                "Image-based"
            },
        ));
    }
    append_information_group(&mut lines, format_and_type);
    lines
}

/// A `Key: Value` row whose value turns yellow when the edit is staged.
fn information_field_line(key: &str, value: &str, changed: bool) -> Line<'static> {
    let value_style = if changed {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::ITALIC)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("{key}: "), Style::default().fg(Color::Blue).bold()),
        Span::styled(value.to_string(), value_style),
    ])
}

fn append_information_group(lines: &mut Vec<Line<'static>>, group: Vec<Line<'static>>) {
    if group.is_empty() {
        return;
    }
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.extend(group);
}

fn subtitle_information_format(format: Option<SubtitleFormat>, codec: &str) -> String {
    match format {
        Some(SubtitleFormat::SubRip) => "SubRip (SRT)".to_string(),
        Some(format) => format.label().to_string(),
        None => match codec {
            "eia_608" => "CEA-608 Closed Captions".to_string(),
            "eia_708" => "CEA-708 Closed Captions".to_string(),
            "unknown" => "Unknown".to_string(),
            codec => codec.to_ascii_uppercase(),
        },
    }
}

fn subtitle_language_description(value: &str) -> Option<String> {
    language_name(value)
}

fn disposition_enabled(stream: &BTreeMap<String, Value>, key: &str) -> bool {
    stream
        .get("disposition")
        .and_then(Value::as_object)
        .and_then(|disposition| disposition.get(key))
        .and_then(Value::as_i64)
        == Some(1)
}

fn container_information_lines(
    info: &MediaInfo,
    path: &Path,
    fallback_size: u64,
    metadata: Option<&crate::edit::ContainerMetadata>,
    changed: &dyn Fn(ContainerSettingsField) -> bool,
) -> Vec<Line<'static>> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown".to_string());
    let size = number_string(&info.format, "size")
        .and_then(parse_number)
        .unwrap_or(fallback_size as f64);
    let duration = number_string(&info.format, "duration")
        .and_then(parse_number)
        .map(format_duration_24h)
        .unwrap_or_else(|| "Unknown".to_string());
    let format = container_format_description(info, path);

    let mut lines = Vec::new();
    append_information_group(
        &mut lines,
        vec![
            field_line(0, "File name", &file_name),
            field_line(0, "Path", &path.to_string_lossy()),
        ],
    );
    append_information_group(
        &mut lines,
        vec![
            field_line(0, "Duration", &duration),
            field_line(0, "Size", &format_bytes(size)),
        ],
    );
    append_information_group(&mut lines, vec![field_line(0, "Format", &format)]);

    // The metadata the container settings dialog edits, shown whether or not it is set,
    // so the panel answers "what would I be changing" as well as "what is there".
    let metadata = metadata.cloned().unwrap_or_default();
    append_information_group(
        &mut lines,
        [
            (ContainerSettingsField::Title, "Title", &metadata.title),
            (
                ContainerSettingsField::Comment,
                "Comment",
                &metadata.comment,
            ),
            (ContainerSettingsField::Date, "Date", &metadata.date),
            (ContainerSettingsField::Genre, "Genre", &metadata.genre),
            (ContainerSettingsField::Artist, "Artist", &metadata.artist),
        ]
        .into_iter()
        .map(|(field, label, value)| {
            information_field_line(
                label,
                value.as_deref().unwrap_or("Not provided"),
                changed(field),
            )
        })
        .collect(),
    );
    lines
}

fn container_format_description(info: &MediaInfo, path: &Path) -> String {
    let probed_name =
        string(&info.format, "format_long_name").or_else(|| string(&info.format, "format_name"));
    match (ContainerFormat::from_path(path), probed_name) {
        (Some(container), Some(probed_name))
            if !probed_name.eq_ignore_ascii_case(container.label()) =>
        {
            format!("{} ({probed_name})", container.label())
        }
        (Some(container), _) => container.label().to_string(),
        (None, Some(probed_name)) => probed_name.to_string(),
        (None, None) => "Unknown".to_string(),
    }
}

fn format_duration_24h(seconds: f64) -> String {
    let total = seconds.round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn padded_popup_text(mut text: Text<'static>) -> Text<'static> {
    text.lines.insert(0, Line::from(""));
    text.lines.push(Line::from(""));
    text
}

fn max_scroll(text: &Text<'_>, area: Rect) -> u16 {
    let content_width = area.width.saturating_sub(2).max(1) as usize;
    let viewport_height = area.height.saturating_sub(2) as usize;
    let rendered_lines: usize = text
        .lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(content_width))
        .sum();
    rendered_lines
        .saturating_sub(viewport_height)
        .min(u16::MAX as usize) as u16
}

fn scroll_to_show_line(text: &Text<'_>, area: Rect, line_index: usize, current: u16) -> u16 {
    let content_width = area.width.saturating_sub(2).max(1) as usize;
    let viewport_height = area.height.saturating_sub(2).max(1) as usize;
    let line_height = |line: &Line<'_>| line.width().max(1).div_ceil(content_width);
    let start: usize = text.lines.iter().take(line_index).map(line_height).sum();
    let end = start + text.lines.get(line_index).map_or(1, line_height);
    let current = current as usize;

    if start < current {
        start.min(u16::MAX as usize) as u16
    } else if end > current + viewport_height {
        end.saturating_sub(viewport_height).min(u16::MAX as usize) as u16
    } else {
        current.min(u16::MAX as usize) as u16
    }
}

fn field_line(depth: usize, key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  ".repeat(depth)),
        Span::styled(format!("{key}: "), Style::default().fg(Color::Blue).bold()),
        Span::raw(value.to_string()),
    ])
}

fn popup_area(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - height_percent) / 2),
        Constraint::Percentage(height_percent),
        Constraint::Percentage((100 - height_percent) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - width_percent) / 2),
        Constraint::Percentage(width_percent),
        Constraint::Percentage((100 - width_percent) / 2),
    ])
    .split(vertical[1])[1]
}

/// Share of the terminal every `i` panel takes, whichever track it describes. One width
/// for all of them: a panel that resized itself to its content made the container, video
/// and subtitle panels three different shapes.
const DETAILS_POPUP_WIDTH_PERCENT: u32 = 60;

/// The `i` panel: a fixed share of the width, and whatever height its text wraps to.
fn details_popup_area(area: Rect, text: &Text<'_>, title: &str) -> Rect {
    let target_width = ((u32::from(area.width) * DETAILS_POPUP_WIDTH_PERCENT) / 100) as u16;
    let title_width = u16::try_from(title.chars().count().saturating_add(4)).unwrap_or(u16::MAX);
    let width = target_width.max(title_width);
    let content_width = width.saturating_sub(2).max(1) as usize;
    let rendered_height = text
        .lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(content_width))
        .sum::<usize>()
        .saturating_add(2);
    centered_fixed(
        area,
        width,
        u16::try_from(rendered_height).unwrap_or(u16::MAX),
    )
}

fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn focus_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

fn focused_style(changed: bool) -> Style {
    let style = Style::default()
        .fg(Color::White)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    if changed {
        style.add_modifier(Modifier::ITALIC)
    } else {
        style
    }
}

/// What an overview row is currently saying about itself. Selection outranks deletion,
/// deletion outranks a container conflict, and a conflict outranks an ordinary edit —
/// one ranking, shared by the container, stream and sidecar rows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TrackRowState {
    selected: bool,
    deleted: bool,
    conflict: bool,
    changed: bool,
}

impl TrackRowState {
    /// The style of the row as a whole.
    fn line_style(self) -> Style {
        if self.selected {
            // A deleted row is not also "edited" — the deletion is the edit.
            focused_style(self.changed && !self.deleted)
        } else if self.deleted {
            Style::default().fg(Color::Red)
        } else if self.conflict {
            warning_style(self.changed)
        } else if self.changed {
            changed_style()
        } else {
            Style::default()
        }
    }

    /// The style of the leading `×`/`⚠`/`~` marker. Bold, and never restyled while
    /// selected so the selection colour stays uniform across the row.
    fn marker_style(self) -> Style {
        if self.selected {
            Style::default()
        } else if self.deleted {
            Style::default().fg(Color::Red).bold()
        } else if self.conflict {
            warning_style(false).bold()
        } else if self.changed {
            changed_style().bold()
        } else {
            Style::default()
        }
    }

    /// The style of the `#index` column, which is dimmed when nothing else applies.
    fn index_style(self) -> Style {
        if self.selected {
            Style::default()
        } else if self.deleted {
            Style::default().fg(Color::Red)
        } else if self.conflict {
            warning_style(false)
        } else if self.changed {
            changed_style()
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }
}

fn changed_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::ITALIC)
}

fn warning_style(changed: bool) -> Style {
    let style = Style::default().fg(Color::Yellow);
    if changed {
        style.add_modifier(Modifier::ITALIC)
    } else {
        style
    }
}

fn choice_style(cursor: bool, changed: bool, enabled: bool) -> Style {
    if !enabled {
        Style::default().fg(Color::DarkGray)
    } else if cursor {
        focused_style(changed)
    } else if changed {
        changed_style()
    } else {
        Style::default().fg(Color::White)
    }
}

fn string<'a>(map: &'a std::collections::BTreeMap<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

fn number_string(map: &std::collections::BTreeMap<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(|value| match value {
        Value::Number(number) => Some(number.to_string()),
        Value::String(value) => Some(value.clone()),
        _ => None,
    })
}

fn tag<'a>(stream: &'a std::collections::BTreeMap<String, Value>, key: &str) -> Option<&'a str> {
    stream
        .get("tags")
        .and_then(Value::as_object)
        .and_then(|tags| tags.get(key))
        .and_then(Value::as_str)
}

/// A video or audio track's flags, written the way a subtitle track writes its own:
/// one bracketed group of short codes rather than a run of separate `[word]` tags.
fn disposition_flag_tag(
    stream: &std::collections::BTreeMap<String, Value>,
    default: bool,
) -> Option<String> {
    const FLAGS: [(&str, &str); 5] = [
        ("forced", "F"),
        ("hearing_impaired", "HI"),
        ("visual_impaired", "VI"),
        ("comment", "CM"),
        ("dub", "DUB"),
    ];

    let mut active = Vec::new();
    if default {
        active.push("D");
    }
    if let Some(disposition) = stream.get("disposition").and_then(Value::as_object) {
        active.extend(
            FLAGS
                .iter()
                .filter(|(key, _)| disposition.get(*key).and_then(Value::as_i64) == Some(1))
                .map(|(_, code)| *code),
        );
    }
    (!active.is_empty()).then(|| format!("[{}]", active.join("/")))
}

/// A language tag written out, so an overview row reads "Korean" rather than "KOR".
/// Codes the language table does not know fall back to the tag itself, upper-cased.
fn language_display_name(code: &str) -> String {
    crate::subtitle::language_choice(code)
        .map(|choice| choice.name)
        .unwrap_or_else(|| code.to_uppercase())
}

fn parse_number(value: String) -> Option<f64> {
    value.parse().ok()
}

fn format_duration(seconds: f64) -> String {
    let total = seconds.round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn format_bytes(bytes: f64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_bitrate(bits: f64) -> String {
    if bits >= 1_000_000.0 {
        format!("{:.1} Mb/s", bits / 1_000_000.0)
    } else {
        format!("{:.0} kb/s", bits / 1_000.0)
    }
}

fn format_sample_rate(hertz: f64) -> String {
    if hertz >= 1000.0 {
        let kilohertz = hertz / 1000.0;
        if (kilohertz - kilohertz.round()).abs() < 0.01 {
            format!("{kilohertz:.0} kHz")
        } else {
            format!("{kilohertz:.1} kHz")
        }
    } else {
        format!("{hertz:.0} Hz")
    }
}

fn format_frame_rate(rate: &str) -> Option<String> {
    let (numerator, denominator) = rate.split_once('/')?;
    let numerator: f64 = numerator.parse().ok()?;
    let denominator: f64 = denominator.parse().ok()?;
    if denominator == 0.0 {
        return None;
    }
    let fps = numerator / denominator;
    if (fps - fps.round()).abs() < 0.01 {
        Some(format!("{fps:.0}"))
    } else {
        Some(format!("{fps:.2}"))
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let tail: String = value
        .chars()
        .rev()
        .take(width - 1)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("…{tail}")
}

fn truncate_end(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let head = value.chars().take(width - 1).collect::<String>();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;

    /// A scratch directory holding `files`, and an `App` pointed at it. Every render test
    /// needs this and nothing else varies, so the incantation lives here once. The files
    /// are written before the app is built because `App::new` scans on construction.
    fn test_app(tag: &str, files: &[&str]) -> (App, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        for name in files {
            std::fs::write(directory.join(name), b"media").unwrap();
        }
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (conflict_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let app = App::new(
            directory.clone(),
            probe_tx,
            conflict_tx,
            edit_tx.clone(),
            edit_tx,
        )
        .unwrap();
        (app, directory)
    }

    /// An app with a probed file: one video, one audio, two embedded subtitles and two
    /// sidecars. Enough for every section of the overview to be non-empty.
    fn probed_app(tag: &str) -> (App, std::path::PathBuf) {
        let (mut app, directory) = test_app(tag, &["movie.mkv", "movie.eng.srt", "movie.nld.srt"]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "format": {
                    "format_name": "matroska,webm",
                    "duration": "3723.0",
                    "size": "1572864"
                },
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264",
                     "width": 1920, "height": 1080, "avg_frame_rate": "24/1"},
                    {"index": 1, "codec_type": "audio", "codec_name": "aac",
                     "channel_layout": "stereo", "tags": {"language": "eng"}},
                    {"index": 2, "codec_type": "subtitle", "codec_name": "subrip",
                     "tags": {"language": "eng", "title": "English"}},
                    {"index": 3, "codec_type": "subtitle", "codec_name": "subrip",
                     "tags": {"language": "nld"}}
                ]
            }))
            .unwrap(),
        ));
        app.loading = false;
        app.stream_order = vec![0, 1, 2, 3];
        app.sidecars = vec![
            SidecarEntry {
                path: directory.join("movie.eng.srt"),
                companion: Some(directory.join("movie.mkv")),
                display_name: "movie.eng.srt".to_string(),
                format: SubtitleFormat::SubRip,
                language: "eng".to_string(),
                forced: false,
                hearing_impaired: false,
                number: None,
                fingerprint: crate::files::FileFingerprint {
                    length: 2,
                    modified: None,
                },
                companion_fingerprint: None,
            },
            SidecarEntry {
                path: directory.join("movie.nld.srt"),
                companion: Some(directory.join("movie.mkv")),
                display_name: "movie.nld.srt".to_string(),
                format: SubtitleFormat::SubRip,
                language: "nld".to_string(),
                forced: true,
                hearing_impaired: false,
                number: None,
                fingerprint: crate::files::FileFingerprint {
                    length: 2,
                    modified: None,
                },
                companion_fingerprint: None,
            },
        ];
        (app, directory)
    }

    /// Draws the whole application and returns the screen, one string per row.
    fn draw(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Every dialog, and the state it needs to be worth drawing.
    fn open_dialog(app: &mut App, dialog: Dialog) {
        app.layer = Layer::Streams;
        match dialog {
            Dialog::Keybindings => {}
            Dialog::ContainerSettings => {
                app.container_settings_popup = Some(ContainerSettingsPopup {
                    field: ContainerSettingsField::Title,
                    mode: ContainerSettingsMode::Summary,
                    help_visible: true,
                    format_cursor: 0,
                    text_input: TextInputState::new(String::new()),
                });
            }
            Dialog::VideoSettings => {
                app.video_settings_popup = Some(crate::app::VideoSettingsPopup {
                    stream_index: 0,
                    field: VideoSettingsField::Codec,
                    mode: VideoSettingsMode::Summary,
                    codec_cursor: 0,
                    resolution_cursor: 0,
                    custom_resolution: None,
                });
            }
            Dialog::SubtitleSettings => {
                app.subtitle_settings_popup = Some(SubtitleSettingsPopup {
                    source: SubtitleSource::Embedded(2),
                    source_format: SubtitleFormat::SubRip,
                    field: SubtitleSettingsField::Language,
                    mode: SubtitleSettingsMode::Summary,
                    help_visible: true,
                    codec_cursor: 0,
                    language_cursor: 0,
                    language_search: SearchState::default(),
                    title_input: TextInputState::new(String::new()),
                });
            }
            Dialog::ConfirmCancel => {
                app.active_batch = Some(crate::staging::BatchState {
                    cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    items: vec![crate::staging::BatchItem {
                        path: app.directory.join("movie.mkv"),
                        label: Some("Remuxing movie.mkv".to_string()),
                        fraction: Some(0.42),
                        status: crate::staging::BatchItemStatus::Running,
                        output_path: None,
                    }],
                    started: std::time::Instant::now(),
                });
            }
            Dialog::Error => {
                app.edit_error = Some("ffmpeg exited with status 1".to_string());
            }
            Dialog::ConfirmProcessAll => {
                let path = app.directory.join("movie.mkv");
                let fingerprint = crate::files::FileFingerprint {
                    length: 0,
                    modified: None,
                };
                app.cache.insert(
                    crate::app::CacheKey {
                        path: path.clone(),
                        length: fingerprint.length,
                        modified: fingerprint.modified,
                    },
                    app.outcome.clone().expect("probed_app sets an outcome"),
                );
                app.staged_edits.insert(
                    path,
                    crate::staging::StagedEdit {
                        fingerprint,
                        stale: false,
                        conflict_groups: Default::default(),
                        stream_order: vec![0, 1, 2, 3],
                        moved_streams: Default::default(),
                        deleted_streams: Default::default(),
                        default_streams: Default::default(),
                        default_sidecars: Default::default(),
                        video_settings: Default::default(),
                        subtitle_changes: Default::default(),
                        left_subtitle_order: Vec::new(),
                        container_target: Some(ContainerFormat::Mp4),
                        container_metadata: None,
                        original_stream_order: vec![0, 1, 2, 3],
                        original_default_streams: Default::default(),
                        track_groups: Default::default(),
                    },
                );
            }
            Dialog::BatchProcessing => {
                app.active_batch = Some(crate::staging::BatchState {
                    cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    items: vec![crate::staging::BatchItem {
                        path: app.directory.join("movie.mkv"),
                        label: Some("Remuxing movie.mkv".to_string()),
                        fraction: Some(0.42),
                        status: crate::staging::BatchItemStatus::Running,
                        output_path: None,
                    }],
                    started: std::time::Instant::now(),
                });
            }
            Dialog::ConfirmReset => {
                app.request_reset_current_file();
            }
            Dialog::ResolveConflicts => {
                let path = app.directory.join("movie.mkv");
                let fingerprint = crate::files::FileFingerprint {
                    length: 0,
                    modified: None,
                };
                app.staged_edits.insert(
                    path,
                    crate::staging::StagedEdit {
                        fingerprint,
                        stale: true,
                        conflict_groups: std::collections::BTreeSet::from(["video"]),
                        stream_order: vec![0, 1, 2, 3],
                        moved_streams: Default::default(),
                        deleted_streams: Default::default(),
                        default_streams: Default::default(),
                        default_sidecars: Default::default(),
                        video_settings: Default::default(),
                        subtitle_changes: Default::default(),
                        left_subtitle_order: Vec::new(),
                        container_target: Some(ContainerFormat::Mp4),
                        container_metadata: None,
                        original_stream_order: vec![0, 1, 2, 3],
                        original_default_streams: Default::default(),
                        track_groups: Default::default(),
                    },
                );
            }
        }
        app.dialog = Some(dialog);
    }

    #[test]
    fn render_should_draw_every_layer_and_dialog() {
        // Arrange: the whole application, not a single widget — `render` is the only
        // entry point the binary uses, and nothing below it was reachable from a test.
        const DIALOGS: [(Dialog, &str); 10] = [
            (Dialog::Keybindings, "Keybindings"),
            (Dialog::ContainerSettings, "Container settings"),
            (Dialog::VideoSettings, "Video track #0 settings"),
            (Dialog::SubtitleSettings, "Subtitle track #2"),
            (Dialog::ConfirmCancel, "Are you sure you want to cancel"),
            (Dialog::Error, "ffmpeg exited with status 1"),
            (
                Dialog::ConfirmProcessAll,
                "Changing container from MKV to MP4",
            ),
            (Dialog::BatchProcessing, "Remuxing movie.mkv"),
            (Dialog::ConfirmReset, "Reset this file's edits?"),
            (Dialog::ResolveConflicts, "Changed:   video tracks, on disk"),
        ];

        // Act / Assert: each dialog names itself on screen.
        for (dialog, expected) in DIALOGS {
            let (mut app, directory) = probed_app("render-dialogs");
            open_dialog(&mut app, dialog);
            let screen = draw(&mut app, 140, 40).join(" ");
            assert!(
                screen.contains(expected),
                "{dialog:?} should show {expected:?}; screen was:\n{screen}",
            );
            std::fs::remove_dir_all(directory).unwrap();
        }

        // Act / Assert: each layer draws its own furniture, dialog or not.
        for layer in [Layer::Files, Layer::Streams, Layer::StreamDetails] {
            let (mut app, directory) = probed_app("render-layers");
            app.layer = layer;
            app.selected_stream = 1;
            let screen = draw(&mut app, 140, 40).join(" ");
            assert!(
                screen.contains("Files (1)"),
                "{layer:?} should keep the file pane"
            );
            assert!(
                screen.contains("movie.mkv"),
                "{layer:?} should name the file"
            );
            if layer == Layer::StreamDetails {
                assert!(
                    screen.contains("Video #0"),
                    "{layer:?} should open the details popup",
                );
            }
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn confirm_process_all_dialog_should_list_each_files_own_staged_changes() {
        // Arrange: two staged files with different container conversions — each
        // file's row must show its own summary, not just its own name.
        let (mut app, directory) =
            test_app("confirm-process-all-summary", &["alpha.mkv", "beta.mkv"]);
        for (name, target) in [
            ("alpha.mkv", ContainerFormat::Mp4),
            ("beta.mkv", ContainerFormat::WebM),
        ] {
            let path = directory.join(name);
            let fingerprint = crate::files::FileFingerprint {
                length: 0,
                modified: None,
            };
            app.cache.insert(
                crate::app::CacheKey {
                    path: path.clone(),
                    length: fingerprint.length,
                    modified: fingerprint.modified,
                },
                ProbeOutcome::Video(
                    MediaInfo::from_json(serde_json::json!({
                        "format": {"format_name": "matroska,webm"},
                        "streams": [
                            {"index": 0, "codec_type": "video", "codec_name": "h264"}
                        ]
                    }))
                    .unwrap(),
                ),
            );
            app.staged_edits.insert(
                path,
                crate::staging::StagedEdit {
                    fingerprint,
                    stale: false,
                    conflict_groups: Default::default(),
                    stream_order: vec![0],
                    moved_streams: Default::default(),
                    deleted_streams: Default::default(),
                    default_streams: Default::default(),
                    default_sidecars: Default::default(),
                    video_settings: Default::default(),
                    subtitle_changes: Default::default(),
                    left_subtitle_order: Vec::new(),
                    container_target: Some(target),
                    container_metadata: None,
                    original_stream_order: vec![0],
                    original_default_streams: Default::default(),
                    track_groups: Default::default(),
                },
            );
        }
        app.dialog = Some(Dialog::ConfirmProcessAll);

        // Act
        let screen = draw(&mut app, 140, 40).join(" ");

        // Assert
        assert!(screen.contains("alpha.mkv"), "screen was:\n{screen}");
        assert!(screen.contains("beta.mkv"), "screen was:\n{screen}");
        assert!(
            screen.contains("Changing container from MKV to MP4"),
            "screen was:\n{screen}"
        );
        assert!(
            screen.contains("Changing container from MKV to WebM"),
            "screen was:\n{screen}"
        );
        assert!(
            screen.contains("Start") && screen.contains("Cancel"),
            "screen should show a Start/Cancel button bar; screen was:\n{screen}"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolve_conflicts_dialog_should_merge_two_conflicting_types_into_one_block() {
        // A file can lose the tracks of more than one type it edits at once. That's
        // still one block, with both types named on the `Changed` line and every
        // affected change under a single `Reverting` — not one block per type.
        let (mut app, directory) = test_app("resolve-conflicts-both", &["movie.mkv"]);
        let path = directory.join("movie.mkv");
        let fingerprint = crate::files::FileFingerprint {
            length: 5,
            modified: None,
        };
        app.cache.insert(
            crate::app::CacheKey {
                path: path.clone(),
                length: fingerprint.length,
                modified: fingerprint.modified,
            },
            ProbeOutcome::Video(
                MediaInfo::from_json(serde_json::json!({
                    "format": {"format_name": "matroska,webm"},
                    "streams": [
                        {"index": 0, "codec_type": "video", "codec_name": "h264"},
                        {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                        {"index": 2, "codec_type": "audio", "codec_name": "ac3"},
                        {"index": 3, "codec_type": "subtitle", "codec_name": "subrip",
                         "tags": {"language": "eng"}}
                    ]
                }))
                .unwrap(),
            ),
        );
        let mut edit = crate::staging::StagedEdit {
            fingerprint,
            stale: true,
            conflict_groups: std::collections::BTreeSet::from(["audio", "video"]),
            stream_order: vec![0, 2, 3],
            moved_streams: Default::default(),
            deleted_streams: std::collections::BTreeSet::from([1]),
            default_streams: std::collections::BTreeSet::from([2]),
            default_sidecars: Default::default(),
            video_settings: Default::default(),
            subtitle_changes: Default::default(),
            left_subtitle_order: Vec::new(),
            container_target: Some(ContainerFormat::Mp4),
            container_metadata: None,
            original_stream_order: vec![0, 1, 2, 3],
            original_default_streams: std::collections::BTreeSet::from([1]),
            track_groups: std::collections::BTreeMap::from([
                (0, "video"),
                (1, "audio"),
                (2, "audio"),
                (3, "subtitle"),
            ]),
        };
        edit.video_settings.insert(
            0,
            crate::edit::VideoSettings {
                codec: crate::edit::VideoCodec::Hevc,
                resolution: crate::edit::VideoResolution::P1080,
            },
        );
        app.staged_edits.insert(path, edit);
        assert!(app.maybe_open_conflict_dialog());

        // Act: narrow enough that the video encode line has to wrap.
        let lines = draw(&mut app, 100, 30);
        let screen = lines.join(" ");

        // Assert: one block, both types named once.
        assert_that!(screen.matches("File:").count()).is_equal_to(1);
        assert_that!(screen.matches("Reverting:").count()).is_equal_to(1);
        assert!(
            screen.contains("audio and video tracks, on disk"),
            "screen was:\n{screen}"
        );
        // Both types' changes are listed together, the audio ones and the video one.
        assert!(
            screen.contains("Deleting 1 audio track")
                && screen.contains("Changing the default audio track")
                && screen.contains("Encoding video track #0 as HEVC"),
            "screen was:\n{screen}"
        );
        // The subtitle track and the container conversion are untouched by both.
        assert!(
            screen.contains("Keeping:") && screen.contains("Changing container from MKV to MP4"),
            "screen was:\n{screen}"
        );

        // A value too long for the popup wraps to the label column, not to column
        // zero — ratatui's own `Wrap` would do the latter and break the alignment.
        let label_column = lines
            .iter()
            .find_map(|line| Some(line.find("Reverting:")? + "Reverting: ".len()))
            .expect("the reverting row must render");
        let wrapped = lines
            .iter()
            .find(|line| line.contains("1920×1080"))
            .expect("the video encode line must wrap at this width");
        assert_that!(wrapped.find("1920×1080")).is_equal_to(Some(label_column));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resolve_conflicts_dialog_should_name_what_changed_what_goes_and_what_stays() {
        // Arrange: two staged files, each flagged by a background re-probe as having
        // lost the tracks its edit names. Both also stage a container conversion,
        // which survives acknowledgement — the notice has to distinguish the two.
        let (mut app, directory) = test_app("resolve-conflicts", &["alpha.mkv", "beta.mkv"]);
        for (name, group) in [("alpha.mkv", "video"), ("beta.mkv", "audio")] {
            let path = directory.join(name);
            let fingerprint = crate::files::FileFingerprint {
                length: 5,
                modified: None,
            };
            // The summary describes the content the edit was staged against, so the
            // probe at the *staged* fingerprint has to be cached (see
            // `App::reconcile_files`, which keeps it alive past the change).
            app.cache.insert(
                crate::app::CacheKey {
                    path: path.clone(),
                    length: fingerprint.length,
                    modified: fingerprint.modified,
                },
                ProbeOutcome::Video(
                    MediaInfo::from_json(serde_json::json!({
                        "format": {"format_name": "matroska,webm"},
                        "streams": [
                            {"index": 0, "codec_type": "video", "codec_name": "h264"},
                            {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                            {"index": 2, "codec_type": "audio", "codec_name": "ac3"}
                        ]
                    }))
                    .unwrap(),
                ),
            );
            let mut edit = crate::staging::StagedEdit {
                fingerprint,
                stale: true,
                conflict_groups: std::collections::BTreeSet::from([group]),
                stream_order: vec![0, 1, 2],
                moved_streams: Default::default(),
                deleted_streams: Default::default(),
                default_streams: Default::default(),
                default_sidecars: Default::default(),
                video_settings: Default::default(),
                subtitle_changes: Default::default(),
                left_subtitle_order: Vec::new(),
                container_target: Some(ContainerFormat::Mp4),
                container_metadata: None,
                original_stream_order: vec![0, 1, 2],
                original_default_streams: Default::default(),
                track_groups: std::collections::BTreeMap::from([
                    (0, "video"),
                    (1, "audio"),
                    (2, "audio"),
                ]),
            };
            if group == "video" {
                edit.video_settings.insert(
                    0,
                    crate::edit::VideoSettings {
                        codec: crate::edit::VideoCodec::Hevc,
                        resolution: crate::edit::VideoResolution::Original,
                    },
                );
            } else {
                edit.stream_order = vec![0, 2, 1];
                edit.moved_streams = std::collections::BTreeSet::from([2]);
            }
            app.staged_edits.insert(path, edit);
        }
        assert!(
            app.maybe_open_conflict_dialog(),
            "conflicts must auto-open the dialog"
        );

        // Act
        let width = 140;
        let height = 40;
        let lines = draw(&mut app, width, height);
        let screen = lines.join(" ");

        // Assert
        assert!(screen.contains("alpha.mkv"), "screen was:\n{screen}");
        assert!(screen.contains("beta.mkv"), "screen was:\n{screen}");
        assert!(
            screen.contains("Changed:") && screen.contains("video tracks, on disk"),
            "screen was:\n{screen}"
        );
        assert!(
            screen.contains("audio tracks, on disk"),
            "screen was:\n{screen}"
        );
        assert!(
            screen.contains("Reverting:") && screen.contains("Encoding video track #0 as HEVC"),
            "screen was:\n{screen}"
        );
        assert!(
            screen.contains("Moving 1 audio track"),
            "screen was:\n{screen}"
        );
        // The container conversion survives acknowledgement, so it belongs under
        // `Keeping` — without it the notice lists only losses and reads as though
        // everything staged for the file is going.
        assert!(
            screen.contains("Keeping:") && screen.contains("Changing container from MKV to MP4"),
            "the surviving changes must be named, not just the reverted ones; screen \
             was:\n{screen}"
        );
        // No header line restating what the popup title already says.
        assert!(
            !screen.contains("while you were editing"),
            "screen was:\n{screen}"
        );

        // Every label's value starts in the same column, across both files' blocks.
        let value_columns: Vec<usize> = lines
            .iter()
            .filter_map(|line| {
                let label_end = ["File:", "Changed:", "Reverting:", "Keeping:"]
                    .iter()
                    .find_map(|label| Some(line.find(label)? + label.len()))?;
                let rest = &line[label_end..];
                Some(label_end + rest.len() - rest.trim_start().len())
            })
            .collect();
        assert_that!(value_columns.len()).is_equal_to(8);
        assert!(
            value_columns.windows(2).all(|pair| pair[0] == pair[1]),
            "label values must share one column, got {value_columns:?}"
        );

        // The one button, pinned to the popup's last inner row.
        let area = popup_area(ratatui::layout::Rect::new(0, 0, width, height), 60, 50);
        let inner = Block::default().borders(Borders::ALL).inner(area);
        let bottom_row = lines[(inner.y + inner.height - 1) as usize].clone();
        // It opens counting down, so a stray Enter can't acknowledge it — and says so
        // in the label rather than just sitting there inert.
        assert!(
            bottom_row.contains("Understood (5)"),
            "expected a counting-down button on the bottom row, got: {bottom_row:?}",
        );
        assert_that!(screen.matches("Understood").count()).is_equal_to(1);

        // Once armed the count disappears and the button highlights.
        app.conflict_opened_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
        let lines = draw(&mut app, width, height);
        let screen = lines.join(" ");
        let bottom_row = lines[(inner.y + inner.height - 1) as usize].clone();
        assert!(
            bottom_row.contains("Understood") && !bottom_row.contains("("),
            "expected a plain armed button, got: {bottom_row:?}",
        );
        // The Keep/Discard buttons this notice replaced must be gone. ("Keeping:" is
        // a content label, not the old button, so match the button text itself.)
        assert!(
            !screen.contains("Keep staged changes") && !screen.contains("Discard"),
            "screen was:\n{screen}"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn confirm_process_all_dialog_should_scroll_when_content_exceeds_the_popup() {
        // Arrange: enough staged files that the summary can't fit a small popup.
        let (mut app, directory) = test_app("confirm-process-all-scroll", &[]);
        for index in 0..30 {
            let name = format!("movie{index:02}.mkv");
            let path = directory.join(&name);
            std::fs::write(&path, b"media").unwrap();
            let fingerprint = crate::files::FileFingerprint {
                length: 4,
                modified: None,
            };
            app.cache.insert(
                crate::app::CacheKey {
                    path: path.clone(),
                    length: fingerprint.length,
                    modified: fingerprint.modified,
                },
                ProbeOutcome::Video(
                    MediaInfo::from_json(serde_json::json!({
                        "streams": [
                            {"index": 0, "codec_type": "video", "codec_name": "h264"}
                        ]
                    }))
                    .unwrap(),
                ),
            );
            app.staged_edits.insert(
                path,
                crate::staging::StagedEdit {
                    fingerprint,
                    stale: false,
                    conflict_groups: Default::default(),
                    stream_order: vec![0],
                    moved_streams: Default::default(),
                    deleted_streams: Default::default(),
                    default_streams: Default::default(),
                    default_sidecars: Default::default(),
                    video_settings: Default::default(),
                    subtitle_changes: Default::default(),
                    left_subtitle_order: Vec::new(),
                    container_target: Some(ContainerFormat::Mp4),
                    container_metadata: None,
                    original_stream_order: vec![0],
                    original_default_streams: Default::default(),
                    track_groups: Default::default(),
                },
            );
        }
        app.dialog = Some(Dialog::ConfirmProcessAll);

        // Act / Assert: rendering computes a non-zero max scroll, and scrolling
        // actually changes what's on screen.
        let first_screen = draw(&mut app, 140, 20).join("\n");
        assert_that!(app.confirm_process_all_max_scroll).is_greater_than(0);

        app.scroll_confirm_process_all_down(5);
        let scrolled_screen = draw(&mut app, 140, 20).join("\n");
        assert_that!(scrolled_screen.as_str()).is_not_equal_to(first_screen.as_str());

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn render_should_survive_a_file_with_no_probe_result_and_a_scan_error() {
        // Arrange / Act / Assert: the two states that reach `render` before any media
        // information exists, both of which take different branches through the details
        // pane than the probed case above.
        let (mut empty, empty_directory) = test_app("render-none", &[]);
        assert!(draw(&mut empty, 140, 40).join(" ").contains("Files (0)"));
        std::fs::remove_dir_all(empty_directory).unwrap();

        let (mut app, directory) = test_app("render-empty", &["movie.mkv"]);
        app.loading = true;
        assert!(draw(&mut app, 140, 40).join(" ").contains("Loading"));

        app.loading = false;
        app.scan_error = Some("permission denied".to_string());
        assert!(
            draw(&mut app, 140, 40)
                .join(" ")
                .contains("permission denied")
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn overview_should_mark_a_deleted_track_and_a_container_conflict() {
        // Arrange: MP4 cannot store SubRip, so targeting it puts a conflict on both
        // subtitle tracks; one video track is also marked for deletion.
        let (mut app, directory) = probed_app("overview-marks");
        app.layer = Layer::Streams;
        app.selected_stream = app
            .track_rows()
            .iter()
            .position(|row| *row == TrackRef::Embedded(0))
            .unwrap();
        app.toggle_delete_selected_stream();
        app.container_target = Some(crate::edit::ContainerFormat::Mp4);

        // Act
        let screen = draw(&mut app, 160, 40).join("\n");

        // Assert: the deletion marker and the conflict marker both reach the screen.
        assert_that!(screen.as_str()).contains("×");
        assert_that!(screen.as_str()).contains("compatibility conflict");

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Builds the overview exactly as `render_details` does, but with the column layout
    /// under the test's control. Returns the rendered lines and the line the selection
    /// landed on, which is what drives auto-scrolling.
    fn overview(app: &App, side_by_side: bool) -> (Vec<String>, Option<usize>) {
        let info = match &app.outcome {
            Some(ProbeOutcome::Video(info)) => info,
            other => panic!("overview needs a probed video, got {other:?}"),
        };
        let changed = app.changed_streams();
        let rows = app.track_rows();
        let conflicting_streams = app.selected_container_conflict_streams();
        let (text, selected_line) = media_text(
            info,
            details_selected_stream(app),
            MediaTextState {
                order: &app.stream_order,
                rows: &rows,
                sidecars: &app.sidecars,
                deleted: &app.deleted_streams,
                defaults: &app.default_streams,
                default_sidecars: &app.default_sidecars,
                changed: &changed,
                subtitle_changes: &app.subtitle_changes,
                source_container: app.source_container(),
                container_target: app.container_target,
                container_metadata_changed: app.container_metadata_changed(),
                container_conflicts: app.selected_container_conflicts().len(),
                conflicting_streams: &conflicting_streams,
                subtitle_columns_side_by_side: side_by_side,
                subtitle_column_width: 60,
            },
        );
        let lines = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        (lines, selected_line)
    }

    /// Moves the selection onto `track` and returns its row index.
    fn select_track(app: &mut App, track: TrackRef) -> usize {
        let index = app
            .track_rows()
            .iter()
            .position(|row| *row == track)
            .unwrap_or_else(|| panic!("{track:?} is not a selectable row"));
        app.layer = Layer::Streams;
        app.selected_stream = index;
        index
    }

    /// Every combination of the four row states, and the style each of the three overview
    /// row builders gives them. Pinned exhaustively because the cascades are the only
    /// thing telling a user that a track is deleted, in conflict, or edited — and because
    /// they were duplicated four times before being shared.
    #[test]
    fn track_row_styles_should_rank_selection_over_deletion_over_conflict_over_change() {
        let stream = serde_json::json!({"index": 4, "codec_type": "audio", "codec_name": "aac"});
        let stream = stream
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let info = MediaInfo::from_json(serde_json::json!({
            "format": {"format_name": "matroska,webm"},
            "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}]
        }))
        .unwrap();

        for selected in [false, true] {
            for deleted in [false, true] {
                for conflict in [false, true] {
                    for changed in [false, true] {
                        let case = format!(
                            "selected={selected} deleted={deleted} \
                             conflict={conflict} changed={changed}"
                        );
                        let expected = if selected {
                            focused_style(changed && !deleted)
                        } else if deleted {
                            Style::default().fg(Color::Red)
                        } else if conflict {
                            warning_style(changed)
                        } else if changed {
                            changed_style()
                        } else {
                            Style::default()
                        };
                        let line =
                            stream_line(&stream, 4, selected, deleted, changed, conflict, false);
                        assert_eq!(line.style, expected, "stream row, {case}");

                        // The container row cannot be deleted, and the sidecar row can be
                        // neither deleted nor in conflict; both otherwise rank the same.
                        if !deleted {
                            let expected = if selected {
                                focused_style(changed)
                            } else if conflict {
                                warning_style(changed)
                            } else if changed {
                                changed_style()
                            } else {
                                Style::default()
                            };
                            let line = container_line(
                                &info,
                                None,
                                changed.then_some(crate::edit::ContainerFormat::Mp4),
                                changed,
                                usize::from(conflict),
                                selected,
                            );
                            assert_eq!(line.style, expected, "container row, {case}");
                        }
                    }
                }
            }
        }
    }

    fn stream_of(value: serde_json::Value) -> BTreeMap<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    fn rendered(lines: Vec<Line<'static>>) -> String {
        lines
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn video_information_should_describe_the_picture_from_the_probe_fields() {
        // Arrange: the `i` panel is where a user decides whether a file is worth keeping,
        // so the picture description has to read the probe fields it claims to.
        let hdr = stream_of(serde_json::json!({
            "index": 0, "codec_type": "video", "codec_name": "hevc",
            "width": 3840, "height": 2160, "display_aspect_ratio": "16:9",
            "color_transfer": "smpte2084", "bits_per_raw_sample": "10",
            "field_order": "progressive", "avg_frame_rate": "24000/1001",
            "bit_rate": "18000000",
            "disposition": {"default": 1, "original": 1},
            "tags": {"language": "eng", "title": "Main feature"}
        }));

        // Act
        let panel = rendered(video_information_lines(&hdr, true));

        // Assert
        assert_that!(panel.as_str()).contains("HEVC (H.265)");
        assert_that!(panel.as_str()).contains("3840×2160");
        assert_that!(panel.as_str()).contains("16:9");
        assert_that!(panel.as_str()).contains("4K");
        assert_that!(panel.as_str()).contains("HDR10");
        assert_that!(panel.as_str()).contains("10-bit");
        assert_that!(panel.as_str()).contains("Progressive");
        assert_that!(panel.as_str()).contains("Default");
        assert_that!(panel.as_str()).contains("Main feature");

        // And a plainer stream: bit depth inferred from the pixel format, SDR transfer,
        // interlaced scan, and a height that matches no marketing tier.
        let sdr = stream_of(serde_json::json!({
            "index": 0, "codec_type": "video", "codec_name": "mpeg2video",
            "width": 720, "height": 576, "pix_fmt": "yuv420p",
            "color_transfer": "bt470bg", "field_order": "tt"
        }));
        let panel = rendered(video_information_lines(&sdr, false));
        assert_that!(panel.as_str()).contains("MPEG-2 Video");
        assert_that!(panel.as_str()).contains("576p");
        assert_that!(panel.as_str()).contains("8-bit");
        assert_that!(panel.as_str()).contains("Interlaced");
        assert_that!(panel.as_str()).does_not_contain("Default");

        // A codec with no friendly name falls back to its ffmpeg name, uppercased, and a
        // stream with no picture fields at all still renders.
        let bare = stream_of(serde_json::json!({"index": 0, "codec_type": "video"}));
        let panel = rendered(video_information_lines(&bare, false));
        assert_that!(panel.as_str()).contains("Unknown");
    }

    #[test]
    fn audio_information_should_name_the_codec_profile_and_channel_layout() {
        // Arrange / Act / Assert: DTS in particular is several formats behind one codec
        // name, and the profile is the only thing telling them apart.
        for (codec, profile, expected) in [
            ("dts", Some("DTS-HD MA"), "DTS-HD Master Audio"),
            ("dts", Some("DTS-HD HRA"), "DTS-HD High Resolution Audio"),
            ("dts", None, "DTS"),
            ("eac3", None, "Dolby Digital Plus (E-AC-3)"),
            ("pcm_s24le", None, "PCM · Uncompressed"),
            ("flac", None, "FLAC · Lossless"),
            ("opus", None, "Opus"),
        ] {
            let mut stream = serde_json::json!({
                "index": 1, "codec_type": "audio", "codec_name": codec,
                "channel_layout": "5.1(side)", "sample_rate": "48000"
            });
            if let Some(profile) = profile {
                stream["profile"] = serde_json::json!(profile);
            }
            let panel = rendered(audio_information_lines(&stream_of(stream), false));
            assert!(
                panel.contains(expected),
                "{codec} {profile:?} should read as {expected:?}:\n{panel}",
            );
            assert!(
                panel.contains("5.1 surround"),
                "the channel layout should be spelled out:\n{panel}",
            );
        }

        // Named layouts, and a fallback to the raw channel count when the layout is
        // missing or unrecognised.
        for (layout, channels, expected) in [
            (Some("mono"), None, "Mono"),
            (Some("stereo"), None, "Stereo"),
            (Some("quad"), None, "Quadraphonic"),
            (None, Some(2), "Stereo"),
            (None, Some(8), "8 channels"),
        ] {
            let mut stream = serde_json::json!({
                "index": 1, "codec_type": "audio", "codec_name": "aac"
            });
            if let Some(layout) = layout {
                stream["channel_layout"] = serde_json::json!(layout);
            }
            if let Some(channels) = channels {
                stream["channels"] = serde_json::json!(channels);
            }
            let panel = rendered(audio_information_lines(&stream_of(stream), false));
            assert!(
                panel.contains(expected),
                "{layout:?}/{channels:?} should read as {expected:?}:\n{panel}",
            );
        }

        // Every disposition the panel knows about is listed as a role.
        let described = stream_of(serde_json::json!({
            "index": 1, "codec_type": "audio", "codec_name": "ac3", "channels": 2,
            "disposition": {"visual_impaired": 1, "comment": 1, "dub": 1},
            "tags": {"language": "nld"}
        }));
        let panel = rendered(audio_information_lines(&described, true));
        assert_that!(panel.as_str()).contains("Default");
        assert_that!(panel.as_str()).contains("Audio description");
        assert_that!(panel.as_str()).contains("Commentary");
        assert_that!(panel.as_str()).contains("Dubbed");
        assert_that!(panel.as_str()).contains("Dutch");
    }

    #[test]
    fn custom_resolution_dialog_should_show_the_source_size_the_draft_and_any_error() {
        // Arrange: the custom-resolution editor, which is the only place a user types a
        // resolution and so the only place a typo has to be caught before saving.
        let (mut app, directory) = probed_app("custom-resolution");
        app.layer = Layer::Streams;
        app.selected_stream = 1;
        app.open_video_settings();
        app.move_video_settings_cursor(1);
        app.activate_video_settings();
        let custom = app.resolution_choices(0).len() - 1;
        app.video_settings_popup.as_mut().unwrap().resolution_cursor = custom;
        app.activate_video_settings();
        assert!(
            app.video_settings_popup
                .as_ref()
                .is_some_and(|popup| popup.custom_resolution.is_some()),
            "the custom editor should be open",
        );

        // Act: as opened, prefilled from the source.
        let screen = draw(&mut app, 120, 30).join("\n");

        // Assert
        assert_that!(screen.as_str()).contains("Original: 1920×1080");
        assert_that!(screen.as_str()).contains("Width");
        assert_that!(screen.as_str()).contains("Height");

        // Act: an odd height, which no encoder will take.
        let draft = app
            .video_settings_popup
            .as_mut()
            .unwrap()
            .custom_resolution
            .as_mut()
            .unwrap();
        draft.height.clear();
        for character in "1081".chars() {
            draft.height.insert(character, TextInputConfig::RESOLUTION);
        }
        let screen = draw(&mut app, 120, 30).join("\n");

        // Assert: the dialog says so rather than waiting for ffmpeg to fail.
        assert!(
            app.custom_resolution_error().is_some(),
            "1081 is not a usable height",
        );
        assert!(
            screen.contains(&app.custom_resolution_error().unwrap()),
            "the dialog should show its own error:\n{screen}",
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn every_selectable_row_should_have_an_information_panel() {
        // Arrange: a file carrying an attachment and a data stream alongside the real
        // tracks. Those extra streams are deliberately not selectable — and every row
        // that *is* selectable must have something to show when the user presses `i`,
        // or the key does nothing with no explanation.
        let (mut app, directory) = probed_app("details-every-row");
        let Some(ProbeOutcome::Video(info)) = app.outcome.as_mut() else {
            unreachable!("probed_app always yields a video");
        };
        for (index, kind, codec) in [(4, "attachment", "ttf"), (5, "data", "bin")] {
            info.streams.push(
                serde_json::from_value(serde_json::json!({
                    "index": index, "codec_type": kind, "codec_name": codec
                }))
                .unwrap(),
            );
        }
        app.stream_order = vec![0, 1, 2, 3, 4, 5];

        // Act / Assert
        let rows = app.track_rows();
        assert!(
            !rows.contains(&TrackRef::Embedded(4)) && !rows.contains(&TrackRef::Embedded(5)),
            "attachments and data streams are not selectable: {rows:?}",
        );
        for (index, row) in rows.iter().enumerate() {
            app.layer = Layer::Streams;
            app.selected_stream = index;
            let (text, title) = details_popup_content(&app)
                .unwrap_or_else(|| panic!("{row:?} has no information panel"));
            assert!(!title.trim().is_empty(), "{row:?} has an untitled panel");
            assert!(
                text.lines
                    .iter()
                    .any(|line| !line.to_string().trim().is_empty()),
                "{row:?} has an empty information panel",
            );
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn settings_dialogs_should_scroll_the_focused_field_into_view_and_honour_the_help_panel() {
        // Arrange: both settings popups, focused on their last field, in a terminal too
        // short to show the whole field list. The two dialogs share one renderer, so
        // whatever is asserted here has to hold for both.
        for (dialog, last_field, help_title) in [
            (
                Dialog::ContainerSettings,
                "Artist",
                "Information about Artist",
            ),
            (
                Dialog::SubtitleSettings,
                "Commentary",
                "Information about Commentary",
            ),
        ] {
            let (mut app, directory) = probed_app("settings-scroll");
            open_dialog(&mut app, dialog);
            if let Some(popup) = app.container_settings_popup.as_mut() {
                popup.field = ContainerSettingsField::Artist;
                popup.help_visible = false;
            }
            if let Some(popup) = app.subtitle_settings_popup.as_mut() {
                popup.field = SubtitleSettingsField::Commentary;
                popup.help_visible = false;
            }

            // Act
            let short = draw(&mut app, 140, 16).join("\n");

            // Assert: the field the user is on is on screen even though the dialog is
            // taller than the terminal, and no help panel is drawn while it is closed.
            assert!(
                short.contains(last_field),
                "{dialog:?} should scroll {last_field:?} into view:\n{short}",
            );
            assert!(
                !short.contains(help_title),
                "{dialog:?} should not show help while it is closed:\n{short}",
            );

            // Act: the same dialog with help open.
            if let Some(popup) = app.container_settings_popup.as_mut() {
                popup.help_visible = true;
            }
            if let Some(popup) = app.subtitle_settings_popup.as_mut() {
                popup.help_visible = true;
            }
            let with_help = draw(&mut app, 140, 16).join("\n");

            // Assert: the panel appears, titled for the focused field, without pushing
            // that field off screen.
            assert!(
                with_help.contains(help_title),
                "{dialog:?} should title the help panel for the focused field:\n{with_help}",
            );
            assert!(
                with_help.contains(last_field),
                "{dialog:?} should keep {last_field:?} visible beside the help:\n{with_help}",
            );

            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn overview_should_place_exported_and_imported_subtitles_in_opposite_columns() {
        // Arrange: an embedded subtitle marked for export and a sidecar marked for
        // import. Both cross over, so each column's count moves by one.
        let (mut app, directory) = probed_app("overview-transfer");
        let before = draw(&mut app, 160, 40).join("\n");
        assert_that!(before.as_str()).contains("Embedded subtitles (2)");
        assert_that!(before.as_str()).contains("External subtitles (2)");

        select_track(&mut app, TrackRef::Embedded(2));
        assert!(app.transfer_subtitle(1), "export should be accepted");
        select_track(&mut app, TrackRef::Sidecar(0));
        assert!(app.transfer_subtitle(-1), "import should be accepted");

        // Act: read the stacked layout, where the two columns are separate sections and
        // a track can be attributed to one of them unambiguously.
        app.selected_stream = 0;
        let (lines, _) = overview(&app, false);
        let embedded = lines
            .iter()
            .position(|line| line.contains("Embedded subtitles (2)"))
            .expect("embedded section");
        let external = lines
            .iter()
            .position(|line| line.contains("External subtitles (2)"))
            .expect("external section");

        // Assert: the exported embedded track (#2) is now listed under External, and the
        // imported sidecar under Embedded. Embedded tracks carry a `#index`, sidecars do
        // not, so each section should now hold one of each.
        let rows = |start: usize| {
            lines[start + 1..]
                .iter()
                .take_while(|line| !line.trim().is_empty())
                .cloned()
                .collect::<Vec<_>>()
        };
        let embedded_rows = rows(embedded);
        let external_rows = rows(external);
        assert_eq!(
            embedded_rows.iter().filter(|row| row.contains('#')).count(),
            1,
            "Embedded should hold one embedded track and the imported sidecar: {embedded_rows:?}",
        );
        assert!(
            external_rows.iter().any(|row| row.contains("#2")),
            "the exported track #2 should have moved to External: {external_rows:?}",
        );
        assert_eq!(
            external_rows.iter().filter(|row| row.contains('#')).count(),
            1,
            "External should hold the exported track and one sidecar: {external_rows:?}",
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn overview_should_reach_the_same_tracks_stacked_as_side_by_side() {
        // Arrange
        let (app, directory) = probed_app("overview-stacked");

        // Act
        let (columns, _) = overview(&app, true);
        let (stacked, _) = overview(&app, false);

        // Assert: the two layouts are alternative presentations of one model — both name
        // every section and every track, and neither drops a row the other shows.
        for layout in [&columns, &stacked] {
            let joined = layout.join("\n");
            for expected in [
                "Container",
                "Video (1)",
                "Audio (1)",
                "Embedded subtitles (2)",
                "External subtitles (2)",
                "#2",
                "#3",
            ] {
                assert!(
                    joined.contains(expected),
                    "layout should contain {expected:?}; was:\n{joined}",
                );
            }
        }
        // Stacking is taller because each subtitle gets its own line.
        assert!(
            stacked.len() > columns.len(),
            "stacked {} should exceed side-by-side {}",
            stacked.len(),
            columns.len(),
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn overview_should_report_the_line_of_the_focused_track_in_every_section() {
        // Arrange: every selectable row, in both layouts. `selected_line` is what
        // `render_details` scrolls to, so a row that reports `None` is a row the user
        // can focus but never see.
        let (mut app, directory) = probed_app("overview-selection");
        let rows = app.track_rows();
        // Container, video, audio, two embedded subtitles, two sidecars.
        assert_that!(rows.len()).is_equal_to(7);

        for side_by_side in [true, false] {
            for (index, row) in rows.iter().enumerate() {
                app.layer = Layer::Streams;
                app.selected_stream = index;

                // Act
                let (lines, selected_line) = overview(&app, side_by_side);

                // Assert: the reported line exists and is not a section heading.
                let Some(line) = selected_line else {
                    panic!("{row:?} reported no selected line (side_by_side={side_by_side})");
                };
                let text = lines
                    .get(line)
                    .unwrap_or_else(|| panic!("{row:?} pointed past the end of the overview"));
                assert!(
                    !text.trim().is_empty(),
                    "{row:?} selected a blank line: {lines:?}",
                );
            }
        }

        // And with the file pane focused nothing is highlighted at all.
        app.layer = Layer::Files;
        assert_that!(overview(&app, true).1).is_none();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn render_should_fall_back_to_a_notice_when_the_terminal_is_too_small() {
        // Arrange: side-by-side columns on, then a terminal that cannot hold them.
        let (mut app, directory) = probed_app("render-small");
        app.set_subtitle_columns_side_by_side(true);
        assert_that!(app.subtitle_columns_side_by_side).is_true();

        // Act
        let screen = draw(&mut app, 49, 9).join(" ");

        // Assert: the notice replaces the whole frame, and the columns are turned off so
        // the app does not come back at a usable size still trying to draw two columns.
        assert_that!(screen.as_str()).contains("Terminal too small");
        assert_that!(screen.as_str()).does_not_contain("Files (");
        assert_that!(app.subtitle_columns_side_by_side).is_false();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn render_should_dim_the_backdrop_behind_a_dialog_and_the_details_popup() {
        // Arrange
        let (mut app, directory) = probed_app("render-dim");

        // Act: the plain overview, then the same screen with a dialog over it.
        app.layer = Layer::Streams;
        let plain = draw(&mut app, 140, 40);
        open_dialog(&mut app, Dialog::Keybindings);
        let dimmed = draw(&mut app, 140, 40);

        // Assert: the backdrop is still drawn underneath rather than cleared, which is
        // what makes the dialog read as an overlay.
        assert!(plain.iter().any(|row| row.contains("movie.mkv")));
        assert!(dimmed.iter().any(|row| row.contains("movie.mkv")));
        assert!(dimmed.iter().any(|row| row.contains("Keybindings")));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn progress_dialog_should_use_a_standalone_loader_or_a_real_gauge() {
        let (mut app, directory) = test_app("progress-ui", &[]);
        app.active_batch = Some(crate::staging::BatchState {
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            items: vec![crate::staging::BatchItem {
                path: app.directory.join("movie.dan.sup"),
                label: Some("Running OCR on movie.dan.sup → SRT (dan)".to_string()),
                fraction: None,
                status: crate::staging::BatchItemStatus::Running,
                output_path: None,
            }],
            started: std::time::Instant::now(),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 15)).unwrap();

        terminal
            .draw(|frame| render_progress_dialog(frame, &app))
            .unwrap();
        let indeterminate = terminal.backend().buffer();
        let indeterminate_text = indeterminate
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_that!(&indeterminate_text).contains("Running OCR on movie.dan.sup → SRT (dan)");
        assert_that!(indeterminate_text).contains("●••");
        assert_that!(
            indeterminate
                .content
                .iter()
                .any(|cell| cell.bg == Color::DarkGray)
        )
        .is_false();

        app.active_batch.as_mut().unwrap().items[0].fraction = Some(0.42);
        terminal
            .draw(|frame| render_progress_dialog(frame, &app))
            .unwrap();
        let determinate = terminal.backend().buffer();
        let determinate_text = determinate
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_that!(&determinate_text).contains("42%");
        assert_that!(
            determinate
                .content
                .iter()
                .any(|cell| cell.bg == Color::DarkGray)
        )
        .is_true();

        std::fs::remove_dir_all(directory).unwrap();
    }

    fn batch_of(app: &App, count: usize) -> crate::staging::BatchState {
        crate::staging::BatchState {
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            items: (0..count)
                .map(|index| crate::staging::BatchItem {
                    path: app.directory.join(format!("movie{index}.mkv")),
                    label: None,
                    fraction: None,
                    status: crate::staging::BatchItemStatus::Pending,
                    output_path: None,
                })
                .collect(),
            started: std::time::Instant::now(),
        }
    }

    #[test]
    fn batch_progress_dialog_should_mark_the_cursor_row_and_scroll_to_follow_it() {
        // Arrange: more files than the popup can show at once.
        let (mut app, directory) = test_app("batch-cursor-ui", &[]);
        app.active_batch = Some(batch_of(&app, 8));
        app.dialog = Some(Dialog::BatchProcessing);

        // Act: draw with the cursor at rest, then after moving it past the last row.
        let resting = draw(&mut app, 100, 16);
        app.move_batch_cursor_down(7);
        let scrolled = draw(&mut app, 100, 16);

        // Assert: the bar sits beside the cursor row and nowhere else, and the viewport
        // followed the cursor to the end of the list.
        let bar_rows: Vec<usize> = resting
            .iter()
            .enumerate()
            .filter(|(_, row)| row.contains('┃'))
            .map(|(index, _)| index)
            .collect();
        let first_row = resting
            .iter()
            .position(|row| row.contains("movie0.mkv"))
            .expect("the first file is visible");
        assert_that!(bar_rows).is_equal_to(vec![first_row, first_row + 1]);
        assert_that!(resting.iter().any(|row| row.contains("movie7.mkv"))).is_false();

        assert_that!(scrolled.iter().any(|row| row.contains("movie0.mkv"))).is_false();
        let last_row = scrolled
            .iter()
            .position(|row| row.contains("movie7.mkv"))
            .expect("the cursor row is visible after scrolling");
        let scrolled_bars: Vec<usize> = scrolled
            .iter()
            .enumerate()
            .filter(|(_, row)| row.contains('┃'))
            .map(|(index, _)| index)
            .collect();
        assert_that!(scrolled_bars).is_equal_to(vec![last_row, last_row + 1]);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_settings_dialog_should_align_the_format_row_with_the_metadata_rows() {
        // Arrange
        let (mut app, directory) = test_app("container-alignment-ui", &[]);
        // Select the Comment field so neither Format nor Title is highlighted,
        // keeping their label styling identical for a fair column comparison.
        app.container_settings_popup = Some(ContainerSettingsPopup {
            field: ContainerSettingsField::Comment,
            mode: ContainerSettingsMode::Summary,
            help_visible: false,
            format_cursor: 0,
            text_input: TextInputState::new(String::new()),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();

        // Act
        terminal
            .draw(|frame| render_container_settings_dialog(frame, &app))
            .unwrap();

        // Assert
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        // `String::find` returns a byte offset, not a column — the chevron and border
        // glyphs are multi-byte, so columns must be counted over `.chars()` instead.
        let border_column = |y: u16| -> usize {
            row_text(y)
                .chars()
                .position(|character| character == '│')
                .expect("row should have a left border")
        };
        let format_row = (0..buffer.area.height)
            .find(|&y| row_text(y).contains("Format"))
            .expect("Format row should be rendered");
        let title_row = (0..buffer.area.height)
            .find(|&y| row_text(y).contains("Title"))
            .expect("Title row should be rendered");

        // Format's value sits inside `[ ... ]`; Title's sits inside a field frame. The
        // popup's own left border uses the same glyph as an idle frame, so skip it.
        let format_value_column = row_text(format_row)
            .chars()
            .position(|character| character == '[')
            .expect("Format row should show a value")
            - border_column(format_row);
        let title_value_column = row_text(title_row)
            .chars()
            .enumerate()
            .filter(|(_, character)| FIELD_FRAME.starts_with(*character))
            .map(|(column, _)| column)
            .nth(1)
            .expect("Title row should show its field frame")
            - border_column(title_row);

        assert_that!(format_value_column).is_equal_to(title_value_column);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_settings_dialog_should_show_the_same_format_label_as_the_dropdown_entry() {
        // Arrange — a known .mkv source with no explicit target (target == None,
        // i.e. "keep original"): the summary row must show the same short label
        // ("MKV") the dropdown itself uses for that entry, not a separately-sourced
        // verbose ffprobe string ("Original (matroska,webm)").
        let (mut app, directory) = test_app("container-format-label-ui", &["movie.mkv"]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "format": {
                    "format_name": "matroska,webm",
                    "format_long_name": "Matroska / WebM"
                },
                "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}]
            }))
            .unwrap(),
        ));
        app.container_target = None;
        app.container_settings_popup = Some(ContainerSettingsPopup {
            field: ContainerSettingsField::Format,
            mode: ContainerSettingsMode::Summary,
            help_visible: false,
            format_cursor: 0,
            text_input: TextInputState::new(String::new()),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 15)).unwrap();

        // Act
        terminal
            .draw(|frame| render_container_settings_dialog(frame, &app))
            .unwrap();

        // Assert
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_that!(&text).contains("MKV");
        assert_that!(&text).does_not_contain("matroska");
        assert_that!(&text).does_not_contain("Original (");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_settings_format_dropdown_should_show_tree_guides_connecting_to_the_field() {
        // Arrange
        let (mut app, directory) = test_app("container-format-dropdown-ui", &["movie.mkv"]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}]
            }))
            .unwrap(),
        ));
        app.container_settings_popup = Some(ContainerSettingsPopup {
            field: ContainerSettingsField::Format,
            mode: ContainerSettingsMode::FormatDropdown,
            help_visible: false,
            format_cursor: 0,
            text_input: TextInputState::new(String::new()),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();

        // Act
        terminal
            .draw(|frame| render_container_settings_dialog(frame, &app))
            .unwrap();

        // Assert — the expanded chevron marks the Format row, and every option below
        // it hangs off a tree guide, with the final option closed off by "└──".
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_that!(&text).contains("▿ Format");
        assert_that!(&text).contains("├── ");
        // Trailing space distinguishes a real closing guide from the popup's own
        // unbroken border corner ("└────...┘"), which also contains a bare "└──".
        assert_that!(&text).contains("└── ");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_settings_format_dropdown_should_leave_exactly_one_blank_line_before_title() {
        // Arrange — a single blank line should separate the option list from the
        // Title field below it, same as when the dropdown is collapsed.
        let (mut app, directory) = test_app("container-format-dropdown-spacing-ui", &["movie.mkv"]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}]
            }))
            .unwrap(),
        ));
        app.container_settings_popup = Some(ContainerSettingsPopup {
            field: ContainerSettingsField::Format,
            mode: ContainerSettingsMode::FormatDropdown,
            help_visible: false,
            format_cursor: 0,
            text_input: TextInputState::new(String::new()),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();

        // Act
        terminal
            .draw(|frame| render_container_settings_dialog(frame, &app))
            .unwrap();

        // Assert — the row right after the last option (closed off by "└──") is
        // blank, and the row after that is "Title" itself.
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        let last_option_row = (0..buffer.area.height)
            .find(|&y| row_text(y).contains("└── "))
            .expect("closing tree guide should be rendered");
        assert_that!(row_text(last_option_row + 1)).does_not_contain("Title");
        assert_that!(row_text(last_option_row + 2)).contains("Title");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn container_settings_format_dropdown_should_not_leave_a_blank_line_before_the_first_option() {
        // Arrange — the tree guide on the first option already connects it visually
        // to the Format row above; a blank line in between would defeat that.
        let (mut app, directory) =
            test_app("container-format-dropdown-first-option-ui", &["movie.mkv"]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}]
            }))
            .unwrap(),
        ));
        app.container_settings_popup = Some(ContainerSettingsPopup {
            field: ContainerSettingsField::Format,
            mode: ContainerSettingsMode::FormatDropdown,
            help_visible: false,
            format_cursor: 0,
            text_input: TextInputState::new(String::new()),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();

        // Act
        terminal
            .draw(|frame| render_container_settings_dialog(frame, &app))
            .unwrap();

        // Assert — the row right after "Format" is the first option itself.
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        let format_row = (0..buffer.area.height)
            .find(|&y| row_text(y).contains("Format"))
            .expect("Format row should be rendered");
        assert_that!(row_text(format_row + 1)).contains("├──");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_language_dropdown_empty_state_should_show_a_closing_tree_guide() {
        // Arrange — search for a language that matches nothing.
        let (mut app, directory) = test_app("subtitle-language-empty-ui", &[]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {
                        "index": 1,
                        "codec_type": "subtitle",
                        "codec_name": "subrip",
                        "tags": {"language": "eng"}
                    }
                ]
            }))
            .unwrap(),
        ));
        let source = SubtitleSource::Embedded(1);
        let language_search = SearchState {
            input: TextInputState::new("zzznotalanguage".to_string()),
            ..SearchState::default()
        };
        app.subtitle_settings_popup = Some(crate::app::SubtitleSettingsPopup {
            source: source.clone(),
            source_format: SubtitleFormat::SubRip,
            field: SubtitleSettingsField::Language,
            mode: SubtitleSettingsMode::LanguageDropdown,
            help_visible: false,
            codec_cursor: 0,
            language_cursor: 0,
            language_search,
            title_input: TextInputState::default(),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();

        // Act
        terminal
            .draw(|frame| render_subtitle_settings_dialog(frame, &app))
            .unwrap();

        // Assert
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_that!(&text).contains("No matching languages");
        assert_that!(&text).contains("└── ");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_settings_codec_dropdown_should_leave_a_blank_line_before_language() {
        // Arrange
        let (mut app, directory) = test_app("subtitle-codec-dropdown-spacing-ui", &[]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {
                        "index": 1,
                        "codec_type": "subtitle",
                        "codec_name": "subrip",
                        "tags": {"language": "eng"}
                    }
                ]
            }))
            .unwrap(),
        ));
        let source = SubtitleSource::Embedded(1);
        app.subtitle_settings_popup = Some(crate::app::SubtitleSettingsPopup {
            source: source.clone(),
            source_format: SubtitleFormat::SubRip,
            field: SubtitleSettingsField::Codec,
            mode: SubtitleSettingsMode::CodecDropdown,
            help_visible: false,
            codec_cursor: 0,
            language_cursor: 0,
            language_search: SearchState::default(),
            title_input: TextInputState::default(),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();

        // Act
        terminal
            .draw(|frame| render_subtitle_settings_dialog(frame, &app))
            .unwrap();

        // Assert — the row right after the last option is blank, and the row after
        // that is "Language" itself.
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        let last_option_row = (0..buffer.area.height)
            .find(|&y| row_text(y).contains("└── "))
            .expect("closing tree guide should be rendered");
        assert_that!(row_text(last_option_row + 1)).does_not_contain("Language");
        assert_that!(row_text(last_option_row + 2)).contains("Language");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_settings_language_dropdown_should_leave_a_blank_line_before_title() {
        // Arrange
        let (mut app, directory) = test_app("subtitle-language-dropdown-spacing-ui", &[]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {
                        "index": 1,
                        "codec_type": "subtitle",
                        "codec_name": "subrip",
                        "tags": {"language": "eng", "title": "English dialogue"}
                    }
                ]
            }))
            .unwrap(),
        ));
        let source = SubtitleSource::Embedded(1);
        app.subtitle_settings_popup = Some(crate::app::SubtitleSettingsPopup {
            source: source.clone(),
            source_format: SubtitleFormat::SubRip,
            field: SubtitleSettingsField::Language,
            mode: SubtitleSettingsMode::LanguageDropdown,
            help_visible: false,
            codec_cursor: 0,
            language_cursor: 0,
            language_search: SearchState::default(),
            title_input: TextInputState::new("English dialogue".to_string()),
        });
        // Tall enough that the full (windowed, up-to-10-row) language list, its
        // search box, and the fields below it all fit without internal scrolling.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 45)).unwrap();

        // Act
        terminal
            .draw(|frame| render_subtitle_settings_dialog(frame, &app))
            .unwrap();

        // Assert — the row right after the last option is blank, and the row after
        // that is "Title" itself.
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        // Trailing space distinguishes a real tree guide ("├── "/"└── ") from the
        // popup's own unbroken border ("└────...┘"), which would otherwise also
        // match a bare "└──" search.
        let last_option_row = (0..buffer.area.height)
            .rev()
            .find(|&y| row_text(y).contains("├── ") || row_text(y).contains("└── "))
            .expect("at least one language option should be rendered");
        assert_that!(row_text(last_option_row + 1)).does_not_contain("Title");
        assert_that!(row_text(last_option_row + 2)).contains("Title");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn loader_should_move_one_bright_cell_without_changing_its_width() {
        let first = loader_line(0);
        let second = loader_line(2);
        let third = loader_line(4);
        let returning = loader_line(6);

        assert_that!(first.width()).is_equal_to(3);
        assert_that!(second.width()).is_equal_to(3);
        assert_that!(third.width()).is_equal_to(3);
        assert_that!(returning.width()).is_equal_to(3);
        assert_that!(first.to_string()).is_equal_to("●••".to_string());
        assert_that!(second.to_string()).is_equal_to("•●•".to_string());
        assert_that!(third.to_string()).is_equal_to("••●".to_string());
        assert_that!(returning.to_string()).is_equal_to("•●•".to_string());
        assert_that!(
            first
                .spans
                .iter()
                .filter(|span| span.style.fg == Some(Color::Cyan))
                .count()
        )
        .is_equal_to(1);
        assert_that!(
            second
                .spans
                .iter()
                .filter(|span| span.style.fg == Some(Color::Cyan))
                .count()
        )
        .is_equal_to(1);
        assert_that!(first).is_not_equal_to(second);
    }

    #[test]
    fn subtitle_settings_dialog_should_group_fields_and_hide_embedded_only_fields_when_exported() {
        let (mut app, directory) = test_app("subtitle-settings-ui", &[]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {
                        "index": 1,
                        "codec_type": "subtitle",
                        "codec_name": "subrip",
                        "tags": {"language": "eng", "title": "English dialogue"}
                    }
                ]
            }))
            .unwrap(),
        ));
        app.container_target = Some(ContainerFormat::Matroska);
        let source = SubtitleSource::Embedded(1);
        app.subtitle_changes.insert(
            source.clone(),
            SubtitleChange {
                source: source.clone(),
                source_format: SubtitleFormat::SubRip,
                embedded_target: None,
                export_target: Some(SubtitleFormat::SubRip),
                import_into_media: false,
                ocr_language: None,
                metadata: None,
            },
        );
        app.subtitle_settings_popup = Some(crate::app::SubtitleSettingsPopup {
            source: source.clone(),
            source_format: SubtitleFormat::SubRip,
            field: SubtitleSettingsField::Codec,
            mode: SubtitleSettingsMode::Summary,
            help_visible: false,
            codec_cursor: 0,
            language_cursor: 0,
            language_search: SearchState::default(),
            title_input: TextInputState::new("English dialogue".to_string()),
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();

        terminal
            .draw(|frame| render_subtitle_settings_dialog(frame, &app))
            .unwrap();
        let external_content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_that!(&external_content)
            .contains("Codec")
            .contains("Language")
            .contains("Forced")
            .contains("Hearing impaired")
            .does_not_contain("CC")
            .does_not_contain("Title")
            .does_not_contain("Default")
            .does_not_contain("Original")
            .does_not_contain("Commentary");

        app.subtitle_changes.remove(&source);
        terminal
            .draw(|frame| render_subtitle_settings_dialog(frame, &app))
            .unwrap();
        let embedded_content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_that!(&embedded_content)
            .contains("Title")
            .contains("Default")
            .contains("Hearing impaired")
            .does_not_contain("CC")
            .contains("Original")
            .contains("Commentary");
        let embedded_rows = terminal
            .backend()
            .buffer()
            .content
            .chunks(100)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let row = |label: &str| {
            embedded_rows
                .iter()
                .position(|line| line.contains(label))
                .unwrap()
        };
        assert_eq!(row("Default"), row("Title") + 2);
        assert_eq!(row("Hearing impaired"), row("Forced") + 2);
        assert_eq!(row("Original"), row("Hearing impaired") + 2);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_field_help_should_render_beside_or_below_the_editor_responsively() {
        let help = padded_popup_text(Text::from(Line::from(
            "Contextual information about the selected subtitle field.",
        )));

        let wide_frame = Rect::new(0, 0, 200, 30);
        let (wide_editor, wide_help) = subtitle_settings_dialog_areas(wide_frame, 16, Some(&help));
        let wide_help = wide_help.unwrap();
        assert_eq!(
            wide_editor,
            centered_fixed(wide_frame, SUBTITLE_SETTINGS_WIDTH, 16)
        );
        assert_eq!(wide_editor.y, wide_help.y);
        assert_eq!(wide_editor.height, wide_help.height);
        assert!(wide_editor.x + wide_editor.width < wide_help.x);

        let (narrow_editor, narrow_help) =
            subtitle_settings_dialog_areas(Rect::new(0, 0, 140, 30), 16, Some(&help));
        let narrow_help = narrow_help.unwrap();
        assert_eq!(narrow_editor.x, narrow_help.x);
        assert_eq!(narrow_editor.width, narrow_help.width);
        assert!(narrow_editor.y + narrow_editor.height < narrow_help.y);

        let (closed_editor, closed_help) = subtitle_settings_dialog_areas(wide_frame, 16, None);
        assert_eq!(closed_editor, wide_editor);
        assert!(closed_help.is_none());

        let cleared = combined_popup_area(wide_editor, Some(wide_help));
        assert_eq!(cleared.x, wide_editor.x);
        assert_eq!(cleared.y, wide_editor.y);
        assert_eq!(
            cleared.width,
            wide_editor.width + SUBTITLE_HELP_GAP + wide_help.width
        );
        assert_eq!(cleared.height, wide_editor.height);

        let title = subtitle_field_help_title(SubtitleSettingsField::Commentary);
        assert_eq!(title, " Information about Commentary ");
        assert!(!title.contains("close"));
    }

    #[test]
    fn details_selection_should_not_bleed_around_an_open_dialog() {
        let (mut app, directory) = test_app("dialog-selection", &[]);
        app.layer = Layer::Streams;
        app.selected_stream = 8;

        assert_eq!(details_selected_stream(&app), Some(8));
        app.dialog = Some(Dialog::SubtitleSettings);
        assert_eq!(details_selected_stream(&app), None);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_field_help_should_change_with_the_field_and_explain_sidecar_effects() {
        let (mut app, directory) = test_app("subtitle-field-help", &[]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {"index": 1, "codec_type": "subtitle", "codec_name": "subrip", "tags": {"language": "eng"}}
                ]
            }))
            .unwrap(),
        ));
        app.container_target = Some(ContainerFormat::Matroska);
        let sidecar_path = directory.join("movie.eng.srt");
        app.sidecars.push(SidecarEntry {
            path: sidecar_path.clone(),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        });
        let mut popup = SubtitleSettingsPopup {
            source: SubtitleSource::Sidecar(sidecar_path),
            source_format: SubtitleFormat::SubRip,
            field: SubtitleSettingsField::Language,
            mode: SubtitleSettingsMode::Summary,
            help_visible: true,
            codec_cursor: 0,
            language_cursor: 0,
            language_search: SearchState::default(),
            title_input: TextInputState::default(),
        };

        let language = subtitle_field_help_text(&app, &popup).to_string();
        assert_that!(language)
            .contains("does not translate")
            .contains("canonical language code");

        popup.field = SubtitleSettingsField::HearingImpaired;
        let hearing_impaired = subtitle_field_help_text(&app, &popup).to_string();
        assert_that!(hearing_impaired)
            .contains("deaf or hard-of-hearing")
            .contains("canonical .sdh");

        popup.source = SubtitleSource::Embedded(1);
        popup.field = SubtitleSettingsField::Original;
        let original = subtitle_field_help_text(&app, &popup).to_string();
        assert_that!(original)
            .contains("original language")
            .contains("originally recorded in")
            .does_not_contain("canonical .sdh");

        popup.field = SubtitleSettingsField::Codec;
        let codec = subtitle_field_help_text(&app, &popup).to_string();
        assert_that!(codec)
            .contains("requires seconv and tesseract")
            // The Codec field restates the chosen output format everywhere else on
            // screen, so its help panel no longer repeats it.
            .does_not_contain("stored as SubRip / SRT in MKV");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn render_footer_should_include_network_mode_tag_only_when_in_network_mode() {
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (conflict_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(
            std::env::temp_dir(),
            probe_tx,
            conflict_tx,
            edit_tx.clone(),
            edit_tx,
        )
        .unwrap();

        app.is_network_mount = true;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| render_footer(frame, &app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let content = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("[Network Mode]"));

        app.is_network_mount = false;
        terminal
            .draw(|frame| render_footer(frame, &app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let content_local = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(!content_local.contains("[Network Mode]"));
    }

    #[test]
    fn render_details_should_show_unsupported_format_only_for_non_video_outcomes() {
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (conflict_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(
            std::env::temp_dir(),
            probe_tx,
            conflict_tx,
            edit_tx.clone(),
            edit_tx,
        )
        .unwrap();
        app.files = vec![crate::files::FileEntry {
            path: std::path::PathBuf::from("/media/image.png"),
            display_name: "image.png".to_string(),
            fingerprint: crate::files::FileFingerprint {
                length: 100,
                modified: None,
            },
        }];
        app.list_state.select(Some(0));
        app.loading = false;

        // 1. Non-video outcome (PNG, JPEG, audio, NFO, etc.)
        app.outcome = Some(ProbeOutcome::NotVideo("No video stream found".to_string()));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| render_details(frame, &mut app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let content = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("Unsupported format"));

        // 2. Accepted video container outcome (MKV, MP4, AVI, WEBM, MOV, etc.)
        let info = MediaInfo::from_json(serde_json::json!({
            "format": {"format_name": "matroska", "duration": "120.0"},
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]
        }))
        .unwrap();
        app.outcome = Some(ProbeOutcome::Video(info));
        terminal
            .draw(|frame| render_details(frame, &mut app, frame.area()))
            .unwrap();
        let buffer_valid = terminal.backend().buffer();
        let content_valid = buffer_valid
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(!content_valid.contains("Unsupported format"));
    }

    #[test]
    fn render_files_should_reveal_only_matching_sidecars_during_search() {
        // Arrange
        let (mut app, directory) = test_app(
            "file-search-ui",
            &["movie.mkv", "movie.eng.srt", "movie.nld.srt"],
        );
        let movie_path = directory.join("movie.mkv");
        app.start_file_search();
        for ch in "eng".chars() {
            app.input_text_char(ch);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 12)).unwrap();

        // Act
        terminal
            .draw(|frame| render_files(frame, &mut app, frame.area()))
            .unwrap();
        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        // Assert
        assert_that!(&content)
            .contains("movie.mkv")
            .contains("movie.eng.srt")
            .contains("Search │ eng▏")
            .contains("│ (1 match)")
            .does_not_contain("movie.nld.srt");
        assert_that!(app.is_file_folded(&movie_path)).is_true();

        // Cleanup
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn truncate_should_keep_tail_with_ellipsis_when_value_exceeds_width() {
        // Arrange
        let value = "/a/long/path";
        let width = 6;

        // Act
        let result = truncate(value, width);

        // Assert
        assert_that!(result).is_equal_to("…/path".to_string());
    }

    #[test]
    fn truncate_should_return_original_value_when_value_fits_width() {
        // Arrange
        let value = "short";
        let width = 10;

        // Act
        let result = truncate(value, width);

        // Assert
        assert_that!(result).is_equal_to("short".to_string());
    }

    #[test]
    fn subtitle_columns_should_always_share_a_row_when_subtitles_exist() {
        // Act / Assert
        assert_that!(subtitle_columns_fit(10, 2, 2)).is_true();
        assert_that!(subtitle_columns_fit(10, 0, 2)).is_true();
        assert_that!(subtitle_columns_fit(10, 2, 0)).is_true();
        assert_that!(subtitle_columns_fit(10, 0, 0)).is_false();
    }

    #[test]
    fn subtitle_columns_should_truncate_each_side_independently() {
        // Act
        let line = subtitle_columns_line(
            Line::styled(
                "A very long embedded subtitle",
                Style::default().fg(Color::Yellow),
            ),
            Line::styled(
                "A very long external subtitle",
                Style::default().fg(Color::Cyan),
            ),
            12,
        );

        // Assert
        assert_that!(line.to_string()).is_equal_to("A very long…  A very long…".to_string());
        assert_that!(line.spans[0].style.fg).contains(Color::Yellow);
        assert_that!(line.spans[2].style.fg).contains(Color::Cyan);
    }

    #[test]
    fn processing_info_line_should_render_as_quiet_italic_text() {
        // Arrange
        let description = "Converting MKV to MP4".to_string();

        // Act
        let line = processing_info_line(description);

        // Assert
        assert_that!(line.to_string()).is_equal_to("Converting MKV to MP4".to_string());
        assert_eq!(line.style.fg, Some(Color::DarkGray));
        assert!(line.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn format_duration_should_include_hours_when_duration_exceeds_one_hour() {
        // Arrange
        let seconds = 3723.0;

        // Act
        let result = format_duration(seconds);

        // Assert
        assert_that!(result).is_equal_to("1:02:03".to_string());
    }

    #[test]
    fn format_bytes_should_use_binary_units_when_value_exceeds_one_mebibyte() {
        // Arrange
        let bytes = 1_572_864.0;

        // Act
        let result = format_bytes(bytes);

        // Assert
        assert_that!(result).is_equal_to("1.5 MiB".to_string());
    }

    #[test]
    fn format_bitrate_should_use_megabits_when_value_exceeds_one_megabit() {
        // Arrange
        let bits = 4_200_000.0;

        // Act
        let result = format_bitrate(bits);

        // Assert
        assert_that!(result).is_equal_to("4.2 Mb/s".to_string());
    }

    #[test]
    fn container_line_should_show_staged_format_metadata_and_conflicts() {
        // Arrange
        let info = MediaInfo::from_json(serde_json::json!({
            "format": {
                "format_name": "matroska,webm",
                "duration": "3723",
                "size": "1572864",
                "bit_rate": "4200000"
            },
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]
        }))
        .unwrap();

        // Act
        let line = container_line(
            &info,
            Some(crate::edit::ContainerFormat::Matroska),
            Some(crate::edit::ContainerFormat::Mp4),
            false,
            1,
            true,
        );

        // Assert
        assert_that!(line.to_string())
            .contains("MKV → MP4")
            .contains("1:02:03")
            .contains("1.5 MiB")
            // Bit rate is `i`-panel detail, not overview.
            .does_not_contain("4.2 Mb/s")
            .contains("1 compatibility conflict");
        assert_eq!(line.style.fg, Some(Color::White));
        assert_eq!(line.style.bg, Some(Color::Cyan));
        assert!(line.style.add_modifier.contains(Modifier::BOLD));
        assert!(line.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn container_line_should_not_be_marked_changed_when_metadata_is_not_changed() {
        // Arrange
        let info = MediaInfo::from_json(serde_json::json!({
            "format": {
                "format_name": "matroska,webm",
                "tags": { "title": "Existing Title" }
            },
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]
        }))
        .unwrap();
        // Act
        let line = container_line(&info, None, None, false, 0, false);

        // Assert: the overview row carries format, duration and size only. The title
        // lives in the `i` panel, so an unchanged file is styled plainly.
        assert_that!(line.to_string()).does_not_contain("Existing Title");
        assert_eq!(line.style.fg, None);
    }

    #[test]
    fn format_frame_rate_should_format_decimal_rate_when_input_is_fractional() {
        // Arrange
        let rate = "30000/1001";

        // Act
        let result = format_frame_rate(rate);

        // Assert
        assert_that!(result.as_deref()).contains("29.97");
    }

    #[test]
    fn stream_line_should_include_track_essentials_when_audio_metadata_is_present() {
        // Arrange
        let stream = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
            serde_json::json!({
                "index": 2,
                "codec_type": "audio",
                "codec_name": "opus",
                "sample_rate": "48000",
                "channel_layout": "5.1",
                "tags": {"language": "eng", "title": "Main"},
                "disposition": {"default": 1, "original": 1}
            }),
        )
        .unwrap();

        // Act
        let line = stream_line(&stream, 0, false, false, false, false, true).to_string();

        // Assert: format, channels, language written out, then flags in the same
        // bracketed shorthand a subtitle row uses.
        assert_that!(&line)
            .contains("#2")
            .contains("OPUS")
            .contains("5.1")
            .contains("English")
            .does_not_contain("ENG")
            .contains("[D]")
            // Title and sample rate are `i`-panel detail; `original` has never been
            // shown on an audio row.
            .does_not_contain("Main")
            .does_not_contain("48")
            .does_not_contain("OG");
    }

    #[test]
    fn stream_line_should_warn_before_an_incompatible_track() {
        // Arrange
        let stream = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
            serde_json::json!({
                "index": 2,
                "codec_type": "subtitle",
                "codec_name": "subrip"
            }),
        )
        .unwrap();

        // Act
        let line = stream_line(&stream, 0, false, false, false, true, false);

        // Assert
        assert_that!(line.to_string()).starts_with("⚠ #2");
        assert_eq!(line.spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(line.style.fg, Some(Color::Yellow));
        assert!(!line.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn stream_line_should_format_embedded_subtitle_with_metadata_tags() {
        // Arrange
        let stream = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
            serde_json::json!({
                "index": 2,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "eng", "title": "English"},
                "disposition": {"default": 1, "forced": 1, "hearing_impaired": 1}
            }),
        )
        .unwrap();

        // Act
        let line = stream_line(&stream, 0, false, false, false, false, true).to_string();

        // Assert
        assert_that!(&line)
            .contains("#2")
            .ends_with("SRT · ENG · English · [D/F/HI]")
            .does_not_contain("[Default]")
            .does_not_contain("[Forced]");
        assert_eq!(line.find("SRT"), Some(6));
    }

    #[test]
    fn stream_line_should_show_und_when_embedded_subtitle_has_no_language() {
        // Arrange
        let stream = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
            serde_json::json!({
                "index": 2,
                "codec_type": "subtitle",
                "codec_name": "ass"
            }),
        )
        .unwrap();

        // Act
        let line = stream_line(&stream, 0, false, false, false, false, false).to_string();

        // Assert
        assert_that!(line)
            .ends_with("ASS · UND")
            .does_not_contain("title")
            .does_not_contain("provided");
    }

    #[test]
    fn stream_line_should_preserve_changed_state_when_the_track_is_focused() {
        // Arrange
        let stream = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
            serde_json::json!({
                "index": 2,
                "codec_type": "subtitle",
                "codec_name": "subrip"
            }),
        )
        .unwrap();

        // Act
        let changed = stream_line(&stream, 0, false, false, true, false, false);
        let focused = stream_line(&stream, 0, true, false, true, false, false);

        // Assert
        assert_eq!(changed.style.fg, Some(Color::Yellow));
        assert!(changed.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(focused.style.fg, Some(Color::White));
        assert_eq!(focused.style.bg, Some(Color::Cyan));
        assert!(focused.style.add_modifier.contains(Modifier::ITALIC));
        assert_that!(focused.to_string()).starts_with("~ #2");
    }

    #[test]
    fn stream_line_should_keep_deletion_red_when_the_track_is_not_focused() {
        // Arrange
        let stream = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
            serde_json::json!({
                "index": 2,
                "codec_type": "subtitle",
                "codec_name": "subrip"
            }),
        )
        .unwrap();

        // Act
        let line = stream_line(&stream, 0, false, true, false, false, false);

        // Assert
        assert_that!(line.to_string()).starts_with("× #2");
        assert_eq!(line.style.fg, Some(Color::Red));
    }

    #[test]
    fn file_tree_lines_should_nest_related_sidecars_below_the_media_file() {
        // Arrange
        let sidecars = ["movie.eng.srt", "movie.nld.forced.ass"];

        // Act - Unfolded (false, sidecars present)
        let lines = file_tree_lines(
            "movie.mkv",
            sidecars,
            false,
            true,
            StagedFileStatus::Unstaged,
        );
        let text = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let text = text.iter().map(String::as_str).collect::<Vec<_>>();

        // Assert
        assert_that!(text).contains_exactly_in_given_order([
            "▿ movie.mkv",
            "  ├── movie.eng.srt",
            "  └── movie.nld.forced.ass",
        ]);
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::DarkGray));
        assert_eq!(lines[2].spans[1].style.fg, Some(Color::DarkGray));

        // Act - Folded (true, sidecars present)
        let folded_lines = file_tree_lines(
            "movie.mkv",
            sidecars,
            true,
            true,
            StagedFileStatus::Unstaged,
        );
        let folded_text = folded_lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let folded_strs = folded_text.iter().map(String::as_str).collect::<Vec<_>>();

        // Assert
        assert_that!(folded_strs).contains_exactly_in_given_order(["▹ movie.mkv"]);

        // Act - Standalone file when sidecars exist in folder
        let standalone_padded =
            file_tree_lines("other.mp4", [], false, true, StagedFileStatus::Unstaged);
        assert_eq!(standalone_padded[0].to_string(), "  other.mp4");

        // Act - Standalone file when NO sidecars exist in folder
        let standalone_unpadded =
            file_tree_lines("other.mp4", [], false, false, StagedFileStatus::Unstaged);
        assert_eq!(standalone_unpadded[0].to_string(), "other.mp4");
    }

    #[test]
    fn file_tree_lines_should_mark_staged_files_yellow_and_invalid_ones_with_a_warning() {
        let valid = file_tree_lines("movie.mkv", [], false, false, StagedFileStatus::Valid);
        assert_eq!(valid[0].to_string(), "movie.mkv");
        assert_eq!(valid[0].style, changed_style());

        let invalid = file_tree_lines(
            "movie.mkv",
            [],
            false,
            false,
            StagedFileStatus::Invalid("The file's tracks changed.".to_string()),
        );
        assert_eq!(invalid[0].to_string(), "⚠ movie.mkv");
        assert_eq!(invalid[0].style, warning_style(true));

        let unstaged = file_tree_lines("movie.mkv", [], false, false, StagedFileStatus::Unstaged);
        assert_eq!(unstaged[0].to_string(), "movie.mkv");
        assert_eq!(unstaged[0].style, Style::default());
    }

    #[test]
    fn sidecar_line_should_preserve_changed_state_when_the_sidecar_is_focused() {
        // Arrange
        let sidecar = SidecarEntry {
            path: std::path::PathBuf::from("movie.eng.srt"),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: crate::subtitle::SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: true,
            hearing_impaired: true,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        };

        // Act
        let changed = sidecar_line(&sidecar, false, true, false);
        let focused = sidecar_line(&sidecar, true, true, false);

        // Assert
        assert_eq!(changed.style.fg, Some(Color::Yellow));
        assert!(changed.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(focused.style.fg, Some(Color::White));
        assert_eq!(focused.style.bg, Some(Color::Cyan));
        assert!(focused.style.add_modifier.contains(Modifier::ITALIC));
        assert_that!(focused.to_string())
            .contains("SRT · ENG · [F/HI]")
            .does_not_contain("movie.eng.srt")
            .does_not_contain(" - ");
        assert_that!(focused.to_string()).starts_with("›     SRT");
    }

    #[test]
    fn sidecar_line_should_show_staged_title_when_sidecar_is_imported() {
        // Arrange
        let sidecar = SidecarEntry {
            path: std::path::PathBuf::from("movie.eng.srt"),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        };
        let change = SubtitleChange {
            source: SubtitleSource::Sidecar(sidecar.path.clone()),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: true,
            ocr_language: None,
            metadata: Some(crate::subtitle::SubtitleMetadata {
                language: "nld".to_string(),
                title: Some("Signs and songs".to_string()),
                forced: true,
                cc: false,
                hearing_impaired: false,
                original: false,
                commentary: false,
            }),
        };

        // Act
        let line = sidecar_line_with_subtitle_context(
            &sidecar,
            false,
            true,
            false,
            Some(&change),
            None,
            None,
        )
        .to_string();

        // Assert
        assert_that!(line).ends_with("SRT · NLD · Signs and songs · [F]");
    }

    #[test]
    fn subtitle_rows_should_show_the_staged_destination_format() {
        let sidecar = SidecarEntry {
            path: std::path::PathBuf::from("movie.eng.srt"),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        };
        let imported = SubtitleChange {
            source: SubtitleSource::Sidecar(sidecar.path.clone()),
            source_format: SubtitleFormat::SubRip,
            embedded_target: Some(SubtitleFormat::Ass),
            export_target: None,
            import_into_media: true,
            ocr_language: None,
            metadata: None,
        };
        let stream = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
            serde_json::json!({
                "index": 2,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "dan"}
            }),
        )
        .unwrap();
        let exported = SubtitleChange {
            source: SubtitleSource::Embedded(2),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: Some(SubtitleFormat::VobSub),
            import_into_media: false,
            ocr_language: None,
            metadata: None,
        };

        let imported_line = sidecar_line_with_subtitle_context(
            &sidecar,
            false,
            true,
            false,
            Some(&imported),
            None,
            None,
        )
        .to_string();
        let exported_line = stream_line_with_subtitle_context(
            &stream,
            0,
            false,
            false,
            true,
            false,
            false,
            Some(&exported),
            None,
            None,
        )
        .to_string();

        assert_that!(imported_line).ends_with("ASS · ENG");
        assert_that!(exported_line).ends_with("VobSub · DAN");
    }

    #[test]
    fn subtitle_overview_details_should_omit_title_when_no_title_is_provided() {
        // Arrange / Act
        let details = subtitle_overview_details(
            "ASS",
            "nld",
            None,
            SubtitleOverviewFlags {
                default: false,
                forced: true,
                cc: true,
                hearing_impaired: false,
                original: false,
                commentary: false,
            },
            None,
        );

        // Assert
        assert_that!(details)
            .is_equal_to("ASS · NLD · [F/CC]".to_string())
            .does_not_contain("title")
            .does_not_contain("provided");
    }

    #[test]
    fn subtitle_overview_details_should_combine_active_flags_when_title_is_present() {
        // Arrange / Act
        let details = subtitle_overview_details(
            "SRT",
            "eng",
            Some("English SDH"),
            SubtitleOverviewFlags {
                default: true,
                forced: true,
                cc: true,
                hearing_impaired: true,
                original: false,
                commentary: false,
            },
            None,
        );

        // Assert
        assert_that!(details).is_equal_to("SRT · ENG · English SDH · [D/F/CC/HI]".to_string());
    }

    #[test]
    fn subtitle_overview_details_should_truncate_only_title_when_width_is_limited() {
        // Arrange / Act
        let details = subtitle_overview_details(
            "SRT",
            "eng",
            Some("English SDH"),
            SubtitleOverviewFlags {
                default: true,
                forced: true,
                cc: true,
                hearing_impaired: false,
                original: false,
                commentary: false,
            },
            Some(30),
        );

        // Assert
        assert_that!(details).is_equal_to("SRT · ENG · Englis… · [D/F/CC]".to_string());
    }

    #[test]
    fn stream_line_should_use_staged_subtitle_metadata_when_metadata_was_edited() {
        // Arrange
        let stream = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
            serde_json::json!({
                "index": 2,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "eng", "title": "Old title"}
            }),
        )
        .unwrap();
        let change = SubtitleChange {
            source: SubtitleSource::Embedded(2),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: Some(crate::subtitle::SubtitleMetadata {
                language: "nld".to_string(),
                title: Some("New title".to_string()),
                forced: true,
                cc: true,
                hearing_impaired: false,
                original: false,
                commentary: false,
            }),
        };

        // Act
        let line = stream_line_with_subtitle_context(
            &stream,
            0,
            false,
            false,
            true,
            false,
            false,
            Some(&change),
            None,
            None,
        )
        .to_string();

        // Assert
        assert_that!(line)
            .ends_with("SRT · NLD · New title · [F/CC]")
            .does_not_contain("Old title");
    }

    #[test]
    fn subtitle_rows_should_hide_the_flags_the_target_container_cannot_store() {
        // Arrange: every flag set on both an embedded track and a sidecar being imported,
        // against each container. The overview must promise only what will survive the
        // remux — a `[CM]` on a row headed into MP4 is a lie the user acts on.
        let stream = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
            serde_json::json!({
                "index": 2, "codec_type": "subtitle", "codec_name": "subrip",
                "tags": {"language": "eng"}
            }),
        )
        .unwrap();
        let metadata = crate::subtitle::SubtitleMetadata {
            language: "eng".to_string(),
            title: None,
            forced: true,
            cc: true,
            hearing_impaired: true,
            original: true,
            commentary: true,
        };
        let sidecar = SidecarEntry {
            path: std::path::PathBuf::from("/media/movie.eng.srt"),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: true,
            hearing_impaired: true,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 2,
                modified: None,
            },
            companion_fingerprint: None,
        };

        for container in [
            ContainerFormat::Matroska,
            ContainerFormat::Mp4,
            ContainerFormat::Mov,
            ContainerFormat::WebM,
        ] {
            let expected = |flag: SubtitleFlag, tag: &str| {
                container
                    .supports_subtitle_flag(flag)
                    .then(|| tag.to_string())
            };

            // Act: the embedded track, staying embedded.
            let change = SubtitleChange {
                source: SubtitleSource::Embedded(2),
                source_format: SubtitleFormat::SubRip,
                embedded_target: None,
                export_target: None,
                import_into_media: false,
                ocr_language: None,
                metadata: Some(metadata.clone()),
            };
            let embedded = stream_line_with_subtitle_context(
                &stream,
                0,
                false,
                false,
                true,
                false,
                false,
                Some(&change),
                Some(container),
                None,
            )
            .to_string();

            // Act: the sidecar, being imported into the same container.
            let change = SubtitleChange {
                source: SubtitleSource::Sidecar(sidecar.path.clone()),
                import_into_media: true,
                metadata: Some(metadata.clone()),
                ..change
            };
            let imported = sidecar_line_with_subtitle_context(
                &sidecar,
                false,
                true,
                false,
                Some(&change),
                Some(container),
                None,
            )
            .to_string();

            // Assert: each flag appears exactly when the container can hold it. Sidecars
            // have no CC of their own — an imported CC track reads as hearing impaired.
            for (flag, tag) in [
                (SubtitleFlag::Forced, "F"),
                (SubtitleFlag::HearingImpaired, "HI"),
                (SubtitleFlag::Original, "O"),
                (SubtitleFlag::Commentary, "CM"),
            ] {
                let supported = expected(flag, tag).is_some();
                assert_eq!(
                    embedded.contains(tag),
                    supported,
                    "{container:?} / {flag:?}: embedded row was {embedded:?}",
                );
                assert_eq!(
                    imported.contains(tag),
                    supported,
                    "{container:?} / {flag:?}: imported row was {imported:?}",
                );
            }
            assert_eq!(
                embedded.contains("CC"),
                container.supports_subtitle_flag(SubtitleFlag::Cc),
                "{container:?}: embedded row was {embedded:?}",
            );
        }
    }

    #[test]
    fn stream_line_should_omit_original_title_when_staged_title_was_removed() {
        // Arrange
        let stream = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
            serde_json::json!({
                "index": 2,
                "codec_type": "subtitle",
                "codec_name": "subrip",
                "tags": {"language": "eng", "title": "Old title"}
            }),
        )
        .unwrap();
        let change = SubtitleChange {
            source: SubtitleSource::Embedded(2),
            source_format: SubtitleFormat::SubRip,
            embedded_target: None,
            export_target: None,
            import_into_media: false,
            ocr_language: None,
            metadata: Some(crate::subtitle::SubtitleMetadata {
                language: "eng".to_string(),
                title: None,
                forced: false,
                cc: false,
                hearing_impaired: false,
                original: false,
                commentary: false,
            }),
        };

        // Act
        let line = stream_line_with_subtitle_context(
            &stream,
            0,
            false,
            false,
            true,
            false,
            false,
            Some(&change),
            None,
            None,
        )
        .to_string();

        // Assert
        assert_that!(line)
            .ends_with("SRT · ENG")
            .does_not_contain("Old title")
            .does_not_contain("no title");
    }

    #[test]
    fn padded_popup_text_should_add_one_empty_line_before_and_after_content() {
        // Arrange
        let text = Text::from(vec![Line::from("first"), Line::from("last")]);

        // Act
        let padded = padded_popup_text(text);
        let lines = padded.lines.iter().map(Line::to_string).collect::<Vec<_>>();

        // Assert
        assert_that!(lines.first().unwrap().as_str()).is_equal_to("");
        assert_that!(lines.last().unwrap().as_str()).is_equal_to("");
        assert_that!(lines[1].as_str()).is_equal_to("first");
        assert_that!(lines[2].as_str()).is_equal_to("last");
    }

    #[test]
    fn container_information_lines_should_include_only_curated_human_readable_fields() {
        // Arrange
        let info = MediaInfo::from_json(serde_json::json!({
            "format": {
                "format_name": "matroska,webm",
                "format_long_name": "Matroska / WebM",
                "duration": "3723.0",
                "size": "1572864",
                "bit_rate": "4200000"
            },
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]
        }))
        .unwrap();

        // Act
        let lines =
            container_information_lines(&info, Path::new("/videos/movie.mkv"), 0, None, &|_| false);
        let text = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let text = text.iter().map(String::as_str).collect::<Vec<_>>();

        // Assert
        assert_that!(text).contains_exactly_in_given_order([
            "File name: movie.mkv",
            "Path: /videos/movie.mkv",
            "",
            "Duration: 01:02:03",
            "Size: 1.5 MiB",
            "",
            "Format: MKV (Matroska / WebM)",
            "",
            // The fields the container settings dialog edits, listed whether or not
            // they are set.
            "Title: Not provided",
            "Comment: Not provided",
            "Date: Not provided",
            "Genre: Not provided",
            "Artist: Not provided",
        ]);
    }

    #[test]
    fn video_information_lines_should_replace_raw_fields_with_a_friendly_summary() {
        // Arrange
        let stream = serde_json::from_value(serde_json::json!({
            "index": 0,
            "codec_type": "video",
            "codec_name": "h264",
            "profile": "High",
            "width": 1920,
            "height": 1080,
            "display_aspect_ratio": "16:9",
            "avg_frame_rate": "24000/1001",
            "pix_fmt": "yuv420p10le",
            "color_transfer": "smpte2084",
            "field_order": "progressive",
            "bit_rate": "4200000",
            "time_base": "1/24000",
            "start_pts": 0,
            "codec_tag_string": "avc1",
            "disposition": {
                "default": 1,
                "comment": 1,
                "original": 1
            },
            "tags": {
                "title": "Main feature",
                "language": "eng"
            }
        }))
        .unwrap();

        // Act
        let text = video_information_lines(&stream, true)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        let rendered = text.join("\n");

        // Assert
        assert_that!(text).contains_exactly_in_given_order([
            "Title: Main feature".to_string(),
            "".to_string(),
            "Language: English".to_string(),
            "Role: Default · Commentary · Original".to_string(),
            "".to_string(),
            "Format: H.264 (AVC)".to_string(),
            "Resolution: 1920×1080 · 16:9 · 1080p".to_string(),
            "Frame rate: 23.98 fps".to_string(),
            "Picture: HDR10 · 10-bit · Progressive".to_string(),
            "Bitrate: 4.2 Mb/s".to_string(),
        ]);
        assert_that!(rendered)
            .does_not_contain("time_base")
            .does_not_contain("start_pts")
            .does_not_contain("codec_tag")
            .does_not_contain("profile");
    }

    #[test]
    fn video_information_lines_should_omit_unavailable_optional_information() {
        // Arrange
        let stream = serde_json::from_value(serde_json::json!({
            "index": 0,
            "codec_type": "video",
            "codec_name": "hevc",
            "width": 1280,
            "height": 720
        }))
        .unwrap();

        // Act
        let text = video_information_lines(&stream, false)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        // Assert
        assert_that!(text).contains_exactly_in_given_order([
            "Title: Not provided".to_string(),
            "".to_string(),
            "Format: HEVC (H.265)".to_string(),
            "Resolution: 1280×720 · 720p".to_string(),
        ]);
    }

    #[test]
    fn audio_information_lines_should_translate_codec_channels_and_roles() {
        // Arrange
        let stream = serde_json::from_value(serde_json::json!({
            "index": 1,
            "codec_type": "audio",
            "codec_name": "eac3",
            "channels": 6,
            "channel_layout": "5.1(side)",
            "sample_rate": "48000",
            "bit_rate": "640000",
            "time_base": "1/48000",
            "disposition": {
                "default": 1,
                "comment": 1,
                "original": 1
            },
            "tags": {
                "language": "eng",
                "title": "English surround"
            }
        }))
        .unwrap();

        // Act
        let text = audio_information_lines(&stream, true)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        let rendered = text.join("\n");

        // Assert
        assert_that!(text).contains_exactly_in_given_order([
            "Title: English surround".to_string(),
            "".to_string(),
            "Language: English".to_string(),
            "Role: Default · Commentary · Original".to_string(),
            "".to_string(),
            "Format: Dolby Digital Plus (E-AC-3)".to_string(),
            "Channels: 5.1 surround".to_string(),
            "Bitrate: 640 kb/s".to_string(),
            "Sample rate: 48 kHz".to_string(),
        ]);
        assert_that!(rendered).does_not_contain("time_base");
    }

    #[test]
    fn subtitle_information_lines_should_explain_embedded_text_subtitles() {
        // Arrange
        let stream = serde_json::from_value(serde_json::json!({
            "index": 2,
            "codec_type": "subtitle",
            "codec_name": "subrip",
            "time_base": "1/1000",
            "disposition": {
                "default": 1,
                "forced": 1,
                "hearing_impaired": 1,
                "original": 1
            },
            "tags": {
                "language": "eng",
                "title": "English SDH"
            }
        }))
        .unwrap();

        // Act
        let text = embedded_subtitle_information_lines(&stream, true, None)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        let rendered = text.join("\n");

        // Assert
        assert_that!(text).contains_exactly_in_given_order([
            "Title: English SDH".to_string(),
            "".to_string(),
            "Language: English".to_string(),
            // Every flag says where it stands, so an unset one is distinguishable from
            // one that does not apply to this track at all.
            "Default: Yes".to_string(),
            "Forced: Yes".to_string(),
            "Closed captions: No".to_string(),
            "Hearing impaired: Yes".to_string(),
            "Original: Yes".to_string(),
            "Commentary: No".to_string(),
            "".to_string(),
            "Format: SubRip (SRT)".to_string(),
            "Type: Text-based".to_string(),
        ]);
        assert_that!(rendered)
            .does_not_contain("time_base")
            .does_not_contain("Source:");
    }

    #[test]
    fn subtitle_language_description_should_translate_the_language() {
        // Act
        let language = subtitle_language_description("eng");

        // Assert
        assert_that!(language.as_deref()).contains("English");
    }

    #[test]
    fn language_name_should_translate_iso_codes_instead_of_showing_abbreviations() {
        // Assert
        assert_that!(language_name("ARA").as_deref()).contains("Arabic");
        assert_that!(language_name("est").as_deref()).contains("Estonian");
        assert_that!(language_name("fre").as_deref()).contains("French");
        assert_that!(language_name("en-US").as_deref()).contains("English");
        assert_that!(language_name("qaa").as_deref()).contains("Unknown language (QAA)");
    }

    #[test]
    fn sidecar_information_lines_should_group_file_language_role_and_format() {
        // Arrange
        let sidecar = SidecarEntry {
            path: std::path::PathBuf::from("movie.nld.forced.cc.sup"),
            companion: None,
            display_name: "movie.nld.forced.cc.sup".to_string(),
            format: SubtitleFormat::Pgs,
            language: "nld".to_string(),
            forced: true,
            hearing_impaired: true,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        };

        // Act
        let text = sidecar_subtitle_information_lines(&sidecar, None)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        // Assert
        assert_that!(text).contains_exactly_in_given_order([
            "File: movie.nld.forced.cc.sup".to_string(),
            "".to_string(),
            "Language: Dutch".to_string(),
            // An external sidecar can carry only these two, so the rest are absent
            // rather than listed as "No".
            "Forced: Yes".to_string(),
            "Hearing impaired: Yes".to_string(),
            "".to_string(),
            "Format: PGS / SUP".to_string(),
            "Type: Image-based".to_string(),
        ]);
    }

    #[test]
    fn subtitle_information_should_follow_staged_values_and_effective_container_role() {
        // Arrange
        let (mut app, directory) = test_app("staged-subtitle-information", &[]);
        let stream: BTreeMap<String, Value> = serde_json::from_value(serde_json::json!({
            "index": 1,
            "codec_type": "subtitle",
            "codec_name": "subrip",
            "disposition": {
                "default": 0,
                "forced": 0,
                "hearing_impaired": 0,
                "original": 0,
                "comment": 0
            },
            "tags": {"language": "eng", "title": "Original title"}
        }))
        .unwrap();
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    stream.clone()
                ]
            }))
            .unwrap(),
        ));
        app.container_target = Some(crate::edit::ContainerFormat::Mp4);
        let embedded_source = SubtitleSource::Embedded(1);
        let staged_metadata = crate::subtitle::SubtitleMetadata {
            language: "nld".to_string(),
            title: Some("Nederlandse ondertiteling".to_string()),
            forced: true,
            cc: true,
            hearing_impaired: true,
            original: true,
            commentary: true,
        };
        app.default_streams.insert(1);
        app.subtitle_changes.insert(
            embedded_source.clone(),
            SubtitleChange {
                source: embedded_source.clone(),
                source_format: SubtitleFormat::SubRip,
                embedded_target: Some(SubtitleFormat::Ass),
                export_target: None,
                import_into_media: false,
                ocr_language: None,
                metadata: Some(staged_metadata.clone()),
            },
        );

        let sidecar = SidecarEntry {
            path: directory.join("movie.eng.srt"),
            companion: None,
            display_name: "movie.eng.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "eng".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        };
        let sidecar_source = SubtitleSource::Sidecar(sidecar.path.clone());
        app.sidecars.push(sidecar.clone());
        app.default_sidecars.insert(0);
        app.subtitle_changes.insert(
            sidecar_source.clone(),
            SubtitleChange {
                source: sidecar_source.clone(),
                source_format: SubtitleFormat::SubRip,
                embedded_target: Some(SubtitleFormat::Ass),
                export_target: None,
                import_into_media: true,
                ocr_language: None,
                metadata: Some(staged_metadata),
            },
        );

        // Act
        let embedded_state = app
            .subtitle_display_state(&embedded_source, SubtitleFormat::SubRip)
            .unwrap();
        let embedded_lines =
            embedded_subtitle_information_lines(&stream, false, Some(&embedded_state));
        let imported_state = app
            .subtitle_display_state(&sidecar_source, SubtitleFormat::SubRip)
            .unwrap();
        let imported_lines = sidecar_subtitle_information_lines(&sidecar, Some(&imported_state));

        // Assert
        for text in [&embedded_lines, &imported_lines].map(|lines| {
            lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        }) {
            assert_that!(text)
                .contains("Title: Nederlandse ondertiteling")
                .contains("Language: Dutch")
                .contains("Closed captions: Yes")
                .contains("Hearing impaired: Yes")
                .contains("Default: Yes")
                .contains("Forced: Yes")
                .contains("Commentary: Yes")
                .contains("Format: ASS")
                .contains("Type: Text-based");
        }
        let staged_title = embedded_lines
            .iter()
            .find(|line| line.to_string().starts_with("Title:"))
            .unwrap();
        assert_that!(staged_title.spans[1].style.fg).contains(Color::Yellow);
        assert_that!(
            staged_title.spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        )
        .is_true();
        let staged_hearing_impaired = embedded_lines
            .iter()
            .find(|line| line.to_string().starts_with("Hearing impaired:"))
            .unwrap();
        assert_that!(staged_hearing_impaired.spans[1].style.fg).contains(Color::Yellow);

        // Act: move both tracks to their effective external state.
        let embedded_change = app.subtitle_changes.get_mut(&embedded_source).unwrap();
        embedded_change.embedded_target = None;
        embedded_change.export_target = Some(SubtitleFormat::WebVtt);
        let sidecar_change = app.subtitle_changes.get_mut(&sidecar_source).unwrap();
        sidecar_change.embedded_target = None;
        sidecar_change.export_target = Some(SubtitleFormat::WebVtt);
        sidecar_change.import_into_media = false;
        let exported_state = app
            .subtitle_display_state(&embedded_source, SubtitleFormat::SubRip)
            .unwrap();
        let external_state = app
            .subtitle_display_state(&sidecar_source, SubtitleFormat::SubRip)
            .unwrap();
        let exported = embedded_subtitle_information_lines(&stream, false, Some(&exported_state))
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let external = sidecar_subtitle_information_lines(&sidecar, Some(&external_state))
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        // Assert
        for text in [exported, external] {
            assert_that!(text)
                .contains("Language: Dutch")
                .contains("Hearing impaired: Yes")
                .contains("Forced: Yes")
                .contains("Format: WebVTT")
                .does_not_contain("Closed captions:")
                .does_not_contain("Title:")
                .does_not_contain("Default")
                .does_not_contain("Original")
                .does_not_contain("Commentary");
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stream_details_popup_should_show_friendly_video_audio_and_subtitle_information() {
        // Arrange
        let (mut app, directory) = test_app("video-details-test", &["movie.mkv"]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [{
                    "index": 0,
                    "codec_type": "video",
                    "codec_name": "h264",
                    "width": 1920,
                    "height": 1080,
                    "time_base": "1/24000",
                    "disposition": {"default": 1}
                }, {
                    "index": 1,
                    "codec_type": "audio",
                    "codec_name": "ac3",
                    "channels": 2,
                    "tags": {"language": "nld"},
                    "time_base": "1/48000"
                }, {
                    "index": 2,
                    "codec_type": "subtitle",
                    "codec_name": "hdmv_pgs_subtitle",
                    "tags": {"language": "eng"},
                    "time_base": "1/1000"
                }]
            }))
            .unwrap(),
        ));
        app.loading = false;
        app.stream_order = vec![0, 1, 2];
        app.default_streams.insert(0);
        app.sidecars.push(SidecarEntry {
            path: directory.join("movie.nld.srt"),
            companion: None,
            display_name: "movie.nld.srt".to_string(),
            format: SubtitleFormat::SubRip,
            language: "nld".to_string(),
            forced: false,
            hearing_impaired: false,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        });
        app.layer = Layer::Streams;
        app.selected_stream = 1;
        app.open_stream_details();

        // Act
        app.selected_stream = 0;
        let (container, container_title) = details_popup_content(&app).unwrap();
        app.selected_stream = 1;
        let (video, video_title) = details_popup_content(&app).unwrap();
        app.selected_stream = 2;
        let (audio, audio_title) = details_popup_content(&app).unwrap();
        app.selected_stream = 3;
        let (subtitle, subtitle_title) = details_popup_content(&app).unwrap();
        app.selected_stream = 4;
        let (external, external_title) = details_popup_content(&app).unwrap();

        // Assert
        assert_that!(container_title).is_equal_to(" Container information ".to_string());
        assert_that!(container.to_string())
            .contains("File name: movie.mkv")
            .contains("Format: MKV");
        assert_that!(video_title).is_equal_to(" Video #0 ".to_string());
        assert_that!(video.to_string())
            .contains("Format: H.264 (AVC)")
            .contains("Resolution: 1920×1080 · 1080p")
            .does_not_contain("time_base");
        assert_that!(audio_title).is_equal_to(" Audio #1 ".to_string());
        assert_that!(audio.to_string())
            .contains("Format: Dolby Digital (AC-3)")
            .contains("Channels: Stereo")
            .contains("Language: Dutch")
            .does_not_contain("time_base");
        assert_that!(subtitle_title).is_equal_to(" Subtitle #2 ".to_string());
        assert_that!(subtitle.to_string())
            .contains("Title: Not provided")
            .contains("Format: PGS / SUP")
            .contains("Type: Image-based")
            .does_not_contain("Source:")
            .does_not_contain("time_base");
        assert_that!(external_title).is_equal_to(" External subtitle ".to_string());
        assert_that!(external.to_string())
            .contains("Format: SubRip (SRT)")
            .contains("Language: Dutch")
            .does_not_contain("Source:")
            .contains("File: movie.nld.srt");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn every_details_popup_should_be_the_same_width_whatever_it_describes() {
        // Arrange: a short panel and a long one. Sizing to content used to make these
        // two different shapes, which is what made the `i` panels look inconsistent.
        let terminal = Rect::new(0, 0, 120, 40);
        let short = padded_popup_text(Text::from(vec![
            Line::from("Title: English SDH"),
            Line::from("Format: SubRip (SRT)"),
        ]));
        let long = padded_popup_text(Text::from(Line::from("x".repeat(190))));

        // Act
        let short_area = details_popup_area(terminal, &short, " Subtitle #2 ");
        let long_area = details_popup_area(terminal, &long, " Container information ");

        // Assert: one width, centred, and only the height follows the content.
        assert_that!(short_area.width).is_equal_to(72);
        assert_that!(long_area.width).is_equal_to(short_area.width);
        assert_that!(short_area.x).is_equal_to(long_area.x);
        assert_that!(short_area.height).is_equal_to(6);
        assert_that!(long_area.height).is_equal_to(7);
    }

    #[test]
    fn a_details_popup_should_still_fit_its_own_title() {
        // Arrange: a terminal narrow enough that 60% is shorter than the title.
        let terminal = Rect::new(0, 0, 40, 20);
        let text = padded_popup_text(Text::from(Line::from("short")));

        // Act
        let area = details_popup_area(terminal, &text, " Container information ");

        // Assert: the title is never clipped by the percentage.
        assert_that!(area.width as usize)
            .is_greater_than_or_equal_to(" Container information ".chars().count());
    }

    #[test]
    fn container_information_lines_should_follow_staged_metadata_and_mark_it_changed() {
        // Arrange: what is stored, and what the settings dialog has staged over it.
        let info = MediaInfo::from_json(serde_json::json!({
            "format": {
                "format_name": "matroska,webm",
                "tags": {"title": "Stored title", "genre": "Drama"}
            },
            "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264"}]
        }))
        .unwrap();
        let staged = crate::edit::ContainerMetadata {
            title: Some("Staged title".to_string()),
            genre: Some("Drama".to_string()),
            ..Default::default()
        };

        // Act
        let lines = container_information_lines(
            &info,
            Path::new("/videos/movie.mkv"),
            0,
            Some(&staged),
            &|field| field == ContainerSettingsField::Title,
        );

        // Assert: the panel shows what would be written, not what is on disk.
        let text = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        assert_that!(text.iter().map(String::as_str).collect::<Vec<_>>())
            .contains("Title: Staged title")
            .contains("Genre: Drama")
            .contains("Comment: Not provided")
            .does_not_contain("Title: Stored title");

        // Assert: only the staged field is coloured, the same yellow the settings row
        // and the subtitle panel use.
        let value_style = |needle: &str| {
            lines
                .iter()
                .find(|line| line.to_string().starts_with(needle))
                .and_then(|line| line.spans.last())
                .map(|span| span.style.fg)
                .expect("the field should be rendered")
        };
        assert_that!(value_style("Title:")).is_equal_to(Some(Color::Yellow));
        assert_that!(value_style("Genre:")).is_not_equal_to(Some(Color::Yellow));
    }

    #[test]
    fn container_information_lines_should_identify_mp4_despite_quicktime_family_name() {
        // Arrange
        let info = MediaInfo::from_json(serde_json::json!({
            "format": {
                "format_name": "mov,mp4,m4a,3gp,3g2,mj2",
                "format_long_name": "QuickTime / MOV"
            },
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264"}
            ]
        }))
        .unwrap();

        // Act
        let lines =
            container_information_lines(&info, Path::new("/videos/movie.mp4"), 0, None, &|_| false);
        let text = lines.iter().map(Line::to_string).collect::<Vec<_>>();

        // Assert
        assert_that!(text).contains("Format: MP4 (QuickTime / MOV)".to_string());
    }

    #[test]
    fn format_duration_24h_should_include_zero_padded_hours_for_short_media() {
        // Act
        let duration = format_duration_24h(60.0);

        // Assert
        assert_that!(duration).is_equal_to("00:01:00".to_string());
    }

    #[test]
    fn max_scroll_should_count_hidden_lines_when_content_exceeds_viewport() {
        // Arrange
        let text = Text::from(vec![
            Line::from("1234567890"),
            Line::from("abcdefghij"),
            Line::from("last"),
        ]);
        let area = Rect::new(0, 0, 12, 4);

        // Act
        let result = max_scroll(&text, area);

        // Assert
        assert_that!(result).is_equal_to(1);
    }

    #[test]
    fn max_scroll_should_account_for_wrapping_when_viewport_is_narrow() {
        // Arrange
        let text = Text::from(vec![
            Line::from("1234567890"),
            Line::from("abcdefghij"),
            Line::from("last"),
        ]);
        let area = Rect::new(0, 0, 7, 4);

        // Act
        let result = max_scroll(&text, area);

        // Assert
        assert_that!(result).is_equal_to(3);
    }

    #[test]
    fn max_scroll_should_return_zero_when_content_fits_viewport() {
        // Arrange
        let text = Text::from("short");
        let area = Rect::new(0, 0, 20, 10);

        // Act
        let result = max_scroll(&text, area);

        // Assert
        assert_that!(result).is_equal_to(0);
    }

    #[test]
    fn scroll_to_show_line_should_return_zero_when_selected_line_is_first() {
        // Arrange
        let text = Text::from(vec![
            Line::from("zero"),
            Line::from("one"),
            Line::from("two"),
            Line::from("three"),
            Line::from("four"),
        ]);
        let area = Rect::new(0, 0, 20, 5);

        // Act
        let result = scroll_to_show_line(&text, area, 0, 2);

        // Assert
        assert_that!(result).is_equal_to(0);
    }

    #[test]
    fn scroll_to_show_line_should_scroll_down_when_selected_line_is_below_viewport() {
        // Arrange
        let text = Text::from(vec![
            Line::from("zero"),
            Line::from("one"),
            Line::from("two"),
            Line::from("three"),
            Line::from("four"),
        ]);
        let area = Rect::new(0, 0, 20, 5);

        // Act
        let result = scroll_to_show_line(&text, area, 4, 0);

        // Assert
        assert_that!(result).is_equal_to(2);
    }

    #[test]
    fn scroll_to_show_line_should_keep_position_when_selected_line_is_visible() {
        // Arrange
        let text = Text::from(vec![
            Line::from("zero"),
            Line::from("one"),
            Line::from("two"),
            Line::from("three"),
            Line::from("four"),
        ]);
        let area = Rect::new(0, 0, 20, 5);

        // Act
        let result = scroll_to_show_line(&text, area, 2, 0);

        // Assert
        assert_that!(result).is_equal_to(0);
    }

    #[test]
    fn keybindings_text_should_include_active_bindings_when_help_is_rendered() {
        // Arrange
        let expected = [
            "General",
            "Track editing",
            "Text input",
            "Esc",
            "gg / G",
            "Ctrl-j / Ctrl-k",
            "Ctrl-s",
            "Explain the highlighted subtitle field",
            "i",
            "Ctrl-d / Ctrl-u",
            "Ctrl-n / Ctrl-p",
            "Home/End",
            "Backspace/Delete",
            "languages",
        ];

        // Act
        let help = keybindings_text().to_string();

        // Assert
        for value in expected {
            assert_that!(&help).contains(value);
        }
        // Cancelling a batch goes through the confirm dialog only; there is no
        // immediate-stop chord left to advertise.
        assert_that!(&help).does_not_contain("Cancel processing");
    }

    #[test]
    fn filter_keybindings_text_should_match_substring_case_insensitively() {
        let raw = keybindings_text();
        let (filtered, count) = filter_keybindings_text(raw, "track");
        let content = filtered.to_string();

        assert_that!(&content).contains("Move track down / up");
        assert_that!(&content).does_not_contain("Open or close keybindings");
        assert_eq!(count, 2);
    }

    #[test]
    fn filter_keybindings_text_should_return_all_lines_when_query_is_empty() {
        let raw = keybindings_text();
        let (filtered, count) = filter_keybindings_text(raw.clone(), "");

        assert_eq!(filtered.lines.len(), raw.lines.len());
        assert!(count > 0);
    }

    #[test]
    fn keybindings_text_should_exclude_space_binding_when_space_action_is_removed() {
        // Arrange

        // Act
        let help = keybindings_text().to_string();

        // Assert
        assert_that!(help).does_not_contain("Space");
    }

    #[test]
    fn keybindings_text_should_exclude_removed_default_track_binding() {
        // Act
        let help = keybindings_text().to_string();

        // Assert
        assert_that!(help).does_not_contain("Make track the default");
    }

    #[test]
    fn keybindings_text_should_exclude_refresh_when_files_are_live() {
        // Arrange

        // Act
        let help = keybindings_text().to_string();

        // Assert
        assert_that!(help).does_not_contain("Refresh");
    }

    #[test]
    fn container_choice_line_should_place_warning_beside_the_format() {
        // Arrange
        let choice = ContainerChoice {
            value: Some(crate::edit::ContainerFormat::Mp4),
            label: "MP4".to_string(),
            current: false,
            staged: false,
            conflicts: vec![
                "MP4 can't contain SUBRIP subtitle track #2.".to_string(),
                "MP4 can't contain SUBRIP subtitle track #3.".to_string(),
                "MP4 can't contain ASS subtitle track #4.".to_string(),
            ],
        };

        // Act
        let line = container_choice_line(&choice, false, true);
        let focused = container_choice_line(&choice, true, true);

        // Assert
        assert_that!(line.to_string())
            .contains("MP4  ⚠ can't contain SubRip/SRT or ASS subtitles.");
        assert_eq!(line.spans[0].content, "  └── ");
        assert_eq!(line.spans[0].style.fg, Some(Color::DarkGray));
        assert_eq!(line.spans[2].style.fg, Some(Color::Yellow));
        assert_eq!(focused.spans[2].style.fg, Some(Color::White));
        assert_eq!(focused.spans[2].style.bg, Some(Color::Cyan));
    }

    #[test]
    fn subtitle_codec_line_should_distinguish_staged_and_available_codecs() {
        // Act
        let available = subtitle_codec_line("WebVTT", false, false, true, false);
        let staged = subtitle_codec_line("ASS", false, true, true, false);
        let staged_cursor = subtitle_codec_line("ASS", true, true, true, true);

        // Assert — no "(original)" tag is ever shown, regardless of state.
        assert_that!(available.to_string())
            .does_not_contain("●")
            .does_not_contain("(original)");
        assert_eq!(available.spans[1].style.fg, Some(Color::White));
        assert_eq!(staged.spans[1].style.fg, Some(Color::Yellow));
        assert!(
            staged.spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(staged_cursor.spans[1].style.fg, Some(Color::White));
        assert_eq!(staged_cursor.spans[1].style.bg, Some(Color::Cyan));
        assert!(
            staged_cursor.spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );

        // The guide glyph flips between the two tree connectors based on `last`.
        assert_eq!(available.spans[0].content, "  ├── ");
        assert_eq!(available.spans[0].style.fg, Some(Color::DarkGray));
        assert_eq!(staged_cursor.spans[0].content, "  └── ");
    }

    #[test]
    fn setting_line_should_italicize_only_a_changed_value() {
        // Act
        let unchanged = setting_line("Codec", "SubRip / SRT", false, false, false);
        let changed = setting_line("Codec", "ASS", false, true, false);
        let focused_changed = setting_line("Codec", "ASS", true, true, false);
        let expanded = setting_line("Codec", "ASS", false, false, true);

        // Assert
        assert!(
            !unchanged.spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert!(
            changed.spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(changed.spans[1].style.fg, Some(Color::Yellow));
        assert_eq!(focused_changed.spans[1].style.fg, Some(Color::White));
        assert_eq!(focused_changed.spans[1].style.bg, Some(Color::Cyan));
        assert!(
            focused_changed.spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );

        // The chevron flips between collapsed and expanded markers.
        assert_that!(unchanged.to_string()).contains("▹ Codec");
        assert_that!(expanded.to_string()).contains("▿ Codec");
    }

    #[test]
    fn subtitle_checkbox_lines_should_align_every_checkbox() {
        for label in [
            "Default",
            "Forced",
            "CC",
            "Hearing impaired",
            "Original",
            "Commentary",
        ] {
            let line = subtitle_checkbox_line(label, false, false, false, None);

            assert_eq!(
                line.to_string().chars().position(|glyph| glyph == '['),
                Some(FIELD_VALUE_COLUMN),
                "checkbox {label} should put its box on the shared value column",
            );
        }
    }

    #[test]
    fn every_field_row_should_place_its_value_chrome_in_the_same_column() {
        // Arrange: one row of each kind, using the widest label the popup can show.
        let mut input = TextInputState::new(String::new());
        input.activate();
        let rows = [
            setting_line("Codec", "SubRip / SRT", false, false, false),
            setting_line("Hearing impaired", "value", false, false, true),
            subtitle_checkbox_line("Hearing impaired", true, false, false, None),
            text_field_line(TextField::new(
                "Title",
                FieldValue::Editing(&input),
                TextInputConfig::SUBTITLE_TITLE.width,
            )),
            text_field_line(TextField::new(
                "Hearing impaired",
                FieldValue::Static("value"),
                TextInputConfig::CONTAINER_METADATA.width,
            )),
        ];

        // Assert: every row's chrome starts on the one shared column.
        for row in rows {
            let rendered = row.to_string();
            let chrome = rendered
                .chars()
                .position(|glyph| glyph == '[' || FIELD_FRAME.starts_with(glyph))
                .unwrap_or_else(|| panic!("row {rendered:?} should show value chrome"));
            assert_eq!(chrome, FIELD_VALUE_COLUMN, "row {rendered:?} is off-grid");
        }
    }

    #[test]
    fn stacked_fields_should_frame_only_the_selected_row() {
        // Arrange: the container popup's five metadata fields, Comment selected.
        let idle = TextInputState::new("Big Buck Bunny".to_string());
        let selected = TextInputState::new(String::new());
        let rows = [
            text_field_line(
                TextField::new(
                    "Title",
                    FieldValue::Editing(&idle),
                    TextInputConfig::CONTAINER_METADATA.width,
                )
                .selected(false),
            ),
            text_field_line(
                TextField::new(
                    "Comment",
                    FieldValue::Editing(&selected),
                    TextInputConfig::CONTAINER_METADATA.width,
                )
                .selected(true),
            ),
            text_field_line(
                TextField::new(
                    "Date",
                    FieldValue::Editing(&idle),
                    TextInputConfig::CONTAINER_METADATA.width,
                )
                .selected(false),
            ),
        ];

        // Assert: every row is framed with the same glyph and only its colour marks
        // the selection, so selecting a row never changes the width of anything. No row
        // is filled, so stacked fields cannot merge into one block.
        for (index, row) in rows.iter().enumerate() {
            let frames = row
                .spans
                .iter()
                .filter(|span| span.content.as_ref() == FIELD_FRAME)
                .collect::<Vec<_>>();
            assert_eq!(
                frames.len(),
                2,
                "row {index} should be framed on both sides"
            );
            for frame in frames {
                if index == 1 {
                    assert_eq!(frame.style.fg, Some(Color::Cyan));
                    assert!(frame.style.add_modifier.contains(Modifier::BOLD));
                } else {
                    assert_eq!(frame.style.fg, Some(Color::DarkGray));
                }
            }
            assert!(
                row.spans
                    .iter()
                    .all(|span| span.style.bg != Some(FIELD_EDITING_SURFACE)),
                "row {index} should stay unfilled while no field is being edited",
            );
        }
    }

    #[test]
    fn an_editing_field_should_brighten_its_background_and_show_a_caret() {
        // Arrange
        let mut editing = TextInputState::new("Movie".to_string());
        editing.activate();
        let idle = TextInputState::new("Movie".to_string());

        // Act
        let active = text_field_line(
            TextField::new(
                "Title",
                FieldValue::Editing(&editing),
                TextInputConfig::SUBTITLE_TITLE.width,
            )
            .selected(true),
        );
        let inactive = text_field_line(
            TextField::new(
                "Title",
                FieldValue::Editing(&idle),
                TextInputConfig::SUBTITLE_TITLE.width,
            )
            .selected(true),
        );

        // Assert: selection alone shows the focused frame; only editing fills and
        // shows the caret.
        assert_that!(active.to_string()).contains(FIELD_CARET);
        assert!(
            active
                .spans
                .iter()
                .any(|span| span.style.bg == Some(FIELD_EDITING_SURFACE))
        );
        assert_that!(inactive.to_string())
            .does_not_contain(FIELD_CARET)
            .contains(FIELD_FRAME);
        assert!(
            inactive
                .spans
                .iter()
                .all(|span| span.style.bg != Some(FIELD_EDITING_SURFACE))
        );
    }

    #[test]
    fn an_editing_field_should_fill_every_cell_from_bar_to_bar() {
        // Arrange: a short value, so most of the field is padding.
        let mut editing = TextInputState::new("Movie".to_string());
        editing.activate();

        // Act
        let line = text_field_line(
            TextField::new(
                "Title",
                FieldValue::Editing(&editing),
                TextInputConfig::SUBTITLE_TITLE.width,
            )
            .selected(true),
        );

        // Assert: the filled band starts at the opening bar and ends at the closing
        // one, with no unpainted cell anywhere between them — a gap there reads as two
        // shapes rather than one box.
        let filled = line
            .spans
            .iter()
            .enumerate()
            .filter(|(_, span)| span.style.bg == Some(FIELD_EDITING_SURFACE))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let (first, last) = (filled[0], filled[filled.len() - 1]);
        assert_eq!(
            line.spans[first].content.as_ref(),
            FIELD_FRAME,
            "the fill should start on the opening bar"
        );
        assert_eq!(
            line.spans[last].content.as_ref(),
            FIELD_FRAME,
            "the fill should end on the closing bar"
        );
        assert_eq!(
            filled,
            (first..=last).collect::<Vec<_>>(),
            "every span between the bars should be filled"
        );

        // Assert: and the label outside the box is not dragged into the fill.
        assert_that!(
            line.spans[..first]
                .iter()
                .any(|span| span.style.bg.is_some())
        )
        .is_false();
    }

    #[test]
    fn field_width_should_come_from_the_input_config() {
        // Arrange: a value long enough to fill each field completely.
        let long = "x".repeat(200);

        // Act / Assert: the drawn value window matches the config the cursor scrolls
        // against, so the two can never drift apart again.
        for config in [
            TextInputConfig::CONTAINER_METADATA,
            TextInputConfig::SUBTITLE_TITLE,
            TextInputConfig::RESOLUTION,
        ] {
            let line = text_field_line(TextField::new(
                "Title",
                FieldValue::Static(&long),
                config.width,
            ));
            let value = line
                .to_string()
                .chars()
                .filter(|glyph| *glyph == 'x')
                .count();
            assert_eq!(value, config.visible_width());
        }
    }

    #[test]
    fn a_wide_character_value_should_not_push_the_closing_frame() {
        // Arrange: CJK glyphs occupy two terminal columns each.
        let ascii = TextInputState::new("ab".to_string());
        let wide = TextInputState::new("日本語字幕".to_string());

        // Act
        let ascii_row = text_field_line(TextField::new(
            "Title",
            FieldValue::Editing(&ascii),
            TextInputConfig::SUBTITLE_TITLE.width,
        ));
        let wide_row = text_field_line(TextField::new(
            "Title",
            FieldValue::Editing(&wide),
            TextInputConfig::SUBTITLE_TITLE.width,
        ));

        // Assert: both rows occupy the same columns, so the grid survives wide values.
        assert_eq!(ascii_row.width(), wide_row.width());
    }

    #[test]
    fn a_framed_row_should_never_exceed_the_popup_inner_width() {
        // Arrange: the widest content each settings popup can produce.
        let title = TextInputState::new("x".repeat(200));
        let inner = SUBTITLE_SETTINGS_WIDTH as usize - 2;

        // Act
        let rows = [
            text_field_line(TextField::new(
                "Title",
                FieldValue::Editing(&title),
                TextInputConfig::SUBTITLE_TITLE.width,
            )),
            text_field_line(
                TextField::new(
                    "Search",
                    FieldValue::Editing(&title),
                    TextInputConfig::LANGUAGE_SEARCH.width,
                )
                .suffix(match_suffix(999)),
            ),
            subtitle_checkbox_line("Hearing impaired", false, false, false, None),
        ];

        // Assert: nothing wraps, which would desynchronise the popup's line-based
        // scrolling from what is actually drawn.
        for row in rows {
            assert!(
                row.width() <= inner,
                "row {:?} is {} wide, over the {inner}-column popup",
                row.to_string(),
                row.width(),
            );
        }
    }

    #[test]
    fn a_help_text_should_break_into_paragraphs_on_a_blank_line() {
        // Arrange / Act: one string carrying a break, and a second styled entry.
        let text = help_paragraphs(vec![
            (
                "First paragraph.\n\nSecond paragraph.".to_string(),
                Style::default().fg(Color::White),
            ),
            ("Third.".to_string(), Style::default().fg(Color::Yellow)),
        ]);

        // Assert: a blank line separates all three, and each keeps its own style, so a
        // field can add a caveat without the caller counting paragraphs.
        let bodies = text
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        // Two separators, plus the blank line `padded_popup_text` puts above and below.
        assert_that!(bodies.iter().filter(|line| line.trim().is_empty()).count()).is_equal_to(4);
        assert_that!(
            bodies
                .iter()
                .position(|line| line.contains("Second paragraph"))
                .unwrap()
                - bodies
                    .iter()
                    .position(|line| line.contains("First paragraph"))
                    .unwrap()
        )
        .is_equal_to(2);
    }

    #[test]
    fn every_help_text_should_fit_the_panel_it_is_drawn_in() {
        // Arrange: side by side, the help panel is given the dialog's height, so a text
        // that wraps past it is silently cut off at the bottom. Both panels are drawn
        // for real and the panel's inner rows are counted off the buffer, then compared
        // with the rows the text actually needs at that width.
        let (mut app, directory) = test_app("help-fit", &[]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {"index": 1, "codec_type": "subtitle", "codec_name": "subrip",
                     "tags": {"language": "eng"}}
                ]
            }))
            .unwrap(),
        ));

        /// Inner rows of the help panel, found by its title and its own bottom border.
        fn panel_rows(draw: &dyn Fn(&mut Frame)) -> u16 {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(200, 60)).unwrap();
            terminal.draw(|frame| draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let symbols = |y: u16| -> Vec<&str> {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            };
            let title = (0..buffer.area.height)
                .find(|&y| symbols(y).concat().contains("Information about"))
                .expect("the help panel should be titled");
            // Located by cell, not by byte: the row is full of multi-byte borders, so a
            // string offset is not a column.
            let row = symbols(title);
            let title_x = (0..row.len())
                .find(|&x| row[x..].concat().starts_with("Information about"))
                .expect("the help panel title should be on its own row")
                as u16;
            let left = title_x - 2;
            let bottom = (title + 1..buffer.area.height)
                .find(|&y| buffer[(left, y)].symbol() == "└")
                .expect("the help panel should be closed");
            bottom - title - 1
        }

        let fits = |label: String, help: &Text<'_>, draw: &dyn Fn(&mut Frame)| {
            // `wrapped_popup_height` counts the borders, `panel_rows` counts between
            // them.
            let needed = wrapped_popup_height(help, SUBTITLE_HELP_WIDTH).saturating_sub(2);
            let available = panel_rows(draw);
            assert!(
                needed <= available,
                "{label} help needs {needed} rows and the panel has {available}",
            );
        };

        // Act / Assert: container fields.
        for field in [
            ContainerSettingsField::Format,
            ContainerSettingsField::Title,
            ContainerSettingsField::Comment,
            ContainerSettingsField::Date,
            ContainerSettingsField::Genre,
            ContainerSettingsField::Artist,
        ] {
            app.container_settings_popup = Some(ContainerSettingsPopup {
                field,
                mode: ContainerSettingsMode::Summary,
                help_visible: true,
                format_cursor: 0,
                text_input: TextInputState::new(String::new()),
            });
            let popup = app.container_settings_popup.as_ref().unwrap();
            let help = container_field_help_text(&app, popup);
            fits(format!("{field:?}"), &help, &|frame| {
                render_container_settings_dialog(frame, &app)
            });
        }
        app.container_settings_popup = None;

        // Act / Assert: subtitle fields.
        for field in [
            SubtitleSettingsField::Codec,
            SubtitleSettingsField::Language,
            SubtitleSettingsField::Title,
            SubtitleSettingsField::Default,
            SubtitleSettingsField::Forced,
            SubtitleSettingsField::Cc,
            SubtitleSettingsField::HearingImpaired,
            SubtitleSettingsField::Original,
            SubtitleSettingsField::Commentary,
        ] {
            app.subtitle_settings_popup = Some(SubtitleSettingsPopup {
                source: SubtitleSource::Embedded(1),
                source_format: SubtitleFormat::SubRip,
                field,
                mode: SubtitleSettingsMode::Summary,
                help_visible: true,
                codec_cursor: 0,
                language_cursor: 0,
                language_search: SearchState::default(),
                title_input: TextInputState::new(String::new()),
            });
            let popup = app.subtitle_settings_popup.as_ref().unwrap();
            let help = subtitle_field_help_text(&app, popup);
            fits(format!("{field:?}"), &help, &|frame| {
                render_subtitle_settings_dialog(frame, &app)
            });
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_expanded_dropdown_should_list_its_options_under_its_own_row() {
        // Arrange: the video popup has two collapsible rows, so an expanded Codec list
        // appended after both rows would hang underneath Resolution instead.
        let (mut app, directory) = test_app("video-dropdown-order", &[]);
        app.outcome = Some(ProbeOutcome::Video(
            MediaInfo::from_json(serde_json::json!({
                "streams": [{
                    "index": 0, "codec_type": "video", "codec_name": "hevc",
                    "width": 1920, "height": 1080
                }]
            }))
            .unwrap(),
        ));
        app.video_settings_popup = Some(crate::app::VideoSettingsPopup {
            stream_index: 0,
            field: VideoSettingsField::Codec,
            mode: VideoSettingsMode::Dropdown,
            codec_cursor: 0,
            resolution_cursor: 0,
            custom_resolution: None,
        });
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();

        // Act
        terminal
            .draw(|frame| render_video_settings_dialog(frame, &app))
            .unwrap();

        // Assert: every option sits between Codec and Resolution.
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        let row_of = |needle: &str| {
            (0..buffer.area.height)
                .find(|&y| row_text(y).contains(needle))
                .unwrap_or_else(|| panic!("{needle} should be on screen"))
        };
        let codec_row = row_of("Codec");
        let resolution_row = row_of("Resolution");
        for option in ["H.264", "AV1"] {
            let option_row = row_of(option);
            assert!(
                option_row > codec_row && option_row < resolution_row,
                "{option} is on row {option_row}, outside Codec ({codec_row})..Resolution ({resolution_row})",
            );
        }

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn every_framed_field_except_the_resolution_boxes_should_end_in_one_column() {
        // Arrange: one row per fixed-width site, all with the same short value.
        let value = TextInputState::new("ab".to_string());
        let rows = [
            TextInputConfig::CONTAINER_METADATA,
            TextInputConfig::SUBTITLE_TITLE,
            TextInputConfig::LANGUAGE_SEARCH,
        ]
        .map(|config| {
            text_field_line(TextField::new(
                "Title",
                FieldValue::Editing(&value),
                config.width,
            ))
            .width()
        });

        // Assert: the popups line up on their right edge, not only their labels.
        assert_that!(rows.iter().collect::<std::collections::BTreeSet<_>>().len()).is_equal_to(1);

        // Assert: the resolution boxes are the deliberate exception.
        assert_that!(TextInputConfig::RESOLUTION.width)
            .is_not_equal_to(TextInputConfig::DEFAULT_WIDTH);
    }

    #[test]
    fn a_value_that_continues_past_the_field_should_be_marked_on_that_side() {
        // Arrange: a value far longer than the field, scrolled into its middle.
        let config = TextInputConfig::SUBTITLE_TITLE;
        let mut input = TextInputState::new("x".repeat(200));
        input.activate();
        input.cursor = 100;
        input.keep_cursor_visible(config.visible_width());

        // Act
        let scrolled = text_field_line(TextField::new(
            "Title",
            FieldValue::Editing(&input),
            config.width,
        ));
        let short = text_field_line(TextField::new(
            "Title",
            FieldValue::Editing(&TextInputState::new("ab".to_string())),
            config.width,
        ));

        // Assert: both ends are marked as continuing, and the marker takes the frame's
        // cell rather than a cell of its own.
        let text = scrolled.to_string();
        assert_that!(text.as_str()).contains(FIELD_OVERFLOW);
        assert_that!(text.matches(FIELD_OVERFLOW).count()).is_equal_to(2);
        assert_that!(text.as_str()).does_not_contain(FIELD_FRAME);
        assert_that!(scrolled.width()).is_equal_to(short.width());

        // Assert: a value that fits is not marked at all.
        assert_that!(short.to_string().as_str()).does_not_contain(FIELD_OVERFLOW);
    }

    #[test]
    fn a_truncated_static_value_should_be_marked_only_on_its_right() {
        // Arrange: an idle row whose stored value is wider than the box.
        let config = TextInputConfig::CONTAINER_METADATA;

        // Act
        let line = text_field_line(TextField::new(
            "Title",
            FieldValue::Static(&"x".repeat(200)),
            config.width,
        ));

        // Assert: the opening frame stays, the closing one becomes the marker.
        let text = line.to_string();
        assert_that!(text.matches(FIELD_OVERFLOW).count()).is_equal_to(1);
        assert_that!(text.trim_end().ends_with(FIELD_OVERFLOW)).is_true();
        assert_that!(text.as_str()).contains(FIELD_FRAME);
    }

    #[test]
    fn a_refused_keystroke_should_redden_the_frame_and_say_why() {
        // Arrange: a field being edited, whose last keystroke was refused.
        let mut input = TextInputState::new("192".to_string());
        input.activate();

        // Act
        let refused = text_field_line(
            TextField::new(
                "Width",
                FieldValue::Editing(&input),
                TextInputConfig::RESOLUTION.width,
            )
            .selected(true)
            .reject(Some(InputReject::Character(CharClass::Digits))),
        );

        // Assert: the rule that was broken is spelled out, since the refused character
        // never appeared on screen to explain itself.
        assert_that!(refused.to_string().as_str()).contains("digits only");
        let frame = refused
            .spans
            .iter()
            .find(|span| span.content.as_ref() == FIELD_FRAME)
            .expect("a selected field should be framed");
        assert_that!(frame.style.fg).is_equal_to(Some(Color::Red));

        // Assert: the cap reports the number, which the user cannot count themselves.
        let full = text_field_line(
            TextField::new(
                "Width",
                FieldValue::Editing(&input),
                TextInputConfig::RESOLUTION.width,
            )
            .reject(Some(InputReject::Full(512))),
        );
        assert_that!(full.to_string().as_str()).contains("512 character limit");
    }

    #[test]
    fn a_refusal_aimed_at_an_idle_field_should_not_be_drawn() {
        // Arrange: the same rejection, but the row is not the one being edited.
        let idle = TextInputState::new("192".to_string());

        // Act
        let line = text_field_line(
            TextField::new(
                "Width",
                FieldValue::Editing(&idle),
                TextInputConfig::RESOLUTION.width,
            )
            .reject(Some(InputReject::Character(CharClass::Digits))),
        );

        // Assert: five stacked rows share one popup-wide rejection, so only the row
        // actually in edit mode may show it.
        assert_that!(line.to_string().as_str()).does_not_contain("digits only");
    }

    #[test]
    fn an_empty_field_should_hint_at_its_value_and_drop_the_hint_when_typed_into() {
        // Arrange
        let config = TextInputConfig::CONTAINER_METADATA;
        let empty = TextInputState::new(String::new());
        let typed = TextInputState::new("B".to_string());

        // Act
        let hinted = text_field_line(
            TextField::new("Title", FieldValue::Editing(&empty), config.width)
                .placeholder("e.g. Big Buck Bunny"),
        );
        let filled = text_field_line(
            TextField::new("Title", FieldValue::Editing(&typed), config.width)
                .placeholder("e.g. Big Buck Bunny"),
        );

        // Assert: the hint reads as an example rather than as a stored value, and it
        // does not change the row's width.
        assert_that!(hinted.to_string().as_str()).contains("e.g. Big Buck Bunny");
        assert_that!(filled.to_string().as_str()).does_not_contain("e.g.");
        assert_that!(hinted.width()).is_equal_to(filled.width());
        let hint = hinted
            .spans
            .iter()
            .find(|span| span.content.contains("e.g."))
            .expect("the hint should be rendered");
        assert_that!(hint.style.fg).is_equal_to(Some(Color::DarkGray));
    }

    #[test]
    fn a_search_bar_should_be_framed_like_a_settings_row() {
        // Arrange
        let mut search = SearchState::default();
        search.activate();
        search.input.insert('a', TextInputConfig::search(20));
        let area = Rect::new(0, 0, 60, 1);

        // Act
        let bar = search_line(&mut search, area, None);

        // Assert: one presentation across the application — the bar carries the same
        // focus frame a settings field does, and still fits its pane.
        let text = bar.to_string();
        assert_that!(text.matches(FIELD_FRAME).count()).is_equal_to(2);
        assert_that!(bar.width()).is_less_than_or_equal_to(area.width as usize);
    }

    #[test]
    fn a_search_bar_should_reserve_room_for_the_widest_match_count() {
        // Arrange: the same bar at both ends of the count's width.
        let area = Rect::new(0, 0, 60, 1);
        let mut one = SearchState::default();
        one.activate();
        one.match_count = 1;
        let mut many = SearchState::default();
        many.activate();
        many.match_count = 999;

        // Act
        let narrow = search_line(&mut one, area, None);
        let wide = search_line(&mut many, area, None);

        // Assert: the value window does not move when the count gains a digit, and the
        // longest count still fits the pane.
        assert_that!(one.field_width).is_equal_to(many.field_width);
        assert_that!(wide.width()).is_less_than_or_equal_to(area.width as usize);
        assert_that!(narrow.width()).is_less_than_or_equal_to(area.width as usize);
    }

    #[test]
    fn all_three_search_bars_should_word_the_match_count_identically() {
        assert_eq!(match_suffix(0), " (no matches)");
        assert_eq!(match_suffix(1), " (1 match)");
        assert_eq!(match_suffix(2), " (2 matches)");
    }

    #[test]
    fn an_idle_empty_search_bar_should_show_its_placeholder() {
        // Arrange
        let mut search = SearchState::default();
        let area = Rect::new(0, 0, 60, 1);

        // Act
        let idle = search_line(&mut search, area, None).to_string();
        search.activate();
        let active = search_line(&mut search, area, None).to_string();

        // Assert
        assert_that!(idle.as_str())
            .contains("type to filter")
            .does_not_contain(FIELD_CARET);
        assert_that!(active.as_str())
            .contains("type to filter")
            .contains(FIELD_CARET);
    }

    #[test]
    fn custom_input_should_use_a_flat_dark_surface_and_cursor() {
        // Act
        let mut input = TextInputState::new("1280".to_string());
        input.activate();
        let line = custom_input_line("Width", &input, true, false, "e.g. 1920", None);

        // Assert
        assert_that!(line.to_string())
            .contains("1280")
            .contains(FIELD_CARET)
            .contains(FIELD_FRAME)
            .does_not_contain("NORMAL")
            .does_not_contain("INSERT")
            .does_not_contain("╭")
            .does_not_contain("╰");
        assert_eq!(line.spans[0].style.fg, Some(Color::Cyan));
        let value = value_span(&line, "1280");
        assert_eq!(value.style.bg, Some(FIELD_EDITING_SURFACE));
    }

    #[test]
    fn custom_input_should_mark_a_changed_value_yellow_and_italic() {
        // Act
        let input = TextInputState::new("1280".to_string());
        let line = custom_input_line("Width", &input, false, true, "e.g. 1920", None);

        // Assert
        let value = value_span(&line, "1280");
        assert_eq!(value.style.fg, Some(Color::Yellow));
        assert!(value.style.add_modifier.contains(Modifier::ITALIC));
        // An idle field carries no surface; only the one being edited is filled.
        assert_eq!(value.style.bg, None);
    }

    /// The span carrying a field's value, located by content rather than by index so
    /// these assertions survive changes to the surrounding chrome.
    fn value_span<'a>(line: &'a Line<'a>, value: &str) -> &'a Span<'a> {
        line.spans
            .iter()
            .find(|span| span.content.contains(value))
            .unwrap_or_else(|| panic!("line {line:?} should render {value:?}"))
    }

    #[test]
    fn custom_scaling_should_offer_only_exact_output_options() {
        // Arrange
        let draft = crate::app::CustomResolutionDraft {
            width: TextInputState::new("1280".to_string()),
            height: TextInputState::new("720".to_string()),
            scaling: crate::edit::CustomScaling::FitPad,
            field: CustomResolutionField::Scaling,
            scaling_cursor: 1,
            scaling_dropdown_open: true,
        };

        // Act
        let lines = custom_scaling_lines(&draft);
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        // Assert
        assert_that!(text.as_str())
            .contains("Fit & pad")
            .contains("Stretch")
            .does_not_contain("Fit inside");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2].style.bg, Some(Color::Cyan));
    }

    #[test]
    fn custom_scaling_should_mark_a_nondefault_value_as_changed() {
        // Arrange
        let draft = crate::app::CustomResolutionDraft {
            width: TextInputState::new("1280".to_string()),
            height: TextInputState::new("720".to_string()),
            scaling: crate::edit::CustomScaling::Stretch,
            field: CustomResolutionField::Width,
            scaling_cursor: 1,
            scaling_dropdown_open: false,
        };

        // Act
        let lines = custom_scaling_lines(&draft);

        // Assert
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Yellow));
        assert!(
            lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn dropdown_line_should_mark_the_selected_value_with_a_greater_than_sign() {
        // Act
        let selected = dropdown_line("1920×1080 / 16:9", false, true, true, false, false);
        let available = dropdown_line("1280×720 / 16:9", false, false, true, false, true);

        // Assert
        assert_that!(selected.to_string())
            .starts_with("  ├── > 1920×1080 / 16:9")
            .does_not_contain("●");
        assert_that!(available.to_string())
            .starts_with("  └──   1280×720 / 16:9")
            .does_not_contain("●");
        assert_eq!(selected.spans[0].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn choice_style_should_distinguish_available_changed_focused_and_disabled_choices() {
        // Act
        let available = choice_style(false, false, true);
        let changed = choice_style(false, true, true);
        let focused_changed = choice_style(true, true, true);
        let disabled = choice_style(false, false, false);

        // Assert
        assert_eq!(available.fg, Some(Color::White));
        assert_eq!(changed.fg, Some(Color::Yellow));
        assert!(changed.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(focused_changed.fg, Some(Color::White));
        assert_eq!(focused_changed.bg, Some(Color::Cyan));
        assert!(focused_changed.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(disabled.fg, Some(Color::DarkGray));
        assert_eq!(disabled.bg, None);
    }

    #[test]
    fn redraw_state_should_paint_one_frame_after_an_animation_stops() {
        // Regression test for a conflict notice frozen on a dim "Understood (1)":
        // the frame that shows an animation's finished state is the first frame where
        // `animating` is false, so `dirty || animating` alone never paints it and the
        // screen keeps the last mid-animation frame until something unrelated
        // happens.
        let mut redraw = RedrawState::default();

        // Idle: nothing changed, nothing animating, nothing to draw.
        assert!(!redraw.tick(false, false));

        // Animating: every tick draws, state change or not.
        assert!(redraw.tick(false, true));
        assert!(redraw.tick(false, true));

        // The tick the animation stops on must still draw — this is the armed button.
        assert!(redraw.tick(false, false));
        // ...and exactly one such frame, not a permanent repaint loop.
        assert!(!redraw.tick(false, false));

        // A plain state change still draws on its own.
        assert!(redraw.tick(true, false));
        assert!(!redraw.tick(false, false));
    }

    #[test]
    fn action_option_should_keep_an_unfocused_action_available() {
        // Act
        let action = action_option(" Keep processing ", false);

        // Assert
        assert_eq!(action.style.fg, Some(Color::White));
        assert_eq!(action.style.bg, None);
    }
}
