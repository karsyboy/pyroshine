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
