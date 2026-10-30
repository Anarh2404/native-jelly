# Native Jelly Privacy Policy

Applies to Native Jelly 0.7.0. Last updated 4 October 2026.

Native Jelly is based on PlxNative by Gleb Linnik. This policy covers Native Jelly only.

## Who is responsible for Native Jelly data

sostk is responsible only for data Native Jelly stores locally and for optional reports you choose
to share. Contact: <https://github.com/sostk/native-jelly/issues> (issues there are public).

## Your Jellyfin server

Native Jelly is an independent client for Jellyfin. Apart from the optional reports described
below, it has no account or online service of its own: signing in, browsing and playing media,
watch progress and every other server feature go directly to the Jellyfin server whose address you
enter, and are handled by that server and whoever operates it. Native Jelly’s developer does not
receive any of it.

When you enter an address without `http://` or `https://`, Native Jelly tries plain http first for
an address on your home network and https first for a domain name, and falls back to the other if
the first does not answer. Over http, your password, access token and everything Native Jelly
exchanges with the server travel over the network without encryption. Enter the full `https://`
address for a server you reach over the internet.

Native Jelly sends nothing that signs you in over plain http until you allow it for that server.
It asks once, naming the server and saying whether its address is on your home network or the
internet. The answer covers that server at that address only. Each time Native Jelly connects —
when it starts and when it returns to the screen — it first checks, without sending anything that
signs you in, that the same server answers there. The answers are stored on this television without
any user name or token. They are kept when you sign out. Settings › Unencrypted connections lists
them and turns each one off, and Delete all local data removes them.

## Data stored on this television

Native Jelly has a developer-only input/frame recorder used to reproduce and test bugs. It is not
part of the app you installed: a release build has the developer-trigger inputs, including the
recorder, compiled out — there is no code path in a release binary that can open, write or read a
recording, on this television or off it. The unconditional `nativejelly-*.log` files are create-only
diagnostics, not trigger inputs. The recorder exists only in development builds used to build and
test Native Jelly itself, is started only by explicitly arming it on that build, and its recordings
never leave the device it was made on.

Native Jelly stores the address, name, identifier and version of the Jellyfin server you signed in
to, and for each person signed in on this television their user name and user identifier there,
the access token that server issued to this television and the device identifier it was issued
under, in one file created with mode 0600. It never stores your password. Up to twelve people can
be signed in at once; the Who's watching? screen lists them by name. It also keeps downloaded
artwork (posters, backdrops and cast images, cached as bounded files for reuse across restarts);
for each person their Home library choices, recent searches, last library, sort choices and
subtitle timing adjustments, kept apart from everyone else's; your playback quality, Direct Play
and app language preferences, which the whole television shares; and local technical
logs: a small rotating event log and a bounded storage status snapshot. It also stores your answers
to the two optional-reporting questions, the random Crash report ID if you turned crash reports on,
the random Analytics ID if you turned product analytics on, any report waiting to be sent, and a
marker recording how much of the crash log has already been read.
It keeps no bookmark of its own for where you stopped watching: playback position is held by your
Jellyfin server. The Settings screen can sign out and remove Native Jelly data from this television.

Those lifetimes differ. Signing out removes that one person's Jellyfin sign-in from this
television (their user name and identifier there, and the access token) and asks the server to
revoke the token; everyone else signed in stays. When the last person signs out, Native Jelly keeps
the address, name and identifier of the server signed in to last, with no user or token, so the
sign-in screen can offer it under Recent; signing in to another server replaces it.
**Signing out does not currently remove anything else Native Jelly keeps**, including that
server, the signed-out person's library choices, searches and subtitle adjustments, your
optional-reporting answers, both identifiers and any queued report: turn a category off, or use
Delete all local data, to remove those. Delete all local data signs everyone out, asks the server
to revoke every token, and removes the server under Recent too. A queued report is deleted once sent, or at the moment you
switch its category off. The event log rotates continuously and the storage snapshot is replaced
when its bounded status changes. **webOS gives an application no way to run code as it is
removed**, so the sign-in and the reporting answers can survive an uninstall — use Delete all local
data before uninstalling if you want nothing of Native Jelly left on the television.

## Optional crash reports

Crash reporting is off until you choose to share it. If enabled, Native Jelly sends technical crash
details to Sentry in Germany. A report may include the signal, code addresses, thread information,
internal component labels, app and webOS versions, television model and hardware compatibility
details needed to reproduce and symbolicate the failure.

A native crash report may also contain the last 16 window diagnostic steps: entering or leaving
background, querying the native window, and beginning or completing the first frame after return.
These steps include technical timestamps, fixed labels, whether playback was active, whether display/surface handles were
available, and SDL's version numbers. They contain no window addresses or viewing content.
They are collected only while crash reporting is enabled and accompany a crash rather than being
sent as usage events. Disabling crash reports removes the local trace.

Every crash and error report carries a **Crash report ID**: a random identifier created on this
television when you turn crash reports on, sent as the report's `user.id`. It exists so that
repeated crashes under one Crash report ID are counted once rather than once each — Sentry's
"users affected" figure is the number of distinct Crash report IDs an issue has reached — which is
what tells a problem that hit many people apart from one television that hit it many times. It is not derived from your
account, your television or anything about you, and it is never sent with product analytics.
Settings shows it while crash reports are on. Turning crash reports off, or Delete all local data,
deletes the local identifier; enabling them later creates a new one. Reports already sent keep the old
identifier, so copy it down first if you intend to ask for their deletion.

The same independent choice also covers a handled playback-error report when playback reaches its
explicit terminal error screen. That report contains a fixed failure kind, delivery and quality
classes, coarse raster, rate, HTTP and buffer classes, whether a first picture appeared, and at
most 32 typed playback transitions with bucketed elapsed times. It contains no title, ratingKey,
URL, path, playhead, duration, exact bitrate, server identity, address, token, account or profile,
and is not joined to the product analytics identifier or `playback_id`. It carries the same Crash
report ID as a crash report, and the same television model, SoC, hardware revision, webOS release
and the `rtkmem`/`install` sandbox facts a crash report carries. Buffering, seeking, holding
a low quality, or rejecting an adaptive-bitrate candidate does not by itself send a report.
The closed diagnostic vocabulary includes terminal kinds such as `playback_interrupted` and
`original_rollback`; HLS direction `refresh`; delivery reason `original_open_rollback`; and
Original-check outcomes `started`, `succeeded`, `no_body`, `deadline`, `transport`,
`inconclusive`, `server_state` and `refused`.

When the failure is the media server **refusing to play or convert the item** (`decision_refused`),
that report also carries four closed fields about the refusal — and never the server's own
explanation, which is free text that can name files, paths and servers and stays on the television:
the two numeric decision codes the server answered with, as `absent` / `2000` / `2003` / `4007` /
`other_1xxx` / `other_2xxx` / `other_3xxx` / `other_4xxx` / `other` (a number outside that list is
reported by its documented class, never as itself); the delivery the app had asked for,
`original_remux` / `hls` / `progressive_transcode` (the report's own `delivery` stays `unknown`
because the refused plan never installed a route); and the source file's video codec
(`unknown` / `h264` / `hevc` / `av1` / `vp9` / `mpeg2` / `other`) and audio codec (`unknown` /
`aac` / `ac3` / `eac3` / `truehd` / `dts` / `flac` / `mp3` / `opus` / `other`).

The same choice also covers a **sign-in problem report** when signing in fails. It contains the
`kind` of sign-in step that failed — or, when the failure was inside the app rather than on the
network (the app's own sign-in work refused, stopped or left unfinished), which of those fixed
internal kinds it was — the connection's `link` class, an `http_status` or network error number
(`curl_rc`) when there is one, how long the connection went `unanswered` and how long sign-in had
been `failing_for` (both bucketed), how many codes were shown (`code_generation`, capped), and, when
a sign-in could not be saved, a `persistence` failure class, the `keymanager_stage` a key-service
step stopped at and the key service's own numeric `service_error_code`. A storage-helper failure
also carries fixed startup, connection, activation or backend stages and wire/DB8 error codes
(`helper`), and up to eight failed storage-candidate errno numbers (`candidate_errnos`). It carries
no candidate paths, file owners or helper generation identifiers.
The fields in the rest of this paragraph that describe plex.tv discovery are produced only by the
Plex sign-in inherited from PlxNative, which a Jellyfin sign-in never uses.
When a server was found but answered only over an unencrypted connection, it also carries a fixed
outcome class (`absent`, `timeout`, `dns`, `tls`, `refused` and the like) for each secure route to
that server — `https_lan`, `https_public`, `https_custom` and `https_relay` — and, about the
unencrypted answer, only fixed facts: whether plex.tv marked that connection local
(`plaintext_local`), whether plex.tv saw this television behind the server's own network address
(`public_address_matches`), whether the server is `owned` by the signed-in account, whether it
requires secure connections (`https_required`), the kind of address it was (`plaintext_scope`:
private, link-local, unique-local, loopback, public or a name) and its family
(`plaintext_family`: v4, v6 or unknown), whether the app could offer to connect without encryption
on your home network or the fixed reason it could not (`plaintext_eligibility`: eligible,
https_required, not_local, not_same_network, not_private_address, identity_unverified,
https_unsettled or https_answered) and, when it asked, what became of the question
(`plaintext_consent`: offered, accepted, declined or revoked) — never the address itself, the
server or whose it is. When the account has no
server, it carries how many other devices plex.tv listed (`resources`, bucketed) and whether that
happened right after signing in or on a retry (`discovery_trigger`). For unreachable discovery it
also says whether the failed target was plex.tv or the listed servers (`discovery_target`).
It also carries whether the report was `consent`ed to as a standing choice or as a one-off, the app version and when it
happened. It never includes your account name, tokens, PIN, sign-in code or network addresses. With
crash reports on, it is sent automatically, carries the Crash report ID, and the sign-in screen
shows the random **Report ID** of that one report. With them off, or not yet decided, the sign-in
screen can offer **Send report** instead: that sends one report, only when you press it, with no
Crash report ID and with a random **Report ID** shown on screen that identifies only that one
report — not you or this television. A stalled wait is only ever reported that way, never
automatically.

## Optional product analytics

Product analytics is a separate choice and is off until you choose to share it. If enabled,
Native Jelly sends typed screen and feature events and broad sign-in and playback outcome classes to
PostHog in Germany. Reports carry a random Analytics ID created when you turn product analytics on
and may include the app version, webOS version, television model and SoC, and whether the selected
server is local, remote or relayed. Turning product analytics off, or Delete all local data,
deletes the local identifier; enabling it later creates a new one.

Signing in happens before this choice is asked. The sign-in events from before your answer
(started, completed, failed or cancelled, for each attempt; at most the eight most recent) are held
in memory only, never written to the television. Choosing to share sends them, dated when they
happened; declining discards them, and so does closing the app before you answer.

The Settings screen shows field-by-field example payloads produced through the same serializers
used for real reports.

Every product analytics event also carries this bounded compatibility and connection context:

| property | value |
|---|---|
| `app_version` | the Native Jelly package version |
| `webos_release` | the webOS release reported by nyx |
| `webos_api` | the webOS API version reported by nyx |
| `webos_codename` | the webOS firmware family reported by nyx |
| `device_model` | the LG model/platform class reported by nyx |
| `soc` | the SoC/board class reported by nyx |
| `hardware_revision` | the hardware revision class reported by nyx |
| `server_connection` | `local` / `remote` / `relay` / `unknown` — omitted entirely on an event with no one server, such as `app.launch`, `route.entered` or a `signin.*` event |
| `ip_version` | `v4` / `v6` / `unknown` — omitted entirely on an event with no one server, same as `server_connection` |
| `rtkmem` | `ok` / `missing` / `n/a` — the k5lp/k3lp `/dev/rtkmem` jail pre-flight |
| `install` | `devmode` / `homebrew` / `unknown` — never the install path |

| event | fields |
|---|---|
| `app.launch` | *(none)* |
| `route.entered` | `screen` — one of a fixed list of screen names |
| `signin.completed` | *(none)* |
| `signin.started` | *(none)* |
| `signin.failed` | `kind` — `pin_create` / `authorization` / `discovery` / `other` |
| `signin.cancelled` | *(none)* |
| `feature.used` | `feature` — one of a fixed list of feature names |
| `enhancement.refused` | `boost_dialog` — `true` / `false`; `normalize_loudness` — `true` / `false` |
| `playback.requested` | `playback_id` — a random number minted per attempt, never stored and never reused |
| `playback.started` | `playback_id` — a random number minted per attempt, never stored and never reused; `mode` — `direct` / `transcode` / `unknown` — `unknown` when no route was installed, as for a plan the server or a playback setting refused; `raster` — `sd` / `hd` / `fhd` / `uhd` / `unknown` — never the raster; `fps` — a fixed rung: `24`/`25`/`30`/`50`/`60`/`100`/`other`/`unknown` — never the measured rate; `video` — a codec name from a fixed table; anything else is `other`; `audio` — a codec name from a fixed table; anything else is `other`; `startup` — `<1s` / `1-3s` / `3-10s` / `10s+` — never the interval |
| `playback.failed` | `playback_id` — a random number minted per attempt, never stored and never reused; `mode` — `direct` / `transcode` / `unknown` — `unknown` when no route was installed, as for a plan the server or a playback setting refused; `kind` — `decision_refused` / `playback_policy` / `no_video_transcode_target` / `no_video_track` / `media_source` / `playback_interrupted` / `tv_pipeline` / `original_rollback` / `jail_missing_rtkmem` / `load_timeout` / `release_timeout` / `unspecified` |
| `playback.cancelled` | `playback_id` — a random number minted per attempt, never stored and never reused; `mode` — `direct` / `transcode` / `unknown` — `unknown` when no route was installed, as for a plan the server or a playback setting refused |
| `playback.abandoned` | `playback_id` — a random number minted per attempt, never stored and never reused; `mode` — `direct` / `transcode` / `unknown` — `unknown` when no route was installed, as for a plan the server or a playback setting refused |
| `playback.quality` | `playback_id` — a random number minted per attempt, never stored and never reused; `rebuffers` — `0` / `1` / `2-3` / `4+`; `buffering` — `none` / `<2s` / `2-10s` / `10s+` — never the interval |
| `playback.ended` | `playback_id` — a random number minted per attempt, never stored and never reused; `mode` — `direct` / `transcode` / `unknown` — `unknown` when no route was installed, as for a plan the server or a playback setting refused; `watched` — `abandoned` / `some` / `most` / `finished` — never a position or a duration |

## Never included in optional reports

Optional reports have no fields for media titles, account or profile names, searches, server
names or addresses, access tokens, subtitle text, or exact viewing history.

## Your choices

Crash reports and product analytics are independent. You can enable either, both or neither during
setup, and change either choice later in Settings → Privacy & data. Withdrawing a choice stops new
reports of that category, removes queued records that are no longer permitted, and deletes that
category's identifier from this television. Delete all local data does the same for both categories at once.
One report that the sender had already picked up at the moment you withdraw may still
be sent; no further report is picked up after it.

To ask what a category holds for your installation, or to have it deleted, ask through the contact
below and quote the identifier Settings shows for that category — the Crash report ID for crash
and error reports, the Analytics ID for product analytics. Each identifier is the only handle its
reports carry, so a request without it cannot be matched to anything. A one-off sign-in problem
report is sent only when you press Send report; quote the Report ID it showed to ask about it.

## Contact and non-affiliation

Privacy questions can be asked through <https://github.com/sostk/native-jelly/issues>. Issues there
are public, so do not post anything you would not want others to read. Security vulnerabilities may
be reported privately through GitHub Security Advisories for `sostk/native-jelly`.

Native Jelly is an independent, unofficial application. It is not produced by, endorsed by, or
affiliated with the Jellyfin project, LG Electronics Inc. or the PlxNative developer.
