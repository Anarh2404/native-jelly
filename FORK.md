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
- `route/decision.rs` `auto_bitrate_kbps` + `curlio.rs`: Auto times its `/Playback/BitrateTest`
  probe by duration — a 64 MiB payload read for 600 ms, rate taken after the first 200 ms
  (`ThroughputSample::steady_kbps`) — instead of one 500 KB body, which measured TCP ramp-up (78
  Mbit/s on a link carrying 253) and capped an 84 Mbit/s 4K remux into a re-encode. No LAN floor:
  a 100 Mbit/s cable is measured as one.
- `jf/playback.rs`: a conversion's video is predicted copied only when the stream's bitrate is
  known and within the URL's `VideoBitrate` (server `CanStreamCopyVideo`); otherwise the Load
  declared HEVC while the server sent H.264 and the player refused the stream.
- `app/boot.rs` + `app/mod.rs` + `app/run.rs`: retain the boot GL context, stop workers, drop GPU
  owners with that context current, then delete the context before `SDL_Quit`. On webOS 22 a live
  EGL context/surface during SDL window teardown reproducibly faults inside Mali/Wayland.

Build: `make setup-env` once (webosbrew NDK; also needs cmake, pkg-config, rust nightly
with `rust-src`; host tests also need `brew install sdl2_ttf`), then
`make FLAVOR=stable RELEASE=1 ipk`. Deploy: `make FLAVOR=stable RELEASE=1 deploy` then
`verify-deploy` (both take the TV lock); the first install unpacked the ipk's `data.tar.gz` over
`/media/developer/apps/usr/palm/applications/com.sostk.nativejelly` (root ssh `lg-tv`) and
restarted `sam`. Rollback points next to the app dir: `com.sostk.nativejelly.bak080` (upstream
0.8.0), `com.sostk.nativejelly.bak090fork` (this fork before the playback fixes).

Stable TV update (2026-11-01, CAMP-BG-598): GL teardown fix in `54c51457`, installed binary MD5
`ab36145f5b9b5876057a6cfa4007955a`, `verify-deploy` 17/17. Three SAM close cycles exited with
status 0 and no new SIGSEGV. Previous stable executable is `nativejelly.before-glfix` inside the
app directory (MD5 `be07f0a4dc9225fbe5ab81f2b63bee18`). The separate nightly app and its storage
service were uninstalled after stable acceptance; the local nightly test branch remains available.
