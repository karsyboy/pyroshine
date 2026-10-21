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

## Launch errors

A launch that cannot start returns a GameStream error whose message the
client shows. Integrations display Pyrolight's dialog rather than parsing
these. Codes relevant to Steam launches (`SessionFailure::code`, also in the
management interface's problem log):

| Code | Cause |
| --- | --- |
| `session_busy` | Another session is starting or stopping. |
| `launch_timeout` | The application did not reach an active state in time. |

## Validation

Automated:

```sh
cargo test -p moonshine-core --lib -- app_scanner::catalog webserver
```

- `catalog::tests::launcher_identities_*`: own keys, manual overrides, equal
  titles, hidden entries.
- `webserver::app_list_tests`: the `/applist` document.
- `webserver::security_tests::serverinfo_advertises_the_app_sources_version`.
