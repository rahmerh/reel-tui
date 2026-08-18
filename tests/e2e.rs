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

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;
use std::time::Duration;

use crossterm::event::KeyCode;
use fixtures::{
    MediaSpec, SubtitleSpec, write_media, write_media_with_chapter_and_attachment,
    write_vobsub_media,
};
use harness::{
    Harness, Scratch, codec_names, key, languages, probe, require_tools, stream_indices_of_type,
};
use reel_tui::app::{
    AudioSettingsField, ContainerSettingsField, Layer, SubtitleSettingsField, TrackRef,
    VideoSettingsField,
};
use reel_tui::cli::{HELP_TEXT, USAGE, VERSION_TEXT};
use reel_tui::edit::VideoRotation;

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
    assert_eq!(app.app.conflicting_paths(), [path.clone()]);
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

/// Three cues with a deliberate overlap between the first two, so the timing page has
/// to pack two lanes rather than one.
const OVERLAPPING_CUES: &str = "1\n00:00:00,500 --> 00:00:02,000\nOverlapping opener\n\n\
                                2\n00:00:01,500 --> 00:00:03,000\nOverlapping answer\n\n\
                                3\n00:00:04,000 --> 00:00:05,000\nClosing line\n\n";

const SIDECAR_CUES: &str = "1\n00:00:01,000 --> 00:00:02,000\nSidecar first\n\n\
                            2\n00:00:03,000 --> 00:00:04,000\nSidecar second\n\n";

/// The subtitle timing page reads a track's cues in the background and draws them.
///
/// One fixture carries both source shapes — an embedded `subrip` stream, which the
/// preview worker extracts with a real `ffmpeg`, and a `.srt` sidecar, which it reads
/// straight off disk — so a single build covers both prepare paths. The page is opened,
/// navigated and left through genuine keypresses, and leaving it has to take its scratch
/// directory with it.
#[test]
fn the_subtitle_timing_page_should_load_cues_for_embedded_and_sidecar_srt_tracks() {
    let test = "the_subtitle_timing_page_should_load_cues_for_embedded_and_sidecar_srt_tracks";
    require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]);

    let scratch = Scratch::new("subtitle-sync");
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
    assert_eq!(app.app.layer, Layer::SubtitleSync, "c should open the page");
    let workspace = app
        .app
        .subtitle_sync
        .as_ref()
        .expect("the page should be open")
        .workspace()
        .to_path_buf();
    app.wait_until("the embedded track's cues to be read", |app| {
        app.subtitle_sync
            .as_ref()
            .is_some_and(|state| !state.cues.is_empty())
    });

    let state = app.app.subtitle_sync.as_ref().unwrap();
    assert_eq!(state.cues.len(), 3, "every cue in the track should be read");
    assert_eq!(
        state.layout.lane_count, 2,
        "the two overlapping cues need a lane each"
    );
    let screen = app.screen();
    assert!(
        screen.contains("Overlapping opener") && screen.contains("00:00:00.5 → 00:00:02.0"),
        "the cue list should show cue text and timing:\n{screen}"
    );
    let selected = app.filled_selection();
    assert!(
        selected.contains("00:00:00.5 → 00:00:02.0") && selected.contains("Overlapping opener"),
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
        app.subtitle_sync
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
        app.app.subtitle_sync.as_ref().unwrap().frame_error,
        None,
        "a frame that drew should leave no failure behind"
    );

    // Navigating the list moves the selection, and only the selection — and the frame
    // follows it, rather than the previous cue's picture staying under the new cue.
    app.press(key(KeyCode::Char('j')));
    app.pump();
    assert_eq!(app.app.subtitle_sync.as_ref().unwrap().selected, 1);
    let screen = app.screen();
    let selected = app.filled_selection();
    assert!(
        selected.contains("00:00:01.5 → 00:00:03.0") && selected.contains("Overlapping answer"),
        "j should move the fill onto the second cue: {selected:?}\n{screen}"
    );
    assert!(
        screen.contains("Timeline (00:00:01.5 → 00:00:03.0)"),
        "the timeline's title should follow the selection:\n{screen}"
    );
    app.wait_until("a frame for the second cue", |app| {
        app.subtitle_sync
            .as_ref()
            .is_some_and(|state| state.frame().is_some())
    });

    // Leaving releases the page and its scratch directory.
    app.press(key(KeyCode::Esc));
    app.pump();
    assert_eq!(app.app.layer, Layer::Streams);
    assert!(app.app.subtitle_sync.is_none(), "Esc should close the page");
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
        app.subtitle_sync
            .as_ref()
            .is_some_and(|state| !state.cues.is_empty())
    });

    let state = app.app.subtitle_sync.as_ref().unwrap();
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

/// The timing page reads a track exactly as it is stored and converts nothing, so every
/// format that is not SubRip has to be turned away at the door rather than opening a
/// page that can never fill in. Covers a text format and a bitmap one; runs no ffmpeg
/// beyond building the fixtures.
#[test]
fn the_subtitle_timing_page_should_refuse_formats_other_than_srt() {
    let test = "the_subtitle_timing_page_should_refuse_formats_other_than_srt";
    require_tools(
        test,
        &["ffmpeg:libx264", "ffmpeg:aac", "seconv", "tesseract"],
    );

    let scratch = Scratch::new("subtitle-sync-refusal");
    write_media(
        &scratch.join("styled.mkv"),
        &MediaSpec::mkv()
            .audio(&["eng"])
            .subtitles(vec![SubtitleSpec::new("eng", "ass")]),
    );
    write_vobsub_media(&scratch.join("bitmap.mkv"), "eng");

    let mut app = Harness::start(scratch);
    // Alphabetical, because the file panel is walked downward from wherever the cursor
    // already is.
    for (file, format) in [("bitmap.mkv", "VobSub"), ("styled.mkv", "ASS")] {
        app.open(file);
        let subtitle_row = app.first_subtitle_row();
        app.select_track_row(subtitle_row);
        app.press(key(KeyCode::Char('c')));
        app.pump();

        assert_eq!(
            app.app.layer,
            Layer::Streams,
            "{file} should not have opened the timing page"
        );
        assert!(
            app.app.subtitle_sync.is_none(),
            "{file} should not have left page state behind"
        );
        let notice = app
            .app
            .notice
            .clone()
            .unwrap_or_else(|| panic!("{file} should have been refused with a reason"));
        assert!(
            notice.contains(format) && notice.contains("Only SRT is supported"),
            "the refusal should name the format it turned away: {notice:?}"
        );
        assert!(
            app.screen().contains("Only SRT is supported"),
            "the refusal should reach the screen:\n{}",
            app.screen()
        );
        app.press(key(KeyCode::Esc));
    }
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
    assert_eq!(
        app.app.active_batch.is_none(),
        true,
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
