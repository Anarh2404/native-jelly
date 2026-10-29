//! The Jellyfin wire types this client reads — a deliberately small subset of the 12.0 OpenAPI
//! (`docs/jf-api/jellyfin-openapi-12.0.json`). Every type is `Deserialize + Default` with every
//! field optional-by-default, the contract `plex::models` keeps: a server that omits a field (10.10
//! omits several 12.0 adds, and every list endpoint omits what `Fields=` did not ask for) yields a
//! default, never a failed page.
//!
//! PascalCase on the wire; `rename_all` here rather than per field.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `GET /System/Info/Public` — anonymous, the probe.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct PublicSystemInfo {
    pub server_name: String,
    pub version: String,
    pub product_name: String,
    pub id: String,
    pub startup_wizard_completed: bool,
}

/// `{Items, TotalRecordCount, StartIndex}` — every list endpoint.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct QueryResult<T> {
    pub items: Vec<T>,
    pub total_record_count: i64,
    pub start_index: i64,
}

/// `POST /Users/AuthenticateByName` and `POST /Users/AuthenticateWithQuickConnect`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AuthenticationResult {
    pub user: UserDto,
    pub access_token: String,
    pub server_id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UserDto {
    pub id: String,
    pub name: String,
    pub server_id: String,
    pub primary_image_tag: Option<String>,
    pub has_password: bool,
    pub configuration: UserConfiguration,
    pub policy: UserPolicy,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UserConfiguration {
    pub audio_language_preference: Option<String>,
    pub subtitle_language_preference: Option<String>,
    /// `Default | Always | OnlyForced | None | Smart`.
    pub subtitle_mode: String,
    pub play_default_audio_track: bool,
    pub enable_next_episode_auto_play: bool,
    pub ordered_views: Vec<String>,
    pub my_media_excludes: Vec<String>,
    pub latest_items_excludes: Vec<String>,
    pub hide_played_in_latest: bool,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UserPolicy {
    pub is_administrator: bool,
    pub is_disabled: bool,
    pub enable_media_playback: bool,
    pub enable_video_playback_transcoding: bool,
    pub enable_playback_remuxing: bool,
    pub remote_client_bitrate_limit: i64,
}

/// `POST /QuickConnect/Initiate` and `GET /QuickConnect/Connect?secret=`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct QuickConnectResult {
    pub authenticated: bool,
    pub secret: String,
    pub code: String,
    pub device_id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct NameIdPair {
    pub name: String,
    pub id: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct NameValuePair {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UserItemData {
    pub playback_position_ticks: i64,
    pub play_count: i64,
    pub is_favorite: bool,
    pub played: bool,
    pub last_played_date: Option<String>,
    pub unplayed_item_count: Option<i64>,
    pub played_percentage: Option<f64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct BaseItemPerson {
    pub name: String,
    pub id: String,
    pub role: Option<String>,
    /// `Actor | Director | Writer | Producer | GuestStar | …`.
    #[serde(rename = "Type")]
    pub kind: String,
    pub primary_image_tag: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ChapterInfo {
    pub start_position_ticks: i64,
    pub name: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct MediaStream {
    /// `Video | Audio | Subtitle | EmbeddedImage | Data | Lyric`.
    #[serde(rename = "Type")]
    pub kind: String,
    pub index: i64,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub display_title: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
    pub is_hearing_impaired: bool,
    pub is_external: bool,
    pub is_text_subtitle_stream: bool,
    pub supports_external_stream: bool,
    /// How a PlaybackInfo answer delivers this subtitle to the playback it negotiated:
    /// `Encode | Embed | External | Hls | Drop` (`SubtitleDeliveryMethod`).
    pub delivery_method: Option<String>,
    /// With `External`: where the file is, a server path with an `api_key`, or a remote
    /// source's own URL.
    pub delivery_url: Option<String>,
    pub channels: Option<i64>,
    pub channel_layout: Option<String>,
    pub sample_rate: Option<i64>,
    pub bit_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub average_frame_rate: Option<f64>,
    pub real_frame_rate: Option<f64>,
    pub profile: Option<String>,
    pub level: Option<f64>,
    /// `SDR | HDR | Unknown`.
    pub video_range: Option<String>,
    /// `SDR | HDR10 | HDR10Plus | HLG | DOVI | DOVIWithHDR10 | …`.
    pub video_range_type: Option<String>,
    /// The Dolby Vision configuration record, as the server's probe read it. Absent on a stream
    /// without Dolby Vision, and on a server too old to report it.
    pub dv_version_major: Option<i64>,
    pub dv_version_minor: Option<i64>,
    pub dv_profile: Option<i64>,
    pub dv_level: Option<i64>,
    pub rpu_present_flag: Option<i64>,
    pub el_present_flag: Option<i64>,
    pub bl_present_flag: Option<i64>,
    /// 0 none (Profile 5) / 1 HDR10 / 2 SDR / 4 HLG / 6 the Blu-ray dual layer.
    pub dv_bl_signal_compatibility_id: Option<i64>,
    pub color_transfer: Option<String>,
    pub color_primaries: Option<String>,
    pub color_space: Option<String>,
    pub pixel_format: Option<String>,
    pub audio_spatial_format: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct MediaSourceInfo {
    pub id: String,
    pub name: Option<String>,
    pub path: Option<String>,
    pub protocol: String,
    pub container: Option<String>,
    pub size: Option<i64>,
    pub bitrate: Option<i64>,
    pub run_time_ticks: Option<i64>,
    pub is_remote: bool,
    pub supports_direct_play: bool,
    pub supports_direct_stream: bool,
    pub supports_transcoding: bool,
    pub transcoding_url: Option<String>,
    /// `http | hls`.
    pub transcoding_sub_protocol: Option<String>,
    pub transcoding_container: Option<String>,
    pub default_audio_stream_index: Option<i64>,
    pub default_subtitle_stream_index: Option<i64>,
    pub media_streams: Vec<MediaStream>,
    pub has_segments: bool,
    /// Headers fetching a remote source's `Path` needs, name to value. Kept as JSON: only the
    /// names are read, and an odd value must not cost the whole answer its parse.
    pub required_http_headers: Option<serde_json::Value>,
}

/// One item of any type. `Type` is the discriminator: `Movie | Series | Season | Episode |
/// BoxSet | CollectionFolder | Folder | Person | Video | Trailer | MusicVideo | …`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct BaseItemDto {
    pub id: String,
    pub name: String,
    pub original_title: Option<String>,
    pub sort_name: Option<String>,
    pub server_id: String,
    #[serde(rename = "Type")]
    pub kind: String,
    /// `movies | tvshows | boxsets | music | homevideos | …` on a library view.
    pub collection_type: Option<String>,
    pub is_folder: bool,
    pub parent_id: Option<String>,
    pub overview: Option<String>,
    pub taglines: Vec<String>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub studios: Vec<NameIdPair>,
    pub people: Vec<BaseItemPerson>,
    pub production_year: Option<i64>,
    pub premiere_date: Option<String>,
    pub date_created: Option<String>,
    pub end_date: Option<String>,
    pub official_rating: Option<String>,
    pub community_rating: Option<f64>,
    pub critic_rating: Option<f64>,
    pub run_time_ticks: Option<i64>,
    pub container: Option<String>,
    pub index_number: Option<i64>,
    pub parent_index_number: Option<i64>,
    pub series_id: Option<String>,
    pub series_name: Option<String>,
    pub season_id: Option<String>,
    pub season_name: Option<String>,
    pub child_count: Option<i64>,
    pub recursive_item_count: Option<i64>,
    pub status: Option<String>,
    pub original_language: Option<String>,
    pub provider_ids: HashMap<String, String>,
    pub image_tags: HashMap<String, String>,
    pub backdrop_image_tags: Vec<String>,
    /// `{"Primary": {"<tag>": "<blurhash>"}, …}`.
    pub image_blur_hashes: HashMap<String, HashMap<String, String>>,
    pub primary_image_aspect_ratio: Option<f64>,
    pub series_primary_image_tag: Option<String>,
    pub parent_backdrop_item_id: Option<String>,
    pub parent_backdrop_image_tags: Vec<String>,
    pub parent_logo_item_id: Option<String>,
    pub parent_logo_image_tag: Option<String>,
    pub parent_thumb_item_id: Option<String>,
    pub parent_thumb_image_tag: Option<String>,
    pub user_data: Option<UserItemData>,
    pub media_sources: Vec<MediaSourceInfo>,
    pub media_streams: Vec<MediaStream>,
    pub chapters: Vec<ChapterInfo>,
    pub local_trailer_count: Option<i64>,
    pub special_feature_count: Option<i64>,
    pub extra_type: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

/// `GET /Items/Filters2`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct QueryFilters {
    pub genres: Vec<NameIdPair>,
    pub tags: Vec<String>,
    pub audio_languages: Vec<NameValuePair>,
    pub subtitle_languages: Vec<NameValuePair>,
}

/// `GET /MediaSegments/{itemId}`. Types on 12.0: `Unknown | Commercial | Preview | Recap | Outro
/// | Intro`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct MediaSegmentDto {
    pub id: String,
    pub item_id: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub start_ticks: i64,
    pub end_ticks: i64,
}

/// `POST /Items/{id}/PlaybackInfo` answer.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct PlaybackInfoResponse {
    pub media_sources: Vec<MediaSourceInfo>,
    pub play_session_id: Option<String>,
    /// `NotAllowed | NoCompatibleStream | RateLimitExceeded` when the server refuses.
    pub error_code: Option<String>,
}

/// `POST /Sessions/Playing{,/Progress,/Stopped}` body — `PlaybackStartInfo` on the first report and
/// `PlaybackProgressInfo` after. Serialized, never read.
///
/// Deliberately NOT here: `TranscodingInfo`. Neither DTO carries it — the server builds the
/// dashboard's transcoding read-out from the ffmpeg job the client's stream request started, and a
/// client that tried to report one would be stating something it is not the authority on.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackReport {
    pub item_id: String,
    pub media_source_id: String,
    pub play_session_id: String,
    pub position_ticks: i64,
    pub is_paused: bool,
    pub can_seek: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i64>,
    /// `DirectPlay | DirectStream | Transcode`.
    pub play_method: String,
    /// UTC ticks at which this playback began. The server keeps it to attribute the session's
    /// duration; re-deriving it per report would make every report claim a different start.
    ///
    /// Optional, and the three fields below it are too, for one reason: every one of them is an
    /// enum or an instant the server parses, so an unset field has to be ABSENT rather than sent
    /// as `0` or `""` — a default-constructed report must not date the session to the year 1 or
    /// name a repeat mode that does not exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_start_time_ticks: Option<i64>,
    /// `Default | OneTrack | Shuffle`. This client plays a queue in order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_order: Option<&'static str>,
    /// `RepeatNone | RepeatAll | RepeatOne`. No repeat control is offered yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_mode: Option<&'static str>,
    /// The queue position this playback occupies, when the caller knows it. Omitted rather than
    /// sent empty, because an empty string is a position the server would try to resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_item_id: Option<String>,
    /// The set's own mute and 0–100 volume, the only ones this player has. Both absent until the
    /// set has answered, so the dashboard is never told a level nobody read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_muted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_level: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sparse_list_item_parses_with_defaults() {
        let q: QueryResult<BaseItemDto> = serde_json::from_str(
            r#"{"Items":[{"Id":"36edf81507b5c8eda566778b2a316c29","Name":"A","Type":"Movie",
                "RunTimeTicks":81772160000,"ImageTags":{"Primary":"9fd8"},
                "ImageBlurHashes":{"Primary":{"9fd8":"dIHVJ3Me7$.TITt7bwM{pxtR={xuEkRP?GxuOFbIn#oJ"}},
                "UserData":{"PlaybackPositionTicks":0,"Played":false}}],
                "TotalRecordCount":42,"StartIndex":0}"#,
        )
        .unwrap();
        assert_eq!(q.total_record_count, 42);
        let it = &q.items[0];
        assert_eq!(it.kind, "Movie");
        assert_eq!(it.image_tags.get("Primary").map(String::as_str), Some("9fd8"));
        assert!(it.media_sources.is_empty() && it.people.is_empty());
    }

    #[test]
    fn a_report_serializes_pascal_case_and_omits_unset_tracks() {
        let r = PlaybackReport {
            item_id: "x".into(),
            position_ticks: 10,
            play_method: "DirectPlay".into(),
            ..Default::default()
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"ItemId\":\"x\"") && s.contains("\"PositionTicks\":10"), "{s}");
        assert!(!s.contains("AudioStreamIndex") && !s.contains("Failed"), "{s}");
        // An unset enum or instant is absent, never `""`/`0`: the server parses these, and a
        // year-1 start or a nameless repeat mode is a body it would reject.
        for absent in ["PlaybackStartTimeTicks", "PlaybackOrder", "RepeatMode", "PlaylistItemId", "IsMuted", "VolumeLevel"] {
            assert!(!s.contains(absent), "{absent} should be omitted when unset: {s}");
        }
    }

    #[test]
    fn a_populated_report_names_the_queue_position_and_the_dotnet_start_instant() {
        let r = PlaybackReport {
            item_id: "x".into(),
            play_method: "DirectStream".into(),
            playback_start_time_ticks: Some(637_134_336_000_000_000),
            playback_order: Some("Default"),
            repeat_mode: Some("RepeatNone"),
            playlist_item_id: Some("3".into()),
            ..Default::default()
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"PlayMethod\":\"DirectStream\""), "{s}");
        assert!(s.contains("\"PlaybackStartTimeTicks\":637134336000000000"), "{s}");
        assert!(s.contains("\"PlaybackOrder\":\"Default\""), "{s}");
        assert!(s.contains("\"RepeatMode\":\"RepeatNone\""), "{s}");
        assert!(s.contains("\"PlaylistItemId\":\"3\""), "{s}");
    }
}
