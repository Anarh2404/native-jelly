//! C3: the ONE bounded audio-conversion recovery a play request may make before its first
//! picture, and the only place native audio refusals are learned from.
//!
//! [`AudioRecovery`] is a pure `state + event -> step` value: it reads no global and performs no
//! IO. The pump (`player::pump::recover_refused_audio`, the driver) reads the facts of a failed
//! open into [`OpenFacts`], executes the [`Step`] it gets back (the learner's write through
//! `catalog::session`, the conversion through [`super::convert_refused_audio`]) and reports what
//! happened next as the following events.
//!
//! **The owner is the request.** One value lives in `PlaybackSession::audio_recovery`, and
//! `apply_plan` installs a fresh one with every plan, so a recovery, a counted Load or a pending
//! proof can never cross into the next request. The capability narrowing a conversion needs is
//! the attempt's own (`PlaybackSession::attempt_caps`), never the boot snapshot: a refusal this
//! request learned reaches the next one only as a stored row.
//!
//! **The rung.** It sits after a pending HLS->Original rollback (whose HLS route the failed open
//! owes the viewer) and before the cold Auto Original's HLS contingency and the failure read-out
//! (`player::pump::open_failure_action`). It converts only the audio the pipeline was fed natively,
//! keeps the chosen track, the subtitle and the request's position (a pending seek included), and
//! copies the video whenever the route's contract permits (a Dolby Vision `no_video_copy` stays
//! an encode). It is taken at most once per request, never under Forced Direct Play (the viewer
//! asked not to convert), never after a picture, and only for audio outside the codecs every set
//! decodes ([`suspect`]) — converting AAC to AAC rescues nothing.
//!
//! **The video rung** (upstream #1065, #1077). A manual pick whose video the pipeline refuses before
//! its first picture (a 4K H.264 at `num=100`) gets one encode under the 1080p ceiling instead of
//! the read-out; so does an audio conversion refused with its video still copied, which keeps the
//! audio converted and so tries both facets. Only an explicit pipeline refusal that does not name
//! the audio reaches it. A cold Auto Original keeps its HLS contingency. A video facet proves
//! nothing and is never stored: each play of such a file on a refusing set costs one refused Load.
//!
//! **What it teaches** (upstream #1034/#1061, refreshed against `b3d2b66c0`):
//! * a refusal that NAMES the audio (`type=18 num=200`, or the feed's silent stall,
//!   [`OpenFailure::Stalled`]) is recorded at once, applied from the next
//!   play start ([`Record::Named`]); once per request, whichever rung follows, Force and a pending
//!   rollback included;
//! * a refusal that names nothing (any other number, or the synchronous `sf_load == 0`) is an
//!   ambiguous one: it teaches only through a PROOF ([`Record::Proof`]) — the conversion landed
//!   with the server stating the audio converted and the video copied, and that route's own Load
//!   (exactly one, no other in between) showed the first picture. Two counted proofs apply
//!   (`catalog::session::LEARNED_REFUSAL_PROOFS`);
//! * a timeout, a video failure, cancellation and a failed conversion teach nothing, and a
//!   transport, HTTP or other demux failure never reaches the rung (the pump consults it only for
//!   a pipeline refusal and a refused Opus header). An Opus header the demuxer refused
//!   ([`OpenFailure::HeaderUnsupported`]) is an unsupported SOURCE: converted, never recorded.

/// The refusal number LG's pipeline uses for "Audio Codec Not Supported".
pub(crate) const NAMED_AUDIO_REFUSAL: i32 = 200;

/// How an open failed before its first picture, as the pump's failure branches see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenFailure {
    /// The pipeline refused the Load: `type=18` with its number, or the synchronous `sf_load == 0`
    /// (`None`), which names nothing.
    Refused { num: Option<i32> },
    /// The Load budget ran out with the pipeline silent.
    TimedOut,
    /// The pipeline took the Load and a few access units of native surround Opus, then stopped
    /// accepting with both our queues full and no picture (`player::native_stall`). Names the
    /// audio as a `num=200` does.
    Stalled,
    /// The demuxer refused an Opus track whose header disagrees with the Load declaration
    /// (`Shared::native_audio_header_refused`): the decoder was never asked.
    HeaderUnsupported,
}

/// The audio the pipeline was fed natively: the Load's declared codec and the carried track's
/// channel count (`route::stream_achannels`, 0 when unknown).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FedAudio {
    /// Canonical id (`devcaps::audio_codec_id`).
    pub codec: String,
    pub channels: u32,
}

impl FedAudio {
    pub(crate) fn new(codec: &str, channels: i64) -> Self {
        Self {
            codec: nj_platform::devcaps::audio_codec_id(codec).into_owned(),
            channels: u32::try_from(channels).unwrap_or(0),
        }
    }
}

/// The audio a refusal may put under suspicion: outside the codecs every set decodes. A refusal
/// of AAC, AC-3 or E-AC-3 is not audio evidence this rung can act on — the conversion would hand
/// the pipeline the same codec — so it is neither converted nor learned.
pub(crate) fn suspect(audio: Option<&FedAudio>) -> Option<&FedAudio> {
    audio.filter(|a| !a.codec.is_empty() && !matches!(a.codec.as_str(), "aac" | "ac3" | "eac3"))
}

/// What the driver read at the failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenFacts {
    pub failure: OpenFailure,
    /// The natively fed audio; `None` when the route fed converted audio or none.
    pub audio: Option<FedAudio>,
    /// This Engine has presented a picture (`player::seen_frame`).
    pub seen_picture: bool,
    /// Forced Direct Play: the viewer asked not to convert.
    pub forced: bool,
    /// An HLS->Original trial holds a live encoder for exactly this outcome.
    pub pending_original: bool,
    /// The pipeline was fed the source's own video: a direct play or a remux, not an encode.
    pub video_native: bool,
    /// A cold Auto Original: the ladder's HLS contingency owes the viewer the next route.
    pub auto_original: bool,
}

impl OpenFacts {
    /// May a refusal put the VIDEO under suspicion? Only an explicit pipeline refusal that does not
    /// name the audio, before any picture, on a route the viewer did not forbid converting and with
    /// no other way back waiting.
    fn video_suspect(&self) -> bool {
        matches!(self.failure, OpenFailure::Refused { num } if num != Some(NAMED_AUDIO_REFUSAL))
            && !self.seen_picture && !self.forced && !self.pending_original && !self.auto_original
    }
}

/// A refusal to record (the driver's IO, fire-and-forget).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    /// The pipeline named the audio: applies from the next play start.
    Named(FedAudio),
    /// A clean conversion rescued an ambiguous refusal: one counted proof.
    Proof(FedAudio),
}

/// What the driver does about the failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Not this rung's: the existing ladder (rollback, Auto HLS, read-out) decides.
    Pass,
    /// Convert this audio, video copied where permitted, at the request's intended position.
    Convert(FedAudio),
    /// Encode the video under the 1080p ceiling, at the request's intended position. Any audio an
    /// earlier rung took out of the attempt stays converted, so this is the both-facets rung too.
    EncodeVideo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Step {
    pub record: Option<Record>,
    pub action: Action,
}

/// The `/decision` verdicts of the conversion's landing, as the server stated them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Lanes {
    pub video_copied: bool,
    pub audio_copied: bool,
}

impl Lanes {
    /// The landing changed the audio ALONE: converted audio, copied video.
    fn audio_only(self) -> bool {
        self.video_copied && !self.audio_copied
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    /// No conversion yet this request.
    #[default]
    Idle,
    /// The conversion was asked for. `proof` is the audio an ambiguous refusal may prove once the
    /// landing changed only the audio; `loads` counts Loads since the landing.
    Converting { audio: FedAudio, proof: Option<FedAudio>, landed: bool, loads: u32, video_copied: bool },
    /// The video encode was asked for: the last rung, which teaches nothing either way.
    EncodingVideo,
    /// The rung is used up for this request: a conversion played, failed or was proved.
    Spent,
}

/// One play request's audio recovery. See the module doc.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioRecovery {
    state: State,
    /// A named refusal was already handed to the driver for this request.
    named: bool,
}

impl AudioRecovery {
    /// A request's value before anything happened (`PlaybackSession::IDLE` is a `const`).
    pub(crate) const NEW: AudioRecovery = AudioRecovery { state: State::Idle, named: false };

    /// The open failed before a picture.
    pub(crate) fn on_failure(&mut self, facts: &OpenFacts) -> Step {
        let pass = |record| Step { record, action: Action::Pass };
        // A failure of the conversion's own route (or any later one) never earns a second
        // conversion, and drops a proof still pending.
        let copied_after_audio = matches!(self.state, State::Converting { video_copied: true, .. });
        if !matches!(self.state, State::Idle) {
            // ...except that a conversion refused with its video still copied has not tried the
            // video yet: the one both-facets encode.
            if copied_after_audio && facts.video_suspect() {
                self.state = State::EncodingVideo;
                return Step { record: None, action: Action::EncodeVideo };
            }
            self.state = State::Spent;
            return pass(None);
        }
        if facts.seen_picture {
            return pass(None);
        }
        let Some(audio) = suspect(facts.audio.as_ref()).cloned() else {
            if facts.video_native && facts.video_suspect() {
                self.state = State::EncodingVideo;
                return Step { record: None, action: Action::EncodeVideo };
            }
            return pass(None);
        };
        let (record, ambiguous) = match facts.failure {
            OpenFailure::Refused { num: Some(NAMED_AUDIO_REFUSAL) } | OpenFailure::Stalled => {
                let first = !std::mem::replace(&mut self.named, true);
                (first.then(|| Record::Named(audio.clone())), false)
            }
            OpenFailure::Refused { .. } => (None, true),
            OpenFailure::HeaderUnsupported => (None, false),
            OpenFailure::TimedOut => return pass(None),
        };
        if facts.forced || facts.pending_original {
            return pass(record);
        }
        self.state = State::Converting {
            proof: ambiguous.then(|| audio.clone()),
            audio: audio.clone(),
            landed: false,
            loads: 0,
            video_copied: false,
        };
        Step { record, action: Action::Convert(audio) }
    }

    /// The conversion's route was installed; `lanes` are the server's verdicts. The driver's
    /// reload onto it is the next Load.
    pub(crate) fn on_landed(&mut self, lanes: Lanes) {
        if let State::Converting { proof, landed, video_copied, .. } = &mut self.state {
            *landed = true;
            *video_copied = lanes.video_copied;
            if !lanes.audio_only() {
                *proof = None;
            }
        }
    }

    /// The conversion could not be built or its reload could not start: the rung is spent and the
    /// failure falls through to the rest of the ladder.
    pub(crate) fn on_conversion_failed(&mut self) {
        self.state = State::Spent;
    }

    /// A Load started on this request's route. The landing's own reload is the first; any other
    /// (a seek reload, a foreground restore) before the first picture drops a pending proof.
    pub(crate) fn on_load_started(&mut self) {
        if let State::Converting { proof, landed: true, loads, .. } = &mut self.state {
            *loads += 1;
            if *loads > 1 {
                *proof = None;
            }
        }
    }

    /// The first picture of this request's current Engine.
    pub(crate) fn on_first_picture(&mut self) -> Option<Record> {
        match &mut self.state {
            // A request that played without the rung keeps it for a later Engine's open.
            State::Idle | State::Spent => None,
            State::EncodingVideo => {
                self.state = State::Spent;
                None
            }
            State::Converting { proof, landed, loads, .. } => {
                let proof = (*landed && *loads == 1).then(|| proof.take()).flatten();
                self.state = State::Spent;
                proof.map(Record::Proof)
            }
        }
    }

    /// The audio a conversion of this request took out, while it is the attempt's live rung.
    #[cfg(test)]
    pub(crate) fn converting(&self) -> Option<&FedAudio> {
        match &self.state {
            State::Converting { audio, .. } => Some(audio),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn spent(&self) -> bool {
        self.state == State::Spent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus(channels: i64) -> Option<FedAudio> {
        Some(FedAudio::new("OPUS", channels))
    }

    fn failed(failure: OpenFailure, audio: Option<FedAudio>) -> OpenFacts {
        OpenFacts { failure, audio, seen_picture: false, forced: false, pending_original: false,
            video_native: true, auto_original: false }
    }

    const NAMED: OpenFailure = OpenFailure::Refused { num: Some(NAMED_AUDIO_REFUSAL) };
    const SHUT: OpenFailure = OpenFailure::Refused { num: Some(601) };
    const COPIED_VIDEO: Lanes = Lanes { video_copied: true, audio_copied: false };

    /// The happy path of a counted proof, from the failure to the picture.
    fn rescue(m: &mut AudioRecovery, failure: OpenFailure, lanes: Lanes, loads: u32) -> (Step, Option<Record>) {
        let step = m.on_failure(&failed(failure, opus(2)));
        m.on_landed(lanes);
        for _ in 0..loads {
            m.on_load_started();
        }
        (step, m.on_first_picture())
    }

    #[test]
    fn a_named_refusal_is_recorded_at_once_and_converted() {
        let mut m = AudioRecovery::default();
        let (step, proof) = rescue(&mut m, NAMED, COPIED_VIDEO, 1);
        assert_eq!(step.record, Some(Record::Named(FedAudio { codec: "opus".into(), channels: 2 })));
        assert_eq!(step.action, Action::Convert(FedAudio { codec: "opus".into(), channels: 2 }));
        assert_eq!(proof, None, "a named row needs no proof; it was already recorded");
        assert!(m.spent());
    }

    #[test]
    fn an_ambiguous_refusal_teaches_only_through_a_clean_rescue() {
        let mut m = AudioRecovery::default();
        let (step, proof) = rescue(&mut m, SHUT, COPIED_VIDEO, 1);
        assert_eq!(step.record, None, "601 names nothing");
        assert!(matches!(step.action, Action::Convert(_)));
        assert_eq!(proof, Some(Record::Proof(FedAudio { codec: "opus".into(), channels: 2 })));

        let sync = OpenFailure::Refused { num: None };
        let (_, proof) = rescue(&mut AudioRecovery::default(), sync, COPIED_VIDEO, 1);
        assert!(proof.is_some(), "the synchronous sf_load refusal is ambiguous too");
    }

    #[test]
    fn a_rescue_that_changed_more_than_the_audio_or_crossed_a_load_proves_nothing() {
        let encoded = Lanes { video_copied: false, audio_copied: false };
        assert_eq!(rescue(&mut AudioRecovery::default(), SHUT, encoded, 1).1, None, "video re-encoded");
        let untouched = Lanes { video_copied: true, audio_copied: true };
        assert_eq!(rescue(&mut AudioRecovery::default(), SHUT, untouched, 1).1, None, "audio not converted");
        assert_eq!(rescue(&mut AudioRecovery::default(), SHUT, COPIED_VIDEO, 2).1, None, "a second Load");
        assert_eq!(rescue(&mut AudioRecovery::default(), SHUT, COPIED_VIDEO, 0).1, None, "no reload of the landing");

        let mut m = AudioRecovery::default();
        m.on_failure(&failed(SHUT, opus(2)));
        m.on_load_started();
        m.on_landed(COPIED_VIDEO);
        assert_eq!(m.on_first_picture(), None, "a Load before the landing is not the landing's");
    }

    const VIDEO: OpenFailure = OpenFailure::Refused { num: Some(100) };
    const ENCODE: Step = Step { record: None, action: Action::EncodeVideo };
    const PASS: Step = Step { record: None, action: Action::Pass };

    /// A refused video gets one encode, and teaches nothing whether it plays or not.
    #[test]
    fn a_refused_video_gets_one_encode_and_teaches_nothing() {
        let aac = Some(FedAudio::new("aac", 2));
        let mut m = AudioRecovery::default();
        assert_eq!(m.on_failure(&failed(VIDEO, aac.clone())), ENCODE);
        assert_eq!(m.on_failure(&failed(VIDEO, aac.clone())), PASS, "one per request");
        let mut m = AudioRecovery::default();
        assert_eq!(m.on_failure(&failed(OpenFailure::Refused { num: None }, None)), ENCODE, "no audio at all");
        m.on_load_started();
        assert_eq!(m.on_first_picture(), None, "a video facet never proves");
        assert!(m.spent());
    }

    /// Everything else stays with the ladder.
    #[test]
    fn the_video_rung_answers_only_an_explicit_refusal_of_a_natively_fed_video() {
        let aac = || Some(FedAudio::new("aac", 2));
        let pass = |facts: OpenFacts| AudioRecovery::default().on_failure(&facts);
        assert_eq!(pass(failed(NAMED, aac())), PASS, "num=200 names the audio, not the video");
        assert_eq!(pass(failed(OpenFailure::TimedOut, aac())), PASS);
        assert_eq!(pass(failed(OpenFailure::Stalled, aac())), PASS);
        assert_eq!(pass(failed(OpenFailure::HeaderUnsupported, aac())), PASS);
        assert_eq!(pass(OpenFacts { video_native: false, ..failed(VIDEO, aac()) }), PASS, "already an encode");
        assert_eq!(pass(OpenFacts { seen_picture: true, ..failed(VIDEO, aac()) }), PASS);
        assert_eq!(pass(OpenFacts { forced: true, ..failed(VIDEO, aac()) }), PASS);
        assert_eq!(pass(OpenFacts { pending_original: true, ..failed(VIDEO, aac()) }), PASS);
        assert_eq!(pass(OpenFacts { auto_original: true, ..failed(VIDEO, aac()) }), PASS, "its HLS contingency");
    }

    /// An audio conversion refused with its video still copied tries both facets, once; one whose
    /// video was already encoded has nothing left to try.
    #[test]
    fn a_refused_conversion_with_copied_video_gets_one_both_facets_encode() {
        let mut m = AudioRecovery::default();
        m.on_failure(&failed(SHUT, opus(2)));
        m.on_landed(COPIED_VIDEO);
        m.on_load_started();
        assert_eq!(m.on_failure(&failed(SHUT, None)), ENCODE);
        assert_eq!(m.on_failure(&failed(SHUT, None)), PASS);

        let mut m = AudioRecovery::default();
        m.on_failure(&failed(SHUT, opus(2)));
        m.on_landed(Lanes { video_copied: false, audio_copied: false });
        assert_eq!(m.on_failure(&failed(SHUT, None)), PASS, "the video was already encoded");

        let mut m = AudioRecovery::default();
        m.on_failure(&failed(SHUT, opus(2)));
        m.on_landed(COPIED_VIDEO);
        assert_eq!(m.on_failure(&failed(NAMED, None)), PASS, "a refusal naming the audio is not the video's");
    }

    /// The silent stall names the audio as `num=200` does: recorded at once, and converted once.
    #[test]
    fn a_silent_stall_is_named_evidence_and_converts_once() {
        let mut m = AudioRecovery::default();
        let wide = FedAudio { codec: "opus".into(), channels: 6 };
        let step = m.on_failure(&failed(OpenFailure::Stalled, opus(6)));
        assert_eq!(step.record, Some(Record::Named(wide.clone())));
        assert_eq!(step.action, Action::Convert(wide));
        m.on_landed(COPIED_VIDEO);
        m.on_load_started();
        assert_eq!(m.on_first_picture(), None, "a named refusal needs no proof");
        assert_eq!(m.on_failure(&failed(OpenFailure::Stalled, opus(6))).action, Action::Pass);
    }

    #[test]
    fn one_conversion_per_request_and_a_failed_rescue_teaches_nothing() {
        let mut m = AudioRecovery::default();
        m.on_failure(&failed(SHUT, opus(2)));
        m.on_landed(COPIED_VIDEO);
        m.on_load_started();
        let again = m.on_failure(&failed(SHUT, opus(2)));
        assert_eq!(again, ENCODE, "the conversion's own refusal, video still copied, tries the video");
        assert_eq!(m.on_first_picture(), None, "and its pending proof is gone");
        assert_eq!(m.on_failure(&failed(NAMED, opus(2))).action, Action::Pass);

        let mut m = AudioRecovery::default();
        m.on_failure(&failed(SHUT, opus(2)));
        m.on_conversion_failed();
        assert_eq!(m.on_failure(&failed(SHUT, opus(2))).action, Action::Pass, "no retry loop");
        assert_eq!(m.on_first_picture(), None);
    }

    #[test]
    fn a_named_refusal_is_handed_over_once_per_request() {
        let mut m = AudioRecovery::default();
        let forced = OpenFacts { forced: true, ..failed(NAMED, opus(2)) };
        assert!(m.on_failure(&forced).record.is_some());
        assert_eq!(m.on_failure(&forced).record, None, "a repeated Load does not write again");
    }

    #[test]
    fn force_and_a_pending_rollback_record_a_named_refusal_and_never_convert() {
        for facts in [
            OpenFacts { forced: true, ..failed(NAMED, opus(2)) },
            OpenFacts { pending_original: true, ..failed(NAMED, opus(2)) },
        ] {
            let step = AudioRecovery::default().on_failure(&facts);
            assert!(matches!(step.record, Some(Record::Named(_))));
            assert_eq!(step.action, Action::Pass);
        }
        for facts in [
            OpenFacts { forced: true, ..failed(SHUT, opus(2)) },
            OpenFacts { pending_original: true, ..failed(SHUT, opus(2)) },
            OpenFacts { forced: true, ..failed(OpenFailure::HeaderUnsupported, opus(2)) },
        ] {
            assert_eq!(AudioRecovery::default().on_failure(&facts), Step { record: None, action: Action::Pass });
        }
    }

    #[test]
    fn an_unsupported_header_is_converted_but_never_learned() {
        let (step, proof) = rescue(&mut AudioRecovery::default(), OpenFailure::HeaderUnsupported, COPIED_VIDEO, 1);
        assert_eq!(step.record, None);
        assert!(matches!(step.action, Action::Convert(_)));
        assert_eq!(proof, None, "the decoder was never asked");
    }

    #[test]
    fn failures_that_are_not_the_audios_pass_to_the_ladder_untaught() {
        for (facts, why) in [
            (failed(OpenFailure::TimedOut, opus(2)), "a timeout names nothing"),
            (failed(NAMED, Some(FedAudio::new("aac", 2))), "AAC is decoded by every set"),
            (OpenFacts { video_native: false, ..failed(SHUT, Some(FedAudio::new("eac3", 6))) }, "nor E-AC-3"),
            (failed(NAMED, None), "converted audio was fed"),
            (OpenFacts { seen_picture: true, ..failed(NAMED, opus(2)) }, "after a picture"),
        ] {
            let mut m = AudioRecovery::default();
            assert_eq!(m.on_failure(&facts), Step { record: None, action: Action::Pass }, "{why}");
            assert_eq!(m, AudioRecovery::default(), "{why}: the rung stays available");
        }
    }

    #[test]
    fn an_unknown_width_and_dts_spelling_are_carried_as_stated() {
        let step = AudioRecovery::default().on_failure(&failed(NAMED, Some(FedAudio::new("dca", 0))));
        assert_eq!(step.record, Some(Record::Named(FedAudio { codec: "dts".into(), channels: 0 })));
        assert_eq!(FedAudio::new("opus", -1).channels, 0);
    }
}
