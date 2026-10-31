# player/ — the buffer-feed video engine

This is the in-process **buffer-feed** playback engine (was `src/playback.c`): it pulls the part
stream, demuxes it to access units, and `Feed()`s them to LG's StarfishMediaAPIs while `libAcbAPI`
binds the decoded sink to the hardware video plane. The Starfish/ACB calls cross into C at the seam
`src/starfish.c` (outside this dir — edits there almost always pair with edits here); the **ABI and
bind-order gotchas for that seam live in THIS file**, below. `docs/agent-reference.md` carries only the
one-paragraph playback summary.

## Pipeline

In-process is the point: ACB can only bind an app-owned sink, which the earlier URI/out-of-process
path (`com.webos.media/load`, `start_playback()`) could not provide — that path is kept only as
dead-ish reference, and `docs/buffer-feed-plan.md` records the pivot (treat it as history, not
spec). The stream side: `PMS HTTP GET` → demux → per-lane access-unit
queues with byte-cap backpressure (`aq.rs`) → the pump `Feed()`s each AU to the Starfish pipeline.
The demuxer is **`ff.rs` — the libavformat the app BUNDLES (not the TV's), over a custom AVIO on
one of TWO transports** (design record: `docs/ffmpeg-demuxer-plan.md`; the hand-rolled `mkv.rs`
fallback is retired/deleted). **Which transport is decided in `ff::demux` from the part URL's
scheme, or by a plaintext open's redirect to https (`stream::redirect` hands that hop to curl)** —
`AvioState` holds a source enum and `read_cb`/`seek_cb` dispatch: `http` → `stream.rs`'s raw socket, `https` → `crate::curlio`. That matters *here* because teardown is a
different mechanism per arm: `engine::teardown` fires `stream::http_shutdown` at the socket **and**
`curlio::abort_active()` at the curl source's wake pipe, since a thread parked in `curl_multi_wait`
is not one any `shutdown(2)` of ours can reach. Exactly one of the two ever has anything to do. Ours ships beside the binary as `libav*-plx.so.*`, is `dlopen`'d by absolute path
and pinned to majors 63/63/61, and is built `--disable-network` with `file` as its only protocol —
so the AVIO is not merely how bytes reach it today, it is the only way they *can*. See the root
`docs/agent-reference.md` linking section for why. It
emits Annex-B video AUs (param sets prepended at each keyframe) and, for AV1, Section-5 temporal units (TD OBU first, the av1C sequence header and metadata OBUs in-band on keyframes; `/tmp/nativejelly-av1obu` = notd,noseq,raw tokens varies it on a debug build), for VP9 the demuxed packet unchanged (a frame carries its own headers and libpf's `video/x-vp9` caps have no `codec_data`), and AC3/EAC3, ADTS-framed AAC, or DTS core audio frames,
and seeks by time via `av_seek_frame` (libavformat's own Cues index).

The C3 native Opus feed (`OPUS` plus `opusInfo`, validated source headers and raw
packets) and firmware/rank restrictions are staged. Ordinary policy is mono/stereo; wider
family-1 layouts require an exact typed debug-fixture override. Auto/Force routing and
Jellyfin profiles still withhold Opus. The groundwork is in: one capability snapshot per play
attempt (`route::attempt_caps`, shared with the DeviceProfile), build-scoped refusal records, and
one bounded audio-conversion recovery per request (`route::open_recovery`, driven by
`pump::recover_refused_audio`), which also holds the one video rung: a manual pick's video refused
before its first picture is encoded once under a 1080p ceiling. Enabling waits on real-server and
TV acceptance. See
`docs/opus.md` for its exact subset and verification limits; this is not yet a shipping
direct-play capability.

## Threading model (this is the whole ballgame)

- `engine.rs` — the **main-thread-confined** session object. Every ACB/Starfish *control* call
  happens on the main thread except two Starfish verbs: `Load` (on `load_thread`) and a real stop's
  `Unload` (on the `sf-unload` worker, below). `engine` spawns the workers below.
- **A real stop's `Unload` runs off the frame (`engine::PendingRelease`, upstream #758).** On the
  webOS 6.5.3 set the frame that handled BACK in the player took 61-81 ms, nearly all of it in
  `navcommit` where the release ran (FRAMEDROP, three plays, 2026-10-30). `teardown` (real stop
  only: `for_reload == false`) moves the native phase to `Releasing`, hands the Unload to an
  `sf-unload` worker with an `UnloadTicket`, and parks the worker on the adapter. The rest of the
  release — callback gate, epoch retire, ACB plane unload, D1 or quarantine, `destroy_window` —
  stays on the main thread and runs in `engine::poll_release` (the pure machine `release.rs`
  decides): on the first frame after the Unload returns (`run.rs` → `poll_release_frame`). Nothing
  blocks on the worker. A start over a running Unload returns `BufferfeedStartOutcome::Deferred` (the
  player reads Resolving) and runs the frame the tail has run; `pa.mt()`/`pa.split()` hand out the
  token at once while `player::sink()` answers `RefusingSink` (`player::FENCE`: every verb refused,
  the memoized queries real) for the `Releasing` phase. Past `RELEASE_DEADLINE` (10 s) a start is
  refused with `FailureKind::ReleaseTimeout` (Try Again), and the object is leaked, never destroyed
  under the worker, until the Unload returns. A stop always cancels a queued start. The foreground
  restore follows only a launched attempt, so a start it finds deferred is cancelled and takes the
  refusal edge (the prepared route stays; Play retries). `shutdown` waits through `exit_native`,
  bounded by the same deadline; a worker still in firmware leaks the parked release and any abandoned
  Load, skips `SDL_Quit` and ends in `exit_now` without dropping the `App`. While `Releasing`, only
  that epoch's `UnloadCompleted` callback is admitted (it is D1's evidence, and it counts into nothing
  the next session owns) and no native session can begin. Reloads, suspend and the decoder-release
  park (`for_reload == true`) keep the synchronous release. The log reads `native lifecycle: off-frame
  Unload epoch=N released X ms after the stop`. Not yet measured on the TV after the change.
- `pump.rs` — the **main-thread pump** (was `bufferfeed_pump`): each frame it drives bind → Play →
  feed and services seeks.
- `threads.rs` — the workers beside the demuxer: **`load_thread`** (construct Starfish + `Load()`,
  which owns its own GMainContext) and **`timeline_thread`** (the playback reporter:
  `/Sessions/Playing` at the first picture, then `/Progress` every 10 s and at once on
  `player::report_now` — pause, resume, seek, track change). The **demux thread body is `ff::demux`** (spawned by `engine::start_bufferfeed`): open
  the part URL, read+convert packets, push AUs to the two lanes; **and service seeks** — it
  `av_seek_frame`s on `seek_to_ns` between two `av_read_frame` calls, which is the whole seek
  mechanism (nothing interrupts it; see the seek gotcha below).
- `shared.rs` — the engine's cross-thread transport, callback and clock state (each field replaces a
  C `volatile` global — `g_*`). Route ownership and route-changing intents are the separate
  process-wide authority in `route::PLAYER_CONTROL`. Put new state under the owner whose invariant
  it belongs to; never smuggle it through a raw static.

**The main-thread rule is compiler-enforced.** The seam is the `VideoSink` trait in `nj_platform::tv::sink` (`rust-modules/platform/src/tv/sink.rs`)
(Starfish-shaped: one method per verb, reached through `player::sink()`). `ffi.rs`'s `extern "C"`
declarations are private to that module and only `StarfishSink` there implements the trait on the
television; `ffi_host.rs`'s `HostSink` is the simulator's. Every verb but `load` and `unload`
takes a `task::MainThread` (the memoized reads `feed_abi_is_cow`, `window_mode` and `window_id`
take nothing); `unload` takes an `UnloadTicket`, which only a `&MainThread` can issue and which is
`Send`, so the main thread decides when an Unload happens and a worker may carry it out.
`MainThread` is a `!Send` ZST the application mints once. Since phase 9 it mints exactly one
and `boot` MOVES it into `player::adapter::PlayerAdapter` (`App.adapters.player`), which also owns
the native session that was the `static mut ENGINE`: the seam is reached as `pa.mt()`, the session as `pa.engine()` / `pa.split()`, and a function that
touches the session takes `pa: &mut PlayerAdapter` where it used to take `mt: &MainThread`. Moving
any of it onto a thread stops compiling (the closure captures a `&MainThread`, which `task::spawn`
rejects), and two live `&mut` to the session no longer needs a convention — it does not compile,
which is what turned `pump`'s "reload REPLACES the ENGINE, so `eng` dangles" comment into a rule
the borrow checker keeps. Two intentional holes, both worth knowing: `load` takes **no** token
because `load_thread` runs it off-main by design (and `unload` takes only a ticket, for the
`sf-unload` worker), and `MainThread::assume()` is callable — so an
`unsafe` block inside a worker still defeats this. The rule for new code: take the token **iff**
you reach the seam, the adapter **iff** you reach the session, so a signature keeps meaning
something.

## Gotchas that bite (all verified in code)

- **C-from-C++ Starfish calls** go through `extern … __asm__("<mangled>")`. Each Load gets a fresh,
  16-byte-aligned `SfSlot` whose `object[65536]` is constructed in place by the ctor symbol. Slot
  addresses are never reused or freed: a late firmware callback carries only that address, so a
  retired slot stays in the registry until process exit. Call the destructor only through the
  gated teardown path and **never** hand the object to C++ `new`/`delete` (its real size is unknown).
  Methods returning a `std::string` use a hidden sret first-arg; read the `char*` at offset 0 (SSO)
  for short replies like `"Ok"`/`"BufferFull"`.
- **Dolby Vision and Dolby Atmos have their own document: `docs/dolby-vision.md`.** A DV node is
  emitted only after the boot-time configd probe definitely confirms hardware support. The route
  freezes that capability + presentation decision, and every Load/reload/recovery consumes the
  stored presentation rather than re-reading the later cache; `dvnonode` is the logged diagnostic
  exception. Atmos routing and ACB forwarding are independent and unchanged. The payload evidence,
  Profile 5 one-tick fix and instrument traps live in that document.
- **The Load's `adaptiveStreaming` ceiling is derived per session (`engine::sink_envelope`), and
  it was a 4K60 constant for EVERY codec until 2026-09-03 — which on webOS 10 refused every H.264
  stream: `docs/webos10-resource-allocation.md`.** Lab-measured 2026-08-27 on release 10.3.1: the
  pipeline allocates against the DECLARED ceiling rather than the bitstream, so the Load came back
  `num=601 Resource Allocation Error`, while the identical envelope carrying `"H265"` played in the
  same session, and a 1920x1080 H.264 declaration loaded in 117 ms. The rule now is exactly that
  measurement and no more: HEVC keeps 4K60; H.264 declares 1920x1080@60 when the session's widest
  raster (`route::sink_max_raster` — source for direct play, the ceiling for a fixed quality, the
  catalog's widest feasible actuator for Auto, since a rung commit never re-Loads) fits FHD, and
  stays 4K60 otherwise; the device table's per-codec row (`devcaps::Caps::{h264_row,hevc_row}`,
  frame rate included) clamps it only when the table was actually read. The `load:` line carries
  `max=WxH@F`, the pipeline echoes what it was handed as `smp_cb type=5`, and the pipeline tier
  grades both (`load_max`, `sink_echo`). **Also since 2026-09-03: a `type=18` before any picture
  publishes `load_failed`** (`sf_on_event_inner`), so an asynchronous refusal reaches the failure
  read-out instead of parking the player in Connecting on a black screen (lab report §3.5).
  **And the frame rate IS a discriminator, measured on the dev set the same day:** a 4K H.264 24p
  direct play declared at 60 makes the pipeline announce `frameRate:24` and then `:30` for the same
  stream (a 24p picture on a 30 fps lattice — the judder the maintainer saw on the Auto 4K rung);
  declared at 24 or 30 it announces 24 once and holds it — and the sink's displayed-frame counter
  measured the picture: **13.0 fps presented under the 60 declaration, 24.1 under 24**. So H.264
  declares the stream's rate class
  (`fps_class`), HEVC keeps 60 (4K HEVC under 60 holds 24), and a transcode with no known rate
  keeps 60. AV1 (only on a set whose codec table lists an AV1 decoder) keeps HEVC's 4K60 rule,
  clamped by `Caps::av1_row`; the clamp is unmeasured (one owner reports AV1 playing on a webOS 10.3.1 set, with no log). `/tmp/nativejelly-sinkmax=WxH@F`
  overrides the envelope for the legs still unrun on 10.3.1.
- **There IS a presented-frame instrument now, and it is not a GStreamer trace.** The payload's
  `streamQualityInfo` / `streamQualityInfoNonFlushable` keys make libpf read the video sink's
  `dropped-frames` / `non-flushable-displayed-frames` properties on the same 200 ms timer as the
  position tick and forward them as callback types 46 / 47 (decompiled from libpf
  `CustomPipeline::updatePeriodicalInfo`, 2026-09-03). They land in the event log as
  `smp_cb type=47 num=<frames shown in the last poll>` (per-interval on this firmware, ~5 at 24p)
  AND, normalised for the numbering shift (46/47 here are 48/49 on webOS 5+ — `sink_counter_kind`),
  as `sink: displayed=<n>` / `sink: dropped=<n>`, which is the line the harness reads;
  `tests/run.py::presented_fps` turns them into the `presented:` characterisation line every
  synthetic case prints. Codec-agnostic — it is the instrument `dualsequencer:6` was only for
  Dolby Vision. Type 46 is emitted only when non-zero, and a stream shown at 13 fps reported 0
  drops: the sink does not count a frame it never presented as dropped.
- **A constructed Starfish object is not dispatchable until synchronous `Load` returns.**
  `sf_ready()` still answers whether the object exists; `sf_ready_object()` also requires the
  C `LOAD_RETURNED` gate. Rust records the return on the exact native epoch before publishing
  the route result. The pump waits before even polling `sf_is_load_completed`, with separate
  20-second issued→return and return→loadCompleted budgets. Timeout has code `load_timeout`;
  firmware refusal remains `tv_pipeline`. Teardown after that timeout does NOT join a media
  thread still inside `sf_load` (that froze the SDL thread on BACK): the thread, payload and
  epoch are parked as `engine::AbandonedLoad` (Rust phase `Abandoned`), native starts are
  refused, and `reap_abandoned_load` runs the ordinary Unload → gate → retire → D1 release on the
  main thread once Load returns; a Load that never returns leaks its object. A refused fresh start
  sets `ps.native_start_refused` (`Option<StartRefusal>`), which `player::state` derives `Error` from
  because no Engine exists to publish it: cause `load_timeout` for a parked Load, `release_timeout`
  for an overdue Unload. Both offer Try Again, and the first start that is not refused clears it. A
  Load that has not timed out is still joined. The host concurrent tests model this boundary, not
  the firmware's native initialization. ACB dispatch is additionally constrained by stage/bind
  ordering; the C Starfish gate does not wrap ACB calls.
- **A claimed retranscode HOLDS presentation for its worker's flight (`claim_hold.rs`).** The PMS
  half of a track pick / enhancement toggle / quality change runs on a worker for 1-15 s and the
  landing reloads at the offset captured at claim time, so a stream left playing showed the
  flight's seconds twice. `pump` calls `claim_hold::engage` when the claim dispatches `Pending`
  (the viewer's own `player::pause`, then `player::state()` answers `Buffering` so the HUD's
  existing transport spinner draws) and `take` + `release` AFTER `run_claim_tail`, so an accepted
  claim's Play lands on the new stream and a rejected one's on the kept Engine. A viewer press
  (`player::lifecycle::set_transport_paused` -> `note_user_transport`) forgets the restore: the
  viewer's last transport press always stands. A stream already paused at claim time stays paused.
  While the hold's pause stands the viewer's transport reads see PLAYING (`player::lifecycle::viewer_paused`,
  `claim_hold::owns_pause`): the OK toggle means Pause and a seek's `resume_if_paused` leaves the
  hold alone (its seek is carried past the reload by `pump::commit_or_carry_seek`, including a
  seek pressed mid-flight when none was pending at claim time). An engine failure during the
  flight publishes `Failed` at once (`fail_current_engine`) and clears the hold, so the error
  read-out shows instead of a spinner. The encoder a claim replaces is NOT stopped by the worker:
  the pump retires it (`route::retire_superseded_encoder`) after the reload.
- **The k5lp/k3lp sandbox preflight refuses native playback when `/dev/rtkmem` is unreadable.**
  The device fact is cached at boot, while the refusal belongs to `PlaybackSession` and clears
  on exit. Explicit Repair confirmation spends `Player.repair` once for the whole app lifetime;
  `PlayerAdapter` owns the worker receipt. Success still requires a full app relaunch. See
  `docs/native-video-sandbox.md` for limits.
- **Starfish `Load` must be constructed with `uid = NULL`** (`SMP_ctor(slot->object, NULL)`), and in
  buffer-feed mode the app must **not** `LSRegister` its own `com.webos.media` client — either
  collides with the pipeline's uMS connection (CONN_FIND_ERR). See the comment in `load_thread`.
- **ACB bind order matters** (mirrors Kodi/ss4s): `setSinkType(MAIN)` → `setMediaId` →
  `setState(LOADED)` → *wait for decoded frames* → `setMediaVideoData(<sourceInfo envelope
  as captured; a PQ VP9 gets its HDR keys written in first, see below>)` → `setDisplayWindow` → `setState(PLAYING)`. The payload passed to
  `setMediaVideoData` is the **whole `sourceInfo` envelope** captured verbatim from the pipeline's
  callback (`sourceInfoRaw`), not a reconstructed one — with ONE exception: a VP9 stream whose container says PQ gets `hdrType` / `mediaVui` / `mediaSei` written in first (`player/vp9_hdr.rs`), because a VP9 bitstream cannot carry the transfer function, so the pipeline's own envelope says `"hdrType":"none"` and the panel stays SDR (device-measured on the dev set, whose ACB path is the only one that sends the envelope: the TV's `picture.dynamicRange` flips to HDR with the rewrite and stays SDR with `/tmp/nativejelly-novp9hdr`; webOS 5+ has no ACB, so nothing is sent there and its VP9 HDR behaviour is unknown). The envelope is otherwise untouched, and an envelope that already names an HDR type is never overridden. Audio is owned by the pipeline — **never feed
  ACB an audio SINK or elementary stream**. That half is real and unchanged. Its long-stated
  consequence is not: `SOUND_ERROR_019` is a literal that exists in **no library on this
  television** (swept across ~70 harvested libraries including 92 MB of Chromium), and the clause
  has carried no evidence since the initial commit. **`AcbAPI_setMediaAudioData` IS used**, for a
  two-key METADATA descriptor — `{"audio":{"immersive":"ATMOS"},"context":"<mediaId>"}`, fired
  right after `acb_bind`, which is exactly where LG's own client fires it. Device-measured
  2026-08-21: `rv=1`, audio untouched (1600 AUs, `reply=O`, no error), and the set's own
  "Dolby Vision / Dolby Atmos" read-out captured on screen. Details and addresses:
  `acb_send_atmos` in `src/starfish.c`. `AcbAPI_setMediaVideoData`/`setState`/
  `setDisplayWindow` take a `long *taskId` out-param as their last arg — the 3-arg ABI is required
  (2-arg calls corrupt memory / segfault).
- **The Load payload's codecs must come from the `/decision` OUTPUT, not the source file.** A
  transcode changes the codec/rate; building the Starfish Load config from the *source* metadata gives
  the decoder the wrong description → **silent audio / glitches**. Read the output codecs from the
  transcode decision. For ADTS/HE-AAC, use the **CORE** sample rate from the AudioSpecificConfig (SBR
  doubles it). See `[[audio-payload-codecs]]`.
- **Subtitles are client-rendered here — the TV's HW subtitle engine is URI-mode only** and
  unreachable in buffer-feed. Every text track uses the pinned libass worker: authored ASS/SSA
  retains headers, events and fonts; SRT/WebVTT and other plain-text codecs use `text_ass` and
  synthesized styles. `sub_charset` detects legacy encodings before conversion. The embedded
  `ass_source` window preserves raw packet identities across seeks/reopens; external `sidecar`
  retains the whole bounded script for backward seeks and Off/On. A burned server selection
  suppresses client rendering. Size/position changes replace text-origin headers; authored ASS
  keeps its layout. The HUD lifts bottom text regions conservatively; merged top/bottom regions
  stay still. See `docs/text-subtitles.md` and `docs/ass-subtitles.md`.
  Image subtitles (PGS/VobSub) retain their decoder and their own authoring canvas. `ff::sub_canvas`
  reads that canvas via `avcodec_parameters_from_context` (no guessed struct offset), and
  `appkit::player_hud::sub_screen_rect` scales it into the video rectangle.
- **A seek NEVER interrupts the demuxer.** The pump publishes the target in `seek_to_ns` and the
  demux thread — the only thread that touches the `AVFormatContext` — `av_seek_frame`s on it
  between two reads. Do not reintroduce an interrupt: the pump used to `shutdown(2)` the socket to
  break the read so the outer loop would reopen and seek, and it could not work, because our AVIO
  is **seekable** (`seek_cb` reopens with a byte `Range`), so libavformat treats the broken read as
  recoverable, calls `seek_cb`, gets a fresh connection at the same offset and reads on.
  `av_read_frame` never returns an error, so the reopen never happens and the seek never lands —
  it just runs out the stuck-watchdog on pre-seek packets and escalates to a full reload. This
  survived a long time because the test suite's cases inherited a server-side `viewOffset`, so the
  seek under test usually had nowhere to go (fixed 2026-07-28; see the
  `docs/agent-reference.md` testing note).
- **Seeks are in-place**, in one of three per-session modes (`player::SeekMode`, chosen by
  `engine::select_seek_mode` in `start_bufferfeed`, the `/tmp/nativejelly-seekmode` dev trigger
  `native|segment|reload` wins): the demuxer is `av_seek_frame`d to the target, then on the first
  post-seek keyframe `feed_stream` re-anchors the pipeline — no reload/decoder re-init.
  `Segment` (webOS 4.x) is Kodi-style: pump `flush()`, then `setTimeToDecode` + `sendSegmentEvent`
  through decompile-derived private offsets. `NativeSeek` (`VP_EXPORTED`, webOS 5+; issue #568) is
  LG's PUBLIC `StarfishMediaAPIs::Seek(millis)` (`sink().seek`, `sf_seek_ms`): it flushes itself, so
  the pump does NOT `flush()` and nothing may flush after it (a `flush()` is a seek to 0 and re-creates
  the stale-segment stall); it re-anchors the segment AT the millis passed, so `feed_stream` feeds
  content-time PTS (`pts_shift = 0`) and calls `Seek(max(kf_pts,0) / 1e6)` before feeding the landed
  keyframe — on the main thread, the only Feed caller, so it cannot race a Feed. A refused Seek
  (rv 0) drops the session to `Reload`, re-posts this seek for the pump's `reload_at` (the stuck
  watchdog goes straight to `reload_at` in `Reload` mode, and a flushed keyframe in a non-`Segment`
  mode anchors nothing: the private Segment offsets never run on webOS 5+), and latches
  `native_seek_refused` so the reload's fresh session stays `Reload` for the rest of THAT playback;
  a fresh play (`start_bufferfeed_tracked` with no live Engine) clears the latch. `Reload`
  (old-ABI 3.x, and the fallback of both) is a fresh `Load` per seek. Otherwise the mode is
  re-selected per session. The evidence is in `docs/webos5-port.md` ("The public `Seek`").
  A transcode seek instead
  restarts the encode at `&offset` with a full fresh `Load`. The rebase machinery
  (`pts_shift`/`rebase_pending` in `shared.rs`) keeps Starfish from ever seeing a PTS jump.
- **`frames` is SEEK-scoped, `seen_frame` is SESSION-scoped.** `pump` zeroes `SHARED.frames` as
  *part of applying* an in-place seek (it counts only post-seek frames, for the rebind + resume
  re-pause gate), so `frames == 0` does **not** mean "we have never shown a picture" — it is true
  for the whole of every seek. `SHARED.seen_frame` is the bit that answers that question: set beside
  `frames` in the presented callback, cleared **only** in `reset_session`. The HUD divides its two
  busy indicators on it (`appkit::player_hud::busy_surface`); anything else asking "has this session put
  a picture on the panel" wants `player::seen_frame()`, not `frames() > 0`.
- **App-switch lifecycle** (handled in `app/run.rs`, the frame loop; the machine and the
  transport-pause contract are `player/lifecycle.rs`, which `app::lifecycle` re-exports; details in
  the `docs/agent-reference.md` gotchas): OS
  background suspends the buffer-feed preserving the session. Foreground tracks one exact Load
  attempt at a time, follows reducer-approved superseding or rollback attempts, retries an exact
  failure without repeating route preparation, and applies the saved clock only after `Started`.
  Preserve the suspend/reload pairing if you touch playback.

## Verifying playback changes

Direct-play AVIO retries cut sized HTTP bodies at the next unread byte, with a shared
per-offset retry bound; curl body retries are limited to recoverable transfer errors.
Range answers for sized bodies must start at the requested byte. Unsized progressive
conversions keep their existing policy, and HLS acquisition retains its own deadlines.
`ff_body_heal_tests` covers those boundaries with loopback servers.

The demuxer can temporarily lift queue caps for tracks stored far apart: video headroom
lets it reach late-starting audio, and audio's soft cap can expand while video is starved.
Both have hard bounds, teardown/seek handling and real-demux fixtures in
`ff_late_audio_tests` (`make check-ffmpeg`; requires the ffmpeg and ffprobe CLIs).

A terminal Engine that has presented video releases its pipeline on the next pump frame.
The session retains its failure cause and retry position; a real stop consumes that error
once and sends Jellyfin the retained position. The deferred flag belongs to the Engine,
so a replacement Engine cannot inherit a previous one's pending release.

**Start with `make check`** — the host unit suite (`cargo test --lib`, ~28 s for 3,639 tests,
measured 2026-09-17; do not re-quote that number, `time` it) covers a real
slice of this pipeline's pure logic: `ff.rs`'s `nal_end` bounds guard and AVCC→Annex-B conversion,
the AVIO abort guards (a seek after teardown must not open a second connection — graded on an accept
count), `stream.rs`'s socket lifecycle, `route/plan.rs`'s direct-play-vs-transcode selection, and
`task.rs`'s `MainThread` token being genuinely `!Send`. Cheap enough that there is no reason to skip
it before a deploy.

But there is **no host *runtime*** — nothing above decodes a frame or touches Starfish/ACB (`ff.rs`
once gated `#[link]` directives out of `cfg(test)` to keep the pure logic host-testable; with
everything on `dynlib!` those are gone, so a test that actually calls FFmpeg now fails by taking
`dlopen`'s `None` branch on Darwin rather than by failing to link), and the host is Darwin while the TV is
Linux, which is why `tools/sockprobe.c` exists. So anything about *playback behaviour* is only
observable on device: deploy and read `/tmp/nativejelly-events.log` (feed stats, bind steps,
seek/rebase, `RECEIVE_GOOD_VIDEO`). The `tests/` harness drives real playback per case — see the root
`docs/agent-reference.md` testing section (run as GUEST by default; never run two harness jobs at once).
