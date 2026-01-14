// Drives the buildcleaner binary over its JSON stdout protocol.
// Pure Node — no Electron — so it can be unit/smoke-tested without a display.
const { spawn } = require('node:child_process')
const path = require('node:path')
const fs = require('node:fs')

// Resolve the binary: explicit override → the repo's release build → PATH.
function resolveBin() {
  if (process.env.BUILDCLEANER_BIN) return process.env.BUILDCLEANER_BIN
  const local = path.resolve(__dirname, '..', '..', 'target', 'release', 'buildcleaner')
  return fs.existsSync(local) ? local : 'buildcleaner'
}

// Turn a chunked byte stream into complete NDJSON lines across chunk boundaries.
function lineSplitter(onLine) {
  let buf = ''
  return {
    push(chunk) {
      buf += chunk
      let i
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i)
        buf = buf.slice(i + 1)
        if (line.trim()) onLine(line)
      }
    },
    flush() {
      if (buf.trim()) onLine(buf)
      buf = ''
    },
  }
}

// Common wiring: parse each stdout line as JSON, forward to onEvent; collect stderr.
// Returns { child, done } where done resolves with { code, stderr }.
function wire(child, onEvent) {
  const done = new Promise((resolve, reject) => {
    const splitter = lineSplitter((line) => {
      let obj
      try {
        obj = JSON.parse(line)
      } catch {
        return // ignore anything that isn't a JSON event
      }
      onEvent(obj)
    })
    child.stdout.setEncoding('utf8')
    child.stdout.on('data', (c) => splitter.push(c))
    let stderr = ''
    child.stderr.setEncoding('utf8')
    child.stderr.on('data', (c) => (stderr += c))
    child.on('error', reject)
    child.on('close', (code) => {
      splitter.flush()
      resolve({ code, stderr })
    })
  })
  return { child, done }
}

// Read-only scan of one or more roots. onEvent gets start / progress / match /
// summary events. `roots` may be a single path string or an array of paths;
// the engine walks them all and emits one merged event stream.
function scan(bin, roots, onEvent, configFile = null) {
  const list = Array.isArray(roots) ? roots : [roots]
  const args = ['--output-format', 'stream-json']
  if (configFile) args.push('--config', configFile)
  // `--` MUST come last, immediately before the paths: it forces the engine to
  // parse every remaining argument as a positional path. Without it a caller-
  // supplied path like "--delete" would be honored as a flag, turning a
  // read-only scan into a delete.
  args.push('--', ...list)
  return wire(spawn(bin, args), onEvent)
}

// Delete an explicit list of paths. The engine RE-VALIDATES every path against its
// target rules before removing, so a bad path here is rejected, not obeyed.
function del(bin, paths, onEvent) {
  const child = spawn(bin, [
    '--delete',
    '--input-format',
    'stream-json',
    '--output-format',
    'stream-json',
    '--yes',
  ])
  for (const p of paths) child.stdin.write(JSON.stringify({ path: p }) + '\n')
  child.stdin.end()
  return wire(child, onEvent)
}

module.exports = { resolveBin, scan, delete: del }
