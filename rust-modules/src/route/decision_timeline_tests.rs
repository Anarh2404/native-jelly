//! Machine-id cache scoping and playback timeline/scrobble lease tests.

use super::*;
#[allow(unused_imports)]
use super::test_support::*;
use super::test_support::apply_plan;

/// The `/identity` cache is keyed to the server that taught it. It feeds
/// `uri=server://{machineIdentifier}/…` on the PlayQueue POST, so one cached globally is
/// server A's fingerprint sent to server B — naming a machine B has never heard of, on a POST
/// that is best-effort and therefore fails silently.
#[test]
fn the_machine_id_cache_is_scoped_to_the_server_that_taught_it() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    let a = ServerId::from_raw((crate::catalog::MAX_SERVERS - 2) as u16);
    let b = ServerId::from_raw((crate::catalog::MAX_SERVERS - 1) as u16);
    assert!(crate::catalog::client_for(a).is_none() && crate::catalog::client_for(b).is_none());

    apply_plan(&mut ps, 
        Plan {
            sid: a,
            machine_id: "MACHINE-A".into(),
            ..Default::default()
        },
        "rk-a",
    );
    assert_eq!(
        ResolveEnv::snapshot(&ps, crate::stores::metadata::MetadataStore::default().view(), a, "rk-a").machine_id,
        "MACHINE-A",
        "its own server reuses it"
    );
    assert_eq!(
        ResolveEnv::snapshot(&ps, crate::stores::metadata::MetadataStore::default().view(), b, "rk-b").machine_id,
        "",
        "another server must re-ask rather than inherit A's fingerprint"
    );

    // …and an empty `machine_id` means "leave the cache alone", not "the cache is now B's"
    apply_plan(&mut ps, 
        Plan {
            sid: b,
            ..Default::default()
        },
        "rk-b",
    );
    assert_eq!(ResolveEnv::snapshot(&ps, crate::stores::metadata::MetadataStore::default().view(), a, "rk-a").machine_id, "MACHINE-A");
    assert_eq!(ResolveEnv::snapshot(&ps, crate::stores::metadata::MetadataStore::default().view(), b, "rk-b").machine_id, "");
}

/// The Jellyfin item every report below is about, on whichever loopback it plays from.
const OTHER_GUID: &str = "fedcba9876543210fedcba9876543210";

fn jf_loopback() -> JfLoopback {
    JfLoopback::start(String::new(), user_config(None, true, None, "Default"))
}

/// The reports `requests` holds: `(path, ItemId)` for every `/Sessions/Playing*` POST.
fn reports(requests: &[JfRequest]) -> Vec<(String, String)> {
    requests
        .iter()
        .filter(|r| r.line.starts_with("POST /Sessions/Playing"))
        .map(|r| {
            let path = r.line.split_whitespace().nth(1).unwrap_or("").to_string();
            let item = r.json()["ItemId"].as_str().unwrap_or("").to_string();
            (path, item)
        })
        .collect()
}

/// **The one that had to ship in the same commit as `cur_sid`.** The progress report runs on a
/// worker every ten seconds and used to read the current server fresh on each tick — the only
/// place in the playback path with no capture at all. Split from the rest, the app resolves
/// correctly, plays correctly, and quietly writes the resume point of a friend's film onto your
/// own server for as long as it plays.
///
/// Two servers on loopback, the item playing from B, the user browsing A: the report must land
/// on B. The closing report to A is the control — it proves the two loopbacks are
/// distinguishable, so "A heard nothing" is a fact about the routing and not about a listener
/// that never worked.
#[test]
#[cfg(feature = "devtriggers")]
fn the_timeline_reaches_the_server_the_item_came_from_not_the_current_one() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    assert!(nj_net::net::global_init() && crate::curlio::available());
    let server_a = jf_loopback();
    let server_b = jf_loopback();
    let (a, b) = (server_a.sid, server_b.sid);
    assert_ne!(a, b, "two servers, two slots");
    let rk_a = crate::jf::ids::rating_key(OTHER_GUID);
    let rk_b = jf_rk();

    // an item from B starts playing, then the user walks back to their OWN server's Home
    apply_plan(&mut ps,
        Plan {
            sid: b,
            url: "https://example.invalid/b.mkv".into(),
            sess: "timeline-session-b".into(),
            ..Default::default()
        },
        &rk_b,
    );
    assert!(crate::catalog::set_current(a));
    assert_eq!(cur_sid(&ps), b, "what is PLAYING does not move when the browsed server does");

    let lease_b = begin_timeline_reporting(&ps).expect("B timeline lease");
    assert!(report_timeline(&lease_b, crate::catalog::TimelineState::Playing, 1_000, 2_000));
    assert_eq!(
        reports(&server_b.seen()),
        [("/Sessions/Playing".to_string(), JF_GUID.to_string())],
        "B hears its own item start"
    );
    assert!(reports(&server_a.seen()).is_empty(), "the current server must not receive another server's progress");

    // control: a complete A projection reaches A, so the assertion above is about routing.
    apply_plan(&mut ps,
        Plan {
            sid: a,
            url: "https://example.invalid/a.mkv".into(),
            sess: "timeline-session-a".into(),
            ..Default::default()
        },
        &rk_a,
    );
    let lease_a = begin_timeline_reporting(&ps).expect("A timeline lease");
    assert!(report_timeline(&lease_a, crate::catalog::TimelineState::Stopped, 0, 2_000));
    // A's playback stops before any report of its own, so its start is said first (W2).
    assert_eq!(
        reports(&server_a.seen()),
        [
            ("/Sessions/Playing".to_string(), OTHER_GUID.to_string()),
            ("/Sessions/Playing/Stopped".to_string(), OTHER_GUID.to_string()),
        ],
        "A hears its own reports"
    );

    let heard_b = server_b.finish();
    let _ = server_a.finish();
    assert_eq!(reports(&heard_b).len(), 1, "B heard nothing of A's item");
    reset_session(&mut ps);
}

#[test]
#[cfg(feature = "devtriggers")]
fn replacement_timeline_waits_for_the_announced_old_stop_boundary() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    use std::time::Duration;

    let _g = fresh_registry(&mut ps);
    assert!(nj_net::net::global_init() && crate::curlio::available());
    drain_scrobble();
    reset_session(&mut ps);
    reset_player_control_for_test(&ps);
    let old_server = jf_loopback();
    let new_server = jf_loopback();
    let rk_old = jf_rk();
    let rk_new = crate::jf::ids::rating_key(OTHER_GUID);
    apply_plan(&mut ps,
        Plan {
            sid: old_server.sid,
            sess: "logical-old".into(),
            ..Default::default()
        },
        &rk_old,
    );
    // The test isolates timeline ordering; keep an encoder stop out of the old server's log.
    install_active_encoder("");

    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }
    let mut release = ReleaseOnDrop(Some(release_tx));
    let old_reporter = std::thread::spawn(move || {
        let _ = release_rx.recv();
    });
    scrobble_stop(&mut ps,
        Some((rk_old.clone(), 11_000, 20_000)),
        Some(old_reporter),
        true,
    );

    apply_plan(&mut ps,
        Plan {
            sid: new_server.sid,
            url: "https://example.invalid/new.mkv".into(),
            sess: "logical-new".into(),
            ..Default::default()
        },
        &rk_new,
    );
    let lease = begin_timeline_reporting(&ps).expect("replacement reporter lease");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let replacement = std::thread::spawn(move || {
        let sent = report_timeline(&lease, crate::catalog::TimelineState::Playing, 1_000, 20_000);
        let _ = done_tx.send(sent);
    });

    std::thread::sleep(Duration::from_millis(250));
    assert!(
        reports(&new_server.seen()).is_empty() && reports(&old_server.seen()).is_empty(),
        "replacement playing escaped before the old reporter/stop boundary",
    );
    release.0.take().unwrap().send(()).unwrap();
    assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)), Ok(true));
    replacement.join().unwrap();
    drain_scrobble();

    let at = |server: &JfLoopback, path: &str| {
        server.seen().into_iter().find(|r| r.line.starts_with(&format!("POST {path} "))).map(|r| r.at)
    };
    let stopped = at(&old_server, "/Sessions/Playing/Stopped").expect("old stopped report never reached the server");
    let playing = at(&new_server, "/Sessions/Playing").expect("replacement playing report never reached the server");
    assert!(stopped <= playing, "replacement report arrived before stopped");
    // The old playback stops before any report of its own, so its start is said first (W2).
    assert_eq!(
        reports(&old_server.seen()),
        [
            ("/Sessions/Playing".to_string(), JF_GUID.to_string()),
            ("/Sessions/Playing/Stopped".to_string(), JF_GUID.to_string()),
        ],
        "the old server hears exactly its start and stop"
    );
    assert_eq!(
        reports(&new_server.seen()),
        [("/Sessions/Playing".to_string(), OTHER_GUID.to_string())],
    );

    let _ = old_server.finish();
    let _ = new_server.finish();
    reset_session(&mut ps);
    install_active_encoder("");
    reset_player_control_for_test(&ps);
}

#[test]
fn every_concurrent_scrobble_drain_waits_for_the_same_taken_handle() {
    use std::time::Duration;

    let join = std::sync::Arc::new(ScrobbleJoin::new());
    let generation = join.reserve();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = release_rx.recv();
    });
    join.install(generation, worker);

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let first_join = join.clone();
    let first_done = done_tx.clone();
    let first = std::thread::spawn(move || {
        first_join.drain();
        let _ = first_done.send(1);
    });
    let second_join = join.clone();
    let second = std::thread::spawn(move || {
        second_join.drain();
        let _ = done_tx.send(2);
    });

    assert!(
        done_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "a drainer escaped after another thread took the JoinHandle",
    );
    release_tx.send(()).unwrap();
    let mut completed = [
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
    ];
    completed.sort_unstable();
    assert_eq!(completed, [1, 2]);
    first.join().unwrap();
    second.join().unwrap();
}

#[test]
fn timeline_lease_cannot_cross_engine_teardown() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = nj_base::testlock::serial();
    reset_player_control_for_test(&ps);
    reset_session(&mut ps);
    apply_plan(&mut ps, 
        Plan {
            sid: ServerId::from_raw(0),
            url: "https://example.invalid/old.mkv".into(),
            sess: "logical-old".into(),
            pq_id: "pq-old".into(),
            pq_item_id: "pqi-old".into(),
            audio: Some(CarriedAudio::named(7, -1)),
            sub_sid: 9,
            ..Default::default()
        },
        "rk-old",
    );
    install_active_encoder("wire-old");
    let old = begin_timeline_reporting(&ps).expect("old reporter");
    let before = timeline_snapshot(&old, crate::catalog::TimelineState::Playing, 1_000, 2_000)
        .expect("old projection");
    assert_eq!(before.rating_key, "rk-old");
    assert_eq!(before.session, "wire-old");
    assert_eq!((before.audio_stream_id, before.subtitle_stream_id), (7, 9));

    begin_engine_teardown(true);
    assert!(
        timeline_snapshot(&old, crate::catalog::TimelineState::Playing, 1_500, 2_000).is_none(),
        "an old reporter must not sample any field after its Engine is retired"
    );
    reset_player_control_for_test(&ps);
    reset_session(&mut ps);
}

/// Field report, 0.7.0 prep: an autoplayed trailer whose Original open failed was re-opened as an
/// HLS transcode, and that engine posted `timeline playing t=…/178s` every ten seconds — watch
/// state written to the viewer's account by a trailer they never chose to play. The failure path
/// had cleared the session's preview flag while the abandoned Load still held the engine, so the
/// reload that followed started a reporter. What a session may write is a property of the REQUEST
/// that produced it, and no teardown bookkeeping may turn a preview into a playback.
#[test]
fn a_session_resolved_for_a_preview_never_reports_a_timeline() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = nj_base::testlock::serial();
    reset_player_control_for_test(&ps);
    reset_session(&mut ps);
    ps.request = Some(PlaybackRequest {
        sid: ServerId::from_raw(0),
        rk: "trailer".into(),
        part: "/library/parts/1/file.mp4".into(),
        vcodec: "h264".into(),
        acodec: "aac".into(),
        title: "Trailer".into(),
        ctx: crate::metadata::TRAILER_CONTEXT.into(),
        preview: true,
        tracks: None,
    });
    apply_plan(
        &mut ps,
        Plan {
            sid: ServerId::from_raw(0),
            url: "https://example.invalid/trailer.mp4".into(),
            sess: "logical-trailer".into(),
            ..Default::default()
        },
        "trailer",
    );
    assert!(is_preview(&ps));
    assert!(preview_request(&ps));
    assert!(begin_timeline_reporting(&ps).is_none(), "a preview has no timeline");
    clear_preview(&mut ps);
    assert!(preview_request(&ps), "clearing the preview flag must not make it a playback");
    assert!(
        begin_timeline_reporting(&ps).is_none(),
        "a session resolved for a preview must never report a timeline"
    );
    reset_player_control_for_test(&ps);
    reset_session(&mut ps);
}

/// **A stop that finished the item marks it played, after its `stopped` report, only below the
/// server's own threshold.** Jellyfin 12.0 leaves an episode unwatched at a stop at 89 % and marks it
/// at 91 %, and long credits start before that. The flag is taken by the stop that reads it: the next
/// stop, which the viewer did not finish, posts `stopped` alone.
#[test]
#[cfg(feature = "devtriggers")]
fn a_finished_stop_marks_the_item_played_only_below_the_threshold() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    assert!(nj_net::net::global_init() && crate::curlio::available());
    drain_scrobble();
    reset_session(&mut ps);
    reset_player_control_for_test(&ps);
    let server = jf_loopback();
    let rk = jf_rk();
    let stop = |ps: &mut crate::route::PlaybackSession, finished: bool, t_ms: i64| {
        apply_plan(ps, Plan { sid: server.sid, sess: "logical".into(), ..Default::default() }, &rk);
        install_active_encoder("");
        ps.finished = finished;
        scrobble_stop(ps, Some((rk.clone(), t_ms, 60_000)), None, true);
        drain_scrobble();
        assert!(!ps.finished, "the stop took the flag, so it cannot mark the next item");
    };
    let marks = |lines: Vec<String>| lines.iter().filter(|l| l.starts_with("POST /UserPlayedItems/")).count();
    let lines = || server.seen().into_iter().map(|r| r.line).collect::<Vec<_>>();

    // The viewer left at 85 %: `stopped`, then the mark.
    stop(&mut ps, true, 51_000);
    let seen = lines();
    assert_eq!(marks(seen.clone()), 1, "{seen:?}");
    let at = |needle: &str| seen.iter().position(|l| l.starts_with(needle)).unwrap();
    assert!(at("POST /Sessions/Playing/Stopped ") < at("POST /UserPlayedItems/"), "{seen:?}");
    // Past the threshold the server counted it already.
    stop(&mut ps, true, 55_000);
    assert_eq!(marks(lines()), 1, "a second mark past the threshold: {:?}", lines());
    // An ordinary stop at the same place as the first: not finished, so no mark.
    stop(&mut ps, false, 51_000);
    assert_eq!(marks(lines()), 1, "a stop the viewer did not finish was marked: {:?}", lines());

    let _ = server.finish();
    reset_session(&mut ps);
    install_active_encoder("");
    reset_player_control_for_test(&ps);
}

/// A flag the previous stop never took (the stop posted nothing) must not mark the next item.
#[test]
fn a_play_request_clears_a_stale_finished_flag() {
    let mut ps = crate::route::PlaybackSession::IDLE;
    let _g = fresh_registry(&mut ps);
    reset_session(&mut ps);
    ps.finished = true;
    let mut meta = crate::stores::metadata::MetadataStore::default();
    let _ = request_play(&mut ps, &mut meta, ServerId::from_raw(0), "1", "", "", "", "", "", 0);
    assert!(!ps.finished);
    // The request is live and its pending flag is process-global: a later test would read Resolving.
    cancel_play(&mut ps);
    assert!(!play_pending());
    reset_session(&mut ps);
}
