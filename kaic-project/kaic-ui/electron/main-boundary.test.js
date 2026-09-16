'use strict';

const assert = require('node:assert/strict');
const Module = require('node:module');
const { test } = require('node:test');

test('an untrusted navigated frame cannot invoke the privileged HTTP bridge', async () => {
  let ipcHandler;
  let httpRequests = 0;
  const originalLoad = Module._load;
  const originalFetch = globalThis.fetch;
  const originalToken = process.env.KAIC_CONTROL_TOKEN;

  Module._load = function fakeElectron(request, parent, isMain) {
    if (request === 'electron') {
      return {
        app: {
          isPackaged: false,
          on() {},
          quit() {},
          whenReady() { return { then() {} }; },
        },
        BrowserWindow: function BrowserWindow() {},
        ipcMain: {
          handle(_channel, handler) { ipcHandler = handler; },
        },
      };
    }
    return originalLoad.call(this, request, parent, isMain);
  };
  globalThis.fetch = async () => {
    httpRequests += 1;
    return new Response('{"running_tasks":0}', { status: 200 });
  };
  process.env.KAIC_CONTROL_TOKEN = 'sender-boundary-token';

  try {
    delete require.cache[require.resolve('./main')];
    require('./main');
    const result = await ipcHandler(
      {
        sender: { id: 1 },
        senderFrame: { url: 'https://evil.example/', parent: null },
      },
      'getStatus',
      [],
    );
    assert.deepEqual(result, {
      ok: false,
      error: {
        kind: 'boundary',
        status: 0,
        message: 'Недоверенный renderer Control Center',
      },
    });
    assert.equal(httpRequests, 0);
  } finally {
    Module._load = originalLoad;
    globalThis.fetch = originalFetch;
    if (originalToken === undefined) delete process.env.KAIC_CONTROL_TOKEN;
    else process.env.KAIC_CONTROL_TOKEN = originalToken;
    delete require.cache[require.resolve('./main')];
  }
});

test('the Control Center window blocks external navigation and all child windows', () => {
  let willNavigate;
  let windowOpenHandler;
  const originalLoad = Module._load;
  const originalToken = process.env.KAIC_CONTROL_TOKEN;
  const webContents = {
    mainFrame: {},
    on(name, handler) {
      if (name === 'will-navigate') willNavigate = handler;
    },
    openDevTools() {},
    setWindowOpenHandler(handler) { windowOpenHandler = handler; },
  };

  Module._load = function fakeElectron(request, parent, isMain) {
    if (request === 'electron') {
      return {
        app: {
          isPackaged: false,
          on() {},
          quit() {},
          whenReady() { return { then(callback) { callback(); } }; },
        },
        BrowserWindow: function BrowserWindow() {
          return { loadURL() {}, webContents };
        },
        ipcMain: { handle() {} },
      };
    }
    return originalLoad.call(this, request, parent, isMain);
  };
  process.env.KAIC_CONTROL_TOKEN = 'navigation-boundary-token';

  try {
    delete require.cache[require.resolve('./main')];
    require('./main');
    assert.equal(typeof willNavigate, 'function');
    assert.equal(typeof windowOpenHandler, 'function');

    let blocked = false;
    willNavigate({ preventDefault() { blocked = true; } }, 'https://evil.example/');
    assert.equal(blocked, true);

    blocked = false;
    willNavigate({ preventDefault() { blocked = true; } }, 'http://localhost:5173/task/abc');
    assert.equal(blocked, false);
    assert.deepEqual(windowOpenHandler({ url: 'https://evil.example/' }), { action: 'deny' });
  } finally {
    Module._load = originalLoad;
    if (originalToken === undefined) delete process.env.KAIC_CONTROL_TOKEN;
    else process.env.KAIC_CONTROL_TOKEN = originalToken;
    delete require.cache[require.resolve('./main')];
  }
});
