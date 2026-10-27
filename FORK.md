# eugene fork

Local fork of [sostk/native-jelly](https://github.com/sostk/native-jelly) (base: tag `v0.9.0`,
branch `fork`) for one rooted LG webOS 22 TV that runs the `nlauncher` home screen
(`~/repos/limbo/lg-launcher`).

Delta vs upstream (keep it that small):

- `rust-modules/platform/src/webos.rs`: `HOME_APP_ID` is `tld.my.nlauncher`, not
  `com.webos.app.home` — BACK at the app's root hands the screen to the home launcher directly;
  the existing graded launch falls back to SDL minimize when the launcher is absent.
- `pkg/appinfo.json` + `pkg/resources/*/appinfo.json`: tile title is `Jellyfin`; the
  top-level `appDescription` carries a fork marker.

Build: `make setup-env` once (webosbrew NDK; also needs cmake, pkg-config, rust nightly
with `rust-src`), then `make FLAVOR=stable RELEASE=1 ipk`. Deploy: unpack the ipk's
`data.tar.gz` over `/media/developer/apps/usr/palm/applications/com.sostk.nativejelly`
(root ssh `lg-tv`) and restart `sam`. Rollback of the 2026-10-27 install: the
`com.sostk.nativejelly.bak080` directory next to it (upstream 0.8.0).
