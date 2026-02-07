# Native Cleaner

The Vercel Native SDK wrapper for buildcleaner. It keeps the existing Electron
demo available while opening the same loopback-only scan/delete UI in a native
system-WebView window.

Run it from the Electron example directory:

```sh
npm run native
```

This builds the Rust engine, starts the local cleaner server without opening a
browser on its dedicated `127.0.0.1:45831` port, and launches the Native SDK window through the adjacent `../native`
checkout. The macOS window uses a hidden-inset titlebar: the traffic lights stay
visible while the app content extends into the drag band.

A minimal native-sdk desktop app with a web frontend.

## Setup

`zig build dev`, `zig build run`, and `zig build package` install frontend dependencies automatically. To install them explicitly, run:

```sh
npm install --prefix frontend
```

The generated build defaults to this Native SDK framework path:

```text
../../../native

```

Override it with `-Dnative-sdk-path=/path/to/native-sdk` if you move this app.

## Commands

```sh
zig build dev
zig build run
zig build test
zig build package
native doctor --manifest app.zon
```

`zig build dev` starts the frontend dev server from `app.zon`, waits for it, and launches the native shell with `NATIVE_SDK_FRONTEND_URL`.

Frontend:

- Type: vite
- Production assets: `frontend/dist`
- Dev URL: `http://127.0.0.1:5173/`

## Web Engines

The generated app defaults to the system WebView. On macOS you can switch to Chromium/CEF with:

```sh
native cef install
zig build run -Dplatform=macos -Dweb-engine=chromium
```

`native cef install` downloads Native SDK's prepared CEF runtime, including the native wrapper library.

For one-command local setup, opt into build-time install:

```sh
zig build run -Dplatform=macos -Dweb-engine=chromium -Dcef-auto-install=true
```

Use `-Dcef-dir=/path/to/cef` when you keep CEF outside the platform default under `third_party/cef`.

```sh
native doctor --web-engine chromium
```

Diagnostics:

- Set `NATIVE_SDK_LOG_DIR` to override the platform log directory during development.
- Set `NATIVE_SDK_LOG_FORMAT=text|jsonl` to choose persistent log format.
