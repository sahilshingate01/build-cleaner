// Renderer: turns the engine's JSON events into a live table + stats.
// Knows nothing about subprocesses — it only calls window.bc and reacts to events.

const $ = (id) => document.getElementById(id)
const rows = new Map() // path -> { tr, checkbox, bytes, category, deleted }
let sort = { key: null, direction: 'ascending' }

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
  if (size != null) $('s-size').textContent = human(size)
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

// ---- event stream from the engine (via main process) ----
let matchCount = 0
function applyEvent(ev) {
  switch (ev.event) {
    case 'start':
      if (ev.input) break // delete run starting; keep the table
      rows.clear()
      $('rows').innerHTML = ''
      setDefaultSort()
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

window.bc.onEvent(applyEvent)

window.bc.onDone((info) => {
  if (info.kind === 'scan' && info.code !== 0) {
    $('status').textContent = `scan exited ${info.code}: ${(info.stderr || '').trim()}`
  }
})

// ---- controls ----
$('choose').addEventListener('click', async () => {
  const p = await window.bc.pickFolder()
  if (p) $('path').value = p
})

$('scan').addEventListener('click', () => {
  const root = $('path').value.trim()
  if (!root) {
    $('status').textContent = 'enter a path first'
    return
  }
  window.bc.scan(root)
})

$('path').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') $('scan').click()
})

$('all').addEventListener('change', (e) => {
  for (const r of rows.values()) {
    if (!r.deleted) r.checkbox.checked = e.target.checked
  }
  refreshSelection()
})

$('del').addEventListener('click', () => {
  const paths = []
  for (const [path, r] of rows.entries()) {
    if (!r.deleted && r.checkbox.checked) paths.push(path)
  }
  if (!paths.length) return
  if (!window.confirm(`Delete ${paths.length} selected directories?`)) return
  $('status').textContent = `deleting ${paths.length}…`
  window.bc.delete(paths)
})
document.querySelectorAll('[data-sort]').forEach((button) => {
  button.addEventListener('click', () => setSort(button.dataset.sort))
})

// Main retains the current session so a hot reload restores its table and stats.
window.bc.restore().then(({ root, events }) => {
  if (root) $('path').value = root
  events.forEach(applyEvent)
})
