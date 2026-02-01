// Browser client: same live table as the Electron renderer, but driven over
// SSE (scan) and fetch (delete) instead of Electron IPC.

const $ = (id) => document.getElementById(id)
const PATTERN_STORAGE_KEY = 'buildcleaner.scan-patterns'
const MAX_PATTERN_BYTES = 16 * 1024
const rows = new Map() // path -> { tr, checkbox, bytes, category, deleted }
let sort = { key: null, direction: 'ascending' }
// The folders to scan. `null` means "fall back to the single path typed in the
// input"; an array is an explicit set of dropped/picked folders.
let scanRoots = null

function human(bytes) {
  if (bytes < 1024) return `${bytes} B`
  const u = ['KB', 'MB', 'GB', 'TB', 'PB']
  let b = bytes,
    i = -1
  do {
    b /= 1024
    i++
  } while (b >= 1024 && i < u.length - 1)
  return `${b.toFixed(1)} ${u[i]}`
}

function setStats({ dirs, matches, size, elapsed }) {
  if (dirs != null) $('s-dirs').textContent = dirs.toLocaleString()
  if (matches != null) $('s-matches').textContent = matches.toLocaleString()
  if (size != null) {
    const formatted = human(size)
    $('s-size').textContent = formatted
    $('hero-size').textContent = formatted
  }
  if (elapsed != null) $('s-elapsed').textContent = `${elapsed.toFixed(1)}s`
}

function refreshSelection() {
  let count = 0,
    bytes = 0
  for (const r of rows.values()) {
    if (!r.deleted && r.checkbox.checked) {
      count++
      bytes += r.bytes
    }
  }
  $('del').disabled = count === 0
  $('selinfo').textContent = count ? `${count} selected · ${human(bytes)}` : 'nothing selected'
}

function addMatch(ev) {
  if (rows.has(ev.path)) return
  document.body.classList.add('has-results')
  $('empty').hidden = true
  const tr = document.createElement('tr')

  const tdChk = document.createElement('td')
  const chk = document.createElement('input')
  chk.type = 'checkbox'
  chk.addEventListener('change', refreshSelection)
  tdChk.appendChild(chk)

  const tdSize = document.createElement('td')
  tdSize.className = 'size'
  tdSize.textContent = human(ev.bytes)

  const tdCat = document.createElement('td')
  tdCat.className = 'cat'
  const category = document.createElement('span')
  category.className = 'category'
  category.textContent = ev.category
  category.style.setProperty('--chip-hue', 198 + ([...String(ev.category)].reduce((total, char) => total + char.charCodeAt(0), 0) % 67))
  tdCat.appendChild(category)

  const tdPath = document.createElement('td')
  tdPath.className = 'path'
  tdPath.textContent = ev.path

  tr.append(tdChk, tdSize, tdCat, tdPath)
  $('rows').appendChild(tr)
  rows.set(ev.path, { tr, checkbox: chk, bytes: ev.bytes, category: ev.category, deleted: false })
  applySort()
}

function applySort() {
  if (!sort.key) return
  const multiplier = sort.direction === 'ascending' ? 1 : -1
  const ordered = [...rows.values()].sort((a, b) => {
    const left = a[sort.key]
    const right = b[sort.key]
    return typeof left === 'number'
      ? multiplier * (left - right)
      : multiplier * String(left).localeCompare(String(right), undefined, { numeric: true })
  })
  for (const row of ordered) $('rows').appendChild(row.tr)
}

function setSort(key) {
  sort = {
    key,
    direction: sort.key === key && sort.direction === 'ascending' ? 'descending' : 'ascending',
  }
  document.querySelectorAll('[data-sort]').forEach((button) => {
    button.setAttribute('aria-sort', button.dataset.sort === key ? sort.direction : 'none')
  })
  applySort()
}

function setDefaultSort() {
  sort = { key: 'bytes', direction: 'descending' }
  document.querySelectorAll('[data-sort]').forEach((button) => {
    button.setAttribute('aria-sort', button.dataset.sort === 'bytes' ? 'descending' : 'none')
  })
}

function markDeleted(path) {
  const r = rows.get(path)
  if (!r) return
  r.deleted = true
  r.checkbox.checked = false
  r.checkbox.disabled = true
  r.tr.querySelector('.path').classList.add('gone')
  refreshSelection()
}

let matchCount = 0
function applyEvent(ev) {
  switch (ev.event) {
    case 'start':
      if (ev.input) break
      rows.clear()
      $('rows').innerHTML = ''
      setDefaultSort()
      document.body.classList.remove('has-results')
      $('empty').hidden = false
      matchCount = 0
      setStats({ dirs: 0, matches: 0, size: 0, elapsed: 0 })
      $('status').textContent = 'scanning…'
      break
    case 'progress':
      setStats({ dirs: ev.scanned, matches: ev.matches, size: ev.bytes, elapsed: ev.elapsed_ms / 1000 })
      break
    case 'match':
      addMatch(ev)
      matchCount++
      setStats({ matches: matchCount })
      break
    case 'summary':
      setStats({ dirs: ev.scanned, matches: ev.count, size: ev.total_bytes, elapsed: ev.elapsed_secs })
      $('status').textContent = `done · ${ev.count} reclaimable`
      if (ev.count === 0) {
        document.body.classList.remove('has-results')
        $('empty').hidden = false
      }
      break
    case 'deleted':
      markDeleted(ev.path)
      break
    case 'delete_error':
      $('status').textContent = `delete rejected: ${ev.path} (${ev.error})`
      break
    case 'delete_summary':
      $('status').textContent = `deleted ${ev.deleted}, freed ${human(ev.freed_bytes)}${ev.failed ? ` · ${ev.failed} skipped` : ''}`
      break
  }
}

// ---- controls ----
const scanBtn = $('scan')
let es = null
let scanning = false

function setScanning(on) {
  scanning = on
  document.body.classList.toggle('is-scanning', on)
  scanBtn.textContent = on ? 'Stop scan' : 'Scan space'
  scanBtn.classList.toggle('primary', !on)
  scanBtn.classList.toggle('danger', on)
}

// The authoritative set of roots to scan: an explicit dropped/picked list when
// present, otherwise the single path typed into the input.
function currentRoots() {
  if (scanRoots && scanRoots.length) return scanRoots
  const typed = $('path').value.trim()
  return typed ? [typed] : []
}

function scan() {
  const roots = currentRoots()
  if (!roots.length) {
    $('status').textContent = 'enter a path first'
    return
  }
  if (es) es.close()
  rows.clear()
  $('rows').innerHTML = ''
  setDefaultSort()
  document.body.classList.remove('has-results')
  $('empty').hidden = false
  matchCount = 0
  setStats({ dirs: 0, matches: 0, size: 0, elapsed: 0 })
  $('status').textContent = roots.length > 1 ? `scanning ${roots.length} folders…` : 'scanning…'
  setScanning(true)
  const patterns = localStorage.getItem(PATTERN_STORAGE_KEY) || ''
  const scanUrl = new URL('/scan', window.location.href)
  for (const root of roots) scanUrl.searchParams.append('path', root)
  if (patterns.trim()) scanUrl.searchParams.set('patterns', patterns)
  es = new EventSource(scanUrl)
  es.onmessage = (e) => applyEvent(JSON.parse(e.data))
  es.addEventListener('done', () => {
    if (es) es.close()
    es = null
    setScanning(false)
  })
  es.onerror = () => {
    if (es) es.close()
    es = null
    setScanning(false)
    if (matchCount === 0) $('status').textContent = 'connection lost'
  }
}

async function chooseFolder() {
  try {
    // Use a real root, never the input's text: with several folders selected the
    // input holds a "<path>  +N more" summary, which is not a openable path.
    const currentPath = currentRoots()[0] || ''
    const paths = await window.zero.invoke('native-sdk.dialog.openFile', {
      title: 'Choose folders to scan',
      ...(currentPath.startsWith('/') ? { defaultPath: currentPath } : {}),
      allowDirectories: true,
      allowMultiple: true,
    })
    if (Array.isArray(paths) && paths.length) {
      setRoots(paths)
      scan()
    } else {
      $('status').textContent = 'folder selection cancelled'
    }
  } catch (error) {
    $('status').textContent = `folder picker failed: ${error.message || error.code || 'unknown error'}`
  }
}

// Record an explicit set of folders and mirror it into the input: the exact path
// when there's one, a compact summary (full list on hover) when there are several.
function setRoots(paths) {
  scanRoots = paths.length ? paths : null
  if (!scanRoots) {
    $('path').removeAttribute('title')
    return
  }
  $('path').value = paths.length === 1 ? paths[0] : `${paths[0]}  +${paths.length - 1} more`
  $('path').title = paths.join('\n')
}

// Every file:// entry in a Finder drop. text/uri-list is CRLF-separated and may
// carry `#` comment lines, so filter to real file URIs and decode each.
function droppedPaths(event) {
  const paths = []
  for (const line of event.dataTransfer.getData('text/uri-list').split('\n')) {
    const value = line.trim()
    if (!value.startsWith('file://')) continue
    try {
      paths.push(decodeURIComponent(new URL(value).pathname))
    } catch {
      /* skip a malformed URI rather than failing the whole drop */
    }
  }
  return paths
}

// Closing the EventSource drops the HTTP connection, which fires the server's
// req 'close' handler and kills the buildcleaner process mid-scan.
function stop() {
  if (es) es.close()
  es = null
  setScanning(false)
  $('status').textContent = `stopped · ${matchCount} found so far`
}

async function del() {
  const paths = []
  for (const [path, r] of rows.entries()) {
    if (!r.deleted && r.checkbox.checked) paths.push(path)
  }
  if (!paths.length) return
  if (!window.confirm(`Delete ${paths.length} selected directories?`)) return
  $('status').textContent = `deleting ${paths.length}…`
  const r = await fetch('/delete', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(paths),
  })
  const { events } = await r.json()
  events.forEach(applyEvent)
}

scanBtn.addEventListener('click', () => (scanning ? stop() : scan()))
$('path').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !scanning) scan()
})
// Typing a path by hand takes over from a dropped/picked folder set.
$('path').addEventListener('input', () => setRoots([]))
$('choose').addEventListener('click', chooseFolder)
if (window.zero?.invoke) $('choose').hidden = false
const patternDialog = $('pattern-dialog')
$('settings').addEventListener('click', () => {
  $('patterns').value = localStorage.getItem(PATTERN_STORAGE_KEY) || ''
  patternDialog.showModal()
})
$('reset-patterns').addEventListener('click', () => {
  $('patterns').value = ''
})
$('save-patterns').addEventListener('click', () => {
  const patterns = $('patterns').value.trim()
  if (new TextEncoder().encode(patterns).byteLength > MAX_PATTERN_BYTES) {
    $('status').textContent = 'pattern list is too long'
    return
  }
  localStorage.setItem(PATTERN_STORAGE_KEY, patterns)
  patternDialog.close()
  $('status').textContent = patterns ? 'patterns saved for the next scan' : 'using the built-in pattern list'
})
// A drop target: highlight on drag-over, scan the dropped folder(s) on drop.
function wireDropZone(el) {
  if (!el) return
  for (const eventName of ['dragenter', 'dragover']) {
    el.addEventListener(eventName, (event) => {
      event.preventDefault()
      el.classList.add('dragging')
    })
  }
  // dragleave also fires when the pointer moves onto a CHILD of the zone (the
  // event bubbles up), which would flicker the highlight off and on. relatedTarget
  // is the element being entered, so ignore leaves that land inside the zone.
  el.addEventListener('dragleave', (event) => {
    if (event.relatedTarget && el.contains(event.relatedTarget)) return
    el.classList.remove('dragging')
  })
  el.addEventListener('drop', (event) => {
    event.preventDefault()
    el.classList.remove('dragging')
    const paths = droppedPaths(event)
    if (paths.length) {
      setRoots(paths)
      scan()
    } else {
      $('status').textContent = 'drop one or more folders from Finder to scan them'
    }
  })
}
wireDropZone(document.querySelector('header'))
wireDropZone(document.querySelector('.scanbox'))
$('all').addEventListener('change', (e) => {
  for (const r of rows.values()) if (!r.deleted) r.checkbox.checked = e.target.checked
  refreshSelection()
})
$('del').addEventListener('click', del)
document.querySelectorAll('[data-sort]').forEach((button) => {
  button.addEventListener('click', () => setSort(button.dataset.sort))
})
