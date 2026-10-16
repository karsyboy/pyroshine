# Pyroshine web interface

The management interface for a running Pyroshine server: sign-in, pairing,
paired clients, applications, settings, the stream dashboard, session
history, diagnostics, updates and maintenance. User documentation is in
[Web interface and tray](../docs/MANAGEMENT.md); the design is described in
[Architecture](../docs/ARCHITECTURE.md#management).

It is a React, TypeScript and Material UI application that the server serves
as static files (`share/pyroshine/web`). It talks only to the server's HTTP API
(`/api/v1/`) and its event stream; Node.js is needed to build it, never at
runtime. The desktop tray (`../pyroshine-tray`) only opens these pages.

| Part | Where |
| --- | --- |
| Pages and components | `src/pages/`, `src/components/` |
| API contract | `src/api/backend.ts` (typed operations and stream events) |
| Server client | `src/api/http.ts` (JSON with the session cookie and CSRF token; Server-Sent Events) |
| Development server stand-in | `src/api/mock.ts` (`?mock=…`, never in production builds) |
| Sign-in and live state | `src/state/auth.tsx`, `src/state/daemon.tsx` |
| Routes | `src/state/route.ts` (hash routes the tray and notifications open) |

## Build

Prerequisites: Node.js 20 or newer with npm.

```sh
cd pyroshine-ui
npm ci
npm run build
```

The output is `dist/`; packages install it as `/usr/share/pyroshine/web`. To run
a locally built server against it, set `[management] web_root` (or
`MOONSHINE_WEB_ROOT`) to the `dist/` directory. The build writes
`dist/pyroshine-web.json` with the version from `package.json`; the server
warns when it differs from its own.

The interface is versioned with the server. Do not edit the versions in
`package.json` or `package-lock.json` by hand: `python3 scripts/changelog.py
sync-versions` (also run by the release's `prepare`) copies the workspace
version into them, and CI's changelog check fails while they differ.

## Develop

```sh
npm run dev   # http://localhost:1420
```

With `?mock=<scenario>` in the address the page runs against a simulated
server: `streaming` (default), `idle`, `retained`, `pairing`, `offline`,
`composited`, `crashed`, `old-daemon`, `signin` (signed out; the mock password is
`pyroshine`) or `setup` (first run; any 16-character code). Without it, the dev
server proxies `/api` to a running Pyroshine at `http://127.0.0.1:47990`
(`PYROSHINE_URL` selects another), so the page can be developed against real
state with hot reload.

## Checks

```sh
npm run typecheck
npm test
npm run build
```

The settings editor renders forms from the server's configuration schema.
`src/api/schema.fixture.json` is a copy of that schema used by the tests and the
mock; a server test fails when it is out of date. Regenerate it with:

```sh
MOONSHINE_UPDATE_UI_FIXTURES=1 cargo test -p moonshine-core ui_schema_fixture_is_current
```

Icons derive from `assets/logo-no-text.png`; regenerate them with
`python3 pyroshine-ui/scripts/generate-icons.py` (requires Pillow).

## Acceptance checks

Automated tests cover the HTTP API and its protections over a real listener,
the management interface over a private bus, sign-in and sessions, the session
history store, update checks, the session-phase derivation, pairing token
binding, configuration editing, the tray's routing and session detection, and
the frontend's client, routes, draft logic and library page. These checks need
a host with a GPU, a browser and a Moonlight client; record the browsers,
desktop environment and client used.

1. **First run (headless).** On a host without a desktop session, start the
   service and run `pyroshine web setup` over SSH. Through `ssh -L
   47990:127.0.0.1:47990`, open the page on another computer, enter the code
   and a password: the dashboard appears. Without signing in, every page and
   `/api/v1/overview` refuse access; another local user's `curl` gets 401.
2. **Pairing.** Pair a Moonlight client from **Clients**: the request appears
   live, entering its PIN pairs it and it is listed; a wrong PIN fails on both
   sides; **Reject** ends the request; restarting pairing in Moonlight replaces
   the shown request. Revoke the client: it can no longer connect.
3. **Streaming state.** Start a stream: the dashboard matches the application,
   mode, codec, HDR and audio Moonlight negotiated, with statistics about once
   per second. Disconnect and resume Moonlight: "Client disconnected", then
   streaming again. **End Session** returns to idle through the normal teardown.
   The session appears in **History** with its reconnect and stop.
4. **Settings.** Edit a commented `config.toml` and save: only the edited
   settings change, invalid values are refused, an external edit before saving
   reports a conflict, and the restart notice appears.
5. **Resilience.** With a stream running, close the browser, reload the page
   and open a second tab: the stream is unaffected and both tabs follow it.
   Restart the service: the page shows it is reconnecting and recovers on its
   own, still signed in. Sign out in one tab: the other returns to sign-in.
6. **Desktop tray (KDE Plasma Wayland).** Log in: exactly one tray icon appears
   (plain logo). Left-click opens the dashboard in the default browser, focused.
   A pairing request turns the icon indigo and shows one notification (a
   retrying client updates it); clicking it opens that request. Streaming shows
   the green badge, a disconnected client the amber one; **End Session** works
   without a browser. **Quit Pyroshine Tray** and `kill -9` leave the server and
   stream running; starting the tray again shows the current state.
7. **Browsers.** Repeat 1–5 briefly in Firefox and a Chromium-based browser, and
   check the layout on a phone-sized window (navigation drawer, no sideways
   scrolling).
8. **Applications.** With a Steam scanner, install or uninstall a small game:
   within about 15 seconds it appears in or leaves **Applications** and the
   client's list. Hide a game; restart: it stays hidden with the same ID.
