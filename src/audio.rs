//! Sound for the timing page's scrub playback, and the clock the picture is driven from.
//!
//! The page's whole purpose is judging whether a subtitle lands when the speech does, so a
//! playback whose picture and sound disagree by a few tens of milliseconds does not merely
//! look sloppy — it reports a correctly timed cue as late, and once the page can *edit*
//! times that wrong reading gets written to the file. Everything in this module exists to
//! make that disagreement structurally impossible rather than merely small.
//!
//! The arrangement is one idea: **the sound is the clock**. Nothing here schedules a frame.
//! The audio device is asked where it has got to, and the frame index is derived from that
//! ([`frame_index_at`]). There is no second clock to drift against, so drift does not have
//! to be corrected — it cannot occur. A picture that arrives late is a picture that arrives
//! late; it is never a picture of the wrong moment.
//!
//! [`PlaybackClock`] mentions no audio library at all, which is what lets the part that has
//! to be right be tested directly, on a machine with no sound card and on a CI runner with
//! no device at all. [`SilentOutput`] is the same story from the other end: it is
//! *production* code for a user with no working output and for media with no audio track,
//! and the tests get it for free rather than getting a double built for them.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// What the sound has to be produced as, so `ffmpeg` can be told to emit exactly that.
///
/// Queried from the real device before the terminal is taken over, for the same reason
/// `Picker::from_query_stdio()` is: afterwards the answer is either unavailable or arrives
/// mixed into the alternate screen. Resampling in `ffmpeg` — which is doing a scale and a
/// burn anyway — is free next to resampling in the audio callback, where the cost lands on
/// the one thread that must never be late.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl OutputFormat {
    /// What a device that could not be asked is assumed to want.
    ///
    /// Only ever reached alongside [`SilentOutput`], where nothing plays it — but the
    /// `ffmpeg` command is built before the output is opened, so it still needs an answer.
    pub const FALLBACK: Self = Self {
        sample_rate: 48_000,
        channels: 2,
    };

    /// How long `samples` interleaved floats last.
    ///
    /// Interleaved, so the channel count divides out: a stereo buffer of 2048 floats is
    /// 1024 frames of sound.
    pub fn duration_of(self, samples: usize) -> Duration {
        let per_second = u64::from(self.sample_rate) * u64::from(self.channels.max(1));
        if per_second == 0 {
            return Duration::ZERO;
        }
        Duration::from_nanos(samples as u64 * 1_000_000_000 / per_second)
    }
}

/// Where the sound actually is, as opposed to where it was handed over.
///
/// **The distinction is the entire point.** A callback fills a buffer that the device plays
/// one period later, so counting the samples written to the device gives a clock that
/// *leads* the sound by the device's latency: constant, silent, and tens of milliseconds —
/// which is precisely the error this feature exists to let a user see, with nothing on
/// screen to reveal that the instrument itself is the thing that is out.
///
/// So the origin is anchored from what the device reports about *playback*, not about
/// delivery, and re-anchored on every callback — wall time then only ever interpolates
/// across a single buffer period, which is a millisecond or two.
#[derive(Debug)]
pub struct PlaybackClock {
    /// Fixed at construction so the origin can live in an atomic. An `Instant` is opaque
    /// and not storable; a nanosecond offset from a known one is.
    base: Instant,
    /// Nanoseconds after `base` at which the span's first sample is heard, or
    /// [`Self::UNANCHORED`] before the device has reported anything.
    origin_nanos: AtomicU64,
}

impl Default for PlaybackClock {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackClock {
    /// No callback has landed yet. A sentinel rather than a second atomic, so a reader
    /// cannot observe half of an update.
    const UNANCHORED: u64 = u64::MAX;

    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            origin_nanos: AtomicU64::new(Self::UNANCHORED),
        }
    }

    /// Records where the sound is, from inside the audio callback.
    ///
    /// `now` is when the callback ran, `lead` is how far ahead of that the buffer being
    /// filled will actually be heard, and `already_queued` is how much of the span was
    /// handed over before it. The first sample of this buffer is therefore heard at
    /// `now + lead` and sits `already_queued` into the span, which places the span's start
    /// at `now + lead - already_queued`.
    ///
    /// Saturating rather than signed: `lead` covers the backlog as well as the device
    /// period, so the subtraction cannot go negative on a device that reports honestly —
    /// and on one that does not, a clock pinned to the start of the span is a picture that
    /// waits, which is recoverable, where a wrapped one is a picture that jumps to the end.
    pub fn anchor(&self, now: Instant, lead: Duration, already_queued: Duration) {
        let heard_at = now
            .saturating_duration_since(self.base)
            .saturating_add(lead);
        let origin = heard_at.saturating_sub(already_queued);
        self.origin_nanos.store(nanos(origin), Ordering::Relaxed);
    }

    /// How far into the span the sound has got, or `None` before it has started.
    ///
    /// `None` is not an error: it is the gap between the stream being built and the device
    /// asking for its first buffer, and the page draws nothing rather than guessing during
    /// it. Guessing would mean starting the picture ahead of the sound, which is the one
    /// thing this module is for.
    pub fn position(&self, now: Instant) -> Option<Duration> {
        let origin = self.origin_nanos.load(Ordering::Relaxed);
        if origin == Self::UNANCHORED {
            return None;
        }
        Some(
            now.saturating_duration_since(self.base)
                .saturating_sub(Duration::from_nanos(origin)),
        )
    }
}

/// A `Duration` as nanoseconds, kept clear of the unanchored sentinel.
///
/// The clamp is unreachable — `u64` nanoseconds is five hundred years — and exists so the
/// sentinel is a value this function provably never returns rather than one it merely does
/// not happen to.
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(PlaybackClock::UNANCHORED - 1)
}

/// Which frame of the span belongs at `position`.
///
/// Truncating, not rounding: frame *n* is what was on screen from `n/fps` onwards, so the
/// frame for a moment is the last one that had already started. Rounding would show the
/// next frame for half of every frame period, which at 30 fps is a sixtieth of a second of
/// systematic lead — small, but in the one direction this page must not be wrong in.
pub fn frame_index_at(position: Duration, fps: u32) -> usize {
    if fps == 0 {
        return 0;
    }
    let frames = position.as_nanos() * u128::from(fps) / 1_000_000_000;
    usize::try_from(frames).unwrap_or(usize::MAX)
}

/// Somewhere the span's sound is going, and the clock that says where it has got to.
///
/// The trait is the seam that keeps every other module free of the audio library: `ui.rs`,
/// `sync.rs` and `app.rs` ask this where the sound is and never learn what is producing it.
/// Dropping it stops playback — the single stop path, so that every way of leaving the page
/// releases the device without each one having to remember to.
pub trait AudioOutput: Send + std::fmt::Debug {
    /// How far into the span the sound has got, or `None` before it has started.
    fn position(&self, now: Instant) -> Option<Duration>;
}

/// Playback with nothing to play it: no device, or media with no audio track.
///
/// **Production code, not a test double.** A user whose machine has no working output still
/// gets the slideshow, at the right rate, rather than a page that refuses — and a track
/// with no sound in it is the same situation arrived at from the other side. The tests and
/// the e2e harness take this path because it is the path a real user without a sound card
/// takes, not because it was built for them.
///
/// Wall time is the clock here, which is exactly the mistake [`PlaybackClock`] exists to
/// prevent everywhere else — and is correct here for the same reason it is wrong there:
/// there is no sound to be out of step with.
#[derive(Debug)]
pub struct SilentOutput {
    clock: Arc<PlaybackClock>,
}

impl Default for SilentOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl SilentOutput {
    pub fn new() -> Self {
        let clock = Arc::new(PlaybackClock::new());
        // Anchored at once rather than left for a callback that is never coming: with no
        // device there is nothing to wait for, and an unanchored clock would leave the
        // picture on the first frame forever.
        clock.anchor(Instant::now(), Duration::ZERO, Duration::ZERO);
        Self { clock }
    }
}

impl AudioOutput for SilentOutput {
    fn position(&self, now: Instant) -> Option<Duration> {
        self.clock.position(now)
    }
}

/// Somewhere to send a span's sound, falling back rather than failing.
///
/// A machine with no working audio output still gets the slideshow — at the right rate,
/// because the clock does not depend on there being any sound to hear. Refusing to play at
/// all would be the wrong trade: most of what this page shows is in the picture, and a user
/// without a sound card would lose the whole feature to a device they already know they do
/// not have.
pub fn open() -> Box<dyn AudioOutput> {
    Box::new(SilentOutput::new())
}

#[cfg(test)]
mod tests {
    use kernal::prelude::*;

    use super::*;

    /// **The regression test this module exists for.** A clock built by counting the
    /// samples handed to the device reports the span's start at the instant the first
    /// buffer was *written*; the sound of it is not heard until one device period later.
    /// Against such a clock the picture runs ahead of the sound by that period, silently
    /// and constantly — so every subtitle reads as late by an amount the user would then
    /// "correct" into the file.
    ///
    /// Fails outright against `position = already_queued + elapsed`: that reports 150 ms
    /// here where the sound has been audible for 50.
    #[test]
    fn anchor_should_place_position_zero_behind_the_device_buffer() {
        // Arrange: the first buffer is handed over at `start` and reaches the speakers
        // 100 ms later, with nothing queued ahead of it.
        let clock = PlaybackClock::new();
        let start = Instant::now();
        clock.anchor(start, Duration::from_millis(100), Duration::ZERO);

        // Act / Assert: nothing has been heard yet at the moment it was written…
        assert_that!(clock.position(start)).is_equal_to(Some(Duration::ZERO));
        // …nor until the buffer actually reaches the device.
        assert_that!(clock.position(start + Duration::from_millis(99)))
            .is_equal_to(Some(Duration::ZERO));

        // Assert: and afterwards the position is how long it has been *audible*, not how
        // long ago it was written.
        assert_that!(clock.position(start + Duration::from_millis(150)))
            .is_equal_to(Some(Duration::from_millis(50)));
    }

    /// The origin is re-pinned on every callback, so wall time only ever interpolates
    /// across one buffer period — but re-pinning must land on the *same* origin, or the
    /// picture would step backwards and forwards around it once per period.
    #[test]
    fn re_anchoring_as_buffers_are_queued_should_keep_the_same_origin() {
        // Arrange: 100 ms of latency, and a callback every 20 ms handing over 20 ms of
        // sound. The lead covers the growing backlog, which is what keeps the origin still.
        let clock = PlaybackClock::new();
        let start = Instant::now();
        clock.anchor(start, Duration::from_millis(100), Duration::ZERO);
        let origin = clock.position(start + Duration::from_secs(1));

        // Act / Assert
        for buffer in 1..5 {
            let step = Duration::from_millis(20 * buffer);
            clock.anchor(start + step, Duration::from_millis(100), step);
            assert_that!(clock.position(start + Duration::from_secs(1))).is_equal_to(origin);
        }
    }

    /// A device that has genuinely fallen behind — the backlog outrunning the reported lead
    /// — must pin the clock to the start of the span rather than wrap around it. A picture
    /// that waits is recoverable; one that jumps to the end of the span is not.
    #[test]
    fn an_anchor_behind_the_clocks_own_base_should_pin_to_the_start_of_the_span() {
        // Arrange
        let clock = PlaybackClock::new();
        let start = Instant::now();

        // Act: more queued than has been reported as outstanding.
        clock.anchor(start, Duration::from_millis(10), Duration::from_secs(3600));

        // Assert: at the start of the span rather than an hour into it. Not exactly zero —
        // the clock's own base was taken a few nanoseconds before `start`, and that offset
        // only cancels out of the subtraction while the origin has room to hold it.
        let pinned = clock.position(start).expect("the clock has been anchored");
        assert_that!(pinned < Duration::from_millis(1)).is_true();

        // Assert: and running forwards from there, not wrapped around to the end.
        let later = clock
            .position(start + Duration::from_millis(500))
            .expect("the clock has been anchored");
        assert_that!(later - pinned).is_equal_to(Duration::from_millis(500));
    }

    /// The gap between building the stream and the device asking for its first buffer is
    /// real, and the page draws nothing during it. Guessing a position there would start
    /// the picture ahead of the sound, which is the failure this whole module is about.
    #[test]
    fn position_should_report_nothing_until_the_device_has_spoken() {
        // Arrange
        let clock = PlaybackClock::new();

        // Act / Assert
        assert_that!(clock.position(Instant::now())).is_none();
        clock.anchor(Instant::now(), Duration::ZERO, Duration::ZERO);
        assert_that!(clock.position(Instant::now()).is_some()).is_true();
    }

    /// Frame *n* is what was on screen from `n/fps` onwards, so a moment belongs to the
    /// last frame that had already started. Rounding instead would put the next frame up
    /// for half of every frame period — a systematic lead, in the one direction a timing
    /// instrument must not be wrong in.
    #[test]
    fn frame_index_should_step_once_per_frame_period_and_never_early() {
        // Act / Assert: at 25 fps a frame is exactly 40 ms.
        assert_that!(frame_index_at(Duration::ZERO, 25)).is_equal_to(0);
        assert_that!(frame_index_at(Duration::from_millis(39), 25)).is_equal_to(0);
        assert_that!(frame_index_at(Duration::from_millis(40), 25)).is_equal_to(1);
        assert_that!(frame_index_at(Duration::from_millis(79), 25)).is_equal_to(1);
        assert_that!(frame_index_at(Duration::from_secs(4), 25)).is_equal_to(100);

        // Act / Assert: and a rate that does not divide a second evenly still lands on the
        // frame that had started, rather than a rounding of it.
        assert_that!(frame_index_at(Duration::from_millis(33), 30)).is_equal_to(0);
        assert_that!(frame_index_at(Duration::from_micros(33_333), 30)).is_equal_to(0);
        assert_that!(frame_index_at(Duration::from_micros(33_334), 30)).is_equal_to(1);
    }

    /// The config clamps the rate well above zero, so this is reachable only through a
    /// caller that has not been written yet — and dividing by it would panic rather than
    /// misbehave, which is not a failure mode a playback should be able to have.
    #[test]
    fn a_frame_rate_of_nothing_should_stay_on_the_first_frame() {
        // Act / Assert
        assert_that!(frame_index_at(Duration::from_secs(9), 0)).is_equal_to(0);
    }

    /// The span's sound is sliced out of one buffer by time, so the mapping from samples to
    /// duration has to divide the channels out — a stereo buffer of 2048 floats is 1024
    /// frames of sound, not 2048.
    #[test]
    fn a_format_should_measure_interleaved_samples_as_the_sound_they_hold() {
        // Arrange
        let stereo = OutputFormat {
            sample_rate: 48_000,
            channels: 2,
        };
        let mono = OutputFormat {
            sample_rate: 48_000,
            channels: 1,
        };

        // Act / Assert
        assert_that!(stereo.duration_of(96_000)).is_equal_to(Duration::from_secs(1));
        assert_that!(mono.duration_of(96_000)).is_equal_to(Duration::from_secs(2));
        assert_that!(stereo.duration_of(0)).is_equal_to(Duration::ZERO);
    }

    /// A device that reports nonsense must not divide by it. Unreachable from cpal, which
    /// does not offer a zero-channel or zero-rate configuration, and cheap enough to rule
    /// out rather than reason about.
    #[test]
    fn a_format_reporting_no_rate_should_measure_nothing_rather_than_panic() {
        // Arrange
        let broken = OutputFormat {
            sample_rate: 0,
            channels: 0,
        };

        // Act / Assert
        assert_that!(broken.duration_of(4096)).is_equal_to(Duration::ZERO);
    }

    /// A user with no sound card still gets the slideshow, at the right rate. An
    /// unanchored clock would leave the picture on the first frame forever, waiting for a
    /// callback that is never coming.
    #[test]
    fn a_silent_output_should_start_its_clock_without_waiting_for_a_device() {
        // Arrange
        let output = SilentOutput::new();
        let opened = Instant::now();

        // Act / Assert: running from the moment it was opened, and advancing with the wall.
        let started = output
            .position(opened)
            .expect("a silent output has nothing to wait for");
        assert_that!(started < Duration::from_millis(50)).is_true();
        let later = output
            .position(opened + Duration::from_millis(500))
            .expect("a silent output has nothing to wait for");
        assert_that!(later.saturating_sub(started)).is_equal_to(Duration::from_millis(500));
    }

    /// The page holds its output as a `Box<dyn AudioOutput>` and hands it between threads
    /// on the way in, so the fallback has to satisfy the same bounds the real device does.
    #[test]
    fn the_fallback_output_should_be_usable_wherever_a_device_would_be() {
        // Arrange
        let output: Box<dyn AudioOutput> = Box::new(SilentOutput::new());

        // Act
        let described = format!("{output:?}");

        // Assert
        assert_that!(described.as_str()).contains("SilentOutput");
        assert_that!(output.position(Instant::now()).is_some()).is_true();
    }

    /// The rate and channel count are handed to `ffmpeg` before the output is opened, so
    /// there has to be an answer even when there is no device to ask.
    #[test]
    fn the_fallback_format_should_be_something_ffmpeg_will_emit() {
        // Act / Assert
        assert_that!(OutputFormat::FALLBACK.sample_rate).is_equal_to(48_000);
        assert_that!(OutputFormat::FALLBACK.channels).is_equal_to(2);
        assert_that!(OutputFormat::FALLBACK.duration_of(96_000))
            .is_equal_to(Duration::from_secs(1));
    }
}
