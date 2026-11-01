//! Shared fixtures and helpers for the `route::decision` test modules split out below.

use super::*;

/// Duplicated from `plan::tests` (a 3-line Dolby Vision Profile 8 fixture the shared-
/// fixture split left on both sides of the module boundary; see that module's `p8` for
/// the sibling copy and its provenance comment).
// Dev-only: used only by the `#[cfg(feature = "devtriggers")]` tests in this module (see the
// comment on the first one).
#[cfg(feature = "devtriggers")]
pub(super) fn p8() -> crate::metadata::Dovi {
    crate::metadata::Dovi {
        present: true,
        profile: 8,
        bl_compat: 1,
        el_present: false,
        ..crate::metadata::Dovi::NONE
    }
}

/// Same provenance as `plan::tests::p5`: single-layer IPT-PQ with no HDR10 fallback.
// Dev-only: used only by the `#[cfg(feature = "devtriggers")]` tests in this module (see the
// comment on the first one).
#[cfg(feature = "devtriggers")]
pub(super) fn p5() -> crate::metadata::Dovi {
    crate::metadata::Dovi {
        present: true,
        profile: 5,
        bl_compat: 0,
        el_present: false,
        ..crate::metadata::Dovi::NONE
    }
}

/// Most route tests install a projection without constructing a native Engine. Make that
/// synthetic boundary explicit in the fixture layer so production `apply_plan` and tests of
/// the start reducer both retain the real `Prepared -> Starting -> result` semantics.
pub(super) fn apply_plan(ps: &mut PlaybackSession, plan: Plan, rk: &str) {
    let start = super::apply_plan(ps, &mut crate::stores::metadata::MetadataStore::default(), plan, rk);
    settle_plan_start_in_unit_test(ps, start);
}

pub(super) fn settle_pending_native_start(ps: &mut PlaybackSession, result: RouteStartResult) -> RouteStartAttempt {
    let transaction = pending_route_start().expect("prepared native start transaction");
    let attempt = claim_route_start_attempt(transaction).expect("physical Load attempt");
    assert!(settle_route_start(ps, attempt, result));
    attempt
}

pub(super) fn rollback_seconds(ps: &mut PlaybackSession) -> Option<i64> {
    rollback_original_recovery(ps).map(|rollback| rollback.offset_ns / 1_000_000_000)
}

pub(super) fn test_original_candidate(subtitle_ordinal: Option<i32>) -> AutoOriginalCandidate {
    AutoOriginalCandidate {
        url: "https://example.invalid/source.mkv".into(),
        probe_part: "https://example.invalid/source.mkv".into(),
        direct: true,
        vcodec: "hevc".into(),
        fps: 23.976,
        dovi: crate::metadata::Dovi::NONE,
        dv_decision: crate::metadata::DvDecision::NONE,
        audio: Some(CarriedAudio {
            sid: 42,
            ordinal: 1,
            codec: "eac3".into(),
            channels: 6,
            can_normalize_loudness: false,
            immersive: true,
        }),
        subtitle_ordinal,
    }
}

// ---- the playing item's SERVER: captured once, carried by value ---------------------------
// A ratingKey, a Part id, a Stream id, a playQueueID and a resume point are all keys on ONE
// server. Every PMS call in this file used to find its server by asking which one was current
// at the instant of the call, so an item borrowed from a shared source was resolved, queued,
// PUT, stopped and — every ten seconds — reported to whichever server the user had since
// wandered off to. None of that is observable from inside the app, which is why these exist.

/// Take the crate-wide serialization lock, empty the server registry AND idle the session, so
/// each test below starts from "nothing installed" and, more importantly, LEAVES the table that
/// way. A test that registers a loopback server and walks off owes the next one an empty table:
/// its ports close when it returns, so what it leaves behind is a `client_opt()` that answers
/// `Some` with a client nothing will ever answer.
///
/// The session goes with it because it holds a `ServerId` INTO that table: a leftover `cur_sid`
/// names a slot the next test is about to re-fill with a different server, and `machine_id` is
/// a cache keyed on exactly that id — which `the_machine_id_cache_is_scoped_to_the_server_that_taught_it`
/// then reads. `reset_session` is the whole-session write, and this is what it is for.
pub(super) fn fresh_registry(ps: &mut PlaybackSession) -> nj_base::testlock::Serial {
    let g = nj_base::testlock::serial();
    // These are process-global route transactions, not Session fields. A host test has no
    // Engine pump to spend them, so leaving either behind makes a later loopback server see a
    // stop for an encoder from a completely different case.
    let _ = take_pending_original();
    // A worker landing (or a presentation hold) a previous test left behind — a test that panicked
    // mid-flight leaves both — belongs to nothing this test owns.
    drop(take_claim_landing());
    clear_injected_fault();
    forget_auto_bitrate();
    crate::player::claim_hold::clear();
    // Establish the idle projection before resetting the reducer: its applied snapshot must
    // describe this test's empty route, not the previous test's final encoder.  Quality is
    // part of the same baseline; cases which need Auto opt in after this boundary and then
    // land a route explicitly.
    reset_session(ps);
    restore_quality(Quality::Original);
    restore_direct_play_mode(DirectPlayMode::Auto);
    // Issue #266: `ResolveEnv::snapshot` reads the enhancement preference from a global, so a
    // test that set it must not leak an enhanced resolve into the next one.
    crate::player::restore_audio_enhancements(crate::catalog::AudioEnhancements::NONE);
    crate::player::reset_route_requests_for_test(ps);
    crate::catalog::reset_servers_for_test();
    crate::player::clear_original_failure();
    g
}

/// A `ServerId` naming a slot nothing is registered in — so `client_for` answers `None` and
/// `build_stream` takes its no-client exit without opening a socket.
pub(super) fn unregistered_sid() -> ServerId {
    let id = ServerId::from_raw((crate::catalog::MAX_SERVERS - 1) as u16);
    assert!(
        crate::catalog::client_for(id).is_none(),
        "the test needs an EMPTY slot"
    );
    id
}

/// Exact `key=value` on a request-line query, so `subtitleStreamID=0` cannot match `88001`.
pub(super) fn query_param<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let query = line.split_once('?')?.1;
    let query = query.split_whitespace().next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

pub(super) fn write_json(socket: &mut std::net::TcpStream, body: &[u8]) {
    use std::io::Write;
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    )
    .expect("headers");
    socket.write_all(body).expect("body");
}

/// One request a [`JfLoopback`] saw: the request line and the body it carried.
#[derive(Clone, Debug)]
pub(super) struct JfRequest {
    pub line: String,
    pub body: String,
    /// When the request finished arriving, for ordering across two loopbacks.
    pub at: std::time::Instant,
}

impl JfRequest {
    /// The JSON body, or `Null` when there was none.
    pub(super) fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// The GUID every loopback item is filed under, and the ratingKey it interns to.
pub(super) const JF_GUID: &str = "0123456789abcdef0123456789abcdef";

pub(super) fn jf_rk() -> String {
    crate::jf::ids::rating_key(JF_GUID)
}

/// A `PlaybackInfo` answer offering the original for direct play (when `direct`) and a
/// conversion at `transcoding_url` (when given) — the shape Jellyfin sends.
pub(super) fn playback_info(direct: bool, container: &str, streams: &str, transcoding_url: Option<&str>) -> String {
    let url = transcoding_url.map_or_else(|| "null".to_string(), |u| format!("{u:?}"));
    format!(
        r#"{{"MediaSources":[{{"Id":"{JF_GUID}","Container":"{container}","Protocol":"File","SupportsDirectPlay":{direct},"SupportsDirectStream":true,"SupportsTranscoding":true,"TranscodingUrl":{url},"MediaStreams":[{streams}]}}],"PlaySessionId":"ps-loopback"}}"#
    )
}

/// `/Users/Me` with the given playback preferences.
pub(super) fn user_config(audio: Option<&str>, play_default: bool, subtitle: Option<&str>, mode: &str) -> String {
    let q = |v: Option<&str>| v.map_or_else(|| "null".to_string(), |v| format!("{v:?}"));
    format!(
        r#"{{"Id":"user-loopback","Name":"loopback","Configuration":{{"AudioLanguagePreference":{},"PlayDefaultAudioTrack":{play_default},"SubtitleLanguagePreference":{},"SubtitleMode":"{mode}"}}}}"#,
        q(audio),
        q(subtitle),
    )
}

/// A loopback Jellyfin registered in the server table: answers `/Users/Me`, `PlaybackInfo` and
/// `/Playback/BitrateTest`, 404s the item reads a PlayQueue would make, and 204s every report.
/// [`JfLoopback::finish`] stops it and yields every request it saw.
pub(super) struct JfLoopback {
    pub sid: ServerId,
    done: std::sync::mpsc::Sender<()>,
    handle: std::thread::JoinHandle<()>,
    log: std::sync::Arc<std::sync::Mutex<Vec<JfRequest>>>,
    info: std::sync::Arc<std::sync::Mutex<String>>,
    converted: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    by_request: std::sync::Arc<std::sync::Mutex<Option<fn(&serde_json::Value) -> String>>>,
}

impl JfLoopback {
    pub(super) fn start(info: String, me: String) -> Self {
        Self::start_slow(info, me, std::time::Duration::ZERO)
    }

    /// [`JfLoopback::start`], holding every `PlaybackInfo` answer for `delay` — a window in which
    /// a negotiating worker is known to be in flight.
    pub(super) fn start_slow(info: String, me: String, delay: std::time::Duration) -> Self {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port() as i32;
        listener.set_nonblocking(true).unwrap();
        let (done, stop) = std::sync::mpsc::channel();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let requests = log.clone();
        let info = std::sync::Arc::new(std::sync::Mutex::new(info));
        let answer = info.clone();
        let converted = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let converted_answer = converted.clone();
        let by_request = std::sync::Arc::new(std::sync::Mutex::new(None::<fn(&serde_json::Value) -> String>));
        let by_request_answer = by_request.clone();
        let handle = std::thread::spawn(move || {
            loop {
                match nj_base::testnet::accept(&listener) {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        let mut reader = BufReader::new(socket.try_clone().expect("clone"));
                        let mut line = String::new();
                        reader.read_line(&mut line).expect("request line");
                        let mut length = 0usize;
                        loop {
                            let mut h = String::new();
                            reader.read_line(&mut h).expect("header");
                            if h == "\r\n" || h.is_empty() {
                                break;
                            }
                            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                                length = v.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0; length];
                        let _ = reader.read_exact(&mut body);
                        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                        // Logged before the answer, so a client that has been answered can rely on
                        // the log already holding its request.
                        requests.lock().unwrap_or_else(|e| e.into_inner()).push(JfRequest {
                            line: line.clone(),
                            body: String::from_utf8_lossy(&body).into_owned(),
                            at: std::time::Instant::now(),
                        });
                        if path.starts_with("/Users/Me") {
                            write_json(&mut socket, me.as_bytes());
                        } else if path.contains("/PlaybackInfo") {
                            std::thread::sleep(delay);
                            let ask = serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default();
                            let modelled = *by_request_answer.lock().unwrap_or_else(|e| e.into_inner());
                            let no_direct = ask["EnableDirectPlay"] == false;
                            let body = modelled
                                .map(|f| f(&ask))
                                .or_else(|| no_direct.then(|| converted_answer.lock().unwrap_or_else(|e| e.into_inner()).clone()).flatten())
                                .unwrap_or_else(|| answer.lock().unwrap_or_else(|e| e.into_inner()).clone());
                            write_json(&mut socket, body.as_bytes());
                        } else if path.starts_with("/Playback/BitrateTest") {
                            let n: usize = query_param(&line, "Size").and_then(|v| v.parse().ok()).unwrap_or(0);
                            let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n");
                            // Paced to roughly 10 Mbit/s: an unpaced loopback measures a link no
                            // real server has. Auto reads for a fixed window and hangs up long
                            // before a 64 MiB body ends, so a failed write ends the answer — this
                            // server is single-threaded, and the next request waits behind it.
                            let chunk = [0x55; 50_000];
                            let mut left = n;
                            while left > 0 {
                                let k = left.min(chunk.len());
                                if socket.write_all(&chunk[..k]).is_err() {
                                    break;
                                }
                                left -= k;
                                std::thread::sleep(std::time::Duration::from_millis(40));
                            }
                        } else if line.starts_with("GET ") {
                            let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        } else {
                            let _ = socket.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if stop.try_recv().is_ok() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(e) => panic!("loopback accept: {e}"),
                }
            }
        });
        // Named by port: the table files a server under its name, and two loopbacks are two servers.
        let name = format!("jf-loopback-{port}");
        let sid = crate::catalog::register_for_test(&name, "127.0.0.1", port, "jf-token", "jf-loopback-client");
        let client = crate::catalog::client_for(sid).expect("loopback registered");
        crate::jf::seat::register_with(client.origin(), crate::jf::seat::Seat {
            user_id: "user-loopback".into(),
            ..Default::default()
        });
        let _ = jf_rk();
        JfLoopback { sid, done, handle, log, info, converted, by_request }
    }

    /// Answer every later `PlaybackInfo` with `info`.
    pub(super) fn answer_playback_info(&self, info: &str) {
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = info.to_string();
    }

    /// Answer a `PlaybackInfo` that withdraws direct play with `info` instead — the server fills
    /// `TranscodingUrl` only when direct play is not the answer.
    pub(super) fn answer_converted(&self, info: &str) {
        *self.converted.lock().unwrap_or_else(|e| e.into_inner()) = Some(info.to_string());
    }

    /// Answer every later `PlaybackInfo` with what `f` makes of its request body — for a server
    /// whose answer depends on the profile it was sent, which a canned answer cannot model.
    pub(super) fn answer_by(&self, f: fn(&serde_json::Value) -> String) {
        *self.by_request.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// Every request seen so far, while the loopback keeps serving.
    pub(super) fn seen(&self) -> Vec<JfRequest> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Stop serving and yield every request seen. Empties the server table: a test running two
    /// loopbacks finishes them together.
    pub(super) fn finish(self) -> Vec<JfRequest> {
        let _ = self.done.send(());
        let requests = self.log.clone();
        self.handle.join().expect("loopback thread");
        crate::catalog::reset_servers_for_test();
        let seen = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
        seen
    }
}

/// Negotiate a conversion of `rk` on `sid` under `session`, so the session table holds the
/// PlaySessionId a later stop reports.
pub(super) fn negotiate_jf_session(sid: ServerId, rk: &str, session: &str, contract: crate::catalog::EncodeContract) {
    let client = crate::catalog::client_for(sid).expect("loopback registered");
    let spec = transcode_spec(rk, session, session, "", crate::catalog::TranscodeOffset::from_seconds(0), 0, 0, contract, nj_platform::devcaps::caps());
    assert!(
        matches!(client.transcode(&spec), crate::catalog::Negotiation::Playable(_)),
        "the loopback negotiates {session}"
    );
}

/// The `PlaybackInfo` request body among `requests`.
pub(super) fn playback_info_body(requests: &[JfRequest]) -> serde_json::Value {
    requests
        .iter()
        .find(|r| r.line.contains("/PlaybackInfo"))
        .unwrap_or_else(|| panic!("PlaybackInfo was never asked: {requests:?}"))
        .json()
}

pub(super) fn fourk_item(
    sid: ServerId,
    audio: Vec<crate::metadata::Stream>,
) -> crate::metadata::PlayingItem {
    fourk_item_with_subs(sid, audio, Vec::new())
}

pub(super) fn fourk_item_with_subs(
    sid: ServerId,
    audio: Vec<crate::metadata::Stream>,
    subs: Vec<crate::metadata::Stream>,
) -> crate::metadata::PlayingItem {
    crate::metadata::PlayingItem {
        sid,
        rk: "rk-4k".into(),
        audio,
        subs,
        video_fps: 23.976,
        width: 3840,
        height: 2160,
        bitrate: 48_000,
        dovi: crate::metadata::Dovi::NONE,
        markers: Vec::new(),
        chapters: Vec::new(),
        blur: None,
    }
}

pub(super) fn eac3_track() -> crate::metadata::Stream {
    crate::metadata::Stream {
        id: 36014,
        index: 1,
        lang_code: "eng".into(),
        codec: "eac3".into(),
        channels: 6,
        default: true,
        selected: true,
        profile: "dolby digital plus + dolby atmos".into(),
        ..Default::default()
    }
}

/// The pipeline tier's entry into HLS, both ways round. Differential against the old seam,
/// which had ONE way in: declare an Original source rate no link could carry and let the
/// starvation horizon fire on a reserve that was visibly filling. That entry stopped working
/// when the horizon started requiring an observed drain, and it should have — the reserve was
/// growing, so nothing was starving. This is the honest replacement.
/// The raster every pre-2026-08-28 `auto_network` case had hardcoded into the function.
pub(super) const HD: (u16, u16) = (1_920, 1_080);

