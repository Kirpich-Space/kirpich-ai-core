'use strict';

const assert = require('node:assert/strict');
const { spawn } = require('node:child_process');
const http = require('node:http');
const path = require('node:path');
const { test } = require('node:test');

function listen(server) {
  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      server.removeListener('error', reject);
      resolve(server.address());
    });
  });
}

function close(server) {
  return new Promise((resolve) => server.close(resolve));
}

test('real Electron renderer reaches HTTP through preload, trusted IPC, and main', { timeout: 20_000 }, async () => {
  const token = 'electron-e2e-ephemeral-token';
  const requests = [];
  const server = http.createServer(async (request, response) => {
    let body = '';
    for await (const chunk of request) body += chunk;
    requests.push({
      authorization: request.headers.authorization,
      body: body || undefined,
      method: request.method,
      path: request.url,
    });
    if (request.headers.authorization !== `Bearer ${token}`) {
      response.writeHead(401);
      response.end('unauthorized');
      return;
    }
    response.writeHead(200, { 'Content-Type': 'application/json' });
    response.end('{"running_tasks":0}');
  });
  const address = await listen(server);
  const electron = require('electron');
  const child = spawn(
    'xvfb-run',
    ['-a', electron, '--no-sandbox', path.join(__dirname, 'e2e-main.js')],
    {
      cwd: path.join(__dirname, '..'),
      env: {
        ...process.env,
        ELECTRON_RUN_AS_NODE: '',
        KAIC_CONTROL_TOKEN: token,
        KAIC_E2E_API_BASE: `http://127.0.0.1:${address.port}`,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', (chunk) => { stdout += chunk; });
  child.stderr.on('data', (chunk) => { stderr += chunk; });

  try {
    const exit = await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('exit', (code, signal) => resolve({ code, signal }));
    });
    assert.deepEqual(exit, { code: 0, signal: null }, stderr);
    assert.deepEqual(requests, [{
      authorization: `Bearer ${token}`,
      body: undefined,
      method: 'GET',
      path: '/status',
    }]);

    const marker = stdout.split('\n').find((line) => line.startsWith('KAIC_E2E_RESULT '));
    assert.ok(marker, `missing renderer result; stderr=${stderr}`);
    const result = JSON.parse(marker.slice('KAIC_E2E_RESULT '.length));
    assert.deepEqual(result.status, { ok: true, value: { running_tasks: 0 } });
    assert.deepEqual(result.bridgeKeys, [
      'cancelTask',
      'continueTask',
      'createTask',
      'getStatus',
      'getTask',
      'listModels',
      'listTasks',
      'pauseTask',
      'setModelTemperature',
    ]);
    assert.equal(result.nodeIntegrationType, 'undefined');
    assert.equal(result.bridgeSerialized.includes(token), false);
    assert.equal(result.bridgeSerialized.includes(address.port.toString()), false);
    assert.equal(stdout.includes(token), false);
    assert.equal(stderr.includes(token), false);
  } finally {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    await close(server);
  }
});
