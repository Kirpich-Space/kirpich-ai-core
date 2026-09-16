'use strict';

const { app, BrowserWindow, ipcMain } = require('electron');
const path = require('node:path');
const {
  IPC_CHANNEL,
  createControlCenterHandler,
  createTrustedControlCenterHandler,
} = require('./control-center');

app.commandLine.appendSwitch('disable-gpu');

app.whenReady().then(async () => {
  const window = new BrowserWindow({
    show: false,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  const handleControlCenter = createControlCenterHandler({
    apiBase: process.env.KAIC_E2E_API_BASE,
    token: process.env.KAIC_CONTROL_TOKEN,
  });
  const isTrustedTestRenderer = (event) => (
    event.sender === window.webContents
      && event.senderFrame === window.webContents.mainFrame
      && event.senderFrame.url.startsWith('data:text/html')
  );
  ipcMain.handle(
    IPC_CHANNEL,
    createTrustedControlCenterHandler(handleControlCenter, isTrustedTestRenderer),
  );

  const rendererCode = `
    window.e2eResultPromise = (async () => ({
      bridgeKeys: Object.keys(window.kaicControlCenter).sort(),
      bridgeSerialized: JSON.stringify(window.kaicControlCenter),
      nodeIntegrationType: typeof require,
      status: await window.kaicControlCenter.getStatus(),
    }))();
  `;
  const html = `<html><body><script>${rendererCode}</script></body></html>`;
  await window.loadURL(`data:text/html;charset=utf-8,${encodeURIComponent(html)}`);
  const result = await window.webContents.executeJavaScript('window.e2eResultPromise');
  process.stdout.write(`KAIC_E2E_RESULT ${JSON.stringify(result)}\n`);
  window.destroy();
  app.quit();
}).catch((error) => {
  const safeMessage = String(error && error.message ? error.message : 'unknown error')
    .split(process.env.KAIC_CONTROL_TOKEN || '').join('[REDACTED]')
    .split(process.env.KAIC_E2E_API_BASE || '').join('[REDACTED]');
  process.stderr.write(`KAIC_E2E_FAILED ${safeMessage}\n`);
  app.exit(1);
});
