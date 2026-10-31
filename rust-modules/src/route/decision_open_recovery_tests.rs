//! C3: the request's audio-conversion recovery, through the real route rebuild against the
//! Jellyfin loopback — what the server is asked, what the session installs, what is learned.
use super::*;
use crate::route::open_recovery::{Action, FedAudio, OpenFacts, OpenFailure, Record};
use super::test_support::*;

const H264_TWO_OPUS: &str = r#"{"Type":"Video","Codec":"h264","Index":0},{"Type":"Audio","Codec":"opus","Index":1,"Channels":2,"IsDefault":true},{"Type":"Audio","Codec":"opus","Index":2,"Channels":2,"Language":"eng"}"#;

fn conversion_url(reasons: &str) -> String {
    format!("/videos/{JF_GUID}/stream.mkv?VideoCodec=h264&AudioCodec=aac&TranscodeReasons={reasons}&PlaySessionId=ps-loopback")
}

/// A set whose attempt offers stereo Opus natively — the shape C3 enables.
fn opus_caps() -> std::sync::Arc<nj_platform::devcaps::Caps> {
    let mut caps = nj_platform::devcaps::Caps::assumed();
    caps.audio.push_str(",opus");
    caps.opus_max_channels = 2;
    std::sync::Arc::new(caps)
}

/// Install a direct play of the item's SECOND Opus track (app id 3, stream index 2) under
/// `opus_caps`, and fail its open with `failure`.
fn refused_direct_play(ps: &mut PlaybackSession, lb: &JfLoopback, contract: crate::catalog::EncodeContract, failure: OpenFailure) -> FedAudio {
    let rk = jf_rk();
    let track = crate::metadata::Stream { id: 3, codec: "opus".into(), channels: 2, ..Default::default() };
    test_support::apply_plan(ps,
        Plan {
            sid: lb.sid,
            sess: "jf-opus-logical".into(),
            url: format!("http://127.0.0.1/Videos/{JF_GUID}/stream.mkv?static=true&MediaSourceId={JF_GUID}"),
            contract,
            vcodec: "h264".into(),
            acodec: "opus".into(),
            audio: Some(CarriedAudio::from_stream(&track, 1)),
            caps: Some(opus_caps()),
            ..Default::default()
        },
        &rk,
    );
    assert!(!is_transcoding(ps));
    let facts = OpenFacts { failure, audio: fed_audio(ps), seen_picture: false, forced: false, pending_original: false,
        video_native: video_fed_natively(ps), auto_original: false };
    match open_recovery_failure(ps, &facts).action {
        Action::Convert(audio) => audio,
        other => panic!("a refused native Opus open is converted, not {other:?}"),
    }
}

/// The rung keeps everything the viewer chose: the carried track, the position, a copyable video.
/// It narrows only this attempt, and a clean rescue of an ambiguous refusal is one counted proof.
#[test]
fn a_refused_native_opus_is_converted_keeping_track_position_and_copied_video() {
    let mut ps = PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    let boot = nj_platform::devcaps::caps().clone();
    let lb = JfLoopback::start(
        playback_info(false, "mkv", H264_TWO_OPUS, Some(&conversion_url("AudioCodecNotSupported"))),
        user_config(None, true, None, "Default"),
    );
    let audio = refused_direct_play(&mut ps, &lb, Default::default(), OpenFailure::Refused { num: Some(601) });
    assert_eq!(audio, FedAudio { codec: "opus".into(), channels: 2 });

    assert!(convert_refused_audio(&mut ps, &audio, 90).is_some(), "the server converts");
    let body = playback_info_body(&lb.finish());
    assert_eq!(body["AudioStreamIndex"], 2, "the viewer's track, not the default");
    assert_eq!(body["StartTimeTicks"], 900_000_000i64, "at the request's position");
    assert_eq!(body["EnableDirectPlay"], false);
    assert_eq!(body["AllowVideoStreamCopy"], true, "the video stays copyable");

    assert!(is_transcoding(&ps));
    assert_eq!(cur_audio_sid(&ps), 3, "the session still carries the viewer's track");
    assert_eq!((stream_vcodec(&ps).as_str(), stream_acodec(&ps).as_str()), ("h264", "aac"));
    assert!(!attempt_caps(&ps).audio_has("opus"), "the attempt no longer offers the refused audio");
    assert!(!audio_track_direct_plays(&ps, "opus", 2), "nor does a later track switch of this request");
    assert_eq!(*nj_platform::devcaps::caps(), boot, "the boot snapshot is untouched");

    assert_eq!(ps.open_recovery.converting(), Some(&audio));
    note_open_recovery_load(&mut ps);
    assert_eq!(open_recovery_first_picture(&mut ps), Some(Record::Proof(audio)), "video copied, audio converted");
    assert_eq!(open_recovery_first_picture(&mut ps), None, "one proof per request");

    reset_session(&mut ps);
    install_active_encoder("");
}

/// A route whose contract forbids a video copy (a Dolby Vision source the TV cannot take) keeps
/// that rule in the conversion; the rescue then changed more than the audio and proves nothing.
#[test]
fn a_route_that_forbids_a_video_copy_converts_both_and_proves_nothing() {
    let mut ps = PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    let lb = JfLoopback::start(
        playback_info(false, "mkv", H264_TWO_OPUS, Some(&conversion_url("VideoCodecNotSupported,AudioCodecNotSupported"))),
        user_config(None, true, None, "Default"),
    );
    let contract = crate::catalog::EncodeContract { no_video_copy: true, ..Default::default() };
    let audio = refused_direct_play(&mut ps, &lb, contract, OpenFailure::Refused { num: Some(601) });
    assert!(convert_refused_audio(&mut ps, &audio, 0).is_some());
    let body = playback_info_body(&lb.finish());
    assert_eq!(body["AllowVideoStreamCopy"], false, "the contract's no-copy rule survives the rescue");
    note_open_recovery_load(&mut ps);
    assert_eq!(open_recovery_first_picture(&mut ps), None);

    reset_session(&mut ps);
    install_active_encoder("");
}

/// A conversion the server will not make spends the rung without touching the route, so the
/// failure falls through to the ladder once and never loops; the next request starts afresh.
#[test]
fn a_refused_conversion_spends_the_rung_and_the_next_request_starts_fresh() {
    let mut ps = PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    let lb = JfLoopback::start(playback_info(false, "mkv", H264_TWO_OPUS, None), user_config(None, true, None, "Default"));
    let audio = refused_direct_play(&mut ps, &lb, Default::default(), OpenFailure::HeaderUnsupported);
    assert!(convert_refused_audio(&mut ps, &audio, 30).is_none(), "no delivery method offered");
    let sid = lb.sid;
    let _ = lb.finish();
    assert!(!is_transcoding(&ps), "the route is not replaced");
    assert_eq!(stream_acodec(&ps), "opus");
    assert!(ps.open_recovery.spent());
    let again = OpenFacts { failure: OpenFailure::Refused { num: Some(200) }, audio: fed_audio(&ps), seen_picture: false, forced: false, pending_original: false, video_native: false, auto_original: false };
    assert_eq!(open_recovery_failure(&mut ps, &again).action, Action::Pass, "no second conversion");

    test_support::apply_plan(&mut ps, Plan { sid, url: "http://127.0.0.1/next".into(), ..Default::default() }, &jf_rk());
    assert_eq!(ps.open_recovery, crate::route::open_recovery::OpenRecovery::NEW, "a plan brings its own recovery");
    reset_session(&mut ps);
}

/// Force never converts: the rebuild itself refuses a forced route, whatever asked for it.
#[test]
fn a_forced_route_is_never_rebuilt_by_the_recovery() {
    let mut ps = PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    let lb = JfLoopback::start(
        playback_info(false, "mkv", H264_TWO_OPUS, Some(&conversion_url("AudioCodecNotSupported"))),
        user_config(None, true, None, "Default"),
    );
    let _ = refused_direct_play(&mut ps, &lb, Default::default(), OpenFailure::Refused { num: Some(601) });
    ps.direct_play_mode = DirectPlayMode::Forced;
    assert!(convert_refused_audio(&mut ps, &FedAudio::new("opus", 2), 0).is_none());
    let seen = lb.finish();
    assert!(!seen.iter().any(|r| r.line.contains("/PlaybackInfo")), "the server is not asked: {seen:?}");
    reset_session(&mut ps);
}

/// The answer Jellyfin 12.0 gave on the TV (2026-10-30) for an audio-only conversion of an HEVC
/// file: `AudioCodecNotSupported,DirectPlayError` with the HEVC copied. The rebuilt route must
/// declare the copied HEVC, and the rescue changed only the audio, so it is a proof.
#[test]
fn a_direct_play_error_in_the_answer_still_copies_the_video() {
    let mut ps = PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    let hevc_opus = H264_TWO_OPUS.replace(r#""Codec":"h264""#, r#""Codec":"hevc""#);
    let url = format!("/videos/{JF_GUID}/stream.mkv?VideoCodec=h264,hevc&AudioCodec=ac3,eac3,aac&TranscodeReasons=AudioCodecNotSupported,DirectPlayError&PlaySessionId=ps-loopback");
    let lb = JfLoopback::start(playback_info(false, "mkv", &hevc_opus, Some(&url)), user_config(None, true, None, "Default"));
    let audio = refused_direct_play(&mut ps, &lb, Default::default(), OpenFailure::Refused { num: Some(601) });
    assert!(convert_refused_audio(&mut ps, &audio, 0).is_some());
    let _ = lb.finish();
    assert_eq!(stream_vcodec(&ps), "hevc", "the server copied the HEVC; the Load must say so");
    note_open_recovery_load(&mut ps);
    assert_eq!(open_recovery_first_picture(&mut ps), Some(Record::Proof(audio)));
    reset_session(&mut ps);
    install_active_encoder("");
}

/// A refused video is asked for again as an encode under 1080p, at the request's position, with no
/// copy of the video allowed; the session reads as an encode afterwards, so the rung cannot refire.
#[test]
fn a_refused_video_is_encoded_under_the_1080p_ceiling() {
    let mut ps = PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    let lb = JfLoopback::start(
        playback_info(false, "mkv", H264_TWO_OPUS, Some(&conversion_url("VideoCodecNotSupported"))),
        user_config(None, true, None, "Default"),
    );
    let _ = refused_direct_play(&mut ps, &lb, Default::default(), OpenFailure::Refused { num: Some(601) });
    assert!(video_fed_natively(&ps));
    assert!(encode_refused_video(&mut ps, 90).is_some(), "the server encodes");
    let body = playback_info_body(&lb.finish());
    assert_eq!(body["AllowVideoStreamCopy"], false, "the refused video is not offered again");
    assert_eq!(body["StartTimeTicks"], 900_000_000i64, "at the request's position");
    assert!(body["MaxStreamingBitrate"].as_i64().is_some_and(|rate| rate <= 20_000_000), "{body}");
    assert_eq!(body["AudioStreamIndex"], 2, "the viewer's track");
    assert!(!video_fed_natively(&ps));
    assert_eq!(cur_ceiling(&ps).map(|c| (c.max_w, c.max_h)), Some((1920, 1080)));

    reset_session(&mut ps);
    install_active_encoder("");
}

/// A lower ceiling the viewer picked stays; only what exceeds 1080p comes down.
#[test]
fn the_refused_video_ceiling_never_raises_a_route() {
    use crate::catalog::Ceiling;
    let low = Ceiling { max_kbps: 4_000, max_w: 1280, max_h: 720 };
    assert_eq!(refused_video_ceiling(Some(low)), low);
    assert_eq!(refused_video_ceiling(None), REFUSED_VIDEO_CEILING);
    assert_eq!(refused_video_ceiling(Some(Ceiling { max_kbps: 80_000, max_w: 3840, max_h: 2160 })), REFUSED_VIDEO_CEILING);
}
