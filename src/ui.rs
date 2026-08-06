use std::{collections::BTreeMap, path::Path};

use isolang::Language;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Wrap},
};
use serde_json::Value;

use crate::{
    app::{
        App, CancelEditChoice, ContainerChoice, CustomResolutionField, Dialog, Layer,
        SaveDialogField, SearchState, SubtitleDisplayState, SubtitleSettingsField,
        SubtitleSettingsMode, SubtitleSettingsPopup, TextInputState, TrackRef, VideoSettingsField,
        VideoSettingsMode,
    },
    edit::{ContainerFormat, SaveDestination, stream_index},
    probe::{MediaInfo, ProbeOutcome},
    subtitle::{
        SidecarEntry, SubtitleChange, SubtitleFlag, SubtitleFormat, SubtitleSource,
        canonical_language_code, language_choice, stream_cc, stream_commentary, stream_forced,
        stream_hearing_impaired, stream_language, stream_original, stream_title,
    },
};

const SUBTITLE_COLUMN_GUTTER: u16 = 2;

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
            )))
        })
        .collect();
    app.file_search.match_count = entries.len();
    let title = if app.is_network_mount {
        format!(" Files ({}) [NET] ", app.files.len())
    } else {
        format!(" Files ({}) ", app.files.len())
    };
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
        frame.render_widget(Paragraph::new(search_line(&app.file_search)), chunks[1]);
    } else {
        frame.render_stateful_widget(list, inner, &mut app.list_state);
    }
}

fn file_tree_lines<'a>(
    display_name: &str,
    sidecar_names: impl IntoIterator<Item = &'a str>,
    folded: bool,
    has_any_sidecars: bool,
) -> Vec<Line<'static>> {
    let sidecar_names = sidecar_names.into_iter().collect::<Vec<_>>();
    if sidecar_names.is_empty() {
        let prefix = if has_any_sidecars { "  " } else { "" };
        return vec![Line::from(format!("{prefix}{display_name}"))];
    }
    let prefix = if folded { "▹ " } else { "▿ " };
    let first_line = Line::from(format!("{prefix}{display_name}"));
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

    let other: Vec<_> = order
        .iter()
        .filter_map(|index| {
            let stream = info
                .streams
                .iter()
                .find(|stream| stream_index(stream) == Some(*index))?;
            let selection_index = rows
                .iter()
                .position(|row| *row == TrackRef::Embedded(*index))?;
            (!matches!(
                string(stream, "codec_type"),
                Some("video" | "audio" | "subtitle")
            ))
            .then_some((selection_index, stream))
        })
        .collect();
    if !other.is_empty() {
        section(&mut lines, &format!("Other ({})", other.len()));
        for (selection_index, stream) in other {
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

    if !info.chapters.is_empty() {
        section(
            &mut lines,
            &format!(
                "Chapters ({}) · detailed chapter information coming later",
                info.chapters.len()
            ),
        );
    }

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
    let mut forced = metadata.map_or(sidecar.forced, |metadata| metadata.forced);
    let mut hearing_impaired = metadata.map_or(sidecar.hearing_impaired, |metadata| {
        metadata.cc || metadata.hearing_impaired
    });
    let mut original = metadata.is_some_and(|metadata| metadata.original);
    let mut commentary = metadata.is_some_and(|metadata| metadata.commentary);
    if change.is_some_and(|change| change.import_into_media)
        && let Some(container) = container
    {
        forced &= container.supports_subtitle_flag(SubtitleFlag::Forced);
        hearing_impaired &= container.supports_subtitle_flag(SubtitleFlag::HearingImpaired);
        original &= container.supports_subtitle_flag(SubtitleFlag::Original);
        commentary &= container.supports_subtitle_flag(SubtitleFlag::Commentary);
    }
    let format = change
        .and_then(|change| change.export_target.or(change.embedded_target))
        .unwrap_or(sidecar.format);
    let details = subtitle_overview_details(
        format.overview_label(),
        crate::subtitle::normalized_language(language),
        title,
        SubtitleOverviewFlags {
            default,
            forced,
            cc: false,
            hearing_impaired,
            original,
            commentary,
        },
        max_width.map(|width| width.saturating_sub(prefix.chars().count())),
    );
    Line::from(format!("{prefix}{details}")).style(if selected {
        focused_style(changed)
    } else if changed {
        changed_style()
    } else {
        Style::default()
    })
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
    let mut parts = vec![format];
    if let Some(duration) = number_string(&info.format, "duration").and_then(parse_number) {
        parts.push(format_duration(duration));
    }
    if let Some(size) = number_string(&info.format, "size").and_then(parse_number) {
        parts.push(format_bytes(size));
    }
    if let Some(bit_rate) = number_string(&info.format, "bit_rate").and_then(parse_number) {
        parts.push(format_bitrate(bit_rate));
    }
    if conflicts > 0 {
        parts.push(format!(
            "⚠ {conflicts} compatibility conflict{}",
            if conflicts == 1 { "" } else { "s" }
        ));
    }
    let marker = if selected { "›" } else { " " };
    let changed = target.is_some();
    Line::from(format!("{marker}    {}", parts.join("  ·  "))).style(if selected {
        focused_style(changed)
    } else if conflicts > 0 {
        warning_style(changed)
    } else if changed {
        changed_style()
    } else {
        Style::default()
    })
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
        let mut forced = metadata.map_or_else(|| stream_forced(stream), |metadata| metadata.forced);
        let mut cc = metadata.map_or_else(|| stream_cc(stream), |metadata| metadata.cc);
        let mut hearing_impaired = metadata.map_or_else(
            || stream_hearing_impaired(stream),
            |metadata| metadata.hearing_impaired,
        );
        let mut original =
            metadata.map_or_else(|| stream_original(stream), |metadata| metadata.original);
        let mut commentary =
            metadata.map_or_else(|| stream_commentary(stream), |metadata| metadata.commentary);
        if subtitle_change.is_some_and(|change| change.export_target.is_some()) {
            hearing_impaired |= cc;
            cc = false;
        } else if let Some(container) = container {
            forced &= container.supports_subtitle_flag(SubtitleFlag::Forced);
            cc &= container.supports_subtitle_flag(SubtitleFlag::Cc);
            hearing_impaired &= container.supports_subtitle_flag(SubtitleFlag::HearingImpaired);
            original &= container.supports_subtitle_flag(SubtitleFlag::Original);
            commentary &= container.supports_subtitle_flag(SubtitleFlag::Commentary);
        }
        let format = subtitle_change
            .and_then(|change| change.export_target.or(change.embedded_target))
            .map(|format| format.overview_label().to_string())
            .unwrap_or_else(|| subtitle_format_overview_label(codec));
        vec![subtitle_overview_details(
            &format,
            crate::subtitle::normalized_language(language),
            title,
            SubtitleOverviewFlags {
                default,
                forced,
                cc,
                hearing_impaired,
                original,
                commentary,
            },
            max_width.map(|width| {
                width.saturating_sub(marker.chars().count() + index_text.chars().count())
            }),
        )]
    } else {
        vec![codec.to_uppercase()]
    };

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
            if let Some(rate) = number_string(stream, "sample_rate").and_then(parse_number) {
                details.push(format_sample_rate(rate));
            }
        }
        "subtitle" => {}
        _ => {
            if kind != "unknown" {
                details.push(kind.to_string());
            }
        }
    }

    if !subtitle {
        if let Some(language) = tag(stream, "language")
            && language != "und"
        {
            details.push(language.to_uppercase());
        }
        if let Some(title) = tag(stream, "title") {
            details.push(title.to_string());
        }
        details.extend(disposition_flags(stream, default));
    }

    let semantic_span_style = if selected {
        Style::default()
    } else if deleted {
        Style::default().fg(Color::Red).bold()
    } else if conflict {
        warning_style(false).bold()
    } else if changed {
        changed_style().bold()
    } else {
        Style::default()
    };
    let index_style = if selected {
        Style::default()
    } else if deleted {
        Style::default().fg(Color::Red)
    } else if conflict {
        warning_style(false)
    } else if changed {
        changed_style()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let line = Line::from(vec![
        if marker == "  " {
            Span::raw(marker)
        } else {
            Span::styled(marker, semantic_span_style)
        },
        Span::styled(index_text, index_style),
        Span::raw(details.join(if subtitle { " - " } else { "  ·  " })),
    ]);
    if selected {
        line.style(focused_style(changed && !deleted))
    } else {
        line.style(if deleted {
            Style::default().fg(Color::Red)
        } else if conflict {
            warning_style(changed)
        } else if changed {
            changed_style()
        } else {
            Style::default()
        })
    }
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
    if dialog == Dialog::Processing {
        render_progress_dialog(frame, app);
        return;
    }
    if dialog == Dialog::ConfirmCancel {
        render_progress_dialog(frame, app);
        render_cancel_edit_dialog(frame, app);
        return;
    }
    if dialog == Dialog::ConfirmSave {
        render_save_dialog(frame, app);
        return;
    }
    let (title, body, color) = match dialog {
        Dialog::Keybindings
        | Dialog::ContainerSettings
        | Dialog::VideoSettings
        | Dialog::SubtitleSettings
        | Dialog::ConfirmSave => unreachable!(),
        Dialog::Processing | Dialog::ConfirmCancel => unreachable!(),
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

fn render_container_settings_dialog(frame: &mut Frame, app: &App) {
    let Some(popup) = app.container_settings_popup.as_ref() else {
        return;
    };
    let choices = app.container_choices();
    let mut lines = Vec::new();
    for (position, choice) in choices.iter().enumerate() {
        lines.push(container_choice_line(choice, position == popup.cursor));
    }
    let text = padded_popup_text(Text::from(lines));
    let height = (text.lines.len() as u16 + 2)
        .max(8)
        .min(frame.area().height.saturating_sub(2));
    let area = centered_fixed(frame.area(), 88, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" Container format "),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn container_choice_line(choice: &ContainerChoice, cursor: bool) -> Line<'static> {
    let changed = choice.staged && !choice.current;
    let mut line = subtitle_codec_line(&choice.label, cursor, changed, true, choice.current);
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

fn render_save_dialog(frame: &mut Frame, app: &App) {
    let summary = app.save_summary();
    let mut lines = vec![Line::styled(
        "Changes",
        Style::default().fg(Color::Cyan).bold(),
    )];
    lines.extend(
        summary
            .into_iter()
            .map(|item| Line::from(format!("• {item}"))),
    );
    lines.push(Line::from(""));

    if app.media_will_change() {
        let destination_focused = app.save_dialog_field == SaveDialogField::Destination;
        lines.push(Line::from(vec![
            Span::styled(
                if destination_focused {
                    "› Output  "
                } else {
                    "  Output  "
                },
                Style::default().add_modifier(if destination_focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            destination_option(
                " Replace original ",
                app.save_destination == SaveDestination::ReplaceOriginal,
            ),
            Span::raw("  "),
            destination_option(
                " Create a copy ",
                app.save_destination == SaveDestination::CreateCopy,
            ),
        ]));
        lines.push(Line::from(""));
    }

    let start_focused = app.save_dialog_field == SaveDialogField::Start;
    lines.push(
        Line::from(Span::styled(
            if start_focused {
                "       ▶  START       "
            } else {
                "          START       "
            },
            if start_focused {
                focused_style(false)
            } else {
                Style::default().fg(Color::White).bold()
            },
        ))
        .centered(),
    );

    let text = padded_popup_text(Text::from(lines));
    let area = save_dialog_area(frame.area(), &text.lines);
    let scroll = max_scroll(&text, area);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(if app.media_will_change() {
                        " Save media edits "
                    } else {
                        " Save subtitle changes "
                    }),
            )
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn save_dialog_area(frame_area: Rect, lines: &[Line<'_>]) -> Rect {
    let width = 68.min(frame_area.width.saturating_sub(2)).max(1);
    let content_width = width.saturating_sub(2).max(1) as usize;
    let rendered_lines = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(content_width))
        .sum::<usize>();
    let height = rendered_lines.saturating_add(2).min(u16::MAX as usize) as u16;
    centered_fixed(frame_area, width, height.max(10))
}

fn action_option(label: &'static str, focused: bool) -> Span<'static> {
    Span::styled(
        label,
        if focused {
            focused_style(false)
        } else {
            Style::default().fg(Color::White)
        },
    )
}

fn destination_option(label: &'static str, chosen: bool) -> Span<'static> {
    Span::styled(
        label,
        if chosen {
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

fn search_line(search: &SearchState) -> Line<'_> {
    if search.value.is_empty() && !search.is_active {
        return Line::styled("  Search: ...", Style::default().fg(Color::Yellow));
    }
    let match_suffix = match search.match_count {
        0 => " (no matches)".to_string(),
        1 => " (1 match)".to_string(),
        count => format!(" ({count} matches)"),
    };
    let cursor_byte = search
        .value
        .char_indices()
        .nth(search.cursor)
        .map_or(search.value.len(), |(index, _)| index);
    let (before, after) = search.value.split_at(cursor_byte);
    Line::from(vec![
        Span::styled("  Search: ", Style::default().fg(Color::Cyan)),
        Span::styled(before, Style::default().fg(Color::Yellow).bold()),
        Span::styled(
            if search.is_active { "▏" } else { "" },
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(after, Style::default().fg(Color::Yellow).bold()),
        Span::styled(match_suffix, Style::default().fg(Color::DarkGray)),
    ])
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

        frame.render_widget(
            Paragraph::new(search_line(&app.keybindings_search)),
            chunks[1],
        );
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
    keybinding(&mut lines, "Ctrl-c", "Cancel processing");

    keybindings_section(&mut lines, "Text input");
    keybinding(&mut lines, "i", "Edit the selected text or number field");
    keybinding(&mut lines, "Left/Right", "Move the text cursor");
    keybinding(&mut lines, "Home/End", "Move to the start or end of text");
    keybinding(
        &mut lines,
        "Backspace/Delete",
        "Delete text around the cursor",
    );
    keybinding(&mut lines, "Esc / Enter", "Finish or accept text input");
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

fn render_progress_dialog(frame: &mut Frame, app: &App) {
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
    let label = app
        .edit_progress_label
        .clone()
        .unwrap_or_else(|| app.processing_description());
    frame.render_widget(
        Paragraph::new(processing_info_line(truncate_end(
            &label,
            rows[2].width as usize,
        )))
        .centered(),
        rows[2],
    );

    let tick = app
        .edit_started
        .map_or(0, |started| (started.elapsed().as_millis() / 80) as usize);
    let percent = app
        .edit_progress
        .map(|progress| (progress.clamp(0.0, 1.0) * 100.0).round() as u16);
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

    let mut lines = vec![
        setting_line(
            "Codec",
            codec_label,
            popup.field == VideoSettingsField::Codec,
            settings.codec != crate::edit::VideoCodec::Original,
        ),
        setting_line(
            "Resolution",
            &resolution_label,
            popup.field == VideoSettingsField::Resolution,
            settings.resolution != crate::edit::VideoResolution::Original,
        ),
    ];
    if popup.mode == VideoSettingsMode::Dropdown {
        lines.push(Line::from(""));
        match popup.field {
            VideoSettingsField::Codec => {
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
                        choice.current,
                    ));
                }
            }
            VideoSettingsField::Resolution => {
                for (position, choice) in resolution_choices.iter().enumerate() {
                    let selected = choice.selected(settings.resolution);
                    lines.push(dropdown_line(
                        &choice.label,
                        position == popup.resolution_cursor,
                        selected,
                        choice.enabled,
                        settings.resolution != crate::edit::VideoResolution::Original && selected,
                    ));
                }
            }
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
    lines.push(custom_input_line(
        "Width",
        &draft.width,
        draft.field == CustomResolutionField::Width,
        width_changed,
    ));
    lines.push(Line::from(""));
    lines.push(custom_input_line(
        "Height",
        &draft.height,
        draft.field == CustomResolutionField::Height,
        height_changed,
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

const CUSTOM_INPUT_WIDTH: usize = 16;

fn custom_input_line(
    label: &str,
    input: &TextInputState,
    focused: bool,
    changed: bool,
) -> Line<'static> {
    text_input_line(label, input, focused, changed, CUSTOM_INPUT_WIDTH, None)
}

fn custom_scaling_lines(draft: &crate::app::CustomResolutionDraft) -> Vec<Line<'static>> {
    let focused = draft.field == CustomResolutionField::Scaling;
    let changed = draft.scaling != crate::edit::CustomScaling::FitPad;
    let mut lines = vec![setting_line(
        "Scaling",
        draft.scaling.label(),
        focused,
        changed,
    )];
    if draft.scaling_dropdown_open {
        lines.push(Line::from(""));
        for (position, scaling) in crate::edit::CustomScaling::OPTIONS.iter().enumerate() {
            lines.push(dropdown_line(
                scaling.label(),
                position == draft.scaling_cursor,
                *scaling == draft.scaling,
                true,
                changed && *scaling == draft.scaling,
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
    ));
    let mut codec_dropdown_start = None;

    if popup.mode == SubtitleSettingsMode::CodecDropdown {
        lines.push(Line::from(""));
        codec_dropdown_start = Some(lines.len());
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
                choice.current,
            ));
        }
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
    ));
    let mut language_dropdown_start = None;
    if popup.mode == SubtitleSettingsMode::LanguageDropdown {
        let choices = app.filtered_subtitle_languages();
        let mut search = text_input_line(
            "Search",
            &popup.language_search.input,
            true,
            false,
            32,
            None,
        );
        search.spans.push(Span::styled(
            format!("  ({} matches)", choices.len()),
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(search);
        language_dropdown_start = Some(lines.len());
        let start = popup.language_cursor.saturating_sub(5).min(choices.len());
        let end = (start + 10).min(choices.len());
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
            ));
        }
        if choices.is_empty() {
            lines.push(Line::styled(
                "    No matching languages",
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    if app.subtitle_field_visible(SubtitleSettingsField::Title) {
        field_lines.push((SubtitleSettingsField::Title, lines.len()));
        lines.push(text_input_line(
            "Title",
            &popup.title_input,
            selected(SubtitleSettingsField::Title),
            changed(SubtitleSettingsField::Title),
            SUBTITLE_TITLE_WIDTH,
            app.subtitle_field_reason(SubtitleSettingsField::Title)
                .as_deref(),
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
    let text = padded_popup_text(Text::from(lines));
    let height = (text.lines.len() as u16 + 2).max(12);
    let help_text = popup
        .help_visible
        .then(|| subtitle_field_help_text(app, popup));
    let (area, help_area) =
        subtitle_settings_dialog_areas(frame.area(), height, help_text.as_ref());
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
    if let (Some(help_text), Some(help_area)) = (help_text, help_area) {
        frame.render_widget(
            Paragraph::new(help_text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Blue))
                        .title(subtitle_field_help_title(popup.field)),
                )
                .wrap(Wrap { trim: false }),
            help_area,
        );
    }
}

const SUBTITLE_TITLE_WIDTH: usize = 48;
const SUBTITLE_CHECKBOX_LABEL_WIDTH: usize = 18;
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
                "Sets the subtitle format written to the output. Text formats remain searchable and editable; image formats preserve rendered graphics. Converting an image subtitle to text requires OCR."
            }
            SubtitleSettingsField::Language => {
                "Identifies the subtitle language to players and media libraries. It changes metadata only; it does not translate the subtitle text."
            }
            SubtitleSettingsField::Title => {
                "An optional name shown by players, such as “English SDH” or “Director commentary.” Use it to distinguish subtitle tracks that otherwise look alike."
            }
            SubtitleSettingsField::Default => {
                "Marks this as the subtitle track a player should prefer automatically. Enabling it clears the Default flag from other subtitle tracks, though players can still apply their own preferences."
            }
            SubtitleSettingsField::Forced => {
                "Marks subtitles containing essential dialogue that should appear when ordinary subtitles are off, such as foreign-language lines. It does not make the whole track permanently visible."
            }
            SubtitleSettingsField::Cc => {
                "Marks the track as closed captions: a transcription of speech and relevant sounds. Use it only when the track was authored as captions; this flag does not add missing cues."
            }
            SubtitleSettingsField::HearingImpaired => {
                "Marks the track as intended for deaf or hard-of-hearing viewers, usually with speaker labels and sound cues. This flag does not change the subtitle content."
            }
            SubtitleSettingsField::Original => {
                "Marks the subtitle as matching the work’s original language. This means original-language subtitles, not an untouched or source file."
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
        SubtitleSettingsField::Codec => state.as_ref().map(|state| {
            if state.external {
                format!(
                    "This subtitle will be saved as {} outside the media file.",
                    state.format.label()
                )
            } else if let Some(container) = app.effective_container() {
                format!(
                    "This subtitle will be stored as {} in {}.",
                    state.format.label(),
                    container.label()
                )
            } else {
                format!("The selected output format is {}.", state.format.label())
            }
        }),
        SubtitleSettingsField::Language if external_sidecar => Some(
            "For an external sidecar, Reel also writes the canonical language code into its file name."
                .to_string(),
        ),
        SubtitleSettingsField::Forced if external_sidecar => Some(
            "For an external sidecar, Reel represents this choice with the .forced file name marker."
                .to_string(),
        ),
        SubtitleSettingsField::HearingImpaired if external_sidecar => Some(
            "For an external sidecar, Reel represents this choice with the canonical .sdh file name marker."
                .to_string(),
        ),
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

    let mut lines = Vec::new();
    for (position, (paragraph, style)) in paragraphs.into_iter().enumerate() {
        if position > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::styled(paragraph, style));
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
        let help = Rect::new(help_x, editor.y, SUBTITLE_HELP_WIDTH, editor.height);
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

fn text_input_line(
    label: &str,
    input: &TextInputState,
    selected: bool,
    changed: bool,
    width: usize,
    reason: Option<&str>,
) -> Line<'static> {
    let enabled = reason.is_none();
    let input_style = if !enabled {
        Style::default().fg(Color::DarkGray)
    } else if changed {
        changed_style()
    } else {
        Style::default().fg(Color::White)
    }
    .bg(Color::Rgb(32, 32, 32));
    let characters = input.value.chars().collect::<Vec<_>>();
    let visible_width = width.saturating_sub(1);
    let start = if input.is_active {
        input.view_offset
    } else {
        0
    }
    .min(characters.len());
    let end = (start + visible_width).min(characters.len());
    let caret = input
        .cursor
        .saturating_sub(start)
        .min(end.saturating_sub(start));
    let before = characters[start..start + caret].iter().collect::<String>();
    let after = characters[start + caret..end].iter().collect::<String>();
    let cursor_marker = if input.is_active { "▏" } else { "" };
    let padding = " ".repeat(width.saturating_sub(
        before.chars().count() + after.chars().count() + cursor_marker.chars().count(),
    ));
    let mut spans = vec![
        Span::styled(
            format!("{label:<12}"),
            Style::default().fg(if selected { Color::Cyan } else { Color::Gray }),
        ),
        Span::styled(before, input_style),
        Span::styled(cursor_marker.to_string(), input_style.fg(Color::Cyan)),
        Span::styled(after, input_style),
        Span::styled(padding, input_style),
    ];
    if let Some(reason) = reason {
        spans.push(Span::styled(
            format!("  {reason}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
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
        Span::styled(
            format!("{label:<width$}", width = SUBTITLE_CHECKBOX_LABEL_WIDTH),
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

fn setting_line(label: &str, value: &str, selected: bool, changed: bool) -> Line<'static> {
    let value_style = if selected {
        focused_style(changed)
    } else if changed {
        changed_style()
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(
            format!("{label:<12}"),
            Style::default().fg(if selected { Color::Cyan } else { Color::Gray }),
        ),
        Span::styled(format!("[ {value} ]"), value_style),
    ])
}

fn subtitle_codec_line(
    label: &str,
    cursor: bool,
    staged: bool,
    enabled: bool,
    current: bool,
) -> Line<'static> {
    let label_style = choice_style(cursor, staged, enabled);
    let mut spans = vec![Span::styled(format!("    {label}"), label_style)];
    if current {
        let original_style = if cursor {
            focused_style(false).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(" (original)", original_style));
    }
    Line::from(spans)
}

fn dropdown_line(
    label: &str,
    cursor: bool,
    selected: bool,
    enabled: bool,
    staged: bool,
) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    let line = Line::from(format!("  {marker} {label}"));
    line.style(choice_style(cursor, staged, enabled))
}

fn render_details_popup(frame: &mut Frame, app: &mut App) {
    let Some((text, title, sizing)) = details_popup_content(app) else {
        return;
    };
    let text = padded_popup_text(text);
    let area = match sizing {
        DetailsPopupSizing::Content => content_popup_area(frame.area(), &text, &title),
        DetailsPopupSizing::Subtitle => subtitle_popup_area(frame.area(), &text, &title),
        DetailsPopupSizing::Large => popup_area(frame.area(), 90, 86),
    };
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetailsPopupSizing {
    Content,
    Subtitle,
    Large,
}

fn details_popup_content(app: &App) -> Option<(Text<'static>, String, DetailsPopupSizing)> {
    let info = app.media_info()?;
    match app.selected_track()? {
        TrackRef::Container => {
            let file = app.selected_file()?;
            Some((
                Text::from(container_information_lines(
                    info,
                    &file.path,
                    file.fingerprint.length,
                )),
                " Container information ".to_string(),
                DetailsPopupSizing::Content,
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
                    DetailsPopupSizing::Content,
                ));
            }
            if kind == "audio" {
                return Some((
                    Text::from(audio_information_lines(stream, default)),
                    format!(" Audio #{index_label} "),
                    DetailsPopupSizing::Content,
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
                    DetailsPopupSizing::Subtitle,
                ));
            }
            let mut lines = Vec::new();
            append_map(&mut lines, stream, 0);
            Some((
                Text::from(lines),
                format!(" Stream #{index_label} · {kind} "),
                DetailsPopupSizing::Large,
            ))
        }
        TrackRef::Sidecar(index) => {
            let sidecar = app.sidecars.get(index)?;
            let source = SubtitleSource::Sidecar(sidecar.path.clone());
            let state = app.subtitle_display_state(&source, sidecar.format);
            Some((
                Text::from(sidecar_subtitle_information_lines(sidecar, state.as_ref())),
                " External subtitle ".to_string(),
                DetailsPopupSizing::Subtitle,
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
    let mut roles = disposition_flags(stream, default)
        .into_iter()
        .map(|role| title_case(role.trim_matches(['[', ']'])))
        .collect::<Vec<_>>();
    if disposition_enabled(stream, "original") {
        roles.push("Original".to_string());
    }
    if crate::probe::is_attached_picture(stream) {
        roles.push("Cover art".to_string());
    }
    roles
}

fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut characters = word.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + characters.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
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
            vec![subtitle_information_field_line(
                "Title",
                metadata.title.as_deref().unwrap_or("Not provided"),
                changed(SubtitleSettingsField::Title),
            )],
        );
    }

    let language = subtitle_language_description(&metadata.language)
        .unwrap_or_else(|| "Not provided".to_string());
    let mut language_and_role = vec![subtitle_information_field_line(
        "Language",
        &language,
        changed(SubtitleSettingsField::Language),
    )];
    for (field, label, enabled) in [
        (SubtitleSettingsField::Cc, "Closed captions", metadata.cc),
        (
            SubtitleSettingsField::HearingImpaired,
            "Hearing impaired",
            metadata.hearing_impaired,
        ),
    ] {
        if visible(field) {
            language_and_role.push(subtitle_information_field_line(
                label,
                if enabled { "Yes" } else { "No" },
                changed(field),
            ));
        }
    }
    let mut roles = Vec::new();
    if visible(SubtitleSettingsField::Default) && default {
        roles.push("Default".to_string());
    }
    if visible(SubtitleSettingsField::Forced) && metadata.forced {
        roles.push("Forced".to_string());
    }
    if visible(SubtitleSettingsField::Original) && metadata.original {
        roles.push("Original".to_string());
    }
    if visible(SubtitleSettingsField::Commentary) && metadata.commentary {
        roles.push("Commentary".to_string());
    }
    if !roles.is_empty() {
        let role_changed = [
            SubtitleSettingsField::Default,
            SubtitleSettingsField::Forced,
            SubtitleSettingsField::Original,
            SubtitleSettingsField::Commentary,
        ]
        .into_iter()
        .filter(|field| visible(*field))
        .any(changed);
        language_and_role.push(subtitle_information_field_line(
            "Role",
            &roles.join(" · "),
            role_changed,
        ));
    }
    append_information_group(&mut lines, language_and_role);

    let mut format_and_type = vec![subtitle_information_field_line(
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

fn subtitle_information_field_line(key: &str, value: &str, changed: bool) -> Line<'static> {
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

fn append_map(lines: &mut Vec<Line<'static>>, map: &BTreeMap<String, Value>, depth: usize) {
    for (key, value) in map {
        match value {
            Value::Object(object) => {
                lines.push(field_line(depth, key, ""));
                let nested = object
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                append_map(lines, &nested, depth + 1);
            }
            _ => lines.push(field_line(depth, key, &value_text(value))),
        }
    }
}

fn field_line(depth: usize, key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  ".repeat(depth)),
        Span::styled(format!("{key}: "), Style::default().fg(Color::Blue).bold()),
        Span::raw(value.to_string()),
    ])
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
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

fn content_popup_area(area: Rect, text: &Text<'_>, title: &str) -> Rect {
    let content_width = text.lines.iter().map(Line::width).max().unwrap_or(0);
    let width = content_width
        .saturating_add(4)
        .max(title.chars().count().saturating_add(4));
    let height = text.lines.len().saturating_add(2);
    centered_fixed(
        area,
        u16::try_from(width).unwrap_or(u16::MAX),
        u16::try_from(height).unwrap_or(u16::MAX),
    )
}

fn subtitle_popup_area(area: Rect, text: &Text<'_>, title: &str) -> Rect {
    let target_width = ((u32::from(area.width) * 80) / 100) as u16;
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

fn disposition_flags(
    stream: &std::collections::BTreeMap<String, Value>,
    default: bool,
) -> Vec<String> {
    const FLAGS: [(&str, &str); 6] = [
        ("default", "default"),
        ("forced", "forced"),
        ("hearing_impaired", "hearing impaired"),
        ("visual_impaired", "visual impaired"),
        ("comment", "commentary"),
        ("dub", "dub"),
    ];

    let disposition = stream.get("disposition").and_then(Value::as_object);
    let mut flags = disposition.map_or_else(Vec::new, |disposition| {
        FLAGS
            .iter()
            .filter(|(key, _)| *key != "default")
            .filter(|(key, _)| disposition.get(*key).and_then(Value::as_i64) == Some(1))
            .map(|(_, label)| format!("[{label}]"))
            .collect::<Vec<_>>()
    });
    if default {
        flags.insert(0, "[default]".to_string());
    }
    flags
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

    #[test]
    fn progress_dialog_should_use_a_standalone_loader_or_a_real_gauge() {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-progress-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(directory.clone(), probe_tx, edit_tx).unwrap();
        app.edit_progress_label = Some("Running OCR on movie.dan.sup → SRT (dan)".to_string());
        app.edit_started = Some(std::time::Instant::now());
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

        app.edit_progress = Some(0.42);
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
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-subtitle-settings-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(directory.clone(), probe_tx, edit_tx).unwrap();
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
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-dialog-selection-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(directory.clone(), probe_tx, edit_tx).unwrap();
        app.layer = Layer::Streams;
        app.selected_stream = 8;

        assert_eq!(details_selected_stream(&app), Some(8));
        app.dialog = Some(Dialog::SubtitleSettings);
        assert_eq!(details_selected_stream(&app), None);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn subtitle_field_help_should_change_with_the_field_and_explain_sidecar_effects() {
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-subtitle-field-help-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(directory.clone(), probe_tx, edit_tx).unwrap();
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
            .contains("not an untouched or source file")
            .does_not_contain("canonical .sdh");

        popup.field = SubtitleSettingsField::Codec;
        let codec = subtitle_field_help_text(&app, &popup).to_string();
        assert_that!(codec)
            .contains("image subtitle to text requires OCR")
            .contains("stored as SubRip / SRT in MKV");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn render_footer_should_include_network_mode_tag_only_when_in_network_mode() {
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(std::env::temp_dir(), probe_tx, edit_tx).unwrap();

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
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(std::env::temp_dir(), probe_tx, edit_tx).unwrap();
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
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-file-search-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        for name in ["movie.mkv", "movie.eng.srt", "movie.nld.srt"] {
            std::fs::write(directory.join(name), b"media").unwrap();
        }
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(directory.clone(), probe_tx, edit_tx).unwrap();
        let movie_path = directory.join("movie.mkv");
        app.start_file_search();
        for ch in "eng".chars() {
            app.file_search_push(ch);
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
            .contains("Search: eng▏ (1 match)")
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
            1,
            true,
        );

        // Assert
        assert_that!(line.to_string())
            .contains("MKV → MP4")
            .contains("1:02:03")
            .contains("1.5 MiB")
            .contains("4.2 Mb/s")
            .contains("1 compatibility conflict");
        assert_eq!(line.style.fg, Some(Color::White));
        assert_eq!(line.style.bg, Some(Color::Cyan));
        assert!(line.style.add_modifier.contains(Modifier::BOLD));
        assert!(line.style.add_modifier.contains(Modifier::ITALIC));
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

        // Assert
        assert_that!(&line)
            .contains("#2")
            .contains("OPUS")
            .contains("5.1")
            .contains("ENG")
            .contains("[default]")
            .does_not_contain("[original]");
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
        let lines = file_tree_lines("movie.mkv", sidecars, false, true);
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
        let folded_lines = file_tree_lines("movie.mkv", sidecars, true, true);
        let folded_text = folded_lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let folded_strs = folded_text.iter().map(String::as_str).collect::<Vec<_>>();

        // Assert
        assert_that!(folded_strs).contains_exactly_in_given_order(["▹ movie.mkv"]);

        // Act - Standalone file when sidecars exist in folder
        let standalone_padded = file_tree_lines("other.mp4", [], false, true);
        assert_eq!(standalone_padded[0].to_string(), "  other.mp4");

        // Act - Standalone file when NO sidecars exist in folder
        let standalone_unpadded = file_tree_lines("other.mp4", [], false, false);
        assert_eq!(standalone_unpadded[0].to_string(), "other.mp4");
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
    fn save_dialog_area_should_grow_for_wrapped_change_lines() {
        // Arrange
        let lines = (0..9)
            .map(|_| Line::from("A change summary that occupies more than one rendered line because it is deliberately longer than the dialog content width"))
            .collect::<Vec<_>>();

        // Act
        let area = save_dialog_area(Rect::new(0, 0, 100, 40), &lines);

        // Assert
        assert_that!(area.width).is_equal_to(68);
        assert_that!(area.height).is_equal_to(20);
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
        let lines = container_information_lines(&info, Path::new("/videos/movie.mkv"), 0);
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
            "Closed captions: No".to_string(),
            "Hearing impaired: Yes".to_string(),
            "Role: Default · Forced · Original".to_string(),
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
            "Hearing impaired: Yes".to_string(),
            "Role: Forced".to_string(),
            "".to_string(),
            "Format: PGS / SUP".to_string(),
            "Type: Image-based".to_string(),
        ]);
    }

    #[test]
    fn subtitle_information_should_follow_staged_values_and_effective_container_role() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-staged-subtitle-information-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(directory.clone(), probe_tx, edit_tx).unwrap();
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
                .contains("Role: Default · Forced · Commentary")
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
                .contains("Role: Forced")
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
        let directory = std::env::temp_dir().join(format!(
            "reel-tui-video-details-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("movie.mkv"), b"media").unwrap();
        let (probe_tx, _) = std::sync::mpsc::channel();
        let (edit_tx, _) = std::sync::mpsc::channel();
        let mut app = App::new(directory.clone(), probe_tx, edit_tx).unwrap();
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
        let (container, container_title, container_compact) = details_popup_content(&app).unwrap();
        app.selected_stream = 1;
        let (video, video_title, video_compact) = details_popup_content(&app).unwrap();
        app.selected_stream = 2;
        let (audio, audio_title, audio_compact) = details_popup_content(&app).unwrap();
        app.selected_stream = 3;
        let (subtitle, subtitle_title, subtitle_compact) = details_popup_content(&app).unwrap();
        app.selected_stream = 4;
        let (external, external_title, external_compact) = details_popup_content(&app).unwrap();

        // Assert
        assert_that!(container_title).is_equal_to(" Container information ".to_string());
        assert_that!(container_compact).is_equal_to(DetailsPopupSizing::Content);
        assert_that!(container.to_string())
            .contains("File name: movie.mkv")
            .contains("Format: MKV");
        assert_that!(video_title).is_equal_to(" Video #0 ".to_string());
        assert_that!(video_compact).is_equal_to(DetailsPopupSizing::Content);
        assert_that!(video.to_string())
            .contains("Format: H.264 (AVC)")
            .contains("Resolution: 1920×1080 · 1080p")
            .does_not_contain("time_base");
        assert_that!(audio_title).is_equal_to(" Audio #1 ".to_string());
        assert_that!(audio_compact).is_equal_to(DetailsPopupSizing::Content);
        assert_that!(audio.to_string())
            .contains("Format: Dolby Digital (AC-3)")
            .contains("Channels: Stereo")
            .contains("Language: Dutch")
            .does_not_contain("time_base");
        assert_that!(subtitle_title).is_equal_to(" Subtitle #2 ".to_string());
        assert_that!(subtitle_compact).is_equal_to(DetailsPopupSizing::Subtitle);
        assert_that!(subtitle.to_string())
            .contains("Title: Not provided")
            .contains("Format: PGS / SUP")
            .contains("Type: Image-based")
            .does_not_contain("Source:")
            .does_not_contain("time_base");
        assert_that!(external_title).is_equal_to(" External subtitle ".to_string());
        assert_that!(external_compact).is_equal_to(DetailsPopupSizing::Subtitle);
        assert_that!(external.to_string())
            .contains("Format: SubRip (SRT)")
            .contains("Language: Dutch")
            .does_not_contain("Source:")
            .contains("File: movie.nld.srt");

        // Cleanup
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn content_popup_area_should_fit_short_information_instead_of_using_most_of_the_screen() {
        // Arrange
        let terminal = Rect::new(0, 0, 120, 40);
        let text = padded_popup_text(Text::from(vec![
            Line::from("Title: English SDH"),
            Line::from(""),
            Line::from("Language: English · CC"),
            Line::from("Role: Default · SDH"),
            Line::from(""),
            Line::from("Format: SubRip (SRT)"),
            Line::from("Type: Text-based"),
        ]));

        // Act
        let area = content_popup_area(terminal, &text, " Subtitle #2 ");

        // Assert
        assert_that!(area.width).is_less_than(60);
        assert_that!(area.height).is_equal_to(11);
    }

    #[test]
    fn subtitle_popup_area_should_be_eighty_percent_wide_and_wrap_height_to_content() {
        // Arrange
        let terminal = Rect::new(0, 0, 120, 40);
        let text = padded_popup_text(Text::from(Line::from("x".repeat(190))));

        // Act
        let area = subtitle_popup_area(terminal, &text, " Subtitle #2 ");

        // Assert
        assert_that!(area.width).is_equal_to(96);
        assert_that!(area.height).is_equal_to(7);
        assert_that!(area.x).is_equal_to(12);
        assert_that!(area.y).is_equal_to(16);
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
        let lines = container_information_lines(&info, Path::new("/videos/movie.mp4"), 0);
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
            "Ctrl-c",
        ];

        // Act
        let help = keybindings_text().to_string();

        // Assert
        for value in expected {
            assert_that!(&help).contains(value);
        }
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
        let line = container_choice_line(&choice, false);
        let focused = container_choice_line(&choice, true);

        // Assert
        assert_that!(line.to_string())
            .contains("MP4  ⚠ can't contain SubRip/SRT or ASS subtitles.");
        assert_eq!(line.spans[1].style.fg, Some(Color::Yellow));
        assert_eq!(focused.spans[1].style.fg, Some(Color::White));
        assert_eq!(focused.spans[1].style.bg, Some(Color::Cyan));
    }

    #[test]
    fn subtitle_codec_line_should_distinguish_original_staged_and_available_codecs() {
        // Act
        let original = subtitle_codec_line("SubRip / SRT", false, false, true, true);
        let staged = subtitle_codec_line("ASS", false, true, true, false);
        let available = subtitle_codec_line("WebVTT", false, false, true, false);
        let staged_original = subtitle_codec_line("SubRip / SRT", false, true, true, true);
        let staged_cursor = subtitle_codec_line("ASS", true, true, true, false);

        // Assert
        assert_that!(original.to_string())
            .contains("(original)")
            .does_not_contain("●");
        assert_eq!(original.spans[0].style.fg, Some(Color::White));
        assert_eq!(original.spans[1].style.fg, Some(Color::DarkGray));
        assert!(
            !original.spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(staged.spans[0].style.fg, Some(Color::Yellow));
        assert!(
            staged.spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(available.spans[0].style.fg, Some(Color::White));
        assert_eq!(staged_original.spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(staged_original.spans[1].style.fg, Some(Color::DarkGray));
        assert_eq!(staged_cursor.spans[0].style.fg, Some(Color::White));
        assert_eq!(staged_cursor.spans[0].style.bg, Some(Color::Cyan));
        assert!(
            staged_cursor.spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn setting_line_should_italicize_only_a_changed_value() {
        // Act
        let unchanged = setting_line("Codec", "SubRip / SRT", false, false);
        let changed = setting_line("Codec", "ASS", false, true);
        let focused_changed = setting_line("Codec", "ASS", true, true);

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
                line.spans[0].content.chars().count(),
                SUBTITLE_CHECKBOX_LABEL_WIDTH
            );
            assert_eq!(
                line.to_string().find('['),
                Some(SUBTITLE_CHECKBOX_LABEL_WIDTH)
            );
        }
    }

    #[test]
    fn custom_input_should_use_a_flat_dark_surface_and_cursor() {
        // Act
        let mut input = TextInputState::new("1280".to_string());
        input.activate();
        let line = custom_input_line("Width", &input, true, false);

        // Assert
        assert_that!(line.to_string())
            .contains("1280")
            .contains("▏")
            .does_not_contain("NORMAL")
            .does_not_contain("INSERT")
            .does_not_contain("╭")
            .does_not_contain("╰");
        assert_eq!(line.spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(line.spans[1].style.bg, Some(Color::Rgb(32, 32, 32)));
    }

    #[test]
    fn custom_input_should_mark_a_changed_value_yellow_and_italic() {
        // Act
        let input = TextInputState::new("1280".to_string());
        let line = custom_input_line("Width", &input, false, true);

        // Assert
        assert_eq!(line.spans[1].style.fg, Some(Color::Yellow));
        assert!(line.spans[1].style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(line.spans[1].style.bg, Some(Color::Rgb(32, 32, 32)));
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
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[3].style.bg, Some(Color::Cyan));
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
        let selected = dropdown_line("1920×1080 / 16:9", false, true, true, false);
        let available = dropdown_line("1280×720 / 16:9", false, false, true, false);

        // Assert
        assert_that!(selected.to_string())
            .starts_with("  > 1920×1080 / 16:9")
            .does_not_contain("●");
        assert_that!(available.to_string())
            .starts_with("    1280×720 / 16:9")
            .does_not_contain("●");
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
    fn destination_option_should_keep_the_chosen_destination_as_a_full_cyan_block() {
        // Act
        let chosen = destination_option(" Replace original ", true);
        let available = destination_option(" Create a copy ", false);

        // Assert
        assert_that!(chosen.content.as_ref()).is_equal_to(" Replace original ");
        assert_eq!(chosen.style.fg, Some(Color::White));
        assert_eq!(chosen.style.bg, Some(Color::Cyan));
        assert!(chosen.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(available.style.fg, Some(Color::White));
        assert_eq!(available.style.bg, None);
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
