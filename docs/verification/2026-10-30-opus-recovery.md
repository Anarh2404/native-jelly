# C3 refusal records, attempt snapshot and audio recovery — host and ARM

30 October 2026. Application source: `4e52880386b155fad547018f442bd54765ccbbd5` on
`feat/opus-direct-play`, based on main `e673debb`. The tree was committed and clean before and
after every gate below; this record is a documentation-only successor.

## Scope

| Commit | Change |
|---|---|
| `414c6e3b` | Build-scoped refusal layer over the immutable boot capabilities (`devcaps::Refusal`, `Caps::minus`, `effective_caps`) |
| `71fa2f6f` | Bounded install preference `native_audio_refused`; named rows apply at once, counted rows after two proofs |
| `f43a47db` | One capability snapshot per play attempt, shared by local gates and the Jellyfin DeviceProfile |
| `7655b93b` | First pre-picture `type=18` number latched; Opus header refusal published separately |
| `8c8310be` | `route::open_recovery`: one bounded audio-conversion rung per request, driven by the pump |
| `4e528803` | Regression pinning the failed open's intended position (resume or pending seek) |

Opus remains withheld from Auto/Force routing and Jellyfin profiles. No TV run, real-server
negotiation, merge, push, release, tag or version bump was performed for this checkpoint.

## Gates on the committed source

- Full `make check` (`umask 022`): passed in 650.10 seconds. Main application result lines:

```text
test result: ok. 5231 passed; 0 failed; 27 ignored; 0 measured; 0 filtered out; finished in 74.35s
test result: ok. 5327 passed; 0 failed; 27 ignored; 0 measured; 0 filtered out; finished in 77.95s
```

- Platform suite 238 passed in both configurations; the other workspace suites, dependency,
  module-layer, statics and Python/C gates passed.
- Shipping `--no-default-features` check passed.
- Debug ARM build (`make`): passed in 106.10 seconds.

Under this host's default `umask 0002`,
`catalog::session::migration_tests::recorded_identity_refusal_and_unavailability_preserve_secure_input_without_retry`
fails: its fixture file is group-writable and storage correctly refuses it as an unsafe mode
before the opener runs. It fails identically at `a963da40`, is unrelated to C3, and passes under
`umask 022`.

## New coverage

- `route::open_recovery::tests` (9): named vs ambiguous evidence, proof only from a clean
  audio-only landing with exactly its own Load, one conversion per request, no learning from
  timeouts, failed conversions or headers, Force and pending-rollback behaviour, baseline codecs.
- `route::decision::open_recovery_tests` (4, Jellyfin loopback): carried track, start position
  and copyable video in the PlaybackInfo ask; attempt narrowed while boot caps stay unchanged;
  Dolby Vision no-copy contract preserved and unproven; refused conversion spends the rung and a
  new plan starts fresh; a forced route never asks the server.
- `route::decision::attempt_caps_tests` (2), `player` refusal-latch and intended-position tests.

## Television checks — 30 October 2026

Set: webOS 6.5.3 build 6.5.3-47 (`kisscurl-koli`); synthetic 72 s H.264 720p30 + 48 kHz
stereo Opus served by a local mock Jellyfin through a reverse tunnel; one TV lease per case,
released after each; app closed, triggers cleared, panel restored. Sound output, volume and
mute were not touched (current output only; speakers/soundbar not separated).

1. **Pushed `main` (`0bb0bcda`, Opus withheld), H.264/AC-3 direct play:** profile
   `audio=aac,ac3,eac3,dts` with no Opus; one Load; native picture; position to 51 s; pause and
   resume; native seeks to 45 s and 5 s; no reopen, watchdog reload, recovery, pipeline error
   or crash.
2. **Local enable build** (an uncommitted `devcaps` patch admitting the measured stereo Opus
   candidate; reverted afterwards) — valid stereo Opus (pre-skip 312, gain 0): profile
   `audio=…,opus` with `opus:2`; one native `H264`/`OPUS` Load; position to 32 s; pause and
   resume; two native seeks; recovery not taken.
3. **Same build — the same file with a +1 dB OpusHead output gain** (legal, outside the fed
   subset): the demuxer refused the header; `audio recovery: HeaderUnsupported`; the second
   PlaybackInfo withheld direct play and kept `AudioStreamIndex` 1, `AllowVideoStreamCopy`
   true and `StartTimeTicks` 0; landing `video copied, audio converted`; reload as
   `H264`/`AAC`; position to 46 s with the converted picture on the video plane; pause and
   resume; no refusal remembered; no pipeline error or crash.

The conversion was a pre-made AAC file standing in for the server; real Jellyfin
negotiation, a pipeline refusal (`num=200`/`601`) on the set, listening and output
separation remain unverified.

## Real Jellyfin 12.0 checks and the `DirectPlayError` fix — 30 October 2026

Server: Jellyfin 12.0.0 on the host, reached through a reverse tunnel; the authorized test
account only (its watch state is the only server write). Same set and handback rules as above;
sound output, volume and mute untouched.

1. **4K HEVC SDR episode, E-AC-3 5.1, on `main`:** `method=DirectPlay`; one `H265`/`AC3 PLUS`
   Load; position to 124 s; pause and resume; native seeks to 120 s and 60 s; no error.
2. **4K HEVC HDR10 film, switch to its TrueHD 7.1 track at ~4 s, on `main`: failed.** The
   server answered `TranscodeReasons=AudioCodecNotSupported,DirectPlayError` and copied the
   HEVC. The client counted `DirectPlayError` as a video reason, read the lane as an encode,
   declared `H264` and the demuxer refused the copied HEVC (`the Load declared H264 but the file
   is hevc`); playback stopped at 3 s. Present since `41fbe62c` (2026-10-04), not introduced by
   C3, but every C3 recovery conversion withholds direct play and would hit it.
3. **Same film with `b24ab6fe`:** `delivery=audio-transcode video=hevc->hevc
   audio=#1:truehd->ac3`, external subtitle kept; `H265`/`AC3` Load; a seek to 300 s restarted
   the real conversion at 300 s; position to 351 s; capture shows the picture at 5:52 with
   TrueHD selected and subtitles drawn; no pipeline error or crash.

Jellyfin source (`StreamBuilder.GetVideoDirectPlayProfile`) sets `DirectPlayError` only as a
fallback when no direct profile matched without a compatibility reason; it is in none of the
server's video reason groups, and `GetVideoTranscodeProfile`'s copy decision does not read the
reasons. `b24ab6fe` adds a classifier regression and a recovery-level loopback regression with
the measured answer; both failed before the fix.

Gates on `b24ab6fe` (`umask 022`): shipping check and debug ARM build passed. The first full
`make check` failed one unrelated test,
`person::tests::a_roles_landing_captions_by_key_and_a_media_landing_resets_it`
("a caption survived the list it was addressed to"), which passed 5/5 isolated and in its
module; the second full run on the same commit passed in 387.27 s:

```text
test result: ok. 5233 passed; 0 failed; 27 ignored; 0 measured; 0 filtered out; finished in 85.04s
test result: ok. 5329 passed; 0 failed; 27 ignored; 0 measured; 0 filtered out; finished in 81.15s
```

The person test is an order-dependent flake to fix separately. The account's library holds no
Opus item, so the Opus recovery itself was not run against the real server.

## Limits

- Host tests do not drive the pump's failure-branch reload; on the TV only the
  unsupported-header trigger was exercised.
- No stall detector: surround stalls (debug fixtures only) are neither recovered nor learned.
- Remaining: test-account negotiation/recovery, restart persistence, track changes, then
  enabling stereo Opus together in routing and profiles and TV acceptance on both outputs.
