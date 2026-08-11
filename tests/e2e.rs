//! End-to-end regressions: the app driven through keypresses, running real
//! `ffprobe`/`ffmpeg` against real files.
//!
//! Excluded from `cargo test` (see `test = false` in `Cargo.toml`); run with
//! `cargo test --test e2e`.
//!
//! Every scenario here reproduces a failure that actually reached
//! `~/.cache/reel-tui/edit_errors.log` during real use. The existing unit tests all
//! passed while those bugs were live, because they either build `TrackEdits` by hand
//! (skipping the `App` seam where several of them originated) or use `ffv1`/
//! `pcm_s16le` fixtures too simple to express the codec/container conflicts that
//! actually broke. The failure each test reproduces is quoted in its comment.

// `tests/e2e.rs` is a crate root, so submodules would otherwise resolve against
// `tests/` and collide with any future sibling suite.
#[path = "e2e/fixtures.rs"]
mod fixtures;
#[path = "e2e/harness.rs"]
mod harness;

use std::fs;
use std::time::Duration;

use crossterm::event::KeyCode;
use fixtures::{MediaSpec, SubtitleSpec, write_media};
use harness::{
    Harness, Scratch, codec_names, key, languages, probe, require_tools, stream_indices_of_type,
};
use reel_tui::app::TrackRef;

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
    if !require_tools(test, &["ffmpeg"]) {
        return;
    }

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
    if !require_tools(test, &["ffmpeg"]) {
        return;
    }

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
    if !require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]) {
        return;
    }

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
    if !require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac", "ffmpeg:mov_text"]) {
        return;
    }

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
    if !require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac", "ffmpeg:mov_text"]) {
        return;
    }

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
    if !require_tools(test, &["ffmpeg:libx264"]) {
        return;
    }

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
    if !require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac", "ffmpeg:mov_text"]) {
        return;
    }

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
    if !require_tools(test, &["ffmpeg:libx264", "ffmpeg:aac"]) {
        return;
    }

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
