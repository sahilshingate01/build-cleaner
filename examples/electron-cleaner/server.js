// No-dependency browser demo: a tiny HTTP server that spawns buildcleaner and
// streams its events to a browser tab over SSE. Node built-ins only — nothing to
// install, no Electron binary. Reuses the same engine.js the Electron app uses.
const http = require('node:http')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const engine = require('./engine')

const bin = engine.resolveBin()
const PORT = Number(process.env.PORT) || 4173
const HOST = '127.0.0.1'
const MAX_BODY_BYTES = 1024 * 1024
const MAX_DELETE_PATHS = 1024
const MAX_PATH_BYTES = 4096
const MAX_SCAN_ROOTS = 256
const MAX_PATTERN_BYTES = 16 * 1024

function temporaryPatternFile(patterns) {
  if (!patterns.trim()) return null
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'buildcleaner-patterns-'))
  const file = path.join(dir, 'patterns.conf')
  fs.writeFileSync(file, patterns, { mode: 0o600 })
  return { dir, file }
}

function removeTemporaryPatternFile(config) {
  if (config) fs.rmSync(config.dir, { recursive: true, force: true })
}

// Track live child processes so a server shutdown (Ctrl-C) doesn't orphan scans.
const active = new Set()
function track(child) {
  active.add(child)
  child.on('close', () => active.delete(child))
  return child
}

function sseHead(res) {
  res.writeHead(200, {
    'content-type': 'text/event-stream',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
  })
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `http://localhost:${PORT}`)

  // --- static files ---
  if (req.method === 'GET' && (url.pathname === '/' || url.pathname === '/index.html')) {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' })
    return fs.createReadStream(path.join(__dirname, 'web.html')).pipe(res)
  }
  if (req.method === 'GET' && url.pathname === '/client.js') {
    res.writeHead(200, { 'content-type': 'text/javascript; charset=utf-8' })
    return fs.createReadStream(path.join(__dirname, 'client.js')).pipe(res)
  }
  if (req.method === 'GET' && url.pathname === '/assets/storage-crystal.png') {
    res.writeHead(200, { 'content-type': 'image/png', 'cache-control': 'no-cache' })
    return fs.createReadStream(path.join(__dirname, 'assets', 'storage-crystal.png')).pipe(res)
  }

  // --- scan: stream events as Server-Sent Events ---
  if (req.method === 'GET' && url.pathname === '/scan') {
    const roots = url.searchParams.getAll('path').filter((p) => p.trim())
    if (roots.length === 0) roots.push('.')
    const patterns = url.searchParams.get('patterns') || ''
    if (roots.length > MAX_SCAN_ROOTS) {
      res.writeHead(400, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ error: 'too many scan paths' }))
      return
    }
    if (roots.some((root) => Buffer.byteLength(root) > MAX_PATH_BYTES)) {
      res.writeHead(400, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ error: 'scan path is too long' }))
      return
    }
    // A path starting with `-` would be an attempt to smuggle a flag (e.g.
    // `--delete`) into the engine. engine.js already fences the paths behind a
    // `--` separator; this rejects the attempt outright rather than scanning it.
    if (roots.some((root) => root.startsWith('-'))) {
      res.writeHead(400, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ error: 'scan path may not start with "-"' }))
      return
    }
    if (Buffer.byteLength(patterns) > MAX_PATTERN_BYTES) {
      res.writeHead(400, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ error: 'pattern list is too long' }))
      return
    }
    let config
    try {
      config = temporaryPatternFile(patterns)
    } catch {
      res.writeHead(500, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ error: 'could not prepare pattern list' }))
      return
    }
    sseHead(res)
    let closed = false
    const { child, done } = engine.scan(bin, roots, (ev) => {
      if (!closed) res.write(`data: ${JSON.stringify(ev)}\n\n`)
    }, config?.file)
    track(child)
    // Client hit Stop (or navigated away): kill the scan and stop writing so a
    // mid-scan disconnect can't write-after-end and crash the server.
    req.on('close', () => {
      closed = true
      child.kill()
    })
    done.then(({ code, stderr }) => {
      removeTemporaryPatternFile(config)
      if (closed) return
      res.write(`event: done\ndata: ${JSON.stringify({ code, stderr })}\n\n`)
      res.end()
    })
    return
  }

  // --- delete: POST a JSON array of paths, get the events back ---
  if (req.method === 'POST' && url.pathname === '/delete') {
    let body = ''
    let bodyBytes = 0
    let tooLarge = false
    req.on('data', (c) => {
      if (tooLarge) return
      bodyBytes += c.length
      if (bodyBytes > MAX_BODY_BYTES) {
        tooLarge = true
        res.writeHead(413, { 'content-type': 'application/json' })
        res.end(JSON.stringify({ error: 'request body is too large' }))
        return
      }
      body += c
    })
    req.on('end', () => {
      if (tooLarge) return
      let paths
      try {
        paths = JSON.parse(body)
      } catch {
        res.writeHead(400, { 'content-type': 'application/json' })
        res.end(JSON.stringify({ error: 'body must be a JSON path array' }))
        return
      }
      if (
        !Array.isArray(paths) ||
        paths.length > MAX_DELETE_PATHS ||
        paths.some((p) => typeof p !== 'string' || Buffer.byteLength(p) > MAX_PATH_BYTES)
      ) {
        res.writeHead(400, { 'content-type': 'application/json' })
        res.end(JSON.stringify({ error: 'invalid delete path array' }))
        return
      }
      const events = []
      const { child, done } = engine.delete(bin, paths, (ev) => events.push(ev))
      track(child)
      done.then(({ code }) => {
        res.writeHead(200, { 'content-type': 'application/json' })
        res.end(JSON.stringify({ code, events }))
      })
    })
    return
  }

  res.writeHead(404)
  res.end('not found')
})

function openBrowser(url) {
  const cmd = process.platform === 'darwin' ? 'open' : process.platform === 'win32' ? 'start' : 'xdg-open'
  try {
    require('node:child_process').spawn(cmd, [url], { stdio: 'ignore', detached: true }).unref()
  } catch {
    /* non-fatal: just open the URL yourself */
  }
}

// Kill any running scans/deletes if the server itself is stopped.
for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, () => {
    for (const c of active) c.kill()
    process.exit(0)
  })
}

server.listen(PORT, HOST, () => {
  const url = `http://${HOST}:${PORT}`
  console.log(`\n  buildcleaner demo → ${url}`)
  console.log(`  engine: ${bin}\n`)
  if (!process.env.BUILDCLEANER_NO_OPEN) openBrowser(url)
})
