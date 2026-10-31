//! player::native_stall — the watchdog for a native surround Opus feed the TV's decoder swallows
//! without a word.
//!
//! A pipeline that cannot take a native Opus track usually says so (`type=18 num=200`, which
//! `route::open_recovery` turns into a learned refusal). Some wider layouts do not: the Load is
//! accepted, a handful of access units are taken, and then neither a picture nor an error ever
//! comes (`docs/verification/2026-10-30-opus-tv.md`). [`Watch`] tells that stall from a slow server
//! by what OUR side holds: both AU queues have data (the demuxer is not the bottleneck) while the
//! pipeline has accepted nothing new for [`NATIVE_AUDIO_STALL_MS`]. The pump hands a [`Step::Fire`]
//! to the same recovery as the loud refusal (`OpenFailure::Stalled`).
//!
//! Pure state, no clock and no globals read here: the pump hands in a [`Probe`]. The value lives on
//! the `Engine`, so each Load starts a fresh watch. Upstream #1034.

/// How long the pipeline may accept no access unit, with both queues holding data and no picture
/// shown, before a native surround audio feed counts as swallowed. A clean native play shows its
/// first picture within about a second of the Load returning.
pub(super) const NATIVE_AUDIO_STALL_MS: u32 = 4_000;

/// What the pump knows this tick.
pub(super) struct Probe {
    /// The pump's millisecond clock.
    pub now: u32,
    /// Channel count of the natively fed Opus track; 0 when no Opus is fed natively. The stall was
    /// measured on Opus alone (`docs/verification/2026-10-30-opus-tv.md`).
    pub channels: u32,
    /// The pump may feed now: a viewer's Pause before the first picture closes the feed on
    /// purpose, and a closed feed accepts nothing.
    pub feed_open: bool,
    /// `sf_load` returned for this session's epoch.
    pub load_returned: bool,
    /// Pictures the pipeline has shown this session.
    pub frames: i32,
    /// Access units the pipeline has ACCEPTED this session (video and audio).
    pub accepted: i64,
    pub audio_queue_bytes: i64,
    pub video_queue_bytes: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    Quiet,
    /// First tick of a candidate stall; the pump logs it once.
    Armed,
    Fire {
        /// Milliseconds since the watch armed.
        armed_ms: u32,
        /// Milliseconds the accepted count has not moved.
        stalled_ms: u32,
        accepted: i64,
    },
}

#[derive(Debug, Default)]
pub(crate) struct Watch {
    armed: Option<Armed>,
    /// Fired, or a picture appeared: this session is done with the watchdog.
    spent: bool,
}

#[derive(Debug)]
struct Armed {
    at: u32,
    progress_at: u32,
    accepted: i64,
}

impl Watch {
    pub(crate) const fn new() -> Watch {
        Watch { armed: None, spent: false }
    }

    pub(super) fn step(&mut self, p: &Probe) -> Step {
        if self.spent {
            return Step::Quiet;
        }
        if p.frames > 0 {
            self.spent = true;
            self.armed = None;
            return Step::Quiet;
        }
        // Stereo is proven fine natively, so it is never learned from a stall.
        let candidate = p.channels > 2 && p.feed_open && p.load_returned && p.audio_queue_bytes > 0 && p.video_queue_bytes > 0;
        if !candidate {
            self.armed = None;
            return Step::Quiet;
        }
        match &mut self.armed {
            None => {
                self.armed = Some(Armed { at: p.now, progress_at: p.now, accepted: p.accepted });
                Step::Armed
            }
            Some(a) if a.accepted != p.accepted => {
                a.accepted = p.accepted;
                a.progress_at = p.now;
                Step::Quiet
            }
            Some(a) => {
                let stalled_ms = p.now.wrapping_sub(a.progress_at);
                if stalled_ms < NATIVE_AUDIO_STALL_MS {
                    return Step::Quiet;
                }
                let step = Step::Fire { armed_ms: p.now.wrapping_sub(a.at), stalled_ms, accepted: p.accepted };
                self.spent = true;
                self.armed = None;
                step
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(now: u32) -> Probe {
        Probe { now, channels: 6, feed_open: true, load_returned: true, frames: 0, accepted: 8, audio_queue_bytes: 200_000, video_queue_bytes: 1_400_000 }
    }

    #[test]
    fn a_stalled_surround_feed_arms_then_fires_after_the_window() {
        let mut w = Watch::new();
        assert_eq!(w.step(&probe(1_000)), Step::Armed);
        assert_eq!(w.step(&probe(2_000)), Step::Quiet);
        assert_eq!(w.step(&probe(1_000 + NATIVE_AUDIO_STALL_MS - 1)), Step::Quiet);
        assert_eq!(
            w.step(&probe(1_000 + NATIVE_AUDIO_STALL_MS)),
            Step::Fire { armed_ms: NATIVE_AUDIO_STALL_MS, stalled_ms: NATIVE_AUDIO_STALL_MS, accepted: 8 }
        );
        assert_eq!(w.step(&probe(60_000)), Step::Quiet, "once");
    }

    #[test]
    fn feed_progress_restarts_the_window() {
        let mut w = Watch::new();
        w.step(&probe(0));
        let mut p = probe(3_900);
        p.accepted = 9;
        assert_eq!(w.step(&p), Step::Quiet);
        p.now = 7_000;
        assert_eq!(w.step(&p), Step::Quiet, "3.1 s since the last accepted AU");
        p.now = 7_900;
        assert!(matches!(w.step(&p), Step::Fire { stalled_ms: 4_000, .. }));
    }

    #[test]
    fn an_empty_queue_is_a_slow_server_not_a_stall() {
        let mut w = Watch::new();
        w.step(&probe(0));
        let mut p = probe(10_000);
        p.video_queue_bytes = 0;
        assert_eq!(w.step(&p), Step::Quiet);
        let mut p = probe(20_000);
        p.audio_queue_bytes = 0;
        assert_eq!(w.step(&p), Step::Quiet);
        // The window restarts when the queues fill again.
        assert_eq!(w.step(&probe(21_000)), Step::Armed);
        assert_eq!(w.step(&probe(24_000)), Step::Quiet);
    }

    #[test]
    fn it_never_fires_after_a_picture() {
        let mut w = Watch::new();
        w.step(&probe(0));
        let mut p = probe(10_000);
        p.frames = 1;
        assert_eq!(w.step(&p), Step::Quiet);
        p.frames = 0; // a seek zeroes the count; the watch stays spent for the session
        assert_eq!(w.step(&p), Step::Quiet);
    }

    #[test]
    fn stereo_and_non_native_audio_are_never_watched() {
        for channels in [0, 1, 2] {
            let mut w = Watch::new();
            let mut p = probe(0);
            p.channels = channels;
            assert_eq!(w.step(&p), Step::Quiet);
            p.now = 30_000;
            assert_eq!(w.step(&p), Step::Quiet, "{channels} channels");
        }
    }

    #[test]
    fn nothing_arms_before_the_load_returned() {
        let mut w = Watch::new();
        let mut p = probe(0);
        p.load_returned = false;
        assert_eq!(w.step(&p), Step::Quiet);
        p.now = 30_000;
        assert_eq!(w.step(&p), Step::Quiet);
    }

    #[test]
    fn a_closed_feed_is_a_pause_not_a_stall() {
        let mut w = Watch::new();
        w.step(&probe(0));
        let mut p = probe(10_000);
        p.feed_open = false;
        assert_eq!(w.step(&p), Step::Quiet);
        assert_eq!(w.step(&probe(10_016)), Step::Armed, "resuming starts the window again");
    }
}
