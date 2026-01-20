// The only bridge between renderer and main. Exposes a tiny, explicit API.
const { contextBridge, ipcRenderer } = require('electron')

contextBridge.exposeInMainWorld('bc', {
  pickFolder: () => ipcRenderer.invoke('bc:pick-folder'),
  scan: (root) => ipcRenderer.invoke('bc:scan', root),
  delete: (paths) => ipcRenderer.invoke('bc:delete', paths),
  restore: () => ipcRenderer.invoke('bc:restore'),
  onEvent: (cb) => ipcRenderer.on('bc:event', (_e, ev) => cb(ev)),
  onDone: (cb) => ipcRenderer.on('bc:done', (_e, info) => cb(info)),
})
