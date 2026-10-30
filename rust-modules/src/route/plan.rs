//! The PURE half of the route split (spec §9, §15.2): functions of their arguments alone, plus
//! the plain data types they need. Nothing here reads a `static mut`, touches [`super::decision`]'s
//! `SESSION`/`PLAYER_CONTROL`/`PLAY_SLOT`/`ENCODER_CLEANUP`/`SCROBBLE_JOIN`/`TIMELINE_STOP_FENCE`/
//! `QUALITY`, or calls `task::spawn*` — that is what makes [`build_stream`] safe to run on the
//! resolve worker. Everything else in the former `route.rs` (session state, the synchronized
//! `PlayerControl`, PMS/native I/O, the encoder/scrobble/timeline machinery) lives in
//! [`super::decision`]. `ci/check-deps.sh`'s `wall` gate holds this file to zero
//! `Instant::now`/`SystemTime::now`/`.elapsed()`; a function that needs wall time is not pure and
//! belongs in `decision.rs`.

use crate::metadata::lang_matches;
use crate::catalog::ServerId;
use std::sync::atomic::Ordering;

use super::decision::{
    auto_bitrate_kbps, resolve_playqueue, ActiveEncoderState, AutomaticRouteIntent, PlayerControl,
    ENCODER_GENERATION,
};

/// One worker's right to observe or replace the active route. Both fields are required: `encoder`
/// addresses PMS, while `epoch` distinguishes semantic routes which intentionally reuse that
/// exact Streaming Resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RouteLease {
    pub(super) epoch: u64,
    pub(super) encoder: String,
}


impl RouteLease {
    pub(crate) fn encoder(&self) -> &str {
        &self.encoder
    }
}


/// Everything a media worker must still own before it may publish a route-affecting result.
/// `route` rejects same-id ABA, `engine_epoch` rejects a worker from an earlier Load,
/// `media_epoch` rejects evidence collected before an applied seek, and `applied_revision`
/// names the physical route contract this worker actually serves. Desired user edits deliberately
/// do not change this ticket until their PMS/native effect commits: a refusal must leave the
/// unchanged worker authorized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkerTicket {
    pub(super) route: RouteLease,
    pub(super) engine_epoch: u64,
    pub(super) media_epoch: u64,
    pub(super) applied_revision: u64,
}


impl WorkerTicket {
    pub(crate) fn encoder(&self) -> &str {
        self.route.encoder()
    }
}


/// Identity of one physical `sf_load` attempt inside a prepared route transaction. Attempts are
/// never reused: a late result from A cannot settle retry B even though both open the same URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RouteStartAttempt {
    pub(super) serial: u64,
    pub(super) attempt: u64,
}


impl RouteStartAttempt {
    #[cfg(all(test, feature = "hostsim"))]
    pub(crate) const fn fixture() -> Self {
        Self {
            serial: 1,
            attempt: 1,
        }
    }
}


pub(super) fn next_route_epoch(epoch: u64) -> u64 {
    let next = epoch.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}


pub(super) fn lease_of(active: &ActiveEncoderState) -> RouteLease {
    RouteLease {
        epoch: active.epoch,
        encoder: active.id.clone(),
    }
}


pub(super) fn next_generation(value: u64) -> u64 {
    let next = value.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}


pub(super) fn worker_ticket_of(control: &PlayerControl) -> WorkerTicket {
    WorkerTicket {
        route: lease_of(&control.active),
        engine_epoch: control.engine_epoch,
        media_epoch: control.media_epoch,
        applied_revision: control.applied_revision,
    }
}


pub(super) fn ticket_is_current(control: &PlayerControl, ticket: &WorkerTicket) -> bool {
    ticket == &worker_ticket_of(control)
}


pub(super) fn automatic_ticket(intent: &AutomaticRouteIntent) -> &WorkerTicket {
    match intent {
        AutomaticRouteIntent::OriginalToHls { ticket, .. }
        | AutomaticRouteIntent::HlsToOriginal { ticket, .. } => ticket,
    }
}


pub(super) fn next_encoder_generation() -> u64 {
    ENCODER_GENERATION.fetch_add(1, Ordering::Relaxed) + 1
}


/// The audio track a route has decided to carry, frozen at the moment of decision (issue #266's
/// vocabulary). Used as `Option<CarriedAudio>` everywhere: `None` means "server default, facts
/// unknown", and every reader fails CLOSED on it — an unknown track is never treated as
/// loudness-capable or immersive just because nobody has said otherwise.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct CarriedAudio {
    /// PMS `Stream.id` — the id this track is selected/burned by (`audioStreamID`,
    /// `PUT /library/parts`).
    pub(crate) sid: i64,
    /// The demuxer-facing ordinal (`metadata::Stream.index` order) — what `set_audio_track` feeds
    /// on a direct play. `-1` mirrors the pre-refactor "unknown" sentinel.
    pub(crate) ordinal: i32,
    pub(crate) codec: String,
    pub(crate) channels: i64,
    /// PMS 1.43.4+ Plex Pass: can the server honor `boostDialog`/`normalizeLoudness` for this
    /// track (issue #266; wire field is `plex::Stream::can_normalize_loudness`).
    pub(crate) can_normalize_loudness: bool,
    /// Dolby Atmos (`metadata::Stream::has_atmos`) — the Load payload's `contents.immersive`.
    pub(crate) immersive: bool,
}

impl CarriedAudio {
    /// The constructor for a track whose facts were read: a copy of a `metadata::Stream` this
    /// resolve actually fetched and looked at, never assembled field-by-field, so nothing can hand
    /// out capability facts for a track nobody read. (The other is [`named`](Self::named).)
    pub(crate) fn from_stream(s: &crate::metadata::Stream, ordinal: i32) -> Self {
        CarriedAudio {
            sid: s.id,
            ordinal,
            // Lowercased like every other codec this module compares (`audio_sel`, the payload):
            // PMS ids are lowercase already, and a stray capital must not make a recovery's Load
            // payload disagree with the direct play it restores.
            codec: s.codec.to_lowercase(),
            channels: s.channels,
            can_normalize_loudness: s.can_normalize_loudness,
            immersive: s.has_atmos(),
        }
    }

    /// A track the route NAMES (`sid`, e.g. a retry's or the session's earlier pick) whose stream
    /// this resolve never read — its track list was not fetched or does not contain it. Only the
    /// id is known: no codec, no channels, and every capability fact false, so every reader still
    /// fails closed. It exists so the playing session keeps reporting the id it asked for (the
    /// timeline's `audioStreamID`, a later rebuild's pick) exactly as before issue #266.
    pub(crate) fn named(sid: i64, ordinal: i32) -> Self {
        CarriedAudio {
            sid,
            ordinal,
            codec: String::new(),
            channels: 0,
            can_normalize_loudness: false,
            immersive: false,
        }
    }
}

/// The track a route carries, as [`CarriedAudio`] — the fetched stream whose id the route names on
/// the wire (`audioStreamID` / the PUT / the direct-play pick). `sid <= 0` (server default) or a
/// track the fetched list does not contain is `None`: facts unknown, and nothing is invented.
///
/// `immersive` survives only on a route that feeds the FILE's own audio (`feeds_source` — direct
/// play), for `Plan::immersive`'s reason: `contents.immersive` describes the elementary stream the
/// pipeline decodes, and on a remux or a transcode that is the server's output, not this track.
fn carried_track(
    tracks: &[crate::metadata::Stream],
    sid: i64,
    ordinal: i32,
    feeds_source: bool,
) -> Option<CarriedAudio> {
    let stream = tracks.iter().find(|t| sid > 0 && t.id == sid)?;
    let mut carried = CarriedAudio::from_stream(stream, ordinal);
    carried.immersive &= feeds_source;
    Some(carried)
}

/// The track a PLAN installs as `Session::cur_audio`: [`carried_track`] when the stream was read,
/// else [`CarriedAudio::named`] for a nonzero id the route still names on the wire. Unlike an
/// Original candidate (which stays `None`, so a recovery falls back to the source codec), the
/// playing session must keep the id it asked for even when nothing else about it is known.
fn plan_track(
    tracks: &[crate::metadata::Stream],
    sid: i64,
    ordinal: i32,
    feeds_source: bool,
) -> Option<CarriedAudio> {
    carried_track(tracks, sid, ordinal, feeds_source)
        .or_else(|| (sid != 0).then(|| CarriedAudio::named(sid, ordinal)))
}

/// Everything needed to restore Auto's zero-video-encode state after HLS. `url` is the cold-start
/// playback target; `probe_part` is the raw Part key used to bind runtime measurement and direct
/// playback to the exact live HLS Streaming Resource. `direct` says whether the Part itself is
/// playable or whether PMS must container-remux it while copying the video.
#[derive(Clone)]
pub(super) struct AutoOriginalCandidate {
    pub(super) url: String,
    pub(super) probe_part: String,
    pub(super) direct: bool,
    pub(super) vcodec: String,
    pub(super) fps: f64,
    pub(super) dovi: crate::metadata::Dovi,
    pub(super) dv_decision: crate::metadata::DvDecision,
    /// The audio track this candidate carries. `None` = server default, facts unknown (issue
    /// #266 fails closed on it: no enhancement is ever offered without a known, capable track).
    pub(super) audio: Option<CarriedAudio>,
    pub(super) subtitle_ordinal: Option<i32>,
}

impl AutoOriginalCandidate {
    /// Follow a mid-play audio pick on a Direct or Remux route (issue #266). These two methods are
    /// the ONLY writers of the candidate's audio and subtitle halves once it is installed.
    ///
    /// Why retarget instead of dropping, as the HLS family does: on Direct/Remux the candidate is
    /// also the way BACK from an enhanced remux (`EnhancementReleased`), so it must describe the
    /// track the viewer is hearing now — a release that restored the capture-time track would
    /// undo their pick. On HLS the candidate is only a recovery target and the old drop stays.
    ///
    /// The track is taken as a unit (sid, ordinal, codec, channels, capability, immersive), so the
    /// release's Load payload can never pair one track's ordinal with another's codec. `immersive`
    /// survives only on a direct candidate, for `carried_track`'s reason. `false` means the pick
    /// cannot be carried by this candidate — a direct candidate cannot feed a track the TV cannot
    /// decode — and the caller drops the candidate.
    pub(super) fn retarget_audio(&mut self, a: &CarriedAudio, direct_plays: bool) -> bool {
        if self.direct && !direct_plays {
            return false;
        }
        let mut carried = a.clone();
        carried.immersive &= self.direct;
        self.audio = Some(carried);
        true
    }

    /// Follow a mid-play subtitle pick (see [`retarget_audio`](Self::retarget_audio)). `ordinal`
    /// is `None` for Off and `Some(render ordinal)` for a pick; an external sidecar pick has no
    /// demuxer ordinal (`-1`), which is stored as `None` because `player::sidecar` draws it once
    /// the route is direct and `request_subtitle(-1)` never touches that renderer. A pick the
    /// client cannot draw would need a burn, which the Original route by definition does not do:
    /// `false`, and the caller drops the candidate.
    pub(super) fn retarget_subtitle(&mut self, ordinal: Option<i32>, client_renderable: bool) -> bool {
        if ordinal.is_some() && !client_renderable {
            return false;
        }
        self.subtitle_ordinal = ordinal.filter(|o| *o >= 0);
        true
    }
}


pub(super) fn source_probe_sample_outcome(
    sample: crate::curlio::ThroughputSample,
) -> crate::player::report::TraceOutcome {
    if sample.target_reached {
        crate::player::report::TraceOutcome::Succeeded
    } else {
        // A non-empty prefix is useful only as a right-censored observation. `curlio` currently
        // collapses the terminal deadline/read reason once bytes exist, so naming it successful
        // would be stronger than the evidence. Keep the trace honest until that result type grows
        // a terminal-cause field.
        crate::player::report::TraceOutcome::Inconclusive
    }
}


/// **Why [`HlsAbrControl::prime`] would not register a candidate encoder**, in the one distinction
/// the caller's backoff turns on. It maps straight onto `crate::abr::RejectCause` and is a
/// separate type only because `route` must not decide an ABR policy question — it reports which
/// exit it took, and `ff.rs` translates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrimeRefusal {
    /// The session moved underneath the request: the active encoder changed or the server client
    /// vanished. **Says nothing about the rung**, so it must not arm N11's backoff — the same
    /// reading `origin_changed` already gets one branch later.
    Session,
    /// The decision API completed without a usable decision: HTTP rejection, malformed success,
    /// or a transport failure. The typed request chain preserves each as non-deadline evidence;
    /// all three remain inconclusive about the rung and must not arm its backoff.
    Control,
    /// The caller-owned absolute snapshot actually stopped the PMS request. This is the only
    /// outcome eligible for a reserve retry; observing the clock after any other completed cause
    /// cannot manufacture it.
    Deadline,
    /// PMS was asked for this rung's ceiling and refused it. The one exit that IS about the
    /// candidate, and the one that should arm the backoff: re-proposing buys the same answer at
    /// the same price.
    Rung,
}


/// This playback's universal-transcoder spec, rebuilt from the module state (rk + session are
/// borrowed from the caller's locals; audio/subtitle ride the CURRENT selection) — so every
/// (re)start of the item's transcode carries identical params.
///
/// `contract` is an ARGUMENT rather than a read of [`quality`], for the same reason it always was
/// (back when it was four separate fields — `remux`, `no_video_copy`, `ceiling`, `delivery`):
/// [`build_stream`] runs on the resolve worker and must take it from [`ResolveEnv`], while
/// [`retranscode`] runs on the main thread and reads the live selection. A read inside here would
/// be a `static` touched from a worker. Folding the four (now five, with issue #266's
/// [`AudioEnhancements`](crate::catalog::AudioEnhancements)) into one [`EncodeContract`] argument is
/// what keeps every caller stating the whole shape at once rather than four/five positional bools
/// and options a reader has to keep straight by position.
#[allow(clippy::too_many_arguments)]
pub(super) fn transcode_spec<'a>(
    rk: &'a str,
    session: &'a str,
    encoder_session: &'a str,
    continues: &'a str,
    offset: crate::catalog::TranscodeOffset,
    aud: i64,
    sub: i64,
    contract: crate::catalog::EncodeContract,
    caps: &'a nj_platform::devcaps::Caps,
) -> crate::catalog::TranscodeSpec<'a> {
    crate::catalog::TranscodeSpec {
        rating_key: rk,
        session,
        encoder_session,
        continues,
        contract,
        audio_stream_id: aud,
        subtitle_stream_id: sub,
        offset,
        caps,
    }
}


pub(crate) use crate::catalog::session::{PlaybackQuality as Quality, DirectPlayMode, NextEpisodeMode, SkipInterval, InstantDpadSeeking, EpisodeNavigation, SubtitleSize, SubtitlePosition};


/// The ladder IN ORDER, best first. The ONE place row order lives, so the picker's index mapping
/// cannot drift from what was drawn (`appkit::more_menu`'s rule, and its bug).
pub(crate) const QUALITY_LADDER: [Quality; 7] = [
    Quality::Auto,
    Quality::Original,
    Quality::P1080High,
    Quality::P1080,
    Quality::P720,
    Quality::P720Low,
    Quality::P480,
];


/// The explicit support/readiness gate for automatic playback. The measured PMS contract,
/// segmented demux, per-encoder wire identity, prime/commit transaction and single-Load LG
/// resolution gate are all present. Keeping this named (instead of deleting it after launch)
/// preserves one fail-closed switch should a future protocol change invalidate that evidence.
pub(crate) const fn auto_quality_ready() -> bool {
    true
}


pub(super) fn quality_ladder_for(auto_ready: bool) -> &'static [Quality] {
    if auto_ready {
        &QUALITY_LADDER
    } else {
        &QUALITY_LADDER[1..]
    }
}


pub(super) fn supported_quality(q: Quality) -> Quality {
    if q == Quality::Auto && !auto_quality_ready() {
        Quality::Original
    } else {
        q
    }
}


/// **What the user's chosen ceiling allows a plan to ask for** — the same two flags
/// [`crate::catalog::link_policy`] returns, deliberately, so [`build_stream`] can compose the two by
/// AND and the stricter always wins. A relay link cannot be loosened by picking a high rung, and a
/// low rung is not rescued by a fast link.
///
/// PURE, and the whole routing half of this feature is here:
///
/// * **Original restricts nothing** — the migration regression gate.
/// * **Auto includes Original as its top state.** `auto_original` is true immediately on a
///   verified LAN and only after a bounded file-throughput measurement on a direct Remote. A
///   relay, an unknown link, or an inconclusive/slow Remote measurement selects encoded HLS.
/// * A source MEASURED under the rung keeps both fast paths. Picking "1080p · 8 Mbps" must not
///   send a 3 Mbit/s 720p episode to an encoder; there is nothing there to fix.
/// * Anything else loses BOTH — direct play *and* the remux, for the one reason `link_policy`
///   already states twice: they ship the same bytes at the same rate, one container apart, and
///   neither carries a cap the server could come in under. What survives is the re-encode, which
///   is the only flavor that can honour the ask at all.
///
/// **Unmeasured fails CLOSED** ([`crate::catalog::Ceiling::admits`] holds the full argument): `0` is
/// "the server did not say", and the only way to honour an explicit ask about a file you have not
/// measured is to route it where the server applies the bound for you. That is the opposite of
/// [`video_direct_plays`]'s unknown-passes rule, and deliberately so: a device bound is a
/// capability, a user ceiling is an instruction.
pub(super) fn quality_policy(
    q: Quality,
    auto_original: bool,
    src_kbps: i64,
    src_w: i64,
    src_h: i64,
) -> crate::catalog::LinkPolicy {
    if q == Quality::Auto {
        return if auto_uses_hls(q, auto_original) {
            crate::catalog::LinkPolicy {
                direct_play: false,
                remux: false,
            }
        } else {
            crate::catalog::LinkPolicy::UNRESTRICTED
        };
    }
    match q.ceiling() {
        None => crate::catalog::LinkPolicy::UNRESTRICTED,
        Some(c) if c.admits(src_kbps, src_w, src_h) => crate::catalog::LinkPolicy::UNRESTRICTED,
        Some(_) => crate::catalog::LinkPolicy {
            direct_play: false,
            remux: false,
        },
    }
}


pub(super) fn auto_uses_hls(q: Quality, auto_original: bool) -> bool {
    q == Quality::Auto && !auto_original
}


/// **Two ceilings mean the stricter one**, per flavor, and this is the only place the two are put
/// together. A ceiling can only ever REMOVE a flavor: a fast link cannot restore what a low rung
/// denied, and a high rung cannot restore what a relay denied.
///
/// A named function rather than two `&&`s inline at the decision site, so the composition the
/// tests grade is literally the composition [`build_stream`] runs — a re-implementation in a test
/// would agree with itself forever while the shipped path drifted.
pub(super) fn flavors_allowed(
    link: crate::catalog::LinkPolicy,
    quality: crate::catalog::LinkPolicy,
) -> crate::catalog::LinkPolicy {
    crate::catalog::LinkPolicy {
        direct_play: link.direct_play && quality.direct_play,
        remux: link.remux && quality.remux,
    }
}

/// The family of route a caller is BUILDING (or, mid-play, is on) — the only shape question the
/// Plex Pass audio enhancement (issue #266) asks. `Direct` = the raw Part; `Remux` = the
/// codec-preserving progressive-MKV copy; `Other` = every encoder rung, fixed quality, HLS and
/// relay. The enhancement decorates the Original route and nothing else (I5): M3 measured that on
/// a re-encode shape the params change nothing observable, because the audio is transcoded there
/// at baseline anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RouteFamily {
    Direct,
    Remux,
    Other,
}

/// How a subtitle on screen interacts with the Plex Pass audio enhancement (issue #266 I6,
/// reworked per M7 and the owner's "do it the Plex way" decision: PMS burns a subtitle in rather
/// than dropping it, exactly as official Plex's playback lib does).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SubtitleEffect {
    /// No subtitle on screen.
    None,
    /// An external file the CLIENT draws itself (`metadata::Stream::sidecar_renderable`) — fetched
    /// and rendered by `player::sidecar` entirely outside the transcoded stream, so an enhanced
    /// remux never touches it.
    Sidecar,
    /// An embedded (in-container) subtitle direct play renders from the demuxed stream — an
    /// enhanced remux DROPS it (M4); only a forced re-encode with `subtitles=burn` keeps it (M7).
    Embedded,
}

/// Everything [`enhancement_availability`] reads, gathered by the caller from whichever side of
/// the playback it stands on (the resolve worker's locals, or the live session). Borrowed so the
/// predicate can never be handed a copy that has drifted from the thing it describes.
#[derive(Clone, Copy)]
pub(super) struct EnhancementFacts<'a> {
    /// The server's Plex Pass state (`serverinfo::subscription_of`), captured at the request.
    pub(super) pass: crate::catalog::serverinfo::Subscription,
    /// The frozen non-enhanced Original route. `None` = Original was never feasible here (forced
    /// direct play, a fixed rung, relay, a non-Original MDE…), which is I5's whole exclusion list.
    pub(super) base: Option<&'a AutoOriginalCandidate>,
    /// The audio track the route carries. `None` = server default, facts unknown: treated the same
    /// as "not analyzed yet" — a plain reason, never a silent absence (Plex Pass is already Yes).
    pub(super) carried: Option<&'a CarriedAudio>,
    /// What a subtitle on screen, if any, does to the offer (I6).
    pub(super) subtitle_effect: SubtitleEffect,
    /// This playback's server already refused or ignored the params once.
    pub(super) refused: bool,
}

/// What the enhancement, once offered, actually asks the server for — the one fork the menu, the
/// resolve and the recovery path must never disagree about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EnhancementRoute {
    /// The ordinary uncapped progressive-MKV remux: no subtitle on screen, or an external one the
    /// client draws itself. A declared Dolby Vision source whose base layer IS self-displayable
    /// (P7/P8, `!dovi.base_layer_unusable()`) takes this route too: a server that copies the video
    /// keeps the `DolbyHdrInfo` the direct play would have declared ([`copied_video_dv`]); one
    /// that re-encodes the lane drops it.
    Remux,
    /// A forced re-encode carrying `subtitleStreamID=<id>&subtitles=burn` (M7): the only shape that
    /// keeps an EMBEDDED subtitle in view once the audio enhancement is on.
    Burn,
}

/// Why the toggle is disabled (drawn, dim, with a reason) rather than hidden or enabled. Distinct
/// from [`EnhancementAvailability::Hidden`], which — per the owner's decision — is reserved for the
/// ONE reason that stays a silent absence: no Plex Pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DisabledReason {
    /// The carried track has no `canNormalizeLoudness` yet (or the track itself is unknown) — the
    /// server has not analyzed this audio.
    NotAnalyzed,
    /// A Dolby Vision base layer that is not self-displayable (P5, `dovi.base_layer_unusable()`):
    /// no copy of it is ever made, so there is nothing the enhancement's remux could decorate.
    DolbyVisionUnusable,
    /// A declared Dolby Vision source with an embedded subtitle on screen: M7 measured PMS copying
    /// the DV video regardless of a burn request and silently dropping the subtitle — neither half
    /// of the ask is honoured, so it is never sent.
    DolbyVisionSubtitle,
    /// The live route is not Direct or Remux (a fixed quality rung, HLS, a relay…), or no Original
    /// candidate was ever computed for it (I5's exclusion list) — the enhanced route is always an
    /// uncapped Original.
    NotOriginalQuality,
    /// This playback's server already refused or ignored the params once.
    ServerRefused,
}

/// **What state is the Plex Pass audio enhancement in for a route of family `target`?** One
/// predicate for every caller — the resolve, recovery, and the menu — so "what the toggle looks
/// like" and "what the resolve does" can never disagree. Per the owner's decision, [`Hidden`] is
/// reserved for the ONE absence that stays silent (no Plex Pass); every other gate that used to
/// hide the rows now reports a [`DisabledReason`] instead.
///
/// [`Hidden`]: EnhancementAvailability::Hidden
pub(super) fn enhancement_availability(f: &EnhancementFacts, target: RouteFamily) -> EnhancementAvailability {
    use crate::catalog::serverinfo::Subscription;
    use EnhancementAvailability::{Disabled, Hidden, Offered};
    // I1/I2: `Unknown` fails closed exactly like `No` — an unknown server is not assumed to hold a
    // subscription it may not have, and with it hidden the app's URLs are byte-identical to a
    // build without the feature. The ONE state that stays a silent absence.
    if f.pass != Subscription::Yes {
        return Hidden;
    }
    if f.refused {
        return Disabled(DisabledReason::ServerRefused);
    }
    // I5: Direct or Remux only, and only when an Original candidate was actually computed for it
    // (forced direct play, a fixed rung, relay, a non-Original MDE… all read the same to a viewer:
    // "only at Original quality").
    if !matches!(target, RouteFamily::Direct | RouteFamily::Remux) {
        return Disabled(DisabledReason::NotOriginalQuality);
    }
    let Some(base) = f.base else {
        return Disabled(DisabledReason::NotOriginalQuality);
    };
    // I7: an unusable Dolby Vision base layer is never copied at all — nothing to decorate.
    if base.dovi.base_layer_unusable() {
        return Disabled(DisabledReason::DolbyVisionUnusable);
    }
    let dv_declared = base.dv_decision.presentation.declared().is_some();
    // M7: a declared DV source burning an embedded subtitle is refused by the server itself
    // (video copied regardless, subtitle silently dropped) — never attempted.
    if dv_declared && f.subtitle_effect == SubtitleEffect::Embedded {
        return Disabled(DisabledReason::DolbyVisionSubtitle);
    }
    // A known, capable carried track. `canNormalizeLoudness` is PMS 1.43.4's own per-stream
    // statement that it has the loudness analysis the DSP needs; unknown or absent reads the same
    // to a viewer as "not analyzed yet".
    let analyzed = f.carried.is_some_and(|a| a.can_normalize_loudness);
    if !analyzed {
        return Disabled(DisabledReason::NotAnalyzed);
    }
    Offered(match f.subtitle_effect {
        SubtitleEffect::Embedded => EnhancementRoute::Burn,
        SubtitleEffect::None | SubtitleEffect::Sidecar => EnhancementRoute::Remux,
    })
}

/// **What the menu draws for the toggle**, and (via [`EnhancementAvailability::Offered`]'s payload)
/// what turning it on would actually ask the server for. See [`enhancement_availability`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EnhancementAvailability {
    /// No Plex Pass — absent from the menu; no hint, no upsell (I1/I2).
    Hidden,
    /// Visible, toggle enabled.
    Offered(EnhancementRoute),
    /// Visible, toggle inert (dim), with a plain-language reason.
    Disabled(DisabledReason),
}

/// **Is the Plex Pass audio enhancement OFFERED (toggle enabled) for a route of family `target`?**
/// A thin projection of [`enhancement_availability`] for callers that only need the yes/no — the
/// resolve's contract building reads [`enhancement_route`] instead, since it also needs to know
/// WHICH route.
pub(super) fn enhancements_offered(f: &EnhancementFacts, target: RouteFamily) -> bool {
    enhancement_route(f, target).is_some()
}

/// The concrete route the enhancement would take here, or `None` when it is not offered — Hidden
/// and every `Disabled` reason collapse to `None` for this projection, since the resolve only ever
/// needs to know "should this be asked for", never why not.
pub(super) fn enhancement_route(f: &EnhancementFacts, target: RouteFamily) -> Option<EnhancementRoute> {
    match enhancement_availability(f, target) {
        EnhancementAvailability::Offered(route) => Some(route),
        EnhancementAvailability::Hidden | EnhancementAvailability::Disabled(_) => None,
    }
}

/// The ONLY source of `EncodeContract::audio`: the viewer's preference where the enhancement is
/// offered, `NONE` everywhere else. Every contract that reaches the wire passes through here, so
/// a preference can never leak onto a route the predicate refused.
pub(super) fn desired_audio(
    pref: crate::catalog::AudioEnhancements,
    offered: bool,
) -> crate::catalog::AudioEnhancements {
    if offered {
        pref
    } else {
        crate::catalog::AudioEnhancements::NONE
    }
}

/// Log + diag the server's refusal of an enhanced ask, from whichever call site first learns of
/// it. `context` is the site-specific tail after the shared "enhancement: refused/ignored by
/// server" opening, so every site keeps the exact log sentence it always had.
pub(super) fn note_enhancement_refused(context: &str, audio: crate::catalog::AudioEnhancements) {
    crate::player::log(&format!("enhancement: refused/ignored by server{context}"));
    crate::diag::event(crate::diag::schema::DiagEvent::EnhancementRefused {
        boost_dialog: audio.boost_dialog,
        normalize_loudness: audio.normalize_loudness,
    });
}

/// **Why a plan leaves without a URL on purpose**, as a typed verdict rather than a sentence.
///
/// Every arm is stored as a variant and worded only where it is read ([`PlayVerdict::text`]) — a
/// stored sentence would freeze one language into playback state that tests, logs and replays also
/// read. Jellyfin refuses with an enum code, not a sentence, so the server's refusal is typed too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlayVerdict {
    /// The server's refusal (`PlaybackInfo`'s `ErrorCode`, or no way to serve the item). Its
    /// technical code goes to the event log; the failure report never sends it.
    Server(crate::catalog::Refusal),
    /// Direct Play is Disabled, and this stream can only be played as the original.
    DirectPlayDisabled,
    /// Force Direct Play is on, and this is why the original cannot play.
    Forced(ForcedFailure),
}

/// The limitation a Force Direct Play verdict names. Each one carries the same recovery step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForcedFailure {
    NoOriginal,
    Container,
    Video,
    Audio,
    Unauthorized,
    /// The server could not be asked whether the original plays.
    OpenFailed,
    /// A later audio-track pick needs conversion.
    AudioNeedsConversion,
}

impl PlayVerdict {
    /// The read-out's sentence, from the catalog.
    pub(crate) fn text(&self) -> &str {
        use crate::catalog::Refusal;
        use nj_platform::i18n::msg;
        match self {
            Self::Server(why) => match why {
                Refusal::NotAllowed => msg::widgets_verdict_server_not_allowed(),
                Refusal::NoCompatibleStream => msg::widgets_verdict_server_no_compatible_stream(),
                Refusal::RateLimitExceeded => msg::widgets_verdict_server_rate_limited(),
                Refusal::NoMediaSource => msg::widgets_verdict_server_no_media_source(),
                Refusal::NoDeliveryMethod => msg::widgets_verdict_server_no_delivery(),
                Refusal::Unrecognized(_) => msg::widgets_verdict_server_refused(),
            },
            Self::DirectPlayDisabled => msg::widgets_verdict_direct_play_disabled(),
            Self::Forced(why) => match why {
                ForcedFailure::NoOriginal => msg::widgets_verdict_forced_no_original(),
                ForcedFailure::Container => msg::widgets_verdict_forced_container(),
                ForcedFailure::Video => msg::widgets_verdict_forced_video(),
                ForcedFailure::Audio => msg::widgets_verdict_forced_audio(),
                ForcedFailure::Unauthorized => msg::widgets_verdict_forced_unauthorized(),
                ForcedFailure::OpenFailed => msg::widgets_verdict_forced_open(),
                ForcedFailure::AudioNeedsConversion => msg::widgets_verdict_forced_audio_conversion(),
            },
        }
    }
}

/// Fresh opaque session id per playback. Reads the kernel UUID (the TV is Linux); falls
/// back to a ratingKey + monotonic-counter token if that read fails.
pub(super) fn new_sess(rk: &str) -> String {
    if let Ok(u) = std::fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let t = u.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(1);
    format!("nativejelly-{rk}-{}", CTR.fetch_add(1, Ordering::Relaxed))
}


/// The episode queued after the one now playing — everything the Up Next control draws AND
/// everything [`request_play`] needs to start it, so playing it costs no PMS round trip either.
///
/// It comes free with the `continuous=1` PlayQueue every playback already creates (see
/// [`crate::catalog::Client::create_play_queue`]); nothing here asks the server "what's next".
#[derive(Clone, Default)]
pub(crate) struct UpNext {
    pub(crate) rk: String,
    pub(crate) part: String,
    pub(crate) vcodec: String,
    pub(crate) acodec: String,
    pub(crate) show_title: String, // grandparentTitle
    pub(crate) ep_title: String,
    pub(crate) season: i64,
    pub(crate) index: i64,
    pub(crate) thumb: String,
    pub(crate) dur_ms: i64,
    pub(crate) resume_ms: i64,
}


/// Build the Up Next descriptor from a queue row. Episodes only: `continuous=1` on a movie
/// returns just the movie itself (verified live — total count 1), and "up next" is a show idea.
/// The gate belongs HERE, on the one-item control — the retained row list is deliberately not
/// episode-gated, because a queue list has to be able to show whatever the queue holds.
pub(super) fn up_next_of(r: &crate::catalog::QueueRow) -> Option<UpNext> {
    if r.kind != "episode" || r.rk.is_empty() {
        return None;
    }
    Some(UpNext {
        rk: r.rk.clone(),
        part: r.part.clone(),
        vcodec: r.vcodec.clone(),
        acodec: r.acodec.clone(),
        show_title: r.show_title.clone(),
        ep_title: r.title.clone(),
        season: r.season,
        index: r.index,
        thumb: r.thumb.clone(),
        dur_ms: r.dur_ms,
        resume_ms: r.resume_ms,
    })
}


/// Every piece of [`Session`] the resolve used to READ, captured on the main thread and passed by
/// value.
///
/// Making the worker WRITE-pure was not enough: it still cloned `machine_id` and `sess` — Strings
/// that `apply_plan` reassigns on every landing — so a superseded worker could clone a buffer as
/// it was being dropped (heap corruption on a device with no debugger), and read the two sids as
/// non-atomic i64s, which on armv7 is a tearable two-word load.
///
/// The `sid` is the same idea one step further out: it is not a static the worker could read, it is
/// a *function call* — `plex::client_opt()` — which is worse, because `Send` cannot see a function
/// call and a worker that resolves its own server therefore compiles clean and passes every test.
/// It is captured here, at the request, and every PMS call the worker makes is `client_for(sid)`.
#[derive(Clone, Default)]
pub(crate) struct ResolveEnv {
    /// WHICH SERVER this playback's item lives on — the scope for every server-local key the
    /// resolve then uses (`rk`, `Part.key`, `Stream.id`). Not "the current server" (see
    /// [`Session::cur_sid`]): captured on the main thread with everything else here, because the
    /// resolve worker must not read the current server itself.
    pub sid: ServerId,
    /// `machine_id`, but only when it was learned from `sid`'s own server (`machine_sid`);
    /// otherwise empty, so the worker re-asks rather than addressing a queue to the wrong machine.
    pub machine_id: String,
    pub audio_sid: i64,
    pub sub_sid: i64,
    /// A retry carries the viewer's explicit Off as well as a positive subtitle id.
    pub subtitle_override: Option<i64>,
    /// `audio_sid` is the VIEWER's pick (the Detail page's track chooser), not a session or retry
    /// carry: a track that cannot direct-play is then converted rather than swapped for a
    /// direct-playable sibling, as a pick in the player's own track menu is
    /// (`decision::commit_audio_selection`).
    pub audio_explicit: bool,
    /// the loaded detail's streams when it IS this item — saves the worker a GET
    pub cached_item: Option<crate::metadata::PlayingItem>,
    /// The user's pick off the quality ladder, captured at the press like everything else here.
    /// The worker must not call [`quality`] itself for the reason this struct exists: it reads a
    /// process-global the main thread can move while the resolve is in flight.
    pub quality: Quality,
    /// Captured with the quality preference; never re-read by a worker.
    pub direct_play_mode: DirectPlayMode,
    /// The SOURCE's whole-stream bitrate in **kbps**, or `0` when nobody has measured it — the
    /// other half of what [`quality_policy`] needs, beside the frame size the playing-item store
    /// already carries.
    ///
    /// It comes off the LOADED DETAIL (`metadata::current().bitrate`, `Media[0]`) when that detail
    /// is this item, which is the ordinary path: a card's OK opens the detail page and Play is
    /// pressed there. **Playing straight from a shelf leaves it `0`**, and `0` fails closed (see
    /// [`crate::catalog::Ceiling::admits`]) — so with a rung selected, such a play routes to the
    /// re-encode rather than guessing the file is small enough. Carrying the bitrate on
    /// `PlayingItem` instead would measure every path, and is named as the follow-up in this
    /// unit's PR: that store is `metadata.rs`'s, not this lane's.
    pub src_kbps: i64,
    /// Trailer sessions omit `continuous=1` so EOS cannot Up-Next into a sibling extra.
    pub omit_queue_continuous: bool,
    /// Hero preview. Skip the PlayQueue entirely, and refuse anything that is not a direct play.
    pub preview: bool,
    /// Where the viewer resumes, known at the press (`0` = the start). A conversion is negotiated
    /// to begin there, so the landing does not replace an encoder it has only just started.
    pub start_ns: i64,
    /// The viewer's Plex Pass audio-DSP preference (`player::audio_enhancements`), captured at the
    /// request like the quality: the worker must not read the atomic the main thread moves. Only
    /// ever reaches the wire through [`desired_audio`]. A preview and a start-failure retry
    /// capture `NONE` (`request_play_inner`).
    pub audio_enhancements: crate::catalog::AudioEnhancements,
    /// **This attempt's capability snapshot**, derived ONCE at the play-start edge on the main
    /// thread (`catalog::capabilities::start_caps`: the boot snapshot less the stored refusals in
    /// force) and narrowed only by a recovery of this request. Every local gate below and the
    /// Jellyfin DeviceProfile read this value, so the two cannot disagree about one attempt.
    pub caps: std::sync::Arc<nj_platform::devcaps::Caps>,
    /// Test-only replacement for the process cache. Capability is an explicit policy input in
    /// regressions; no test mutates the production `OnceLock` or makes the whole host a DV set.
    #[cfg(test)]
    pub(super) dv_capability: Option<nj_platform::devcaps::dv::DvCapability>,
}


/// Does the loaded detail describe the leaf `rk` is about to play?
///
/// **Its own ratingKey, OR its on-deck episode's** — and the second half is not an optimisation.
/// A SHOW's `Detail.rk` is the show's key while the play `rk` is the EPISODE's, so an rk-only test
/// (which is all `cached_playing` needs, because it is fetching stream lists a show container does
/// not have) never matches on the commonest path in the app: press Play on a show page. With a
/// rung selected that put every episode in the library into the "unmeasured, fail closed" bucket
/// while [`playback_preview`] — which reads the same `Detail`'s numbers directly — still promised
/// Direct Play for it. Two answers to one question, which is the mismatch that preview exists to
/// prevent.
///
/// The show's technical fields ARE the on-deck episode's: `metadata::fetch_item_streams` backfills
/// them from exactly the leaf `playback_preview` answers for. An episode reached some OTHER way (a
/// season list, Up Next) still measures 0 and still fails closed — honest, and the residue that
/// `PlayingItem` carrying its own bitrate would close (`ResolveEnv::src_kbps`).
///
/// The SERVER half of the test is load-bearing on both arms: a ratingKey names an item only within
/// one server, so a bare-rk match against a colliding item on the other machine would hand the
/// ceiling the wrong file's bitrate.
pub(super) fn detail_describes(d: &crate::metadata::Detail, sid: ServerId, rk: &str) -> bool {
    crate::catalog::same_item((d.sid, &d.rk), (sid, rk))
        || d.on_deck
            .as_ref()
            .is_some_and(|ep| crate::catalog::same_item((d.sid, &ep.rk), (sid, rk)))
}


/// The source rate to judge against a ceiling, in kbps: **the VIDEO stream's own**, falling back
/// to the whole-file figure.
///
/// The distinction is the units the ceiling is spent in. `Ceiling::max_kbps` ships as
/// `maxVideoBitrate`, which bounds the VIDEO lane alone, while `Detail::bitrate` is `Media[0]`'s
/// whole-stream number — video plus every audio track. Comparing the second against the first
/// makes each rung bite about one AC-3 track early: a 7.9 Mbit/s video beside a 640 kbit/s track
/// measures 8.5 and loses direct play to the "1080p · 8 Mbps" rung, for an encode that would then
/// be capped at a rate its video already met.
///
/// `Detail::video` is the stream's own record and carries its own bitrate; it is `None` for a show
/// that never got an episode backfill and for an audio-only part, and PMS omits the field often
/// enough that the whole-file fallback has to stay. Falling back is the conservative direction,
/// which is the right one here — see [`crate::catalog::Ceiling::admits`].
pub(super) fn source_kbps(d: &crate::metadata::Detail) -> i64 {
    match d.video.as_ref().map(|v| v.bitrate) {
        Some(b) if b > 0 => b,
        _ => d.bitrate,
    }
}

/// Rate the quality ceiling judges for this play. A trailer extra is a different file from the
/// loaded parent: using the movie's 4K figure (or 0, which [`crate::catalog::Ceiling::admits`]
/// fails closed on) would force every non-Auto rung through the encoder.
pub(super) fn resolve_src_kbps(
    d: Option<&crate::metadata::Detail>,
    sid: ServerId,
    rk: &str,
) -> i64 {
    let Some(d) = d else {
        return 0;
    };
    if let Some(extra) = d
        .extras
        .iter()
        .find(|e| crate::catalog::same_item((d.sid, e.rk.as_str()), (sid, rk)))
    {
        return extra.bitrate;
    }
    if detail_describes(d, sid, rk) {
        source_kbps(d)
    } else {
        0
    }
}


/// Everything `resolve` DECIDES, as owned data. No `static mut`, no `SHARED`, no ACB/Starfish —
/// so it is `Send` and the resolve can run on a worker. `apply_plan` (main thread) is the ONLY
/// code that installs it. Adding a field here is how you add a resolve output; writing a static
/// from the worker is how you reintroduce the races the audit found.
#[derive(Default)]
pub(crate) struct Plan {
    pub direct_play_mode: DirectPlayMode,
    /// The capability snapshot this plan was resolved under ([`ResolveEnv::caps`]); `apply_plan`
    /// installs it as the attempt's (`PlaybackSession::attempt_caps`). `None` only on the default
    /// `Plan` a panicking resolve lands.
    pub caps: Option<std::sync::Arc<nj_platform::devcaps::Caps>>,
    /// The server this plan was resolved against — copied straight from [`ResolveEnv::sid`], so
    /// what `apply_plan` installs as `cur_sid` is the id the request captured and not a re-read of
    /// whatever became current while the worker ran. `UNSET` only on the default `Plan` a panicking
    /// resolve lands, which carries no URL either and so never starts an engine.
    pub sid: ServerId,
    pub url: String,
    pub tsession: String,
    /// Whole seconds into the item at which `tsession`'s encoder begins (`0` for a direct play and
    /// for a conversion from the start). The landing's resume compares against it before it
    /// replaces the encoder.
    pub encoder_start_secs: i64,
    /// The item's whole length as the negotiation stated it (`Negotiated::runtime_ns`),
    /// nanoseconds; `0` when unknown. Installed as `Session::runtime_ns`.
    pub runtime_ns: i64,
    pub sess: String,
    pub pq_id: String,
    pub pq_item_id: String,
    pub machine_id: String, // "" = leave the cached one alone
    pub vcodec: String,
    pub acodec: String,
    /// The SOURCE file's codecs, kept beside the ones above because on a transcode those are the
    /// server's OUTPUT. "hevc → h264" is the whole server-side transform, and it is invisible if
    /// only one half is recorded. Equal to `vcodec`/`acodec` for a direct play and for a remux.
    pub src_vcodec: String,
    pub src_acodec: String,
    /// The SOURCE file's video frame rate (0 = unknown), whichever route carries it. Beside
    /// `src_*` and not beside `fps`: `fps` is what the Load declares for THIS stream (the file's
    /// rate on a direct play and on a conversion that copies the video, 0 on a re-encode), while
    /// this stays the file's, so a later rebuild of the route ([`copied_video_fps`]) can decide again.
    pub src_fps: f64,
    pub fps: f64,
    /// The fed stream's Dolby Vision layering, for the Load payload's `DolbyHdrInfo` node: the
    /// file's on a direct play and on a conversion that copies the video, `NONE` on a re-encode,
    /// whose payload describes the server's OUTPUT ([`copied_video_dv`]).
    pub dovi: crate::metadata::Dovi,
    /// The declaration resolved from one cached capability snapshot. Payload builds, reloads and
    /// recovery consume this stored answer; a re-encode leaves it at `NONE`.
    pub dv_decision: crate::metadata::DvDecision,
    /// The FILE's own record and declaration whatever the route, so a later rebuild that copies
    /// the video can hand it on (`PlaybackSession::src_dovi`).
    pub src_dovi: crate::metadata::Dovi,
    pub src_dv_decision: crate::metadata::DvDecision,
    /// Does the direct-played audio track carry Dolby Atmos, for the Load payload's
    /// `contents.immersive` node. Set on the DIRECT-PLAY branch only, for the same reason `dovi`
    /// is: it describes the FILE's own elementary stream.
    pub immersive: bool,
    /// The encode's flavor/delivery/ceiling/audio-DSP shape — see [`crate::catalog::EncodeContract`].
    /// Was four independent fields (`remux`, `delivery`, `no_video_copy`, `ceiling`) until issue
    /// #266 needed a fifth that only ever means anything alongside `remux == true`; one value
    /// installed as [`Session::cur_contract`] is what lets a seek or a track switch rebuild the
    /// exact same query instead of four/five fields that could drift out of the coupling that
    /// matters (`no_video_copy` only under re-encode, `ceiling` only under re-encode, `audio` only
    /// under remux — see the field docs on `EncodeContract` itself).
    pub contract: crate::catalog::EncodeContract,
    /// The audio track this plan carries, installed as `Session::cur_audio` (the one source of the
    /// timeline's `audioStreamID`). `CarriedAudio::from_stream` of the fetched stream the route
    /// names; [`CarriedAudio::named`] (id only, every fact unknown) when the route names an id
    /// whose stream was never read; `None` when it names no track (server default).
    pub audio: Option<CarriedAudio>,
    /// What the server did with the requested audio enhancement (`contract.audio`), installed as
    /// `Session::cur_enhancement`: `Off` unless an enhanced decision was asked for.
    pub(super) enhancement: super::decision::EnhancementOutcome,
    /// What this plan MEASURED the source at — `(kbps, w, h)`, any of them `0` for "nobody said".
    /// Carried so [`set_quality`] can re-ask [`quality_policy`] for the item already playing when
    /// the user picks a different rung, instead of guessing. See [`Session::cur_src`].
    pub src_measure: (i64, i64, i64),
    /// Whole-file wire rate used by Auto's runtime Original watchdog (video + audio).
    pub transport_kbps: i64,
    /// `video_direct_plays` for this source — see [`Session::cur_source_decodable`].
    ///
    /// **`bool::default()` is the wrong default and it is not a style point.** `false` is the
    /// claim "this television cannot decode the source", which the quality menu renders as a line
    /// of copy; `build_stream` has an exit that returns before the gate runs at all. So the
    /// initializer sets `true` explicitly and the gate overwrites it, which makes every exit carry
    /// something that was either measured or honestly absent.
    pub source_decodable: bool,
    /// This plan admitted Original specifically on a measured direct Remote link.
    pub auto_original_watched: bool,
    /// What the startup probe measured, kept so the live estimator can be SEEDED with it instead
    /// of starting from nothing — and so a later mode transition can hand the next worker the same
    /// evidence. `0` when this plan never probed (Local, Relay, a fixed rung, or Original).
    pub auto_prior_kbps: u32,
    /// Bootstrap's already-decided HLS contingency, retained even when the immediate route is
    /// Original. See [`Session::auto_bootstrap_rung`].
    pub auto_bootstrap_rung: Option<crate::abr::Rung>,
    /// A measured Remote can begin on HLS and later recover. Preserve the exact no-video-encode
    /// source declaration even when this plan's immediate output is H264/AAC HLS.
    pub(super) auto_original: Option<AutoOriginalCandidate>,
    /// demuxer stream ordinal to feed (direct-play, non-default track). None = leave as-is.
    pub feed_audio_ordinal: Option<i32>,
    /// the subtitle selected for this part or by the show preference (0 = none/off), so the
    /// menu checkmark and the timeline report agree with what is on screen — and a later
    /// conversion of this item carries the subtitle the user was already watching. On a
    /// conversion it is set only when the answer delivered one ([`Plan::sub_delivery`]).
    pub sub_sid: i64,
    /// client-renderer ordinal for that subtitle (`metadata::sub_render_ordinal`). None = subs off.
    pub sub_render_ordinal: Option<i32>,
    /// How a CONVERSION delivers `sub_sid` (`None` on direct play, where the client draws the
    /// file's own track, and when no subtitle is on). See [`Session::cur_sub_delivery`].
    pub sub_delivery: Option<crate::catalog::SubtitleDelivery>,
    /// The subtitle-language preference this play resolved under — the SHOW's own pref if it set
    /// one, else the ACCOUNT's — as a BCP-47 code, for the Subtitles menu's "yours" grouping
    /// (`metadata::sub_layout::sub_sections`). `ui/` sees only this code, never a Plex account type.
    pub sub_pref_lang: Option<String>,
    /// the playing item's track store, fetched off-thread and installed by apply_plan
    pub playing: Option<crate::metadata::PlayingItem>,
    /// The server's PRE-FLIGHT refusal (see [`refusal`]), when `/decision` said it can neither
    /// direct play nor convert this item. A plan carrying one has an EMPTY `url` by construction —
    /// that is how it fails, on the same path as every other unresolvable plan — and the sentence
    /// rides along so the read-out can quote the server instead of guessing. `None` on every other
    /// plan, including one that simply failed to reach the server.
    pub verdict: Option<PlayVerdict>,
    /// the episode queued after this one, straight off the `continuous=1` PlayQueue
    pub up_next: Option<UpNext>,
    /// that same PlayQueue's whole returned window, projected on the worker (see `queue`)
    pub queue: Vec<crate::catalog::QueueRow>,
}


/// The two reads a play start can ask side by side. No queue for a preview or a part-only play.
fn queue_and_playing(
    client: &crate::catalog::Client,
    rk: &str,
    part: &str,
    env: &ResolveEnv,
) -> (Option<super::decision::QueueInfo>, Option<crate::metadata::PlayingItem>) {
    std::thread::scope(|s| {
        let queue = (!rk.is_empty() && !env.preview).then(|| {
            nj_base::task::beside(s, "play-queue", || {
                resolve_playqueue(client, rk, &env.machine_id, !env.omit_queue_continuous)
            })
        });
        let playing = env.cached_item.clone().or_else(|| crate::metadata::fetch_playing_item(env.sid, rk, part));
        (queue.map(|q| q.finish()), playing)
    })
}

/// Pick the stream for an item with ONE Jellyfin negotiation (`POST /Items/{id}/PlaybackInfo`):
/// offer direct play when the pipeline can feed the file itself, and let the server answer with
/// the method it will serve — `DirectPlay` (the file's own bytes) or `Transcode` (its
/// `TranscodingUrl`: a remux when the video lane is copied, a re-encode otherwise). The ask names
/// the part's media source, so the track indexes apply to it. The Load payload is built from the
/// codecs the server says will ARRIVE, never from the source.
///
/// PURE: runs on the resolve worker. It must neither WRITE nor READ any `static mut` — every
/// input arrives in `ResolveEnv`, every output leaves in `Plan`, and `apply_plan` installs both
/// on the main thread. Write-purity alone is not enough: `apply_plan` reassigns the `machine_id`
/// and `sess` Strings, so a still-running superseded worker reading them is a use-after-free.
///
/// **And it must not ask which server is current.** The server arrives in `env.sid` and the only
/// client here is `client_for` of it: an item from a shared source is not the current server's.
pub(super) fn build_stream(rk: &str, part: &str, vcodec: &str, acodec: &str, env: &ResolveEnv) -> Plan {
    let mut plan = Plan {
        caps: Some(env.caps.clone()),
        // carried through every exit below, the failing ones included: a plan without a server is
        // a plan `apply_plan` cannot install an honest `cur_sid` from.
        sid: env.sid,
        direct_play_mode: env.direct_play_mode,
        src_vcodec: vcodec.to_string(),
        src_acodec: acodec.to_string(),
        // **`false` here is a CLAIM** — "this television cannot decode the source" — which the
        // quality menu turns into a line of copy, and the next exit returns before the gate runs.
        source_decodable: true,
        ..Default::default()
    };
    if env.direct_play_mode == DirectPlayMode::Disabled && rk.is_empty() {
        plan.verdict = Some(PlayVerdict::DirectPlayDisabled);
        return plan;
    }
    let forced = env.direct_play_mode == DirectPlayMode::Forced;
    let playback_quality = if forced { Quality::Original } else { env.quality };
    let client = match crate::catalog::client_for(env.sid) {
        Some(c) => c,
        None => return plan,
    };
    // Fresh per-playback session string: the negotiated PlaySessionId is filed under it, and
    // every report and the encoder stop name the playback through it.
    let session = new_sess(rk);
    plan.sess = session.clone();
    // The queue and the playing item's OWN track lists (menu + audio pick + esInfo fps read them —
    // the loaded detail can be a different item: show page, straight-from-Home play) need nothing
    // from each other, so they are asked together.
    let (queue, playing) = queue_and_playing(client, rk, part, env);
    if let Some(q) = queue {
        plan.machine_id = q.machine_id;
        plan.pq_id = q.id;
        plan.pq_item_id = q.item_id;
        plan.up_next = q.up_next;
        plan.queue = q.rows;
    }
    plan.playing = playing;
    if let (Some(id), Some(item)) = (env.subtitle_override, plan.playing.as_mut()) {
        // The retry's explicit selection owns both embedded and sidecar restoration; a stale
        // selection must not turn subtitles back on after the viewer chose Off.
        for sub in &mut item.subs { sub.selected = id > 0 && sub.id == id; }
    }
    // The local video gate consults the DEVICE's own decoder table and the source's Dolby Vision
    // layering — two things no DeviceProfile axis can state. A source that fails it is never
    // offered for direct play, whatever the server would say.
    let (src_w, src_h) = plan
        .playing
        .as_ref()
        .map(|p| (p.width, p.height))
        .unwrap_or((0, 0));
    let dovi = plan.playing.as_ref().map(|p| p.dovi).unwrap_or_default();
    // Freeze the capability and derived presentation together. A late configd answer affects the
    // next route only; it cannot change this candidate between the gate and Starfish Load.
    #[cfg(test)]
    let dv_decision = match env.dv_capability {
        Some(capability) => crate::metadata::DvDecision {
            capability,
            presentation: dovi.presentation_for_codec(
                !crate::metadata::dv_withheld(),
                capability,
                vcodec,
            ),
        },
        None => dovi.decision_now(vcodec),
    };
    #[cfg(not(test))]
    let dv_decision = dovi.decision_now(vcodec);
    let dv = dv_decision.presentation;
    let caps: &nj_platform::devcaps::Caps = &env.caps;
    let video_dp = if forced { video_feed_supported(vcodec, dv, caps) } else {
        video_direct_plays(vcodec, src_w, src_h, dv, caps)
    };
    plan.source_decodable = video_dp;
    // **Refusing direct play is only half of it.** A remux COPIES the video, and no
    // profile axis can say "Dolby Vision", so a refused Profile 5 would come back as the same
    // IPT-PQ bitstream one container down. Withdrawing the copy is what makes the refusal mean
    // something. This stays the base-layer question: a copy carries no declaration.
    let no_video_copy = dovi.base_layer_unusable();
    if dovi.present {
        crate::player::log(&format!(
            "dv: capability={} presentation={} profile={} bl_compat={}",
            dv_decision.capability.label(),
            dv.label(),
            dovi.profile,
            dovi.bl_compat,
        ));
    }
    if let Some(why) = dv.refusal() {
        crate::player::log(&format!(
            "route: dolby vision P{} (bl_compat={} el={}) — {why}, base layer is not self-displayable; re-encoding (no copy)",
            dovi.profile, dovi.bl_compat, dovi.el_present as i32
        ));
    } else if let Some(n) = dv.declared() {
        crate::player::log(&format!(
            "route: dolby vision P{} (bl_compat={} el={}) — declaring DolbyHdrInfo (trackType={} profileId={}); direct play",
            dovi.profile, dovi.bl_compat, dovi.el_present as i32, n.track_type, n.profile_id
        ));
    }
    let streamable = part_is_streamable(part);
    let tracks = plan
        .playing
        .as_ref()
        .map(|p| p.audio.as_slice())
        .unwrap_or(&[]);
    // The signed-in user's own language preferences (Jellyfin keeps them per user, not per show),
    // ranked the way the server's MediaStreamSelector ranks them.
    let user_prefs = if rk.is_empty() { None } else { client.language_prefs() };
    let audio_prefs = AudioLangPrefs {
        language: user_prefs.as_ref().and_then(|p| p.audio.as_deref()),
        prefer_default: user_prefs.as_ref().is_none_or(|p| p.play_default_audio_track),
    };
    let audio_sel = if env.audio_sid > 0 {
        tracks.iter().enumerate().find(|(_, t)| t.id == env.audio_sid
            && audio_direct_plays_with(caps, env.direct_play_mode, &t.codec, t.channels))
            .map(|(i, t)| (i as i32, t.codec.to_lowercase(), t.id))
            .or_else(|| (!forced && !env.audio_explicit).then(|| pick_dp_audio_mode(caps, tracks, acodec, audio_prefs, DirectPlayMode::Auto)).flatten())
    } else if rk.is_empty() {
        None
    } else {
        pick_dp_audio_mode(caps, tracks, acodec, audio_prefs, env.direct_play_mode)
    };
    if let Some(lang) = audio_prefs.language {
        let hit = audio_sel
            .as_ref()
            .and_then(|(i, _, _)| usize::try_from(*i).ok())
            .and_then(|i| tracks.get(i))
            .is_some_and(|s| lang_matches(lang, &s.lang_code));
        crate::player::log(&format!(
            "route: user prefers audio {lang} (default track first: {}) — {}",
            audio_prefs.prefer_default as i32,
            if hit {
                "playing that track"
            } else if tracks.iter().any(|s| lang_matches(lang, &s.lang_code)) {
                "a track in it exists but is not the direct-play pick"
            } else {
                "no track in it; using the usual order"
            }
        ));
    }
    // the language of the audio that will play — what the Smart subtitle mode is judged by
    let audio_lang: String = audio_sel
        .as_ref()
        .and_then(|(i, _, _)| usize::try_from(*i).ok())
        .and_then(|i| tracks.get(i))
        .or_else(|| tracks.iter().find(|s| s.default))
        .or_else(|| tracks.first())
        .map(|s| s.lang_code.clone())
        .unwrap_or_default();
    let subtitle_prefs = SubtitleLangPrefs {
        language: user_prefs.as_ref().and_then(|p| p.subtitle.as_deref()),
        mode: user_prefs.as_ref().map_or(SubtitleMode::Default, |p| SubtitleMode::parse(&p.subtitle_mode)),
    };
    plan.sub_pref_lang = subtitle_prefs.language.map(str::to_string);
    // ONE subtitle decision for the negotiation and the direct-play plan.
    let sub_pick = plan.playing.as_ref().and_then(|p| match env.subtitle_override {
        Some(id) => p.subs.iter()
            .position(|s| s.id == id && !s.external && embedded_subtitle_renderable(&s.codec))
            .and_then(|i| (id > 0).then_some((id, crate::metadata::sub_render_ordinal(&p.subs, i)))),
        None => pick_dp_subtitle_pref(&p.subs, subtitle_prefs, &audio_lang),
    });
    let audio_id = audio_sel.as_ref().map(|(_, _, id)| *id).unwrap_or(0);
    // What the connection allows (`link_policy` — a relay forbids the two flavours that ship the
    // file's own bytes) and what the direct-play mode setting allows, composed so the stricter
    // wins.
    let location = client.link();
    let allowed = direct_play_policy(env.direct_play_mode, crate::catalog::link_policy(location));
    if forced {
        let failure = if part.is_empty() { Some(ForcedFailure::NoOriginal) }
            else if !streamable { Some(ForcedFailure::Container) }
            else if !video_dp { Some(ForcedFailure::Video) }
            else if audio_sel.is_none() && (!tracks.is_empty() || !audio_direct_plays_with(caps, env.direct_play_mode, acodec, 0)) {
                Some(ForcedFailure::Audio)
            } else { None };
        if let Some(failure) = failure {
            plan.verdict = Some(PlayVerdict::Forced(failure));
            return plan;
        }
    }
    if rk.is_empty() {
        // The local-sample path: no item for the server to negotiate, only a part to open.
        if !part.is_empty() {
            let url = client.direct_play_url(part, &session).to_url();
            fill_direct_plan(&mut plan, url, vcodec, acodec, audio_sel.as_ref(), dovi, dv_decision, sub_pick);
        }
        return plan;
    }
    // **Auto, the Jellyfin way**: time `/Playback/BitrateTest` and advertise 70% of it as
    // `MaxStreamingBitrate` (bounding direct play too — see `jf::playback::device_profile`). A
    // verified LAN needs no proof; a fixed rung is its own ceiling; Original asks for no bound.
    let (max_w, max_h) = caps.hevc_max;
    let ceiling = match playback_quality {
        Quality::Auto if location != Some(crate::catalog::probe::Location::Local) && !env.preview => {
            auto_bitrate_kbps(client).map(|kbps| crate::catalog::Ceiling {
                max_kbps: kbps.max(1),
                max_w: max_w as i64,
                max_h: max_h as i64,
            })
        }
        q => q.ceiling(),
    };
    plan.contract.ceiling = ceiling;
    plan.contract.no_video_copy = no_video_copy;
    plan.src_dovi = dovi;
    plan.src_dv_decision = dv_decision;
    plan.src_fps = plan.playing.as_ref().map_or(0.0, |p| p.video_fps);
    plan.src_measure = (env.src_kbps, src_w, src_h);
    plan.transport_kbps = plan
        .playing
        .as_ref()
        .map(|p| p.bitrate)
        .filter(|&v| v > 0)
        .unwrap_or(env.src_kbps);
    let direct_candidate = allowed.direct_play && video_dp && streamable && !part.is_empty()
        && (audio_sel.is_some() || tracks.is_empty());
    // Direct play feeds the smart-DP audio pick; a conversion can carry any track (the server
    // converts what the panel cannot decode), so it carries the preference-ranked one.
    let ask_audio = if direct_candidate {
        audio_id
    } else {
        encode_audio_id(audio_id, env.audio_sid, tracks, audio_prefs)
    };
    // ONE subtitle for either path. Direct play draws the file's own track (`sub_pick`). A
    // conversion asks for the same pick — else the sidecar the server has selected, else a retry's
    // track (`env.sub_sid`) — and the answer says how it arrives (`Plan::sub_delivery`). Nothing
    // here forces a burn: the server burns only a track no soft delivery fits.
    let subtitle_id = if direct_candidate {
        sub_pick.map_or(0, |(id, _)| id)
    } else {
        sub_pick
            .map(|(id, _)| id)
            .or_else(|| plan.playing.as_ref().and_then(crate::metadata::server_selected_sidecar).map(|s| s.id))
            .or((env.sub_sid > 0).then_some(env.sub_sid))
            .unwrap_or(0)
    };
    let negotiation = client.negotiate(&crate::catalog::PlaybackAsk {
        rk,
        session: &session,
        media_source_id: crate::jf::convert::media_source_id(part),
        audio_index: (ask_audio > 0).then(|| crate::jf::ids::stream_index(ask_audio)),
        subtitle_index: (subtitle_id > 0).then(|| crate::jf::ids::stream_index(subtitle_id)),
        start_ticks: encoder_start_secs(env) * 10_000_000,
        ceiling,
        direct_play: direct_candidate || forced,
        video_copy: !forced && !no_video_copy && allowed.remux,
        forced,
        burn: false,
        hls_segment_secs: None,
        caps,
    });
    let n = match negotiation {
        crate::catalog::Negotiation::Playable(n) => n,
        crate::catalog::Negotiation::Refused(why) => {
            crate::player::log(&format!("playbackinfo: REFUSED — {}", why.code()));
            plan.verdict = Some(if forced {
                PlayVerdict::Forced(ForcedFailure::Unauthorized)
            } else {
                PlayVerdict::Server(why)
            });
            return plan;
        }
        crate::catalog::Negotiation::Unreachable => {
            crate::player::log("playbackinfo: no usable answer");
            if forced {
                plan.verdict = Some(PlayVerdict::Forced(ForcedFailure::OpenFailed));
            }
            return plan;
        }
    };
    crate::player::log(&format!(
        "playbackinfo: {} video {}->{}{} audio {}->{}{} ceiling={}",
        n.method.as_str(),
        n.video.source, n.video.output, if n.video.copied { " (copy)" } else { "" },
        n.audio.source, n.audio.output, if n.audio.copied { " (copy)" } else { "" },
        ceiling.map_or_else(|| "none".to_string(), |c| format!("{}kbps", c.max_kbps)),
    ));
    let direct = n.method == crate::catalog::PlayMethod::DirectPlay;
    // The item's length rides the route for both branches: a conversion's progressive stream has
    // no container duration to read, and a direct play whose header lacks one is no better off.
    plan.runtime_ns = n.runtime_ns;
    if forced && !direct {
        let _ = client.transcode_stop(&session);
        plan.verdict = Some(PlayVerdict::Forced(ForcedFailure::Unauthorized));
        return plan;
    }
    if env.preview && !crate::player::preview::accepts_direct_play(direct, !part.is_empty(), false) {
        crate::player::log("preview: refused — not a direct play");
        let _ = client.transcode_stop(&session);
        return plan;
    }
    if direct {
        plan.contract.ceiling = ceiling;
        fill_direct_plan(&mut plan, n.url, vcodec, acodec, audio_sel.as_ref(), dovi, dv_decision, sub_pick);
        return plan;
    }
    // A conversion: describe what the server will SEND, and keep the shape a seek or a track
    // switch re-negotiates from (`Session::cur_contract`).
    (plan.dovi, plan.dv_decision) = copied_video_dv(&n.video, dovi, dv_decision);
    plan.fps = copied_video_fps(&n.video, plan.src_fps);
    if plan.dv_decision.presentation.declared().is_some() {
        crate::player::log("route: the conversion copies the video; keeping the Dolby Vision declaration");
    }
    plan.vcodec = n.video.output;
    plan.acodec = n.audio.output;
    // A remux is a conversion whose video lane is copied; the wire method says `Transcode` for it,
    // as for every TranscodingUrl.
    plan.contract.remux = n.video.copied;
    let carried_id = n.audio_index.map_or(ask_audio, crate::jf::ids::track_id);
    plan.audio = plan_track(tracks, carried_id, -1, false);
    // The subtitle rides the conversion only as the answer delivers it; `apply_plan` points the
    // client renderer at it (`decision::adopt_subtitle_delivery`).
    if n.subtitle.is_some() {
        plan.sub_sid = subtitle_id;
    }
    plan.sub_delivery = n.subtitle;
    plan.url = n.url;
    plan.tsession = session;
    plan.encoder_start_secs = encoder_start_secs(env);
    plan
}

/// The Dolby Vision a conversion's stream carries. A video lane the server COPIES hands on the
/// file's own HEVC or AV1 elementary stream, DOVI configuration record and RPUs included, so the Load
/// declares it as the direct play would have (upstream #839; an LG C1 has no DTS decoder, so every
/// DV file with DTS audio arrives this way). A re-encoded lane carries the server's picture and
/// nothing is declared. A source whose base layer is unusable is never offered a copy
/// (`EncodeContract::no_video_copy`), and is refused here too should a server copy it anyway.
pub(super) fn copied_video_dv(
    video: &crate::catalog::Lane,
    dovi: crate::metadata::Dovi,
    decision: crate::metadata::DvDecision,
) -> (crate::metadata::Dovi, crate::metadata::DvDecision) {
    if video.copied && crate::metadata::dv_video_codec(dovi.profile, &video.output) && !dovi.base_layer_unusable() {
        (dovi, decision)
    } else {
        (crate::metadata::Dovi::NONE, crate::metadata::DvDecision::NONE)
    }
}

/// The video frame rate the Load may declare for a conversion: the SOURCE's own when the server
/// copies the video lane (the file's elementary stream, so direct play's rule holds there too), 0
/// (unknown) when it re-encodes it, whose rate the encoder owns. An H.264 24p stream declared at
/// the 60 an unknown rate keeps is presented at ~13 fps (`player/CLAUDE.md`; upstream #832). The
/// one place the rule lives: the plan, the recovery install and the retranscode landing ask it.
pub(super) fn copied_video_fps(video: &crate::catalog::Lane, src_fps: f64) -> f64 {
    if video.copied { src_fps } else { 0.0 }
}

/// The offset a conversion starts at: the resume in whole seconds, the unit a transcode seek
/// restarts at (`player::resume_at`), so both paths ask the server for the same position.
fn encoder_start_secs(env: &ResolveEnv) -> i64 {
    if env.preview { 0 } else { env.start_ns.max(0) / 1_000_000_000 }
}


/// The direct-play plan: the pipeline decodes the SOURCE codecs natively, so the Load payload uses
/// them (h264/hevc + the chosen audio track's codec). If a specific track was picked (aidx >= 0),
/// tell the demuxer to feed that stream — by CONTAINER ordinal, not the list position
/// (audio_ordinal sorts on the stream index).
#[allow(clippy::too_many_arguments)]
fn fill_direct_plan(
    plan: &mut Plan,
    url: String,
    vcodec: &str,
    acodec: &str,
    audio_sel: Option<&(i32, String, i64)>,
    dovi: crate::metadata::Dovi,
    dv_decision: crate::metadata::DvDecision,
    sub_pick: Option<(i64, i32)>,
) {
    let (aidx, achosen, asid) = audio_sel.cloned().unwrap_or((-1, acodec.to_string(), 0));
    // source fps for the Load esInfo — from the playing item's own store (present for the
    // straight-from-Home path too, which never ran load_detail)
    let fps = plan.playing.as_ref().map(|p| p.video_fps).unwrap_or(0.0);
    plan.vcodec = vcodec.to_string();
    plan.acodec = achosen.clone();
    plan.fps = fps;
    // Only here: this is the branch that feeds the FILE's own elementary stream, so it is the
    // only one whose Load payload may describe the file's Dolby Vision.
    plan.dovi = dovi;
    plan.dv_decision = dv_decision;
    // **Dolby Atmos**, read off the track we ACTUALLY PICKED, not off the part: a film routinely
    // ships an Atmos 7.1 beside a plain 5.1, and declaring one while feeding the other is a lie the
    // pipeline cannot detect. Set on this branch only: a conversion's payload describes the
    // server's output, not this track.
    plan.immersive = plan
        .playing
        .as_ref()
        .and_then(|p| {
            if aidx >= 0 {
                p.audio.get(aidx as usize)
            } else {
                p.audio.iter().find(|a| a.default)
            }
        })
        .is_some_and(|a| a.has_atmos());
    if plan.immersive {
        crate::player::log("audio: dolby atmos — declaring contents.immersive=ATMOS");
    }
    if aidx >= 0 {
        // apply_plan stores this on the main thread — the demux thread reads it on every reopen.
        plan.feed_audio_ordinal = Some(
            plan.playing
                .as_ref()
                .map(|p| crate::metadata::audio_ordinal(&p.audio, aidx as usize))
                .unwrap_or(aidx),
        );
    }
    // Record the picked track so the reports name what actually plays (sid 0 = default/unknown).
    let ordinal = plan.feed_audio_ordinal.unwrap_or(-1);
    plan.audio = plan_track(plan.playing.as_ref().map_or(&[][..], |p| &p.audio[..]), asid, ordinal, true);
    // The subtitle the user's preferences turn on — free here, since direct play renders
    // subtitles itself. apply_plan installs it on the main thread.
    if let Some((ssid, ord)) = sub_pick {
        plan.sub_sid = ssid;
        plan.sub_render_ordinal = Some(ord);
    }
    plan.url = url;
}


/// The signed-in Jellyfin user's audio preferences (`UserConfiguration`), the inputs the server's
/// own `MediaStreamSelector` ranks audio tracks by. There is deliberately no built-in language:
/// one used to sit here as a hard-coded English, and it opened a French user's French-default
/// files in English whenever an English track existed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct AudioLangPrefs<'a> {
    /// `AudioLanguagePreference`, an ISO 639-2 code (`"fra"`); `None` when unset.
    pub language: Option<&'a str>,
    /// `PlayDefaultAudioTrack`: the file's own default track outranks the language preference.
    pub prefer_default: bool,
}

/// One thing an audio pick may honour. [`audio_intents`] ranks them — THE precedence, decided
/// once, here, for every path that names an audio track: the direct-play pick
/// ([`pick_dp_audio_pref`]) and the conversion pick ([`encode_audio_id`]) each walk that same
/// ranking and take the first entry they can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioIntent<'a> {
    /// The user's language preference, when the item has a track in it.
    Language(&'a str),
    /// The file's own default track.
    FileDefault,
}

/// Jellyfin's order: with `PlayDefaultAudioTrack` the file's default track first, then the
/// preferred language; without it the preferred language first. A preference with no track in
/// it is left out.
fn audio_intents<'a>(tracks: &[crate::metadata::Stream], prefs: AudioLangPrefs<'a>) -> Vec<AudioIntent<'a>> {
    let lang = prefs
        .language
        .map(str::trim)
        .filter(|l| !l.is_empty() && tracks.iter().any(|s| lang_matches(l, &s.lang_code)))
        .map(AudioIntent::Language);
    if prefs.prefer_default && tracks.iter().any(|s| s.default) {
        std::iter::once(AudioIntent::FileDefault).chain(lang).collect()
    } else {
        lang.into_iter().chain(std::iter::once(AudioIntent::FileDefault)).collect()
    }
}


/// Pick the audio track to DIRECT-PLAY from the playing item's track store, returning
/// (list_idx, codec, stream_id): list_idx -1 = codec-default (demuxer matches by payload codec —
/// only when the track list is unavailable), else the index into `playing().audio`, with that
/// track's stream id so the reports can name the truth. [`audio_intents`] ranks what to honour;
/// this takes the first entry it can carry:
///   - [`AudioIntent::Language`]: the first direct-playable track in it;
///   - [`AudioIntent::FileDefault`]: the file's flagged default track if its codec is
///     direct-playable — by EXPLICIT index (matching by codec alone fed the first same-codec
///     stream, not the flagged default, when another track of that codec preceded it);
///   - then any other direct-playable track (TrueHD/DTS-default item with an AC3 sibling —
///     smart-DP).
/// None when NO audio track is direct-playable (→ conversion).
///
/// An intent this path cannot carry falls through rather than forcing a conversion to obey it,
/// which would drop the whole smart-direct-play class onto the server's encoder for one track.
///
/// PURE: takes the playing item's audio tracks explicitly instead of reaching into
/// `metadata::playing()`, whose `Vec`s the track menu and info panel hold slices into.
#[cfg(test)]
pub(super) fn pick_dp_audio(
    tracks: &[crate::metadata::Stream],
    default_acodec: &str,
) -> Option<(i32, String, i64)> {
    pick_dp_audio_pref(tracks, default_acodec, AudioLangPrefs::default())
}

/// [`pick_dp_audio`] with the user's Jellyfin language preferences.
#[cfg(test)]
pub(super) fn pick_dp_audio_pref(
    tracks: &[crate::metadata::Stream],
    default_acodec: &str,
    prefs: AudioLangPrefs<'_>,
) -> Option<(i32, String, i64)> {
    pick_dp_audio_mode(&crate::catalog::capabilities::start_caps(), tracks, default_acodec, prefs, DirectPlayMode::Auto)
}

/// [`audio_direct_plays_with`] against what the next play start would offer; for pages, not for
/// an attempt in flight (which reads its own snapshot).
pub(super) fn audio_direct_plays(mode: DirectPlayMode, codec: &str, channels: i64) -> bool {
    audio_direct_plays_with(&crate::catalog::capabilities::start_caps(), mode, codec, channels)
}

/// [`audio_direct_plays`] over an explicit capability snapshot. Auto and Forced both go through
/// `devcaps`' canonicaliser (the catalog's `dca` is the table's `dts`), so they cannot drift apart.
pub(super) fn audio_direct_plays_with(
    caps: &nj_platform::devcaps::Caps, mode: DirectPlayMode, codec: &str, channels: i64,
) -> bool {
    if mode == DirectPlayMode::Forced {
        caps.forced_supports_audio(codec, channels)
    } else {
        caps.audio_supports(codec, channels)
    }
}

fn pick_dp_audio_mode(
    caps: &nj_platform::devcaps::Caps,
    tracks: &[crate::metadata::Stream], default_acodec: &str,
    prefs: AudioLangPrefs<'_>, mode: DirectPlayMode,
) -> Option<(i32, String, i64)> {
    pick_dp_audio_eligible(tracks, default_acodec, prefs,
        |codec, channels| audio_direct_plays_with(caps, mode, codec, channels))
}

fn pick_dp_audio_eligible(
    tracks: &[crate::metadata::Stream], default_acodec: &str, prefs: AudioLangPrefs<'_>,
    eligible: impl Fn(&str, i64) -> bool,
) -> Option<(i32, String, i64)> {
    let dp = |codec: &str| eligible(codec, 0);
    if tracks.is_empty() {
        // no track info — fall back to the codec-default (or a conversion if that isn't DP)
        return if dp(default_acodec) {
            Some((-1, default_acodec.to_string(), 0))
        } else {
            None
        };
    }
    let pick = |i: usize| (i as i32, tracks[i].codec.to_lowercase(), tracks[i].id);
    let dp_at = |s: &crate::metadata::Stream| eligible(&s.codec, s.channels);
    let honoured = audio_intents(tracks, prefs).into_iter().find_map(|intent| match intent {
        AudioIntent::Language(l) => tracks
            .iter()
            .position(|s| dp_at(s) && lang_matches(l, &s.lang_code)),
        AudioIntent::FileDefault => tracks.iter().position(|s| s.default && dp_at(s)),
    });
    if let Some(i) = honoured {
        return Some(pick(i));
    }
    if !tracks.iter().any(|s| s.default) {
        // Once the server supplied tracks, their concrete channel count outranks the codec-only
        // Media default. Never erase a known 8-channel refusal by rechecking it as unknown/0.
        if let Some(i) = tracks.iter().position(|s| s.codec.eq_ignore_ascii_case(default_acodec) && dp_at(s)) {
            return Some(pick(i));
        }
    }
    // any direct-playable track (smart direct-play over a non-DP default)
    tracks.iter().position(dp_at).map(pick)
}

/// Stream id a CONVERSION carries — [`audio_intents`]' ranking with no codec limit, since the
/// server converts whatever the panel cannot decode.
///
/// `env_audio_sid` is the session/retry pick and wins when set (a mid-play quality drop keeps
/// what is already playing); a cold play zeros it (`request_play`). Otherwise:
///   - [`AudioIntent::Language`]: the direct-play pick when it is already in that language (so
///     lowering quality preserves the dub and does not needlessly encode a lossless sibling),
///     else the first track in it, whatever its codec;
///   - [`AudioIntent::FileDefault`]: the file's default track itself, whatever its codec;
///   - else the direct-play pick; `0` lets the server choose.
pub(super) fn encode_audio_id(
    dp_audio_id: i64,
    env_audio_sid: i64,
    tracks: &[crate::metadata::Stream],
    prefs: AudioLangPrefs<'_>,
) -> i64 {
    if env_audio_sid > 0 {
        return env_audio_sid;
    }
    audio_intents(tracks, prefs)
        .into_iter()
        .find_map(|intent| match intent {
            AudioIntent::Language(l)
                if tracks.iter().any(|s| s.id == dp_audio_id && lang_matches(l, &s.lang_code)) =>
            {
                Some(dp_audio_id)
            }
            AudioIntent::Language(l) => {
                tracks.iter().find(|s| s.id > 0 && lang_matches(l, &s.lang_code)).map(|s| s.id)
            }
            AudioIntent::FileDefault => tracks.iter().find(|s| s.default && s.id > 0).map(|s| s.id),
        })
        .unwrap_or(dp_audio_id)
}


/// Whether the client can render this embedded subtitle codec.
fn embedded_subtitle_renderable(codec: &str) -> bool {
    // Advertised bitmap/ASS/text codecs plus ff::sub_kind's raw UTF-8 packet formats.
    crate::catalog::is_dp_subtitle(codec) || matches!(codec,
        "vplayer" | "pjs" | "jacosub" | "microdvd" | "sami" | "realtext" |
        "subviewer" | "subviewer1" | "stl" | "mpl2")
}

/// The embedded subtitle the item already has selected (a retry's explicit pick), as
/// (stream id, embedded-subtitle ordinal for the client renderer). The ordinal is
/// `metadata::sub_render_ordinal` — the SAME identifier space the track menu commits and the
/// demuxer enumerates, not a list position. An external selection has no container ordinal and
/// is restored as a sidecar by `apply_plan` instead.
pub(super) fn pick_dp_subtitle(subs: &[crate::metadata::Stream]) -> Option<(i64, i32)> {
    let i = subs.iter().position(|s| s.selected && !s.external)?;
    let ord = crate::metadata::sub_render_ordinal(subs, i);
    // Both halves must be usable or neither is: the id is what the menu checkmark and the
    // reports key on, so rendering a stream we cannot NAME would show a subtitle while the menu
    // says Off.
    if ord < 0 || subs[i].id <= 0 || !embedded_subtitle_renderable(&subs[i].codec) {
        return None;
    }
    Some((subs[i].id, ord))
}

/// Jellyfin's `SubtitlePlaybackMode`, as the user's `UserConfiguration.SubtitleMode` names it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum SubtitleMode {
    /// Turn on what the file flags (default or forced).
    #[default]
    Default,
    /// Always the preferred language's full subtitles, else what the file flags.
    Always,
    /// Only forced subtitles.
    OnlyForced,
    /// Never automatically.
    None,
    /// The preferred language when the audio is not in it; otherwise only forced subtitles.
    Smart,
}

impl SubtitleMode {
    pub(super) fn parse(s: &str) -> Self {
        match s {
            "Always" => Self::Always,
            "OnlyForced" => Self::OnlyForced,
            "None" => Self::None,
            "Smart" => Self::Smart,
            _ => Self::Default,
        }
    }
}

/// The user's subtitle preferences (`SubtitleLanguagePreference` + `SubtitleMode`). Automatic
/// picks stay embedded/client-rendered: a preference never requests a burn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SubtitleLangPrefs<'a> {
    pub language: Option<&'a str>,
    pub mode: SubtitleMode,
}

/// The subtitle to turn ON at the start of a direct play: an explicit selection first
/// ([`pick_dp_subtitle`]), then the rules of the server's own `MediaStreamSelector` over the
/// renderable embedded tracks, preferred language ranked first. `audio_lang` is the language of
/// the audio that will play — what `Smart` judges by.
pub(super) fn pick_dp_subtitle_pref(
    subs: &[crate::metadata::Stream], prefs: SubtitleLangPrefs<'_>, audio_lang: &str,
) -> Option<(i64, i32)> {
    if let Some(pick) = pick_dp_subtitle(subs) {
        return Some(pick);
    }
    if subs.iter().any(|s| s.selected) {
        return None;
    }
    let lang = prefs.language.map(str::trim).filter(|l| !l.is_empty());
    let in_lang = |s: &crate::metadata::Stream| lang.is_some_and(|l| lang_matches(l, &s.lang_code));
    let mut order: Vec<usize> = (0..subs.len())
        .filter(|&i| !subs[i].external && embedded_subtitle_renderable(&subs[i].codec) && subs[i].id > 0)
        .collect();
    order.sort_by_key(|&i| !in_lang(&subs[i]));
    let first = |pred: &dyn Fn(&crate::metadata::Stream) -> bool| order.iter().copied().find(|&i| pred(&subs[i]));
    let flagged = |s: &crate::metadata::Stream| s.default || s.forced;
    let forced = |s: &crate::metadata::Stream| s.forced;
    let i = match prefs.mode {
        SubtitleMode::None => None,
        SubtitleMode::Default => first(&flagged),
        SubtitleMode::OnlyForced => first(&forced),
        SubtitleMode::Always => first(&|s| !s.forced && in_lang(s)).or_else(|| first(&flagged)),
        SubtitleMode::Smart => {
            if lang.is_some_and(|l| !audio_lang.is_empty() && lang_matches(l, audio_lang)) {
                first(&forced)
            } else {
                first(&in_lang).or_else(|| first(&flagged))
            }
        }
    }?;
    let ord = crate::metadata::sub_render_ordinal(subs, i);
    (ord >= 0).then_some((subs[i].id, ord))
}

/// Software feed formats and Dolby Vision declaration support. Forced mode bypasses device bounds
/// for h264/hevc, but AV1 and VP9 need the table's row: a set without one has no such decoder to
/// feed, so Forced cannot make it play.
pub(super) fn video_feed_supported(
    vcodec: &str,
    dv: crate::metadata::DvPresentation,
    caps: &nj_platform::devcaps::Caps,
) -> bool {
    (matches!(vcodec, "h264" | "hevc")
        || (vcodec == "av1" && caps.av1)
        || (vcodec == "vp9" && caps.vp9))
        && dv.refusal().is_none()
}

pub(super) fn direct_play_policy(mode: DirectPlayMode, policy: crate::catalog::LinkPolicy) -> crate::catalog::LinkPolicy {
    match mode {
        DirectPlayMode::Auto => policy,
        DirectPlayMode::Forced => crate::catalog::LinkPolicy { direct_play: true, remux: false },
        DirectPlayMode::Disabled => crate::catalog::LinkPolicy { direct_play: false, remux: policy.remux },
    }
}

/// PURE: the local direct-play VIDEO test — the codec, the source's stated frame size and its
/// Dolby Vision layering must ALL clear what this device and this pipeline can actually show.
///
/// The codec half: h264 unconditionally (every webOS SoC decodes it), hevc when the table lists the
/// decoder, av1 and vp9 only when the table lists their own rows (`Caps::av1`, `Caps::vp9`), and
/// anything else the pipeline cannot feed at all. The resolution half is the
/// local agreement with the profile's `*`-scoped `video.width`/`video.height` limitation: the
/// profile makes PMS transcode a 4K source down for a 1080p-bounded SoC, but when `/decision` is
/// unreachable the fallback never asks PMS, so without this test a 4K file with one
/// direct-playable audio track was fed verbatim to a decoder whose table says 1920x1088 — the
/// wrong-side failure devcaps' own doc names (issue #22's over-claim class), invisible on the
/// dev TV, whose bound is 4096x2176.
///
/// **The Dolby Vision half is the same shape of bug, found the same way, and it is NOT about the
/// decoder.** Every profile's base layer is ordinary HEVC and every one of them decodes here — so
/// a codec-name gate cannot see the difference, which is exactly why this one is needed. What
/// differs is whether the base layer MEANS anything on its own: Profile 8.1's does (it is HDR10,
/// and dropping the RPU costs only the dynamic metadata), Profile 5's does not (single-layer
/// IPT-PQ, no fallback — it decodes cleanly and displays in visibly wrong colours), and Profile
/// 7's is only half the picture.
///
/// **That half arrives here already DECIDED**, as a [`DvPresentation`] rather than as the raw
/// record, and that is the point: the same value the caller passes here is the value the Load
/// payload reads for its `DolbyHdrInfo` node. A stream we DECLARE is one the pipeline puts in
/// Dolby Vision mode, so Profile 5 direct-plays correctly and this gate must let it through; a
/// stream we do not declare falls back to `Dovi::base_layer_unusable`, the pre-declaration rule,
/// which carries the never-convict-on-silence reasoning. Taking the decision as an argument is
/// what makes "the gate and the payload can never disagree" checkable in one place —
/// [`Dovi::presentation`] — instead of being a coincidence between two functions.
///
/// **Refusing here is only half the work, and the other half is not in this function.** A refusal
/// sends the item down the transcode branch — but that branch's query grants PMS `directStream=1`,
/// permission to COPY the video rather than encode it, and the server takes it whenever the source
/// fits the caps: resolution, bitrate, and the profile's own limitation axes. None of those can say
/// "Dolby Vision", so a refused Profile 5 came back `Part.decision=transcode` with the video's own
/// decision `copy` — the same bitstream, the same wrong colours, one container down. `build_stream`
/// therefore also sets [`crate::catalog::TranscodeSpec::no_video_copy`], off `base_layer_unusable` and
/// never off this gate: a COPY carries no declaration, so it stays wrong even for a profile we are
/// happy to direct-play. The measurement is in `docs/pms-api.md` §"What the server actually does
/// with a Dolby Vision source". A server that cannot encode the result is then allowed to say so —
/// this PMS answers general code 2000, *"File is unplayable. DoVi (Profile 5) color space is not
/// supported."*, which [`DvPresentation::Refuse`] turns into the player's read-out. A read-out that
/// names the reason is the honest end of that road; a picture in the wrong colours is not.
///
/// Unknown dimensions (0) PASS: PMS omitting a Media attribute is not evidence of 4K, and
/// failing open is yesterday's behavior for every file the server never measured — the same
/// misread-degrades-to-assumed rule `devcaps::parse` applies, and `Dovi` applies it too.
///
/// **AV1 and VP9 are the codecs that need the table's own row.** `hevc_max` is the shared bound
/// and applies to every codec. AV1 and VP9 additionally have to clear `caps.av1_row` /
/// `caps.vp9_row` on each non-zero axis, because the row is the decoder's own statement of its
/// size; a set whose table lists no such decoder refuses the codec at any size. Bit depth is not
/// tested here (no codec's is): the DeviceProfile's `VideoBitDepth` condition carries it to the
/// server.
pub(super) fn video_direct_plays(
    vcodec: &str,
    src_w: i64,
    src_h: i64,
    dv: crate::metadata::DvPresentation,
    caps: &nj_platform::devcaps::Caps,
) -> bool {
    let codec_ok = vcodec == "h264"
        || (vcodec == "hevc" && caps.hevc)
        || (vcodec == "av1" && caps.av1)
        || (vcodec == "vp9" && caps.vp9);
    let (bw, bh) = caps.hevc_max;
    // The codec's own row, on each axis it states: AV1 and VP9 are the codecs whose bound is the
    // table's separate row rather than the shared one.
    let (rw, rh, _) = match vcodec {
        "av1" => caps.av1_row,
        "vp9" => caps.vp9_row,
        _ => (0, 0, 0),
    };
    let row_ok = (rw == 0 || src_w <= rw as i64) && (rh == 0 || src_h <= rh as i64);
    codec_ok && row_ok && src_w <= bw as i64 && src_h <= bh as i64 && dv.refusal().is_none()
}


/// The detail page's "how this plays" answer, BEFORE anything is played — the same FOUR gates
/// `build_stream` will apply (codec+resolution via [`video_direct_plays`], container via
/// [`part_is_streamable`], one direct-playable audio track, and the user's quality ceiling via
/// [`quality_policy`] — applied last and able only to downgrade), asked of the loaded `Detail`.
/// The ceiling is the one a reader debugging "why does this ordinary h264/AC-3 MKV say Converts"
/// will not think of, which is why it is named in the list rather than left to the code.
/// An approximation by design: the real decision can still consult the server (`server_decision`
/// when no DP audio track is found), so this leans the same way that fallback usually lands.
/// It exists for `Details Screen.dc.html`'s facts row and must stay a READ-ONLY preview —
/// nothing in the playback path may branch on it (the path re-derives for itself).
///
/// **THREE answers, not two, and the third is the one a two-valued preview got wrong.** "The
/// server has to do something" and "the server has to re-encode the picture" are different facts
/// (`is_remux`'s doc says so for the LIVE session; this is the same distinction before Play), and
/// the UI hangs a Plex Pass claim on the difference: hardware conversion and HDR tone mapping are
/// both properties of an ENCODE, so naming either one for a stream where no encoder runs points
/// the user at a purchase that would fix nothing — `player::error_shape`'s own rule, and the
/// polarity issue #22 is about.
#[derive(PartialEq, Clone, Copy, Debug)]
pub(crate) enum Preview {
    DirectPlay,
    /// Container-only REMUX — Plex's own "Direct Stream". The video (and usually the audio) is
    /// COPIED into progressive MKV because the container is not one the demuxer streams, or
    /// because no audio track direct-plays; the pixels arrive untouched, 4K and HDR10 intact.
    /// `build_stream` spells this exact case `plan.contract.remux = video_dp` on the transcode
    /// branch.
    Remux,
    /// A real re-encode: the server decodes and re-encodes the video.
    Converts,
}

/// [`playback_preview`]'s pure core — the three-way answer from the fields it actually needs, so
/// a caller holding an EPISODE's file and a show's stream list can ask the same question.
pub(crate) fn playback_preview_of(
    part: &str,
    vcodec: &str,
    width: i64,
    height: i64,
    dv: crate::metadata::DvPresentation,
    audio_streams: &[crate::metadata::Stream],
) -> Option<Preview> {
    if part.is_empty() {
        return None; // nothing playable loaded (a show still resolving its episode)
    }
    let video = video_direct_plays(vcodec, width, height, dv, &crate::catalog::capabilities::start_caps());
    let audio = audio_streams
        .iter()
        .any(|a| crate::catalog::is_dp_audio_track(&a.codec, a.channels));
    // Mirrors `build_stream`'s own ladder: the video gate decides whether an ENCODER runs at all,
    // and only once it has passed do the container and the audio decide between pulling the file
    // ourselves and asking the server to repackage it.
    Some(if !video {
        Preview::Converts
    } else if part_is_streamable(part) && audio {
        Preview::DirectPlay
    } else {
        Preview::Remux
    })
}


/// True when the part's container is one the buffer-feed demuxer streams over HTTP: MKV, or
/// MP4/M4V since the AVIO became seekable (see the `streamable` note at the decision site — the
/// old mkv-only gate was measured obsolete on-device 2026-08-11). Other containers (mov/avi/…)
/// are sent to Plex for a container remux instead of direct-play. Matches the container
/// extension in the part-key filename; the m4v spelling is the same mov demuxer and the same
/// `container=mp4` in PMS metadata. WebM is a Matroska subset the same demuxer reads, and the
/// container VP9 and AV1 files usually come in (`jf::convert::stream_ext` names a Jellyfin `webm`
/// source `stream.webm`).
pub(super) fn part_is_streamable(part_key: &str) -> bool {
    let name = part_key.rsplit('/').next().unwrap_or(part_key);
    let name = name.split('?').next().unwrap_or(name);
    name.ends_with(".mkv") || name.ends_with(".webm") || name.ends_with(".mp4") || name.ends_with(".m4v")
}


// ---- async resolve: worker computes an owned Plan, main thread installs it ------------------
// The house idiom (metadata::load_season / browse.rs): generation counter + single-flight +
// a monotone one-slot mailbox + a per-frame pump that applies on the MAIN thread.
//
// Cancellation is FLAG-ONLY by design: `cancel_play` bumps the generation so a landing is
// discarded, but it cannot wake a worker blocked in recv(2) — publishing the socket fd to make
// that possible broke the seek path and was reverted (docs/async-model-decision.md). That costs
// nothing here: the freeze is fixed by getting the resolve OFF the loop, and a worker lingering
// in the background is invisible once the UI has already moved on.

pub(super) struct AbandonedPlanResources {
    pub(super) sid: ServerId,
    pub(super) identities: Vec<String>,
}

pub(super) fn abandoned_plan_resources(plan: &Plan) -> Option<AbandonedPlanResources> {
    let mut identities = Vec::with_capacity(2);
    if !plan.tsession.is_empty() {
        identities.push(plan.tsession.clone());
    }
    if !plan.sess.is_empty() && !identities.iter().any(|id| id == &plan.sess) {
        identities.push(plan.sess.clone());
    }
    if identities.is_empty() {
        None
    } else {
        Some(AbandonedPlanResources {
            sid: plan.sid,
            identities,
        })
    }
}


pub(super) fn take_resume_for(pending: &mut Option<(u32, i64)>, gen: u32) -> i64 {
    match pending.take() {
        Some((owner, ns)) if owner == gen => ns,
        Some(other) => {
            // A later request already owns this value. Put it back; this landing cannot steal
            // another generation's position.
            *pending = Some(other);
            0
        }
        None => 0,
    }
}

#[cfg(test)]
#[path = "plan_test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "plan_session_tests.rs"]
mod session_tests;

#[cfg(test)]
#[path = "plan_quality_ceiling_tests.rs"]
mod quality_ceiling_tests;

#[cfg(test)]
#[path = "plan_track_selection_tests.rs"]
mod track_selection_tests;

#[cfg(test)]
#[path = "plan_dolby_vision_tests.rs"]
mod dolby_vision_tests;

#[cfg(test)]
#[path = "plan_av1_tests.rs"]
mod av1_tests;

#[cfg(test)]
#[path = "plan_codec_agreement_tests.rs"]
mod codec_agreement_tests;

#[cfg(test)]
#[cfg(feature = "devtriggers")]
mod beside_tests {
    use crate::catalog::rendezvous_test_support::Rendezvous;

    const G: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn answer(path: &str) -> String {
        if path.starts_with(&format!("/Items/{G}?")) {
            return format!(r#"{{"Id":"{G}","Name":"t","Type":"Movie"}}"#);
        }
        r#"{"Items":[],"TotalRecordCount":0}"#.into()
    }

    /// A play start's queue read and its item read are on the wire together.
    #[test]
    fn a_play_start_asks_the_queue_beside_the_item() {
        let _g = nj_base::testlock::serial();
        assert!(nj_net::net::global_init() && crate::curlio::available());
        // Only the queue's read names `Fields=MediaSources`; only the item's goes on to its segments.
        let server = Rendezvous::start(&[("Fields=MediaSources", "/MediaSegments/")], answer);
        let client = crate::catalog::client_for(server.sid).unwrap();
        let rk = crate::jf::ids::rating_key(G);
        let env = super::ResolveEnv { sid: server.sid, ..Default::default() };
        let (queue, playing) = super::queue_and_playing(client, &rk, "", &env);
        assert!(queue.is_some() && playing.is_some(), "{:?}", server.seen());
        assert_eq!(server.missed(), Vec::<String>::new(), "asked one after another: {:?}", server.seen());
    }
}
