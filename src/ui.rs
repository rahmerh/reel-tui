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
        SaveDialogField, SubtitleSettingsField, TrackRef, VideoSettingsField, VideoSettingsMode,
    },
    edit::{ContainerFormat, SaveDestination, stream_index},
    probe::{MediaInfo, ProbeOutcome},
    subtitle::{SidecarEntry, SubtitleFormat, SubtitleSource, stream_cc, stream_forced},
};

const MIN_SUBTITLE_COLUMN_WIDTH: u16 = 38;
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
        render_details_popup(frame, app);
    }
    if let Some(dialog) = app.dialog {
        render_dialog(frame, app, dialog);
    }
}

fn render_files(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.layer == Layer::Files;
    let items: Vec<_> = app
        .files
        .iter()
        .map(|file| {
            ListItem::new(file_tree_lines(
                &file.display_name,
                app.sidecars_for_media(&file.path)
                    .iter()
                    .map(|sidecar| sidecar.display_name.as_str()),
            ))
        })
        .collect();
    let title = if app.is_network_mount {
        format!(" Files ({}) [NET] ", app.files.len())
    } else {
        format!(" Files ({}) ", app.files.len())
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(focus_border(app.layer == Layer::Files))
                .title(title),
        )
        .highlight_style(if focused {
            focused_style(false)
        } else {
            Style::default().fg(Color::White).bold()
        })
        .highlight_symbol(if focused { "› " } else { "  " });
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn file_tree_lines<'a>(
    display_name: &str,
    sidecar_names: impl IntoIterator<Item = &'a str>,
) -> Vec<Line<'static>> {
    let sidecar_names = sidecar_names.into_iter().collect::<Vec<_>>();
    let mut lines = Vec::with_capacity(sidecar_names.len() + 1);
    lines.push(Line::from(display_name.to_string()));
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
                    (app.layer != Layer::Files).then_some(app.selected_stream),
                    MediaTextState {
                        order: &app.stream_order,
                        rows: &rows,
                        sidecars: &app.sidecars,
                        deleted: &app.deleted_streams,
                        defaults: &app.default_streams,
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
                message("Not a video file", reason, Color::Yellow)
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

fn subtitle_columns_fit(content_width: u16, embedded: usize, external: usize) -> bool {
    embedded > 0
        && external > 0
        && content_width
            >= MIN_SUBTITLE_COLUMN_WIDTH
                .saturating_mul(2)
                .saturating_add(SUBTITLE_COLUMN_GUTTER)
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

    let embedded_subtitles = order
        .iter()
        .filter_map(|index| {
            let stream = info
                .streams
                .iter()
                .find(|stream| stream_index(stream) == Some(*index))?;
            let selection_index = rows
                .iter()
                .position(|row| *row == TrackRef::Embedded(*index))?;
            (string(stream, "codec_type") == Some("subtitle")).then_some((selection_index, stream))
        })
        .collect::<Vec<_>>();
    let external_subtitles = sidecars
        .iter()
        .enumerate()
        .map(|(sidecar_index, sidecar)| {
            let selection_index = rows
                .iter()
                .position(|row| *row == TrackRef::Sidecar(sidecar_index))
                .unwrap_or(0);
            (selection_index, sidecar)
        })
        .collect::<Vec<_>>();

    if subtitle_columns_side_by_side {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(subtitle_columns_line(
            Line::styled(
                format!("Embedded subtitles ({})", embedded_subtitles.len()),
                Style::default().fg(Color::Cyan).bold(),
            ),
            Line::styled(
                format!("External subtitles ({})", external_subtitles.len()),
                Style::default().fg(Color::Cyan).bold(),
            ),
            subtitle_column_width,
        ));
        let count = embedded_subtitles.len().max(external_subtitles.len());
        for row in 0..count {
            let left = embedded_subtitles
                .get(row)
                .map(|(selection_index, stream)| {
                    if selected == Some(*selection_index) {
                        selected_line = Some(lines.len());
                    }
                    stream_line(
                        stream,
                        *selection_index,
                        selected == Some(*selection_index),
                        stream_index(stream).is_some_and(|index| deleted.contains(&index)),
                        stream_index(stream).is_some_and(|index| changed.contains(&index)),
                        stream_index(stream)
                            .is_some_and(|index| conflicting_streams.contains(&index)),
                        stream_index(stream).is_some_and(|index| defaults.contains(&index)),
                    )
                });
            let right = external_subtitles
                .get(row)
                .map(|(selection_index, sidecar)| {
                    if selected == Some(*selection_index) {
                        selected_line = Some(lines.len());
                    }
                    sidecar_line(
                        sidecar,
                        selected == Some(*selection_index),
                        subtitle_changes
                            .contains_key(&SubtitleSource::Sidecar(sidecar.path.clone())),
                    )
                });
            lines.push(subtitle_optional_columns_line(
                left,
                right,
                subtitle_column_width,
            ));
        }
    } else {
        if !embedded_subtitles.is_empty() {
            section(
                &mut lines,
                &format!("Embedded subtitles ({})", embedded_subtitles.len()),
            );
            for (selection_index, stream) in embedded_subtitles {
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
        if !external_subtitles.is_empty() {
            section(
                &mut lines,
                &format!("External subtitles ({})", external_subtitles.len()),
            );
            for (selection_index, sidecar) in external_subtitles {
                if selected == Some(selection_index) {
                    selected_line = Some(lines.len());
                }
                lines.push(sidecar_line(
                    sidecar,
                    selected == Some(selection_index),
                    subtitle_changes.contains_key(&SubtitleSource::Sidecar(sidecar.path.clone())),
                ));
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

fn sidecar_line(sidecar: &SidecarEntry, selected: bool, changed: bool) -> Line<'static> {
    let marker = if selected { "›" } else { " " };
    let changed_marker = if changed { "  ✎" } else { "" };
    let details = subtitle_overview_details(
        sidecar.format.label(),
        crate::subtitle::normalized_language(&sidecar.language),
        false,
        sidecar.forced,
        sidecar.cc,
        true,
    );
    Line::from(format!("{marker}     {details}{changed_marker}",)).style(if selected {
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
            |line| (truncate(&line.to_string(), column_width), line.style),
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
    let index = number_string(stream, "index").unwrap_or_else(|| fallback_index.to_string());
    let kind = string(stream, "codec_type").unwrap_or("unknown");
    let codec = string(stream, "codec_name").unwrap_or("unknown");
    let subtitle = kind == "subtitle";
    let mut details = if subtitle {
        vec![subtitle_overview_details(
            &subtitle_format_label(codec),
            crate::subtitle::normalized_language(tag(stream, "language").unwrap_or("und")),
            default,
            stream_forced(stream),
            stream_cc(stream),
            false,
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
        if deleted {
            Span::styled("× ", semantic_span_style)
        } else if conflict {
            Span::styled("⚠ ", semantic_span_style)
        } else if changed {
            Span::styled("~ ", semantic_span_style)
        } else {
            Span::raw("  ")
        },
        Span::styled(format!("#{index:<2} "), index_style),
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

fn subtitle_format_label(codec: &str) -> String {
    crate::subtitle::SubtitleFormat::from_codec(codec).map_or_else(
        || codec.to_ascii_uppercase(),
        |format| format.label().to_string(),
    )
}

fn subtitle_overview_details(
    format: &str,
    language: &str,
    default: bool,
    forced: bool,
    cc: bool,
    external: bool,
) -> String {
    let language = language.to_ascii_uppercase();
    let mut parts = vec![format!("{format:<12}"), language];
    if default {
        parts.push("[Default]".to_string());
    }
    if forced {
        parts.push("[Forced]".to_string());
    }
    if cc {
        parts.push("[CC]".to_string());
    }
    if external {
        parts.push("[External]".to_string());
    }
    parts.join(" · ")
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
    lines.extend(summary.into_iter().map(Line::from));
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

fn render_keybindings_dialog(frame: &mut Frame, app: &mut App) {
    let area = popup_area(frame.area(), 80, 80);
    let text = padded_popup_text(keybindings_text());
    app.set_keybindings_max_scroll(max_scroll(&text, area));

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" Keybindings "),
            )
            .wrap(Wrap { trim: false })
            .scroll((app.keybindings_scroll, 0)),
        area,
    );
}

fn keybindings_text() -> Text<'static> {
    let mut lines = Vec::new();
    keybindings_section(&mut lines, "General");
    keybinding(&mut lines, "?", "Open or close keybindings");
    keybinding(&mut lines, "Esc / q", "Close, go back, or quit");
    keybinding(&mut lines, "j/k / Up/Down", "Move or scroll vertically");
    keybinding(&mut lines, "h/l / Left/Right", "Change a horizontal choice");
    keybinding(&mut lines, "gg / G", "Go to the first/top or last/bottom");
    keybinding(&mut lines, "Ctrl-d / Ctrl-u", "Scroll ten lines");
    keybinding(&mut lines, "Enter", "Open, select, or confirm");

    keybindings_section(&mut lines, "Track editing");
    keybinding(
        &mut lines,
        "Ctrl-j / Ctrl-k",
        "Move track down / up within its type",
    );
    keybinding(&mut lines, "a", "Make track the default for its type");
    keybinding(
        &mut lines,
        "Enter",
        "Edit container, video, or subtitle settings",
    );
    keybinding(&mut lines, "i", "Toggle container or stream information");
    keybinding(&mut lines, "d", "Mark or unmark track for deletion");
    keybinding(&mut lines, "Ctrl-s", "Review and save pending edits");
    keybinding(&mut lines, "Ctrl-c", "Cancel processing");
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
    frame.render_widget(
        Paragraph::new(processing_info_line(app.processing_description())).centered(),
        rows[2],
    );

    const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let tick = app
        .edit_started
        .map_or(0, |started| (started.elapsed().as_millis() / 80) as usize);
    let percent = app
        .edit_progress
        .map(|progress| (progress.clamp(0.0, 1.0) * 100.0).round() as u16);
    frame.render_widget(
        Gauge::default()
            .gauge_style(
                Style::default()
                    .fg(Color::Cyan)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .percent(percent.unwrap_or(0))
            .label(percent.map_or_else(
                || SPINNER[tick % SPINNER.len()].to_string(),
                |value| format!("{value}%"),
            )),
        rows[4],
    );
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
    let width_changed =
        source_dimensions.is_some_and(|(width, _)| draft.width.parse::<u64>().ok() != Some(width));
    let height_changed = source_dimensions
        .is_some_and(|(_, height)| draft.height.parse::<u64>().ok() != Some(height));
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

fn custom_input_line(label: &str, value: &str, focused: bool, changed: bool) -> Line<'static> {
    let input_style = if changed {
        changed_style()
    } else {
        Style::default().fg(Color::White)
    }
    .bg(Color::Rgb(32, 32, 32));
    let cursor = if focused { "▏" } else { "" };
    let prefix = format!(" {value}");
    let padding = " ".repeat(
        CUSTOM_INPUT_WIDTH
            .saturating_sub(prefix.chars().count())
            .saturating_sub(cursor.chars().count()),
    );
    Line::from(vec![
        Span::styled(
            format!("{label:<12}"),
            Style::default().fg(if focused { Color::Cyan } else { Color::Gray }),
        ),
        Span::styled(prefix, input_style),
        Span::styled(
            cursor.to_string(),
            input_style.fg(if focused { Color::Cyan } else { Color::White }),
        ),
        Span::styled(padding, input_style),
    ])
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
    let change = app.subtitle_changes.get(&popup.source);
    let codec_choices = app.subtitle_choices(&popup.source, popup.source_format);
    let codec_selected = |choice: &crate::subtitle::FormatChoice| {
        change.and_then(|change| change.embedded_target) == choice.value
    };
    let codec_staged = |choice: &crate::subtitle::FormatChoice| {
        change.and_then(|change| change.embedded_target).is_some()
            && change.and_then(|change| change.embedded_target) == choice.value
    };
    let codec_label = codec_choices
        .iter()
        .find(|choice| codec_selected(choice))
        .map(|choice| choice.label.as_str())
        .unwrap_or_else(|| popup.source_format.label());
    let sidecar = matches!(popup.source, SubtitleSource::Sidecar(_));
    let exporting = change.is_some_and(|change| change.export_target.is_some());
    let importing = change.is_some_and(|change| change.import_into_media);
    let mut lines = vec![setting_line(
        "Codec",
        codec_label,
        popup.field == SubtitleSettingsField::Codec,
        change.is_some_and(|change| change.embedded_target.is_some()),
    )];
    if !sidecar {
        lines.push(subtitle_export_line(
            exporting,
            popup.field == SubtitleSettingsField::Export,
        ));
    } else {
        lines.push(subtitle_import_line(
            importing,
            popup.field == SubtitleSettingsField::Import,
        ));
    }

    if popup.dropdown_open {
        lines.push(Line::from(""));
        match popup.field {
            SubtitleSettingsField::Codec => {
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
            SubtitleSettingsField::Export | SubtitleSettingsField::Import => {}
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
    let text = padded_popup_text(Text::from(lines));
    let height = (text.lines.len() as u16 + 2).min(frame.area().height.saturating_sub(2));
    let area = centered_fixed(frame.area(), 76, height.max(8));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(title),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn subtitle_export_line(checked: bool, focused: bool) -> Line<'static> {
    subtitle_checkbox_line("Export", checked, focused)
}

fn subtitle_import_line(checked: bool, focused: bool) -> Line<'static> {
    subtitle_checkbox_line("Import", checked, focused)
}

fn subtitle_checkbox_line(label: &str, checked: bool, focused: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<12}"),
            Style::default().fg(if focused { Color::Cyan } else { Color::Gray }),
        ),
        Span::styled(if checked { "[x]" } else { "[ ]" }, {
            if focused {
                focused_style(checked)
            } else if checked {
                changed_style()
            } else {
                Style::default().fg(Color::White)
            }
        }),
    ])
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
    let Some((text, title, compact)) = details_popup_content(app) else {
        return;
    };
    let text = padded_popup_text(text);
    let area = if compact {
        content_popup_area(frame.area(), &text, &title)
    } else {
        popup_area(frame.area(), 90, 86)
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

fn details_popup_content(app: &App) -> Option<(Text<'static>, String, bool)> {
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
                true,
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
                    true,
                ));
            }
            if kind == "audio" {
                return Some((
                    Text::from(audio_information_lines(stream, default)),
                    format!(" Audio #{index_label} "),
                    true,
                ));
            }
            if kind == "subtitle" {
                return Some((
                    Text::from(embedded_subtitle_information_lines(stream, default)),
                    format!(" Subtitle #{index_label} "),
                    true,
                ));
            }
            let mut lines = Vec::new();
            append_map(&mut lines, stream, 0);
            Some((
                Text::from(lines),
                format!(" Stream #{index_label} · {kind} "),
                false,
            ))
        }
        TrackRef::Sidecar(index) => {
            let sidecar = app.sidecars.get(index)?;
            Some((
                Text::from(sidecar_subtitle_information_lines(sidecar)),
                " External subtitle ".to_string(),
                true,
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
    let canonical_code = match code {
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
        _ => code,
    };
    if let Ok(language) = canonical_code.parse::<Language>() {
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
) -> Vec<Line<'static>> {
    let codec = string(stream, "codec_name").unwrap_or("unknown");
    let format = SubtitleFormat::from_codec(codec);
    let mut lines = Vec::new();

    let title = tag(stream, "title")
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Not provided");
    append_information_group(&mut lines, vec![field_line(0, "Title", title)]);

    let mut language_and_role = Vec::new();
    if let Some(language) = tag(stream, "language")
        .and_then(|language| subtitle_language_description(language, stream_cc(stream)))
    {
        language_and_role.push(field_line(0, "Language", &language));
    }
    let roles = embedded_subtitle_roles(stream, default);
    if !roles.is_empty() {
        language_and_role.push(field_line(0, "Role", &roles.join(" · ")));
    }
    append_information_group(&mut lines, language_and_role);

    let mut format_and_type = vec![field_line(
        0,
        "Format",
        &subtitle_information_format(format, codec),
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

fn sidecar_subtitle_information_lines(sidecar: &SidecarEntry) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    append_information_group(
        &mut lines,
        vec![field_line(0, "File", &sidecar.display_name)],
    );

    let mut language_and_role = Vec::new();
    if let Some(language) = subtitle_language_description(&sidecar.language, sidecar.cc) {
        language_and_role.push(field_line(0, "Language", &language));
    }
    let mut roles = Vec::new();
    if sidecar.forced {
        roles.push("Forced");
    }
    if sidecar.cc {
        roles.push("Closed captions");
    }
    if !roles.is_empty() {
        language_and_role.push(field_line(0, "Role", &roles.join(" · ")));
    }
    append_information_group(&mut lines, language_and_role);

    append_information_group(
        &mut lines,
        vec![
            field_line(
                0,
                "Format",
                &subtitle_information_format(Some(sidecar.format), sidecar.format.ffmpeg_codec()),
            ),
            field_line(
                0,
                "Type",
                if sidecar.format.is_text() {
                    "Text-based"
                } else {
                    "Image-based"
                },
            ),
        ],
    );
    lines
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

fn subtitle_language_description(value: &str, cc: bool) -> Option<String> {
    let language = language_name(value)?;
    Some(if cc {
        format!("{language} · CC")
    } else {
        language
    })
}

fn embedded_subtitle_roles(stream: &BTreeMap<String, Value>, default: bool) -> Vec<String> {
    let mut roles = Vec::new();
    if default {
        roles.push("Default".to_string());
    }
    if stream_forced(stream) {
        roles.push("Forced".to_string());
    }
    if disposition_enabled(stream, "hearing_impaired") {
        roles.push("SDH".to_string());
    } else if stream_cc(stream) {
        roles.push("Closed captions".to_string());
    }
    if disposition_enabled(stream, "original") {
        roles.push("Original".to_string());
    }
    roles
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

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;

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
    fn subtitle_columns_should_only_share_a_row_when_both_fit() {
        // Act / Assert
        assert_that!(subtitle_columns_fit(77, 2, 2)).is_false();
        assert_that!(subtitle_columns_fit(78, 2, 2)).is_true();
        assert_that!(subtitle_columns_fit(120, 0, 2)).is_false();
        assert_that!(subtitle_columns_fit(120, 2, 0)).is_false();
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
        assert_that!(line.to_string()).is_equal_to("…ed subtitle  …al subtitle".to_string());
        assert_that!(line.spans[0].style.fg).contains(Color::Yellow);
        assert_that!(line.spans[2].style.fg).contains(Color::Cyan);
    }

    #[test]
    fn subtitle_import_line_should_show_checked_and_focused_state() {
        // Act
        let line = subtitle_import_line(true, true);

        // Assert
        assert_that!(line.to_string()).is_equal_to("Import      [x]".to_string());
        assert_that!(line.spans[0].style.fg).contains(Color::Cyan);
        assert_that!(line.spans[1].style.bg).contains(Color::Cyan);
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
            .ends_with("SubRip / SRT · ENG · [Default] · [Forced] · [CC]")
            .does_not_contain("English")
            .does_not_contain("[default]")
            .does_not_contain("[forced]");
        assert_eq!(line.find("SubRip / SRT"), Some(6));
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
        assert_that!(line).ends_with("ASS          · UND");
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

        // Act
        let lines = file_tree_lines("movie.mkv", sidecars);
        let text = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        let text = text.iter().map(String::as_str).collect::<Vec<_>>();

        // Assert
        assert_that!(text).contains_exactly_in_given_order([
            "movie.mkv",
            "  ├── movie.eng.srt",
            "  └── movie.nld.forced.ass",
        ]);
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::DarkGray));
        assert_eq!(lines[2].spans[1].style.fg, Some(Color::DarkGray));
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
            cc: true,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        };

        // Act
        let changed = sidecar_line(&sidecar, false, true);
        let focused = sidecar_line(&sidecar, true, true);

        // Assert
        assert_eq!(changed.style.fg, Some(Color::Yellow));
        assert!(changed.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(focused.style.fg, Some(Color::White));
        assert_eq!(focused.style.bg, Some(Color::Cyan));
        assert!(focused.style.add_modifier.contains(Modifier::ITALIC));
        assert_that!(focused.to_string())
            .contains("SubRip / SRT · ENG · [Forced] · [CC] · [External]")
            .contains("✎")
            .does_not_contain("movie.eng.srt")
            .does_not_contain(" - ");
        assert_that!(focused.to_string()).starts_with("›     SubRip / SRT");
    }

    #[test]
    fn subtitle_overview_details_should_keep_format_language_and_external_in_columns() {
        // Act
        let short_format = subtitle_overview_details("ASS", "nld", false, false, false, false);
        let tagged = subtitle_overview_details("ASS", "nld", true, true, true, false);
        let external = subtitle_overview_details("ASS", "nld", false, false, false, true);

        // Assert
        assert_that!(short_format).is_equal_to("ASS          · NLD".to_string());
        assert_that!(tagged)
            .is_equal_to("ASS          · NLD · [Default] · [Forced] · [CC]".to_string());
        assert_that!(external).is_equal_to("ASS          · NLD · [External]".to_string());
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
        let text = embedded_subtitle_information_lines(&stream, true)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        let rendered = text.join("\n");

        // Assert
        assert_that!(text).contains_exactly_in_given_order([
            "Title: English SDH".to_string(),
            "".to_string(),
            "Language: English · CC".to_string(),
            "Role: Default · Forced · SDH · Original".to_string(),
            "".to_string(),
            "Format: SubRip (SRT)".to_string(),
            "Type: Text-based".to_string(),
        ]);
        assert_that!(rendered)
            .does_not_contain("time_base")
            .does_not_contain("Source:");
    }

    #[test]
    fn subtitle_language_description_should_append_cc_only_for_captions() {
        // Act
        let captions = subtitle_language_description("eng", true);
        let subtitles = subtitle_language_description("eng", false);

        // Assert
        assert_that!(captions.as_deref()).contains("English · CC");
        assert_that!(subtitles.as_deref()).contains("English");
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
            cc: true,
            number: None,
            fingerprint: crate::files::FileFingerprint {
                length: 0,
                modified: None,
            },
            companion_fingerprint: None,
        };

        // Act
        let text = sidecar_subtitle_information_lines(&sidecar)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        // Assert
        assert_that!(text).contains_exactly_in_given_order([
            "File: movie.nld.forced.cc.sup".to_string(),
            "".to_string(),
            "Language: Dutch · CC".to_string(),
            "Role: Forced · Closed captions".to_string(),
            "".to_string(),
            "Format: PGS / SUP".to_string(),
            "Type: Image-based".to_string(),
        ]);
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
            cc: false,
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
        assert_that!(container_compact).is_true();
        assert_that!(container.to_string())
            .contains("File name: movie.mkv")
            .contains("Format: MKV");
        assert_that!(video_title).is_equal_to(" Video #0 ".to_string());
        assert_that!(video_compact).is_true();
        assert_that!(video.to_string())
            .contains("Format: H.264 (AVC)")
            .contains("Resolution: 1920×1080 · 1080p")
            .does_not_contain("time_base");
        assert_that!(audio_title).is_equal_to(" Audio #1 ".to_string());
        assert_that!(audio_compact).is_true();
        assert_that!(audio.to_string())
            .contains("Format: Dolby Digital (AC-3)")
            .contains("Channels: Stereo")
            .contains("Language: Dutch")
            .does_not_contain("time_base");
        assert_that!(subtitle_title).is_equal_to(" Subtitle #2 ".to_string());
        assert_that!(subtitle_compact).is_true();
        assert_that!(subtitle.to_string())
            .contains("Title: Not provided")
            .contains("Format: PGS / SUP")
            .contains("Type: Image-based")
            .does_not_contain("Source:")
            .does_not_contain("time_base");
        assert_that!(external_title).is_equal_to(" External subtitle ".to_string());
        assert_that!(external_compact).is_true();
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
            "Esc",
            "gg / G",
            "Ctrl-j / Ctrl-k",
            "Ctrl-s",
            "i",
            "Ctrl-d / Ctrl-u",
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
    fn keybindings_text_should_exclude_space_binding_when_space_action_is_removed() {
        // Arrange

        // Act
        let help = keybindings_text().to_string();

        // Assert
        assert_that!(help).does_not_contain("Space");
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
    fn subtitle_export_line_should_render_an_independent_checkbox() {
        // Act
        let unchecked = subtitle_export_line(false, true);
        let checked = subtitle_export_line(true, false);

        // Assert
        assert_that!(unchecked.to_string())
            .contains("Export      [ ]")
            .does_not_contain("Export sidecar");
        assert_that!(checked.to_string())
            .contains("Export      [x]")
            .does_not_contain("Export sidecar");
        assert!(unchecked.to_string().starts_with("Export"));
        assert_eq!(unchecked.spans[1].style.fg, Some(Color::White));
        assert_eq!(unchecked.spans[1].style.bg, Some(Color::Cyan));
        assert_eq!(checked.spans[1].style.fg, Some(Color::Yellow));
        assert!(
            checked.spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert!(
            !unchecked.spans[1]
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
    fn custom_input_should_use_a_flat_dark_surface_and_cursor() {
        // Act
        let line = custom_input_line("Width", "1280", true, false);

        // Assert
        assert_that!(line.to_string())
            .contains("1280")
            .contains("▏")
            .does_not_contain("╭")
            .does_not_contain("╰");
        assert_eq!(line.spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(line.spans[1].style.bg, Some(Color::Rgb(32, 32, 32)));
    }

    #[test]
    fn custom_input_should_mark_a_changed_value_yellow_and_italic() {
        // Act
        let line = custom_input_line("Width", "1280", false, true);

        // Assert
        assert_eq!(line.spans[1].style.fg, Some(Color::Yellow));
        assert!(line.spans[1].style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(line.spans[1].style.bg, Some(Color::Rgb(32, 32, 32)));
    }

    #[test]
    fn custom_scaling_should_offer_only_exact_output_options() {
        // Arrange
        let draft = crate::app::CustomResolutionDraft {
            width: "1280".to_string(),
            height: "720".to_string(),
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
            width: "1280".to_string(),
            height: "720".to_string(),
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
