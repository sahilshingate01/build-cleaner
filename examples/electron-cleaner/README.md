# electron-cleaner

A small Electron app and loopback-only browser demo that drive the `buildcleaner`
engine over its JSON stdout protocol — proving the CLI can be the single source
of truth behind any UI.

It does the full loop: **scan → tick a subset → delete only that subset**, with
live stats (dirs · matches · reclaimable · elapsed) climbing as the scan runs.

```
┌──────────────────────────────────────────────────────────┐
│ /Users/you/code            [ Choose… ]  [ Scan ]          │
├──────────────────────────────────────────────────────────┤
│ 21,187 dirs   37 matches   4.2 GB reclaimable   2.0s      │
├──┬────────┬──────────┬───────────────────────────────────┤
│☑ │ 181 MB │ node     │ /Users/you/code/app/node_modules  │
│☐ │  92 MB │ next     │ /Users/you/code/web/.next         │
├──┴────────┴──────────┴───────────────────────────────────┤
│ [ Delete selected ]   6 selected · 1.1 GB      done · 37  │
└──────────────────────────────────────────────────────────┘
```

## How it works

The app never scans or deletes anything itself — it spawns `buildcleaner` and
reads its NDJSON events.

- **`engine.js`** — the whole integration, ~90 lines, no Electron. Spawns the
  binary, splits stdout into JSON events, exposes `scan()` and `delete()`.
- **`main.js`** — Electron main process. Owns the window + the subprocess; relays
  engine events to the renderer over IPC. (`contextIsolation` on, `nodeIntegration` off.)
- **`preload.js`** — the only renderer↔main bridge: `pickFolder / scan / delete / onEvent`.
- **`renderer.js` + `index.html`** — turns events into a live table and stats.

### The two commands it runs

```sh
# scan (read-only, streaming):
buildcleaner <path> --output-format stream-json

# delete the ticked rows (paths piped in on stdin, re-validated by the engine):
buildcleaner --delete --yes --input-format stream-json --output-format stream-json
# stdin: {"path":"/Users/you/code/app/node_modules"}  (one per line)
```

The engine re-validates every path before deleting, so the UI physically cannot
make it remove something that isn't a recognized reclaimable directory.

## Run

```sh
# 1. build the engine (from the repo root)
cargo build --release

# 2. install dependencies, then launch Electron
cd examples/electron-cleaner
npm install
npm start

# Or launch the loopback-only browser demo
npm run web

# Or launch the Vercel Native SDK wrapper (uses ../native)
npm run native
```

Electron finds the binary automatically at `../../target/release/buildcleaner`.
Override with `BUILDCLEANER_BIN=/path/to/buildcleaner npm start`.

The browser demo binds only to `127.0.0.1`, limits delete requests to 1 MiB / 1,024
paths, and asks for confirmation before a selected deletion.

Electron keeps the current scan's events and root in its main process. Reloading
the renderer during development restores the current table, stats, and deleted-row
state without starting a new scan.

`npm run native` keeps Electron available but launches the Native SDK wrapper in
[`../native-cleaner`](../native-cleaner). The wrapper uses a hidden-inset native
titlebar, retaining macOS traffic lights while content can extend into the drag band.

## Verify headlessly (no GUI)

`engine.js` is pure Node, so you can prove the integration without a display:

```sh
npm run smoke -- ~/code      # scans and prints the events engine.js parses
```

## Port it anywhere

`engine.js` has no Electron dependency — the same spawn-and-parse pattern drives a
Tauri app, a terminal TUI, a menu-bar utility, or a thin web backend. The contract
is just: spawn the binary, pick your `--output-format`, read stdout.
