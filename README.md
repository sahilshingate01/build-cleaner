# buildcleaner

A Rust CLI that scans a drive for regenerable build output — caches, dependency
dirs, and generated artifacts (`node_modules`, `.next`, `.turbo`, `target`,
`zig-out`, `__pycache__`, `.gradle`, `Pods`, …) — reports how much space they're
using, and optionally deletes them.

**Dry run by default. Deletion is opt-in and prompts before touching anything.**

## Build

```sh
cargo build --release   # binary at target/release/buildcleaner
cargo test              # unit tests
```

## Usage

```sh
buildcleaner ~/Documents/GitHub                    # dry run: find + total the savings
buildcleaner / --min-size 500M                     # only the big wins
buildcleaner ~/code --output-format json           # one machine-readable JSON blob
buildcleaner ~/code --output-format stream-json    # live NDJSON event stream (for apps)
buildcleaner --list-targets                        # print the reverse-.gitignore list
buildcleaner ~/code --delete                       # delete (prompts y/N)
buildcleaner ~/code --delete --yes                 # delete without prompting
```

The three knobs — **output format · input source · action** — work like `claude -p`.
`--json` and `--stream` are kept as aliases for `--output-format json|stream-json`.

### Live progress (terminal)

On a TTY the scan shows a live status line while it works and streams each match
as it's discovered and sized. Long paths are shortened in the middle (`/Users/…/node_modules`)
to fit the terminal width; piped/redirected output keeps full paths. To force a
width when piping (e.g. driving a fixed-width panel), set `COLUMNS=<n>`. Machine
formats (`--json`, `--stream`) always emit full, unshortened paths.

```
      SIZE  CATEGORY     PATH
  143.4 MB  target       /Users/j/…/rustthing/target
⠹  20,050 dirs · 50 matches · 194.4 MB reclaimable · 1s
```

Progress goes to stderr, results to stdout, so piping stays clean.
Force with `--progress always`, disable with `--progress never`.

### Driving it from an app (the engine contract)

The CLI *is* the API: a UI spawns it, sets `--output-format`, and reads stdout.
`--output-format stream-json` emits **newline-delimited JSON** (one event per
line, flushed immediately), so a host app can parse it and drive a realtime UI.
No ANSI, nothing on stderr except hard errors.

Event types:

| event | fields |
|-------|--------|
| `start` | `root`, `min_bytes` |
| `progress` | `scanned`, `matches`, `bytes`, `elapsed_ms` — heartbeat every ~80ms |
| `match` | `path`, `category`, `bytes`, `files` — one per reclaimable dir as it's sized |
| `summary` | `scanned`, `elapsed_secs`, `total_bytes`, `count`, `categories[]` (`{category,bytes,dirs}`) |

With `--delete --yes` you also get `deleted` (`path`,`bytes`),
`delete_error` (`path`,`error`), and a final `delete_summary`
(`deleted`,`failed`,`freed_bytes`).

```jsonc
{"event":"start","root":"/code","min_bytes":0}
{"event":"progress","scanned":1663,"matches":0,"bytes":0,"elapsed_ms":87}
{"event":"match","path":"/code/app/node_modules","category":"node","bytes":1048576,"files":1}
{"event":"summary","scanned":21187,"elapsed_secs":2.038,"total_bytes":1048576,"count":1,"categories":[{"category":"node","bytes":1048576,"dirs":1}]}
```

Consume it from Node, for example:

```js
const cp = require("node:child_process")
const rl = require("node:readline")
const p = cp.spawn("buildcleaner", ["/code", "--stream"])
rl.createInterface({ input: p.stdout }).on("line", (line) => {
  const e = JSON.parse(line)
  if (e.event === "progress") updateBar(e.scanned, e.bytes)
  if (e.event === "match") addRow(e.path, e.category, e.bytes)
  if (e.event === "summary") finish(e.total_bytes, e.count)
})
```

#### Deleting a selection (`--input-format`)

A UI's loop is **scan → user picks a subset → delete only that subset**. Feed the
chosen paths back on stdin:

```sh
# pipe the match events you kept straight back in:
picks.ndjson | buildcleaner --delete --yes \
  --input-format stream-json --output-format stream-json
```

`--input-format text` accepts one path per line; `stream-json` accepts one
`{"path":"…"}` object per line — so you can pipe back the exact `match` events you
received (extra fields are ignored). You get `deleted` / `delete_error` /
`delete_summary` events.

**Every stdin path is re-validated against the same target rules before removal** —
it must be an existing real directory (not a symlink) whose name matches a known
target with marker-gating satisfied. Hand it `~/Documents` and it's rejected, not
deleted. Input is limited to 10,000 paths and 16 KiB per line. Non-interactive
deletes require `--yes`.

A complete ~200-line Electron app that drives exactly this contract (live stats,
checkboxes, delete-selected) lives in
[`examples/electron-cleaner`](examples/electron-cleaner).

### Flags

| flag | effect |
|------|--------|
| `--delete` | remove the found dirs (dry run otherwise) |
| `-y, --yes` | skip the confirmation prompt (required for any non-interactive `--delete`) |
| `--min-size <SIZE>` | ignore dirs smaller than e.g. `100M`, `1.5G` |
| `--progress <WHEN>` | live progress: `auto` (tty), `always`, `never` |
| `--follow-symlinks` | follow in-root directory symlinks and linked files (off by default; loops and out-of-root links are skipped) |
| `--include <NAME>` | treat an extra dir name as reclaimable (no marker) |
| `--exclude <NAME>` | never match/descend into a dir name |
| `--config <FILE>` | load extra targets from a reverse-gitignore file |
| `--output-format <FMT>` | `text` (table), `json` (one blob), or `stream-json` (NDJSON events; required for scan-and-delete) |
| `--input-format <FMT>` | operate on stdin paths: `text` (one/line) or `stream-json` (`{"path":…}`/line) |
| `--json` / `--stream` | aliases for `--output-format json` / `stream-json` |
| `--list-targets` | print the built-in list and exit |

## The reverse .gitignore

A normal `.gitignore` lists what to *skip*. buildcleaner works off the inverse:
a curated list of directory names to actively *look for* and reclaim. See the
full built-in set with `buildcleaner --list-targets`.

Generic names (`target`, `dist`, `build`, `obj`, `vendor`, `coverage`, `Pods`,
`_build`, `deps`) only match when a **marker file** proving the project type
sits next to them — so a hand-written `dist/` with no `package.json`, or a
`target/` with no `Cargo.toml`, is left alone.

### Extending the list

`--include somedir` adds an ad-hoc name for one run. For a persistent list,
point `--config` at a file (same format as `--list-targets` output):

```
# one entry per line: name[:category[:marker,...]]
.deno:deno
build:build:package.json,CMakeLists.txt
Library:unity:ProjectSettings
```

Config/include entries override built-ins with the same name.

## Safety

- Dry run unless `--delete`; `--delete` still prompts unless `-y`.
- A non-interactive/empty answer aborts (nothing is deleted).
- Symlinks are not followed by default (no escaping the tree, no loops).
- Marker gating keeps generic dir names from being false positives.
- Deletes fed via `--input-format` are re-validated against the target rules
  before removal, so a UI can never make it delete an arbitrary directory.
- Scan-mode deletes are re-validated under the original scan root immediately
  before removal. Use `--output-format stream-json` for scan-and-delete; JSON
  output is intentionally dry-run only so stdout always remains one JSON object.
