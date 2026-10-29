//! Playback against Jellyfin: the `POST /Items/{id}/PlaybackInfo` negotiation, the URL it yields,
//! and the session reports that follow.
//!
//! **The protocol this implements is Jellyfin's own.** A client states what it can decode as a
//! `DeviceProfile`; the server answers per media source with `SupportsDirectPlay`,
//! `SupportsDirectStream` and `SupportsTranscoding`, plus a `TranscodingUrl` when it would convert
//! and `TranscodeReasons` saying why. The reported `PlayMethod` follows the server and the official
//! web client (`playbackmanager.js` `createStreamInfo`):
//!
//! * `DirectPlay` — `SupportsDirectPlay`: the file's own bytes,
//!   `GET /Videos/{id}/stream.{ext}?static=true`.
//! * `Transcode` — anything played from the `TranscodingUrl`, which the server itself marks
//!   `Transcode` (`MediaInfoHelper`). That includes a remux (both streams copied into a new
//!   container) and an audio-only conversion: the dashboard derives "Remux" / "Direct Stream" from
//!   `TranscodingInfo`, which the server clears on every report whose method is not `Transcode`.
//!   Which lanes are copied is carried separately, on [`Lane::copied`].
//!
//! [`Jf::negotiate`] asks once and answers with a typed [`Negotiation`]: the method the server
//! agreed to, the URL to open, and per lane the SOURCE codec beside the codec that will actually
//! arrive (the Load payload has to describe the latter).
//!
//! `PlaySessionId` is the server's handle for the negotiated playback. Everything after the
//! negotiation is keyed by it: `/Sessions/Playing{,/Progress,/Stopped}` and
//! `DELETE /Videos/ActiveEncodings`, which ends one encoder. The negotiation stores that id (and
//! the source id and track indexes it resolved) in a table keyed by the app's own session string,
//! so the later calls report the same playback the server registered.
//!
//! **One playback, many encoders.** A seek, a track switch, a quality change and every resumed
//! conversion re-negotiate under a new `PlaySessionId`. As in the web client's `changeStream`, the
//! replacement names the playing media source, inherits the playback's start (so it reports
//! `/Progress`, not a second `/Sessions/Playing` the server would count as another play), and the
//! encoder it replaced is ended with `DELETE /Videos/ActiveEncodings` — never with a `Stopped`
//! report, whose position the server writes into the user's resume point.
use super::models::*;
use super::{api::Jf, convert, ids, ticks};
use crate::catalog::{
    Ceiling, MediaContainer, StreamUrl, TimelineReport, TimelineState, TranscodeDelivery, TranscodeOffset, TranscodeSpec,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// The bitrate bound to advertise when the user has asked for no ceiling.
///
/// `DeviceProfile.MaxStaticBitrate` defaults to 8 Mbit/s on the server, which would refuse direct
/// play of any remux — so an unconstrained ask has to state its own, and on the LAN this app is
/// built for there is no honest bound to state. A ceiling the user DID pick is applied to both
/// rates (see [`device_profile`]), which is what makes a low rung mean something for the original
/// file as well as for the re-encode.
const UNBOUNDED_BPS: i64 = 200_000_000;

/// `PlayMethod`, the three values the API defines.
///
/// This client produces `DirectPlay` and `Transcode` only. `DirectStream` is the static stream of a
/// source the server marks `SupportsDirectStream` but not `SupportsDirectPlay`, and 10.10 through
/// 12 never answer that way: `MediaInfoHelper` forces `EnableDirectStream` off and then sets
/// `SupportsDirectStream` equal to `SupportsDirectPlay`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlayMethod {
    #[default]
    DirectPlay,
    DirectStream,
    Transcode,
}

impl PlayMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectPlay => "DirectPlay",
            Self::DirectStream => "DirectStream",
            Self::Transcode => "Transcode",
        }
    }
}

/// One playback request, in the terms PlaybackInfo takes.
#[derive(Clone, Copy, Debug)]
pub struct Ask<'a> {
    pub rk: &'a str,
    /// The app's own session string the negotiated `PlaySessionId` is filed under.
    pub session: &'a str,
    /// `MediaSourceId` — the version to play. The server applies `AudioStreamIndex` and
    /// `SubtitleStreamIndex` only to the source with this id. `None` names the item's own source
    /// (its id is the item's), the one the server lists first.
    pub media_source_id: Option<&'a str>,
    /// `AudioStreamIndex` (Jellyfin's 0-based index); `None` lets the server apply the user's
    /// audio preferences.
    pub audio_index: Option<i64>,
    /// `SubtitleStreamIndex`; `None` sends `-1` (off).
    pub subtitle_index: Option<i64>,
    /// Where playback begins, so a conversion is primed at the resume point rather than at zero.
    pub start_ticks: i64,
    /// The bound this playback may not exceed; `None` advertises no bound (see [`UNBOUNDED_BPS`]).
    pub ceiling: Option<Ceiling>,
    /// `EnableDirectPlay` — offer the file's own bytes.
    pub direct_play: bool,
    /// `EnableDirectStream` + `AllowVideoStreamCopy` — let the server copy the video into a new
    /// container. Withdrawn for a source whose bitstream must not reach the panel unconverted.
    pub video_copy: bool,
    /// Strict Original: the software feed's formats, no device limits, no transcode target.
    pub forced: bool,
    /// Burn the selected subtitle into the video (`AlwaysBurnInSubtitleWhenTranscoding`).
    pub burn: bool,
    /// A conversion delivered as HLS with segments of this many seconds; `None` asks for the
    /// progressive Matroska stream.
    pub hls_segment_secs: Option<u8>,
    /// The attempt's own capability snapshot (`route::ResolveEnv::caps`), the same value the
    /// local route gates read. The DeviceProfile is built from this, never from the boot answer.
    pub caps: &'a nj_platform::devcaps::Caps,
}

/// One elementary stream of a negotiated playback, in the app's codec spelling.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lane {
    /// The file's own codec.
    pub source: String,
    /// What the server will send: the source codec when copied, else the encode target.
    pub output: String,
    pub copied: bool,
}

/// How a conversion delivers its selected subtitle, as the answer's `MediaStream.DeliveryMethod`
/// states it (`MediaInfoHelper.SetDeviceSpecificSubtitleInfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubtitleDelivery {
    /// Muxed into the converted container. It is the only subtitle stream there
    /// (`EncodingHelper.GetMapArgs` maps the selected one alone), so the renderer's ordinal 0.
    Embedded,
    /// A separate file on the server, fetched under the request header: its path with no
    /// credential, and the format it is served in.
    External { path: String, codec: String },
    /// Burned into the picture by the server.
    Burned,
}

/// What the server agreed to.
#[derive(Clone, Debug, Default)]
pub struct Negotiated {
    pub method: PlayMethod,
    /// The full URL to open, its `ApiKey` included.
    pub url: String,
    pub video: Lane,
    pub audio: Lane,
    /// The audio stream this playback carries (requested, else the source's default).
    pub audio_index: Option<i64>,
    pub subtitle_index: Option<i64>,
    /// How a conversion delivers `subtitle_index`; `None` on direct play (the client draws the
    /// file's own track) and when no subtitle is selected or delivered.
    pub subtitle: Option<SubtitleDelivery>,
    pub media_source_id: String,
    pub play_session_id: String,
    /// The played media source's whole length (`MediaSourceInfo.RunTimeTicks`), nanoseconds; `0`
    /// when the server did not say. A live progressive conversion is Matroska written to a pipe,
    /// which cannot carry its own Segment Duration, so this is the only total that playback has.
    pub runtime_ns: i64,
}

/// Why the server answered and still offered nothing to play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `PlaybackErrorCode.NotAllowed` — the user's policy forbids this playback.
    NotAllowed,
    /// `PlaybackErrorCode.NoCompatibleStream` — no delivery fits this device's profile.
    NoCompatibleStream,
    /// `PlaybackErrorCode.RateLimitExceeded` — the server is limiting streams.
    RateLimitExceeded,
    /// An `ErrorCode` this client has no meaning for, kept for the event log.
    Unrecognized(String),
    /// The answer carried no media source.
    NoMediaSource,
    /// The source offers neither direct play nor a `TranscodingUrl`.
    NoDeliveryMethod,
}

impl Refusal {
    fn from_error_code(code: &str) -> Self {
        match code {
            "NotAllowed" => Self::NotAllowed,
            "NoCompatibleStream" => Self::NoCompatibleStream,
            "RateLimitExceeded" => Self::RateLimitExceeded,
            other => Self::Unrecognized(other.to_string()),
        }
    }

    /// The technical name, for the event log — never for the viewer.
    pub fn code(&self) -> &str {
        match self {
            Self::NotAllowed => "NotAllowed",
            Self::NoCompatibleStream => "NoCompatibleStream",
            Self::RateLimitExceeded => "RateLimitExceeded",
            Self::Unrecognized(code) => code,
            Self::NoMediaSource => "NoMediaSource",
            Self::NoDeliveryMethod => "NoDeliveryMethod",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Negotiation {
    Playable(Negotiated),
    /// The server answered and can serve neither direct play nor a conversion.
    Refused(Refusal),
    /// No usable answer: transport failure, an unknown item, or a malformed body.
    Unreachable,
}

impl Negotiation {
    pub fn playable(self) -> Option<Negotiated> {
        match self {
            Self::Playable(n) => Some(n),
            Self::Refused(_) | Self::Unreachable => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Session {
    item_guid: String,
    media_source_id: String,
    play_session_id: String,
    play_method: PlayMethod,
    audio_index: Option<i64>,
    subtitle_index: Option<i64>,
    /// `/Sessions/Playing` has been sent; later reports are `/Progress`.
    started: bool,
    /// `PlaybackStartTimeTicks` — UTC of the first report for this playback, held so every later
    /// report states the same start rather than re-deriving a drifting one.
    start_time_ticks: i64,
    /// When this entry was last written, in table order: the eviction key.
    seq: u64,
}

const SESSION_CAP: usize = 64;

fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    static T: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

fn session(key: &str) -> Option<Session> {
    sessions().lock().ok()?.get(key).cloned()
}

/// A preview belongs to the actual negotiated source, including alternate versions.
/// Match the item as well: a stale encoder receipt cannot supply another episode's images.
pub(crate) fn preview_owner(key: &str, rk: &str, generation: &str) -> Option<super::trickplay::Owner> {
    let s = session(key)?;
    let item = ids::guid_of_key(rk)?;
    if ids::normalize(&s.item_guid) != item { return None; }
    super::trickplay::Owner::new(&item, &s.media_source_id, generation)
}

fn put_session(key: &str, mut s: Session) {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    s.seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut t) = sessions().lock() {
        // Bounded by evicting the least recently written entry. The playing session is rewritten
        // by every report, so a long run of seeks retires the encoders it replaced, never it.
        if t.len() >= SESSION_CAP && !t.contains_key(key) {
            if let Some(oldest) = t.iter().min_by_key(|(_, s)| s.seq).map(|(k, _)| k.clone()) {
                t.remove(&oldest);
            }
        }
        t.insert(key.to_string(), s);
    }
}

fn update_session(key: &str, f: impl FnOnce(&mut Session)) {
    if let Ok(mut t) = sessions().lock() {
        if let Some(s) = t.get_mut(key) {
            f(s);
        }
    }
}

fn drop_session(key: &str) -> Option<Session> {
    sessions().lock().ok()?.remove(key)
}

pub(crate) struct ProfileAsk {
    /// Offer direct play (and the Embed/External subtitle methods that go with it).
    pub direct: bool,
    /// Strict Original: the software feed's formats, no device limits, no transcode target.
    pub forced: bool,
    pub ceiling: Option<Ceiling>,
    /// Burn the selected subtitle into the video. The caller asks for this deliberately — a burn
    /// is the server's most expensive option and the one the subtitle profiles below exist to
    /// avoid, so it is never inferred from "a subtitle is selected".
    pub burn: bool,
    /// Ask for an HLS conversion with segments of this many seconds instead of progressive mkv.
    pub hls: Option<u8>,
    /// Offer a conversion the bitmap formats as `Embed`. Withdrawn only to ask again for a text
    /// track the server answered as an embedded bitmap ([`Reask::Sidecar`]).
    pub embed_bitmap: bool,
}

/// The DeviceProfile for this client: [`Capabilities`] in the server's vocabulary, shaped by what
/// this one playback may ask for. It reads nothing but its two arguments.
///
/// [`Capabilities`]: crate::catalog::capabilities::Capabilities
pub(crate) fn device_profile(caps: &crate::catalog::capabilities::Capabilities, ask: &ProfileAsk) -> Value {
    use crate::catalog::capabilities::{BITMAP_SUBTITLES, CONTAINERS, EMBEDDED_SUBTITLES, SIDECAR_SUBTITLES};
    let dp_video = if ask.forced { caps.pipeline_video_codecs() } else { caps.video_codecs() };
    let dp_audio = if ask.forced { caps.pipeline_audio_codecs() } else { caps.audio_codecs() };
    let target_codecs = caps.transcode_video_codecs();
    let target_video = target_codecs.join(",");
    // HLS segments are MPEG-TS, which the player demuxes only as H.264 (`ff::hls_demux_segment`)
    // and the ask re-encodes anyway: the VP9/AV1 copy lanes have no place there.
    let hls_target_video =
        target_codecs.iter().copied().filter(|c| !matches!(*c, "vp9" | "av1")).collect::<Vec<_>>().join(",");
    let target_audio = caps.transcode_audio_codecs();
    let (mut w, mut h) = caps.max_resolution();
    let device_bps = caps.max_bitrate_bps().unwrap_or(UNBOUNDED_BPS);
    // Both rates, not just the streaming one. `MaxStreamingBitrate` bounds a re-encode;
    // `MaxStaticBitrate` is what the server weighs direct play against. Leaving the latter
    // unbounded under an explicit ceiling let a 40 Mbit/s remux direct-play while the user had
    // asked for "720p · 3 Mbps" — the ask was honoured for the encode branch and silently ignored
    // for the branch that actually ships the most bytes.
    let mut max_streaming_bps = device_bps;
    let mut max_static_bps = device_bps;
    if let Some(c) = ask.ceiling {
        w = w.min(c.max_w as u32);
        h = h.min(c.max_h as u32);
        max_streaming_bps = c.max_kbps.saturating_mul(1000).min(device_bps);
        max_static_bps = max_streaming_bps;
    }
    let direct_play = if ask.direct {
        vec![json!({
            "Container": CONTAINERS.join(","),
            "Type": "Video",
            "VideoCodec": dp_video.join(","),
            "AudioCodec": dp_audio.join(","),
        })]
    } else {
        Vec::new()
    };
    let target_audio = if target_audio.is_empty() { "aac".to_string() } else { target_audio.join(",") };
    let transcoding = match (ask.forced, ask.hls) {
        (true, _) => Vec::new(),
        // MPEG-TS segments: the player's HLS cursor demuxes nothing else.
        (false, Some(secs)) => vec![json!({
            "Container": "ts",
            "Type": "Video",
            "VideoCodec": hls_target_video,
            "AudioCodec": target_audio,
            "Protocol": "hls",
            "Context": "Streaming",
            "SegmentLength": secs,
            "MinSegments": 1,
            "BreakOnNonKeyFrames": false,
        })],
        (false, None) => vec![json!({
            "Container": "mkv",
            "Type": "Video",
            "VideoCodec": target_video,
            "AudioCodec": target_audio,
            "Protocol": "http",
            "Context": "Streaming",
            "CopyTimestamps": true,
            "BreakOnNonKeyFrames": false,
        })],
    };
    let mut codec = Vec::new();
    if ask.forced {
        for (c, ch) in caps.forced_audio_channel_limits() {
            codec.push(json!({
                "Type": "VideoAudio", "Codec": c,
                "Conditions": [cond("LessThanEqual", "AudioChannels", &ch.to_string())],
            }));
        }
    }
    if !ask.forced {
        codec.push(json!({
            "Type": "Video",
            "Conditions": [
                cond("LessThanEqual", "Width", &w.to_string()),
                cond("LessThanEqual", "Height", &h.to_string()),
                cond("LessThanEqual", "VideoBitDepth", "10"),
            ],
        }));
        // The panel's dynamic ranges, which also decide whether the server may COPY the video:
        // an HDR stream copied to an SDR panel is shown washed out, so it is tone-mapped instead.
        // H.264 is offered as SDR only, as the web client does.
        if let Some(ranges) = caps.hevc_video_ranges() {
            codec.push(json!({
                "Type": "Video",
                "Codec": "hevc",
                "Conditions": [cond("EqualsAny", "VideoRangeType", &ranges.join("|"))],
            }));
            codec.push(json!({
                "Type": "Video",
                "Codec": "h264",
                "Conditions": [cond("EqualsAny", "VideoRangeType", "SDR")],
            }));
        }
        // VP9 and AV1, offered only where the table lists their decoders: each one's own row bound,
        // VP9's measured bit depth, and the ranges that can reach the panel for each.
        for (name, rw, rh, depth) in caps.codec_row_bounds() {
            let mut conditions = Vec::new();
            if rw > 0 {
                conditions.push(cond("LessThanEqual", "Width", &rw.min(w).to_string()));
            }
            if rh > 0 {
                conditions.push(cond("LessThanEqual", "Height", &rh.min(h).to_string()));
            }
            if let Some(bits) = depth {
                conditions.push(cond("LessThanEqual", "VideoBitDepth", &bits.to_string()));
            }
            let ranges = match name {
                "vp9" => caps.vp9_video_ranges(),
                _ => caps.av1_video_ranges(),
            };
            if let Some(ranges) = ranges {
                conditions.push(cond("EqualsAny", "VideoRangeType", &ranges.join("|")));
            }
            if !conditions.is_empty() {
                codec.push(json!({ "Type": "Video", "Codec": name, "Conditions": conditions }));
            }
        }
        for (c, ch) in caps.audio_channel_limits() {
            codec.push(json!({
                "Type": "VideoAudio",
                "Codec": c,
                "Conditions": [cond("LessThanEqual", "AudioChannels", &ch.to_string())],
            }));
        }
    }
    // `SubtitleProfiles` is how the server is told which deliveries are available, and it picks
    // `Encode` (a burn, re-encoding video nobody asked to re-encode) only when nothing else fits.
    // So an EMPTY list is not "no subtitles" — it is a request to burn whichever track the item
    // defaults to. Offer the soft methods on every ask that is not an explicit burn, including the
    // transcode ask: a text track delivered as a sidecar keeps the video lane copyable.
    let mut subs = Vec::new();
    if !ask.burn {
        if ask.direct {
            // Direct play feeds the container's own bytes, so an embedded track rides along and
            // the client's renderer draws it.
            subs.extend(EMBEDDED_SUBTITLES.iter().map(|f| json!({ "Format": f, "Method": "Embed" })));
        } else if ask.embed_bitmap {
            // On a converted stream only the bitmap formats need muxing in — they have no text
            // form to fetch, and `Embed` is the only delivery that is not a burn.
            subs.extend(BITMAP_SUBTITLES.iter().map(|f| json!({ "Format": f, "Method": "Embed" })));
        }
        subs.extend(SIDECAR_SUBTITLES.iter().map(|f| json!({ "Format": f, "Method": "External" })));
    }
    json!({
        "Name": crate::catalog::identity::PRODUCT,
        "MaxStreamingBitrate": max_streaming_bps,
        "MaxStaticBitrate": max_static_bps,
        "DirectPlayProfiles": direct_play,
        "TranscodingProfiles": transcoding,
        "ContainerProfiles": [],
        "CodecProfiles": codec,
        "SubtitleProfiles": subs,
    })
}

fn cond(condition: &str, property: &str, value: &str) -> Value {
    json!({ "Condition": condition, "Property": property, "Value": value, "IsRequired": false })
}

/// One query value of a URL, undecoded.
fn query_param<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        k.eq_ignore_ascii_case(key).then_some(v)
    })
}

/// `TranscodeReasons` of a TranscodingUrl, split (`%2C`-joined on the wire).
fn transcode_reasons(url: &str) -> Vec<String> {
    query_param(url, "TranscodeReasons")
        .map(|v| v.replace("%2C", ",").replace("%2c", ","))
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// A reason that means the server re-encodes the video. `DirectPlayError` is not one: Jellyfin
/// sets it as a fallback when no direct profile matched (`StreamBuilder.GetVideoDirectPlayProfile`)
/// and its video copy decision never reads the reasons.
fn video_reason(r: &str) -> bool {
    r.starts_with("Video")
        || matches!(r, "RefFramesNotSupported" | "AnamorphicVideoNotSupported" | "InterlacedVideoNotSupported")
}

fn audio_reason(r: &str) -> bool {
    r.starts_with("Audio") || r == "SecondaryAudioNotSupported"
}

/// Whether the server may still COPY the video under the URL's `VideoBitrate`. No reason says so:
/// a source over the ceiling is answered `ContainerBitrateExceedsLimit` alone, and the stream
/// endpoint then refuses the copy in `EncodingHelper.CanStreamCopyVideo` (12.4: "Video bitrate must
/// fall within requested value" — a stream whose bitrate is unknown or above the request is
/// re-encoded). Missing that sent a 4K HEVC remux over an Auto ceiling to a pipeline loaded for
/// HEVC while the server encoded H.264 (live 12.4, 2026-10-29).
fn video_bitrate_admits_copy(url: &str, v: Option<&MediaStream>) -> bool {
    match query_param(url, "VideoBitrate").and_then(|b| b.parse::<i64>().ok()) {
        None => true,
        Some(limit) => v.and_then(|s| s.bit_rate).is_some_and(|rate| rate <= limit),
    }
}

/// The selected source stream of `kind` (`Video`/`Audio`): the explicit index, else the
/// source's default, else the first of that type.
fn pick<'a>(src: &'a MediaSourceInfo, kind: &str, index: Option<i64>) -> Option<&'a MediaStream> {
    let of_kind = || src.media_streams.iter().filter(move |s| s.kind == kind);
    let default_index = match kind {
        "Audio" => src.default_audio_stream_index,
        _ => None,
    };
    index
        .or(default_index)
        .and_then(|i| of_kind().find(|s| s.index == i))
        .or_else(|| of_kind().find(|s| s.is_default))
        .or_else(|| of_kind().next())
}

fn source_codec(s: Option<&MediaStream>) -> String {
    s.and_then(|s| s.codec.as_deref()).map(convert::codec).unwrap_or_default()
}

/// Output codec of one lane, in the app's spelling: the source's own when the lane is copied, else
/// the codec the server encodes to from the target list the URL names.
fn lane_codec(url: &str, list_key: &str, src: Option<&MediaStream>, copied: bool) -> String {
    let source = src.and_then(|s| s.codec.clone()).unwrap_or_default().to_ascii_lowercase();
    let list = query_param(url, list_key).unwrap_or("").replace("%2C", ",");
    let in_list = list.split(',').any(|c| c.eq_ignore_ascii_case(&source));
    let codec = if copied && (in_list || list.is_empty() || list.eq_ignore_ascii_case("copy")) && !source.is_empty() {
        source
    } else {
        let targets: Vec<&str> = list.split(',').filter(|c| !c.is_empty()).collect();
        encoded_head(&targets, list_key, src).unwrap_or(&source).to_ascii_lowercase()
    };
    convert::codec(&codec)
}

/// The target the server encodes to. Before taking the head it moves to the end the audio codecs
/// it avoids for the source (`EncodingHelper.ShiftAudioCodecsIfNeeded`): DTS and TrueHD for six
/// channels or more (an unknown count is taken as six), AC-3 and E-AC-3 below that — unless every
/// listed codec is one of them. The video list leads with H.264, which it never moves.
fn encoded_head<'a>(targets: &[&'a str], list_key: &str, src: Option<&MediaStream>) -> Option<&'a str> {
    if list_key == "AudioCodec" {
        let avoided: &[&str] = if src.and_then(|s| s.channels).unwrap_or(6) >= 6 { &["dts", "truehd"] } else { &["ac3", "eac3"] };
        let moved = |c: &&str| avoided.iter().any(|a| a.eq_ignore_ascii_case(c));
        if let Some(head) = targets.iter().find(|c| !moved(c)) {
            return Some(head);
        }
    }
    targets.first().copied()
}

/// The static-stream path of `src`: its own bytes, no conversion.
/// Always the server's stream, never the source's own `Path`: a remote source's
/// `RequiredHttpHeaders` are the server's to send, and the transports here put no headers of their
/// own on a media URL.
fn direct_path(guid: &str, src: &MediaSourceInfo) -> String {
    convert::part_key(guid, src)
}

/// The names in `src`'s `RequiredHttpHeaders`.
fn required_header_names(src: &MediaSourceInfo) -> Vec<String> {
    src.required_http_headers
        .as_ref()
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// The required headers the server's static stream of a remote Http source would not send: it
/// forwards only `User-Agent` (`GetStaticRemoteStreamResult`), while the ffmpeg behind a
/// `TranscodingUrl` is also given `Referer`.
fn headers_the_static_proxy_drops(src: &MediaSourceInfo) -> Vec<String> {
    if !src.protocol.eq_ignore_ascii_case("Http") {
        return Vec::new();
    }
    required_header_names(src).into_iter().filter(|n| !n.eq_ignore_ascii_case("User-Agent")).collect()
}

/// `path` tagged with the playback it belongs to, so the server attributes the stream to it.
fn with_play_session(path: &str, play_session_id: &str) -> String {
    if play_session_id.is_empty() || query_param(path, "PlaySessionId").is_some() {
        return path.to_string();
    }
    let sep = if path.contains('?') { '&' } else { '?' };
    format!("{path}{sep}PlaySessionId={}", crate::catalog::urlenc_str(play_session_id))
}

/// Why a conversion's subtitle answer is asked again, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reask {
    /// As a burn: a delivery this client cannot draw.
    Burn(&'static str),
    /// Without the bitmap `Embed` profiles: a text track the server answered as an embedded bitmap.
    /// Jellyfin tries every `Embed` profile before an `External` one and takes, in its second pass,
    /// any format a text track converts to (`StreamBuilder.GetSubtitleProfile`,
    /// `MediaStream.SupportsSubtitleConversionTo`) — so the bitmap entries capture an SRT track and
    /// the server runs `-codec:s pgssub`, which ffmpeg cannot encode. Asked without them, the same
    /// track is answered `External`.
    Sidecar(&'static str),
}

/// How the conversion at `url` delivers the subtitle at `index` of `src`, from the stream's
/// `DeliveryMethod`; `Err` names an answer the caller asks again, and how ([`Reask`]).
///
/// * `External` — a path on the server (`/Videos/{id}/{source}/Subtitles/{index}/0/Stream.{fmt}`,
///   start 0 because the transcoding profile copies timestamps). The `api_key` the server appends
///   is dropped: the sidecar fetch carries the request header. A remote source's own URL
///   (`IsExternalUrl`) is not on the server, so it cannot be fetched that way.
/// * `Embed` — muxed into the converted Matroska, except DVB, which the server burns instead
///   (`EncodingHelper.NormalizeSubtitleEmbed`) while still answering `Embed`.
/// * `Encode` — burned. `Drop` — not delivered.
/// * `Hls` — a playlist rendition. The profile never offers it and the player reads none.
///
/// A server that states no method is read from the URL it built: `SubtitleMethod` names it, and
/// a `SubtitleStreamIndex` without one is a burn (`StreamInfo.ToUrl`).
fn subtitle_delivery(
    src: &MediaSourceInfo,
    index: Option<i64>,
    url: &str,
    burned: bool,
) -> Result<Option<SubtitleDelivery>, Reask> {
    let Some(index) = index.filter(|i| *i >= 0) else { return Ok(None) };
    if burned {
        return Ok(Some(SubtitleDelivery::Burned));
    }
    let stream = src.media_streams.iter().find(|s| s.kind == "Subtitle" && s.index == index);
    let Some(method) = stream.and_then(|s| s.delivery_method.as_deref()) else {
        return Ok(match query_param(url, "SubtitleMethod") {
            Some(m) if m.eq_ignore_ascii_case("Embed") => Some(SubtitleDelivery::Embedded),
            Some(_) => Some(SubtitleDelivery::Burned),
            None if query_param(url, "SubtitleStreamIndex").is_some() => Some(SubtitleDelivery::Burned),
            None => None,
        });
    };
    match method {
        "External" => {
            let path = stream.and_then(|s| s.delivery_url.as_deref()).unwrap_or("").split('?').next().unwrap_or("");
            let codec = path.rsplit_once("/Stream.").map(|(_, ext)| convert::codec(ext)).unwrap_or_default();
            let on_server = path.starts_with("/Videos/") && path.contains("/Subtitles/");
            let drawable = crate::catalog::capabilities::SIDECAR_SUBTITLES.iter().any(|f| convert::codec(f) == codec);
            if on_server && drawable {
                Ok(Some(SubtitleDelivery::External { path: path.to_string(), codec }))
            } else {
                Err(Reask::Burn("an external subtitle the client cannot fetch from the server"))
            }
        }
        "Embed" => {
            let into_bitmap = stream.is_some_and(|s| s.is_text_subtitle_stream)
                && query_param(url, "SubtitleCodec").is_some_and(|c| {
                    crate::catalog::capabilities::BITMAP_SUBTITLES.iter().any(|b| b.eq_ignore_ascii_case(c))
                });
            if into_bitmap {
                return Err(Reask::Sidecar("a text track the server would convert to a bitmap format it cannot encode"));
            }
            let dvb = stream
                .and_then(|s| s.codec.as_deref())
                .is_some_and(|c| c.eq_ignore_ascii_case("dvbsub") || c.eq_ignore_ascii_case("dvb_subtitle"));
            Ok(Some(if dvb { SubtitleDelivery::Burned } else { SubtitleDelivery::Embedded }))
        }
        "Encode" => Ok(Some(SubtitleDelivery::Burned)),
        "Drop" => Ok(None),
        _ => Err(Reask::Burn("a subtitle delivery the client does not draw")),
    }
}

/// The event log's name for a subtitle delivery.
fn subtitle_label(index: Option<i64>, delivery: Option<&SubtitleDelivery>) -> String {
    let Some(i) = index.filter(|i| *i >= 0) else { return "off".to_string() };
    let how = match delivery {
        None => "",
        Some(SubtitleDelivery::Embedded) => ":embed",
        Some(SubtitleDelivery::External { .. }) => ":external",
        Some(SubtitleDelivery::Burned) => ":burn",
    };
    format!("#{i}{how}")
}

/// The answer's source for `want`, matched by id; without a match, the first — the server lists the
/// queried item's own source first.
fn select_source<'a>(sources: &'a [MediaSourceInfo], want: Option<&str>) -> Option<&'a MediaSourceInfo> {
    let want = want.map(ids::normalize).filter(|w| !w.is_empty());
    let matched = want.as_deref().and_then(|w| sources.iter().find(|s| ids::normalize(&s.id) == w));
    if matched.is_none() && want.is_some() && !sources.is_empty() {
        nj_base::eventlog::log("jf: playbackinfo answered without the requested media source; playing its first");
    }
    matched.or_else(|| sources.first())
}

/// One `/Sessions/Playing*` body for `s` at `time_ms`.
fn report_body(
    s: &Session,
    time_ms: i64,
    paused: bool,
    playlist_item_id: Option<String>,
    failed: bool,
    volume: Option<nj_platform::devcaps::volume::Volume>,
) -> PlaybackReport {
    PlaybackReport {
        item_id: s.item_guid.clone(),
        media_source_id: s.media_source_id.clone(),
        play_session_id: s.play_session_id.clone(),
        position_ticks: ticks::from_ms(time_ms),
        is_paused: paused,
        can_seek: true,
        audio_stream_index: s.audio_index,
        subtitle_stream_index: s.subtitle_index,
        play_method: s.play_method.as_str().into(),
        playback_start_time_ticks: (s.start_time_ticks > 0).then_some(s.start_time_ticks),
        playback_order: Some("Default"),
        repeat_mode: Some("RepeatNone"),
        playlist_item_id,
        is_muted: volume.map(|v| v.muted),
        volume_level: volume.map(|v| i64::from(v.level)),
        failed: failed.then_some(true),
    }
}

/// How the bytes are produced, for the event log: the dashboard's vocabulary.
fn delivery_label(method: PlayMethod, video: &Lane, audio: &Lane) -> &'static str {
    match (method, video.copied, audio.copied) {
        (PlayMethod::DirectPlay | PlayMethod::DirectStream, ..) => "static",
        (PlayMethod::Transcode, true, true) => "remux",
        (PlayMethod::Transcode, true, false) => "audio-transcode",
        (PlayMethod::Transcode, false, _) => "video-transcode",
    }
}

/// What one PlaybackInfo request asks for, beside the profile.
struct InfoAsk {
    direct_play: bool,
    /// `EnableDirectStream`. Defaults to true on the server and is the documented middle rung
    /// between direct play and a re-encode; withdrawing it collapses every container mismatch into
    /// a full video transcode.
    direct_stream: bool,
    allow_video_copy: bool,
    start_ticks: i64,
    media_source_id: Option<String>,
    audio_index: Option<i64>,
    subtitle_index: Option<i64>,
    /// `AlwaysBurnInSubtitleWhenTranscoding` — overrides the subtitle profiles and forces `Encode`.
    burn_subtitle: bool,
}

impl Jf<'_> {
    fn playback_info(&self, guid: &str, profile: Value, max_channels: Option<u32>, ask: &InfoAsk) -> Option<PlaybackInfoResponse> {
        let uid = self.user_id()?;
        // `MaxAudioChannels` is a per-request bound and has no profile equivalent. Without it the
        // server has no reason to downmix, so a 5.1 or 7.1 track reaches a stereo panel at its own
        // channel count and the pipeline plays what it can of it.
        let body = json!({
            "UserId": uid,
            "MaxStreamingBitrate": profile["MaxStreamingBitrate"].clone(),
            "MaxAudioChannels": max_channels,
            "StartTimeTicks": ask.start_ticks,
            "MediaSourceId": ask.media_source_id,
            "AudioStreamIndex": ask.audio_index,
            "SubtitleStreamIndex": ask.subtitle_index.unwrap_or(-1),
            "DeviceProfile": profile,
            "EnableDirectPlay": ask.direct_play,
            "EnableDirectStream": ask.direct_stream,
            "EnableTranscoding": true,
            "AllowVideoStreamCopy": ask.allow_video_copy,
            "AllowAudioStreamCopy": true,
            // Movies and episodes are static files. `RequiresOpening` is only set for Live TV,
            // IPTV and in-progress recordings, none of which this client offers, so there is no
            // live stream to open or close.
            "AutoOpenLiveStream": false,
            "AlwaysBurnInSubtitleWhenTranscoding": ask.burn_subtitle,
        });
        self.post(&format!("/Items/{guid}/PlaybackInfo"), &body)
    }

    /// Negotiate one playback: a single PlaybackInfo request, answered with the method, the URL
    /// to open and the codecs each lane will arrive in. The PlaySessionId is filed under
    /// `ask.session` for the reports that follow.
    pub fn negotiate(&self, ask: &Ask) -> Negotiation {
        self.negotiate_offering(ask, true)
    }

    /// [`Self::negotiate`], offering a conversion the bitmap `Embed` profiles or not
    /// ([`ProfileAsk::embed_bitmap`]).
    fn negotiate_offering(&self, ask: &Ask, embed_bitmap: bool) -> Negotiation {
        let Some(guid) = self.guid(ask.rk) else { return Negotiation::Unreachable };
        let caps = crate::catalog::capabilities::Capabilities::for_attempt(ask.caps);
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| nj_base::eventlog::log(&format!("jf: capabilities {}", caps.summary())));
        let profile = device_profile(&caps, &ProfileAsk {
            direct: ask.direct_play,
            forced: ask.forced,
            ceiling: ask.ceiling,
            burn: ask.burn,
            hls: ask.hls_segment_secs,
            embed_bitmap,
        });
        let Some(r) = self.playback_info(&guid, profile, caps.max_audio_channels(), &InfoAsk {
            direct_play: ask.direct_play,
            // Strict Original means the file as it is or nothing, so it never offers the copy.
            direct_stream: ask.video_copy && !ask.forced,
            allow_video_copy: ask.video_copy,
            start_ticks: ask.start_ticks,
            // Without it the server applies neither track index, so "subtitles off" goes unheard.
            media_source_id: Some(
                ask.media_source_id.map(ids::normalize).filter(|id| !id.is_empty()).unwrap_or_else(|| ids::normalize(&guid)),
            ),
            audio_index: ask.audio_index,
            subtitle_index: ask.subtitle_index,
            burn_subtitle: ask.burn,
        }) else {
            return Negotiation::Unreachable;
        };
        let refuse = |why: Refusal| {
            nj_base::eventlog::log(&format!("jf: playbackinfo refused item={guid} code={}", why.code()));
            Negotiation::Refused(why)
        };
        if let Some(e) = r.error_code.as_deref().filter(|e| !e.is_empty()) {
            return refuse(Refusal::from_error_code(e));
        }
        let Some(src) = select_source(&r.media_sources, ask.media_source_id) else {
            return refuse(Refusal::NoMediaSource);
        };
        let dropped = headers_the_static_proxy_drops(src);
        if ask.direct_play && !ask.forced && src.supports_direct_play && !dropped.is_empty() {
            nj_base::eventlog::log(&format!(
                "jf: source {} needs headers the server's static stream does not send ({}); asking without direct play",
                ids::normalize(&src.id),
                dropped.join(","),
            ));
            return self.negotiate_offering(&Ask { direct_play: false, ..*ask }, embed_bitmap);
        }
        let play_session_id = r.play_session_id.clone().unwrap_or_default();
        let v = pick(src, "Video", None);
        let a = pick(src, "Audio", ask.audio_index);
        let audio_index = ask.audio_index.or(a.map(|s| s.index));
        let direct = ask.direct_play && src.supports_direct_play;
        let mut reasons = Vec::new();
        let (method, path, video, audio) = if direct {
            let path = with_play_session(&direct_path(&guid, src), &play_session_id);
            let lane = |s| Lane { source: source_codec(s), output: source_codec(s), copied: true };
            (PlayMethod::DirectPlay, path, lane(v), lane(a))
        } else if let Some(mut url) = src.transcoding_url.clone().filter(|u| !u.is_empty()) {
            reasons = transcode_reasons(&url);
            let video_copy =
                ask.video_copy && !reasons.iter().any(|r| video_reason(r)) && video_bitrate_admits_copy(&url, v);
            let audio_copy = !reasons.iter().any(|r| audio_reason(r));
            // An HLS master lists the whole film from zero and the player starts on the segment
            // covering the offset; the server refuses a segment request carrying StartTimeTicks.
            let hls = url.split('?').next().is_some_and(|p| p.ends_with(".m3u8"));
            if ask.start_ticks > 0 && !hls && query_param(&url, "StartTimeTicks").is_none() {
                url.push_str(&format!("&StartTimeTicks={}", ask.start_ticks));
            }
            let video = Lane { source: source_codec(v), output: lane_codec(&url, "VideoCodec", v, video_copy), copied: video_copy };
            let audio = Lane { source: source_codec(a), output: lane_codec(&url, "AudioCodec", a, audio_copy), copied: audio_copy };
            (PlayMethod::Transcode, url, video, audio)
        } else {
            return refuse(Refusal::NoDeliveryMethod);
        };
        // A direct play's subtitle is the file's own track, drawn by the client; a conversion's is
        // whatever the server answered for it.
        let subtitle = if method == PlayMethod::DirectPlay {
            None
        } else {
            let index = ask.subtitle_index.unwrap_or(-1);
            match subtitle_delivery(src, ask.subtitle_index, &path, ask.burn) {
                Ok(subtitle) => subtitle,
                Err(Reask::Sidecar(why)) if embed_bitmap => {
                    nj_base::eventlog::log(&format!("jf: subtitle #{index} is {why}; asking for it without the bitmap formats"));
                    return self.negotiate_offering(ask, false);
                }
                Err(Reask::Burn(why) | Reask::Sidecar(why)) => {
                    nj_base::eventlog::log(&format!("jf: subtitle #{index} is {why}; asking for it burned"));
                    return self.negotiate_offering(&Ask { burn: true, ..*ask }, embed_bitmap);
                }
            }
        };
        // Ids, codecs and the server's reasons only: never the URL, which carries the token.
        nj_base::eventlog::log(&format!(
            "jf: playback item={guid} source={} of {} container={} protocol={} headers={} method={} delivery={} \
             video={}->{} audio=#{}:{}->{} subtitle={} start={}s reasons={} play_session={}",
            ids::normalize(&src.id),
            r.media_sources.len(),
            src.container.as_deref().unwrap_or("?"),
            if src.protocol.is_empty() { "?" } else { src.protocol.as_str() },
            {
                let names = required_header_names(src);
                if names.is_empty() { "-".to_string() } else { names.join(",") }
            },
            method.as_str(),
            delivery_label(method, &video, &audio),
            video.source,
            video.output,
            audio_index.map_or_else(|| "?".to_string(), |i| i.to_string()),
            audio.source,
            audio.output,
            subtitle_label(ask.subtitle_index, subtitle.as_ref()),
            ask.start_ticks / 10_000_000,
            if reasons.is_empty() { "-".to_string() } else { reasons.join(",") },
            if play_session_id.is_empty() { "-" } else { play_session_id.as_str() },
        ));
        put_session(ask.session, Session {
            item_guid: guid,
            media_source_id: src.id.clone(),
            play_session_id: play_session_id.clone(),
            play_method: method,
            audio_index,
            subtitle_index: ask.subtitle_index,
            started: false,
            start_time_ticks: 0,
            seq: 0,
        });
        let path = if super::url::has_api_key(&path) { path } else { super::url::with_api_key(&path, &self.token()) };
        Negotiation::Playable(Negotiated {
            method,
            url: StreamUrl { origin: self.origin().clone(), path }.to_url(),
            video,
            audio,
            audio_index,
            subtitle_index: ask.subtitle_index,
            subtitle,
            media_source_id: src.id.clone(),
            play_session_id,
            runtime_ns: ticks::to_ns(src.run_time_ticks.unwrap_or(0)),
        })
    }

    /// A conversion of the playing item, rebuilt from the route's encode contract: a seek, a track
    /// switch or a quality change re-negotiates with direct play withdrawn.
    ///
    /// The replacement continues the playback it replaces (see the module doc): same media source,
    /// same start. Only the encoder — and so the `PlaySessionId` — is new.
    pub fn transcode(&self, spec: &TranscodeSpec) -> Negotiation {
        let c = spec.contract;
        let start_ticks = match spec.offset {
            TranscodeOffset::Fresh => 0,
            TranscodeOffset::AtMicros(us) => us.saturating_mul(10),
        };
        let prior = Some(spec.continues).filter(|k| !k.is_empty() && *k != spec.session).and_then(session);
        let subtitle_index = (spec.subtitle_stream_id > 0).then(|| ids::stream_index(spec.subtitle_stream_id));
        let hls_segment_secs = match c.delivery {
            TranscodeDelivery::FixedHls { seconds_per_segment } => Some(seconds_per_segment),
            TranscodeDelivery::ProgressiveMkv => None,
        };
        let negotiation = self.negotiate(&Ask {
            rk: spec.rating_key,
            session: spec.session,
            media_source_id: prior.as_ref().map(|p| p.media_source_id.as_str()).filter(|id| !id.is_empty()),
            audio_index: (spec.audio_stream_id > 0).then(|| ids::stream_index(spec.audio_stream_id)),
            // The selected subtitle, not a burn: the answer says how it is delivered, and the
            // server burns only what no soft delivery fits (`StreamBuilder.GetSubtitleProfile`).
            subtitle_index,
            start_ticks,
            ceiling: c.ceiling,
            direct_play: false,
            // A copied video is cut at the source's keyframes, and the playlist's durations follow
            // them only when the server could extract them (`DynamicHlsPlaylistGenerator`); the
            // player's HLS timeline is built from those durations, so an HLS rung is re-encoded.
            video_copy: hls_segment_secs.is_none() && (c.remux || !c.no_video_copy),
            forced: false,
            burn: false,
            hls_segment_secs,
            caps: spec.caps,
        });
        if let (Negotiation::Playable(_), Some(p)) = (&negotiation, prior) {
            update_session(spec.session, |s| {
                s.started = p.started;
                s.start_time_ticks = p.start_time_ticks;
            });
        }
        negotiation
    }

    /// The part URL for direct play: the static stream plus this playback's PlaySessionId.
    pub fn direct_play_url(&self, part_key: &str, session_key: &str) -> StreamUrl {
        let play_session_id = session(session_key).map(|s| s.play_session_id).unwrap_or_default();
        let path = with_play_session(part_key, &play_session_id);
        StreamUrl { origin: self.origin().clone(), path: super::url::with_api_key(&path, &self.token()) }
    }

    /// `GET /Playback/BitrateTest?Size=` — the payload Jellyfin clients time to choose
    /// `MaxStreamingBitrate` when the viewer leaves quality on Auto.
    pub fn bitrate_test_url(&self, bytes: usize) -> StreamUrl {
        let path = format!("/Playback/BitrateTest?Size={bytes}");
        StreamUrl { origin: self.origin().clone(), path: super::url::with_api_key(&path, &self.token()) }
    }

    /// End the server's encoder for `session` — the one a seek, a track switch or a quality change
    /// replaced, or one a refused plan started — with `DELETE /Videos/ActiveEncodings`, the call the
    /// web client's `stopActiveEncodings` makes. The route is real on 10.10 through 12 but hidden
    /// from the OpenAPI document (`HlsSegmentController`, `IgnoreApi`); the server kills the jobs
    /// whose `PlaySessionId` matches and answers 204.
    ///
    /// Not a `Stopped` report: the server writes that report's position into the user's resume
    /// point and announces the end of the playback, while the playback this encoder served may well
    /// be continuing on its replacement. Only a server without the route (404/405) gets one, marked
    /// `Failed`, which the server takes as "kill the job, leave the user's data alone".
    ///
    /// The final stop of a playback is the timeline's `Stopped` with the real position; it already
    /// ends that playback's job and drops the entry, so this answers true without a request.
    pub fn transcode_stop(&self, session_key: &str) -> bool {
        let Some(s) = session(session_key) else { return true };
        if s.play_method != PlayMethod::Transcode || s.play_session_id.is_empty() {
            return true;
        }
        let path = super::api::Q::new("/Videos/ActiveEncodings")
            .s("deviceId", &self.identity().device_id)
            .s("playSessionId", &s.play_session_id)
            .build();
        match self.status(&path, crate::http::Method::Delete, None) {
            Some(status) if (200..300).contains(&status) => true,
            Some(404 | 405) => {
                nj_base::eventlog::log("jf: no ActiveEncodings route; ending the encoder with a failed stop");
                self.report("/Sessions/Playing/Stopped", &s, 0, false, None, true)
            }
            other => {
                nj_base::eventlog::log(&format!(
                    "jf: ending encoder play_session={} failed status={}",
                    s.play_session_id,
                    other.map_or_else(|| "none".to_string(), |c| c.to_string()),
                ));
                false
            }
        }
    }

    pub fn timeline(&self, r: &TimelineReport) -> bool {
        let mut s = session(r.session).unwrap_or_else(|| Session {
            item_guid: self.guid(r.rating_key).unwrap_or_default(),
            ..Default::default()
        });
        if s.item_guid.is_empty() {
            return false;
        }
        if r.audio_stream_id > 0 {
            s.audio_index = Some(ids::stream_index(r.audio_stream_id));
        }
        if r.subtitle_stream_id > 0 {
            s.subtitle_index = Some(ids::stream_index(r.subtitle_stream_id));
        }
        // Stamped once, on the first report, and carried by every later one. Taken here rather
        // than at the negotiation because one that never reaches the engine (a refusal, a
        // superseded resolve) never starts a playback to attribute time to.
        if s.start_time_ticks == 0 {
            s.start_time_ticks = ticks::now_utc();
        }
        // The queue position, when the caller has one. Sent as `PlaylistItemId` so the server can
        // place this playback in the queue it was told about; "0" is this protocol's "no position"
        // and must not be forwarded as one.
        let playlist_item_id = Some(r.play_queue_item_id)
            .filter(|id| !id.is_empty() && *id != "0")
            .map(str::to_string);
        let paused = r.state == TimelineState::Paused;
        match r.state {
            // A playback that never put a picture on the panel is no playback: end its encoder and
            // report nothing — its stop's position (0, or wherever the load stopped) would
            // otherwise overwrite the resume point.
            TimelineState::Stopped if !s.started && !r.presented => {
                let ended = self.transcode_stop(r.session);
                drop_session(r.session);
                ended
            }
            TimelineState::Stopped => {
                // A playback stopped before its first report was ever taken still STARTED as far
                // as the server is concerned: say so first, or the stop arrives for a session it
                // never saw begin.
                if !s.started {
                    s.started = self.report("/Sessions/Playing", &s, r.time_ms, paused, playlist_item_id.clone(), false);
                }
                drop_session(r.session);
                self.report("/Sessions/Playing/Stopped", &s, r.time_ms, paused, playlist_item_id, false)
            }
            _ if !s.started => {
                let ok = self.report("/Sessions/Playing", &s, r.time_ms, paused, playlist_item_id, false);
                s.started = ok;
                put_session(r.session, s);
                ok
            }
            _ => {
                let ok = self.report("/Sessions/Playing/Progress", &s, r.time_ms, paused, playlist_item_id, false);
                // The start stamp is this session's and outlives the local copy above.
                put_session(r.session, s);
                ok
            }
        }
    }

    fn report(
        &self,
        path: &str,
        s: &Session,
        time_ms: i64,
        paused: bool,
        playlist_item_id: Option<String>,
        failed: bool,
    ) -> bool {
        // This report carries the last reading; the next one carries the one asked for here.
        nj_platform::devcaps::volume::refresh();
        let body = report_body(s, time_ms, paused, playlist_item_id, failed, nj_platform::devcaps::volume::latest());
        self.post_ok(path, Some(&body))
    }

    /// The signed-in user's language preferences (`UserConfiguration`), which every track pick
    /// ranks against.
    pub fn language_prefs(&self) -> Option<LanguagePrefs> {
        let me: UserDto = self.get("/Users/Me")?;
        let c = me.configuration;
        let lang = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Some(LanguagePrefs {
            audio: lang(c.audio_language_preference),
            subtitle: lang(c.subtitle_language_preference),
            subtitle_mode: c.subtitle_mode,
            play_default_audio_track: c.play_default_audio_track,
        })
    }

    /// A bounded episode neighbourhood in the server's series order. Every episode start
    /// refreshes it, so Previous survives Next/Up Next without retaining a stale Plex queue.
    /// Movies, extras and unavailable neighbourhoods remain a one-item queue.
    pub fn create_play_queue(&self, rk: &str, continuous: bool) -> Option<MediaContainer> {
        let guid = self.guid(rk)?;
        let uid = self.user_id()?;
        let it: BaseItemDto = self.get(&super::api::Q::new(format!("/Items/{guid}")).s("userId", &uid)
            .s("Fields", "MediaSources").build())?;
        let mut rows = vec![convert::item(&it, 0)];
        if continuous && it.kind == "Episode" {
            if let Some(series) = it.series_id.as_deref().map(ids::normalize).filter(|s| !s.is_empty()) {
                let r: Option<QueryResult<BaseItemDto>> = self.get(&super::api::Q::new(format!("/Shows/{series}/Episodes"))
                    .s("userId", &uid).s("AdjacentTo", &guid).b("IsMissing", false).s("Fields", "MediaSources").i("Limit", 3).build());
                if let Some(r) = r.filter(|r| r.items.len() <= 3
                    && r.items.iter().any(|e| ids::normalize(&e.id) == guid)
                    && r.items.iter().all(|e| e.kind == "Episode"
                        && e.series_id.as_deref().is_some_and(|s| ids::normalize(s) == series))) {
                    rows = r.items.iter().map(|e| convert::item(e, 0)).collect();
                }
            }
        }
        // GUID-derived identities survive a sliding neighbourhood; the selected row is not
        // necessarily first. PlaylistItemId is an opaque client queue identity, not a position.
        for m in &mut rows { m.play_queue_item_id = m.rating_key.parse().unwrap_or(0); }
        let selected = rows.iter().position(|m| m.rating_key == rk).unwrap_or(0) as i64;
        let selected_id = rows[selected as usize].play_queue_item_id;
        let n = rows.len() as i64;
        Some(MediaContainer {
            play_queue_id: ids::intern(&guid),
            play_queue_selected_item_id: selected_id,
            play_queue_total_count: n,
            play_queue_selected_item_offset: selected,
            size: n,
            metadata: rows,
            ..Default::default()
        })
    }
}

/// `UserConfiguration`'s playback half.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LanguagePrefs {
    /// `AudioLanguagePreference`, an ISO 639-2 code (`eng`), `None` when unset.
    pub audio: Option<String>,
    pub subtitle: Option<String>,
    /// `Default | Always | OnlyForced | None | Smart`.
    pub subtitle_mode: String,
    /// `PlayDefaultAudioTrack` — the file's default track wins over the language preference.
    pub play_default_audio_track: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> nj_platform::devcaps::Caps {
        let mut c = nj_platform::devcaps::Caps::assumed();
        c.audio = "aac,ac3,eac3,dts".into();
        c.audio_channels.insert("dts".into(), 6);
        c
    }

    fn profile(ask: &ProfileAsk) -> Value {
        let d = device();
        device_profile(
            &crate::catalog::capabilities::Capabilities::of(&d, nj_platform::devcaps::dv::DvCapability::Unknown),
            ask,
        )
    }

    #[test]
    fn forced_profiles_keep_hard_firmware_audio_limits() {
        let mut device = device();
        device.native_audio_limits.insert("aac".into(), 2);
        let caps = crate::catalog::capabilities::Capabilities::of(&device, nj_platform::devcaps::dv::DvCapability::Unknown);
        let profile = device_profile(&caps, &ProfileAsk { direct: true, forced: true, ceiling: None, burn: false, hls: None, embed_bitmap: true });
        let rows = profile["CodecProfiles"].as_array().unwrap();
        assert!(rows.iter().any(|row| row["Type"] == "VideoAudio" && row["Codec"] == "aac"
            && row["Conditions"][0]["Property"] == "AudioChannels"
            && row["Conditions"][0]["Value"] == "2"));
    }

    #[test]
    fn opus_payload_support_alone_does_not_enable_server_profiles() {
        for forced in [false, true] {
            let p = profile(&ProfileAsk { direct: true, forced, ceiling: None, burn: false, hls: None, embed_bitmap: true });
            assert!(!p["DirectPlayProfiles"][0]["AudioCodec"].as_str().unwrap().split(',').any(|c| c == "opus"));
            for target in p["TranscodingProfiles"].as_array().unwrap() {
                assert!(!target["AudioCodec"].as_str().unwrap().split(',').any(|c| c == "opus"));
            }
        }
    }

    #[test]
    fn the_direct_profile_advertises_the_panels_own_decode_limits() {
        let p = profile(&ProfileAsk { direct: true, forced: false, ceiling: None, burn: false, hls: None, embed_bitmap: true });
        let dp = &p["DirectPlayProfiles"][0];
        assert_eq!(dp["VideoCodec"], "h264,hevc");
        assert_eq!(dp["AudioCodec"], "aac,ac3,eac3,dts");
        assert_eq!(p["TranscodingProfiles"][0]["Container"], "mkv");
        assert_eq!(p["TranscodingProfiles"][0]["VideoCodec"], "h264,hevc");
        assert_eq!(p["TranscodingProfiles"][0]["AudioCodec"], "ac3,eac3,aac,dts");
        let video = &p["CodecProfiles"][0]["Conditions"];
        assert_eq!(video[0]["Value"], "3840");
        assert_eq!(video[1]["Value"], "2176");
        let dts = p["CodecProfiles"].as_array().unwrap().iter().find(|c| c["Codec"] == "dts").unwrap();
        assert_eq!(dts["Conditions"][0]["Value"], "6");
        assert!(p["SubtitleProfiles"].as_array().unwrap().iter().any(|s| s["Format"] == "pgssub" && s["Method"] == "Embed"));
    }

    fn range_condition(p: &Value, codec: &str) -> Option<String> {
        p["CodecProfiles"].as_array().unwrap().iter()
            .filter(|c| c["Codec"] == codec)
            .flat_map(|c| c["Conditions"].as_array().unwrap().iter())
            .find(|c| c["Property"] == "VideoRangeType")
            .map(|c| {
                assert_eq!(c["Condition"], "EqualsAny");
                c["Value"].as_str().unwrap().to_string()
            })
    }

    /// A reading of the set's volume reaches the report as `IsMuted`/`VolumeLevel`; without one both
    /// are absent rather than claiming an unmuted full volume.
    #[test]
    fn the_report_states_the_sets_volume_only_once_read() {
        use nj_platform::devcaps::volume::Volume;
        let s = Session { item_guid: "g".into(), ..Default::default() };
        let read = serde_json::to_value(report_body(&s, 0, false, None, false, Some(Volume { level: 12, muted: true }))).unwrap();
        assert_eq!(read["IsMuted"], true);
        assert_eq!(read["VolumeLevel"], 12);
        let unread = serde_json::to_value(report_body(&s, 0, false, None, false, None)).unwrap();
        assert!(unread.get("IsMuted").is_none() && unread.get("VolumeLevel").is_none(), "{unread}");
    }

    /// The server is told the panel's dynamic ranges once they are measured, so an HDR source on an
    /// SDR panel is tone-mapped rather than direct-played or copied washed out. Unmeasured, the
    /// profile says nothing and the server applies its own defaults.
    #[test]
    fn the_profile_states_the_measured_panels_video_ranges() {
        use nj_platform::devcaps::{dv::DvCapability, hdr::HdrCapability};
        let d = device();
        let ask = ProfileAsk { direct: true, forced: false, ceiling: None, burn: false, hls: None, embed_bitmap: true };
        let caps = crate::catalog::capabilities::Capabilities::of(&d, DvCapability::Unknown);
        let unmeasured = device_profile(&caps, &ask);
        assert_eq!(range_condition(&unmeasured, "hevc"), None);
        assert_eq!(range_condition(&unmeasured, "h264"), None);

        let sdr = device_profile(&caps.with_hdr(HdrCapability::Unsupported), &ask);
        assert_eq!(range_condition(&sdr, "hevc").as_deref(), Some("SDR|DOVIWithSDR"));
        assert_eq!(range_condition(&sdr, "h264").as_deref(), Some("SDR"));

        let hdr = device_profile(&caps.with_hdr(HdrCapability::Supported), &ask);
        let ranges = range_condition(&hdr, "hevc").unwrap();
        assert!(ranges.split('|').any(|r| r == "HDR10") && ranges.split('|').any(|r| r == "HLG"), "{ranges}");
        assert!(!ranges.split('|').any(|r| r == "DOVI"), "{ranges}");
    }

    #[test]
    fn av1_dolby_negotiation_requires_an_av1_decoder() {
        use nj_platform::devcaps::{dv::DvCapability, hdr::HdrCapability};
        let ask = ProfileAsk { direct: true, forced: false, ceiling: None, burn: false, hls: None, embed_bitmap: true };
        let mut d = device();
        for av1 in [false, true] {
            d.av1 = av1;
            d.av1_row = if av1 { (3840, 2160, 60) } else { (0, 0, 0) };
            let caps = crate::catalog::capabilities::Capabilities::of(&d, DvCapability::Supported)
                .with_hdr(HdrCapability::Supported);
            let p = device_profile(&caps, &ask);
            let offered = p["DirectPlayProfiles"][0]["VideoCodec"].as_str().unwrap().split(',').any(|c| c == "av1");
            assert_eq!(offered, av1);
            if av1 {
                let ranges = range_condition(&p, "av1").unwrap();
                assert!(ranges.split('|').any(|r| r == "DOVI"));
                assert!(ranges.split('|').any(|r| r == "DOVIWithHDR10"));
                assert!(!ranges.contains("EL") && !ranges.contains("DOVIInvalid"));
            } else {
                assert_eq!(range_condition(&p, "av1"), None);
            }
        }
    }

    #[test]
    fn a_ceiling_bounds_the_original_file_as_well_as_the_encode() {
        let c = Ceiling { max_kbps: 4000, max_w: 1280, max_h: 720 };
        let p = profile(&ProfileAsk { direct: false, forced: false, ceiling: Some(c), burn: true, hls: None, embed_bitmap: true });
        assert_eq!(p["MaxStreamingBitrate"], 4_000_000);
        // The whole point: an explicit ask the direct-play branch cannot ignore.
        assert_eq!(p["MaxStaticBitrate"], 4_000_000);
        assert_eq!(p["CodecProfiles"][0]["Conditions"][0]["Value"], "1280");
        assert!(p["DirectPlayProfiles"].as_array().unwrap().is_empty());
        assert!(p["SubtitleProfiles"].as_array().unwrap().is_empty(), "a burn withdraws every soft method");
    }

    /// An HLS ask names the protocol, the MPEG-TS segments the player demuxes and the route's
    /// segment length; the codecs are the progressive profile's.
    /// A set whose table lists VP9 and AV1 decoders is offered both, in WebM too: direct play,
    /// copy lanes after the H.264 encode head on the progressive target (but never on the HLS one,
    /// whose TS segments the player demuxes only as H.264), and each codec's own bounds.
    #[test]
    fn vp9_and_av1_are_offered_where_the_table_lists_them() {
        let mut d = device();
        d.vp9 = true;
        d.vp9_row = (3840, 2160, 60);
        d.av1 = true;
        d.av1_row = (1920, 1080, 60);
        let caps = crate::catalog::capabilities::Capabilities::of(&d, nj_platform::devcaps::dv::DvCapability::Unknown)
            .with_hdr(nj_platform::devcaps::hdr::HdrCapability::Supported);
        let ask = ProfileAsk { direct: true, forced: false, ceiling: None, burn: false, hls: None, embed_bitmap: true };
        let p = device_profile(&caps, &ask);
        let dp = &p["DirectPlayProfiles"][0];
        assert_eq!(dp["VideoCodec"], "h264,hevc,vp9,av1");
        assert!(dp["Container"].as_str().unwrap().split(',').any(|c| c == "webm"));
        assert_eq!(p["TranscodingProfiles"][0]["VideoCodec"], "h264,hevc,vp9,av1");
        let codec = |name: &str| {
            p["CodecProfiles"].as_array().unwrap().iter().find(|c| c["Codec"] == name).cloned().unwrap()
        };
        let value = |c: &Value, prop: &str| {
            c["Conditions"].as_array().unwrap().iter().find(|x| x["Property"] == prop).map(|x| x["Value"].clone())
        };
        let vp9 = codec("vp9");
        assert_eq!(value(&vp9, "Width"), Some("3840".into()));
        assert_eq!(value(&vp9, "VideoBitDepth"), Some("8".into()), "8-bit VP9 unless measured otherwise");
        assert_eq!(value(&vp9, "VideoRangeType"), Some("SDR".into()));
        let av1 = codec("av1");
        assert_eq!(value(&av1, "Width"), Some("1920".into()));
        assert_eq!(value(&av1, "Height"), Some("1080".into()));
        assert_eq!(value(&av1, "VideoRangeType"), Some("SDR|HDR10|HDR10Plus|HLG|DOVIWithSDR|DOVIWithHDR10|DOVIWithHDR10Plus|DOVIWithHLG".into()));

        let hls = device_profile(
            &caps,
            &ProfileAsk { direct: false, forced: false, ceiling: None, burn: false, hls: Some(2), embed_bitmap: true },
        );
        assert_eq!(hls["TranscodingProfiles"][0]["VideoCodec"], "h264,hevc", "TS segments carry no VP9/AV1 lane");

        let plain = profile(&ask);
        assert!(plain["CodecProfiles"].as_array().unwrap().iter().all(|c| c["Codec"] != "vp9" && c["Codec"] != "av1"));
    }

    #[test]
    fn an_hls_ask_offers_an_hls_conversion() {
        let p = profile(&ProfileAsk { direct: false, forced: false, ceiling: None, burn: false, hls: Some(2), embed_bitmap: true });
        let t = &p["TranscodingProfiles"][0];
        assert_eq!(p["TranscodingProfiles"].as_array().unwrap().len(), 1);
        assert_eq!(t["Protocol"], "hls");
        assert_eq!(t["Container"], "ts");
        assert_eq!(t["SegmentLength"], 2);
        assert_eq!(t["MinSegments"], 1);
        assert_eq!(t["Context"], "Streaming");
        assert_eq!(t["VideoCodec"], "h264,hevc");
        assert_eq!(t["AudioCodec"], "ac3,eac3,aac,dts");
    }

    #[test]
    fn no_ceiling_leaves_both_rates_unbounded() {
        let p = profile(&ProfileAsk { direct: true, forced: false, ceiling: None, burn: false, hls: None, embed_bitmap: true });
        assert_eq!(p["MaxStreamingBitrate"], UNBOUNDED_BPS);
        assert_eq!(p["MaxStaticBitrate"], UNBOUNDED_BPS);
    }

    #[test]
    fn a_conversion_still_offers_the_soft_subtitle_methods() {
        let p = profile(&ProfileAsk { direct: false, forced: false, ceiling: None, burn: false, hls: None, embed_bitmap: true });
        let subs = p["SubtitleProfiles"].as_array().unwrap();
        assert!(!subs.is_empty(), "an empty list asks the server to burn the item's default track");
        // Text travels as a sidecar, so the video lane stays copyable...
        assert!(subs.iter().any(|s| s["Format"] == "srt" && s["Method"] == "External"));
        // ...and a bitmap track, which has no text form, is muxed rather than burned.
        assert!(subs.iter().any(|s| s["Format"] == "pgssub" && s["Method"] == "Embed"));
        // `External` is not available for a bitmap format.
        assert!(!subs.iter().any(|s| s["Format"] == "pgssub" && s["Method"] == "External"));
    }

    #[test]
    fn the_play_method_is_one_of_the_three_the_api_defines() {
        assert_eq!(PlayMethod::DirectPlay.as_str(), "DirectPlay");
        assert_eq!(PlayMethod::DirectStream.as_str(), "DirectStream");
        assert_eq!(PlayMethod::Transcode.as_str(), "Transcode");
    }

    #[test]
    fn the_log_names_a_conversion_by_what_it_copies() {
        let lane = |copied| Lane { copied, ..Default::default() };
        assert_eq!(delivery_label(PlayMethod::DirectPlay, &lane(true), &lane(true)), "static");
        assert_eq!(delivery_label(PlayMethod::Transcode, &lane(true), &lane(true)), "remux");
        assert_eq!(delivery_label(PlayMethod::Transcode, &lane(true), &lane(false)), "audio-transcode");
        assert_eq!(delivery_label(PlayMethod::Transcode, &lane(false), &lane(true)), "video-transcode");
    }

    #[test]
    fn the_source_played_is_the_one_asked_for() {
        let src = |id: &str| MediaSourceInfo { id: id.into(), ..Default::default() };
        let sources = [src("aaaa0000aaaa0000aaaa0000aaaa0000"), src("BBBB0000-BBBB-0000-BBBB-0000BBBB0000")];
        assert_eq!(select_source(&sources, Some("bbbb0000bbbb0000bbbb0000bbbb0000")).map(|s| &s.id), Some(&sources[1].id));
        // No ask, or an id the answer does not hold: the server's own first choice.
        assert_eq!(select_source(&sources, None).map(|s| &s.id), Some(&sources[0].id));
        assert_eq!(select_source(&sources, Some("cccc")).map(|s| &s.id), Some(&sources[0].id));
        assert!(select_source(&[], Some("aaaa")).is_none());
    }

    #[test]
    fn a_stream_path_carries_its_play_session_once() {
        assert_eq!(with_play_session("/Videos/x/stream.mkv?static=true", "p 1"), "/Videos/x/stream.mkv?static=true&PlaySessionId=p%201");
        assert_eq!(with_play_session("/Videos/x/stream.mkv", "p"), "/Videos/x/stream.mkv?PlaySessionId=p");
        assert_eq!(with_play_session("/x?PlaySessionId=a", "b"), "/x?PlaySessionId=a");
        assert_eq!(with_play_session("/x?static=true", ""), "/x?static=true");
    }

    #[test]
    fn refusal_codes_map_to_their_category() {
        assert_eq!(Refusal::from_error_code("NotAllowed"), Refusal::NotAllowed);
        assert_eq!(Refusal::from_error_code("NoCompatibleStream"), Refusal::NoCompatibleStream);
        assert_eq!(Refusal::from_error_code("RateLimitExceeded"), Refusal::RateLimitExceeded);
        assert_eq!(Refusal::from_error_code("Nope"), Refusal::Unrecognized("Nope".into()));
        assert_eq!(Refusal::Unrecognized("Nope".into()).code(), "Nope");
    }

    /// The table used to empty itself past 64 entries, taking the playing session's PlaySessionId
    /// with it; every later report then named no playback. A long run of seeks must retire the
    /// encoders it replaced and keep the one that is still reporting.
    #[test]
    fn a_long_run_of_encoders_never_evicts_the_playing_session() {
        let _g = nj_base::testlock::serial();
        let s = || Session { item_guid: "evict-item".into(), play_session_id: "ps".into(), ..Default::default() };
        put_session("evict-playing", s());
        for i in 0..(SESSION_CAP * 2) {
            put_session(&format!("evict-encoder-{i}"), s());
            // Every report rewrites the playing entry.
            if let Some(p) = session("evict-playing") {
                put_session("evict-playing", p);
            }
        }
        assert!(session("evict-playing").is_some(), "the playing session survived");
        assert!(session("evict-encoder-0").is_none(), "the oldest replaced encoder went");
        assert!(sessions().lock().unwrap().len() <= SESSION_CAP);
        sessions().lock().unwrap().retain(|k, _| !k.starts_with("evict-"));
    }

    #[test]
    fn forced_original_offers_no_target_and_no_limits() {
        let p = profile(&ProfileAsk { direct: true, forced: true, ceiling: None, burn: false, hls: None, embed_bitmap: true });
        assert!(p["TranscodingProfiles"].as_array().unwrap().is_empty());
        assert!(p["CodecProfiles"].as_array().unwrap().is_empty());
    }

    /// Force Direct Play offers DTS only to a set whose table has a DTS row (issue #24 is a C1,
    /// which has none): an absent decoder takes the Load and never starts.
    #[test]
    fn forced_original_offers_dts_only_with_a_dts_row() {
        let audio = |c: &nj_platform::devcaps::Caps| {
            let caps = crate::catalog::capabilities::Capabilities::of(c, nj_platform::devcaps::dv::DvCapability::Unknown);
            let ask = ProfileAsk { direct: true, forced: true, ceiling: None, burn: false, hls: None, embed_bitmap: true };
            device_profile(&caps, &ask)["DirectPlayProfiles"][0]["AudioCodec"].as_str().unwrap().to_string()
        };
        assert_eq!(audio(&device()), "aac,ac3,eac3,dts");
        assert_eq!(audio(&nj_platform::devcaps::Caps::assumed()), "aac,ac3,eac3");
    }

    #[test]
    fn transcode_reasons_split_into_lanes() {
        let url = "/videos/x/stream.mkv?VideoCodec=hevc,h264&AudioCodec=ac3&TranscodeReasons=ContainerNotSupported%2CAudioCodecNotSupported";
        let r = transcode_reasons(url);
        assert_eq!(r, ["ContainerNotSupported", "AudioCodecNotSupported"]);
        assert!(!r.iter().any(|r| video_reason(r)));
        assert!(r.iter().any(|r| audio_reason(r)));
    }

    /// Jellyfin sets `DirectPlayError` only as a fallback when no direct-play or direct-stream
    /// profile matched with no compatibility reason (`StreamBuilder.GetVideoDirectPlayProfile`);
    /// it is in none of the video reason groups, and the video copy decision
    /// (`GetVideoTranscodeProfile`) never reads the reasons. Jellyfin 12.0 answers a TrueHD
    /// switch of an HEVC film `AudioCodecNotSupported,DirectPlayError` and COPIES the HEVC; read
    /// as a video reason, the Load declared H.264 and the demuxer refused the copied HEVC.
    #[test]
    fn a_direct_play_error_says_nothing_about_the_video_lane() {
        assert!(!video_reason("DirectPlayError"));
        assert!(!audio_reason("DirectPlayError"));
        assert!(video_reason("VideoCodecNotSupported") && video_reason("RefFramesNotSupported"));
    }

    #[test]
    fn a_copied_lane_reports_the_source_codec_and_an_encoded_one_the_target() {
        let url = "/videos/x/stream.mkv?VideoCodec=hevc,h264&AudioCodec=ac3,eac3";
        let v = MediaStream { kind: "Video".into(), codec: Some("h264".into()), ..Default::default() };
        let a = MediaStream { kind: "Audio".into(), codec: Some("truehd".into()), ..Default::default() };
        assert_eq!(lane_codec(url, "VideoCodec", Some(&v), true), "h264");
        assert_eq!(lane_codec(url, "VideoCodec", Some(&v), false), "hevc");
        assert_eq!(lane_codec(url, "AudioCodec", Some(&a), true), "ac3", "a codec off the list cannot be copied");
        let dts = MediaStream { kind: "Audio".into(), codec: Some("dts".into()), ..Default::default() };
        assert_eq!(lane_codec("/x?AudioCodec=dts", "AudioCodec", Some(&dts), true), "dca", "the app's spelling");
    }

    /// The bitrate rule of `CanStreamCopyVideo`: the URL's `VideoBitrate` bounds the copy, an
    /// unknown stream bitrate fails it, and no `VideoBitrate` leaves the copy to the reasons.
    #[test]
    fn a_video_over_the_requested_bitrate_is_not_copied() {
        let url = "/videos/x/stream.mkv?VideoCodec=h264,hevc,vp9,av1&VideoBitrate=77242000&TranscodeReasons=ContainerBitrateExceedsLimit";
        let hevc = |bit_rate| MediaStream { kind: "Video".into(), codec: Some("hevc".into()), bit_rate, ..Default::default() };
        assert!(!video_bitrate_admits_copy(url, Some(&hevc(Some(84_130_112)))));
        assert!(video_bitrate_admits_copy(url, Some(&hevc(Some(77_242_000)))), "equal is within");
        assert!(video_bitrate_admits_copy(url, Some(&hevc(Some(40_000_000)))));
        assert!(!video_bitrate_admits_copy(url, Some(&hevc(None))), "unknown is refused");
        assert!(video_bitrate_admits_copy("/videos/x/stream.mkv?VideoCodec=hevc", Some(&hevc(None))));
        assert!(!transcode_reasons(url).iter().any(|r| video_reason(r)), "no reason names the video");
        let copied = video_bitrate_admits_copy(url, Some(&hevc(Some(84_130_112))));
        assert_eq!(lane_codec(url, "VideoCodec", Some(&hevc(Some(84_130_112))), copied), "h264");
    }

    /// The server moves codecs it will not pick to the end of the list before taking the head
    /// (`EncodingHelper.ShiftVideoCodecsIfNeeded`/`ShiftAudioCodecsIfNeeded`). HEVC moves unless
    /// the administrator allowed HEVC encoding, which a client cannot read, so H.264 leads the target
    /// list as in the web client; AC-3 and E-AC-3 move for a source under six channels, DTS and
    /// TrueHD otherwise. The lane names the codec that arrives: a server without HEVC encoding sent
    /// H.264 to a pipeline loaded for HEVC (live 12.0, 2026-10-08).
    #[test]
    fn an_encoded_lane_is_the_codec_the_server_encodes() {
        let p = profile(&ProfileAsk { direct: false, forced: false, ceiling: None, burn: false, hls: None, embed_bitmap: true });
        assert_eq!(p["TranscodingProfiles"][0]["VideoCodec"], "h264,hevc");
        let url = "/videos/x/stream.mkv?VideoCodec=h264,hevc&AudioCodec=ac3,eac3,aac";
        let hevc = MediaStream { kind: "Video".into(), codec: Some("hevc".into()), ..Default::default() };
        assert_eq!(lane_codec(url, "VideoCodec", Some(&hevc), false), "h264");
        assert_eq!(lane_codec(url, "VideoCodec", Some(&hevc), true), "hevc", "an HEVC source stays copyable");
        let audio = |codec: &str, channels: Option<i64>| MediaStream {
            kind: "Audio".into(), codec: Some(codec.into()), channels, ..Default::default()
        };
        assert_eq!(lane_codec(url, "AudioCodec", Some(&audio("opus", Some(2))), false), "aac");
        assert_eq!(lane_codec(url, "AudioCodec", Some(&audio("truehd", Some(8))), false), "ac3");
        assert_eq!(lane_codec(url, "AudioCodec", Some(&audio("flac", None)), false), "ac3", "unknown counts as six");
        assert_eq!(lane_codec("/x?AudioCodec=dts,ac3", "AudioCodec", Some(&audio("truehd", Some(6))), false), "ac3");
        assert_eq!(lane_codec("/x?AudioCodec=ac3,eac3", "AudioCodec", Some(&audio("opus", Some(2))), false), "ac3", "nothing to move to");
    }

    /// Each `DeliveryMethod` the server can answer, the cases the route tests do not reach, and a
    /// server that states none (read from the URL it built, `StreamInfo.ToUrl`).
    #[test]
    fn a_subtitle_delivery_is_read_from_the_answer() {
        let sub = |codec: &str, method: Option<&str>, url: Option<&str>| MediaSourceInfo {
            media_streams: vec![MediaStream {
                kind: "Subtitle".into(),
                index: 2,
                codec: Some(codec.into()),
                delivery_method: method.map(str::to_string),
                delivery_url: url.map(str::to_string),
                ..Default::default()
            }],
            ..Default::default()
        };
        let plain = "/videos/x/stream.mkv?VideoCodec=hevc";
        assert_eq!(subtitle_delivery(&sub("subrip", Some("External"), None), None, plain, false), Ok(None), "no subtitle asked");
        assert_eq!(
            subtitle_delivery(&sub("subrip", Some("External"), Some("/Videos/a/b/Subtitles/2/0/Stream.subrip?api_key=t")), Some(2), plain, false),
            Ok(Some(SubtitleDelivery::External { path: "/Videos/a/b/Subtitles/2/0/Stream.subrip".into(), codec: "srt".into() })),
        );
        assert!(subtitle_delivery(&sub("subrip", Some("External"), Some("/Videos/a/b/Subtitles/2/0/Stream.ttml")), Some(2), plain, false).is_err(), "a format the renderer cannot draw");
        assert_eq!(subtitle_delivery(&sub("PGSSUB", Some("Embed"), None), Some(2), plain, false), Ok(Some(SubtitleDelivery::Embedded)));
        assert_eq!(subtitle_delivery(&sub("DVBSUB", Some("Embed"), None), Some(2), plain, false), Ok(Some(SubtitleDelivery::Burned)), "the server burns DVB it says it embeds");
        assert_eq!(subtitle_delivery(&sub("PGSSUB", Some("Encode"), None), Some(2), plain, false), Ok(Some(SubtitleDelivery::Burned)));
        assert_eq!(subtitle_delivery(&sub("PGSSUB", Some("Drop"), None), Some(2), plain, false), Ok(None));
        assert!(subtitle_delivery(&sub("subrip", Some("Hls"), None), Some(2), plain, false).is_err());
        assert_eq!(subtitle_delivery(&sub("subrip", Some("External"), None), Some(2), plain, true), Ok(Some(SubtitleDelivery::Burned)), "an asked burn");
        // No method stated.
        assert_eq!(subtitle_delivery(&sub("PGSSUB", None, None), Some(2), "/x?SubtitleStreamIndex=2&SubtitleMethod=Embed", false), Ok(Some(SubtitleDelivery::Embedded)));
        assert_eq!(subtitle_delivery(&sub("PGSSUB", None, None), Some(2), "/x?SubtitleStreamIndex=2", false), Ok(Some(SubtitleDelivery::Burned)));
        assert_eq!(subtitle_delivery(&sub("subrip", None, None), Some(2), plain, false), Ok(None));
    }

    /// Jellyfin tries every `Embed` profile before any `External` one, and its second pass takes a
    /// format a text track merely converts to (`StreamBuilder.GetSubtitleProfile`,
    /// `MediaStream.SupportsSubtitleConversionTo`, true for any target but ASS/SSA). A conversion's
    /// bitmap `Embed` entries therefore captured an SRT track: the server ran `-codec:s:0 pgssub`,
    /// ffmpeg has no such encoder, and the stream answered 500 (live 12.0, 2026-10-18). Such an
    /// answer is asked again for a sidecar; a bitmap track stays embedded, and so does a text track
    /// embedded in its own format.
    #[test]
    fn a_text_track_answered_as_an_embedded_bitmap_is_asked_again() {
        let sub = |codec: &str, text: bool| MediaSourceInfo {
            media_streams: vec![MediaStream {
                kind: "Subtitle".into(),
                index: 4,
                codec: Some(codec.into()),
                is_text_subtitle_stream: text,
                delivery_method: Some("Embed".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        // The answer the server gave for SubRip #4 of a conversion ask.
        let pgs = "/videos/x/stream.mkv?VideoCodec=h264,hevc&AudioCodec=ac3,eac3,aac&SubtitleStreamIndex=4&SubtitleMethod=Embed&SubtitleCodec=pgssub";
        assert!(
            matches!(subtitle_delivery(&sub("subrip", true), Some(4), pgs, false), Err(Reask::Sidecar(_))),
            "SRT as PGS is an encode the server cannot run: ask for the sidecar, not a burn",
        );
        assert_eq!(subtitle_delivery(&sub("PGSSUB", false), Some(4), pgs, false), Ok(Some(SubtitleDelivery::Embedded)));
        let same = "/videos/x/stream.mkv?SubtitleStreamIndex=4&SubtitleMethod=Embed&SubtitleCodec=subrip";
        assert_eq!(subtitle_delivery(&sub("subrip", true), Some(4), same, false), Ok(Some(SubtitleDelivery::Embedded)));
    }

    /// The ask that follows a [`Reask::Sidecar`]: the same conversion with the text sidecars alone,
    /// which the server answers `External` because no `Embed` profile is left to capture the track.
    /// A burn still withdraws everything, and a direct ask keeps its embedded formats.
    #[test]
    fn a_sidecar_reask_offers_a_conversion_no_bitmap_embed() {
        let methods = |p: &Value| -> Vec<(String, String)> {
            p["SubtitleProfiles"].as_array().unwrap().iter()
                .map(|s| (s["Format"].as_str().unwrap().to_string(), s["Method"].as_str().unwrap().to_string()))
                .collect()
        };
        let conversion = |embed_bitmap| ProfileAsk { direct: false, forced: false, ceiling: None, burn: false, hls: None, embed_bitmap };
        let reask = methods(&profile(&conversion(false)));
        assert!(!reask.is_empty(), "an empty list would ask the server to burn");
        assert!(reask.iter().all(|(_, m)| m == "External"), "{reask:?}");
        let sidecars: Vec<&str> = reask.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(sidecars, crate::catalog::capabilities::SIDECAR_SUBTITLES);
        // The first ask is unchanged: a PGS track is still muxed rather than burned.
        assert!(methods(&profile(&conversion(true))).contains(&("pgssub".into(), "Embed".into())));
        let direct = methods(&profile(&ProfileAsk { direct: true, embed_bitmap: false, ..conversion(false) }));
        assert!(direct.contains(&("pgssub".into(), "Embed".into())), "a direct ask is not what the flag governs");
    }

    #[test]
    fn the_direct_path_names_the_source_and_its_container() {
        let src = MediaSourceInfo { id: "abc".into(), container: Some("mov,mp4,m4a,3gp,3g2,mj2".into()), ..Default::default() };
        let g = "0123456789abcdef0123456789abcdef";
        assert_eq!(direct_path(g, &src), format!("/Videos/{g}/stream.mp4?static=true&MediaSourceId=abc"));
        let mkv = MediaSourceInfo { id: "d".into(), container: Some("mkv".into()), ..Default::default() };
        assert_eq!(direct_path(g, &mkv), format!("/Videos/{g}/stream.mkv?static=true&MediaSourceId=d"));
    }
}

#[cfg(test)]
mod queue_tests;
