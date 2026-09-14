//! Pure signal-processing core for "auto sync" (`A` on the subtitle edit page): finding
//! the single time offset that best lines a track's cues up with where the file's own
//! audio actually has someone talking.
//!
//! No `App`, no `ratatui`, no subprocesses — everything here is a function over plain
//! data, the same rule [`crate::cue`] and [`crate::framecache`] follow, so the whole
//! algorithm is testable without `ffmpeg` or a terminal. [`crate::preview`] owns the one
//! subprocess this needs (decoding the file's audio to raw samples) and hands the result
//! to [`find_offset`].
//!
//! The technique is the same family `ffsubsync`/`alass` use: no transcription, no model —
//! an energy-based "is someone talking right now" signal cross-correlated against "is a
//! cue on screen right now". It answers one question, a single offset in milliseconds
//! (positive moves the cues later), the same shape `T`'s global retiming already applies
//! through `SubtitleEditState::shift_track_by`. There is deliberately no speed/stretch
//! correction: `T` has none either, and a constant offset from end to end is the failure
//! this is aimed at.

use std::time::Duration;

/// Speech energy is measured over frames this long — fine enough to place an onset
/// usefully, coarse enough that one frame's energy is a meaningful average rather than a
/// single sample's noise.
const FRAME: Duration = Duration::from_millis(20);

/// How long a gap between loud frames is bridged rather than split into two regions, so
/// a breath or a brief pause mid-sentence does not fragment one line of dialogue into
/// several. Purely a merging tolerance — it does not pad a region's reported boundary,
/// which stays exactly where the energy last cleared the threshold.
const HANGOVER: Duration = Duration::from_millis(300);

/// A region shorter than this is a noise blip rather than a word, and is dropped.
const MIN_REGION: Duration = Duration::from_millis(100);

/// A frame counts as speech once its energy clears the track's own noise floor — the
/// 20th percentile of every frame's energy — by this factor. Relative to the file's own
/// floor rather than a fixed level, since a quiet recording and a loud one otherwise need
/// different thresholds.
const THRESHOLD_FACTOR: f32 = 3.0;

/// The resolution the two signals are compared at. Fine enough to place an offset well
/// under a nudge step (`subtitle_edit::TIMING_STEP`, 50ms), coarse enough that a
/// feature-length file's correlation stays a background-thread-sized job.
const BIN: Duration = Duration::from_millis(50);

/// How far either side of zero the search looks. A subtitle sync error is a human
/// mistake — an early credits cut, a framerate mismatch's accumulated drift, a track for
/// the wrong release — and those land within a few minutes, never further; bounding the
/// search is also what keeps the correlation's cost proportional to the file rather than
/// unbounded.
const MAX_SHIFT: Duration = Duration::from_secs(180);

/// Below this fraction of the cue timeline actually landing on speech at the best offset,
/// the alignment is not trustworthy enough to apply. A wrong-language track, a mostly
/// silent scene, or a best offset pinned to the search boundary (the true one is likely
/// outside it) all show up as a low score here, and refusing is the point — a bad guess
/// applied silently is worse than no guess at all.
const MIN_CONFIDENCE: f32 = 0.2;

/// Short-time-energy voice activity detection over mono PCM samples.
///
/// Deliberately not spectral: this only asks "is there energy here at all", not "is it a
/// voice", which is what keeps it a few dozen lines with no model to ship — a burst of
/// music or noise registers exactly as speech would, and the cue timeline is what keeps
/// that from mattering, since the correlation is only ever scored against where cues
/// already are. `sample_rate` is the rate `samples` was decoded at; `0` or an empty
/// buffer yields no regions rather than dividing by it.
pub fn detect_speech_regions(samples: &[f32], sample_rate: u32) -> Vec<(Duration, Duration)> {
    if samples.is_empty() || sample_rate == 0 {
        return Vec::new();
    }
    let frame_len = ((FRAME.as_secs_f64() * f64::from(sample_rate)) as usize).max(1);
    let energies: Vec<f32> = samples
        .chunks(frame_len)
        .map(|chunk| {
            let sum_sq: f32 = chunk.iter().map(|sample| sample * sample).sum();
            (sum_sq / chunk.len() as f32).sqrt()
        })
        .collect();

    let mut sorted = energies.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let noise_floor = sorted[sorted.len() / 5];
    let threshold = (noise_floor * THRESHOLD_FACTOR).max(f32::EPSILON);
    let hangover_frames = ((HANGOVER.as_secs_f64() / FRAME.as_secs_f64()) as usize).max(1);

    let mut regions = Vec::new();
    let mut active: Option<(usize, usize)> = None; // (first loud frame, last loud frame)
    for (frame, &energy) in energies.iter().enumerate() {
        if energy >= threshold {
            active = Some(match active {
                Some((start, _)) => (start, frame),
                None => (frame, frame),
            });
        } else if let Some((start, last_loud)) = active
            && frame - last_loud > hangover_frames
        {
            regions.push((start, last_loud + 1));
            active = None;
        }
    }
    if let Some((start, last_loud)) = active {
        regions.push((start, last_loud + 1));
    }

    regions
        .into_iter()
        .map(|(start, end)| {
            (
                frame_time(start, frame_len, sample_rate),
                frame_time(end, frame_len, sample_rate),
            )
        })
        .filter(|(start, end)| end.saturating_sub(*start) >= MIN_REGION)
        .collect()
}

fn frame_time(frame: usize, frame_len: usize, sample_rate: u32) -> Duration {
    Duration::from_secs_f64((frame * frame_len) as f64 / f64::from(sample_rate))
}

/// Bins a set of half-open `[start, end)` intervals into a fixed-resolution boolean
/// series covering `[0, total)`. The one representation both "someone is talking" and "a
/// cue is on screen" are put into before they are compared, so the two are read bin for
/// bin at the same resolution regardless of how differently they were produced.
fn to_bins(intervals: &[(Duration, Duration)], total: Duration, resolution: Duration) -> Vec<bool> {
    let bin_count = ((total.as_secs_f64() / resolution.as_secs_f64()).ceil() as usize).max(1);
    let mut bins = vec![false; bin_count];
    for &(start, end) in intervals {
        let from =
            ((start.as_secs_f64() / resolution.as_secs_f64()).floor() as usize).min(bins.len());
        let to = ((end.as_secs_f64() / resolution.as_secs_f64()).ceil() as usize).min(bins.len());
        for bin in &mut bins[from..to] {
            *bin = true;
        }
    }
    bins
}

/// The offset (milliseconds, positive moves the cues later) that best lines `cues` up
/// with `speech`, or `None` when nothing in `[-MAX_SHIFT, MAX_SHIFT]` clears
/// [`MIN_CONFIDENCE`].
///
/// Cross-correlates the two interval sets at [`BIN`] resolution, scored on the *cue*
/// timeline's own "on" bins only — a track with dialogue over a third of the running time
/// only ever has a third of the bins to score against, and scoring against the whole
/// timeline would make a sparse track's honest alignment read as low confidence next to a
/// dense one's. Walking just the cue-on positions is also what keeps this cheap: the cost
/// is proportional to how much dialogue the track actually has, not to the length of the
/// file.
pub fn find_offset(
    speech: &[(Duration, Duration)],
    cues: &[(Duration, Duration)],
    total: Duration,
) -> Option<i64> {
    let speech_bins = to_bins(speech, total, BIN);
    let cue_bins = to_bins(cues, total, BIN);
    let cue_on: Vec<usize> = cue_bins
        .iter()
        .enumerate()
        .filter_map(|(i, &on)| on.then_some(i))
        .collect();
    if cue_on.is_empty() || speech_bins.is_empty() {
        return None;
    }
    let bin_ms = i64::try_from(BIN.as_millis()).ok()?;
    let max_bins = i64::try_from(MAX_SHIFT.as_millis() / BIN.as_millis())
        .ok()?
        .max(1);

    let mut best_offset = 0i64;
    let mut best_overlap = 0usize;
    for offset in -max_bins..=max_bins {
        let overlap = cue_on
            .iter()
            .filter(|&&i| {
                let shifted = i as i64 + offset;
                shifted >= 0 && speech_bins.get(shifted as usize).copied().unwrap_or(false)
            })
            .count();
        if overlap > best_overlap {
            best_overlap = overlap;
            best_offset = offset;
        }
    }

    let confidence = best_overlap as f32 / cue_on.len() as f32;
    if confidence < MIN_CONFIDENCE || best_offset.abs() >= max_bins {
        return None;
    }
    Some(best_offset * bin_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal::prelude::*;

    /// A low sample rate keeps the synthetic buffers small and the tests fast; the
    /// algorithm has no dependency on any particular rate.
    const RATE: u32 = 2_000;

    fn silence(duration: Duration) -> Vec<f32> {
        vec![0.0; (duration.as_secs_f64() * f64::from(RATE)) as usize]
    }

    fn tone(duration: Duration) -> Vec<f32> {
        vec![1.0; (duration.as_secs_f64() * f64::from(RATE)) as usize]
    }

    fn burst(before: Duration, tone_for: Duration, after: Duration) -> Vec<f32> {
        [silence(before), tone(tone_for), silence(after)].concat()
    }

    #[test]
    fn detect_speech_regions_should_return_nothing_for_silence() {
        let samples = silence(Duration::from_secs(1));
        assert_that!(detect_speech_regions(&samples, RATE)).is_empty();
    }

    #[test]
    fn detect_speech_regions_should_return_nothing_for_an_empty_or_rateless_buffer() {
        assert_that!(detect_speech_regions(&[], RATE)).is_empty();
        assert_that!(detect_speech_regions(&[1.0, 1.0, 1.0], 0)).is_empty();
    }

    #[test]
    fn detect_speech_regions_should_find_one_burst_in_silence() {
        let samples = burst(
            Duration::from_millis(500),
            Duration::from_millis(400),
            Duration::from_millis(500),
        );
        let regions = detect_speech_regions(&samples, RATE);
        assert_that!(regions.clone()).has_length(1);
        let (start, end) = regions[0];
        // Frame-quantised (20ms), so the region is found within one frame of the burst.
        assert_that!(start.as_millis()).is_less_than_or_equal_to(520);
        assert_that!(start.as_millis()).is_greater_than_or_equal_to(480);
        assert_that!(end).is_greater_than_or_equal_to(Duration::from_millis(880));
    }

    #[test]
    fn detect_speech_regions_should_bridge_a_gap_shorter_than_the_hangover() {
        let mut samples = burst(
            Duration::from_millis(200),
            Duration::from_millis(100),
            Duration::from_millis(100),
        );
        samples.extend(tone(Duration::from_millis(100)));
        samples.extend(silence(Duration::from_millis(200)));
        // Two 100ms bursts separated by 100ms of silence, well under the 300ms hangover,
        // so they read as one region rather than two.
        let regions = detect_speech_regions(&samples, RATE);
        assert_that!(regions).has_length(1);
    }

    #[test]
    fn detect_speech_regions_should_drop_a_blip_shorter_than_the_minimum_region() {
        let samples = burst(
            Duration::from_millis(500),
            Duration::from_millis(20),
            Duration::from_millis(500),
        );
        assert_that!(detect_speech_regions(&samples, RATE)).is_empty();
    }

    #[test]
    fn find_offset_should_recover_a_positive_injected_offset() {
        // A minute of media with speech from 10s to 40s, and cues that say the same
        // thing three seconds early — auto sync should say "move them 3s later".
        let speech = vec![(Duration::from_secs(10), Duration::from_secs(40))];
        let cues = vec![(Duration::from_secs(7), Duration::from_secs(37))];
        let offset = find_offset(&speech, &cues, Duration::from_secs(60));
        assert_that!(offset).is_equal_to(Some(3_000));
    }

    #[test]
    fn find_offset_should_recover_a_negative_injected_offset() {
        let speech = vec![(Duration::from_secs(10), Duration::from_secs(40))];
        let cues = vec![(Duration::from_secs(12), Duration::from_secs(42))];
        let offset = find_offset(&speech, &cues, Duration::from_secs(60));
        assert_that!(offset).is_equal_to(Some(-2_000));
    }

    #[test]
    fn find_offset_should_report_zero_for_an_already_aligned_track() {
        let speech = vec![(Duration::from_secs(10), Duration::from_secs(40))];
        let cues = vec![(Duration::from_secs(10), Duration::from_secs(40))];
        let offset = find_offset(&speech, &cues, Duration::from_secs(60));
        assert_that!(offset).is_equal_to(Some(0));
    }

    #[test]
    fn find_offset_should_refuse_when_nothing_correlates() {
        // Speech and cues sit so far apart that no shift within the search range brings
        // them anywhere near each other, so every offset scores zero overlap — the
        // confidence floor refuses it without the answer ever being pinned to the
        // search's edge, unlike `find_offset_should_refuse_a_true_offset_past_the_search_boundary`.
        let speech = vec![(Duration::from_secs(1_000), Duration::from_secs(1_001))];
        let cues = vec![(Duration::from_secs(0), Duration::from_secs(1))];
        assert_that!(find_offset(&speech, &cues, Duration::from_secs(1_200))).is_equal_to(None);
    }

    #[test]
    fn find_offset_should_refuse_with_no_cues_or_no_speech() {
        let total = Duration::from_secs(60);
        let some = vec![(Duration::from_secs(1), Duration::from_secs(2))];
        assert_that!(find_offset(&[], &some, total)).is_equal_to(None);
        assert_that!(find_offset(&some, &[], total)).is_equal_to(None);
    }

    /// A decode is never expected to hand back `NaN`, but nothing here can prove it won't —
    /// so the frame-energy sort falls back to treating an unorderable pair as equal rather
    /// than panicking on `partial_cmp`'s `None`.
    #[test]
    fn detect_speech_regions_should_not_panic_on_a_nan_sample() {
        let mut samples = tone(Duration::from_millis(200));
        samples[10] = f32::NAN;
        // Only that it returns rather than panics; what it reports for a buffer holding a
        // NaN is not a claim worth making.
        let _ = detect_speech_regions(&samples, RATE);
    }

    #[test]
    fn find_offset_should_refuse_a_true_offset_past_the_search_boundary() {
        // The true offset here (200s) is outside `MAX_SHIFT` (180s), so shifting the cues
        // by the most the search is allowed only ever gets them partway into the speech
        // region — a real, non-trivial overlap that still must not be reported, since it
        // sits pinned to the search's own edge rather than at an actual best answer.
        let speech = vec![(Duration::from_secs(200), Duration::from_secs(260))];
        let cues = vec![(Duration::from_secs(0), Duration::from_secs(60))];
        assert_that!(find_offset(&speech, &cues, Duration::from_secs(280))).is_equal_to(None);
    }
}
