# eugene fork

Fork of [sostk/native-jelly](https://github.com/sostk/native-jelly), published as
[Anarh2404/native-jelly](https://github.com/Anarh2404/native-jelly) (base: tag `v0.9.0`,
branch `fork`; remote `origin`, upstream is remote `upstream`) for one rooted LG webOS 22 TV that runs the `nlauncher` home screen
(`~/repos/limbo/lg-launcher`).

Delta vs upstream (keep it that small):

- `rust-modules/platform/src/webos.rs`: `HOME_APP_ID` is `tld.my.nlauncher`, not
  `com.webos.app.home` — BACK at the app's root hands the screen to the home launcher directly;
  the existing graded launch falls back to SDL minimize when the launcher is absent.
- `pkg/appinfo.json` + `pkg/resources/*/appinfo.json`: tile title is `Jellyfin`; the
  top-level `appDescription` carries a fork marker.
- `route/decision.rs` `auto_bitrate_kbps`: Auto raises its measured ceiling to jellyfin-web's
  LAN floor (140 Mbit/s) when `GET /System/Endpoint` says `IsInNetwork` — a server reached by
  name was tiered Remote, one 500 KB sample capped the LAN at ~77 Mbit/s and an 84 Mbit/s 4K
  remux became a re-encode.
- `jf/playback.rs`: a conversion's video is predicted copied only when the stream's bitrate is
  known and within the URL's `VideoBitrate` (server `CanStreamCopyVideo`); otherwise the Load
  declared HEVC while the server sent H.264 and the player refused the stream.

Build: `make setup-env` once (webosbrew NDK; also needs cmake, pkg-config, rust nightly
with `rust-src`; host tests also need `brew install sdl2_ttf`), then
`make FLAVOR=stable RELEASE=1 ipk`. Deploy: `make FLAVOR=stable RELEASE=1 deploy` then
`verify-deploy` (both take the TV lock); the first install unpacked the ipk's `data.tar.gz` over
`/media/developer/apps/usr/palm/applications/com.sostk.nativejelly` (root ssh `lg-tv`) and
restarted `sam`. Rollback points next to the app dir: `com.sostk.nativejelly.bak080` (upstream
0.8.0), `com.sostk.nativejelly.bak090fork` (this fork before the two playback fixes).
