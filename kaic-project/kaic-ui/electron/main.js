// Главный процесс Electron. Единственная задача — открыть одно окно
// с интерфейсом (по архитектуре KAIC Control Center: одно окно ОС,
// без плодения дополнительных BrowserWindow — детали задачи и модалки
// живут как состояние внутри этого же окна, не как отдельные окна).
//
// Обычный CommonJS JS, не TypeScript: файл маленький (меньше 30 строк),
// заводить для него отдельный шаг компиляции ради типов — по принципу
// "50 строк вместо 500" не оправдано. TypeScript живёт там, где реальная
// сложность — в React-коде renderer'а (см. src/).

const { app, BrowserWindow, ipcMain } = require('electron');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
const {
  IPC_CHANNEL,
  createControlCenterHandler,
  createTrustedControlCenterHandler,
} = require('./control-center');

// app.isPackaged — true только для собранного приложения. В dev-режиме
// грузим Vite dev-сервер (быстрый hot-reload вместо пересборки).
const isDev = !app.isPackaged;
const rendererEntry = isDev
  ? 'http://localhost:5173/'
  : pathToFileURL(path.join(__dirname, '../dist/index.html')).href;
let controlWindow = null;

function isTrustedRendererUrl(candidate) {
  try {
    const actual = new URL(candidate);
    const expected = new URL(rendererEntry);
    if (isDev) return actual.origin === expected.origin;
    return actual.protocol === 'file:' && actual.pathname === expected.pathname;
  } catch {
    return false;
  }
}

function isTrustedRenderer(event) {
  return controlWindow !== null
    && event.sender === controlWindow.webContents
    && event.senderFrame === controlWindow.webContents.mainFrame
    && isTrustedRendererUrl(event.senderFrame.url);
}

function createWindow() {
  const window = new BrowserWindow({
    width: 1280,
    height: 800,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });
  controlWindow = window;

  window.webContents.on('will-navigate', (event, url) => {
    if (!isTrustedRendererUrl(url)) event.preventDefault();
  });
  window.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));

  if (isDev) {
    window.loadURL('http://localhost:5173');
    window.webContents.openDevTools();
  } else {
    window.loadFile(path.join(__dirname, '../dist/index.html'));
  }
}

const handleControlCenter = createControlCenterHandler({
  token: process.env.KAIC_CONTROL_TOKEN,
});
ipcMain.handle(
  IPC_CHANNEL,
  createTrustedControlCenterHandler(handleControlCenter, isTrustedRenderer),
);

app.whenReady().then(createWindow);

app.on('window-all-closed', () => {
  // На macOS принято оставлять процесс приложения живым после закрытия
  // окна — но KAIC Control Center не претендует на мультиплатформенный
  // полноценный UX прямо сейчас, простое поведение "закрыли окно — вышли"
  // одинаково для всех платформ пока вполне достаточно.
  if (process.platform !== 'darwin') {
    app.quit();
  }
});
