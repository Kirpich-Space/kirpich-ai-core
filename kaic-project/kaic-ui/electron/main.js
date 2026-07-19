// Главный процесс Electron. Единственная задача — открыть одно окно
// с интерфейсом (по архитектуре KAIC Control Center: одно окно ОС,
// без плодения дополнительных BrowserWindow — детали задачи и модалки
// живут как состояние внутри этого же окна, не как отдельные окна).
//
// Обычный CommonJS JS, не TypeScript: файл маленький (меньше 30 строк),
// заводить для него отдельный шаг компиляции ради типов — по принципу
// "50 строк вместо 500" не оправдано. TypeScript живёт там, где реальная
// сложность — в React-коде renderer'а (см. src/).

const { app, BrowserWindow } = require('electron');
const path = require('node:path');

// app.isPackaged — true только для собранного приложения. В dev-режиме
// грузим Vite dev-сервер (быстрый hot-reload вместо пересборки).
const isDev = !app.isPackaged;

function createWindow() {
  const window = new BrowserWindow({
    width: 1280,
    height: 800,
    webPreferences: {
      // Renderer общается с backend напрямую через fetch() к локальному
      // HTTP API (см. control_center_api.rs) — никакого IPC/preload-моста
      // сейчас не нужно, поэтому Node-интеграция в renderer'е отключена
      // по умолчанию (безопасная настройка Electron).
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  if (isDev) {
    window.loadURL('http://localhost:5173');
    window.webContents.openDevTools();
  } else {
    window.loadFile(path.join(__dirname, '../dist/index.html'));
  }
}

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
