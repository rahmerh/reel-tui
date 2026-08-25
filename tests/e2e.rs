//! End-to-end regressions: the app driven through keypresses, running real
//! `ffprobe`/`ffmpeg` against real files.
//!
//! Excluded from `cargo test` (see `test = false` in `Cargo.toml`); run with
//! `cargo test --test e2e`.
//!
//! Regression scenarios here originated in failures that reached
//! `~/.cache/reel-tui/edit_errors.log` during real use. The existing unit tests all
//! passed while those bugs were live, because they either build `TrackEdits` by hand
//! (bypassing the `App` seam where several of them originated) or use `ffv1`/
//! `pcm_s16le` fixtures too simple to express the codec/container conflicts that
//! actually broke. Those scenarios quote the original failure in their comments;
//! feature-level scenarios cover complete user workflows independently of unit tests.

// `tests/e2e.rs` is a crate root, so submodules would otherwise resolve against
// `tests/` and collide with any future sibling suite.
#[path = "e2e/fixtures.rs"]
mod fixtures;
#[path = "e2e/harness.rs"]
mod harness;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::KeyCode;
use fixtures::{
    MediaSpec, SubtitleSpec, write_media, write_media_with_chapter_and_attachment,
    write_solid_frame, write_vobsub_media,
};
use harness::{
    Harness, Scratch, codec_names, ctrl, key, languages, probe, require_tools,
    stream_indices_of_type,
};
use reel_tui::app::{
    AudioSettingsField, ContainerSettingsField, Dialog, Layer, PreviewSettings,
    SubtitleSettingsField, TrackRef, VideoSettingsField,
};
use reel_tui::cli::{HELP_TEXT, USAGE, VERSION_TEXT};
use reel_tui::edit::VideoRotation;
use reel_tui::subtitle::ToolCapabilities;
use reel_tui::subtitle_edit::WarmState;

/// An FFmpeg older than the supported floor has to stop `reel` at the door.
///
/// Below FFmpeg 8.1 the `mov` demuxer does not read the ISO-BMFF `name` atom, so
/// ffprobe reports every MP4/MOV track title as absent. Remuxing then erases all of
/// them, `apply_edits` writes the erasure out as `title=`, and `validate_result`
/// compares absent against absent and calls it correct — the loss is silent end to end.
/// Starting up and letting the user reach Save is not an option, so this asserts the
/// refusal happens before the TUI does anything, with a message that names the version
/// found and what it costs.
///
/// The old and unversioned builds are stub `ffprobe`/`ffmpeg` scripts on `PATH` rather
/// than real downloads: the behaviour under test is `reel`'s reaction to a banner, and
/// vendoring a 7.1 toolchain into the test tree to produce one banner would trade
/// minutes of fixture wrangling for nothing. Every other scenario in this file runs the
/// genuine binaries.
#[test]
fn an_unsupported_ffmpeg_should_refuse_to_start_instead_of_silently_losing_titles() {
    let binary = env!("CARGO_BIN_EXE_reel");
    let scratch = Scratch::new("ffmpeg-floor");

    let stub = |name: &str, banner: &str| {
        let path = scratch.join(name);
        fs::write(&path, format!("#!/bin/sh\necho '{banner}'\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    };

    // A real n7.1.1 banner: the newest release that still cannot read the atom.
    stub(
        "ffprobe",
        "ffprobe version n7.1.1-20-g9373b442a6 Copyright (c) 2007-2025",
    );
    stub(
        "ffmpeg",
        "ffmpeg version n7.1.1-20-g9373b442a6 Copyright (c) 2000-2025",
    );

    let run = |path: &str| {
        Command::new(binary)
            .arg(scratch.path())
            .env("PATH", path)
            .output()
            .unwrap()
    };

    let outdated = run(&scratch.path().to_string_lossy());
    assert!(!outdated.status.success(), "an old FFmpeg must not launch");
    assert!(outdated.stdout.is_empty(), "the TUI must not have drawn");
    let message = String::from_utf8(outdated.stderr).unwrap();
    assert!(
        message.contains("8.1") && message.contains("`ffprobe` is 7.1"),
        "the refusal must name the floor and the version found, got: {message}"
    );
    assert!(
        message.contains("silently erases every title"),
        "the refusal must say what running anyway would cost, got: {message}"
    );

    // A git snapshot carries no release in its banner. Guessing would defeat the floor,
    // so it is refused too — with a message that quotes what could not be parsed.
    stub("ffprobe", "ffprobe version N-119779-g6c291232cf Copyright");
    let unversioned = run(&scratch.path().to_string_lossy());
    assert!(!unversioned.status.success());
    let message = String::from_utf8(unversioned.stderr).unwrap();
    assert!(
        message.contains("could not determine the version") && message.contains("N-119779"),
        "an unversioned build must be refused by name, got: {message}"
    );

    // No FFmpeg at all is the same refusal, pointed at installing it.
    let absent = run("");
    assert!(!absent.status.success());
    let message = String::from_utf8(absent.stderr).unwrap();
    assert!(
        message.contains("could not run `ffprobe`") && message.contains("PATH"),
        "a missing FFmpeg must be refused actionably, got: {message}"
    );

    // The gate must not be a blanket refusal. A supported banner gets past it — the run
    // still fails, because a piped stdin is not a terminal, but it fails on something
    // other than the floor.
    stub("ffprobe", "ffprobe version n8.1.2 Copyright (c) 2007-2026");
    stub("ffmpeg", "ffmpeg version n8.1.2 Copyright (c) 2000-2026");
    let supported = run(&scratch.path().to_string_lossy());
    let message = String::from_utf8(supported.stderr).unwrap();
    assert!(
        !message.contains("reel requires FFmpeg"),
        "a supported FFmpeg must clear the floor, got: {message}"
    );

    // The check runs after argument parsing, so `--help` still works on a machine with
    // no FFmpeg at all — otherwise the one command that could explain the requirement
    // would be the one the requirement blocks.
    let help = Command::new(binary)
        .arg("--help")
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(help.status.success(), "--help must not need FFmpeg");
    assert_eq!(help.stdout, HELP_TEXT.as_bytes());
}

/// Command-line informational and usage-error paths must exit before terminal setup
/// or media workers start, with output suitable for scripts and shell users.
#[test]
fn cli_flags_should_report_help_version_and_usage_errors_without_starting_the_tui() {
    let binary = env!("CARGO_BIN_EXE_reel");

    for flag in ["--help", "-h"] {
        let output = Command::new(binary).arg(flag).output().unwrap();
        assert!(output.status.success(), "{flag} should succeed");
        assert_eq!(output.stdout, HELP_TEXT.as_bytes());
        assert!(output.stderr.is_empty());
    }

    for flag in ["--version", "-V"] {
        let output = Command::new(binary).arg(flag).output().unwrap();
        assert!(output.status.success(), "{flag} should succeed");
        assert_eq!(output.stdout, VERSION_TEXT.as_bytes());
        assert!(output.stderr.is_empty());
    }

    let output = Command::new(binary).arg("--wat").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "error: unknown option '--wat'\n\n{USAGE}\n\n\
             For more information, try '--help'.\n"
        )
    );
}

/// Browsing is the front door to every edit. This scenario covers the real directory
/// monitor, probe worker, file search (including a sidecar-only match), fold commands,
/// track navigation, and the rendered container/audio inspection views as one workflow.
#[test]
fn browsing_searching_and_inspecting_media_should_use_the_live_file_tree() {
    let test = "browsing_searching_and_inspecting_media_should_use_the_live_file_tree";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("browse-inspect");
    write_media(
        &scratch.join("alpha.mkv"),
        &MediaSpec::mkv().audio(&["eng"]),
    );
    write_media(
        &scratch.join("bravo.mkv"),
        &MediaSpec::mkv().audio(&["eng"]),
    );
    fs::write(
        scratch.join("bravo.nld.srt"),
        "1\n00:00:00,000 --> 00:00:00,500\nNederlandse ondertiteling\n\n",
    )
    .unwrap();

    let mut app = Harness::start(scratch);
    app.wait_until("both media files to appear", |state| state.files.len() == 2);

    app.press(key(KeyCode::Char('z')));
    app.press(key(KeyCode::Char('R')));
    app.pump();
    assert!(
        app.screen().contains("bravo.nld.srt"),
        "unfolding should reveal the matched sidecar\nscreen:\n{}",
        app.screen()
    );

    app.press(key(KeyCode::Char('/')));
    for character in "nld".chars() {
        app.press(key(KeyCode::Char(character)));
    }
    app.pump();
    assert_eq!(app.app.file_panel_entries().len(), 1);
    assert_eq!(
        app.app
            .selected_file()
            .map(|file| file.display_name.as_str()),
        Some("bravo.mkv"),
        "a sidecar-only match should retain its parent media file"
    );
    assert!(app.screen().contains("bravo.nld.srt"));
    assert!(!app.screen().contains("alpha.mkv"));

    app.press(key(KeyCode::Enter));
    app.press(key(KeyCode::Esc));
    assert!(app.app.file_search.value.is_empty());
    app.open("bravo.mkv");

    app.select_track_row(0);
    app.press(key(KeyCode::Char('i')));
    assert_eq!(app.app.layer, Layer::StreamDetails);
    app.pump();
    let container = app.screen();
    assert!(container.contains("Container information"));
    assert!(container.contains("Format: MKV"));
    assert!(container.contains("Duration:"));

    app.press(key(KeyCode::Char('i')));
    assert_eq!(app.app.layer, Layer::Streams);
    let audio_row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(1))
        .expect("the fixture should expose its audio track");
    app.select_track_row(audio_row);
    app.press(key(KeyCode::Char('i')));
    app.pump();
    let audio = app.screen();
    assert!(audio.contains("Audio #1"));
    assert!(audio.contains("Language: English"));
    assert!(audio.contains("Format: AAC"));
    assert!(audio.contains("Channels: Stereo"));
    assert!(audio.contains("Sample rate: 48 kHz"));
}

/// Container metadata is edited in the same popup as format conversion, but its
/// text-editor path and FFmpeg mapping are independent. Exercise every exposed field
/// through key handling and verify the values from the container written on disk.
#[test]
fn editing_container_metadata_should_persist_every_exposed_field() {
    let test = "editing_container_metadata_should_persist_every_exposed_field";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("container-metadata");
    write_media(&scratch.join("clip.mkv"), &MediaSpec::mkv().audio(&["eng"]));

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    app.open_container_settings();
    for (field, value) in [
        (ContainerSettingsField::Title, "The Test Reel"),
        (ContainerSettingsField::Comment, "E2E container comment"),
        (ContainerSettingsField::Date, "2026"),
        (ContainerSettingsField::Genre, "Documentary"),
        (ContainerSettingsField::Artist, "Reel Director"),
    ] {
        app.type_container_metadata(field, value);
    }
    app.close_container_settings();

    assert!(
        app.app.container_metadata.is_some(),
        "editing metadata through the popup should stage a media edit"
    );
    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    assert_eq!(after.container_title().as_deref(), Some("The Test Reel"));
    assert_eq!(
        after.container_comment().as_deref(),
        Some("E2E container comment")
    );
    assert_eq!(after.container_date().as_deref(), Some("2026"));
    assert_eq!(after.container_genre().as_deref(), Some("Documentary"));
    assert_eq!(after.container_artist().as_deref(), Some("Reel Director"));
}

/// Track order is staged from the visual rows but applied by FFmpeg stream mapping.
/// Reorder two independent stream groups and verify both orders in the saved file so
/// neither group can accidentally overwrite or mask the other one's edit.
#[test]
fn reordering_audio_and_subtitle_tracks_should_persist_both_group_orders() {
    let test = "reordering_audio_and_subtitle_tracks_should_persist_both_group_orders";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("track-order");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv().audio(&["eng", "nld"]).subtitles(vec![
            SubtitleSpec::new("fra", "subrip"),
            SubtitleSpec::new("deu", "subrip"),
        ]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");

    for stream_index in [1, 3] {
        let row = app
            .app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(stream_index))
            .unwrap_or_else(|| panic!("stream #{stream_index} should have a track row"));
        app.select_track_row(row);
        app.press(harness::ctrl('j'));
    }
    assert_eq!(
        app.app.stream_order,
        [0, 2, 1, 4, 3],
        "both the first audio and first subtitle should move below their neighbor"
    );

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    assert_eq!(
        stream_languages_of_type(&after, "audio"),
        ["nld", "eng"],
        "the audio order should match the track list"
    );
    assert_eq!(
        stream_languages_of_type(&after, "subtitle"),
        ["deu", "fra"],
        "the subtitle order should match the track list"
    );
}

/// Subtitle metadata has container-dependent visibility and disposition semantics.
/// Edit every field Matroska exposes, move the single-default flag from its neighbor,
/// and verify both the edited and untouched tracks after the real remux.
#[test]
fn editing_subtitle_metadata_should_persist_flags_and_preserve_its_neighbor() {
    let test = "editing_subtitle_metadata_should_persist_flags_and_preserve_its_neighbor";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-metadata");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv().audio(&["eng"]).subtitles(vec![
            SubtitleSpec {
                default: true,
                ..SubtitleSpec::new("eng", "subrip")
            },
            SubtitleSpec::new("nld", "subrip"),
        ]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let edited_row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(3))
        .expect("the second subtitle should have a row");
    app.open_subtitle_settings(edited_row);
    app.choose_subtitle_language("spanish", "spa");
    app.type_subtitle_title("Director subtitles");
    for field in [
        SubtitleSettingsField::Default,
        SubtitleSettingsField::Forced,
        SubtitleSettingsField::HearingImpaired,
        SubtitleSettingsField::Original,
        SubtitleSettingsField::Commentary,
    ] {
        app.toggle_subtitle_field(field);
    }
    assert!(
        !app.app
            .visible_subtitle_fields()
            .contains(&SubtitleSettingsField::Cc),
        "Matroska's unsupported CC flag must not appear editable"
    );
    app.close_subtitle_settings();

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    let subtitles = after
        .streams
        .iter()
        .filter(|stream| {
            stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("subtitle")
        })
        .collect::<Vec<_>>();
    assert_eq!(subtitles.len(), 2);
    let untouched = subtitles[0];
    let edited = subtitles[1];

    assert_eq!(stream_tag(untouched, "language"), Some("eng"));
    assert_eq!(stream_tag(untouched, "title"), None);
    assert!(!stream_disposition(untouched, "default"));
    for flag in ["forced", "hearing_impaired", "original", "comment"] {
        assert!(
            !stream_disposition(untouched, flag),
            "the neighboring subtitle unexpectedly gained {flag}"
        );
    }

    assert_eq!(stream_tag(edited, "language"), Some("spa"));
    assert_eq!(stream_tag(edited, "title"), Some("Director subtitles"));
    for flag in [
        "default",
        "forced",
        "hearing_impaired",
        "original",
        "comment",
    ] {
        assert!(
            stream_disposition(edited, flag),
            "the edited subtitle is missing {flag}: {edited:?}"
        );
    }
}

/// Export an embedded subtitle through the column-transfer key, including filename
/// metadata that cannot be checked by probing the remuxed media alone.
#[test]
fn exporting_an_embedded_subtitle_should_publish_a_named_sidecar_and_remove_the_track() {
    let test = "exporting_an_embedded_subtitle_should_publish_a_named_sidecar_and_remove_the_track";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-export");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .audio(&["eng"])
            .subtitles(vec![SubtitleSpec::new("nld", "subrip")]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let subtitle_row = app.first_subtitle_row();
    app.open_subtitle_settings(subtitle_row);
    app.toggle_subtitle_field(SubtitleSettingsField::Forced);
    app.toggle_subtitle_field(SubtitleSettingsField::HearingImpaired);
    app.close_subtitle_settings();

    app.press(harness::ctrl('l'));
    assert!(
        app.app.is_stream_exported(2),
        "Ctrl-l should move the embedded subtitle to the external column"
    );
    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let sidecar = app.path("clip.nld.forced.sdh.srt");
    assert!(
        sidecar.exists(),
        "the exported sidecar should encode language and flags in its filename; files: {:?}",
        fs::read_dir(app.directory())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>()
    );
    let body = fs::read_to_string(&sidecar).expect("the exported SRT should be readable text");
    assert!(body.contains("nld subtitle line"));
    assert!(
        stream_indices_of_type(&probe(&app.path("clip.mkv")), "subtitle").is_empty(),
        "exporting should remove the embedded copy"
    );
}

/// Import a metadata-bearing sidecar through the opposite transfer key, then edit
/// the fields that become available only after it is staged as embedded. The save
/// must consume the sidecar and preserve all metadata on the new media track.
#[test]
fn importing_a_sidecar_should_embed_it_with_filename_and_popup_metadata() {
    let test = "importing_a_sidecar_should_embed_it_with_filename_and_popup_metadata";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-import");
    write_media(&scratch.join("clip.mkv"), &MediaSpec::mkv().audio(&["eng"]));
    fs::write(
        scratch.join("clip.spa.forced.sdh.srt"),
        "1\n00:00:00,000 --> 00:00:00,500\nImportado\n\n",
    )
    .unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let sidecar_row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Sidecar(0))
        .expect("the sidecar should have an external track row");
    app.select_track_row(sidecar_row);
    app.press(harness::ctrl('h'));
    assert!(
        app.app.is_sidecar_imported(0),
        "Ctrl-h should move the sidecar to the embedded column"
    );

    let imported_row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Sidecar(0))
        .expect("the staged import should still have a selectable row");
    app.open_subtitle_settings(imported_row);
    app.type_subtitle_title("Imported subtitles");
    app.toggle_subtitle_field(SubtitleSettingsField::Default);
    app.close_subtitle_settings();

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    assert!(
        !app.path("clip.spa.forced.sdh.srt").exists(),
        "a successfully imported sidecar should be consumed"
    );
    let after = probe(&app.path("clip.mkv"));
    let subtitle = after
        .streams
        .iter()
        .find(|stream| {
            stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("subtitle")
        })
        .expect("the imported subtitle should be embedded");
    assert_eq!(stream_tag(subtitle, "language"), Some("spa"));
    assert_eq!(stream_tag(subtitle, "title"), Some("Imported subtitles"));
    for flag in ["default", "forced", "hearing_impaired"] {
        assert!(
            stream_disposition(subtitle, flag),
            "the imported subtitle is missing {flag}: {subtitle:?}"
        );
    }
}

/// A save started from the file list must collect edits staged in different files,
/// dispatch every staged item, and leave an unstaged neighbor byte-identical.
#[test]
fn processing_all_should_save_multiple_staged_files_without_touching_unstaged_media() {
    let test = "processing_all_should_save_multiple_staged_files_without_touching_unstaged_media";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("multi-file-batch");
    for name in ["alpha.mkv", "bravo.mkv", "untouched.mkv"] {
        write_media(
            &scratch.join(name),
            &MediaSpec::mkv().audio(&["eng", "nld"]),
        );
    }
    let untouched_before = fs::read(scratch.join("untouched.mkv")).unwrap();

    let mut app = Harness::start(scratch);
    let (notification_tx, notification_rx) = std::sync::mpsc::channel();
    app.app
        .set_completion_notification_sender(Some(notification_tx));
    // This scenario is about notifications firing at all, not the separate
    // focus-gating behavior covered by its own scenario — a fresh app starts assumed
    // focused, which would otherwise suppress every notification checked below.
    app.app.set_terminal_focused(false);
    for name in ["alpha.mkv", "bravo.mkv"] {
        app.open(name);
        let second_audio = app
            .app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(2))
            .expect("the second audio should have a track row");
        app.select_track_row(second_audio);
        app.press(key(KeyCode::Char('d')));
        assert!(
            app.app.deleted_streams.contains(&2),
            "{name} should stage its second audio for deletion"
        );
        app.press(key(KeyCode::Esc));
        assert_eq!(app.app.layer, Layer::Files);
    }
    // Moving off the second edited file snapshots its live edit into `staged_edits`,
    // exactly as moving off the first one did.
    app.press(key(KeyCode::Char('j')));
    app.pump();
    assert_eq!(
        app.app
            .selected_file()
            .map(|file| file.display_name.as_str()),
        Some("untouched.mkv")
    );
    app.wait_until("the unstaged neighbor to finish probing", |state| {
        !state.loading && matches!(state.outcome, Some(reel_tui::probe::ProbeOutcome::Video(_)))
    });
    assert_eq!(
        app.app.staged_edits.len(),
        2,
        "both edited files should remain staged from the file list"
    );

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();
    assert!(
        app.app
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Processed 2 files")),
        "the completion notice should account for the whole batch: {:?}",
        app.app.notice
    );

    for name in ["alpha.mkv", "bravo.mkv"] {
        assert_eq!(
            stream_indices_of_type(&probe(&app.path(name)), "audio").len(),
            1,
            "{name} should have had exactly one audio track removed"
        );
    }
    assert_eq!(
        fs::read(app.path("untouched.mkv")).unwrap(),
        untouched_before,
        "processing staged files must not rewrite an unstaged neighbor"
    );
    let mut notified = notification_rx
        .try_iter()
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect::<Vec<_>>();
    notified.sort();
    assert_eq!(
        notified,
        ["alpha.mkv", "bravo.mkv"],
        "each successfully processed file should emit one completion notification"
    );
}

/// A completion notification is meant to tell you about a file the terminal isn't
/// currently showing — while it's the focused window, the result is already on
/// screen. `App::set_terminal_focused` drives this from `main`'s
/// `FocusGained`/`FocusLost` events; this scenario proves the real save path honors
/// the flag both ways, not just the gate's unit-level logic in isolation.
#[test]
fn a_focused_terminal_should_not_receive_the_completion_notification_an_unfocused_one_would() {
    let test =
        "a_focused_terminal_should_not_receive_the_completion_notification_an_unfocused_one_would";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("notification-focus-gating");
    for name in ["focused.mkv", "unfocused.mkv"] {
        write_media(
            &scratch.join(name),
            &MediaSpec::mkv().audio(&["eng", "nld"]),
        );
    }

    let mut app = Harness::start(scratch);
    let (notification_tx, notification_rx) = std::sync::mpsc::channel();
    app.app
        .set_completion_notification_sender(Some(notification_tx));

    for (name, focused) in [("focused.mkv", true), ("unfocused.mkv", false)] {
        app.app.set_terminal_focused(focused);
        app.open(name);
        let second_audio = app
            .app
            .track_rows()
            .iter()
            .position(|track| *track == TrackRef::Embedded(2))
            .expect("the second audio should have a track row");
        app.select_track_row(second_audio);
        app.press(key(KeyCode::Char('d')));
        app.press(key(KeyCode::Esc));
        assert_eq!(app.app.layer, Layer::Files);

        app.process_all();
        app.assert_batch_succeeded();
        // A one-file batch reopens that file's Streams view on completion; the next
        // iteration's `open` needs the file list to select from.
        if app.app.layer != Layer::Files {
            app.press(key(KeyCode::Esc));
            assert_eq!(app.app.layer, Layer::Files);
        }
    }

    let notified = notification_rx
        .try_iter()
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(
        notified,
        ["unfocused.mkv"],
        "focused.mkv finished while the terminal was focused and must stay silent; \
         unfocused.mkv finished after focus was reported lost and must notify"
    );
}

/// Cancelling is cooperative across the UI, shared batch flag, FFmpeg process, and
/// transaction cleanup. Use a real long-running transcode so the confirmation path
/// can stop active work, then prove the original and staged edit both survive.
#[test]
fn cancelling_an_active_transcode_should_preserve_the_source_and_staged_edit() {
    let test = "cancelling_an_active_transcode_should_preserve_the_source_and_staged_edit";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("cancel-transcode");
    let mut spec = MediaSpec::mkv().size(1280, 720).audio(&["eng"]);
    spec.duration = 60.0;
    write_media(&scratch.join("clip.mkv"), &spec);
    let before = fs::read(scratch.join("clip.mkv")).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    app.set_custom_resolution("640", "360");

    app.press(harness::ctrl('s'));
    assert_eq!(
        app.app.dialog,
        Some(reel_tui::app::Dialog::ConfirmProcessAll)
    );
    app.press(key(KeyCode::Enter));
    assert_eq!(app.app.dialog, Some(reel_tui::app::Dialog::BatchProcessing));

    app.press(key(KeyCode::Esc));
    assert_eq!(app.app.dialog, Some(reel_tui::app::Dialog::ConfirmCancel));
    app.press(key(KeyCode::Char('l')));
    app.press(key(KeyCode::Enter));
    assert_eq!(app.app.dialog, Some(reel_tui::app::Dialog::BatchProcessing));
    assert!(
        app.app
            .active_batch
            .as_ref()
            .is_some_and(|batch| batch.cancelled.load(std::sync::atomic::Ordering::Relaxed)),
        "confirming cancellation should signal the worker"
    );

    app.wait_until("the cancelled transcode to finish cleanup", |state| {
        state.active_batch.is_none()
    });
    assert!(
        app.app
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("cancelled")),
        "the completed cancellation should be reported: {:?}",
        app.app.notice
    );
    assert_eq!(
        fs::read(app.path("clip.mkv")).unwrap(),
        before,
        "cancelling must preserve the original byte-for-byte"
    );
    assert!(
        !app.app.video_settings.is_empty()
            || app.app.staged_edits.contains_key(&app.path("clip.mkv")),
        "a cancelled edit should remain staged for a later retry"
    );
    app.assert_no_temp_leftovers();
}

/// Codec selection is a separate video-popup path from the already-covered custom
/// resolution editor. Drive the dropdown into a genuine HEVC encode and verify that
/// the neighboring audio stream is copied without losing its metadata.
#[test]
fn changing_the_video_codec_should_encode_only_the_selected_track() {
    let test = "changing_the_video_codec_should_encode_only_the_selected_track";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:libx265", "ffmpeg:aac"]);

    let scratch = Scratch::new("video-codec");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv().size(160, 120).audio(&["nld"]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let video_row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(0))
        .expect("the video track should have a row");
    app.choose_video_codec(video_row, "HEVC / H.265");
    assert!(
        !app.app.video_settings.is_empty(),
        "the codec dropdown should stage a video encode"
    );

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    assert_eq!(codec_names(&after), ["hevc", "aac"]);
    assert_eq!(stream_tag(&after.streams[1], "language"), Some("nld"));
    assert!(stream_disposition(&after.streams[1], "default"));
}

/// If another program replaces a staged file, only edits for track groups whose
/// structure changed should be discarded. Container metadata is independent of the
/// changed audio group and must remain processable after the conflict is acknowledged.
#[test]
fn an_external_track_change_should_revert_only_the_conflicting_edits() {
    let test = "an_external_track_change_should_revert_only_the_conflicting_edits";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("external-conflict");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv().audio(&["eng", "nld"]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let second_audio = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(2))
        .expect("the second audio should have a track row");
    app.select_track_row(second_audio);
    app.press(key(KeyCode::Char('d')));
    app.open_container_settings();
    app.type_container_metadata(ContainerSettingsField::Title, "Keep this title");
    app.close_container_settings();

    let replacement = app.directory().join(".replacement.mkv");
    write_media(&replacement, &MediaSpec::mkv().audio(&["fra"]));
    fs::rename(&replacement, app.path("clip.mkv")).unwrap();

    app.wait_until("the external track conflict to be detected", |state| {
        state.dialog == Some(reel_tui::app::Dialog::ResolveConflicts)
    });
    let path = app.path("clip.mkv");
    assert_eq!(app.app.conflicting_paths(), [path.clone()][..]);
    assert!(
        app.app
            .conflicting_change_summary(&path)
            .iter()
            .any(|line| line.to_ascii_lowercase().contains("audio")),
        "the removed audio track should be identified as the conflicting edit"
    );
    assert!(
        app.app
            .kept_change_summary(&path)
            .iter()
            .any(|line| line.contains("container metadata: title")),
        "the independent title edit should be listed as kept"
    );

    app.press(key(KeyCode::Enter));
    assert_eq!(
        app.app.dialog,
        Some(reel_tui::app::Dialog::ResolveConflicts),
        "the conflict acknowledgement should remain guarded during its countdown"
    );
    app.wait_until(
        "the conflict acknowledgement countdown to finish",
        |state| state.conflict_countdown().is_none(),
    );
    app.press(key(KeyCode::Enter));
    assert_eq!(app.app.dialog, None);
    assert!(app.app.conflicting_paths().is_empty());
    assert!(
        app.app.deleted_streams.is_empty(),
        "the stale audio deletion should have been reverted"
    );
    assert!(
        app.app.container_metadata.is_some(),
        "the unrelated container title should remain staged"
    );

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&path);
    assert_eq!(stream_languages_of_type(&after, "audio"), ["fra"]);
    assert_eq!(after.container_title().as_deref(), Some("Keep this title"));
}

/// Bitmap subtitles cross every expensive subtitle boundary: extraction, OCR,
/// conversion, validation, sidecar publication, and removal from the media. Drive the
/// whole path from the TUI using a genuine VobSub source and real Tesseract.
#[test]
fn exporting_vobsub_as_srt_should_run_real_ocr_and_publish_valid_text() {
    let test = "exporting_vobsub_as_srt_should_run_real_ocr_and_publish_valid_text";
    require_tools(
        test,
        &["ffmpeg:libx264", "ffmpeg:aac", "seconv", "tesseract"],
    );

    let scratch = Scratch::new("vobsub-ocr");
    write_vobsub_media(&scratch.join("clip.mkv"), "eng");

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let subtitle_row = app.first_subtitle_row();
    app.select_track_row(subtitle_row);
    app.press(harness::ctrl('l'));
    assert!(app.app.is_stream_exported(2));
    app.convert_subtitle_to(subtitle_row, "SubRip / SRT");

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let sidecar = app.path("clip.eng.srt");
    assert!(
        sidecar.exists(),
        "OCR should publish an English SRT sidecar"
    );
    let body = fs::read_to_string(&sidecar).expect("the OCR output should be text");
    assert!(
        body.contains("-->") && !body.trim().is_empty(),
        "the published file should contain valid-looking subtitle timing: {body:?}"
    );
    assert!(
        stream_indices_of_type(&probe(&app.path("clip.mkv")), "subtitle").is_empty(),
        "the exported bitmap track should be removed from the media"
    );
}

/// Three cues with a deliberate overlap between the first two, so the subtitle edit page has
/// to pack two lanes rather than one.
const OVERLAPPING_CUES: &str = "1\n00:00:00,500 --> 00:00:02,000\nOverlapping opener\n\n\
                                2\n00:00:01,500 --> 00:00:03,000\nOverlapping answer\n\n\
                                3\n00:00:04,000 --> 00:00:05,000\nClosing line\n\n";

const SIDECAR_CUES: &str = "1\n00:00:01,000 --> 00:00:02,000\nSidecar first\n\n\
                            2\n00:00:03,000 --> 00:00:04,000\nSidecar second\n\n";

/// The subtitle edit page reads a track's cues in the background and draws them.
///
/// One fixture carries both source shapes — an embedded `subrip` stream, which the
/// preview worker extracts with a real `ffmpeg`, and a `.srt` sidecar, which it reads
/// straight off disk — so a single build covers both prepare paths. The page is opened,
/// navigated and left through genuine keypresses, and leaving it has to take its scratch
/// directory with it.
#[test]
fn the_subtitle_edit_page_should_load_cues_for_embedded_and_sidecar_srt_tracks() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "the_subtitle_edit_page_should_load_cues_for_embedded_and_sidecar_srt_tracks";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-edit");
    write_media(
        &scratch.join("clip.mkv"),
        // Big enough and long enough to grab a real frame at a cue: the cues run to five
        // seconds, and a seek past the end produces no frame at all.
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(6.0)
            .audio(&["eng"])
            .subtitles(vec![
                SubtitleSpec::new("nld", "subrip").cues(OVERLAPPING_CUES),
            ]),
    );
    fs::write(scratch.join("clip.eng.srt"), SIDECAR_CUES).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");

    // The embedded track: extracted by the worker before anything can be drawn.
    let subtitle_row = app.first_subtitle_row();
    app.select_track_row(subtitle_row);
    app.press(key(KeyCode::Char('c')));
    assert_eq!(app.app.layer, Layer::SubtitleEdit, "c should open the page");
    let workspace = app
        .app
        .subtitle_edit
        .as_ref()
        .expect("the page should be open")
        .workspace()
        .to_path_buf();
    app.wait_until("the embedded track's cues to be read", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| !state.cues.is_empty())
    });

    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert_eq!(state.cues.len(), 3, "every cue in the track should be read");
    assert_eq!(
        state.layout.lane_count, 2,
        "the two overlapping cues need a lane each"
    );
    let screen = app.screen();
    // The first two cues are on screen together, so they are one row of the list and carry
    // the compact timing that fits half a panel; the third stands alone and keeps the full
    // one. See **Cues That Share The Screen Are One Row** in AGENTS.md.
    assert!(
        screen.contains("Overlapping opener") && screen.contains("0:00.5→0:02.0"),
        "the cue list should show cue text and timing:\n{screen}"
    );
    assert!(
        screen.contains("00:00:04.0 → 00:00:05.0"),
        "a cue that overlaps nothing should keep the full timing:\n{screen}"
    );
    let selected = app.filled_selection();
    assert!(
        selected.contains("0:00.5→0:02.0") && selected.contains("Overlapping opener"),
        "the first cue's block should start out filled: {selected:?}\n{screen}"
    );
    assert!(
        screen.contains("Timeline (00:00:00.5 → 00:00:02.0)"),
        "the timeline should name the selected cue's exact span:\n{screen}"
    );
    // The time axis: a ten-second reading to judge cue widths against, and the selected
    // cue's two ends marked on it. Found by the marks, since nothing else on the page
    // draws one — the cue list prints times of its own, so the whole screen cannot be
    // asked whether a reading is present.
    let axis = screen
        .lines()
        .find(|line| line.contains('▲'))
        .unwrap_or_else(|| panic!("the timeline should mark the selected cue:\n{screen}"));
    assert!(
        axis.contains("0:00"),
        "the axis should read out absolute time: {axis:?}"
    );

    // The frame: a real `ffmpeg` seek with the cue burned in by libass, decoded and
    // encoded for the pane by the worker.
    app.wait_until("a frame for the first cue", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.frame().is_some())
    });
    let shades = app.preview_shades();
    assert!(
        shades.len() > 1,
        "the preview should hold a decoded image rather than a blank or solid pane; \
         shades: {shades:?}\nscreen:\n{}",
        app.screen()
    );
    assert_eq!(
        app.app.subtitle_edit.as_ref().unwrap().frame_error(),
        None,
        "a frame that drew should leave no failure behind"
    );

    // Navigating the list moves the selection, and only the selection — and the frame
    // follows it, rather than the previous cue's picture staying under the new cue. `l`
    // rather than `j` for the second cue: it shares the screen with the first, so the two
    // are one row and `j` would step over both of them.
    app.press(key(KeyCode::Char('l')));
    app.pump();
    assert_eq!(app.app.subtitle_edit.as_ref().unwrap().selected, 1);
    let screen = app.screen();
    let selected = app.filled_selection();
    assert!(
        selected.contains("0:01.5→0:03.0") && selected.contains("Overlapping answer"),
        "l should move the fill onto the second cue: {selected:?}\n{screen}"
    );
    assert!(
        screen.contains("Timeline (00:00:01.5 → 00:00:03.0)"),
        "the timeline's title should follow the selection:\n{screen}"
    );
    app.wait_until("a frame for the second cue", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.frame().is_some())
    });

    // Leaving releases the page and its scratch directory.
    app.press(key(KeyCode::Esc));
    app.pump();
    assert_eq!(app.app.layer, Layer::Streams);
    assert!(app.app.subtitle_edit.is_none(), "Esc should close the page");
    assert!(
        !workspace.exists(),
        "closing the page should remove its workspace at {}",
        workspace.display()
    );

    // The sidecar: read directly, with no ffmpeg involved at all. With both an embedded
    // track and a sidecar present the subtitle rows are drawn as two columns, and `l` is
    // how the cursor crosses to the external one — `j` deliberately stays in its column.
    app.press(key(KeyCode::Char('l')));
    app.pump();
    assert_eq!(
        app.app.selected_track(),
        Some(TrackRef::Sidecar(0)),
        "l should move onto the sidecar's column"
    );
    app.press(key(KeyCode::Char('c')));
    app.wait_until("the sidecar's cues to be read", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| !state.cues.is_empty())
    });

    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert_eq!(state.cues.len(), 2, "the sidecar holds two cues");
    assert_eq!(
        state.layout.lane_count, 1,
        "the sidecar's cues do not overlap"
    );
    let screen = app.screen();
    assert!(
        screen.contains("Sidecar first") && screen.contains("Sidecar second"),
        "the sidecar's own cues should be on screen:\n{screen}"
    );
}

/// `p` plays the stretch of media around the selected cue as a slideshow with its sound.
///
/// Drives the whole thing through the real application: one `ffmpeg` decoding a span into
/// raw frames with the cue burned in by libass, the slicing that reads frames out of it,
/// the halfblocks encode, the audio clock the picture is derived from, and the event loop
/// stepping the two together.
///
/// The audio device is real here and stays real on a runner that has none — `audio::open`
/// falls back to `SilentOutput`, which is the production path for a machine without a
/// sound card. The fixture's audio track is `anullsrc`, so a developer running this hears
/// nothing either way.
#[test]
fn the_subtitle_edit_page_should_play_the_span_around_a_cue() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "the_subtitle_edit_page_should_play_the_span_around_a_cue";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-playback");
    write_media(
        &scratch.join("clip.mkv"),
        // Long enough that a cue in the middle has real media either side of it, which is
        // what the padding is for — a span clamped at both ends would prove nothing about
        // the padding at all.
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(20.0)
            .audio(&["eng"]),
    );
    // One cue, well inside the file: 8.0–10.0 s, so the span runs 6.0–12.0 s.
    fs::write(
        scratch.join("clip.eng.srt"),
        "1\n00:00:08,000 --> 00:00:10,000\nPLAY THIS LINE\n\n",
    )
    .unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);
    app.wait_until("a still frame for the cue", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.frame().is_some())
    });
    let still = app.preview_shades();

    // Act
    app.press(key(KeyCode::Char('p')));
    app.pump();

    // Assert: the page says it is working, so the key does not read as having done nothing
    // during the second or two the decode takes.
    let screen = app.screen();
    assert!(
        screen.contains("Preparing playback"),
        "pressing p should say a playback is being prepared:\n{screen}"
    );

    // Assert: the span arrives and starts drawing.
    app.wait_until("the span to start playing", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.playback_frame().is_some())
    });
    let state = app.app.subtitle_edit.as_ref().unwrap();
    let first = state
        .playback_position()
        .expect("a playing span knows where it is");
    assert!(
        first >= Duration::from_secs(7) && first < Duration::from_secs(8),
        "the span should start a second before the cue, not at it: {first:?}"
    );
    assert_eq!(
        state.playback_error(),
        None,
        "a span that played should leave no failure behind"
    );

    // Assert: the playback has taken the pane over from the still frame. The fixture's
    // video is solid black, so the span opens on a single shade where the still frame — a
    // grab from inside the cue, with the line burned into it — has several.
    let playing = app.preview_shades();
    assert!(
        playing != still,
        "the playback should take the pane over from the still frame; \
         playing: {playing:?}, still: {still:?}"
    );

    // Counted rather than searched for, since the same glyph draws every vertical border.
    let marks = |app: &Harness| app.screen().matches('│').count();
    let with_playhead = marks(&app);

    // Assert: over the span the picture moves, the playhead follows it, and — the part
    // nothing short of the pixels would notice — the burned line *arrives and leaves*.
    //
    // That last one is what says the cue was staged at its place inside the span rather
    // than covering it. A staging that covered the whole span passes every other assertion
    // here: same frame count, same playhead, same everything but the picture. Against solid
    // black, more than one shade on screen is the line and nothing else.
    let started = Instant::now();
    let mut moved_playhead = false;
    let mut with_line = false;
    let mut without_line = false;
    let mut ended = false;
    while started.elapsed() < harness::DEFAULT_TIMEOUT {
        app.pump();
        let Some(state) = app.app.subtitle_edit.as_ref() else {
            break;
        };
        if !state.playback_active() {
            ended = true;
            break;
        }
        if let Some(position) = state.playback_position()
            && position > first
        {
            moved_playhead = true;
        }
        if state.playback_frame().is_some() {
            if app.preview_shades().len() > 1 {
                with_line = true;
            } else {
                without_line = true;
            }
        }
    }
    assert!(
        moved_playhead,
        "the playhead should follow the sound through the span"
    );
    assert!(
        with_line,
        "the cue should be burned into the frames where it falls inside the span"
    );
    assert!(
        without_line,
        "the cue should be absent from the padding either side of it, \
         rather than covering the whole span"
    );

    // Assert: a playback is over when the span is, not paused on its last frame — the next
    // `p` should replay it rather than having to stop it first.
    assert!(
        ended,
        "a six-second span should finish on its own well inside the timeout"
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "a six-second span should not take thirty seconds to play: {:?}",
        started.elapsed()
    );
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert!(
        state.playback_frame().is_none(),
        "a finished playback should hand the pane back to the still frame"
    );
    assert_eq!(
        marks(&app),
        with_playhead - 1,
        "the playhead should go when the playback does:\n{}",
        app.screen()
    );

    // Assert: and the same key stops one that is still being decoded, so pressing it by
    // mistake does not mean sitting through a playback you have decided against.
    app.press(key(KeyCode::Char('p')));
    app.pump();
    assert!(
        app.app
            .subtitle_edit
            .as_ref()
            .is_some_and(|state| state.preparing_playback().is_some()),
        "p should start another span"
    );
    app.press(key(KeyCode::Char('p')));
    app.pump();
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert!(
        !state.playback_active(),
        "p again should stop a span that is still decoding"
    );

    // Assert: and Esc peels the playback before it closes the page.
    app.press(key(KeyCode::Char('p')));
    app.pump();
    app.press(key(KeyCode::Esc));
    app.pump();
    assert_eq!(
        app.app.layer,
        Layer::SubtitleEdit,
        "Esc should stop the playback before leaving the page"
    );
    assert!(
        app.app
            .subtitle_edit
            .as_ref()
            .is_some_and(|state| !state.playback_active()),
        "Esc should have stopped the playback"
    );
    app.press(key(KeyCode::Esc));
    app.pump();
    assert_eq!(
        app.app.layer,
        Layer::Streams,
        "Esc should then close the page"
    );
}

/// The preview-settings popup decides how the *next* playback is decoded.
///
/// Asserted end to end because the popup is wired through five separate things that each
/// look right on their own: a key that opens a dialog, a dialog that mutates
/// `PreviewSettings`, a request built from those settings, an `ffmpeg` command built from
/// that request, and a page that maps the resulting playhead back to media time. A unit test
/// of any one of them passes while the chain is broken anywhere else.
///
/// Speed is checked by *rate* rather than by the command line: at half speed the playhead
/// crosses the media at half the wall clock, which is the thing the user is actually
/// judging, and which a `setpts` that reached `ffmpeg` but was never accounted for in
/// `Playback::position` would fail while every command-line assertion still passed.
#[test]
fn preview_settings_should_change_how_the_next_playback_is_decoded() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "preview_settings_should_change_how_the_next_playback_is_decoded";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("preview-settings");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(20.0)
            .audio(&["eng"]),
    );
    // Two cues: a long one to measure a speed against, and a short one whose span ends
    // quickly enough that a loop is what tells it apart from a playback that never started.
    fs::write(
        scratch.join("clip.eng.srt"),
        "1\n00:00:08,000 --> 00:00:10,000\nPLAY THIS LINE\n\n\
         2\n00:00:14,000 --> 00:00:14,500\nAND THIS ONE\n\n",
    )
    .unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);
    app.wait_until("a still frame for the cue", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.frame().is_some())
    });

    // Arrange: an ordinary playback first, so the muted one below follows a run that left
    // its sound in the page's workspace — the order that made a muted playback inherit the
    // previous cue's audio. The silence itself is asserted where it is observable, in
    // `preview::tests::a_muted_playback_should_not_inherit_the_sound_of_the_one_before_it`:
    // the page takes a span's samples the moment it arrives, so by the time a scenario can
    // look at one, every playback holds none.
    app.press(key(KeyCode::Char('p')));
    app.wait_until("a first, unmuted span to play", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.playback_frame().is_some())
    });
    app.press(key(KeyCode::Char('p')));
    app.pump();

    // Act: open the popup and set half speed, no sound, and no padding.
    app.press(key(KeyCode::Char(':')));
    app.pump();
    let screen = app.screen();
    assert!(
        screen.contains("Preview settings") && screen.contains("Speed"),
        "`:` should open the preview settings popup:\n{screen}"
    );
    // `K` explains the row under the cursor, in the panel every other settings popup uses.
    app.press(key(KeyCode::Char('K')));
    app.pump();
    let helped = app.screen();
    assert!(
        helped.contains("Information about Speed") && helped.contains("How fast the preview runs"),
        "`K` should explain the focused row:\n{helped}"
    );
    app.press(key(KeyCode::Char('K')));
    app.pump();
    assert!(
        !app.screen().contains("Information about"),
        "`K` again should put the explanation away"
    );

    // Speed: Enter opens the list, G walks to its slowest entry, k steps back up to half,
    // Enter commits — the lists run fastest first, so the slow end is the bottom. Assert the
    // list is really on screen first, since a dropdown that opened into nothing would still
    // leave the keys below doing something plausible.
    app.press(key(KeyCode::Enter));
    app.pump();
    let open = app.screen();
    assert!(
        open.contains("0.25x") && open.contains("2x"),
        "Enter should open the speed dropdown with every speed in it:\n{open}"
    );
    app.press(key(KeyCode::Char('G')));
    app.press(key(KeyCode::Char('k')));
    app.press(key(KeyCode::Enter));
    app.pump();
    // The Frame rate row follows the speed, and only end to end does that mean anything: the
    // fixture is a 10 fps source, so at half speed it has five distinct frames to give each
    // second of playback and the row has to say so. The config file asked for thirty; before
    // the speed was folded into the cap this row read `10 fps`, naming a rate the decode
    // below would never produce.
    let capped = app.screen();
    assert!(
        capped.contains("5 fps") && !capped.contains("10 fps"),
        "the frame rate row should follow the speed, not just the source:\n{capped}"
    );
    // Sound: a toggle, so `l` picks the right-hand button where a dropdown would need three
    // keys.
    app.press(key(KeyCode::Char('j')));
    app.press(key(KeyCode::Char('j')));
    app.press(key(KeyCode::Char('l')));
    // Padding: down to the list's last entry, which is no padding at all.
    app.press(key(KeyCode::Char('j')));
    app.press(key(KeyCode::Enter));
    app.press(key(KeyCode::Char('G')));
    app.press(key(KeyCode::Enter));
    app.press(key(KeyCode::Esc));
    app.pump();

    // Assert: the popup closed onto the page rather than out of it, and the page says what
    // it will now do without the user having to open the popup again to find out.
    assert_eq!(
        app.app.layer,
        Layer::SubtitleEdit,
        "Esc should close the popup, not the page"
    );
    let screen = app.screen();
    assert!(
        screen.contains("0.5x") && screen.contains("muted"),
        "the preview pane should name the settings that differ from the config file:\n{screen}"
    );

    // Act
    app.press(key(KeyCode::Char('p')));
    app.wait_until("the span to start playing", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.playback_frame().is_some())
    });

    // Assert: no padding means the span starts at the cue rather than a second before it,
    // which is the settings reaching the request.
    let state = app.app.subtitle_edit.as_ref().unwrap();
    let started_at = state
        .playback_position()
        .expect("a playing span knows where it is");
    assert!(
        started_at >= Duration::from_secs(8) && started_at < Duration::from_millis(8_300),
        "a playback with no padding should start at the cue, not before it: {started_at:?}"
    );

    // Assert: and the playhead crosses the media at about half the wall clock.
    //
    // Sampled over a couple of seconds rather than a fraction of one. The playhead moves a
    // frame at a time, and the fixture is a 10 fps source played at half speed — which
    // `source_capped_fps` correctly asks for five frames a second of — so a short window
    // would measure mostly the gap to the next frame.
    let sampling = Instant::now();
    let mut moved = Duration::ZERO;
    while sampling.elapsed() < Duration::from_millis(2_000) {
        app.pump();
        let Some(state) = app.app.subtitle_edit.as_ref() else {
            break;
        };
        if !state.playback_active() {
            break;
        }
        if let Some(position) = state.playback_position() {
            moved = position.saturating_sub(started_at);
        }
    }
    let elapsed = sampling.elapsed().as_secs_f64();
    let rate = moved.as_secs_f64() / elapsed;
    assert!(
        (0.3..0.75).contains(&rate),
        "half speed should cross the media at about half the wall clock, \
         not {rate:.2}x ({moved:?} of media in {elapsed:.2} s)"
    );

    // Assert: the popup cannot be raised over the playback. A span's pixels reach the
    // terminal through its image protocol rather than through the cell buffer a dialog is
    // drawn into, so a popup opened here would be painted once, wiped by the next frame,
    // and left open swallowing every key while invisible.
    app.press(key(KeyCode::Char(':')));
    app.pump();
    assert!(
        app.app.dialog.is_none(),
        "`:` should be inert while a span is playing, not open a popup the next frame wipes"
    );
    app.press(key(KeyCode::Char('?')));
    app.pump();
    assert!(
        app.app.dialog.is_none(),
        "`?` should be inert while a span is playing, for the same reason"
    );

    // Act: back to the config file's settings, then loop the short cue instead.
    app.press(key(KeyCode::Char('p')));
    app.pump();
    app.press(key(KeyCode::Char(':')));
    app.press(key(KeyCode::Char('R')));
    app.pump();
    let screen = app.screen();
    assert!(
        !screen.contains("0.5x") && !screen.contains("muted"),
        "resetting should take the badge away again:\n{screen}"
    );
    // Loop on — a toggle, so `h` picks its left-hand button — then padding back to nothing.
    app.press(key(KeyCode::Char('j')));
    app.press(key(KeyCode::Char('h')));
    app.press(key(KeyCode::Char('j')));
    app.press(key(KeyCode::Char('j')));
    app.press(key(KeyCode::Enter));
    app.press(key(KeyCode::Char('G')));
    app.press(key(KeyCode::Enter));
    app.press(key(KeyCode::Esc));
    app.press(key(KeyCode::Char('j')));
    app.press(key(KeyCode::Char('p')));
    app.wait_until("the short span to start playing", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.playback_frame().is_some())
    });

    // Assert: a half-second span is still going several seconds later, which it could only
    // be by starting again.
    let looping = Instant::now();
    while looping.elapsed() < Duration::from_secs(3) {
        app.pump();
        assert!(
            app.app
                .subtitle_edit
                .as_ref()
                .is_some_and(|state| state.playback_active()),
            "a looping playback should start again rather than end after {:?}",
            looping.elapsed()
        );
    }

    // Assert: and it is still an ordinary playback — `p` stops it, and Esc leaves the page.
    app.press(key(KeyCode::Char('p')));
    app.pump();
    assert!(
        app.app
            .subtitle_edit
            .as_ref()
            .is_some_and(|state| !state.playback_active()),
        "p should stop a looping playback"
    );
    app.press(key(KeyCode::Esc));
    app.pump();
    assert_eq!(
        app.app.layer,
        Layer::Streams,
        "Esc should close the page once nothing is playing"
    );
}

/// A terminal with no image protocol gets a page that says so, and renders nothing.
///
/// The page's whole job is judging a subtitle against the picture it is burned into, and a
/// terminal that cannot draw a picture cannot answer that. So `preview::drawing_picker`
/// refuses the halfblocks fallback at startup and the page opens with its reason on screen
/// — rather than rendering, caching and playing frames that arrive as coloured mush and
/// leave the user judging the subtitle by them.
///
/// Asserted at this level because the refusal has to hold across four things that are wired
/// separately: the cue list still loads, no frame is requested, the background cache pass
/// never starts, and `p` starts no playback. Any one of them left on would spend real work
/// on a picture that cannot be shown.
#[test]
fn a_terminal_with_no_image_protocol_should_say_so_rather_than_draw() {
    let _frame_cache = harness::frame_cache_lock();
    let test = "a_terminal_with_no_image_protocol_should_say_so_rather_than_draw";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("no-image-protocol");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(20.0)
            .audio(&["eng"]),
    );
    fs::write(
        scratch.join("clip.eng.srt"),
        "1\n00:00:08,000 --> 00:00:10,000\nPLAY THIS LINE\n\n",
    )
    .unwrap();

    let mut app = Harness::start_without_image_protocol(scratch);
    // Read after the harness starts, since that is what redirects `XDG_CACHE_HOME`.
    let before = cached_tracks();
    app.open("clip.mkv");
    // The page opens and reads the track — the cue list is the half that still works.
    open_sidecar_edit_page(&mut app);

    // Assert: and it says why there is no picture, rather than leaving the pane blank —
    // silence there is indistinguishable from a render that has not finished.
    let screen = app.screen();
    assert!(
        screen.contains("cannot display images"),
        "the page should name what the terminal cannot do:\n{screen}"
    );

    // Assert: nothing was drawn and nothing is coming.
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert!(
        state.frame().is_none(),
        "a page that cannot draw should hold no frame"
    );
    assert_eq!(
        state.support,
        reel_tui::subtitle_edit::PreviewSupport::NoImageProtocol,
        "the page should know why it is empty"
    );

    // Act / Assert: and `p` starts no playback either, however long it is given to.
    app.press(key(KeyCode::Char('p')));
    for _ in 0..10 {
        app.pump();
    }
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert!(
        state.preparing_playback().is_none() && state.playback_frame().is_none(),
        "a terminal that cannot draw a frame should not start a playback"
    );

    // Assert: and the frame cache gained nothing, so the refusal cost no ffmpeg either.
    // Compared against a reading taken before the page opened rather than against zero:
    // `XDG_CACHE_HOME` is process-global, so this directory also holds whatever the other
    // scenarios in this binary have rendered.
    assert_eq!(
        cached_tracks(),
        before,
        "a page that cannot draw should render no frames"
    );
}

/// The media directories the preview frame cache currently holds.
fn cached_tracks() -> BTreeSet<std::ffi::OsString> {
    fs::read_dir(
        reel_tui::cache::DiskCache::cache_dir()
            .expect("the redirected cache directory")
            .join("preview_frames"),
    )
    .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
    .unwrap_or_default()
}

/// `c` on a row the subtitle edit page does not cover names that kind of track and says the
/// feature is missing, rather than telling the reader to select something else.
///
/// Pressing it on a video or audio track is a reasonable thing to try — the page is about
/// editing a track, and which tracks it can edit is not written on the row — so the answer
/// has to be about the gap in the program rather than about the reader's choice. Covers
/// the two selectable non-subtitle kinds and the container row, since each takes a
/// different branch to its subject; runs no ffmpeg beyond building the fixture.
#[test]
fn the_edit_page_should_name_the_track_kind_it_cannot_edit_yet() {
    let test = "the_edit_page_should_name_the_track_kind_it_cannot_edit_yet";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-edit-unimplemented");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .audio(&["eng"])
            .subtitles(vec![SubtitleSpec::new("eng", "subrip").cues(SIDECAR_CUES)]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");

    let rows = app.app.track_rows();
    let video_row = rows
        .iter()
        .position(|track| matches!(track, TrackRef::Embedded(0)))
        .expect("the fixture should have a video row");
    let container_row = rows
        .iter()
        .position(|track| *track == TrackRef::Container)
        .expect("the overview should have a container row");
    let audio_row = video_row + 1;

    for (row, expected) in [
        (video_row, "Editing video tracks is not implemented yet."),
        (audio_row, "Editing audio tracks is not implemented yet."),
        (
            container_row,
            "Editing the container is not implemented yet.",
        ),
    ] {
        app.select_track_row(row);
        app.press(key(KeyCode::Char('c')));
        app.pump();

        assert_eq!(
            app.app.layer,
            Layer::Streams,
            "row {row} should not have opened the subtitle edit page"
        );
        assert!(
            app.app.subtitle_edit.is_none(),
            "row {row} should not have left page state behind"
        );
        assert_eq!(
            app.app.notice.as_deref(),
            Some(expected),
            "row {row} should say which kind of track is not editable yet"
        );
        assert!(
            app.screen().contains(expected),
            "the refusal should reach the screen:\n{}",
            app.screen()
        );
    }

    // And the subtitle track beside them still opens, so the refusal is about the kind of
    // row rather than about the file.
    let subtitle_row = app.first_subtitle_row();
    app.select_track_row(subtitle_row);
    app.press(key(KeyCode::Char('c')));
    assert_eq!(
        app.app.layer,
        Layer::SubtitleEdit,
        "the subtitle track should still open the page"
    );
    app.press(key(KeyCode::Esc));
}

/// A format the page has no road to a cue list through is turned away at the door rather
/// than opening a page that can never fill in. Covers a text format and a bitmap one;
/// runs no ffmpeg beyond building the fixtures.
///
/// The text one is TTML, and it is a **sidecar** rather than an embedded track: TTML has no
/// FFmpeg decoder at all, so there is nothing to mux one into a Matroska with. That is also
/// exactly why the page refuses it. ASS used to stand here and no longer can — it opens the
/// page now, with its own styles, which
/// `an_ass_track_should_preview_with_its_own_styles_rather_than_libass_defaults` covers.
#[test]
fn the_subtitle_edit_page_should_refuse_a_format_it_cannot_read() {
    let test = "the_subtitle_edit_page_should_refuse_a_format_it_cannot_read";
    require_tools(
        test,
        &["ffmpeg:libx264", "ffmpeg:aac", "seconv", "tesseract"],
    );

    let scratch = Scratch::new("subtitle-edit-refusal");
    write_vobsub_media(&scratch.join("bitmap.mkv"), "eng");
    write_media(
        &scratch.join("timed.mkv"),
        &MediaSpec::mkv().audio(&["eng"]),
    );
    fs::write(
        scratch.join("timed.eng.ttml"),
        "<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div>\
         <p begin=\"00:00:01.000\" end=\"00:00:02.000\">A timed line</p>\
         </div></body></tt>",
    )
    .unwrap();

    let mut app = Harness::start(scratch);
    // Alphabetical, because the file panel is walked downward from wherever the cursor
    // already is.
    for (file, format) in [("bitmap.mkv", "VobSub"), ("timed.mkv", "TTML")] {
        app.open(file);
        // One fixture carries its track inside the container and the other beside it, so
        // the sidecar's row is taken when there is one and the embedded track's otherwise.
        let sidecar_row = app
            .app
            .track_rows()
            .iter()
            .position(|track| matches!(track, TrackRef::Sidecar(_)));
        let subtitle_row = match sidecar_row {
            Some(row) => row,
            None => app.first_subtitle_row(),
        };
        app.select_track_row(subtitle_row);
        app.press(key(KeyCode::Char('c')));
        app.pump();

        assert_eq!(
            app.app.layer,
            Layer::Streams,
            "{file} should not have opened the subtitle edit page"
        );
        assert!(
            app.app.subtitle_edit.is_none(),
            "{file} should not have left page state behind"
        );
        let notice = app
            .app
            .notice
            .clone()
            .unwrap_or_else(|| panic!("{file} should have been refused with a reason"));
        assert!(
            notice.contains(format) && notice.contains("not implemented yet"),
            "the refusal should name the format it turned away: {notice:?}"
        );
        assert!(
            app.screen().contains("not implemented yet"),
            "the refusal should reach the screen:\n{}",
            app.screen()
        );
        app.press(key(KeyCode::Esc));
    }
}

/// A WebVTT track opens the subtitle edit page and draws frames, exactly as SubRip does.
///
/// The page's own parser reads SubRip and nothing else, so this track reaches the cue list
/// only by being transcoded on the way out of the container — one `ffmpeg` rather than
/// two, since the extraction does it as it demuxes. What makes this worth an end-to-end
/// scenario is that the staged filename and the extraction's `-c:s` have to agree: written
/// to `cues.vtt` instead, the WebVTT muxer refuses the SubRip the extraction hands it, and
/// the page fails with a codec error nothing about WebVTT would suggest.
#[test]
fn the_subtitle_edit_page_should_read_a_webvtt_track_by_transcoding_it() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "the_subtitle_edit_page_should_read_a_webvtt_track_by_transcoding_it";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-edit-webvtt");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(6.0)
            .audio(&["eng"])
            .subtitles(vec![SubtitleSpec::new("eng", "webvtt").cues(WALKED_CUES)]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let subtitle_row = app.first_subtitle_row();
    app.select_track_row(subtitle_row);
    app.press(key(KeyCode::Char('c')));
    app.pump();

    assert_eq!(
        app.app.layer,
        Layer::SubtitleEdit,
        "a WebVTT track should open the page; notice: {:?}",
        app.app.notice
    );
    app.wait_until("the transcoded track's cues to be read", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| !state.cues.is_empty())
    });

    // The cues themselves, with their own timings — not a single cue spanning the clip,
    // which is what a transcode that dropped the timing would collapse to.
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert_eq!(
        state.cues.len(),
        4,
        "every cue should have survived the transcode: {:?}",
        state.cues
    );
    assert_eq!(state.cues[0].text, "Walkedone");
    assert_eq!(state.cues[3].text, "Walkedfour");
    assert!(
        state.cues[1].start > state.cues[0].start,
        "the cues should keep their own timings: {:?}",
        state.cues
    );
    let screen = app.screen();
    assert!(
        screen.contains("Walkedone") && screen.contains("Walkedfour"),
        "the cue list should be drawn:\n{screen}"
    );

    // And a real frame gets burned and drawn, so the whole pipeline behind the transcode
    // works rather than only the parsing half.
    app.wait_until("a frame for the first cue", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.frame().is_some())
    });
    assert!(
        app.preview_shades().len() > 1,
        "the preview should hold a decoded image:\n{}",
        app.screen()
    );
}

/// Chapters and attachments are not editable track groups, but every ordinary remux
/// must carry them through untouched. Delete an audio stream to force a real rewrite
/// and verify both less-common structures survive alongside the intended edit.
#[test]
fn remuxing_tracks_should_preserve_chapters_and_attachments() {
    let test = "remuxing_tracks_should_preserve_chapters_and_attachments";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("preserve-container-data");
    write_media_with_chapter_and_attachment(&scratch.join("clip.mkv"));
    let before = probe(&scratch.join("clip.mkv"));
    assert_eq!(
        before.chapters.len(),
        1,
        "fixture should contain one chapter"
    );
    assert_eq!(
        stream_indices_of_type(&before, "attachment").len(),
        1,
        "fixture should contain one attachment"
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let second_audio = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(2))
        .expect("the second audio should have a row");
    app.select_track_row(second_audio);
    app.press(key(KeyCode::Char('d')));
    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    assert_eq!(stream_indices_of_type(&after, "audio").len(), 1);
    assert_eq!(
        after.chapters.len(),
        1,
        "the chapter should survive the remux"
    );
    assert_eq!(
        after.chapters[0]
            .get("tags")
            .and_then(|tags| tags.get("title"))
            .and_then(serde_json::Value::as_str),
        Some("Opening")
    );
    let attachments = after
        .streams
        .iter()
        .filter(|stream| {
            stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("attachment")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        attachments.len(),
        1,
        "the attachment should survive the remux"
    );
    assert_eq!(stream_tag(attachments[0], "filename"), Some("notes.txt"));
    assert_eq!(stream_tag(attachments[0], "mimetype"), Some("text/plain"));
}

/// > SourceChanged: test2.mp4 — The file's tracks changed: track(s) [7] are both kept
/// > and marked for deletion. Reopen it and try again.
///
/// `confirm_process_all` sent the raw display `stream_order`, which still contains
/// deleted tracks, so `validate_edit` saw the same track as both kept and deleted and
/// refused every save with a message blaming the file for changing. Deleting a track
/// and saving is the single most common thing the app does.
#[test]
fn deleting_a_track_should_save_instead_of_claiming_the_file_changed() {
    let test = "deleting_a_track_should_save_instead_of_claiming_the_file_changed";
    require_tools(test, &["ffmpeg"]);

    let scratch = Scratch::new("delete");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv().audio(&["eng", "nld"]).subtitles(vec![
            SubtitleSpec::new("eng", "subrip"),
            SubtitleSpec::new("nld", "subrip"),
        ]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");

    // Delete the second audio track: index 2 among the streams, so the row after the
    // container and video rows plus the first audio.
    let dutch_audio = app
        .app
        .stream_order
        .iter()
        .position(|index| *index == 2)
        .expect("the fixture should have a third stream");
    let row = app
        .app
        .track_rows()
        .iter()
        .position(|track| format!("{track:?}").contains(&format!("Embedded({dutch_audio}")))
        .unwrap_or(dutch_audio + 1);
    app.select_track_row(row);
    app.press(key(KeyCode::Char('d')));
    assert!(
        !app.app.deleted_streams.is_empty(),
        "pressing d should have staged a deletion"
    );

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    assert_eq!(
        after.streams.len(),
        4,
        "one of the five streams should be gone; codecs: {:?}",
        codec_names(&after)
    );
}

/// The save succeeds, and then the app immediately accuses itself: the conflict
/// notice opens over the freshly written file, demanding the user discard the very
/// deletion that was just applied.
///
/// Nothing on disk changed except by the app's own hand. `finish_batch_if_done`
/// clears `active_batch`/`dialog` and *then* rescans, so `reconcile_files` no longer
/// recognises the rescan as its own edit landing; the open file's live edit fields
/// were never cleared on completion, so it re-stages them against the pre-save
/// fingerprint, flags them stale, and the structural re-check correctly reports that
/// the audio track the edit deletes is not there any more.
///
/// The whole flow is post-save background work, which is why every existing scenario
/// misses it: they stop pumping the instant the batch finishes.
#[test]
fn saving_a_deleted_track_should_not_restage_it_against_the_file_it_just_wrote() {
    let test = "saving_a_deleted_track_should_not_restage_it_against_the_file_it_just_wrote";
    require_tools(test, &["ffmpeg"]);

    let scratch = Scratch::new("restage");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv().audio(&["eng", "nld"]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");

    let row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(2))
        .expect("the fixture should have a second audio track");
    app.select_track_row(row);
    app.press(key(KeyCode::Char('d')));
    assert!(
        !app.app.deleted_streams.is_empty(),
        "pressing d should have staged a deletion"
    );

    app.process_all();
    app.assert_batch_succeeded();

    assert!(
        app.app.staged_edits.is_empty(),
        "a completed save must leave nothing staged, got {:?}",
        app.app.staged_edits.keys().collect::<Vec<_>>()
    );
    assert!(
        app.app.deleted_streams.is_empty(),
        "the applied deletion must not survive as a live edit: {:?}",
        app.app.deleted_streams
    );

    // The re-check runs in the background, so the accusation lands a beat after the
    // save reports success.
    app.settle(Duration::from_secs(5));
    assert_eq!(
        app.app.dialog,
        None,
        "the app must not raise a conflict over its own completed save\nscreen:\n{}",
        app.screen()
    );
    assert!(
        app.app.conflicting_paths().is_empty(),
        "no file should be conflicted after a clean save: {:?}",
        app.app.conflicting_paths()
    );
    // Clearing the live edits must leave the view rebuilt from the saved file, not
    // emptied: the container row plus the two surviving tracks.
    assert_eq!(
        app.app.track_rows().len(),
        3,
        "the Streams view should show the saved file's tracks\nscreen:\n{}",
        app.screen()
    );

    let after = probe(&app.path("clip.mkv"));
    assert_eq!(
        after.streams.len(),
        2,
        "the second audio track should be gone; codecs: {:?}",
        codec_names(&after)
    );
}

/// > Failed: test2.mp4 (container: MP4) — ffmpeg could not apply the track edits:
/// > [mp4] Could not find tag for codec subrip in stream #3, codec not currently
/// > supported in container
///
/// Converting a Matroska source carrying `subrip` subtitles to MP4 handed the raw
/// subtitle stream to the MP4 muxer, which cannot store it. The user saw an ffmpeg
/// dump rather than either a working conversion or an actionable message.
///
/// The conflict is now caught before anything is dispatched, so this locks in the
/// pre-flight refusal: it must name the offending track and say what to do about it,
/// and no ffmpeg may run.
#[test]
fn converting_subrip_subtitles_to_mp4_should_be_refused_with_an_actionable_message() {
    let test = "converting_subrip_subtitles_to_mp4_should_be_refused_with_an_actionable_message";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subrip-mp4");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .audio(&["eng"])
            .subtitles(vec![SubtitleSpec::new("eng", "subrip")]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let before = fs::read(app.path("clip.mkv")).unwrap();

    app.choose_container_format("MP4");
    app.press(harness::ctrl('s'));

    let error = app.app.edit_error.clone().unwrap_or_default();
    assert!(
        error.contains("MP4 can't contain") && error.contains("Convert it to"),
        "the refusal should name the conflict and the remedy, got: {error:?}"
    );
    assert!(
        !error.contains("Could not find tag for codec"),
        "the raw MP4 muxer error leaked to the user: {error}"
    );
    assert!(
        app.app.active_batch.is_none(),
        "nothing should have been dispatched"
    );
    assert_eq!(
        fs::read(app.path("clip.mkv")).unwrap(),
        before,
        "a refused conversion must not touch the source"
    );
    assert!(
        !app.path("clip.mp4").exists(),
        "a refused conversion must not leave an output file"
    );
    app.assert_no_temp_leftovers();
}

/// The other half of the same conflict: once the user follows the app's own advice
/// and converts the track to MOV Text, the MKV → MP4 conversion must actually work.
/// This is the sequence the log records as
/// `subtitle_changes: [#3(embedded_target=Some(MovText))]`.
#[test]
fn converting_subrip_to_movtext_should_let_the_mp4_conversion_succeed() {
    let test = "converting_subrip_to_movtext_should_let_the_mp4_conversion_succeed";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac", "ffmpeg:mov_text"]);

    let scratch = Scratch::new("subrip-movtext");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .audio(&["eng"])
            .subtitles(vec![SubtitleSpec::new("eng", "subrip")]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");

    // Container first: the codec choices offered for a subtitle depend on the
    // container it is being written into.
    app.choose_container_format("MP4");
    let subtitle_row = app.first_subtitle_row();
    app.convert_subtitle_to(subtitle_row, "MOV Text");
    app.process_all();

    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let output = app.path("clip.mp4");
    assert!(
        output.exists(),
        "the converted file should exist; directory: {:?}",
        fs::read_dir(app.directory())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect::<Vec<_>>()
    );
    let after = probe(&output);
    assert!(
        !codec_names(&after).contains(&"subrip".to_string()),
        "MP4 cannot hold subrip; codecs: {:?}",
        codec_names(&after)
    );
    assert!(
        !stream_indices_of_type(&after, "subtitle").is_empty(),
        "the subtitle should have survived as mov_text; codecs: {:?}",
        codec_names(&after)
    );
}

/// > Failed: test2.mp4 (container: MP4) — The remuxed track at position 3 (source
/// > track #3) has the wrong default flag: expected false, found true.
/// > Staged defaults: {0, 1}.
///
/// The MP4 muxer sets the default disposition on a lone subtitle track whether or not
/// it was asked to, so post-write validation rejected its own correct output. This
/// recurred five times in the log across two files, and the error message visibly
/// grew more detailed between attempts — production code was instrumented because the
/// failure could not be reproduced in a test.
#[test]
fn mp4_forcing_a_default_flag_onto_a_lone_subtitle_should_not_fail_validation() {
    let test = "mp4_forcing_a_default_flag_onto_a_lone_subtitle_should_not_fail_validation";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac", "ffmpeg:mov_text"]);

    let scratch = Scratch::new("lone-default");
    // One subtitle, explicitly *not* default — the exact shape that made the MP4
    // muxer's forced flag disagree with the staged defaults {video, audio}.
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .audio(&["eng"])
            .subtitles(vec![SubtitleSpec::new("eng", "subrip")]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");

    // Follow the app's own remedy, exactly as the log shows the user did, so the run
    // reaches the muxer instead of stopping at the codec-conflict guard.
    app.choose_container_format("MP4");
    let subtitle_row = app.first_subtitle_row();
    app.convert_subtitle_to(subtitle_row, "MOV Text");
    app.process_all();

    let notice = app.app.notice.clone().unwrap_or_default();
    assert!(
        !notice.contains("wrong default flag"),
        "validation rejected ffmpeg's own output: {notice}"
    );
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    // The staged defaults were {video, audio}; MP4 forcing the lone subtitle default
    // is the muxer's business, but the tracks the user did stage must be right.
    let after = probe(&app.path("clip.mp4"));
    let defaults = harness::default_flags(&after);
    assert_eq!(
        defaults.first(),
        Some(&true),
        "the video track should still be default; flags: {defaults:?}"
    );
}

/// > Failed: test.mkv — ffmpeg could not apply the track edits: [libx265] Image size
/// > is too small (1920x10).
///
/// The degenerate value itself is guarded in `App` and unit-tested at
/// `src/app.rs:9106` ("A real report: 1920x10 …"), which is the right place for pure
/// validation. What no unit test can check is the other half: that a *legal* custom
/// downscale survives the whole trip — dropdown, draft, `EditRequest`, scale filter,
/// encoder — and lands with the dimensions the user asked for. A wrong filter here
/// produces a plausible-looking file at the wrong size rather than a loud failure.
#[test]
fn a_custom_downscale_should_reach_the_encoder_with_the_requested_dimensions() {
    let test = "a_custom_downscale_should_reach_the_encoder_with_the_requested_dimensions";
    require_tools(test, &["ffmpeg:libx264"]);

    let scratch = Scratch::new("downscale");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv().size(320, 240).audio(&["eng"]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    app.set_custom_resolution("160", "120");

    assert!(
        !app.app.video_settings.is_empty(),
        "a custom resolution should have been staged; notice: {:?}",
        app.app.notice
    );

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    let video = after
        .streams
        .iter()
        .find(|stream| {
            stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("video")
        })
        .expect("the output should still have a video stream");
    assert_eq!(
        (
            video.get("width").and_then(serde_json::Value::as_u64),
            video.get("height").and_then(serde_json::Value::as_u64)
        ),
        (Some(160), Some(120)),
        "the encode should honour the requested custom resolution"
    );
}

/// > Failed: test2.mp4 (container: MP4) — The remuxed track at position 3 (source
/// > track #3) has the wrong default flag: expected false, found true.
/// > Staged defaults: {0, 1}.
///
/// Audio editing crosses the same fragile app → staged edit → worker → ffmpeg seam
/// that produced the recorded default-disposition failure. This drives every
/// meaningful visible Matroska audio field through the popup, including moving the
/// default flag between neighboring audio tracks, and then verifies the real encoded
/// file. Bitrate and sample rate are chosen for the user rather than offered, so this
/// also pins that choice: an AC-3 bitrate for the layout, and an automatic downsample of
/// the 96 kHz source AC-3 cannot carry. The edited track starts as Original, reproducing
/// the save-time conflict that appeared only after another supported Matroska role was
/// selected.
#[test]
fn editing_an_audio_track_should_encode_every_staged_field_and_preserve_its_neighbor() {
    let test = "editing_an_audio_track_should_encode_every_staged_field_and_preserve_its_neighbor";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac", "ffmpeg:ac3"]);

    let scratch = Scratch::new("audio-settings");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .audio(&["eng", "fra"])
            .video_disposition("original")
            .audio_dispositions(&["default", "original"])
            .audio_sample_rate(96_000),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    app.pump();
    assert_eq!(
        app.screen().matches("[OG]").count(),
        1,
        "Original should be visible on the audio overview row:\n{}",
        app.screen()
    );
    let video_row = app
        .screen()
        .lines()
        .find(|line| line.contains("H264"))
        .expect("the video overview row should be on screen")
        .to_string();
    assert!(
        !video_row.contains("OG"),
        "the source's `original` flag describes its language, not its picture, so it \
         should not reach the video row: {video_row}"
    );
    let edited_row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(2))
        .expect("the second audio track should have a row");
    app.open_audio_settings(edited_row);

    // K opens contextual help for the highlighted audio field and the panel follows
    // navigation, just as it does in the other metadata editors.
    app.press(key(KeyCode::Char('K')));
    app.pump();
    assert!(app.app.audio_settings_popup.as_ref().unwrap().help_visible);
    assert!(
        app.screen().contains("Information about Codec")
            && app.screen().contains("audio format written to the output"),
        "Codec help should be rendered beside the audio editor:\n{}",
        app.screen()
    );
    app.press(key(KeyCode::Char('j')));
    app.pump();
    assert!(
        app.screen().contains("Information about Channel layout")
            // The phrase wraps across rows at this popup width, so the check uses a
            // substring short enough to land on one wrapped line.
            && app.screen().contains("possible yet"),
        "help should follow the highlighted audio field:\n{}",
        app.screen()
    );

    app.choose_audio_setting(AudioSettingsField::Codec, "Dolby Digital (AC-3)");
    app.choose_audio_setting(AudioSettingsField::ChannelLayout, "Mono");
    app.choose_audio_language("dutch", "nld");
    app.type_audio_title("Director commentary");
    for field in [
        AudioSettingsField::Default,
        AudioSettingsField::Commentary,
        AudioSettingsField::HearingImpaired,
        AudioSettingsField::AudioDescription,
    ] {
        app.toggle_audio_field(field);
    }

    let staged = app
        .app
        .effective_audio_settings(2)
        .expect("the popup should have staged audio settings");
    assert!(staged.metadata.original && !staged.metadata.dubbed);
    assert!(
        app.app.selected_container_conflicts().is_empty(),
        "supported Matroska roles should not conflict: {:?}",
        app.app.selected_container_conflicts()
    );
    assert_eq!(
        app.app.default_streams.iter().copied().collect::<Vec<_>>(),
        vec![2],
        "making audio #2 default should clear that flag from audio #1"
    );

    app.close_audio_settings();
    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    assert_eq!(
        codec_names(&after),
        ["h264", "aac", "ac3"],
        "only the selected audio track should have been transcoded"
    );
    let untouched = &after.streams[1];
    let edited = &after.streams[2];

    assert_eq!(stream_number(untouched, "channels"), Some(2));
    assert_eq!(stream_number(untouched, "sample_rate"), Some(96_000));
    assert_eq!(stream_tag(untouched, "language"), Some("eng"));
    assert_eq!(stream_tag(untouched, "title"), None);
    assert!(!stream_disposition(untouched, "default"));
    for role in ["comment", "hearing_impaired", "visual_impaired"] {
        assert!(
            !stream_disposition(untouched, role),
            "the neighboring audio track unexpectedly gained {role}"
        );
    }

    assert_eq!(stream_number(edited, "channels"), Some(1));
    assert_eq!(stream_number(edited, "sample_rate"), Some(48_000));
    assert_eq!(stream_number(edited, "bit_rate"), Some(128_000));
    assert_eq!(stream_tag(edited, "language"), Some("nld"));
    assert_eq!(stream_tag(edited, "title"), Some("Director commentary"));
    for role in ["default", "comment", "hearing_impaired", "visual_impaired"] {
        assert!(
            stream_disposition(edited, role),
            "the edited audio track is missing {role}: {edited:?}"
        );
    }
    assert!(stream_disposition(edited, "original"));
    assert!(!stream_disposition(edited, "dub"));
}

#[test]
/// Video tracks get the same language/title/default/commentary metadata editing as audio
/// and subtitle tracks, without forcing a re-encode and without disturbing an unrelated
/// neighboring track. The source starts flagged `original`, the language role mkvmerge
/// stamps onto a picture track: Reel neither shows it nor offers it, and a metadata edit
/// must leave it exactly as it found it rather than dropping it on the way through.
fn editing_a_video_tracks_metadata_should_persist_and_preserve_its_neighbor() {
    let test = "editing_a_video_tracks_metadata_should_persist_and_preserve_its_neighbor";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("video-metadata");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .audio(&["eng"])
            .video_disposition("original")
            .audio_dispositions(&["default"]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let video_row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(0))
        .expect("the video track should have a row");
    app.open_video_settings(video_row);

    // K opens contextual help for the highlighted video field, matching every other
    // metadata editor.
    app.press(key(KeyCode::Char('K')));
    app.pump();
    assert!(app.app.video_settings_popup.as_ref().unwrap().help_visible);
    assert!(
        app.screen().contains("Information about Codec")
            && app.screen().contains("video format written to the output"),
        "Codec help should be rendered beside the video editor:\n{}",
        app.screen()
    );

    app.choose_video_language("dutch", "nld");
    app.type_video_title("Director's cut");
    app.toggle_video_field(VideoSettingsField::Default);
    app.toggle_video_field(VideoSettingsField::Commentary);

    assert!(
        app.app.selected_container_conflicts().is_empty(),
        "a supported Matroska video language should not conflict: {:?}",
        app.app.selected_container_conflicts()
    );

    app.close_video_settings();

    // The staged commentary flag reaches the overview row before it reaches the file.
    let staged_video_row = app
        .screen()
        .lines()
        .find(|line| line.contains("H264"))
        .expect("the video overview row should be on screen")
        .to_string();
    assert!(
        staged_video_row.contains("CM"),
        "a staged commentary flag should show on the video row: {staged_video_row}"
    );

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    assert_eq!(
        codec_names(&after),
        ["h264", "aac"],
        "metadata-only edits must not force a re-encode of either track"
    );
    let video = &after.streams[0];
    let audio = &after.streams[1];

    assert_eq!(stream_tag(video, "language"), Some("nld"));
    assert_eq!(stream_tag(video, "title"), Some("Director's cut"));
    assert!(stream_disposition(video, "default"));
    assert!(stream_disposition(video, "comment"));
    // Untouched because Reel never offers it on a picture track: a flag it does not show
    // is a flag it must not silently drop either.
    assert!(stream_disposition(video, "original"));

    // The neighboring audio track's own metadata and default flag are untouched: video
    // and audio defaults are tracked independently, and the commentary flag went only
    // where it was staged.
    assert_eq!(stream_tag(audio, "language"), Some("eng"));
    assert!(stream_disposition(audio, "default"));
    assert!(!stream_disposition(audio, "comment"));
}

/// Sideways phone footage is the commonest defect a video track has, and the fix is a
/// tag rather than a re-encode: the picture is untouched, the codec is untouched, and the
/// save takes a remux rather than an encode. The source already carries a rotation, so
/// this also covers replacing one angle with another and clearing one back to upright —
/// the case that needs an explicit `-display_rotation 0` rather than a missing argument.
#[test]
fn rotating_a_video_track_should_tag_the_picture_without_re_encoding_it() {
    let test = "rotating_a_video_track_should_tag_the_picture_without_re_encoding_it";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("video-rotation");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv().size(64, 48).video_rotation(90),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    app.pump();
    assert!(
        app.screen().contains("↻90°"),
        "the source's own rotation should be on the overview row:\n{}",
        app.screen()
    );

    let video_row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(0))
        .expect("the video track should have a row");
    app.open_video_settings(video_row);
    app.choose_video_rotation(VideoRotation::Cw180);
    app.close_video_settings();
    assert!(
        app.screen().contains("↻180°"),
        "the staged angle should replace the source's on the row:\n{}",
        app.screen()
    );

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    assert_eq!(
        codec_names(&after),
        ["h264", "aac"],
        "a rotation is metadata: neither track may be re-encoded"
    );
    let video = &after.streams[0];
    assert_eq!(stream_rotation_degrees(video), Some(-180));
    // The picture itself is untouched — an encode would have transposed these.
    assert_eq!(stream_number(video, "width"), Some(64));
    assert_eq!(stream_number(video, "height"), Some(48));

    // And clearing it removes the matrix rather than leaving the old angle behind. The
    // save re-probes in the background, so the view catches up a beat later.
    app.settle(Duration::from_secs(2));
    assert!(
        app.screen().contains("↻180°"),
        "the saved rotation should be read back onto the row:\n{}",
        app.screen()
    );
    app.open_video_settings(video_row);
    app.choose_video_rotation(VideoRotation::None);
    app.close_video_settings();
    app.process_all();
    app.assert_batch_succeeded();

    let cleared = probe(&app.path("clip.mkv"));
    assert_eq!(stream_rotation_degrees(&cleared.streams[0]), None);
    assert!(
        !app.screen().contains("↻"),
        "a cleared rotation should leave no badge behind:\n{}",
        app.screen()
    );
}

/// Rotation and a re-encode in one save: ffmpeg applies the angle to the picture instead
/// of tagging it, so the output must come out upright with its dimensions swapped and no
/// matrix left over. Locking this in matters because reel validates the encoded size, and
/// an unrotated expectation would reject the file ffmpeg wrote exactly as asked.
#[test]
fn rotating_while_re_encoding_should_bake_the_angle_into_the_picture() {
    let test = "rotating_while_re_encoding_should_bake_the_angle_into_the_picture";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac", "ffmpeg:libx265"]);

    let scratch = Scratch::new("video-rotation-encode");
    // Larger than the usual fixture on purpose: x265 aborts ("double free or corruption")
    // when it is handed a transposed frame as small as the default 64×48, with or without
    // a rotation involved — a plain `transpose` filter crashes it just the same.
    write_media(&scratch.join("clip.mkv"), &MediaSpec::mkv().size(320, 240));

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let video_row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(0))
        .expect("the video track should have a row");
    app.open_video_settings(video_row);
    app.choose_video_rotation(VideoRotation::Cw90);
    app.close_video_settings();
    app.choose_video_codec(video_row, "HEVC / H.265");

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let after = probe(&app.path("clip.mkv"));
    let video = &after.streams[0];
    assert_eq!(codec_names(&after), ["hevc", "aac"]);
    // Baked in: the frame is transposed and there is no matrix left to carry.
    assert_eq!(stream_number(video, "width"), Some(240));
    assert_eq!(stream_number(video, "height"), Some(320));
    assert_eq!(stream_rotation_degrees(video), None);
}

/// > [1786639801] Failed: /home/bas/Downloads/reel/test.mkv (destination:
/// > ReplaceOriginal, container: MP4, stream_order: [0, 1, 2, 3], deleted_streams:
/// > {4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
/// > 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38}, default_streams:
/// > {0, 1}, audio_settings: 2, video_settings: 0, subtitle_changes:
/// > [#3(embedded_target=Some(MovText), export_target=None, import=false,
/// > metadata=false)]) — The audio track at position 1 has the wrong metadata.
///
/// MP4 cannot store Matroska's Original audio role, but conversion should normalize
/// that metadata automatically instead of making the user clear a field the target
/// container cannot expose. Drive the same mixed copy/transcode workflow through the
/// TUI and prove supported metadata on both audio tracks survives the real round trip.
#[test]
fn converting_audio_metadata_to_mp4_should_drop_unsupported_roles_and_preserve_its_neighbor() {
    let test =
        "converting_audio_metadata_to_mp4_should_drop_unsupported_roles_and_preserve_its_neighbor";
    require_tools(
        test,
        &["ffmpeg:libx264", "ffmpeg:flac", "ffmpeg:aac", "ffmpeg:alac"],
    );

    let scratch = Scratch::new("audio-metadata-mp4");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .audio(&["eng", "nld"])
            .audio_codecs(&["flac", "aac"])
            .audio_titles(&[Some("Main mix"), Some("Director commentary")])
            .audio_dispositions(&["default+original+comment", "comment"]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    app.choose_container_format("MP4");
    let first_audio_row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(1))
        .expect("the FLAC audio track should have a row");
    app.open_audio_settings(first_audio_row);
    assert!(
        !app.app
            .visible_audio_fields()
            .contains(&AudioSettingsField::Original),
        "MP4's unsupported Original role should not be editable"
    );
    // The staged edit keeps the source's Original role even though MP4 cannot store it:
    // the container target is not a decision the user made about this flag, and it has to
    // come back intact if they pick MKV again. The drop happens when the file is written,
    // which the probe below checks.
    let effective = app
        .app
        .effective_audio_settings(1)
        .expect("the selected audio track should have effective settings");
    assert!(effective.metadata.original);
    assert!(effective.metadata.commentary);
    app.choose_audio_setting(AudioSettingsField::Codec, "ALAC");
    app.close_audio_settings();

    app.process_all();
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let output = app.path("clip.mp4");
    assert!(output.exists(), "the converted MP4 should exist");
    let after = probe(&output);
    assert_eq!(codec_names(&after), ["h264", "alac", "aac"]);
    let converted = &after.streams[1];
    let neighbor = &after.streams[2];
    assert_eq!(stream_tag(converted, "language"), Some("eng"));
    assert_eq!(stream_tag(converted, "handler_name"), Some("Main mix"));
    assert!(stream_disposition(converted, "comment"));
    assert!(!stream_disposition(converted, "original"));
    assert_eq!(stream_tag(neighbor, "language"), Some("nld"));
    assert_eq!(
        stream_tag(neighbor, "handler_name"),
        Some("Director commentary")
    );
    assert!(stream_disposition(neighbor, "comment"));
    assert!(!stream_disposition(neighbor, "original"));
}

/// A `tmcd` timecode track is what a camera writes into an MP4, and ffmpeg genuinely
/// refuses to write one back out ("codec not currently supported in container"). Reel
/// shows no row for it, so it can be neither dropped nor converted — which makes the
/// up-front refusal the only useful outcome. Without it the batch starts, runs, and dies
/// on a raw muxer error with the work already half done.
#[test]
fn a_stream_reel_cannot_edit_should_be_refused_before_the_batch_starts() {
    let test = "a_stream_reel_cannot_edit_should_be_refused_before_the_batch_starts";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("unwritable-stream");
    write_media(
        &scratch.join("clip.mp4"),
        &MediaSpec::mp4()
            .audio(&["eng", "nld"])
            .timecode("00:00:00:00"),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mp4");
    let before = fs::read(app.path("clip.mp4")).unwrap();
    let probed = probe(&app.path("clip.mp4"));
    assert!(
        probed
            .streams
            .iter()
            .any(|stream| stream.get("codec_type").and_then(|kind| kind.as_str()) == Some("data")),
        "the fixture should carry the timecode track this scenario is about"
    );

    // An edit that says nothing about the data track: move the default audio flag.
    let second_audio = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Embedded(2))
        .expect("the second audio track should have a row");
    app.open_audio_settings(second_audio);
    app.toggle_audio_field(AudioSettingsField::Default);
    app.close_audio_settings();
    app.press(harness::ctrl('s'));

    let error = app.app.edit_error.clone().unwrap_or_default();
    assert!(
        error.contains("data track") && error.contains("MP4 can't contain"),
        "the refusal should name the unwritable track, got: {error:?}"
    );
    assert!(
        !error.contains("codec not currently supported"),
        "the raw muxer error leaked to the user: {error}"
    );
    assert!(
        app.app.active_batch.is_none(),
        "nothing should have been dispatched"
    );
    assert_eq!(
        fs::read(app.path("clip.mp4")).unwrap(),
        before,
        "a refused edit must not touch the source"
    );
    app.assert_no_temp_leftovers();
}

/// > Failed: test.mp4 (container: MKV) — Could not extract subtitle for conversion:
/// > [matroska] Subtitle codec mov_text (94213) is not supported.
///
/// Converting MP4 → MKV extracts each subtitle to an intermediate before converting
/// it, and the intermediate was written as Matroska — which cannot hold `mov_text`,
/// the very codec an MP4 source carries. The conversion failed on exactly the input
/// it exists to handle.
#[test]
fn converting_an_mp4_with_movtext_subtitles_to_mkv_should_succeed() {
    let test = "converting_an_mp4_with_movtext_subtitles_to_mkv_should_succeed";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac", "ffmpeg:mov_text"]);

    let scratch = Scratch::new("movtext-mkv");
    write_media(
        &scratch.join("clip.mp4"),
        &MediaSpec::mp4()
            .audio(&["eng"])
            .subtitles(vec![SubtitleSpec::new("eng", "mov_text")]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mp4");
    app.choose_container_format("MKV");
    // Matroska cannot hold mov_text, so the app requires a conversion — which is the
    // path that has to extract the mov_text track first, and that extract is what
    // used to be written as Matroska and fail.
    let subtitle_row = app.first_subtitle_row();
    app.convert_subtitle_to(subtitle_row, "SubRip / SRT");
    app.process_all();

    let notice = app.app.notice.clone().unwrap_or_default();
    assert!(
        !notice.contains("Could not extract subtitle"),
        "the mov_text extract path failed: {notice}"
    );
    app.assert_batch_succeeded();
    app.assert_no_temp_leftovers();

    let output = app.path("clip.mkv");
    assert!(output.exists(), "the converted MKV should exist");
    let after = probe(&output);
    assert!(
        !stream_indices_of_type(&after, "subtitle").is_empty(),
        "the subtitle should have survived the conversion; codecs: {:?}, languages: {:?}",
        codec_names(&after),
        languages(&after)
    );
}

/// > Failed: test.mp4 (container: MKV) — /home/bas/Downloads/reel/test.mkv already
/// > exists; choose Create a copy or rename it.
///
/// Refusing is correct — the point of the regression is that it refuses *safely*: the
/// pre-existing file must be left byte-identical and no half-written scratch file may
/// survive. A collision guard that clobbers the other file is far worse than one that
/// never fires.
#[test]
fn a_conversion_onto_an_existing_file_should_refuse_without_touching_it() {
    let test = "a_conversion_onto_an_existing_file_should_refuse_without_touching_it";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("collision");
    write_media(&scratch.join("clip.mp4"), &MediaSpec::mp4().audio(&["eng"]));
    // A different file that the conversion would land on top of.
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv().audio(&["eng", "nld"]),
    );
    let bystander = scratch.join("clip.mkv");
    let before = fs::read(&bystander).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mp4");
    app.choose_container_format("MKV");
    app.press(harness::ctrl('s'));

    // The guard may fire before dispatch (an error dialog) or as a failed batch
    // outcome; either is acceptable, silently overwriting is not.
    if app.app.dialog == Some(reel_tui::app::Dialog::ConfirmProcessAll) {
        app.press(key(KeyCode::Enter));
        app.wait_until("the batch to finish", |app| app.active_batch.is_none());
    }

    assert_eq!(
        fs::read(&bystander).unwrap(),
        before,
        "the pre-existing clip.mkv was modified by a refused conversion"
    );
    assert!(
        app.path("clip.mp4").exists(),
        "the source should still be there after a refused conversion"
    );
    app.assert_no_temp_leftovers();
}

fn stream_number(stream: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<u64> {
    stream.get(key).and_then(|value| match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(number) => number.parse().ok(),
        _ => None,
    })
}

/// The raw angle in a stream's display matrix, read straight from the probe rather than
/// through `VideoRotation`, so a scenario asserts what ffmpeg actually wrote — including
/// its sign convention — rather than reel's normalized view of it.
fn stream_rotation_degrees(stream: &BTreeMap<String, serde_json::Value>) -> Option<i64> {
    stream
        .get("side_data_list")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|entry| entry.get("rotation").and_then(serde_json::Value::as_i64))
}

fn stream_tag<'a>(stream: &'a BTreeMap<String, serde_json::Value>, key: &str) -> Option<&'a str> {
    stream
        .get("tags")
        .and_then(|tags| tags.get(key))
        .and_then(serde_json::Value::as_str)
}

fn stream_disposition(stream: &BTreeMap<String, serde_json::Value>, key: &str) -> bool {
    stream
        .get("disposition")
        .and_then(|disposition| disposition.get(key))
        .and_then(serde_json::Value::as_i64)
        == Some(1)
}

fn stream_languages_of_type(info: &reel_tui::probe::MediaInfo, kind: &str) -> Vec<String> {
    info.streams
        .iter()
        .filter(|stream| stream.get("codec_type").and_then(serde_json::Value::as_str) == Some(kind))
        .map(|stream| stream_tag(stream, "language").unwrap_or("und").to_string())
        .collect()
}

/// Two cues whose text is easy to tell apart, and which a rewrite can change one of
/// without touching the other.
const CACHED_CUES: &str = "1\n00:00:01,000 --> 00:00:02,000\nFirst line\n\n\
                           2\n00:00:03,000 --> 00:00:04,000\nSecond line\n\n";

const RETYPED_CUES: &str = "1\n00:00:01,000 --> 00:00:02,000\nFirst line\n\n\
                            2\n00:00:03,000 --> 00:00:04,000\nSecond line, rewritten\n\n";

/// Opening the subtitle edit page renders every cue's frame in the background and keeps it on
/// disk, so a second visit costs a decode rather than an `ffmpeg` seek — and a cue whose
/// text changed is rendered again, because the cache is keyed on what the frame shows.
///
/// The cache is proven by planting a frame the application could not have produced: a
/// solid magenta picture where one cue's frame belongs. A page that draws magenta is a
/// page that read the cache. The same trick then shows that rewriting *that* cue's line
/// stops it being drawn, while the cue left alone still is.
#[test]
fn the_subtitle_edit_page_should_cache_and_prefetch_preview_frames() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "the_subtitle_edit_page_should_cache_and_prefetch_preview_frames";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-frame-cache");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(6.0)
            .audio(&["eng"]),
    );
    let sidecar = scratch.join("clip.eng.srt");
    fs::write(&sidecar, CACHED_CUES).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);

    // The background pass: every cue rendered without the cursor going near it, counting
    // up on the cue panel's border while it works.
    let counted = wait_for_frames(&mut app);
    assert!(
        counted,
        "the page should count the frames it is generating while the pass runs"
    );
    let state = app.app.subtitle_edit.as_ref().unwrap();
    let cues = state.cues.clone();
    assert_eq!(cues.len(), 2, "the sidecar holds two cues");
    let keys: Vec<PathBuf> = (0..cues.len())
        .map(|index| cached_frame(state, index))
        .collect();
    for (cue, path) in cues.iter().zip(&keys) {
        assert!(
            path.is_file(),
            "the background pass should have cached a frame for {:?} at {}",
            cue.text,
            path.display()
        );
    }
    // And the count goes away once there is nothing left to report.
    let screen = app.screen();
    assert!(
        !warm_count_pattern(&screen),
        "a finished pass should stop reporting:\n{screen}"
    );

    // Plant a frame the application could not have rendered, then come back to it.
    app.press(key(KeyCode::Esc));
    app.pump();
    write_solid_frame(&keys[1], "magenta", 320, 240);
    open_sidecar_edit_page(&mut app);
    app.press(key(KeyCode::Char('j')));
    app.wait_until("the second cue's frame", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.selected == 1 && state.frame().is_some())
    });
    let shades = app.preview_shades();
    assert!(
        shades
            .iter()
            .any(|(red, green, blue)| *red > 200 && *green < 80 && *blue > 200),
        "the planted frame should be drawn from the cache rather than rendered again; \
         shades: {shades:?}\nscreen:\n{}",
        app.screen()
    );

    // Rewriting that cue's line changes what its frame would show, so its key changes
    // with it: the planted frame is no longer what the page draws, while the cue that was
    // left alone keeps the frame it already had.
    app.press(key(KeyCode::Esc));
    app.pump();
    fs::write(&sidecar, RETYPED_CUES).unwrap();
    open_sidecar_edit_page(&mut app);
    wait_for_frames(&mut app);

    let state = app.app.subtitle_edit.as_ref().unwrap();
    let retyped = state.cues.clone();
    assert_eq!(
        retyped[1].text, "Second line, rewritten",
        "the page should re-read the sidecar it was opened on"
    );
    assert_eq!(
        cached_frame(state, 0),
        keys[0],
        "the cue that did not change should reuse the frame already rendered for it"
    );
    let rewritten = cached_frame(state, 1);
    assert_ne!(
        rewritten, keys[1],
        "a rewritten cue should not be served the frame of the line it replaced"
    );
    assert!(
        rewritten.is_file(),
        "the rewritten cue should have been rendered again at {}",
        rewritten.display()
    );

    app.press(key(KeyCode::Char('j')));
    app.wait_until("the rewritten cue's frame", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.selected == 1 && state.frame().is_some())
    });
    let shades = app.preview_shades();
    assert!(
        !shades
            .iter()
            .any(|(red, green, blue)| *red > 200 && *green < 80 && *blue > 200),
        "the rewritten cue should be drawn from the video, not from the old line's frame; \
         shades: {shades:?}\nscreen:\n{}",
        app.screen()
    );
}

/// Re-opening a track the background pass has already rendered must not render any of it
/// again — that is the whole point of the cache, and it was silently not happening.
///
/// Frames were stored as PNG at around two megabytes each, so the default 512 MB cache held
/// roughly 240 of them: fewer than a feature-length subtitle track has cues. Opening such a
/// track pruned the cache, rendered the track, and evicted its own earliest frames on the
/// way past, so the next opening found the start of the track missing and rendered it all
/// over again — every time, forever.
///
/// Asserted on mtimes rather than on the files existing: a re-rendered frame is written
/// again under the same content-addressed name, so existence alone would pass against
/// exactly the bug this is here for.
#[test]
fn re_opening_a_rendered_track_should_not_render_any_of_it_again() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "re_opening_a_rendered_track_should_not_render_any_of_it_again";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-frame-reopen");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(6.0)
            .audio(&["eng"]),
    );
    fs::write(scratch.join("clip.eng.srt"), WALKED_CUES).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);
    wait_for_frames(&mut app);

    let state = app.app.subtitle_edit.as_ref().unwrap();
    let paths: Vec<PathBuf> = (0..state.cues.len())
        .map(|index| cached_frame(state, index))
        .collect();
    assert_eq!(paths.len(), 4, "the sidecar holds four cues");
    let rendered: Vec<SystemTime> = paths
        .iter()
        .map(|path| {
            fs::metadata(path)
                .unwrap_or_else(|error| {
                    panic!("the pass should have cached {}: {error}", path.display())
                })
                .modified()
                .expect("the platform reports mtimes")
        })
        .collect();

    // The filesystem's mtime granularity can be coarse enough that a rewrite within the
    // same tick is indistinguishable from no rewrite at all.
    std::thread::sleep(Duration::from_millis(1100));

    // Leave the page entirely and come back to the same track.
    app.press(key(KeyCode::Esc));
    app.pump();
    assert!(app.app.subtitle_edit.is_none(), "Esc should close the page");
    open_sidecar_edit_page(&mut app);
    wait_for_frames(&mut app);

    // Every frame is the one already on disk, untouched.
    for (path, was) in paths.iter().zip(&rendered) {
        let now = fs::metadata(path)
            .unwrap_or_else(|error| panic!("{} should still be cached: {error}", path.display()))
            .modified()
            .expect("the platform reports mtimes");
        assert_eq!(
            now,
            *was,
            "re-opening the track re-rendered {} rather than reading it back",
            path.display()
        );
    }

    // And the second visit really did go through the cache rather than skipping the pass:
    // the page reports it finished, and the frames are still the ones it started with.
    assert_eq!(
        app.app.subtitle_edit.as_ref().unwrap().warm,
        WarmState::Done,
        "the pass should run and find everything already there"
    );
}

/// A build that cannot burn subtitles in says so in the preview pane, for as long as the
/// page is open.
///
/// Without this the pane is an unexplained empty box: the page opens, the cues load, the
/// timeline draws, and the largest thing on screen stays blank forever with nothing to
/// say why. The reason is drawn *in* the pane rather than on the status row precisely
/// because it cannot change while the page is open — a message that moves with the cursor
/// is the flicker the pane had its text fallback removed to stop.
#[test]
fn a_page_that_can_never_draw_a_frame_should_say_why_in_the_pane() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "a_page_that_can_never_draw_a_frame_should_say_why_in_the_pane";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-no-burn");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(6.0)
            .audio(&["eng"]),
    );
    fs::write(scratch.join("clip.eng.srt"), SHORT_CUES).unwrap();

    let mut app = Harness::start(scratch);
    // An FFmpeg without the `subtitles` filter, which is what a build without libass is.
    app.app.subtitle_capabilities = ToolCapabilities {
        ffmpeg_filters: BTreeSet::from(["scale".to_string()]),
        ..app.app.subtitle_capabilities.clone()
    };
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);

    // The page is fully usable — cues, timeline, navigation — it simply cannot draw.
    let screen = app.screen();
    assert!(
        screen.contains("Preview is not possible"),
        "the pane should say why it will stay empty:\n{screen}"
    );
    assert!(
        screen.contains("libass"),
        "the reason should name what is missing:\n{screen}"
    );

    // And it stays said, rather than being a message that flashes past: pumping the loop
    // asks for no frame at all, since asking could only produce the same failure again.
    for _ in 0..5 {
        app.pump();
    }
    assert!(
        app.screen().contains("Preview is not possible"),
        "the reason should hold for as long as the page is open"
    );
    assert!(
        app.preview_shades().is_empty(),
        "nothing should have been drawn into the pane"
    );

    // The cue list still works, which is the point of saying this rather than refusing to
    // open the page.
    app.press(key(KeyCode::Char('j')));
    app.pump();
    assert_eq!(
        app.app.subtitle_edit.as_ref().unwrap().selected,
        1,
        "the page should still navigate"
    );
}

/// Nine cues, three per background worker, each with text of its own.
const PARALLEL_CUES: &str = "1\n00:00:00,200 --> 00:00:00,600\nParallel alpha\n\n\
                             2\n00:00:00,800 --> 00:00:01,200\nParallel bravo\n\n\
                             3\n00:00:01,400 --> 00:00:01,800\nParallel charlie\n\n\
                             4\n00:00:02,000 --> 00:00:02,400\nParallel delta\n\n\
                             5\n00:00:02,600 --> 00:00:03,000\nParallel echo\n\n\
                             6\n00:00:03,200 --> 00:00:03,600\nParallel foxtrot\n\n\
                             7\n00:00:03,800 --> 00:00:04,200\nParallel golf\n\n\
                             8\n00:00:04,400 --> 00:00:04,800\nParallel hotel\n\n\
                             9\n00:00:05,000 --> 00:00:05,400\nParallel india\n\n";

/// The background pass renders a track with three workers, and each burns in its own cue.
///
/// The workers share one scratch directory, so each has to stage its cue under a name of
/// its own — two sharing one would overwrite each other between the write and the burn,
/// and a frame would come back carrying a *different cue's* line, stored under the right
/// key in the right directory and counted correctly. Nothing about keys, counts or files
/// could tell.
///
/// What tells is that the fixture is a solid black clip: the burned-in line is the only
/// thing that can make one frame differ from another, so nine cues with nine different
/// lines have to produce nine different files. A collision makes two of them identical.
#[test]
fn the_background_pass_should_render_every_cue_with_its_own_line_burned_in() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "the_background_pass_should_render_every_cue_with_its_own_line_burned_in";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-frame-parallel");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(6.0)
            .audio(&["eng"]),
    );
    fs::write(scratch.join("clip.eng.srt"), PARALLEL_CUES).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);
    wait_for_frames(&mut app);

    // Every cue rendered, by whichever worker's slice it fell in.
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert_eq!(state.cues.len(), 9, "the sidecar holds nine cues");
    let frames: Vec<Vec<u8>> = state
        .cues
        .iter()
        .enumerate()
        .map(|(index, cue)| {
            let path = cached_frame(state, index);
            fs::read(&path).unwrap_or_else(|error| {
                panic!("the pass should have cached {:?}: {error}", cue.text)
            })
        })
        .collect();
    for (cue, frame) in state.cues.iter().zip(&frames) {
        assert!(!frame.is_empty(), "{:?} cached an empty frame", cue.text);
    }

    // And no two of them are the same picture. On a solid black clip that can only mean
    // two cues were burned with the same line, which is what a shared staging file does.
    let distinct: BTreeSet<&Vec<u8>> = frames.iter().collect();
    assert_eq!(
        distinct.len(),
        frames.len(),
        "two cues cached the same picture, so a worker burned in another's line"
    );
}

/// Two cues, for the shorter of the two tracks the eviction scenario uses.
const SHORT_CUES: &str = "1\n00:00:01,000 --> 00:00:02,000\nShort first\n\n\
                          2\n00:00:03,000 --> 00:00:04,000\nShort second\n\n";

/// A cache with room for fewer tracks than exist evicts whole tracks, least recently used
/// first, and never the one that is open.
///
/// Two properties in one scenario, because they are the same design decision. **Whole
/// tracks**: a track missing some of its frames is re-rendered on every visit, so evicting
/// a fraction of one buys disk at the cost of the work the cache exists to avoid — a
/// surviving track has to be complete, and an evicted one has to be gone entirely. **Never
/// the open one**: the pass is about to render into it, so evicting it guarantees the
/// re-render, and it is the least recently used track here precisely to prove the
/// exclusion is doing the work rather than the ranking happening to agree.
#[test]
fn a_full_cache_should_evict_whole_tracks_and_never_the_open_one() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "a_full_cache_should_evict_whole_tracks_and_never_the_open_one";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-frame-eviction");
    for (name, cues) in [("short", SHORT_CUES), ("long", WALKED_CUES)] {
        write_media(
            &scratch.join(&format!("{name}.mkv")),
            &MediaSpec::mkv()
                .size(320, 240)
                .duration(6.0)
                .audio(&["eng"]),
        );
        fs::write(scratch.join(&format!("{name}.eng.srt")), cues).unwrap();
    }

    let mut app = Harness::start(scratch);

    // The short track first, so it is the *least* recently used of the two.
    let short = render_sidecar_track(&mut app, "short.mkv");
    assert_eq!(short.len(), 2, "the short sidecar holds two cues");
    let rendered: Vec<SystemTime> = short.iter().map(modified).collect();

    // Then the long one, which is larger and more recently used.
    let long = render_sidecar_track(&mut app, "long.mkv");
    assert_eq!(long.len(), 4, "the long sidecar holds four cues");
    let long_track = long[0]
        .parent()
        .expect("a frame lives inside its media's directory")
        .to_path_buf();
    assert!(
        long_track.is_dir(),
        "the long track should have a directory"
    );

    // Room for one track, which the open one takes.
    app.app.set_preview_settings(PreviewSettings {
        prefetch: true,
        network: false,
        cache_tracks: 1,
        ..PreviewSettings::default()
    });
    // The filesystem's mtime granularity can be coarse enough that a rewrite within the
    // same tick is indistinguishable from no rewrite at all.
    std::thread::sleep(Duration::from_millis(1100));

    // Act: back to the short track, whose pass prunes before it renders.
    let reopened = render_sidecar_track(&mut app, "short.mkv");

    // Assert: every one of its frames survived untouched, despite its being the least
    // recently used track in the cache.
    assert_eq!(
        reopened, short,
        "the same track should hash to the same frames"
    );
    for (path, was) in short.iter().zip(&rendered) {
        assert_eq!(
            modified(path),
            *was,
            "the open track's own pass evicted and re-rendered {}",
            path.display()
        );
    }
    // And the track that was not open went as a unit — the directory itself, not some of
    // the frames in it. A cache that could bisect a track would leave this standing with
    // part of its contents, and the next visit would re-render the difference.
    assert!(
        !long_track.exists(),
        "the track that was not open should have been evicted whole, but {} survived",
        long_track.display()
    );
}

/// Opens a file's sidecar subtitle edit page, renders the whole track, and answers where each
/// cue's frame landed. Leaves the page closed, ready for the next file.
fn render_sidecar_track(app: &mut Harness, file: &str) -> Vec<PathBuf> {
    // `Harness::open` only ever walks downwards, so the cursor has to start above the file
    // it is looking for — which after the previous track it is not.
    app.wait_until("the file panel to list the media", |state| {
        state.files.iter().any(|entry| entry.display_name == file)
    });
    for _ in 0..app.app.files.len() {
        app.press(key(KeyCode::Char('k')));
    }
    app.open(file);
    open_sidecar_edit_page(app);
    wait_for_frames(app);
    let state = app.app.subtitle_edit.as_ref().unwrap();
    let paths = (0..state.cues.len())
        .map(|index| cached_frame(state, index))
        .collect();
    // Back out to the file panel: Esc leaves the subtitle edit page for the track list, and
    // again for the files, which is where the next `open` starts from.
    while app.app.layer != Layer::Files {
        app.press(key(KeyCode::Esc));
        app.pump();
    }
    assert!(app.app.subtitle_edit.is_none(), "Esc should close the page");
    paths
}

fn modified(path: &PathBuf) -> SystemTime {
    fs::metadata(path)
        .unwrap_or_else(|error| panic!("{} should be cached: {error}", path.display()))
        .modified()
        .expect("the platform reports mtimes")
}

/// Four cues far enough apart to sit on distinct frames of a six-second clip, with text
/// distinctive enough to be counted on screen.
const WALKED_CUES: &str = "1\n00:00:00,500 --> 00:00:01,500\nWalkedone\n\n\
                           2\n00:00:02,000 --> 00:00:03,000\nWalkedtwo\n\n\
                           3\n00:00:03,500 --> 00:00:04,500\nWalkedthree\n\n\
                           4\n00:00:05,000 --> 00:00:05,800\nWalkedfour\n\n";

/// Walking the cue list of an already-rendered track puts each cue's picture on screen in
/// the draw that handles the keypress, and never stands the cue's text in for it.
///
/// Both halves of one complaint. The pane used to draw the line as text whenever it had no
/// frame, and every frame took a round trip through the worker to arrive — so each `j`
/// flashed the text and then replaced it with the picture a moment later. The page now
/// keeps the cues either side of the selection encoded and ready, and draws nothing at all
/// when it has nothing.
///
/// The single `pump` after each keypress is the assertion: it is one turn of the event
/// loop, so a frame that needed the worker to answer could not possibly be on screen yet.
#[test]
fn walking_the_edit_page_should_draw_each_cues_frame_in_the_same_pass_as_the_keypress() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "walking_the_edit_page_should_draw_each_cues_frame_in_the_same_pass_as_the_keypress";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-frame-walk");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(6.0)
            .audio(&["eng"]),
    );
    fs::write(scratch.join("clip.eng.srt"), WALKED_CUES).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);

    // Before any frame has been drawn, which is the state the text fallback used to fill.
    let bare = app.screen();
    let listed = bare.matches("Walkedone").count();
    assert!(
        listed > 0,
        "the cue list should name the selected cue:\n{bare}"
    );
    assert!(
        app.preview_shades().is_empty(),
        "a page with no frame yet should draw an empty pane:\n{bare}"
    );

    // The whole track rendered to disk, and the window around the cursor encoded.
    wait_for_frames(&mut app);
    app.wait_until("the first cue's frame and the one behind it", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.frame().is_some() && state.has_frame(1))
    });
    assert_eq!(
        app.screen().matches("Walkedone").count(),
        listed,
        "the arriving frame must not change how often the cue's text is on screen — \
         it was never the preview pane drawing it"
    );

    // Walk the track. The wait before each keypress is the window refilling itself as the
    // cursor advances; the single pump after it is the assertion — one turn of the event
    // loop, so anything on screen got there without the worker being given a chance to
    // answer.
    for (selected, text) in [(1, "Walkedtwo"), (2, "Walkedthree"), (3, "Walkedfour")] {
        app.wait_until(
            "the next cue to be encoded ahead of the cursor",
            move |app| {
                app.subtitle_edit
                    .as_ref()
                    .is_some_and(|state| state.has_frame(selected))
            },
        );
        app.press(key(KeyCode::Char('j')));
        app.pump();

        let state = app
            .app
            .subtitle_edit
            .as_ref()
            .expect("the page should still be open");
        assert_eq!(state.selected, selected, "j should move the cursor");
        assert!(
            state.frame().is_some(),
            "cue {selected} should already have been encoded before the cursor reached it"
        );
        let screen = app.screen();
        assert!(
            !app.preview_shades().is_empty(),
            "cue {selected}'s frame should be on screen in the same pass as the keypress; \
             screen:\n{screen}"
        );
        assert_eq!(
            screen.matches(text).count(),
            listed,
            "the preview pane must not draw {text} as text alongside its picture; \
             screen:\n{screen}"
        );
    }
}

/// An ASS script whose two cues read identically and draw completely differently.
///
/// That is the whole fixture. One cue is the file's `Default` style at the bottom of the
/// frame; the other names a `Sign` style and pins itself to the top with `{\pos}`. Nothing
/// about the difference is in the text, so a preview that staged the text — or that
/// transcoded the track to SubRip on the way in — would draw the two cues the same.
const STYLED_ASS: &str = "[Script Info]\n\
     ScriptType: v4.00+\n\
     PlayResX: 320\n\
     PlayResY: 240\n\
     \n\
     [V4+ Styles]\n\
     Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
     Style: Default,Arial,16,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,1,0,2,10,10,10,1\n\
     Style: Sign,Arial,48,&H0000CCFF,&H000000FF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,3,0,8,10,10,10,1\n\
     \n\
     [Events]\n\
     Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
     Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,IDENTICAL WORDS\n\
     Dialogue: 0,0:00:03.00,0:00:04.00,Sign,,0,0,0,,{\\pos(160,20)}IDENTICAL WORDS\n";

/// An ASS track previews with its own styling, not with libass's defaults.
///
/// The reason ASS is copied out of its container rather than transcoded to SubRip like
/// WebVTT is. An ASS cue names a style rather than carrying one and positions itself
/// against the script's declared `PlayRes`, so a `Dialogue:` line lifted out on its own
/// draws in the wrong font at the wrong place — a picture the user will never see, on the
/// one page whose whole job is comparing subtitles against the picture.
///
/// **Asserted on rendered bytes, because nothing else would notice.** The two cues carry
/// the same words at different times, so a preview that lost the styling would stage them
/// byte-for-byte alike and render two identical pictures. Counting cues, counting cache
/// files, or reading the screen all pass in that world. Comparing the frames does not.
#[test]
fn an_ass_track_should_preview_with_its_own_styles_rather_than_libass_defaults() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "an_ass_track_should_preview_with_its_own_styles_rather_than_libass_defaults";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-edit-ass");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(6.0)
            .audio(&["eng"]),
    );
    fs::write(scratch.join("clip.eng.ass"), STYLED_ASS).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);

    // The cue list shows words, not markup: `{\pos(160,20)}` is how the cue draws, not
    // what it says, and a list full of override blocks is unreadable.
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert_eq!(
        state.cues.len(),
        2,
        "both cues should parse: {:?}",
        state.cues
    );
    assert_eq!(state.cues[0].text, "IDENTICAL WORDS");
    assert_eq!(state.cues[1].text, "IDENTICAL WORDS");
    let screen = app.screen();
    assert!(
        !screen.contains("\\pos") && !screen.contains("{"),
        "override markup should not reach the cue list:\n{screen}"
    );

    // Each cue keeps the line that draws it, which is where the styling lives.
    assert!(
        state.cues[1]
            .dialogue
            .iter()
            .any(|line| line.contains("Sign") && line.contains("\\pos(160,20)")),
        "the styled cue should keep its own Dialogue line: {:?}",
        state.cues[1].dialogue
    );

    // Render the whole track, then compare the two cues' frames.
    wait_for_frames(&mut app);
    let state = app.app.subtitle_edit.as_ref().unwrap();
    let first = cached_frame(state, 0);
    let second = cached_frame(state, 1);
    assert_ne!(
        first, second,
        "two cues that draw differently must not share a cache entry"
    );
    let first_bytes = fs::read(&first)
        .unwrap_or_else(|error| panic!("the first cue should have rendered: {error}"));
    let second_bytes = fs::read(&second)
        .unwrap_or_else(|error| panic!("the styled cue should have rendered: {error}"));
    // `assert!` rather than `assert_ne!`, which would print two whole JPEGs.
    assert!(
        first_bytes != second_bytes,
        "the same words in a different style, font size and position must not produce the \
         same picture — the styling is being dropped somewhere between the file and libass; \
         both frames are {} bytes",
        first_bytes.len()
    );

    // And the page draws, so the styled path works end to end rather than only caching.
    app.wait_until("a frame for the selected cue", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.frame().is_some())
    });
    assert!(
        app.preview_shades().len() > 1,
        "the preview should hold a decoded image:\n{}",
        app.screen()
    );
}

/// One visible line spread across events that share a moment — a karaoke or typeset track,
/// reduced to the smallest shape that has the defect.
///
/// The first cue is alone. The second says exactly the same words in exactly the same style
/// for exactly as long, but has a third cue drawn over it. The video is a constant black
/// frame, so the two moments are the same picture apart from what the subtitles put there.
const OVERLAPPING_ASS: &str = "[Script Info]\n\
     ScriptType: v4.00+\n\
     PlayResX: 320\n\
     PlayResY: 240\n\
     \n\
     [V4+ Styles]\n\
     Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
     Style: Default,Arial,16,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,1,0,2,10,10,10,1\n\
     Style: Sign,Arial,48,&H0000CCFF,&H000000FF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,3,0,8,10,10,10,1\n\
     \n\
     [Events]\n\
     Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
     Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,SHARED WORDS\n\
     Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,SHARED WORDS\n\
     Dialogue: 0,0:00:03.00,0:00:03.80,Sign,,0,0,0,,{\\pos(160,20)}OVERLAY\n";

/// A cue's frame shows everything on screen with it, not that cue with the rest deleted.
///
/// The case this exists for is the one the page is worst at: a typeset or karaoke line is
/// routinely a dozen `Dialogue:` events sharing a moment, each drawing part of one effect.
/// Burning the selected one alone draws a fraction of a picture the viewer never sees — and
/// on the one page whose whole job is judging a subtitle against the picture, that is not a
/// degraded preview, it is the wrong answer.
///
/// **Asserted on rendered bytes, because nothing else would notice.** Both halves would pass
/// against the broken code otherwise: the cue list is the same either way, the cache holds a
/// frame per cue either way, and the commands differ only inside a staged file. Two things
/// are compared, and each fails on its own:
///
/// - the lone cue against the identical one with an overlay over it — the same words, style
///   and duration over the same black frame, so the *only* thing that can differ is whether
///   the overlay reached the picture;
/// - the two cues that share a moment against each other — they are one picture, so their
///   frames must be identical. Before this they were complements: words in one, overlay in
///   the other, neither showing what a viewer would see.
#[test]
fn a_cues_frame_should_show_everything_on_screen_with_it() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "a_cues_frame_should_show_everything_on_screen_with_it";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-edit-overlap");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(6.0)
            .audio(&["eng"]),
    );
    fs::write(scratch.join("clip.eng.ass"), OVERLAPPING_ASS).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);

    // Arrange: find the three cues by what they say and when, rather than by position —
    // two of them start at the same instant and nothing here should depend on which of
    // those the parse's sort put first.
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert_eq!(state.cues.len(), 3, "all three cues should parse");
    let index_of = |start_ms: u64, text: &str| {
        state
            .cues
            .iter()
            .position(|cue| cue.start == Duration::from_millis(start_ms) && cue.text == text)
            .unwrap_or_else(|| panic!("no cue saying {text:?} at {start_ms} ms: {:?}", state.cues))
    };
    let lone = index_of(1000, "SHARED WORDS");
    let accompanied = index_of(3000, "SHARED WORDS");
    let overlay = index_of(3000, "OVERLAY");

    // Act: render the whole track.
    wait_for_frames(&mut app);
    let state = app.app.subtitle_edit.as_ref().unwrap();
    let lone_frame = fs::read(cached_frame(state, lone))
        .unwrap_or_else(|error| panic!("the lone cue should have rendered: {error}"));
    let accompanied_frame = fs::read(cached_frame(state, accompanied))
        .unwrap_or_else(|error| panic!("the accompanied cue should have rendered: {error}"));
    let overlay_frame = fs::read(cached_frame(state, overlay))
        .unwrap_or_else(|error| panic!("the overlay cue should have rendered: {error}"));

    // Assert: the overlay reached the accompanied cue's picture. `assert!` rather than
    // `assert_ne!`, which would print two whole JPEGs.
    assert!(
        lone_frame != accompanied_frame,
        "two cues with the same words, style and duration over the same black frame drew the \
         same picture, so the line over the second one never reached it — the preview is \
         burning in the selected cue instead of what is on screen; both frames are {} bytes",
        lone_frame.len()
    );

    // Assert: and the two cues sharing that moment are one picture, so their frames match.
    assert!(
        accompanied_frame == overlay_frame,
        "two cues on screen together should draw the same picture, and these differ by {} \
         bytes against {} — each is being drawn without the other",
        accompanied_frame.len().abs_diff(overlay_frame.len()),
        accompanied_frame.len()
    );

    // Assert: and the page really draws it, so this is the live path rather than the cache.
    app.wait_until("a frame for the selected cue", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.frame().is_some())
    });
    assert!(
        app.preview_shades().len() > 1,
        "the preview should hold a decoded image:\n{}",
        app.screen()
    );
}

/// Two cues of identical words and identical length, one coming in while the picture is
/// black and one after it has turned white, both of whose *midpoints* fall in the white
/// stretch.
const SHOT_CHANGE_CUES: &str = "1\n00:00:01,000 --> 00:00:05,000\nSHARED WORDS\n\n\
                                2\n00:00:05,000 --> 00:00:09,000\nSHARED WORDS\n\n";

/// A cue's still is the frame it comes in on, which is the only frame that says whether it
/// came in with the shot.
///
/// The picture turns from black to white two seconds in, and the two cues are identical in
/// every other way — same words, same length, and both midpoints in the white stretch. So a
/// grab at the midpoint draws two white frames and the difference the reader came here to
/// see is invisible; a grab at the start draws one black and one white.
///
/// Asserted on the cached pictures rather than on the seek, because every layer short of
/// the pixels agrees either way: the cue list is the same, the cache holds a frame per cue,
/// and the commands differ only in one `-ss`.
#[test]
fn a_cues_still_should_be_the_frame_it_comes_in_on() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "a_cues_still_should_be_the_frame_it_comes_in_on";
    require_tools(test, &["ffmpeg:libx264"]);

    let scratch = Scratch::new("subtitle-edit-shot-change");
    write_shot_change_media(&scratch.join("clip.mkv"));
    fs::write(scratch.join("clip.eng.srt"), SHOT_CHANGE_CUES).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);

    // Act: render the whole track.
    wait_for_frames(&mut app);
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert_eq!(state.cues.len(), 2, "both cues should parse");
    let before = mean_luminance(&cached_frame(state, 0));
    let after = mean_luminance(&cached_frame(state, 1));

    // Assert: the cue that comes in while the shot is still black was grabbed there. At the
    // midpoint both of these are the same white frame.
    assert!(
        before < 64.0,
        "the first cue comes in two seconds before the picture turns white, so its still \
         should be the black frame it arrives on — mean luminance was {before:.1}"
    );
    assert!(
        after > 192.0,
        "the second cue comes in after the picture has turned white, so its still should be \
         white — mean luminance was {after:.1}"
    );
}

/// A clip whose picture turns from black to white two seconds in, so the frame a grab lands
/// on can be read off the picture itself. Built here rather than through `MediaSpec`, whose
/// video source is a single flat colour for the whole file.
fn write_shot_change_media(path: &std::path::Path) {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("color=c=black:s=320x240:r=10:d=12")
        .args([
            "-vf",
            "drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='gte(t,2)'",
        ])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(path)
        .status()
        .expect("ffmpeg should run");
    assert!(status.success(), "building {} failed", path.display());
}

/// The average brightness of a rendered frame, as a number the assertions can read.
fn mean_luminance(frame: &std::path::Path) -> f64 {
    let image = image::open(frame)
        .unwrap_or_else(|error| panic!("{} should be a readable frame: {error}", frame.display()))
        .to_luma8();
    let total: u64 = image.pixels().map(|pixel| u64::from(pixel.0[0])).sum();
    total as f64 / (image.width() * image.height()) as f64
}

/// A karaoke effect as a file really carries it: one visible line spread over four
/// `Dialogue:` events that share a timing and a set of words, each scaling the text a little
/// further so that together they animate. Plus one ordinary line over the top of them, whose
/// timing differs.
const KARAOKE_ASS: &str = "[Script Info]\n\
     ScriptType: v4.00+\n\
     PlayResX: 320\n\
     PlayResY: 240\n\
     \n\
     [V4+ Styles]\n\
     Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
     Style: Default,Arial,16,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,1,0,2,10,10,10,1\n\
     \n\
     [Events]\n\
     Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
     Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,fu\n\
     Dialogue: 0,0:00:03.00,0:00:03.60,Default,,0,0,0,,{\\pos(60,60)}wo\n\
     Dialogue: 0,0:00:03.00,0:00:03.60,Default,,0,0,0,,{\\pos(120,60)}wo\n\
     Dialogue: 0,0:00:03.00,0:00:03.60,Default,,0,0,0,,{\\pos(180,60)}wo\n\
     Dialogue: 0,0:00:03.00,0:00:03.60,Default,,0,0,0,,{\\pos(240,60)}wo\n";

/// One visible line is one row on the page, however many events draw it.
///
/// The complaint this answers: a karaoke or typeset track filled the cue list with rows that
/// were identical in every way a reader can see — same words, same timing — each previewing
/// a fraction of the picture, with no way to tell which was which or why there were ten.
///
/// Asserted the whole way through, because the fold has to reach every part of the page at
/// once: the list shows one row, the timeline packs one block, the frame under the cursor is
/// the *whole* line rather than a quarter of it, and `p` plays that one span. A fold that
/// only reached the list would leave the cursor sitting on a row whose preview and playback
/// still belonged to one of the four events.
#[test]
fn events_that_draw_one_line_should_be_one_row_end_to_end() {
    // Serialised against the other frame-cache scenarios: they share one cache and
    // prune each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "events_that_draw_one_line_should_be_one_row_end_to_end";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-edit-karaoke");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(6.0)
            .audio(&["eng"]),
    );
    fs::write(scratch.join("clip.eng.ass"), KARAOKE_ASS).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);

    // Assert: two rows for five events, and the folded one keeps the timing all four of its
    // events shared.
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert_eq!(
        state.cues.len(),
        2,
        "the four events that draw one line should be one row: {:?}",
        state.cues
    );
    assert_eq!(state.cues[1].text, "wo");
    assert_eq!(state.cues[1].start, Duration::from_millis(3000));
    assert_eq!(state.cues[1].end, Duration::from_millis(3600));
    assert_eq!(
        state.cues[1].dialogue.len(),
        4,
        "the row should keep every line that draws it"
    );

    // Assert: the list shows it once. Four rows would put four identical timestamps on
    // screen, which is the thing being complained about.
    let screen = app.screen();
    assert_eq!(
        screen.matches("00:00:03.0").count(),
        1,
        "the folded line should appear once in the cue list:\n{screen}"
    );

    // Assert: and it says how many entries it stands for, so the fold is visible rather than
    // a list that quietly has fewer rows than the file has events. The row that stands on its
    // own says nothing.
    assert!(
        screen.contains("×4"),
        "the folded row should say how many events drew it:\n{screen}"
    );
    assert!(
        !screen.contains("×1"),
        "an ordinary row should carry no count:\n{screen}"
    );

    // Assert: and the frame under it is the whole line. All four events are on screen at the
    // moment it is grabbed, so a preview of one of them would be a quarter of the picture —
    // compared against a deliberately rebuilt one-event frame, since nothing but the pixels
    // would notice.
    wait_for_frames(&mut app);
    app.press(key(KeyCode::Char('j')));
    app.wait_until("the folded row's frame", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.selected == 1 && state.frame().is_some())
    });
    let state = app.app.subtitle_edit.as_ref().unwrap();
    let whole = fs::read(cached_frame(state, 1))
        .unwrap_or_else(|error| panic!("the folded row should have rendered: {error}"));
    let mut one_event = state.cues[1].clone();
    one_event.dialogue.truncate(1);
    let quarter = frame_path(
        &state
            .frames
            .key(&one_event, std::slice::from_ref(&one_event)),
    );
    assert!(
        !quarter.is_file() || fs::read(&quarter).unwrap() != whole,
        "the folded row drew the same picture as one of its four events, so three of them \
         never reached the frame"
    );

    // Assert: the timeline packs one block for it rather than four stacked lanes.
    assert_eq!(
        state.layout.lane_count, 1,
        "one line should occupy one lane, not one per event"
    );

    // Act / Assert: and `p` plays that one span, burning all four events into it.
    app.press(key(KeyCode::Char('p')));
    app.wait_until("the span to start playing", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.playback_frame().is_some())
    });
    let state = app.app.subtitle_edit.as_ref().unwrap();
    let position = state
        .playback_position()
        .expect("a playing span knows where it is");
    assert!(
        position < Duration::from_millis(3600),
        "the span should be the folded row's own, and it started at {position:?}"
    );
    // A shade rather than several: the span opens a second before the cue, where the
    // fixture's video is solid black and nothing is burned in yet.
    assert!(
        !app.preview_shades().is_empty(),
        "the playback should be drawn:\n{}",
        app.screen()
    );
}

/// A track whose middle three cues are on screen together, over a lone cue either side.
const GROUPED_CUES: &str = "1\n\
     00:00:00,500 --> 00:00:01,500\n\
     alone at the start\n\
     \n\
     2\n\
     00:00:02,000 --> 00:00:04,000\n\
     the spoken line\n\
     \n\
     3\n\
     00:00:02,500 --> 00:00:04,500\n\
     a sign over it\n\
     \n\
     4\n\
     00:00:03,000 --> 00:00:05,000\n\
     and a third\n\
     \n\
     5\n\
     00:00:05,500 --> 00:00:06,000\n\
     alone at the end\n";

/// Cues that share the screen are one row of the list, and the row says so.
///
/// The complaint this answers: the cue panel drew two cues that are on screen *together*
/// exactly as it drew two that merely follow one another — a block, an arrow, a block. On the
/// page whose whole job is judging a subtitle against the picture it is burned into, the one
/// relationship worth seeing was the one the list could not express.
///
/// Asserted through the whole page rather than on the panel alone, because the grouping
/// reaches the movement keys as well as the drawing: `j` has to step *over* a group where it
/// used to step through it, `h`/`l` have to move inside one, and the frame and timeline have
/// to follow whichever member the cursor lands on.
#[test]
fn cues_that_share_the_screen_should_be_one_row_of_the_list() {
    // Serialised against the other frame-cache scenarios: they share one cache and prune
    // each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "cues_that_share_the_screen_should_be_one_row_of_the_list";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-edit-overlap");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(7.0)
            .audio(&["eng"]),
    );
    fs::write(scratch.join("clip.eng.srt"), GROUPED_CUES).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);
    wait_for_frames(&mut app);

    // Assert: five cues, but three rows — the middle three are one group.
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert_eq!(state.cues.len(), 5, "every cue should still be read");
    let groups: Vec<(usize, usize)> = state
        .groups
        .iter()
        .map(|group| (group.first, group.len))
        .collect();
    assert_eq!(
        groups,
        vec![(0, 1), (1, 3), (4, 1)],
        "the three overlapping cues should be one group"
    );

    // Assert: the panel draws the group as a fork into two blocks side by side. The lone
    // cues keep the full timing; the pair takes the compact one, because half a panel cannot
    // hold twenty-three characters.
    let screen = app.screen();
    assert!(
        screen.contains("00:00:00.5 → 00:00:01.5"),
        "a lone cue should keep its full timing:\n{screen}"
    );
    assert!(
        screen.contains("0:02.0→0:04.0") && screen.contains("0:02.5→0:04.5"),
        "the group's first two members should be drawn side by side:\n{screen}"
    );
    assert!(
        screen.contains('┬'),
        "the group reaches past what is drawn, so its bar should run off that side:\n{screen}"
    );
    assert!(
        !screen.contains("and a third"),
        "only two members fit, so the third should be off the row:\n{screen}"
    );

    // Act / Assert: `j` steps over the whole group rather than through it, and `k` comes
    // back to the member it entered on.
    app.press(key(KeyCode::Char('j')));
    assert_eq!(app.app.subtitle_edit.as_ref().unwrap().selected, 1);
    app.press(key(KeyCode::Char('j')));
    assert_eq!(
        app.app.subtitle_edit.as_ref().unwrap().selected,
        4,
        "j should leave the group rather than visit its second member"
    );
    app.press(key(KeyCode::Char('k')));
    assert_eq!(app.app.subtitle_edit.as_ref().unwrap().selected, 1);

    // Act / Assert: the first `l` crosses the page without moving the pair, and the second
    // turns it — the drawn pair follows, and the bar now runs off the other side.
    app.press(key(KeyCode::Char('l')));
    assert_eq!(app.app.subtitle_edit.as_ref().unwrap().selected, 2);
    // `press` pumps before the key rather than after it, so the panel is a press behind
    // until the loop runs again.
    app.pump();
    let screen = app.screen();
    assert!(
        !screen.contains("and a third"),
        "crossing the page should leave the pair on screen where it is:\n{screen}"
    );
    app.press(key(KeyCode::Char('l')));
    assert_eq!(app.app.subtitle_edit.as_ref().unwrap().selected, 3);
    app.pump();
    let screen = app.screen();
    assert!(
        screen.contains("and a third"),
        "the third member should be drawn once the page turns:\n{screen}"
    );
    assert!(
        !screen.contains("the spoken line"),
        "the pair should have slid past the group's first member:\n{screen}"
    );

    // Act / Assert: `l` at the group's far end is held rather than spilling into the next
    // row — `j` is the only way out.
    app.press(key(KeyCode::Char('l')));
    assert_eq!(
        app.app.subtitle_edit.as_ref().unwrap().selected,
        3,
        "a group is a closed unit sideways"
    );

    // Act / Assert: `j` out of the group and `k` back into it returns to the member the
    // cursor was left on, with the same pair drawn around it — leaving a row to look at the
    // one below must not cost the reader their place sideways.
    app.press(key(KeyCode::Char('j')));
    assert_eq!(app.app.subtitle_edit.as_ref().unwrap().selected, 4);
    app.pump();
    let screen = app.screen();
    assert!(
        screen.contains("and a third") && !screen.contains("the spoken line"),
        "the group left behind should keep the page it was left on:\n{screen}"
    );
    app.press(key(KeyCode::Char('k')));
    app.pump();
    assert_eq!(
        app.app.subtitle_edit.as_ref().unwrap().selected,
        3,
        "a group should be re-entered where it was left"
    );
    let screen = app.screen();
    assert!(
        screen.contains("and a third") && !screen.contains("the spoken line"),
        "the pair drawn around it should be the one it was left in:\n{screen}"
    );

    // Assert: the rest of the page followed the cursor into the group. The timeline names
    // the member under it, and the preview holds that member's own frame rather than the
    // one the group was entered on.
    app.wait_until("the third member's frame", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.selected == 3 && state.frame().is_some())
    });
    let screen = app.screen();
    assert!(
        screen.contains("00:00:03.0 → 00:00:05.0"),
        "the timeline should name the cue the cursor is on:\n{screen}"
    );
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert!(
        cached_frame(state, 1) != cached_frame(state, 3),
        "each member of a group should have its own frame"
    );
}

/// A typeset track's timeline should be readable rather than a texture.
///
/// The complaint this answers: on an ASS track carrying signs and karaoke, the timeline drew
/// every one of the hundreds of events in its minute-wide window as a bracketed span, four
/// lanes deep. Every lane filled end to end and the pane said nothing at all — least of all
/// the one thing it exists for, which is where the selected cue sits against its neighbours.
///
/// The answer is scale rather than selection: the window shortens until the cues in it can
/// be drawn as spans. Every cue is still drawn in full, because which lines begin and end
/// where is most of what the pane is worth reading for.
#[test]
fn a_dense_track_should_shorten_the_timeline_until_its_cues_are_readable() {
    // Serialised against the other frame-cache scenarios: they share one cache and prune
    // each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "a_dense_track_should_shorten_the_timeline_until_its_cues_are_readable";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-edit-dense");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(45.0)
            .audio(&["eng"]),
    );
    // A hundred and fifty overlapping cues packed into twenty seconds, which is what a
    // typeset scene looks like: far more events than a minute-wide window can draw as spans.
    let dense: String = (0..150)
        .map(|n| {
            let start = 20_000 + n * 130;
            format!(
                "{}\n00:00:{:02},{:03} --> 00:00:{:02},{:03}\nevent {n}\n\n",
                n + 1,
                start / 1000,
                start % 1000,
                (start + 600) / 1000,
                (start + 600) % 1000,
            )
        })
        .collect();
    fs::write(scratch.join("clip.eng.srt"), dense).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);

    // Act: to the end of the track, which is the densest part of it.
    app.press(key(KeyCode::Char('G')));
    app.pump();

    // Assert: the axis no longer reaches back to the start of the media, because the window
    // shortened around the selection rather than keeping the full minute.
    let screen = app.screen();
    let lines: Vec<&str> = screen.lines().collect();
    let top = lines
        .iter()
        .position(|line| line.contains("Timeline ("))
        .expect("the timeline pane should be on screen");
    let bottom = top
        + 1
        + lines[top + 1..]
            .iter()
            .position(|line| line.contains('┘'))
            .expect("the timeline pane should be closed");
    let ruler = lines[bottom - 1];
    let readings: Vec<u64> = ruler
        .split_whitespace()
        .filter_map(|token| token.split_once(':'))
        .filter_map(|(minutes, seconds)| {
            Some(minutes.parse::<u64>().ok()? * 60 + seconds.parse::<u64>().ok()?)
        })
        .collect();
    let selected_at = 20;
    assert!(
        readings.len() >= 2
            && readings
                .iter()
                .all(|reading| reading.abs_diff(selected_at) <= 10),
        "the axis should have closed in around the selection at {selected_at}s:\n{ruler}"
    );

    // Assert: and the cues in it are wide enough to read as spans rather than as marks.
    // Every cue is still drawn in full — the shorter window is what buys the room, and
    // nothing is demoted to make space.
    let track = lines[top + 1..bottom - 1].join("\n");
    assert!(
        track.contains("|<──") && !track.contains("||||"),
        "the cues in the window should be drawn as readable spans:\n{track}"
    );
}

/// A cue is edited on the subtitle edit page, staged like any other edit, and written by Ctrl+S.
///
/// Asserted through the whole workflow rather than on the editor alone, because the feature
/// is the workflow: the words have to reach the buffer, the buffer has to reach the staged
/// edit, the staged edit has to survive leaving the page, and the save has to put the new
/// line — and only that line — into the file on disk. Every one of those halves looks right
/// on its own while the file still says what it always said.
#[test]
fn editing_a_cue_should_stage_it_and_ctrl_s_should_write_it_to_the_file() {
    // Serialised against the other frame-cache scenarios: they share one cache and prune
    // each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "editing_a_cue_should_stage_it_and_ctrl_s_should_write_it_to_the_file";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-cue-edit");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(7.0)
            .audio(&["eng"]),
    );
    let sidecar = scratch.join("clip.eng.srt");
    fs::write(&sidecar, CACHED_CUES).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);
    // The background pass first: while it runs it owns the corner of the cue panel the
    // edited-count uses, so waiting it out is what makes the count assertable at all.
    wait_for_frames(&mut app);

    // Act: edit the second cue — down a row, `i`, type, and leave the editor.
    app.press(key(KeyCode::Char('j')));
    app.press(key(KeyCode::Char('i')));
    assert_eq!(
        app.app.dialog,
        Some(Dialog::EditCue),
        "i should open the cue editor"
    );
    for character in ", rewritten".chars() {
        app.press(key(KeyCode::Char(character)));
    }
    app.press(key(KeyCode::Esc));

    // Assert: staged, and the page shows the new words in the list.
    assert_eq!(app.app.dialog, None, "Esc should close the editor");
    assert!(
        app.app.has_unsaved_cue_edits(),
        "leaving the editor should keep the typing"
    );
    app.pump();
    let screen = app.screen();
    assert!(
        screen.contains("Second line, rewritten") && screen.contains("1 edited"),
        "the page should show the edit and say it is unwritten:\n{screen}"
    );

    // Act / Assert: `Esc` off the page asks before leaving the edits behind, and saying no
    // keeps the reader where they are.
    app.press(key(KeyCode::Esc));
    assert_eq!(app.app.dialog, Some(Dialog::ConfirmLeaveCues));
    app.press(key(KeyCode::Enter));
    assert_eq!(
        app.app.layer,
        Layer::SubtitleEdit,
        "the safe answer should keep the page open"
    );

    // Act: write it.
    app.process_all();

    // Assert: the reader is still on the subtitle edit page, on the cue they edited. The save
    // rewrites the file the page is reading, so the page is closed and the file re-read
    // behind the scenes — but "save this cue" is not "take me somewhere else".
    app.wait_until("the subtitle edit page to come back", |app| {
        app.layer == Layer::SubtitleEdit
            && app
                .subtitle_edit
                .as_ref()
                .is_some_and(|state| !state.cues.is_empty())
    });
    assert_eq!(
        app.app.subtitle_edit.as_ref().unwrap().selected,
        1,
        "the cursor should come back to the cue that was edited"
    );

    // Assert: the file on disk carries the new line, and the cue nobody touched is exactly
    // as it was.
    let written = fs::read_to_string(&sidecar).expect("the sidecar should still be there");
    assert!(
        written.contains("Second line, rewritten"),
        "the save should write the edited cue:\n{written}"
    );
    assert!(
        written.contains("First line"),
        "the save should leave the other cue alone:\n{written}"
    );
    assert!(
        written.contains("00:00:03,000 --> 00:00:04,000"),
        "the save should leave the timings alone:\n{written}"
    );

    // Assert: and with the write done there is nothing left to warn about.
    assert!(
        !app.app.has_unsaved_cue_edits(),
        "a written edit should stop being unsaved work"
    );

    // Act: type into another cue and this time answer the question with "discard".
    app.press(key(KeyCode::Char('i')));
    for character in " and again".chars() {
        app.press(key(KeyCode::Char(character)));
    }
    app.press(key(KeyCode::Esc));
    assert!(
        app.app.has_unsaved_cue_edits(),
        "the second edit should stage like the first"
    );
    app.press(key(KeyCode::Esc));
    assert_eq!(app.app.dialog, Some(Dialog::ConfirmLeaveCues));
    app.press(key(KeyCode::Char('l')));
    app.press(key(KeyCode::Enter));

    // Assert: the page is closed and the words are gone rather than travelling with the
    // file to the next Ctrl+S, where they would be written from a page nobody is looking at.
    assert_eq!(app.app.layer, Layer::Streams, "discarding should leave");
    assert!(
        !app.app.has_track_edits(),
        "discarding should take the staged cue text with it"
    );

    // Assert: and coming back to the page shows the file's words, not the discarded ones.
    open_sidecar_edit_page(&mut app);
    app.pump();
    let screen = app.screen();
    assert!(
        !screen.contains("and again") && !screen.contains("edited"),
        "the discarded edit should be gone from the page:\n{screen}"
    );
    let written = fs::read_to_string(&sidecar).expect("the sidecar should still be there");
    assert!(
        !written.contains("and again"),
        "a discarded edit should never reach the file:\n{written}"
    );
}

/// Saving a cue edit on an embedded track keeps the frames the page already rendered.
///
/// Writing an embedded track means remuxing the file, which moves its length and mtime —
/// and those are in the frame cache's media key, so every frame the page rendered is filed
/// under a name that no longer describes anything. Left alone, saving one word costs a
/// feature-length track its entire cache and the page spends the next several minutes
/// rendering it again, every save. The frames move with the file instead, because the remux
/// copies the video stream through untouched.
///
/// Asserted on the cached files rather than on a count of `ffmpeg` runs: the frame for the
/// cue that was rewritten *should* be rendered again — its picture changed — and it is the
/// cues nobody touched that must survive.
#[test]
fn saving_a_cue_edit_should_keep_the_frames_the_page_already_rendered() {
    // Serialised against the other frame-cache scenarios: they share one cache and prune
    // each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "saving_a_cue_edit_should_keep_the_frames_the_page_already_rendered";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-cue-edit-cache");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(7.0)
            .audio(&["eng"])
            .subtitles(vec![SubtitleSpec::new("eng", "subrip").cues(CACHED_CUES)]),
    );

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    let subtitle_row = app.first_subtitle_row();
    app.select_track_row(subtitle_row);
    app.press(key(KeyCode::Char('c')));
    assert_eq!(app.app.layer, Layer::SubtitleEdit, "c should open the page");
    app.wait_until("the embedded track's cues to be read", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| !state.cues.is_empty())
    });
    wait_for_frames(&mut app);

    // What the pass rendered, and where it put it.
    let state = app.app.subtitle_edit.as_ref().unwrap();
    assert_eq!(state.cues.len(), 2, "the track holds two cues");
    let old_track = track_dir(&state.frames.media_key());
    let untouched = cached_frame(state, 0);
    let untouched_bytes = fs::read(&untouched).expect("the first cue's frame should be cached");

    // Act: rewrite the *second* cue and write it, which remuxes the file.
    app.press(key(KeyCode::Char('j')));
    app.press(key(KeyCode::Char('i')));
    for character in ", rewritten".chars() {
        app.press(key(KeyCode::Char(character)));
    }
    app.press(key(KeyCode::Esc));
    app.process_all();
    app.wait_until("the subtitle edit page to come back", |app| {
        app.layer == Layer::SubtitleEdit
            && app
                .subtitle_edit
                .as_ref()
                .is_some_and(|state| !state.cues.is_empty())
    });

    // Assert: the file really was rewritten, so this is not passing on an unchanged key.
    let state = app.app.subtitle_edit.as_ref().unwrap();
    let new_track = track_dir(&state.frames.media_key());
    assert_ne!(
        new_track, old_track,
        "remuxing the file should move its frame-cache key"
    );
    assert!(
        !old_track.exists(),
        "the frames should have moved rather than been copied: {}",
        old_track.display()
    );

    // Assert: and the untouched cue's frame is there, under the new key, byte for byte —
    // the same picture, not a re-render that happens to look the same.
    let carried = cached_frame(state, 0);
    assert_eq!(
        carried.parent(),
        Some(new_track.as_path()),
        "the surviving frame should be filed under the rewritten file's key"
    );
    assert_eq!(
        fs::read(&carried).ok(),
        Some(untouched_bytes),
        "the cue nobody edited should keep the frame that was already rendered for it"
    );
}

/// A cue is retimed on the subtitle edit page with `t` and `h`/`l`, staged like any other edit, and
/// written by Ctrl+S.
///
/// Asserted through the whole workflow rather than on the nudge alone, because the feature is
/// the workflow: the keys have to reach the cue, the cue has to reach the staged edit, the
/// staged edit has to survive leaving the page, and the save has to put the new `-->` line —
/// and only that line — into the file on disk. Each half looks right on its own while the
/// file still says exactly what it always said.
///
/// The mode is exercised as a mode, too: `h`/`l` have a second meaning only while it is on,
/// and `Esc` has to give the first one back without also leaving the page.
#[test]
fn retiming_a_cue_should_stage_it_and_ctrl_s_should_write_it_to_the_file() {
    // Serialised against the other frame-cache scenarios: they share one cache and prune
    // each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "retiming_a_cue_should_stage_it_and_ctrl_s_should_write_it_to_the_file";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-cue-retime");
    write_media(
        &scratch.join("clip.mkv"),
        &MediaSpec::mkv()
            .size(320, 240)
            .duration(7.0)
            .audio(&["eng"]),
    );
    let sidecar = scratch.join("clip.eng.srt");
    fs::write(&sidecar, CACHED_CUES).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);
    // The background pass first: while it runs it owns the corner of the cue panel the
    // edited-count uses, so waiting it out is what makes the count assertable at all.
    wait_for_frames(&mut app);

    // Act / Assert: with the mode off, `l` is the cue list's sideways move and moves no
    // cue — so a stray press on a page nobody is retiming cannot edit the file.
    app.press(key(KeyCode::Char('j')));
    app.press(key(KeyCode::Char('l')));
    assert!(
        !app.app.has_unsaved_cue_edits(),
        "sideways movement should not be an edit"
    );

    // Act / Assert: into the mode, and `r` undoes a burst of nudges in one press — a cue
    // back at the timing the file gives it is staged as nothing at all.
    app.press(key(KeyCode::Char('t')));
    for _ in 0..3 {
        app.press(key(KeyCode::Char('h')));
    }
    assert!(
        app.app.has_unsaved_cue_edits(),
        "a nudge should stage like any other edit"
    );
    app.press(key(KeyCode::Char('r')));
    assert!(
        !app.app.has_unsaved_cue_edits(),
        "r should put the cue back to the file's timing, which is not an edit"
    );

    // Act: four steps later — 0.20s.
    for _ in 0..4 {
        app.press(key(KeyCode::Char('l')));
    }

    // Assert: staged, and the page says so — the shift on the timeline's title, the count
    // on the cue panel's border.
    assert!(
        app.app.has_unsaved_cue_edits(),
        "a nudge should stage like any other edit"
    );
    app.pump();
    let screen = app.screen();
    assert!(
        screen.contains("+0.20s") && screen.contains("1 edited"),
        "the page should show how far the cue moved and say it is unwritten:\n{screen}"
    );

    // Act / Assert: `Esc` gives `h`/`l` back without leaving the page or raising the
    // question about the edits.
    app.press(key(KeyCode::Esc));
    assert_eq!(app.app.dialog, None, "Esc should not ask anything yet");
    assert_eq!(
        app.app.layer,
        Layer::SubtitleEdit,
        "Esc should take the mode rather than the page"
    );

    // Act: write it.
    app.process_all();

    // Assert: the reader is still on the subtitle edit page, on the cue they moved.
    app.wait_until("the subtitle edit page to come back", |app| {
        app.layer == Layer::SubtitleEdit
            && app
                .subtitle_edit
                .as_ref()
                .is_some_and(|state| !state.cues.is_empty())
    });
    assert_eq!(
        app.app.subtitle_edit.as_ref().unwrap().selected,
        1,
        "the cursor should come back to the cue that was retimed"
    );

    // Assert: the file carries the new timing, the cue's words are untouched, and the cue
    // nobody moved is exactly as it was — including its timing, which a rewrite of the
    // whole file has every opportunity to round.
    let written = fs::read_to_string(&sidecar).expect("the sidecar should still be there");
    assert!(
        written.contains("00:00:03,200 --> 00:00:04,200"),
        "the save should write the retimed cue:\n{written}"
    );
    assert!(
        written.contains("Second line"),
        "the save should leave the cue's words alone:\n{written}"
    );
    assert!(
        written.contains("00:00:01,000 --> 00:00:02,000\nFirst line"),
        "the save should leave the other cue alone:\n{written}"
    );
    assert!(
        !app.app.has_unsaved_cue_edits(),
        "a written edit should stop being unsaved work"
    );

    // Act: move another cue and this time answer the leave question with "discard".
    app.press(key(KeyCode::Char('t')));
    app.press(key(KeyCode::Char('H')));
    assert!(
        app.app.has_unsaved_cue_edits(),
        "the second nudge should stage like the first"
    );
    app.press(key(KeyCode::Esc));
    app.press(key(KeyCode::Esc));
    assert_eq!(app.app.dialog, Some(Dialog::ConfirmLeaveCues));
    app.press(key(KeyCode::Char('l')));
    app.press(key(KeyCode::Enter));

    // Assert: the page is closed and the shift is gone rather than travelling with the file
    // to the next Ctrl+S, where it would be written from a page nobody is looking at.
    assert_eq!(app.app.layer, Layer::Streams, "discarding should leave");
    assert!(
        !app.app.has_track_edits(),
        "discarding should take the staged timing with it"
    );
    let written = fs::read_to_string(&sidecar).expect("the sidecar should still be there");
    assert!(
        written.contains("00:00:03,200 --> 00:00:04,200"),
        "a discarded nudge should never reach the file:\n{written}"
    );
}

/// The cache root, under the `XDG_CACHE_HOME` the harness redirects.
fn frames_root() -> PathBuf {
    PathBuf::from(std::env::var("XDG_CACHE_HOME").expect("the harness redirects the cache"))
        .join("reel-tui")
        .join("preview_frames")
}

/// Where one media's frames live: a directory of its own, which is the unit the cache
/// keeps or evicts.
fn track_dir(media_key: &str) -> PathBuf {
    frames_root().join(media_key)
}

/// Where the page's cached frame for one cue lives.
///
/// Through `frame_target` rather than by keying the cue directly, because a frame is a
/// picture of the whole screen: the key covers every cue burned into it, and asking the page
/// is the only way to get the same answer the worker did.
fn cached_frame(state: &reel_tui::subtitle_edit::SubtitleEditState, cue_index: usize) -> PathBuf {
    let target = state
        .frame_target(cue_index)
        .unwrap_or_else(|| panic!("cue {cue_index} should have a target"));
    frame_path(&state.frames.key(&target.cue, &target.on_screen))
}

/// Where a cached frame lives.
fn frame_path(key: &(String, String)) -> PathBuf {
    track_dir(&key.0).join(format!(
        "{}.{}",
        key.1,
        reel_tui::framecache::FRAME_EXTENSION
    ))
}

/// Opens the subtitle edit page on the sidecar track and waits for its cues.
fn open_sidecar_edit_page(app: &mut Harness) {
    let row = app
        .app
        .track_rows()
        .iter()
        .position(|track| *track == TrackRef::Sidecar(0))
        .expect("the sidecar should have a track row");
    app.select_track_row(row);
    app.press(key(KeyCode::Char('c')));
    assert_eq!(app.app.layer, Layer::SubtitleEdit, "c should open the page");
    app.wait_until("the sidecar's cues to be read", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| !state.cues.is_empty())
    });
}

/// Pumps until the background pass is done, answering whether its count was ever drawn.
///
/// Whether the cue panel's border is carrying the background pass's `[done/total]` count.
///
/// Read off the border rather than off a phrase, because that is where the count lives: the
/// status row is left for the messages about what the reader is doing right now.
fn warm_count_pattern(screen: &str) -> bool {
    screen.lines().any(|line| {
        line.split_once(" Cues ").is_some_and(|(_, title)| {
            title
                .split_once('[')
                .and_then(|(_, rest)| rest.split_once(']'))
                .is_some_and(|(count, _)| count.contains('/'))
        })
    })
}

fn warm_count_drawn(app: &Harness) -> bool {
    warm_count_pattern(&app.screen())
}

/// Its own loop rather than `wait_until`, because what is being watched is the screen the
/// pass paints on the way past, not only the state it ends in.
fn wait_for_frames(app: &mut Harness) -> bool {
    let started = Instant::now();
    let mut counted = false;
    loop {
        app.pump();
        counted |= warm_count_drawn(app);
        if app
            .app
            .subtitle_edit
            .as_ref()
            .is_some_and(|state| state.warm == WarmState::Done)
        {
            return counted;
        }
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "timed out waiting for the background frame pass:\n{}",
            app.screen()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One cue, sitting in the white stretch of a clip that turns from black to white two
/// seconds in — so the cue's own still is a bright frame and the black stretch it says
/// nothing about is only reachable with the timeline cursor.
const LONE_LATE_CUE: &str = "1\n00:00:03,000 --> 00:00:04,000\nFIRST LINE\n\n";

/// The timeline cursor shows a moment the cue list does not point at.
///
/// Everything else this page can draw is anchored to a cue: the still lands on the moment a
/// cue comes in, and a playback covers a cue's span. The question "where does this line
/// actually belong" is answered by a moment no cue names, so `Ctrl+J` hands the cursor to
/// the timeline and `h`/`l` walk it through the media with the preview pane following.
///
/// Asserted on the picture rather than on the state, because every layer short of the pixels
/// agrees whether or not the grab really moved: the cue list is untouched, the frame cache is
/// untouched, and the request differs only in one `-ss`. The clip is black for its first two
/// seconds and white after, and its one cue sits in the white stretch — so walking the cursor
/// back to 0:00, a moment with no cue on it at all, must turn the pane black where the cue's
/// own still is white.
#[test]
fn the_timeline_cursor_should_preview_a_moment_no_cue_points_at() {
    // Serialised against the other frame-cache scenarios: they share one cache and prune
    // each other's tracks — see `harness::frame_cache_lock`.
    let _frame_cache = harness::frame_cache_lock();
    let test = "the_timeline_cursor_should_preview_a_moment_no_cue_points_at";
    require_tools(test, &["ffmpeg:libx264"]);

    let scratch = Scratch::new("subtitle-edit-cursor");
    write_shot_change_media(&scratch.join("clip.mkv"));
    fs::write(scratch.join("clip.eng.srt"), LONE_LATE_CUE).unwrap();

    let mut app = Harness::start(scratch);
    app.open("clip.mkv");
    open_sidecar_edit_page(&mut app);

    // Arrange: the cue's own still, which is three seconds in and so a white frame.
    app.wait_until("the selected cue's frame", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.frame().is_some())
    });
    let cue_frame = brightest_preview_shade(&app);
    assert!(
        cue_frame > 192,
        "the cue at 0:03 sits in the clip's white stretch, so its still should be bright, but \
         the brightest shade drawn was {cue_frame}:\n{}",
        app.screen()
    );

    // Act: the cursor into the timeline, then one leap back — five seconds from the cue's
    // own moment, which the floor of the media clamps to 0:00.
    app.press(ctrl('j'));
    app.press(key(KeyCode::Char('H')));

    // Assert: the page says where the cursor is, in the timeline's own title.
    app.wait_until("the moment the cursor is on to be drawn", |app| {
        app.subtitle_edit
            .as_ref()
            .is_some_and(|state| state.scrub_frame().is_some())
    });
    let screen = app.screen();
    assert!(
        screen.contains("▼ 00:00:00.0"),
        "the timeline should say the moment its cursor stands on:\n{screen}"
    );

    // Assert: and the pane really is showing that moment rather than the cue's — no cue is
    // on screen at 0:00, so this is the bare picture the clip opens on.
    let scrubbed = brightest_preview_shade(&app);
    assert!(
        scrubbed < 64,
        "the clip opens black and no cue is on screen there, so the cursor's frame should be \
         dark, but the brightest shade drawn was {scrubbed} against the cue still's \
         {cue_frame}:\n{screen}"
    );

    // Act: four presses of the fine step, which is the cue nudge's own fifty milliseconds
    // rather than the half second `l` moves — the scale for finding a frame inside a shot
    // once the coarse keys have found the shot.
    for _ in 0..4 {
        app.press(ctrl('l'));
    }
    app.pump();

    // Assert: two tenths on, not two whole seconds. The picture cannot tell the two scales
    // apart here — both land in the clip's black stretch — so the title is what says which
    // step was taken.
    let screen = app.screen();
    assert!(
        screen.contains("▼ 00:00:00.2"),
        "four fine steps from 0:00 should put the cursor two tenths in:\n{screen}"
    );

    // Act: the cursor back to the cue list.
    app.press(ctrl('k'));
    app.pump();

    // Assert: the cue's own still is back, and the timeline no longer carries a cursor.
    let screen = app.screen();
    assert!(
        !screen.contains('▼'),
        "the timeline should drop its cursor when the cue list takes it back:\n{screen}"
    );
    let returned = brightest_preview_shade(&app);
    assert!(
        returned > 192,
        "the preview should be showing the selected cue's still again, but the brightest \
         shade drawn was {returned}:\n{screen}"
    );
}

/// The brightest channel of any colour the preview pane painted.
///
/// `preview_shades` is as much of a decoded picture as `TestBackend` can be asked about, and
/// this clip is deliberately either black or white — so one number separates the two.
fn brightest_preview_shade(app: &Harness) -> u8 {
    app.preview_shades()
        .into_iter()
        .flat_map(|(red, green, blue)| [red, green, blue])
        .max()
        .unwrap_or(0)
}
