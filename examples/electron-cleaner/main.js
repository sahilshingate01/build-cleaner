// Electron main process: owns the window and the buildcleaner subprocess.
// The renderer never spawns anything — it talks to us over IPC, we stream engine
// events back. contextIsolation on, nodeIntegration off.
const { app, BrowserWindow, ipcMain, dialog } = require('electron')
const path = require('node:path')
const engine = require('./engine')

const BIN = engine.resolveBin()
let win = null
let lastRoot = ''
let sessionEvents = []

function forwardEvent(sender, event) {
  // A fresh scan replaces the prior result set. Delete events extend the current
  // result set so a renderer reload can restore both rows and their deleted state.
  if (event.event === 'start' && !event.input) sessionEvents = []
  sessionEvents.push(event)
  sender.send('bc:event', event)
}

function createWindow() {
  win = new BrowserWindow({
    width: 960,
    height: 680,
    backgroundColor: '#0b0e14',
    titleBarStyle: 'hiddenInset',
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
    },
  })
  win.loadFile('index.html')
}

app.whenReady().then(() => {
  createWindow()
  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow()
  })
})

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit()
})

// Folder picker.
ipcMain.handle('bc:pick-folder', async () => {
  const r = await dialog.showOpenDialog(win, { properties: ['openDirectory'] })
  return r.canceled ? null : r.filePaths[0]
})

// Scan: stream every event to the renderer as it arrives.
ipcMain.handle('bc:scan', async (evt, root) => {
  lastRoot = root
  const { done } = engine.scan(BIN, root, (ev) => forwardEvent(evt.sender, ev))
  const { code, stderr } = await done
  evt.sender.send('bc:done', { kind: 'scan', code, stderr })
})

// Delete the selected paths (engine re-validates each one).
ipcMain.handle('bc:delete', async (evt, paths) => {
  const { done } = engine.delete(BIN, paths, (ev) => forwardEvent(evt.sender, ev))
  const { code, stderr } = await done
  evt.sender.send('bc:done', { kind: 'delete', code, stderr })
})

// The renderer can reload during development without losing the current scan.
ipcMain.handle('bc:restore', () => ({ root: lastRoot, events: sessionEvents }))
