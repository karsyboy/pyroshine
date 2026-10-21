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
| `steam` | Steam app ID (decimal) | `steam:892970` |
| `heroic` | store and app name | `heroic:legendary/Fortnite` |
| `lutris` | game slug | `lutris:quake` |
| `desktop` | desktop file ID | `desktop:org.prismlauncher.PrismLauncher.desktop` |

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

## Validation

Automated:

```sh
cargo test -p moonshine-core --lib -- app_scanner::catalog webserver steam_instance steam_game
```

- `catalog::tests::launcher_identities_*`: own keys, manual overrides, equal
  titles, hidden entries.
- `webserver::app_list_tests`: the `/applist` document.
- `webserver::security_tests::serverinfo_advertises_the_app_sources_version`.
- `session::steam_game::tests`: log parsing from real Steam lines, the state
  machine, tailing across partial writes and rotation, read bounds, and the
  observer ending with the session.
- `webserver::security_tests::serverinfo_reports_steam_game_state_with_the_running_game`.
- `session::steam_instance::tests`: Steam command detection, process
  detection by user and state, hooks and Desktop Mode exemptions.

Manual, on a host with Steam installed: with the desktop Steam running,
launching a Steam game from a client reports `steam_already_running` and the
game does not open on the desktop; with it closed, the game streams.
