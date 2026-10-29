//! The playback facade over Jellyfin (impl Client) and the device gates the route layer shares.
//!
//! The protocol lives in `jf::playback`: one `POST /Items/{id}/PlaybackInfo` per playback, answered
//! as a typed [`Negotiation`] — the `PlayMethod` the server agreed to (DirectPlay for the file's
//! own bytes, Transcode for anything from the `TranscodingUrl`), the URL to open, and per lane the
//! source codec beside the codec that will arrive — or a typed [`Refusal`].
//! A server without a Jellyfin seat has no playback here: every call answers "unreachable".
use super::client::{Client, QueryBuilder, StreamUrl};
use super::params::TranscodeSpec;
use super::probe::Location;
pub use crate::jf::playback::{
    Ask as PlaybackAsk, Lane, LanguagePrefs, Negotiated, Negotiation, PlayMethod, Refusal, SubtitleDelivery,
};
// `DP_AUDIO_CODECS` — the AUDIO codec set the buffer-feed pipeline decodes — is defined in
// `devcaps` (the platform layer intersects it with the device's own codec table) and re-exported
// here for the route layer. The live set is `devcaps::Caps::audio`; normal routing uses
// [`is_dp_audio_track`] for membership and channel bounds, shared with the device profile.
pub use nj_platform::devcaps::DP_AUDIO_CODECS;
pub fn is_dp_audio(codec: &str) -> bool {
    nj_platform::devcaps::caps().audio_has(codec)
}

/// Codec and channel check for an actual selected track. Codec-only membership is insufficient
/// on devices whose DTS decoder stops at 5.1 or whose AAC decoder stops at stereo.
pub fn is_dp_audio_track(codec: &str, channels: i64) -> bool {
    nj_platform::devcaps::caps().audio_supports(codec, channels)
}

/// Subtitle codecs the client renders itself (`ff.rs` / the track menu): an embedded track whose
/// codec is here rides a direct play and is drawn locally, so selecting it never asks the server
/// to burn it into a conversion. Defined with the rest of the client's capabilities, beside the
/// subtitle formats the device profile offers, so the two cannot drift.
pub use super::capabilities::{is_rendered_subtitle as is_dp_subtitle, RENDERED_SUBTITLES as DP_SUBTITLE_CODECS};

// ---- the relay policy: what the LINK to a server allows a plan to ask for -------------------

/// What the connection tier a server is reached over lets a playback plan ask for. Both fields
/// are true on every tier but [`Location::Relay`] — see [`link_policy`], which is where the
/// reasoning lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkPolicy {
    /// May the client stream the source file itself? False forces the transcode branch.
    pub direct_play: bool,
    /// May that transcode be a container-only REMUX — codecs copied, no cap? False forces the
    /// RE-ENCODE flavor, the only one whose query carries a bound the server can come in under.
    pub remux: bool,
}

impl LinkPolicy {
    /// Everything allowed: every tier but relay, and every link nothing has told us about.
    ///
    /// **Unknown must be permissive, and that is a decision rather than a default.** Nothing in
    /// this app dials from a [`Location`] yet — `probe::candidates` ranks addresses and the racing
    /// lands with the transport work — so `Client::link()` is `None` on every play today. Reading
    /// `None` as "assume relay" would downgrade every direct play on earth to a server-side
    /// encode, to prevent a stall on a connection this codebase has never once made.
    pub const UNRESTRICTED: LinkPolicy = LinkPolicy {
        direct_play: true,
        remux: true,
    };
}

/// **The relay policy**, and the only place it is spelled out. `None` = nobody has said how this
/// server is reached (see [`LinkPolicy::UNRESTRICTED`]).
///
/// Plex's relay is a tunnel the server holds open to a Plex relay host: a second https connection,
/// conventionally port 8443, **capped at 2 Mbit/s**, with the server transcoding down to fit
/// whatever the client asks for. [`probe`](super::probe) ranks it last, so it only ever wins when
/// there is nothing else — a share behind CGNAT, an owner with no port forward. When it does win,
/// the two fast paths this client is built around become the wrong answer, and both have to go:
///
/// * **No direct play.** Direct play streams the file's own bytes, and a library's own bytes are
///   tens of Mbit/s (the share measured on 2026-08-11: 31 Mbit/s — `docs/shared-servers.md` §2)
///   down a 2 Mbit/s pipe. The server cannot rescue it, because nothing is being transcoded and
///   so there is nothing for it to fit; the cap arrives as a stall two minutes into the film,
///   with the AU queue draining and no error on any surface. Forcing the transcode branch turns
///   that into a `/decision` the server answers with something that fits.
/// * **No container REMUX either** — the half that is easy to miss, because a remux *feels* like
///   a concession already. It is not: a remux copies the codecs and deliberately sends **no
///   cap** (`transcode_query` below omits `videoResolution`/`maxVideoBitrate` on that branch,
///   because a cap is exactly what would force the re-encode it is trying to avoid). So it ships
///   the same bytes at the same rate one layer down, and stalls identically. Denying it leaves
///   the re-encode, which is the only flavor whose whole point is that the server picks the rate.
///
/// **Not a parameter, deliberately — THIS policy sends no number at all.** A cap is meaningless on
/// direct play (no encoder is running to obey it) and on the re-encode branch the server already
/// receives an upper bound it is free to come in far under. The relay's real ceiling is known to
/// the server, which is on the other end of the tunnel and applies it; this policy's whole job is
/// to stop asking for the two flavors that leave it no way to.
///
/// This paragraph used to end "[`TranscodeSpec`] has no bitrate field and this does not add one",
/// and the spec grew [`Ceiling`] on 2026-08-23 — so read it as being about the RELAY, which is
/// what it always was. A **user-chosen** ceiling (`route::Quality`) is a different input through
/// the identical mechanism: `route::quality_policy` returns this same [`LinkPolicy`],
/// `route::flavors_allowed` composes the two so the stricter wins per flavor, and only then does a
/// number reach `transcode_query`. Nothing about the relay tier changed — it still denies both
/// flavors unconditionally and still names no rate.
///
/// **No relay connection has been observed on a device by this codebase.** Before discovery racing,
/// the old chooser discarded every relay. Relay is now a second-phase candidate, but the 2 Mbit/s
/// figure and port-8443 convention remain Plex documentation rather than our measurement, and this
/// policy is **unverified against a real relay** — `docs/shared-servers.md` §7 question 5. What is
/// asserted is the shape, not the number: whatever the ceiling turns out to be, only the server can
/// apply it, and only if an encoder runs.
pub fn link_policy(link: Option<Location>) -> LinkPolicy {
    match link {
        Some(Location::Relay) => LinkPolicy {
            direct_play: false,
            remux: false,
        },
        // Exhaustive on purpose: a new tier must come here and say what it allows, rather than
        // inheriting "unrestricted" from a wildcard because nobody thought about it.
        Some(Location::Local) | Some(Location::Remote) | None => LinkPolicy::UNRESTRICTED,
    }
}

impl Client {
    /// Negotiate one playback with the server (see `jf::playback::Jf::negotiate`).
    pub fn negotiate(&self, ask: &PlaybackAsk) -> Negotiation {
        match self.jf() {
            Some(j) => j.negotiate(ask),
            None => Negotiation::Unreachable,
        }
    }

    /// Re-negotiate the playing item as a conversion built from the route's encode contract — the
    /// seek, track-switch and quality-change path.
    pub fn transcode(&self, spec: &TranscodeSpec) -> Negotiation {
        match self.jf() {
            Some(j) => j.transcode(spec),
            None => Negotiation::Unreachable,
        }
    }

    /// End the server's encoder for `session` (`DELETE /Videos/ActiveEncodings` for its
    /// PlaySessionId) without reporting the playback stopped. Returns whether the server took it; a
    /// direct play has no encoder and answers true without a request.
    pub fn transcode_stop(&self, session: &str) -> bool {
        if session.is_empty() {
            return false;
        }
        self.jf().is_some_and(|j| j.transcode_stop(session))
    }

    /// The throughput-test payload Auto quality times (`/Playback/BitrateTest`).
    pub fn bitrate_test_url(&self, bytes: usize) -> Option<StreamUrl> {
        self.jf().map(|j| j.bitrate_test_url(bytes))
    }

    /// The signed-in user's audio/subtitle language preferences.
    pub fn language_prefs(&self) -> Option<LanguagePrefs> {
        self.jf()?.language_prefs()
    }

    /// GET /photo/:/transcode?width&height&minSize=1&url=…[&format=png]&X-Plex-Token — returns
    /// the built PATH (the sole path-returning method): posters uses it as the LRU key AND the
    /// http_get path. `src_path` is the raw thumb/art path; encoding is centralized in enc().
    pub fn image_transcode_path(&self, src_path: &str, w: i64, h: i64, png: bool) -> String {
        if self.jf().is_some() {
            return crate::jf::images::jf_image_path(src_path, w, h, png).unwrap_or_else(|| src_path.to_string());
        }
        let mut q = QueryBuilder::new("/photo/:/transcode")
            .int("width", w)
            .int("height", h)
            .int("minSize", 1)
            .str("url", src_path);
        if png {
            q = q.str("format", "png");
        }
        self.with_token(&q.build())
    }
}

#[cfg(test)]
mod tests {
    use super::{link_policy, LinkPolicy, Location};

    /// Relay denies BOTH flavors that put the file's own bytes on the wire. Direct play is the
    /// obvious one; the remux is the one a "force a transcode" instinct leaves behind, and it is
    /// no better — it copies the codecs and its query carries no cap by design, so it is the same
    /// 31 Mbit/s down the same 2 Mbit/s tunnel. What survives is the re-encode, where the server
    /// is the one choosing the rate.
    #[test]
    fn a_relay_link_forbids_both_flavors_that_ship_the_file_at_its_own_rate() {
        let p = link_policy(Some(Location::Relay));
        assert!(!p.direct_play, "a relay cannot carry the source file");
        assert!(
            !p.remux,
            "a remux is the same bytes at the same rate, one layer down"
        );
    }

    /// Every other tier — and a link nobody has described — must change nothing at all. This is
    /// the assertion that keeps the policy free for the single-server LAN install: `Client::link`
    /// is `None` on every play today, so a wrong answer here would be a wrong answer for
    /// everybody, on a code path they all take.
    #[test]
    fn every_other_tier_and_an_unknown_link_leave_playback_alone() {
        for l in [Some(Location::Local), Some(Location::Remote), None] {
            assert_eq!(
                link_policy(l),
                LinkPolicy::UNRESTRICTED,
                "link {l:?} must restrict nothing"
            );
        }
    }

    /// The policy's input is the tier of the connection that WON, so grade it from the other end:
    /// a server whose only advertised address is a relay (a share behind CGNAT — the case relay
    /// exists for) can be reached one way, and that way forces the transcode branch. This pins
    /// the join between `probe`'s ranking and this policy: they must speak the same `Location`,
    /// and the last-ranked tier must be the restricted one.
    #[test]
    fn a_server_reachable_only_by_relay_forces_the_transcode_branch() {
        let res: super::super::account::Resource = serde_json::from_str(
            r#"{"name":"cgnat-share","clientIdentifier":"cccc3333","provides":"server","owned":false,
                "connections":[{"protocol":"https","address":"plex-relay.example.net","port":8443,
                 "uri":"https://plex-relay.example.net:8443","local":false,"relay":true}]}"#,
        )
        .expect("fixture parses");

        let cs = super::super::probe::candidates(&res, crate::catalog::CredentialPolicy::HttpsOnly);
        assert_eq!(cs.len(), 1, "a relay gets no plain-http twin: {cs:#?}");
        assert_eq!(cs[0].location, Location::Relay);

        let p = link_policy(Some(cs[0].location));
        assert_eq!(
            p,
            LinkPolicy {
                direct_play: false,
                remux: false
            }
        );
    }

    #[test]
    fn the_client_rendered_subtitle_set_keeps_its_aliases() {
        assert!(super::is_dp_subtitle("MOV_TEXT"));
        assert!(super::is_dp_subtitle("dvd_subtitle"));
        assert!(super::is_dp_subtitle("hdmv_pgs_subtitle"));
        assert!(!super::is_dp_subtitle("vplayer"), "obscure aliases stay off the advertised set");
    }
}
