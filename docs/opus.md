# Opus playback — staged C3 port

Status on 30 October 2026: the native feed foundation, firmware restrictions, exact build
identity, build-scoped refusal records, the per-attempt capability snapshot and the bounded
audio-conversion recovery are implemented and host/ARM verified
([recovery evidence](verification/2026-10-30-opus-recovery.md)). Separate TV-speaker and HDMI ARC
experiments are recorded in [the TV evidence](verification/2026-10-30-opus-tv.md). **Opus direct play is not enabled in Auto or Force mode.** The
shipping codec set, local eligibility gate and Jellyfin profiles continue to send
Opus through the existing server-conversion path until real-server and TV acceptance of the
recovery. C3 is not complete.

## Implemented foundation

- `OPUS` Load declaration with `contents.opusInfo`, a 48 kHz decoded clock and a
  padded-base64 family-0 `OpusHead` for the exact carried mono/stereo track.
- Unknown track widths refuse the Load. Surround needs an explicit typed debug fixture
  override tied to its URL, codec and width; normal capability limits stay unchanged. A reload reads the
  current carried track again rather than retaining the previous track's width.
- Before either lane receives an AU, the demuxer checks the selected source codec,
  channel count and header against the Load declaration. The supported subset uses
  version 1, pre-skip 312 and zero gain; family 0 for mono/stereo and explicit
  debug-only family 1 Vorbis mappings for 3–8 channels. Other header timing/gain or mapping
  needs conversion; it is not silently replaced by the synthesized header.
- Valid Opus packets use the existing raw audio lane, without ADTS framing, header
  insertion, a software decoder or a new native dependency.
- The diagnostics audio label recognizes an actual OPUS declaration.

Header format: [RFC 7845 §5.1](https://www.rfc-editor.org/rfc/rfc7845.html#section-5.1).
The payload vocabulary and common libopus header follow upstream
[#992](https://github.com/GLinnik21/plx-native/commit/f70d42be91ecd822b1d16aa8b4c139bec583b49c),
adapted to this checkout's route ownership. Ordinary staged policy remains mono/stereo;
wider headers are available only to explicitly authorized debug fixtures. This does
not claim support for every valid RFC 7845 stream.

## Refusal records, attempt snapshot and recovery

- **Records** (`catalog::session::native_audio_refused`): an install-wide public preference of
  at most 16 rows keyed by exact firmware build, codec and channel count. A write on a new build
  drops other builds' rows; an old preference file round-trips unchanged while the list is empty.
- **Snapshot** (`route::attempt_caps`): each play start derives the boot capabilities less the
  rows in force on this build, once. Local gates, track switches, rebuilds, the ABR worker, the
  Load guards and the Jellyfin DeviceProfile of that attempt all read this one value; the boot
  snapshot never changes.
- **Recovery** (`route::audio_recovery`, `pump::recover_refused_audio`): a pre-picture failure
  of natively fed audio outside AAC/AC-3/E-AC-3 converts that audio once per request. It runs
  after a pending HLS→Original rollback and before the Auto HLS contingency. It keeps the carried
  track, subtitle, ceiling and the position the viewer intends (resume or pending seek); the
  viewer's pause survives the reload. Video stays copied unless the route's Dolby Vision
  no-copy rule applies. Never under Force, never after a picture; a refused conversion falls
  through to the ladder.
- **Evidence**: `type=18 num=200` is recorded at once. Any other number (or `sf_load == 0`)
  counts one proof only when the server stated audio converted and video copied and that
  landing's own Load, with no other between, showed the first picture; two proofs apply. A
  timeout, a source/HTTP failure, a failed conversion and an Opus header the demuxer refused
  teach nothing (the header case is still converted).

## Remaining before enabling direct play

1. Verify negotiation and recovery against the test Jellyfin account, including restart
   persistence and track changes.
2. Enable stereo Opus in Auto/Force and the profiles together, then verify actual sound, seeks
   and track changes on the TV through the TV-lock workflow, on both outputs.
3. `player::native_stall` watches a natively fed Opus track of more than two channels: both AU
   queues holding data, nothing accepted for 4 s and no picture is `OpenFailure::Stalled`, named
   evidence recorded and converted once like `num=200`. The measured stalls are reachable only
   through debug fixtures (ordinary policy is stereo), and the watchdog is host-tested only.

See [the foundation evidence](verification/2026-10-30-opus-feed-foundation.md).
See [separate-output TV results](verification/2026-10-30-opus-tv.md) for current scope.
Small-packet 5.1 played, but large-packet 5.1 and both 7.1 fixtures stalled on both
outputs. Listening and actual multichannel acoustic output remain unverified.
