use std::collections::BTreeMap;

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
        App, CancelEditChoice, Dialog, Layer, SaveDialogField, SubtitleAction,
        SubtitleSettingsField, TrackRef, VideoSettingsField,
    },
    edit::{SaveDestination, stream_index},
    probe::{MediaInfo, ProbeOutcome},
    subtitle::{SidecarEntry, SubtitleSource},
};

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 50 || area.height < 10 {
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
        render_stream_popup(frame, app);
    }
    if let Some(dialog) = app.dialog {
        render_dialog(frame, app, dialog);
    }
}

fn render_files(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<_> = app
        .files
        .iter()
        .map(|file| ListItem::new(file.display_name.clone()))
        .collect();
    let title = format!(" Files ({}) ", app.files.len());
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(focus_border(app.layer == Layer::Files))
                .title(title),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_details(frame: &mut Frame, app: &mut App, area: Rect) {
    let filename = app
        .selected_file()
        .map(|file| file.display_name.as_str())
        .unwrap_or("Details");
    let title = format!(" {filename} ");

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
    let hint = " ? keybinds ";
    let available = area.width as usize;
    let directory_width = available.saturating_sub(hint.len());
    let directory = truncate(&directory, directory_width);
    let padding = " ".repeat(
        available
            .saturating_sub(directory.chars().count())
            .saturating_sub(hint.len()),
    );
    let line = Line::from(vec![
        Span::styled(directory, Style::default().fg(Color::DarkGray)),
        Span::raw(padding),
        Span::styled(hint, Style::default().fg(Color::Cyan)),
    ]);
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
    } = state;
    let mut lines = Vec::new();
    let mut selected_line = None;
    section(&mut lines, "Overview");
    lines.push(Line::from(format_overview(info)));

    let groups = [
        ("Video", "video"),
        ("Audio", "audio"),
        ("Subtitles", "subtitle"),
    ];

    for (heading, kind) in groups {
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
        let sidecar_count = if kind == "subtitle" {
            sidecars.len()
        } else {
            0
        };
        if !streams.is_empty() || sidecar_count > 0 {
            section(
                &mut lines,
                &format!("{heading} ({})", streams.len() + sidecar_count),
            );
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
                    stream_index(stream).is_some_and(|index| defaults.contains(&index)),
                ));
            }
            if kind == "subtitle" {
                for (sidecar_index, sidecar) in sidecars.iter().enumerate() {
                    let selection_index = rows
                        .iter()
                        .position(|row| *row == TrackRef::Sidecar(sidecar_index))
                        .unwrap_or(0);
                    if selected == Some(selection_index) {
                        selected_line = Some(lines.len());
                    }
                    lines.push(sidecar_line(
                        sidecar,
                        selected == Some(selection_index),
                        subtitle_changes
                            .contains_key(&SubtitleSource::Sidecar(sidecar.path.clone())),
                    ));
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
}

fn sidecar_line(sidecar: &SidecarEntry, selected: bool, changed: bool) -> Line<'static> {
    let marker = if selected { "›" } else { " " };
    let changed = if changed { "  ✎" } else { "" };
    let mut flags = vec!["external".to_string()];
    if sidecar.forced {
        flags.push("forced".to_string());
    }
    if sidecar.cc {
        flags.push("cc".to_string());
    }
    Line::from(format!(
        "{marker}    {}  ·  {}  ·  {}  [{}]{changed}",
        sidecar.format.label(),
        sidecar.language.to_ascii_uppercase(),
        sidecar.display_name,
        flags.join(", ")
    ))
    .style(if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    })
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

fn format_overview(info: &MediaInfo) -> String {
    let mut parts = Vec::new();
    if let Some(format) =
        string(&info.format, "format_long_name").or_else(|| string(&info.format, "format_name"))
    {
        parts.push(format.to_string());
    }
    if let Some(duration) = number_string(&info.format, "duration").and_then(parse_number) {
        parts.push(format_duration(duration));
    }
    if let Some(size) = number_string(&info.format, "size").and_then(parse_number) {
        parts.push(format_bytes(size));
    }
    if let Some(bit_rate) = number_string(&info.format, "bit_rate").and_then(parse_number) {
        parts.push(format_bitrate(bit_rate));
    }
    parts.join("  ·  ")
}

fn stream_line(
    stream: &std::collections::BTreeMap<String, Value>,
    fallback_index: usize,
    selected: bool,
    deleted: bool,
    changed: bool,
    default: bool,
) -> Line<'static> {
    let index = number_string(stream, "index").unwrap_or_else(|| fallback_index.to_string());
    let kind = string(stream, "codec_type").unwrap_or("unknown");
    let codec = string(stream, "codec_name").unwrap_or("unknown");
    let mut details = vec![codec.to_uppercase()];

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

    if let Some(language) = tag(stream, "language")
        && language != "und"
    {
        details.push(language.to_uppercase());
    }
    if let Some(title) = tag(stream, "title") {
        details.push(title.to_string());
    }
    details.extend(disposition_flags(stream, default));

    let line = Line::from(vec![
        if deleted {
            Span::styled("× ", Style::default().fg(Color::Red).bold())
        } else if changed {
            Span::styled("~ ", Style::default().fg(Color::Yellow).bold())
        } else {
            Span::raw("  ")
        },
        Span::styled(
            format!("#{index:<2} "),
            Style::default().fg(if deleted {
                Color::Red
            } else if changed {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ),
        Span::raw(details.join("  ·  ")),
    ]);
    if selected {
        line.style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        line.style(if deleted {
            Style::default().fg(Color::Red)
        } else if changed {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        })
    }
}

fn render_dialog(frame: &mut Frame, app: &mut App, dialog: Dialog) {
    if dialog == Dialog::Keybindings {
        render_keybindings_dialog(frame, app);
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
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(body)
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

fn render_cancel_edit_dialog(frame: &mut Frame, app: &App) {
    let lines = vec![
        Line::from("Are you sure you want to cancel the current operation?").centered(),
        Line::from(""),
        Line::from(vec![
            save_option(
                " Keep processing ",
                app.cancel_edit_choice == CancelEditChoice::KeepProcessing,
            ),
            Span::raw("  "),
            save_option(
                " Cancel processing ",
                app.cancel_edit_choice == CancelEditChoice::CancelProcessing,
            ),
        ])
        .centered(),
    ];
    let area = centered_fixed(frame.area(), 64, 7);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
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
            save_option(
                " Replace original ",
                app.save_destination == SaveDestination::ReplaceOriginal,
            ),
            Span::raw("  "),
            save_option(
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
                Style::default().fg(Color::White).bg(Color::Cyan).bold()
            } else {
                Style::default().fg(Color::White).bold()
            },
        ))
        .centered(),
    );

    let height = (lines.len() as u16 + 2).max(10);
    let area = centered_fixed(frame.area(), 68, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
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
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn save_option(label: &'static str, selected: bool) -> Span<'static> {
    Span::styled(
        label,
        if selected {
            Style::default().fg(Color::White).bg(Color::Cyan).bold()
        } else {
            Style::default().fg(Color::DarkGray)
        },
    )
}

fn render_keybindings_dialog(frame: &mut Frame, app: &mut App) {
    let area = popup_area(frame.area(), 80, 80);
    let text = keybindings_text();
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
        "Edit video settings or convert/export subtitles",
    );
    keybinding(&mut lines, "i", "Open full ffprobe stream information");
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
    let area = centered_fixed(frame.area(), 64, 7);
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
    ])
    .margin(1)
    .split(inner);

    if let Some(progress) = app.edit_progress {
        let percent = (progress.clamp(0.0, 1.0) * 100.0).round() as u16;
        let action = app.edit_progress_label.as_deref().unwrap_or({
            if app.video_settings.is_empty() {
                "Remuxing with ffmpeg…"
            } else {
                "Transcoding with ffmpeg…"
            }
        });
        frame.render_widget(Paragraph::new(action).centered(), rows[0]);
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
            rows[2],
        );
    } else {
        const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let tick = app
            .edit_started
            .map_or(0, |started| (started.elapsed().as_millis() / 80) as usize);
        frame.render_widget(
            Paragraph::new(format!(
                "{}  {}",
                SPINNER[tick % SPINNER.len()],
                app.edit_progress_label.as_deref().unwrap_or({
                    if app.video_settings.is_empty() {
                        "Remuxing with ffmpeg…"
                    } else {
                        "Transcoding with ffmpeg…"
                    }
                })
            ))
            .centered()
            .style(Style::default().fg(Color::Cyan).bold()),
            rows[1],
        );
    }
}

fn render_video_settings_dialog(frame: &mut Frame, app: &App) {
    let Some(popup) = app.video_settings_popup.as_ref() else {
        return;
    };
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
        .find(|choice| choice.value == settings.resolution)
        .map(|choice| choice.label.clone())
        .unwrap_or_else(|| settings.resolution.label().to_string());

    let mut lines = vec![
        setting_line(
            "Codec",
            codec_label,
            popup.field == VideoSettingsField::Codec,
        ),
        setting_line(
            "Resolution",
            &resolution_label,
            popup.field == VideoSettingsField::Resolution,
        ),
    ];
    if popup.dropdown_open {
        lines.push(Line::from(""));
        match popup.field {
            VideoSettingsField::Codec => {
                for (position, choice) in codec_choices.iter().enumerate() {
                    lines.push(subtitle_codec_line(
                        &choice.label,
                        position == popup.codec_cursor,
                        settings.codec != crate::edit::VideoCodec::Original
                            && choice.value == settings.codec,
                        true,
                        choice.current,
                    ));
                }
            }
            VideoSettingsField::Resolution => {
                for (position, choice) in resolution_choices.iter().enumerate() {
                    lines.push(dropdown_line(
                        &choice.label,
                        position == popup.resolution_cursor,
                        choice.value == settings.resolution,
                        choice.enabled,
                    ));
                }
            }
        }
    }

    let height = (lines.len() as u16 + 4).max(7);
    let area = centered_fixed(frame.area(), 58, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(format!(" Video track #{} settings ", popup.stream_index)),
        ),
        area,
    );
}

fn render_subtitle_settings_dialog(frame: &mut Frame, app: &App) {
    let Some(popup) = app.subtitle_settings_popup.as_ref() else {
        return;
    };
    let change = app.subtitle_changes.get(&popup.source);
    let codec_choices = app.subtitle_choices(&popup.source, popup.source_format, popup.action);
    let codec_selected = |choice: &crate::subtitle::FormatChoice| {
        if popup.action == SubtitleAction::ExportSidecar {
            change.and_then(|change| change.export_target) == Some(choice.format)
        } else {
            change.and_then(|change| change.embedded_target) == choice.value
        }
    };
    let codec_staged = |choice: &crate::subtitle::FormatChoice| {
        if popup.action == SubtitleAction::ExportSidecar {
            change.and_then(|change| change.export_target) == Some(choice.format)
        } else {
            change.and_then(|change| change.embedded_target).is_some()
                && change.and_then(|change| change.embedded_target) == choice.value
        }
    };
    let codec_label = codec_choices
        .iter()
        .find(|choice| codec_selected(choice))
        .map(|choice| choice.label.as_str())
        .unwrap_or_else(|| popup.source_format.label());
    let sidecar = matches!(popup.source, SubtitleSource::Sidecar(_));
    let mut lines = Vec::new();
    if !sidecar {
        lines.push(subtitle_action_line(
            popup.action,
            popup.field == SubtitleSettingsField::Action,
        ));
    }
    lines.push(setting_line(
        "Codec",
        codec_label,
        popup.field == SubtitleSettingsField::Codec,
    ));

    if popup.dropdown_open {
        lines.push(Line::from(""));
        match popup.field {
            SubtitleSettingsField::Action => {}
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
    let height = (lines.len() as u16 + 2).min(frame.area().height.saturating_sub(2));
    let area = centered_fixed(frame.area(), 76, height.max(8));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
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

fn subtitle_action_line(action: SubtitleAction, focused: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "Action      ",
            Style::default().fg(if focused { Color::Cyan } else { Color::Gray }),
        ),
        save_option(
            " Convert in container ",
            action == SubtitleAction::ConvertInContainer,
        ),
        Span::raw("  "),
        save_option(" Export sidecar ", action == SubtitleAction::ExportSidecar),
    ])
}

fn setting_line(label: &str, value: &str, selected: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<12}"),
            Style::default().fg(if selected { Color::Cyan } else { Color::Gray }),
        ),
        Span::styled(
            format!("[ {value} ]"),
            if selected {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default()
            },
        ),
    ])
}

fn subtitle_codec_line(
    label: &str,
    cursor: bool,
    staged: bool,
    enabled: bool,
    current: bool,
) -> Line<'static> {
    let background = cursor.then_some(Color::Cyan);
    let mut label_style = if !enabled {
        Style::default().fg(Color::DarkGray)
    } else if staged {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(Color::White)
    };
    if let Some(background) = background {
        label_style = label_style.bg(background).add_modifier(Modifier::BOLD);
    }
    let mut spans = vec![Span::styled(format!("    {label}"), label_style)];
    if current {
        let mut original_style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        if let Some(background) = background {
            original_style = original_style.bg(background);
        }
        spans.push(Span::styled(" (original)", original_style));
    }
    Line::from(spans)
}

fn dropdown_line(label: &str, cursor: bool, selected: bool, enabled: bool) -> Line<'static> {
    let marker = if selected { "●" } else { " " };
    let line = Line::from(format!("  {marker} {label}"));
    if !enabled {
        line.style(Style::default().fg(Color::DarkGray))
    } else if cursor {
        line.style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        line
    }
}

fn render_stream_popup(frame: &mut Frame, app: &mut App) {
    let Some(stream) = app.selected_stream_info() else {
        return;
    };
    let area = popup_area(frame.area(), 90, 86);
    let index = number_string(stream, "index").unwrap_or_else(|| "?".to_string());
    let kind = string(stream, "codec_type")
        .unwrap_or("unknown")
        .to_string();
    let mut lines = Vec::new();
    append_map(&mut lines, stream, 0);
    let text = Text::from(lines);
    app.set_details_max_scroll(max_scroll(&text, area));

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(format!(" Stream #{index} · {kind} ")),
            )
            .wrap(Wrap { trim: false })
            .scroll((app.details_scroll, 0)),
        area,
    );
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
    const FLAGS: [(&str, &str); 7] = [
        ("default", "default"),
        ("forced", "forced"),
        ("hearing_impaired", "hearing impaired"),
        ("visual_impaired", "visual impaired"),
        ("comment", "commentary"),
        ("dub", "dub"),
        ("original", "original"),
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
        format!("{:.1} kHz", hertz / 1000.0)
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
                "disposition": {"default": 1}
            }),
        )
        .unwrap();

        // Act
        let line = stream_line(&stream, 0, false, false, false, true).to_string();

        // Assert
        assert_that!(line)
            .contains("#2")
            .contains("OPUS")
            .contains("5.1")
            .contains("ENG")
            .contains("[default]");
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
            original.spans[1]
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
        assert_eq!(staged_cursor.spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(staged_cursor.spans[0].style.bg, Some(Color::Cyan));
        assert!(
            staged_cursor.spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn subtitle_action_line_should_use_save_dialog_button_styling_without_an_orb() {
        // Act
        let line = subtitle_action_line(SubtitleAction::ConvertInContainer, true);

        // Assert
        assert_that!(line.to_string())
            .contains("Convert in container")
            .contains("Export sidecar")
            .does_not_contain("›")
            .does_not_contain("●");
        assert!(line.to_string().starts_with("Action"));
        assert_eq!(line.spans[1].style.fg, Some(Color::White));
        assert_eq!(line.spans[1].style.bg, Some(Color::Cyan));
        assert_eq!(line.spans[3].style.fg, Some(Color::DarkGray));
        assert_eq!(line.spans[3].style.bg, None);
    }
}
