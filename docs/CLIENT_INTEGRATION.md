# Client integration

Pyroshine extends the GameStream API only with additive, versioned elements
that standard Moonlight clients ignore. This guide is the contract for
integrations built on them, such as
[PyroDeck](https://github.com/karsyboy/pyrodeck), the Steam Deck plugin that
launches a library game on the host through [Pyrolight](https://github.com/karsyboy/pyrolight).

Integrations use the existing GameStream trust: there is no separate
integration credential, port or service. Launching, resuming and quitting
stay the standard `/launch`, `/resume` and `/cancel` requests of a paired
client, handled by the session manager like any other client.

## Launcher identities

Every application the catalog offers can name the launcher games it starts.
Integrations find a game by that identity instead of by title, because titles
are not identities: equal titles from different launchers are suffixed
(`Hades (Steam)`), several games can share a title, and a manual entry can
replace a discovered game under its own title.

| Where | Element | Visibility |
| --- | --- | --- |
| `/serverinfo` (HTTP and HTTPS) | `<PyroshineAppSources>1</PyroshineAppSources>` | Everyone, like the rest of serverinfo; it reveals only that the feature exists |
| `/applist` (HTTPS) | zero or more `<PyroshineSource>KIND:NATIVE_ID</PyroshineSource>` per `<App>` | Paired clients only, like the app list itself |

Identities are the catalog keys described in the
[architecture overview](ARCHITECTURE.md#application-catalog):

| Kind | Native ID | Example |
| --- | --- | --- |
| `steam` | Steam app ID or a non-Steam shortcut's 64-bit launch ID (decimal) | `steam:892970`, `steam:17375808096043008000` |
| `heroic` | store and app name | `heroic:legendary/Fortnite` |
| `lutris` | game slug | `lutris:quake` |
| `bottles` | program ID, or `umu:` and the UMU game for UMU entries | `bottles:3f2c9e1a-6b4d-4f8e-9a1c-2d7e5b8c0f14`, `bottles:umu:umu-1245620` |
| `desktop` | desktop file ID | `desktop:org.prismlauncher.PrismLauncher.desktop` |

Keep native IDs as strings. Steam store app IDs are below `0x80000000`;
non-Steam shortcuts use `(saved_app_id << 32) | 0x02000000`, which exceeds
JavaScript's exact integer range. Such a shortcut is a remote application, not
a Steam store game for local-library matching or Steam CDN artwork. Its
GameStream application ID remains the normal positive 32-bit catalog ID.

Which application reports an identity:

- A discovered application that is offered reports its own key.
- A manual `[[application]]` reports the keys of the discovered entries it
  shadows (same title), since it is the user's override of that game.
- Hidden, duplicate and shadowed discovered entries report nothing; they are
  not offered. Manual keys (`manual:<index>`) are configuration positions and
  are never reported.

An identity is reported by at most one offered application. Application IDs,
titles and the order of the list are unchanged by this extension.

`<PyroshineAppSources>` changes only for an incompatible change. A client that
finds no identity for a game on a host advertising version 1 can report that
the game is not installed (or is hidden); without the element, the host
cannot answer the question.

## Steam game state

While a session launched from a Steam game (`steam … steam://rungameid/<id>`)
is live, `/serverinfo` reports what Steam says about that game next to
`currentgame`:

```xml
<currentgame>1943979320</currentgame><PyroshineGameState>running</PyroshineGameState>
```

| Value | Meaning |
| --- | --- |
| `launching` | Steam is starting or launching the game; nothing reported yet. |
| `updating` | Steam is downloading or applying an update first. |
| `running` | The game's processes run. |
| `exited` | The game exited; Steam (Big Picture) still runs in the session. |

The element is absent when idle, for other applications and until the state
is known. It is public like `currentgame`, which it only qualifies. The
management interface reports the same in `SessionDetails.steam_game`
(`app_id`, `state`, `since_ms`).

Pyroshine reads it from the lines Steam appends to `logs/content_log.txt`
and `logs/gameprocess_log.txt` of the client the command starts
(`~/.steam/root`, or the Flathub Flatpak's data directory) after the launch,
once a second and bounded, without touching the stream. A game ending does
not end the session: Steam keeps it, as before.

## Launch errors

A launch that cannot start returns a GameStream error whose message the
client shows. Integrations display Pyrolight's dialog rather than parsing
these. Codes relevant to Steam launches (`SessionFailure::code`, also in the
management interface's problem log):

| Code | Cause |
| --- | --- |
| `steam_already_running` | A Steam client of the service's user is already running outside the session and would take the launch over. The launch is refused before the command runs; Pyroshine never closes that Steam. Entries with `pre_command` hooks are not checked. |
| `session_busy` | Another session is starting or stopping. |
| `launch_timeout` | The application did not reach an active state in time. |

## Conditional application cancellation

`/serverinfo` (HTTP and HTTPS) advertises the feature as
`<PyroshineConditionalCancel>1</PyroshineConditionalCancel>`. Builds without
the element ignore `expectedAppId` and stop whatever is running, so a client
must not send a conditional cancellation to them: refuse the action and ask
the user to update the host instead of falling back to an unconditional one.

Paired clients may add `expectedAppId=ID` to `/cancel`. The server compares the
positive GameStream application ID while holding the session lifecycle lock,
then begins teardown under that same lock. If no live session matches, the
request returns GameStream status 409 and preserves the current application.
Malformed/non-positive IDs return 400. Requests without this parameter retain
standard GameStream cancellation behavior, with the same paired HTTPS
certificate requirement. There is no new listener or management credential.

This protects an integration's explicit End Remote Game action when a different
application replaced its selected game. It identifies the application, not two
successive sessions of the same application. Client disconnect/resume and
stream reconfiguration never use this operation.

## Display modes

`/launch` and `/resume` take `mode=WIDTHxHEIGHTxREFRESH`. Besides whole
hertz, Pyroshine accepts the two fractional forms that Apollo-derived hosts
define and their clients send:

| Form | Example | Sent by |
| --- | --- | --- |
| Whole hertz, below 1000 | `1920x1080x60` | Every Moonlight client |
| Decimal hertz, up to three decimals | `1920x1080x59.94` | Nightjar, Vibepollo clients |
| Millihertz, 1000 and above | `1920x1080x59940` | Artemis, on a fractional-rate display |

The refresh is rounded to whole hertz (59.94 Hz runs at 60 Hz, as it does for
a client that sends `60`). Like Apollo, a value of 1000 or more is millihertz.

## Dynamic range and bit depth

`/launch` and `/resume` take `hdrMode`; RTSP ANNOUNCE takes
`x-nv-video[0].dynamicRangeMode` and, from Pyrolight, an explicit
`x-moonshine-video[0].bitDepth`. ANNOUNCE decides the stream:

| `dynamicRangeMode` | `bitDepth` | Request's `hdrMode` | Stream |
| --- | --- | --- | --- |
| `0` or absent | absent | any | 8-bit SDR |
| `1` | absent | `1` or absent | HDR10 (10-bit, BT.2020/PQ) |
| `1` | absent | `0` | 10-bit SDR (sRGB, BT.709 primaries), H.264/HEVC/AV1 only |
| either | `8` or `10` | any | that depth, SDR or HDR10 as `dynamicRangeMode` says |

Moonlight clients send `hdrMode=1` with HDR and nothing otherwise. Sunshine
reads `dynamicRangeMode=1` as 10-bit encoding and HDR as a separate launch
choice, and Nightjar asks for 10-bit SDR that way: `hdrMode=0` outright, then
`dynamicRangeMode=1` with HEVC or AV1 Main10. The request is the one that
authorized the stream: the latest `/resume` that sent `hdrMode`, else the
launch. PyroWave has no 10-bit SDR format, so its `dynamicRangeMode=1` stays
HDR10. A codec the GPU cannot encode in 10-bit SDR is refused, as any
unverified profile is.

## Live bitrate

A paired client may change the encoder bitrate of its own stream without
reconnecting:

```text
GET /bitrate?bitrate=20000   (HTTPS, paired client certificate)
<root status_code="200"><bitrate>20000</bitrate></root>
```

This is the runtime call that Vibepollo and Foundation Sunshine define;
Nightjar sends it from its stream menu and automatic bitrate. `bitrate` is in kbps, like `x-ml-video.configuredBitrateKbps`, and
the reply's `<bitrate>` is the rate the encoder accepted: the profile's maximum
may lower it, and PyroWave's per-frame budget is aligned down to 4 bytes.

| Status | `<bitrate>` | Cause |
| --- | --- | --- |
| 200 | accepted kbps | Applied from the next encoded picture. If no picture is encoded within a second (a static VRR scene), the reply carries the requested kbps and the change applies with the next picture. |
| 400 | 0 | `bitrate` missing, zero or above 4294967 kbps. |
| 404 | 0 | No video is streaming, or the caller's address does not hold the stream's authorization. |
| 409 | 0 | A reconnect is in progress, a newer request or reconnect replaced it, or the encoder refused the rate (a PyroWave frame budget outside 1 KiB to 3 MiB). |

Errors use the GameStream convention of HTTP 200 with the status in the XML.
Changes belong to the stream's authorization generation: a reconnect
negotiates its bitrate in its own ANNOUNCE and discards an unapplied request.
Plain HTTP does not serve `/bitrate`.

During a stream, a paired client's `/serverinfo` reports the encoder rates as
`<RustHostEncoderBitrateKbps>` (current) and
`<RustHostInitialEncoderBitrateKbps>` (negotiated), Rubylight's names, which
Nightjar's automatic bitrate starts from.

## Vibepollo extensions

Clients built for Vibepollo, such as Nightjar, enable a group of features when
`/serverinfo` carries `<FrameLimiterSupported>`, whatever its value. Pyroshine
always sends it, with the configured default `[frame_limit]`:

| Element | Value |
| --- | --- |
| `FrameLimiterSupported` | `1` |
| `FrameLimiterEnabled` | `1` unless `mode = "off"` |
| `VirtualDisplayFrameLimiterEnabled` | `1` for `mode = "stream"` |
| `FrameLimiterFpsLimitMilliHz` | `fps` × 1000 for `mode = "custom"`, else `0` |

The features it unlocks, all served here:

- `clientVrrRequested=1` at launch and resume, which selects VRR capture.
- Decimal refresh rates (see [display modes](#display-modes)).
- `/bitrate` (see [live bitrate](#live-bitrate)).
- Host-confirmed replacement. Such a client launches a different application
  without `/cancel`. Pyroshine answers the first such `/launch` with status 410
  and Vibepollo's exact prompt, `An app is already running. Launch this app
  again within 60 seconds to confirm that you want to close it.`; the client
  asks its user and repeats the request. A repeat from the same client
  certificate, for the same application and the same running session, within
  60 seconds stops the running application and launches the new one. Any other
  request re-arms the prompt. A launch of the running application itself, or
  one without a client certificate, is unchanged and reports the running
  session.

## Validation

Automated:

```sh
cargo test -p moonshine-core --lib -- app_scanner::catalog webserver steam_instance steam_game session::negotiation session::stream::video::rate
```

- `catalog::tests::launcher_identities_*`: own keys, manual overrides, equal
  titles, hidden entries.
- `webserver::app_list_tests`: the `/applist` document.
- `app_scanner::steam::tests` and `app_scanner::service::tests::steam_shortcuts_*`:
  saved shortcut launch IDs, custom art, profile/file watches, catalog refresh,
  rename stability and retention during failed reads.
- `webserver::security_tests::serverinfo_advertises_the_app_sources_version`.
- `session::steam_game::tests`: log parsing from real Steam lines, the state
  machine, tailing across partial writes and rotation, read bounds, and the
  observer ending with the session.
- `webserver::security_tests::serverinfo_reports_steam_game_state_with_the_running_game`.
- `session::steam_instance::tests`: Steam command detection, process
  detection by user and state, hooks and Desktop Mode exemptions.
- `session::negotiation::tests::fractional_and_millihertz_refresh_rates_round_to_hertz`
  and `webserver::security_tests::malformed_launch_and_resume_values_change_nothing`:
  the three refresh forms and their rejections.
- `rtsp::tests::explicit_sdr_launch_turns_ten_bit_dynamic_range_into_sdr`,
  `rtsp::tests::profile_axes_preserve_hdr_chroma_and_explicit_sdr_depth` and
  `session::manager::lifecycle_tests::requested_hdr_mode_follows_the_launch_then_each_resume`:
  10-bit SDR from an explicit `hdrMode=0`, explicit depths unchanged.
- `webserver::security_tests::live_bitrate_requires_a_paired_caller_and_its_own_stream`
  (paired HTTPS only, value domain, no stream, foreign peer) and
  `session::stream::video::rate::tests` (coalescing, generation scoping,
  failure reporting).

Manual, on a host with Steam installed: with the desktop Steam running,
launching a Steam game from a client reports `steam_already_running` and the
game does not open on the desktop; with it closed, the game streams.

Manual, with a real client: change the bitrate from Nightjar's stream
menu during an H.264, HEVC, AV1 and PyroWave stream and confirm the encoder rate in the
session log (`Client changed the live bitrate`) and the client's statistics.
Stream from Artemis on a 59.94 Hz display and confirm a 60 Hz session.
Turn on Nightjar's 10-bit SDR setting with HDR off, stream HEVC and AV1, and
confirm `bit_depth=10` with `hdr=false` in the session log and an SDR picture,
then disconnect and resume.
