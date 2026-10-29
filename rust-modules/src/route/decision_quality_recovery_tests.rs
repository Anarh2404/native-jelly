//! Quality-menu, HLS controller and original-source recovery/rollback tests.

use super::*;
#[allow(unused_imports)]
use super::test_support::*;
use super::test_support::apply_plan;

/// The gate's verdict reaches the session, and it is the SOURCE codec that decides it.
///
/// Category: policy plumbing. It is one boolean, but it is the boolean a line of user-visible
/// copy is drawn from, and the two ends are in different modules — the menu asks
/// `route::source_decodable()` and never evaluates `video_direct_plays` itself, deliberately,
/// because a second evaluation could disagree with the routing decision it describes.
#[test]
#[cfg(feature = "devtriggers")]
fn the_codec_gates_verdict_is_what_the_quality_menu_reads() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let caps = nj_platform::devcaps::Caps::assumed();
    let dv = crate::metadata::Dovi::default().presentation_now(true);
    // The two ends of the gate, at a UHD raster this device's table admits.
    assert!(
        video_direct_plays("hevc", 3840, 2160, dv, &caps),
        "the raster is not what refuses a 4K item here — `hevc_max` admits it",
    );
    assert!(
        !video_direct_plays("av1", 3840, 2160, dv, &caps),
        "and the codec is: a set whose table lists no AV1 decoder (the assumed caps these tests run with) cannot direct-play AV1 at any size",
    );

    let _g = fresh_registry(&mut ps);
    { let s = &mut ps; s.cur_source_decodable = false };
    assert!(
        !source_decodable(&ps),
        "the menu reads the session, not the gate"
    );
    { let s = &mut ps; s.cur_source_decodable = true };
    assert!(source_decodable(&ps));
}

/// **The declared source raster reaches the catalog, and that is what makes the Uhd rung
/// reachable at all.**
///
/// Differential, and it is the plan's I9 blocker stated as a test: with a 1080p source
/// `limited_to` deletes the 4K actuator, so every `auto_network` case that ever ran could not
/// select the one rung whose `production_load_pm` the table calls empirical. Before this the
/// raster was a literal inside `arm_auto_fixture`, so the 4K leg was unreachable by construction
/// rather than by policy — `tests/serve_fixtures.py` served no 22000 rung and the literal was
/// there to keep candidates off a 404.
#[test]
fn a_declared_4k_source_makes_the_uhd_actuator_feasible() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _lock = nj_base::testlock::serial();
    let mut uhd_feasible = |raster: (u16, u16)| {
        arm_auto_fixture(&mut ps, 
            "http://host/clip.mp4",
            900_000,
            "http://host/__abr",
            true,
            raster,
        );
        auto_catalog(&ps)
            .feasible()
            .any(|candidate| candidate.rung == crate::abr::Rung::Uhd)
    };
    assert!(
        !uhd_feasible(HD),
        "a 1080p source must not admit the 4K actuator"
    );
    assert!(uhd_feasible((3_840, 2_160)), "a 4K source must");
}

#[test]
fn the_fixture_can_start_in_hls_instead_of_provoking_a_starvation() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);

    // Without the flag the fixture arms an Original and returns nothing to open.
    assert_eq!(
        arm_auto_fixture(&mut ps, 
            "http://host/clip.mp4",
            900_000,
            "http://host/__abr",
            false,
            HD
        ),
        None,
    );
    assert!(matches!(
        cur_delivery(&ps),
        crate::catalog::TranscodeDelivery::ProgressiveMkv
    ));
    assert!(
        auto_original_watch(&ps).is_some(),
        "…and it is WATCHED, which is what the transition case grades",
    );

    // With it, the post-fallback state is installed directly and the playlist comes back.
    let url = arm_auto_fixture(&mut ps, 
        "http://host/clip.mp4",
        900_000,
        "http://host/__abr/",
        true,
        HD,
    )
    .expect("a fixture that starts in HLS hands back the playlist to open");
    assert!(
        url.starts_with("http://host/__abr/720/master.m3u8"),
        "{url}"
    );
    assert!(matches!(
        cur_delivery(&ps),
        crate::catalog::TranscodeDelivery::FixedHls {
            seconds_per_segment: 2
        }
    ));
    assert_eq!(cur_ceiling(&ps), Some(crate::abr::Rung::P480.ceiling()));
    assert!(
        auto_original_watch(&ps).is_none(),
        "there is no Original under it to watch",
    );
    let (control, _) = hls_abr_control(&ps).expect("the direct HLS fixture has a controller");
    assert!(
        !control.has_original_candidate() && !control.can_recover_original(),
        "and a loopback source probe cannot escape the HLS-only test",
    );

    restore_quality(Quality::Original);
    install_active_encoder("");
    reset_session(&mut ps);
}

/// A source request that returns an HTTP error before its first body byte did not measure a
/// slow link.  Falling back as though it measured 0 kbps throws away the exact evidence which
/// admitted Original and opens at the emergency floor; on the incident server that left Auto
/// at 720/1100 kbps while a manual Original played smoothly.
///
/// The remote case carries the rung its completed source probe selected. The local case
/// deliberately carries bootstrap's unknown-link fallback: Local admitted Original without a
/// measurement, and source demand must not be relabelled as capacity after the open fails.
#[test]
#[cfg(feature = "devtriggers")]
fn an_unopened_auto_original_reuses_admission_evidence_instead_of_inventing_zero_rate() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);

    arm_auto_fixture(&mut ps, 
        "http://host/clip.mp4",
        10_000,
        "http://host/__abr",
        false,
        HD,
    );
    { let s = &mut ps; s.auto_bootstrap_rung = Some(crate::abr::Rung::P1080M12) };
    let remote =
        fallback_unopened_auto_to_hls(&mut ps, 0).expect("the refused source falls back to HLS");
    assert!(
        remote.contains("/__abr/12000/"),
        "the completed probe's decision is retained: {remote}"
    );

    reset_session(&mut ps);
    restore_quality(Quality::Auto);
    arm_auto_fixture(&mut ps, 
        "http://host/clip.mp4",
        28_000,
        "http://host/__abr",
        false,
        HD,
    );
    let local =
        fallback_unopened_auto_to_hls(&mut ps, 0).expect("a local refused source also falls back");
    assert!(
        local.contains("/__abr/720/"),
        "unknown capacity keeps bootstrap's honest floor: {local}"
    );

    restore_quality(Quality::Original);
    install_active_encoder("");
    reset_session(&mut ps);
}

/// A route declaration describes the elementary streams arriving at the television, not the
/// file PMS started from.  An Original Dolby Vision + Atmos source that falls back to HLS is
/// re-encoded as H.264 + AAC, so carrying its source-only Dolby flags across the handoff makes
/// diagnostics lie and (for `immersive`) tells the system player that AAC contains Atmos.
#[test]
#[cfg(feature = "devtriggers")]
fn dv_decision_survives_reload_recovery_and_rollback() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);

    arm_auto_fixture(&mut ps, 
        "http://host/dovi-atmos.mkv",
        28_000,
        "http://host/__abr",
        false,
        (3_840, 2_160),
    );
    assert!(set_stream_declaration_for_test(
        &mut ps,
        "hevc",
        "eac3",
        23.976,
        p8(),
        true,
        nj_platform::devcaps::dv::DvCapability::Supported,
    ));
    let frozen = stream_dv_decision(&ps);
    assert!(frozen.presentation.declared().is_some());
    assert_eq!(stream_dv_decision(&ps.publication()), frozen, "reload/session copy");
    let candidate = ps.auto_original.as_mut().expect("fixture Original candidate");
    candidate.dovi = p8();
    candidate.dv_decision = frozen;

    let hls = fallback_auto_to_hls(&mut ps, 8_000, 120).expect("the watched Original falls back");
    assert!(
        hls.contains("/__abr/"),
        "the fixture produced an HLS route: {hls}"
    );
    assert_eq!(stream_vcodec(&ps), "h264");
    assert_eq!(stream_acodec(&ps), "aac");
    assert_eq!(
        stream_fps(&ps),
        0.0,
        "an encoded output must not inherit source FPS metadata"
    );
    assert_eq!(
        ps.stream_dovi,
        crate::metadata::Dovi::NONE,
        "the route must retire the source's Dolby Vision declaration, not merely hide it",
    );
    assert!(
        !ps.stream_immersive,
        "the route must retire the source E-AC3 JOC/Atmos declaration, not merely hide it",
    );
    assert_eq!(stream_dovi(&ps), crate::metadata::Dovi::NONE);
    assert_eq!(stream_dv_decision(&ps), crate::metadata::DvDecision::NONE);
    assert!(!stream_immersive(&ps));

    assert_eq!(
        recover_auto_to_original(&mut ps, 120),
        Some(AutoOriginalReload::Direct),
    );
    assert_eq!(stream_dv_decision(&ps), frozen, "Original recovery candidate");
    assert_eq!(rollback_seconds(&mut ps), Some(120));
    assert_eq!(
        stream_dv_decision(&ps),
        crate::metadata::DvDecision::NONE,
        "rollback restores the frozen HLS output declaration",
    );
    assert!(set_stream_declaration_for_test(
        &mut ps,
        "hevc",
        "eac3",
        23.976,
        p8(),
        false,
        nj_platform::devcaps::dv::DvCapability::Supported,
    ));
    clear_output_dv(&mut ps);
    assert_eq!(
        stream_dv_decision(&ps),
        crate::metadata::DvDecision::NONE,
        "the re-encoded (HLS) output reset clears the declaration",
    );

    restore_quality(Quality::Original);
    install_active_encoder("");
    reset_session(&mut ps);
    assert_eq!(stream_dv_decision(&ps), crate::metadata::DvDecision::NONE);
}

/// **RE-EXPRESSED 2026-08-27**, name and message both. It read
/// `only_a_measured_remote_auto_original_arms_the_progressive_watchdog` and asserted "HLS and
/// Local Original do not use this watchdog" — which was true of the code and is the defect
/// `docs/measurements/local-original-blind.md` measured. What the watchdog needs is a
/// MEASURED SOURCE RATE and a progressive delivery; where the server sits is not part of it.
#[test]
fn a_measured_auto_original_arms_the_progressive_watchdog_wherever_the_server_is() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/source.mkv".into(),
            transport_kbps: 28_000,
            auto_original_watched: true,
            ..Default::default()
        },
        "rk-auto",
    );
    assert_eq!(auto_original_watch(&ps).map(|w| w.source_kbps), Some(28_000));
    { let s = &mut ps; s.cur_auto_original_watched = false };
    assert!(
        auto_original_watch(&ps).is_none(),
        "HLS owns its own controller and needs no watchdog"
    );
    restore_quality(Quality::Original);
    reset_session(&mut ps);
}

/// **The differential for the LOCAL blindness.** A local server, Auto, a direct-playable
/// source with a measured transport rate: `build_stream` must arm the watchdog. Against the
/// old route this fails on the last assertion — `plan.auto_original_watched` was
/// `Auto && Location::Remote && auto_original`, so a LAN playback ran unsupervised and a link
/// that turned out not to carry the source produced 8-25% of real time for the rest of the
/// film with no `abr:` line anywhere in the log.
#[test]
fn a_local_auto_original_is_supervised_exactly_like_a_remote_one() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    let sid = crate::catalog::register_for_test(
        "machine-local-watch",
        "<peer-host-1>.example.invalid",
        32400,
        "token",
        "test-client-id",
    );
    crate::catalog::client_for(sid)
        .expect("server installed")
        .set_link(crate::catalog::probe::Location::Local);
    apply_plan(&mut ps, 
        Plan {
            sid,
            url: "https://example.invalid/source.mkv".into(),
            transport_kbps: 10_634,
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::ProgressiveMkv,
                ceiling: None,
                ..Default::default()
            },
            auto_original_watched: true,
            ..Default::default()
        },
        "rk-local-original",
    );
    let watch = auto_original_watch(&ps).expect("a local Auto Original is still watched");
    assert_eq!(
        watch.source_kbps, 10_634,
        "and it is watched against the MEASURED source"
    );
    restore_quality(Quality::Original);
    reset_session(&mut ps);
    crate::catalog::reset_servers_for_test();
}

#[test]
#[cfg(feature = "devtriggers")]
fn hls_controller_starts_at_the_rung_the_runtime_fallback_selected() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            tsession: "encoder-1".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P720Low.ceiling()),
                ..Default::default()
            },
            ..Default::default()
        },
        "rk-auto",
    );
    let (control, encoder) = hls_abr_control(&ps).expect("Auto HLS control");
    assert_eq!(control.initial_rung, crate::abr::Rung::P720Low);
    assert_eq!(encoder.encoder(), "encoder-1");
    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
    crate::player::reset_route_requests_for_test(&ps);
}

/// **The incident this pins killed a playback from inside the client and read as a server
/// fault.** A switch commits, so the live encoder is `<sess>-abr-1`; a scrub reloads the demux
/// worker, which used to restart its generation counter at 0 while `transcode_seek` kept the
/// session id; the next transaction primed a candidate named `<sess>-abr-1` — the live session.
/// Rollback then stopped it via `abandon`, commit via `retire(previous)`, and the demuxer saw a
/// run of 404s it correctly reports as "not produced in time".
///
/// The assertion is deliberately about the NAME rather than about any one exit: both exits are
/// safe exactly when a candidate can never be called what the live encoder is called.
#[test]
fn a_candidate_is_never_named_after_the_encoder_it_would_replace() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            // `commit_transition` is deliberately closed outside a landed Engine. This fixture
            // exercises a live HLS replacement, so make the synthetic plan playable rather than
            // weakening the production `Stable` gate to accommodate an idle test session.
            url: "http://fixture.invalid/12000/master.m3u8".into(),
            // Two DIFFERENT fields, and their divergence is what makes the collision
            // possible: `sess` is the candidate namespace (`logical_session`), `tsession`
            // seeds the live encoder. Before any switch they agree, as they do on the wire.
            sess: "sess-42".into(),
            tsession: "sess-42".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P1080M12.ceiling()),
                ..Default::default()
            },
            ..Default::default()
        },
        "rk-auto",
    );
    // The fixture base is what lets `prime` answer without a client; the naming it exercises
    // is the same line the live path takes.
    { let s = &mut ps; s.auto_fixture_base = "http://fixture.invalid".into() };
    let (control, first_encoder) = hls_abr_control(&ps).expect("Auto HLS control");
    let proposal = crate::abr::Proposal {
        rung: crate::abr::Rung::P1080M6,
        direction: crate::abr::Direction::Down,
    };

    let primed = control
        .prime(&first_encoder, proposal, 0, None)
        .expect("the fixture path primes");
    let candidate = primed.encoder_session.clone();
    assert_ne!(
        candidate,
        first_encoder.encoder(),
        "a candidate may not be the live encoder",
    );
    // The switch commits: the candidate is now what the playback is reading.
    let raster = proposal.rung.raster();
    let observed = crate::abr::ObservedHlsVariant::new(
        u64::from(proposal.rung.kbps()) * 1_000,
        i32::from(raster.0),
        i32::from(raster.1),
    )
    .unwrap();
    let rejected = control.commit_transition(
        &first_encoder,
        &primed,
        proposal,
        (observed, 20_000),
        || None::<()>,
    );
    assert_eq!(rejected, Err(HlsCommitRefusal::TransitionRejected));
    assert_eq!(
        active_encoder(),
        first_encoder.encoder(),
        "a rejected local/controller transition must leave the process route untouched",
    );
    assert!(
        control.commit(&first_encoder, &primed, proposal, (observed, 20_000)),
        "the commit swap takes",
    );
    let transition_called = std::cell::Cell::new(false);
    let moved = control.commit_transition(
        &first_encoder,
        &primed,
        proposal,
        (observed, 20_000),
        || {
            transition_called.set(true);
            Some(())
        },
    );
    assert_eq!(moved, Err(HlsCommitRefusal::RouteMoved));
    assert!(
        !transition_called.get(),
        "a superseded worker may not mutate its controller/local state",
    );

    // Reconstructing the worker around the live route is the state a seek creates. The seek
    // now publishes a fresh physical encoder first; the important property here is still that
    // a fresh worker cannot restart a local counter and collide with whichever id is live.
    let (control, live) = hls_abr_control(&ps).expect("Auto HLS control survives the reload");
    assert_eq!(
        live.encoder(),
        candidate,
        "the seek carries the committed encoder, as it must",
    );
    assert_eq!(
        control.initial_rung, proposal.rung,
        "a seek must rebuild the controller at the rung the live encoder actually serves, \
         not at the stale bootstrap ceiling stored before the worker committed",
    );
    assert_eq!(
        control.initial_observed,
        Some((observed, 20_000)),
        "seek/reload must carry the delivered response separately from its request rung",
    );

    let after_seek = control
        .prime(&live, proposal, 890_000_000, None)
        .expect("the fixture path primes")
        .encoder_session;
    assert_ne!(
        after_seek,
        live.encoder(),
        "the first post-seek candidate must not be named after the live session",
    );

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
}

/// The canned conversion answer the seek and rebuild tests negotiate against.
fn jf_hls_info() -> String {
    let url = format!(
        "/videos/{JF_GUID}/master.m3u8?VideoCodec=h264&AudioCodec=aac&TranscodeReasons=VideoCodecNotSupported&PlaySessionId=ps-loopback"
    );
    playback_info(
        false,
        "mkv",
        r#"{"Type":"Video","Codec":"hevc","Index":0},{"Type":"Audio","Codec":"eac3","Index":1,"Channels":6}"#,
        Some(&url),
    )
}

/// A fixed-rung HLS conversion of the loopback item, already negotiated under `encoder`.
fn install_jf_hls(ps: &mut PlaybackSession, lb: &JfLoopback, logical: &str, encoder: &str) {
    let rk = jf_rk();
    let contract = crate::catalog::EncodeContract {
        delivery: crate::catalog::TranscodeDelivery::FixedHls { seconds_per_segment: 2 },
        ceiling: Some(crate::abr::Rung::P720Low.ceiling()),
        ..Default::default()
    };
    negotiate_jf_session(lb.sid, &rk, encoder, contract);
    apply_plan(ps,
        Plan {
            sid: lb.sid,
            sess: logical.into(),
            tsession: encoder.into(),
            url: format!("http://127.0.0.1/videos/{JF_GUID}/master.m3u8?PlaySessionId=ps-loopback"),
            contract,
            vcodec: "h264".into(),
            acodec: "aac".into(),
            ..Default::default()
        },
        &rk,
    );
}

/// Encoders the server was asked to end (`DELETE /Videos/ActiveEncodings`).
fn ended_encoders(requests: &[JfRequest]) -> usize {
    requests.iter().filter(|r| r.line.starts_with("DELETE /Videos/ActiveEncodings?")).count()
}

/// A seek during a conversion is a fresh negotiation at the new offset under a fresh encoder
/// session, published atomically; the session it replaced is stopped only after that.
#[test]
#[cfg(feature = "devtriggers")]
fn a_transcode_seek_swaps_to_a_fresh_physical_session_and_retires_the_old_one() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    assert!(nj_net::net::global_init() && crate::curlio::available());
    let lb = JfLoopback::start(jf_hls_info(), user_config(None, true, None, "Default"));
    restore_quality(Quality::Auto);
    install_jf_hls(&mut ps, &lb, "playback-seek", "playback-seek-abr-old");
    let before = ended_encoders(&lb.seen());

    let new_url = transcode_seek(&mut ps, 300).expect("accepted seek negotiation");
    let new_encoder = transcode_session(&ps);
    assert_ne!(new_encoder, "playback-seek-abr-old", "a seek must not reuse the session it is replacing");
    assert_eq!(active_encoder(), new_encoder);
    assert!(new_url.contains("PlaySessionId=ps-loopback"), "{new_url}");
    let seek = lb
        .seen()
        .into_iter()
        .filter(|r| r.line.contains("/PlaybackInfo"))
        .last()
        .expect("the seek negotiated")
        .json();
    assert_eq!(seek["StartTimeTicks"], 300 * 10_000_000_i64, "negotiated at the seek target");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while ended_encoders(&lb.seen()) == before {
        assert!(std::time::Instant::now() < deadline, "the replaced session was never stopped");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    lb.finish();
    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
}

/// A negotiation that fails (an `ErrorCode`, or a body that is not a PlaybackInfo answer) is
/// preparation, not publication: none of the requested route reaches the session.
#[test]
#[cfg(feature = "devtriggers")]
fn a_failed_retranscode_decision_leaves_the_live_route_unchanged() {
    for answer in [r#"{"MediaSources":[],"ErrorCode":"NoCompatibleStream"}"#, "this is not a PlaybackInfo answer"] {
        let mut ps = crate::route::PlaybackSession::IDLE;
        let _g = fresh_registry(&mut ps);
        assert!(nj_net::net::global_init() && crate::curlio::available());
        let lb = JfLoopback::start(jf_hls_info(), user_config(None, true, None, "Default"));
        restore_quality(Quality::Auto);
        install_jf_hls(&mut ps, &lb, "logical-playback", "live-encoder");
        lb.answer_playback_info(answer);
        let expected = worker_ticket();
        let before = (
            url(&ps),
            transcode_session(&ps),
            stream_vcodec(&ps),
            stream_acodec(&ps),
            cur_ceiling(&ps),
            cur_delivery(&ps),
        );

        assert_eq!(retranscode_for(&mut ps, &expected, 90), None, "{answer}");
        assert_eq!(worker_ticket(), expected, "the semantic route did not move");
        assert_eq!(
            (
                url(&ps),
                transcode_session(&ps),
                stream_vcodec(&ps),
                stream_acodec(&ps),
                cur_ceiling(&ps),
                cur_delivery(&ps),
            ),
            before,
            "a failed preparation must publish none of the requested declaration",
        );
        let asked = lb.finish().iter().filter(|r| r.line.contains("/PlaybackInfo")).count();
        assert_eq!(asked, 2, "the install and the rebuild each negotiated once");

        restore_quality(Quality::Original);
        reset_session(&mut ps);
        install_active_encoder("");
    }
}

/// Exact user sequence from the device trace: Auto HLS commits a replacement encoder, a
/// manual Original open fails, the held HLS route is restored, then the user selects Auto.
/// Every boundary must retain the encoder/rung/URL that was ACTUALLY on screen.  Before this
/// regression the worker updated only `ACTIVE_ENCODER`; the main-thread route still named the
/// bootstrap URL and ceiling, so rollback reopened old media and Auto restarted at 720 kbps.
#[test]
#[cfg(feature = "devtriggers")]
fn failed_original_then_auto_keeps_the_live_adaptive_route() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "http://fixture.invalid/720/master.m3u8?offset=100".into(),
            sess: "sess-live".into(),
            tsession: "encoder-bootstrap".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P480.ceiling()),
                ..Default::default()
            },
            transport_kbps: 28_000,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 23.976,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 42, ordinal: 1, codec: "eac3".into(), channels: 0, can_normalize_loudness: false, immersive: true }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-auto",
    );
    set_stream_codecs(&mut ps, "h264", "aac");
    { let s = &mut ps; s.auto_fixture_base = "http://fixture.invalid".into() };

    let (control, bootstrap) = hls_abr_control(&ps).expect("the Auto worker owns HLS");
    let proposal = crate::abr::Proposal {
        rung: crate::abr::Rung::Uhd,
        direction: crate::abr::Direction::Up,
    };
    let primed = control
        .prime(&bootstrap, proposal, 140_000_000, None)
        .expect("fixture candidate");
    let raster = proposal.rung.raster();
    let observed = crate::abr::ObservedHlsVariant::new(
        u64::from(proposal.rung.kbps()) * 1_000,
        i32::from(raster.0),
        i32::from(raster.1),
    )
    .unwrap();
    assert!(control.commit(&bootstrap, &primed, proposal, (observed, 20_000)));

    // The picker changed before the pump performed the codec-changing handoff. Exercise the
    // same claim boundary as the pump: a persisted checkmark alone is deliberately not an
    // applied route contract.
    set_quality(&mut ps, Quality::Original);
    let original = claim_route_action().expect("the manual Original action is explicit");
    assert_eq!(
        original.intent,
        RouteIntent::User(UserRouteIntent::RecoverOriginal(RecoveryCause::ManualOriginal)),
    );
    assert_eq!(
        recover_auto_to_original_for(&mut ps, &original.ticket, 142, RecoveryCause::ManualOriginal),
        Some(AutoOriginalReload::Direct),
    );
    assert_eq!(rollback_seconds(&mut ps), Some(142));
    assert_eq!(
        url(&ps),
        primed.url,
        "rollback must reopen the live candidate URL, never the bootstrap URL it replaced",
    );
    let (restored, restored_encoder) = hls_abr_control(&ps)
        .expect("failed manual Original still needs the adaptive HLS controller");
    assert_eq!(restored_encoder.encoder(), primed.encoder_session);
    assert_eq!(restored.initial_rung, proposal.rung);

    set_quality(&mut ps, Quality::Auto);
    assert_eq!(cur_ceiling(&ps), Some(proposal.rung.ceiling()));
    assert!(
        !crate::player::pending_transcode_refresh(),
        "Auto must adopt the already-live adaptive route instead of rebuilding at 720 kbps",
    );
    assert!(
        crate::player::pending_adaptive_reload(),
        "the retained HLS worker must recapture Auto's Original-recovery contract",
    );

    reset_session(&mut ps);
    restore_quality(Quality::Original);
    install_active_encoder("");
    crate::player::reset_audio_track();
    crate::player::reset_subtitle();
}

#[test]
#[cfg(feature = "devtriggers")]
fn hls_recovery_restores_the_exact_direct_source_and_rearms_its_watchdog() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/hls/master.m3u8".into(),
            tsession: "encoder-1".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P1080High.ceiling()),
                ..Default::default()
            },
            transport_kbps: 28_000,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 23.976,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 42, ordinal: 1, codec: "eac3".into(), channels: 0, can_normalize_loudness: false, immersive: true }),
                subtitle_ordinal: Some(2),
            }),
            ..Default::default()
        },
        "rk-auto",
    );
    assert_eq!(
        recover_auto_to_original(&mut ps, 120),
        Some(AutoOriginalReload::Direct)
    );
    assert_eq!(
        auto_history(&ps, ps.now_ms).visible_switches,
        0,
        "an unproven Original Load is not a switch the viewer has seen",
    );
    assert_eq!(url(&ps), "https://example.invalid/source.mkv");
    assert!(!is_transcoding(&ps));
    assert_eq!(
        cur_delivery(&ps),
        crate::catalog::TranscodeDelivery::ProgressiveMkv
    );
    assert_eq!(cur_ceiling(&ps), None);
    assert_eq!(stream_vcodec(&ps), "hevc");
    assert_eq!(stream_acodec(&ps), "eac3");
    assert_eq!(auto_original_watch(&ps).map(|w| w.source_kbps), Some(28_000));
    assert_eq!(crate::player::desired_sub_idx(), 2);
    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
    crate::player::reset_audio_track();
    crate::player::reset_subtitle();
}

/// **The evidence that authorises a recovery is not the evidence that it WORKED, and the
/// recovery was spending the old route before finding out.** (Device, 2026-08-29 — the
/// reported failure, in its own sequence.)
///
/// `recover_auto_to_original` cleared `tsession`, cleared the active encoder and asked the
/// server to stop the HLS encoder, and only then did the pump open the source URL. On that
/// television the source URL failed — the same server had answered **503** to an Original
/// probe forty seconds earlier while the HLS segments beside it kept succeeding — and by then
/// the working stream had been dismantled. The viewer asked for Original by hand and got the
/// failure read-out, on a film that had been playing.
///
/// So both irreversible steps are deferred until frames prove the new source, and the old
/// route is kept whole until then. This is the "kept whole" half; the pump wiring that spends
/// or restores it is `player/pump.rs`.
///
/// Differential by construction: against the recovery as it stood, the first assertion fails —
/// nothing was kept, so there was nothing to roll back to.
#[test]
fn a_recovery_that_never_opens_can_still_go_back_to_the_encoder_it_replaced() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/hls/master.m3u8".into(),
            tsession: "encoder-1".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P1080High.ceiling()),
                ..Default::default()
            },
            transport_kbps: 28_000,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 23.976,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 42, ordinal: 1, codec: "eac3".into(), channels: 0, can_normalize_loudness: false, immersive: true }),
                subtitle_ordinal: Some(2),
            }),
            ..Default::default()
        },
        "rk-auto",
    );
    // What a live HLS route declares to the pipeline. `apply_plan` leaves these to the
    // decision, so the test states them — they are half of what a rollback has to put back:
    // reloading the m3u8 while the Load payload still says `hevc` is a refusal, not a recovery.
    set_stream_codecs(&mut ps, "h264", "aac");
    assert_eq!(
        recover_auto_to_original(&mut ps, 120),
        Some(AutoOriginalReload::Direct)
    );
    assert_eq!(
        url(&ps),
        "https://example.invalid/source.mkv",
        "the route did commit"
    );
    assert_eq!(stream_vcodec(&ps), "hevc", "…declaration and all");
    assert!(
        original_recovery_pending(),
        "the encoder is still running on the server and the old route is still known —              nothing here has been proven yet",
    );

    // …and the source never opens. The pump asks for the old route back rather than raising
    // the failure read-out on a stream that was working a moment ago.
    assert_eq!(
        rollback_seconds(&mut ps),
        Some(120),
        "reload the old route where the film is"
    );
    assert_eq!(
        url(&ps),
        "https://example.invalid/hls/master.m3u8",
        "and it is the old route"
    );
    assert!(is_transcoding(&ps), "the HLS session id is back");
    assert_eq!(
        active_encoder(),
        "encoder-1",
        "and so is the encoder identity the ABR controller steers by — re-installed rather              than re-requested, because it was never stopped",
    );
    assert!(
        matches!(
            cur_delivery(&ps),
            crate::catalog::TranscodeDelivery::FixedHls { .. }
        ),
        "the delivery shape must come back with it, or the demuxer reads an m3u8 as an mkv",
    );
    assert_eq!(cur_ceiling(&ps), Some(crate::abr::Rung::P1080High.ceiling()));
    assert_eq!(
        stream_vcodec(&ps),
        "h264",
        "the HLS payload declaration, not the source's"
    );
    assert!(!original_recovery_pending(), "and the way back is spent");

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
    crate::player::reset_audio_track();
    crate::player::reset_subtitle();
}

/// **A playback that HAS a way back to Original must be able to LOOK for it.** (Device,
/// 2026-08-30 — the twenty minutes at 480p.)
///
/// `can_recover_original` is `!probe_part.is_empty() && original_source_kbps > 0`, and the
/// second term is filled from `PlaybackSession::cur_transport_kbps`, whose own doc explains the zero
/// as *"PMS did not provide one and disables the watchdog fail-safely"*. That reasoning is
/// sound for the PROGRESSIVE WATCHDOG it was written for, which compares a live socket against
/// that number and cannot do its job without it. It is imported here by accident: the HLS
/// RECOVERY gate does not compare anything against it up front — its whole purpose is to spend
/// a bounded probe finding out what the source actually costs.
///
/// So a missing whole-file bitrate silently deletes the feature. `ff.rs` builds
/// `OriginalRecovery` only when this returns true, and `probe_due` — the one thing that logs a
/// REASON — is inside it. The device log is the shape of that: after the user returned to Auto
/// mid-film there is not one `abr: probe withheld`, not one `abr: checking actual Original`,
/// and not one `abr: mode` in the remaining ~1 600 lines. The recovery did not decide against
/// probing; it was never constructed, and nothing said so.
///
/// The fallback is `cur_src.0`, the video rate, which is the same quantity minus audio and is
/// what the menu already shows the user. Wrong by the audio track, and being wrong by an audio
/// track is not comparable to the feature being absent.
///
/// Differential by construction: against unmodified code the first assertion fails.
#[test]
fn a_missing_whole_file_bitrate_must_not_silently_delete_original_recovery() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/hls/master.m3u8".into(),
            tsession: "encoder-1".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P480.ceiling()),
                ..Default::default()
            },
            // PMS said what the VIDEO runs at and did not say what the whole file does. That
            // is an ordinary answer, not a broken one.
            src_measure: (23_920, 3_840, 2_160),
            transport_kbps: 0,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 23.976,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 42, ordinal: 1, codec: "eac3".into(), channels: 0, can_normalize_loudness: false, immersive: true }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-auto",
    );
    let (control, _) = hls_abr_control(&ps).expect("Auto HLS control");
    assert!(
        control.can_recover_original(),
        "the candidate exists and its probe URL is known — a missing whole-file bitrate is a              reason to go and measure the source, which is what the probe DOES, and not a reason              to remove the only path back to it",
    );
    assert!(
        control.original_source_kbps() > 0,
        "and the gate needs a requirement to score against, or `source_requirement_kbps` is              zero and every link looks sufficient",
    );

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
}

/// The other half: a recovery that DOES open is made permanent, and the way back is spent
/// rather than left to be taken by some later failure on a route it no longer describes.
///
/// Both halves have to be pinned together, because the failure mode of a one-sided fix is
/// silent. A `confirm` that forgot to clear the slot would leave a stale rollback armed for
/// the rest of the film: the next unrelated demux failure would find it, restore an HLS route
/// whose encoder the server had long since reaped, and reload onto a dead URL — a worse
/// outcome than the failure read-out this whole change exists to avoid.
#[test]
fn a_recovery_that_opens_spends_the_way_back_rather_than_leaving_it_armed() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/hls/master.m3u8".into(),
            tsession: "encoder-1".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P1080High.ceiling()),
                ..Default::default()
            },
            transport_kbps: 28_000,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 23.976,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 42, ordinal: 1, codec: "eac3".into(), channels: 0, can_normalize_loudness: false, immersive: true }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-auto",
    );
    assert_eq!(
        recover_auto_to_original(&mut ps, 120),
        Some(AutoOriginalReload::Direct)
    );
    assert!(original_recovery_pending());

    // Frames arrived — the pump's own test for this, and the reason it is frames and not
    // `loadCompleted`.
    settle_pending_native_start(&mut ps, RouteStartResult::Started);
    confirm_original_recovery(&mut ps);
    assert!(!original_recovery_pending(), "the recovery is permanent");
    assert_eq!(
        auto_history(&ps, ps.now_ms).visible_switches,
        1,
        "the first decoded frame commits exactly one visible HLS-to-Original switch",
    );
    assert_eq!(
        url(&ps),
        "https://example.invalid/source.mkv",
        "and the route is the new one"
    );
    assert!(!is_transcoding(&ps));
    assert_eq!(
        active_encoder(),
        "encoder-1",
        "the physical encoder is stopped, but its exact Streaming Resource identity remains \
         the owner of the direct body until playback teardown",
    );
    assert!(
        rollback_original_recovery(&mut ps).is_none(),
        "a spent way back may not be taken by a later, unrelated failure",
    );

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
    crate::player::reset_audio_track();
    crate::player::reset_subtitle();
}

#[test]
#[cfg(feature = "devtriggers")]
fn manual_original_adopts_one_running_trial_and_revokes_its_auto_ticket_on_frame() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/hls/master.m3u8".into(),
            tsession: "adopt-hls".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P1080High.ceiling()),
                ..Default::default()
            },
            transport_kbps: 28_000,
            auto_original: Some(test_original_candidate(None)),
            ..Default::default()
        },
        "rk-adopt-original",
    );
    let hls_worker = worker_ticket();
    assert_eq!(
        recover_auto_to_original_for(&mut ps, &hls_worker, 120, RecoveryCause::Automatic),
        Some(AutoOriginalReload::Direct),
    );
    let trial_worker = worker_ticket();
    let attempts_before = PLAYER_CONTROL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .next_start_attempt;

    set_quality(&mut ps, Quality::Original);
    assert!(original_recovery_pending());
    assert!(is_worker_ticket_current(&trial_worker));
    assert!(PLAYER_CONTROL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pending_original
        .as_ref()
        .is_some_and(|pending| pending.adopted_by_user),);

    settle_pending_native_start(&mut ps, RouteStartResult::Started);
    confirm_original_recovery(&mut ps);
    let control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        control.next_start_attempt,
        attempts_before
            .checked_add(1)
            .expect("test Load attempt identity exhausted"),
    );
    assert_eq!(control.phase, ControlPhase::Stable);
    assert_eq!(control.applied_quality, Quality::Original);
    drop(control);
    assert!(
        !is_worker_ticket_current(&trial_worker),
        "the Auto candidate worker may not publish after manual adoption commits",
    );
    assert!(!original_recovery_pending());

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
    reset_player_control_for_test(&ps);
    crate::player::reset_audio_track();
    crate::player::reset_subtitle();
}

/// A quality pick can arrive after Starfish accepted the replacement Load but before that
/// replacement produced its first frame. The held HLS route is still the only proven route in
/// that interval, so the pick must wait behind the Original commit boundary instead of
/// mutating the transaction underneath its rollback snapshot.
#[test]
fn a_quality_change_waits_for_an_original_handoff_to_commit() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/hls/master.m3u8".into(),
            tsession: "quality-hls".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P1080High.ceiling()),
                ..Default::default()
            },
            transport_kbps: 28_000,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 23.976,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 42, ordinal: 1, codec: "eac3".into(), channels: 0, can_normalize_loudness: false, immersive: true }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-quality-handoff",
    );
    assert_eq!(
        recover_auto_to_original(&mut ps, 120),
        Some(AutoOriginalReload::Direct),
    );

    set_quality(&mut ps, Quality::P480);
    assert_eq!(
        quality(),
        Quality::P480,
        "the preference and checkmark move now"
    );
    assert!(
        original_recovery_pending(),
        "the first-frame proof still owns the route"
    );
    assert_eq!(
        cur_delivery(&ps),
        crate::catalog::TranscodeDelivery::ProgressiveMkv,
        "the pending source declaration may not be rewritten before its first frame",
    );
    assert!(
        !crate::player::pending_transcode_refresh(),
        "the pump must not replace either half of an unconfirmed transaction",
    );

    settle_pending_native_start(&mut ps, RouteStartResult::Started);
    confirm_original_recovery(&mut ps);
    assert!(!original_recovery_pending());
    assert_eq!(
        cur_ceiling(&ps),
        Some(crate::abr::Rung::P480.ceiling()),
        "the deferred pick applies as soon as decoded frames commit Original",
    );
    assert!(crate::player::pending_transcode_refresh());

    let staged = claim_route_action().expect("deferred fixed-rung effect");
    finish_route_action(&mut ps, &staged, RouteApplyResult::Rejected);
    assert_eq!(
        cur_delivery(&ps),
        crate::catalog::TranscodeDelivery::ProgressiveMkv,
        "rejecting the deferred effect must restore the Original candidate which produced frames",
    );
    assert_eq!(cur_ceiling(&ps), None);
    assert_eq!(url(&ps), "https://example.invalid/source.mkv");

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
    crate::player::reset_route_requests_for_test(&ps);
}

#[test]
fn a_quality_change_survives_an_original_handoff_rollback() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/hls/master.m3u8".into(),
            tsession: "quality-rollback-hls".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P1080High.ceiling()),
                ..Default::default()
            },
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 23.976,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 0, ordinal: -1, codec: "eac3".into(), channels: 0, can_normalize_loudness: false, immersive: false }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-quality-rollback",
    );
    assert_eq!(
        recover_auto_to_original(&mut ps, 120),
        Some(AutoOriginalReload::Direct),
    );
    set_quality(&mut ps, Quality::P480);

    assert_eq!(rollback_seconds(&mut ps), Some(120));
    assert_eq!(
        cur_delivery(&ps),
        crate::catalog::TranscodeDelivery::FixedHls {
            seconds_per_segment: 2
        },
        "failure first restores the one route that was proven to play",
    );
    assert_eq!(cur_ceiling(&ps), Some(crate::abr::Rung::P1080High.ceiling()));
    assert!(!crate::player::pending_transcode_refresh());

    // Deferred commands belong to the exact rollback Load, not to thread creation. Only its
    // accepted native result releases the next transaction.
    settle_pending_native_start(&mut ps, RouteStartResult::Started);
    assert_eq!(cur_ceiling(&ps), Some(crate::abr::Rung::P480.ceiling()));
    assert!(crate::player::pending_transcode_refresh());

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
    crate::player::reset_route_requests_for_test(&ps);
}

#[test]
fn a_failed_rollback_load_discards_trial_effects_before_the_next_trial() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    crate::player::reset_route_requests_for_test(&ps);
    reset_session(&mut ps);
    restore_quality(Quality::Auto);
    { let s = &mut ps; {
        s.url = "http://fixture.invalid/hls/master.m3u8".into();
        s.tsession = "rollback-owner".into();
        s.cur_contract.delivery = crate::catalog::TranscodeDelivery::FixedHls {
            seconds_per_segment: 2,
        };
        s.cur_contract.ceiling = Some(crate::abr::Rung::P1080High.ceiling());
    } };
    install_active_hls(
        "rollback-owner",
        "http://fixture.invalid/hls/master.m3u8",
        crate::abr::Rung::P1080High,
    );
    reset_player_control_for_test(&ps);

    let first = snapshot_route(&ps, "rollback-owner".into(), 41);
    { let s = &mut ps; {
        s.url = "https://example.invalid/first-source.mkv".into();
        s.tsession.clear();
        s.cur_contract.delivery = crate::catalog::TranscodeDelivery::ProgressiveMkv;
        s.cur_contract.ceiling = None;
    } };
    set_pending_original(&ps, first, true);
    set_quality(&mut ps, Quality::P480);
    assert_eq!(rollback_seconds(&mut ps), Some(41));
    settle_pending_native_start(&mut ps, RouteStartResult::StartFailed);
    {
        let control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
        assert!(matches!(control.phase, ControlPhase::Failed(_)));
        assert!(control.start_deferred.is_none());
    }
    assert!(
        !crate::player::pending_transcode_refresh(),
        "a terminal rollback Load may not release its deferred quality edit",
    );

    // A later, independent Original trial and successful rollback carry no residue from the
    // failed transaction, even though the durable picker still remembers the user's choice.
    let second = snapshot_route(&ps, "rollback-owner".into(), 52);
    { let s = &mut ps; {
        s.url = "https://example.invalid/second-source.mkv".into();
        s.tsession.clear();
        s.cur_contract.delivery = crate::catalog::TranscodeDelivery::ProgressiveMkv;
        s.cur_contract.ceiling = None;
    } };
    set_pending_original(&ps, second, true);
    assert_eq!(rollback_seconds(&mut ps), Some(52));
    settle_pending_native_start(&mut ps, RouteStartResult::Started);
    {
        let control = PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(control.phase, ControlPhase::Stable);
        assert!(control.start_deferred.is_none());
        assert!(control.pending_user.is_none());
    }
    assert!(!crate::player::pending_transcode_refresh());

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
    reset_player_control_for_test(&ps);
    crate::player::reset_route_requests_for_test(&ps);
}

#[test]
#[cfg(feature = "devtriggers")]
fn audio_selected_during_original_trial_uses_the_route_that_actually_lands() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/hls/master.m3u8".into(),
            tsession: "audio-rollback-hls".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P1080High.ceiling()),
                ..Default::default()
            },
            audio: Some(CarriedAudio::named(7, -1)),
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 23.976,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 42, ordinal: 1, codec: "eac3".into(), channels: 0, can_normalize_loudness: false, immersive: false }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-audio-rollback",
    );
    assert_eq!(
        recover_auto_to_original(&mut ps, 120),
        Some(AutoOriginalReload::Direct)
    );

    commit_audio_selection(&mut ps, CarriedAudio { sid: 99, ordinal: 2, codec: "aac".into(), channels: 2, can_normalize_loudness: false, immersive: false });
    assert!(
        !pending_user_route_intent(UserRouteIntent::NativeAudioReload),
        "the temporary Direct actuator must not escape the Original trial",
    );
    assert_eq!(rollback_seconds(&mut ps), Some(120));
    settle_pending_native_start(&mut ps, RouteStartResult::Started);
    assert_eq!(cur_audio_sid(&ps), 99);
    assert!(
        pending_user_route_intent(UserRouteIntent::Retranscode),
        "after HLS rollback the same semantic pick must be applied by retranscode",
    );

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
    crate::player::reset_route_requests_for_test(&ps);
}

/// #266 follow-up: the Auto watchdog already probes this exact Part on this exact identity on its
/// own worker thread before it ever proposes the recovery
/// (`probe_original_while_hls_cancellable`), so a main-thread `admit_original_part` call for
/// `RecoveryCause::Automatic` would only add up to `PART_ADMISSION_BUDGET` of redundant UI/feed
/// block. `RecoveryCause::needs_part_admission` must keep the admission for `ManualOriginal` and
/// `EnhancementReleased` (the viewer just acted, no prior sample exists) but skip it for
/// `Automatic`.
#[test]
#[cfg(feature = "devtriggers")]
fn automatic_recovery_issues_no_part_admission_before_the_trial() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    use std::io::{BufRead, BufReader, Write};

    let _g = fresh_registry(&mut ps);
    if !nj_net::net::global_init() || !crate::curlio::available() {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port() as i32;
    let (tx, rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        // Any request at all inside this window is a Part admission this cause must not send;
        // the trial itself does not open a socket during `recover_auto_to_original_for` (only the
        // later demux open does), so an empty request list is the whole claim.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut requests = Vec::new();
        while std::time::Instant::now() < deadline {
            match nj_base::testnet::accept(&listener) {
                Ok((mut socket, _)) => {
                    let mut reader = BufReader::new(socket.try_clone().expect("clone socket"));
                    let mut first = String::new();
                    reader.read_line(&mut first).expect("request line");
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).expect("request header");
                        if line == "\r\n" || line.is_empty() { break; }
                    }
                    requests.push(first);
                    let _ = socket.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("accept: {e}"),
            }
        }
        tx.send(requests).expect("publish observation");
    });

    let sid = crate::catalog::register_for_test(
        "auto-no-admission",
        "127.0.0.1",
        port,
        "token",
        "auto-no-admission-client",
    );
    let client = crate::catalog::client_for(sid).expect("test server installed");
    let logical_url = client
        .direct_play_url("/library/parts/1/file.mkv", "auto-no-admission-logical")
        .to_url();
    restore_quality(Quality::Auto);
    apply_plan(&mut ps,
        Plan {
            sid,
            sess: "auto-no-admission-logical".into(),
            url: "http://fixture.invalid/hls/master.m3u8".into(),
            tsession: "auto-no-admission-hls".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P480.ceiling()),
                ..Default::default()
            },
            transport_kbps: 320,
            auto_original: Some(AutoOriginalCandidate {
                url: logical_url,
                probe_part: "/library/parts/1/file.mkv".into(),
                direct: true,
                vcodec: "h264".into(),
                fps: 24.0,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 0, ordinal: -1, codec: "aac".into(), channels: 0, can_normalize_loudness: false, immersive: false }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "42",
    );

    assert_eq!(
        recover_auto_to_original(&mut ps, 120),
        Some(AutoOriginalReload::Direct),
        "Automatic still reaches Direct without asking again"
    );

    let requests = rx.recv().expect("server observation");
    server.join().unwrap();
    assert!(
        requests.is_empty(),
        "Automatic must not admit the Part before the trial — the watchdog already asked: {requests:?}"
    );

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
    crate::catalog::reset_servers_for_test();
    crate::player::reset_audio_track();
    crate::player::reset_subtitle();
}

#[test]
#[cfg(feature = "devtriggers")]
fn direct_recovery_without_its_server_keeps_hls_instead_of_using_a_logical_alias() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            sid: unregistered_sid(),
            sess: "missing-logical".into(),
            url: "http://fixture.invalid/hls/master.m3u8".into(),
            tsession: "missing-hls".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P480.ceiling()),
                ..Default::default()
            },
            auto_original: Some(AutoOriginalCandidate {
                url: "http://missing.invalid/source.mkv?X-Plex-Session-Identifier=missing-logical".into(),
                probe_part: "/library/parts/1/file.mkv".into(),
                direct: true,
                vcodec: "h264".into(),
                fps: 24.0,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 0, ordinal: -1, codec: "aac".into(), channels: 0, can_normalize_loudness: false, immersive: false }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "42",
    );

    assert_eq!(recover_auto_to_original(&mut ps, 120), None);
    assert_eq!(url(&ps), "http://fixture.invalid/hls/master.m3u8");
    assert_eq!(active_encoder(), "missing-hls");
    assert!(!original_recovery_pending());

    restore_quality(Quality::Original);
    reset_session(&mut ps);
    install_active_encoder("");
}

#[test]
#[cfg(feature = "devtriggers")]
fn manually_picking_original_restores_native_dolby_vision_instead_of_retranscoding() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/hls/master.m3u8".into(),
            tsession: "encoder-1".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::FixedHls {
                seconds_per_segment: 2,
            },
                ceiling: Some(crate::abr::Rung::P1080High.ceiling()),
                ..Default::default()
            },
            transport_kbps: 28_000,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 23.976,
                dovi: p8(),
                dv_decision: crate::metadata::DvDecision {
                    capability: nj_platform::devcaps::dv::DvCapability::Supported,
                    presentation: p8().presentation(
                        true,
                        nj_platform::devcaps::dv::DvCapability::Supported,
                        true,
                    ),
                },
                audio: Some(CarriedAudio { sid: 42, ordinal: 1, codec: "eac3".into(), channels: 0, can_normalize_loudness: false, immersive: true }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-auto",
    );

    crate::player::note_original_failure(crate::player::ABR_FAILURE_ORIGINAL_HTTP, 503);

    set_quality(&mut ps, Quality::Original);
    assert_eq!(quality(), Quality::Original);
    assert!(
        matches!(
            cur_delivery(&ps),
            crate::catalog::TranscodeDelivery::FixedHls { .. }
        ),
        "the pump owns the pending codec-changing reload; the menu must not pre-mutate it"
    );
    assert_eq!(
        recover_auto_to_original(&mut ps, 120),
        Some(AutoOriginalReload::Direct)
    );
    assert_eq!(url(&ps), "https://example.invalid/source.mkv");
    assert_eq!(stream_vcodec(&ps), "hevc");
    assert!(stream_dv_presentation(&ps).declared().is_some());
    assert_eq!(
        stream_dovi(&ps),
        p8(),
        "the native Load must regain its Dolby Vision declaration"
    );
    assert_eq!(
        crate::player::SHARED
            .abr_failure_kind
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the new Original attempt supersedes the old probe's failure",
    );
    assert!(!is_transcoding(&ps));
    assert!(
        auto_original_watch(&ps).is_none(),
        "manual Original is not adaptive after the jump"
    );

    reset_session(&mut ps);
    install_active_encoder("");
    crate::player::reset_audio_track();
    crate::player::reset_subtitle();
}

/// Local Auto begins on Original, but it must retain the same source candidate as Remote:
/// after the user selects any fixed rung, Manual Original needs the exact direct/remux
/// declaration to return to. Without it the Local route has no recovery target and asks for
/// one more encoder.
#[test]
#[cfg(feature = "devtriggers")]
fn local_auto_preserves_the_candidate_needed_to_leave_a_fixed_rung() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    let sid = crate::catalog::register_for_test(
        "machine-local",
        "<peer-host-1>.example.invalid",
        32400,
        "token",
        "test-client-id",
    );
    crate::catalog::client_for(sid)
        .expect("server installed")
        .set_link(crate::catalog::probe::Location::Local);
    apply_plan(&mut ps, 
        Plan {
            sid,
            url: "https://example.invalid/source.mkv".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::ProgressiveMkv,
                ceiling: None,
                ..Default::default()
            },
            src_measure: (6_381, 3_832, 2_152),
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 25.0,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 14_778, ordinal: 0, codec: "aac".into(), channels: 0, can_normalize_loudness: false, immersive: false }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-local-original",
    );

    assert!(
        ps.auto_original.is_some(),
        "the route contract must be testable without network by asserting the plan/candidate path"
    );

    reset_session(&mut ps);
    restore_quality(Quality::Original);
    crate::catalog::reset_servers_for_test();
}

/// Returning from a fixed rung to Auto on a Local server must not confuse "the link needs no
/// proof" with "the source needs no feasibility check". Differential against the old route:
/// Local alone set `auto_original = true`, selected progressive MKV, and left this AV1-shaped
/// playback without an HLS controller after the reload.
#[test]
#[cfg(feature = "devtriggers")]
fn local_auto_keeps_hls_when_original_is_infeasible() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::P720);
    let sid = crate::catalog::register_for_test(
        "machine-local-infeasible",
        "<peer-host-1>.example.invalid",
        32400,
        "token",
        "test-client-id",
    );
    crate::catalog::client_for(sid)
        .expect("server installed")
        .set_link(crate::catalog::probe::Location::Local);
    apply_plan(&mut ps, 
        Plan {
            sid,
            tsession: "encoder-fixed".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::ProgressiveMkv,
                ceiling: Some(Quality::P720.ceiling().expect("fixed rung")),
                ..Default::default()
            },
            src_measure: (16_357, 3_840, 1_608),
            auto_original: None,
            ..Default::default()
        },
        "rk-local-infeasible",
    );
    install_active_encoder("encoder-fixed");

    set_quality(&mut ps, Quality::Auto);

    assert_eq!(quality(), Quality::Auto);
    assert!(
        matches!(
            cur_delivery(&ps),
            crate::catalog::TranscodeDelivery::FixedHls { .. }
        ),
        "Auto must rebuild the HLS controller when no native source candidate exists",
    );
    assert_eq!(
        cur_ceiling(&ps),
        Some(crate::abr::Rung::P720.ceiling()),
        "handing a playing 4 Mbps route to Auto must not first replace it with 720 kbps",
    );
    assert!(crate::player::pending_transcode_refresh());

    reset_session(&mut ps);
    restore_quality(Quality::Original);
    install_active_encoder("");
    crate::catalog::reset_servers_for_test();
}

/// The exact remote-control sequence from a 4K Original session: Manual 1080p replaces it
/// with a capped encoder, and Manual Original must use the preserved source candidate to
/// return to direct play. The bad old route kept the progressive-transcode flavor and asked
/// for one more encoder refresh, because the recovery branch only recognized Fixed HLS.
#[test]
#[cfg(feature = "devtriggers")]
fn manual_original_after_a_fixed_rung_returns_to_the_native_source() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/source.mkv".into(),
            tsession: String::new(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::ProgressiveMkv,
                ceiling: None,
                ..Default::default()
            },
            src_measure: (6_381, 3_832, 2_152),
            transport_kbps: 6_381,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 25.0,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 14_778, ordinal: 0, codec: "aac".into(), channels: 0, can_normalize_loudness: false, immersive: false }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-manual-original",
    );
    install_active_encoder("");

    set_quality(&mut ps, Quality::P1080High);
    assert_eq!(quality(), Quality::P1080High);
    assert_eq!(
        cur_ceiling(&ps),
        Some(crate::catalog::Ceiling {
            max_kbps: 20_000,
            max_w: 1920,
            max_h: 1080
        }),
        "a 3832x2152 source cannot fit the 1080p rung, so this pick legitimately starts a cap"
    );
    assert!(
        crate::player::pending_transcode_refresh(),
        "the fixed rung still asks the pump for an encoder reload"
    );
    // The route state the pump owns after that first transition lands.
    { let s = &mut ps; {
        s.tsession = "encoder-1080".into();
        s.cur_contract.remux = false;
        s.cur_contract.no_video_copy = false;
    } };
    install_active_encoder("encoder-1080");

    set_quality(&mut ps, Quality::Original);
    assert_eq!(quality(), Quality::Original);
    assert!(
        !crate::player::pending_transcode_refresh(),
        "Original must not build another capped encoder"
    );
    assert_eq!(
        cur_delivery(&ps),
        crate::catalog::TranscodeDelivery::ProgressiveMkv,
        "the pending recovery owns the route; the pump will perform the native reload"
    );
    assert_eq!(
        recover_auto_to_original(&mut ps, 27),
        Some(AutoOriginalReload::Direct),
        "the pump must restore the exact direct source at the current position"
    );
    assert_eq!(url(&ps), "https://example.invalid/source.mkv");
    assert_eq!(stream_vcodec(&ps), "hevc");
    assert_eq!(stream_acodec(&ps), "aac");
    assert!(!is_transcoding(&ps));
    assert_eq!(cur_ceiling(&ps), None);
    assert_eq!(
        cur_delivery(&ps),
        crate::catalog::TranscodeDelivery::ProgressiveMkv
    );
    assert!(
        auto_original_watch(&ps).is_none(),
        "manual Original is not adaptive"
    );

    reset_session(&mut ps);
    install_active_encoder("");
    crate::player::reset_audio_track();
    crate::player::reset_subtitle();
}

/// Manual Original and Auto Original use the same URL and decoder declaration, but not the
/// same demux worker: Auto's worker owns an `OriginalModeController`. Merely changing the route
/// flag leaves the already-running Manual worker alive with the `None` it captured at spawn,
/// which is the photographed `Auto · controller idle / no adaptive session` state.
#[test]
#[cfg(feature = "devtriggers")]
fn original_to_auto_restarts_the_worker_to_arm_the_watchdog() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Original);
    let sid = crate::catalog::register_for_test(
        "machine-local-original-auto",
        "<peer-host-1>.example.invalid",
        32400,
        "token",
        "test-client-id",
    );
    crate::catalog::client_for(sid)
        .expect("server installed")
        .set_link(crate::catalog::probe::Location::Local);
    apply_plan(&mut ps, 
        Plan {
            sid,
            url: "https://example.invalid/source.mkv".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::ProgressiveMkv,
                ceiling: None,
                ..Default::default()
            },
            src_measure: (23_920, 3_840, 2_160),
            transport_kbps: 23_920,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 24.0,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 42, ordinal: 0, codec: "eac3".into(), channels: 0, can_normalize_loudness: false, immersive: true }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-original-auto",
    );
    assert!(
        auto_original_watch(&ps).is_none(),
        "Manual Original has no watchdog"
    );

    set_quality(&mut ps, Quality::Auto);
    assert!(
        crate::player::pending_adaptive_reload(),
        "the live Manual worker must be replaced so it can capture that watchdog"
    );
    assert!(
        auto_original_watch(&ps).is_none(),
        "the old Manual worker may not be relabelled before the reload is claimed",
    );
    let action = claim_route_action().expect("the adaptive worker reload is explicit");
    assert_eq!(
        action.intent,
        RouteIntent::User(UserRouteIntent::AdaptiveReload),
    );
    finish_route_action(&mut ps, &action, RouteApplyResult::Prepared);
    assert!(
        auto_original_watch(&ps).is_some(),
        "the committed Auto route enables the watchdog for the replacement worker",
    );
    assert!(
        !crate::player::pending_transcode_refresh(),
        "the source and decoder declaration did not change, so this is not a new encode"
    );

    reset_session(&mut ps);
    restore_quality(Quality::Original);
    crate::player::reset_route_requests_for_test(&ps);
    crate::catalog::reset_servers_for_test();
}

#[test]
#[cfg(feature = "devtriggers")]
fn auto_to_an_admitting_fixed_rung_restarts_the_worker_to_remove_the_watchdog() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Auto);
    let sid = crate::catalog::register_for_test(
        "machine-auto-fixed-direct",
        "<peer-host-1>.example.invalid",
        32400,
        "token",
        "test-client-id",
    );
    crate::catalog::client_for(sid)
        .expect("server installed")
        .set_link(crate::catalog::probe::Location::Local);
    apply_plan(&mut ps, 
        Plan {
            sid,
            url: "https://example.invalid/source.mkv".into(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::ProgressiveMkv,
                ceiling: None,
                ..Default::default()
            },
            src_measure: (3_000, 1_280, 720),
            transport_kbps: 3_256,
            auto_original_watched: true,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "h264".into(),
                fps: 24.0,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 42, ordinal: 0, codec: "aac".into(), channels: 0, can_normalize_loudness: false, immersive: false }),
                subtitle_ordinal: None,
            }),
            ..Default::default()
        },
        "rk-auto-fixed-direct",
    );
    assert!(auto_original_watch(&ps).is_some());

    set_quality(&mut ps, Quality::P1080High);

    assert!(
        auto_original_watch(&ps).is_none(),
        "the manual rung has no Auto watchdog"
    );
    assert!(
        crate::player::pending_adaptive_reload(),
        "same direct bytes still need a new non-adaptive worker",
    );
    assert!(
        !crate::player::pending_transcode_refresh(),
        "the 3 Mbps 720p source already satisfies the 20 Mbps fixed rung",
    );

    reset_session(&mut ps);
    restore_quality(Quality::Original);
    crate::player::reset_route_requests_for_test(&ps);
    crate::catalog::reset_servers_for_test();
}

/// Manual Original is not Auto, but it is still a zero-encode route and must be recoverable
/// after the user temporarily selects a fixed rung. This is the Depeche Mode shape: Original
/// direct-play → 480p burned-subtitle transcode → Original.
#[test]
#[cfg(feature = "devtriggers")]
fn manual_original_after_a_fixed_rung_with_a_subtitle_returns_to_direct_play() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    restore_quality(Quality::Original);
    apply_plan(&mut ps, 
        Plan {
            url: "https://example.invalid/source.mkv".into(),
            tsession: String::new(),
            contract: crate::catalog::EncodeContract {
                delivery: crate::catalog::TranscodeDelivery::ProgressiveMkv,
                ceiling: None,
                ..Default::default()
            },
            src_measure: (6_381, 3_832, 2_152),
            transport_kbps: 6_381,
            auto_original: Some(AutoOriginalCandidate {
                url: "https://example.invalid/source.mkv".into(),
                probe_part: "https://example.invalid/source.mkv".into(),
                direct: true,
                vcodec: "hevc".into(),
                fps: 25.0,
                dovi: crate::metadata::Dovi::NONE,
                dv_decision: crate::metadata::DvDecision::NONE,
                audio: Some(CarriedAudio { sid: 14_778, ordinal: 0, codec: "aac".into(), channels: 0, can_normalize_loudness: false, immersive: false }),
                subtitle_ordinal: Some(3),
            }),
            ..Default::default()
        },
        "rk-manual-original-subtitle",
    );
    install_active_encoder("");

    set_quality(&mut ps, Quality::P480);
    { let s = &mut ps; {
        s.tsession = "encoder-480".into();
        s.cur_contract.remux = false;
        s.cur_contract.no_video_copy = false;
    } };
    install_active_encoder("encoder-480");

    set_quality(&mut ps, Quality::Original);
    assert_eq!(
        cur_delivery(&ps),
        crate::catalog::TranscodeDelivery::ProgressiveMkv,
        "Original requests the native reload; it must not build another capped encoder"
    );
    assert_eq!(
        recover_auto_to_original(&mut ps, 938),
        Some(AutoOriginalReload::Direct),
        "a burned fixed rung must still return to the exact direct source"
    );
    assert_eq!(url(&ps), "https://example.invalid/source.mkv");
    assert_eq!(stream_vcodec(&ps), "hevc");
    assert_eq!(stream_acodec(&ps), "aac");
    assert!(!is_transcoding(&ps));
    assert_eq!(
        crate::player::desired_sub_idx(),
        3,
        "the subtitle returns to client rendering"
    );

    reset_session(&mut ps);
    install_active_encoder("");
    crate::player::reset_audio_track();
    crate::player::reset_subtitle();
}
