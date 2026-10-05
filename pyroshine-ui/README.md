# Pyroshine desktop app

Optional desktop companion for a running Pyroshine server: system tray, pairing
notifications and approval, paired-client management, a configuration editor,
a stream dashboard and diagnostics. User documentation is in
[Desktop app](../docs/DESKTOP.md); the design is described in
[Architecture](../docs/ARCHITECTURE.md#desktop-management-interface).

The app is a separate Cargo workspace so that building or testing the server
never compiles its GUI dependencies.

| Part | Technology |
| --- | --- |
| Window | Tauri 2 (WebKitGTK) with React, TypeScript and Material UI (`src/`) |
| Tray | `ksni` StatusNotifierItem over D-Bus (`src-tauri/src/tray.rs`) |
| Notifications | `org.freedesktop.Notifications` over D-Bus (`src-tauri/src/notifications.rs`) |
| Daemon access | `moonshine-management` proxy on the session bus (`src-tauri/src/daemon.rs`) |

## Build

Prerequisites: a Rust toolchain, Node.js 20 or newer with npm, and the WebKitGTK
4.1 and GTK 3 development packages (on Debian/Ubuntu
`libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev`, plus the server's build
dependencies from [CONTRIBUTING.md](../CONTRIBUTING.md)).

```sh
cd pyroshine-ui
npm ci
npm run build
cargo build --release --locked --manifest-path src-tauri/Cargo.toml
```

The binary is `src-tauri/target/release/pyroshine-ui`. `npm run build` must run
first: the Rust build embeds `dist/`.

## Develop

```sh
npm run dev            # browser preview at http://localhost:1420 with a mock backend
npx tauri dev          # the real app against a running Pyroshine
```

The browser preview simulates a daemon (`src/api/mock.ts`); add `?mock=idle`,
`?mock=retained`, `?mock=pairing` or `?mock=offline` to the URL for other
states. Production builds exclude the mock.

## Checks

```sh
npm run typecheck
npm test
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

The settings editor renders forms from the daemon's configuration schema.
`src/api/schema.fixture.json` is a copy of that schema used by the tests and the
preview; a server test fails when it is out of date. Regenerate it with:

```sh
MOONSHINE_UPDATE_UI_FIXTURES=1 cargo test -p moonshine-core ui_schema_fixture_is_current
```

Icons derive from `assets/logo-no-text.png`; regenerate them with
`python3 pyroshine-ui/scripts/generate-icons.py` (requires Pillow).

## Acceptance checks

Automated tests cover the management interface over a private bus, the
session-phase derivation, pairing token binding, configuration editing and the
frontend's draft logic. These checks need a desktop, a GPU and a Moonlight
client; record the desktop environment, session type and client used.

1. **Tray.** With the service running, log in to the desktop: the tray icon
   appears (plain logo). Left-click opens the dashboard ("ready for connections").
2. **Pairing.** Revoke a test client in **Clients** and confirm it can no longer
   connect. Pair it again from Moonlight: a notification appears, clicking it
   opens **Clients** at the request, entering the PIN pairs the client and it is
   listed; a wrong PIN fails on both sides; **Reject** ends the request in
   Moonlight. Restarting pairing in Moonlight replaces the shown request.
3. **Headless fallback.** Quit the app and pair a client using the notification
   or the logged `http://localhost:<port>/pin?uniqueid=…` link.
4. **Streaming state.** Start a stream: the icon shows the green play badge and
   the dashboard matches the application, resolution, refresh rate, codec, HDR
   and chroma Moonlight negotiated, with statistics about once per second.
   Disconnect Moonlight without quitting: the amber pause badge and "Client
   disconnected" appear and the game keeps running. Resume: the state returns to
   streaming. **End Session** (tray or dashboard) closes the application and
   returns to idle through the normal teardown (service log "Stopping session").
5. **Settings.** Edit a commented, non-trivial `config.toml` in the app and
   save: only the edited settings change in the file, invalid values are
   refused with the setting highlighted, editing the file externally before
   saving reports a conflict, and the restart notice appears. Restart the
   service and confirm the settings apply.
6. **Isolation.** During a stream, `kill -9` the app: the stream continues.
   Start the app again: it shows the live session. Restart the service with the
   app open: it reports Pyroshine as unavailable, then reconnects.
