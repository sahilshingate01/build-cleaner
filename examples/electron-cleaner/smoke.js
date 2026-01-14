// Headless proof that engine.js can drive the buildcleaner binary — no GUI/display.
// Usage: node smoke.js [path-to-scan]
const assert = require('node:assert/strict')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const engine = require('./engine')

function run(operation) {
  return new Promise((resolve, reject) => {
    const events = []
    const { done } = operation((event) => events.push(event))
    done.then((result) => resolve({ ...result, events }), reject)
  })
}

async function main() {
  const bin = engine.resolveBin()
  const root = process.argv[2] || '.'
  console.log('bin :', bin)
  console.log('scan:', root, '\n')

  const scan = await run((onEvent) => engine.scan(bin, root, onEvent))
  assert.equal(scan.code, 0, scan.stderr)
  assert.ok(scan.events.some((event) => event.event === 'start'), 'missing start event')
  assert.ok(scan.events.some((event) => event.event === 'summary'), 'missing summary event')

  const fixture = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'buildcleaner-smoke-')))
  try {
    const target = path.join(fixture, 'node_modules')
    const rejected = path.join(fixture, 'not-a-target')
    fs.mkdirSync(target)
    fs.mkdirSync(rejected)
    fs.writeFileSync(path.join(target, 'fixture'), 'x')

    const fixtureScan = await run((onEvent) => engine.scan(bin, fixture, onEvent))
    assert.equal(fixtureScan.code, 0, fixtureScan.stderr)
    assert.ok(fixtureScan.events.some((event) => event.event === 'match' && event.path === target))

    const deletion = await run((onEvent) => engine.delete(bin, [target, rejected], onEvent))
    assert.equal(deletion.code, 0, deletion.stderr)
    assert.ok(deletion.events.some((event) => event.event === 'deleted' && event.path === target))
    assert.ok(deletion.events.some((event) => event.event === 'delete_error' && event.path === rejected))
    assert.ok(deletion.events.some((event) => event.event === 'delete_summary'))
    assert.ok(!fs.existsSync(target), 'recognized target was not deleted')
    assert.ok(fs.existsSync(rejected), 'unrecognized directory was deleted')
  } finally {
    fs.rmSync(fixture, { recursive: true, force: true })
  }

  console.log('smoke passed')
}

main().catch((error) => {
  console.error(error.stack || error)
  process.exit(1)
})
