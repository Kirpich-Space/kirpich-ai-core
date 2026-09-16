const assert = require('node:assert/strict');
const http = require('node:http');
const { afterEach, test } = require('node:test');

const {
  ControlCenterRequestError,
  createControlCenterClient,
  createControlCenterHandler,
} = require('./control-center');

const servers = [];

afterEach(async () => {
  await Promise.all(servers.splice(0).map((server) => new Promise((resolve) => server.close(resolve))));
});

async function listen(handler) {
  const server = http.createServer(handler);
  servers.push(server);
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  return `http://127.0.0.1:${address.port}`;
}

const operationCases = [
  { name: 'listTasks', args: [], method: 'GET', path: '/tasks', body: undefined, response: '[]', value: [] },
  {
    name: 'createTask',
    args: [{ text: 'hello', allow_manual: false, model: 'Fable9B' }],
    method: 'POST',
    path: '/tasks',
    body: '{"text":"hello","allow_manual":false,"model":"Fable9B"}',
    response: '{"id":"created"}',
    value: { id: 'created' },
  },
  { name: 'getTask', args: ['abc'], method: 'GET', path: '/tasks/abc', body: undefined, response: '{"id":"abc"}', value: { id: 'abc' } },
  {
    name: 'continueTask',
    args: ['abc', { message: 'more' }],
    method: 'POST',
    path: '/tasks/abc/continue',
    body: '{"message":"more"}',
    response: '',
    value: undefined,
  },
  { name: 'pauseTask', args: ['abc'], method: 'POST', path: '/tasks/abc/pause', body: undefined, response: '', value: undefined },
  { name: 'cancelTask', args: ['abc'], method: 'POST', path: '/tasks/abc/cancel', body: undefined, response: '', value: undefined },
  { name: 'listModels', args: [], method: 'GET', path: '/models', body: undefined, response: '[]', value: [] },
  { name: 'getStatus', args: [], method: 'GET', path: '/status', body: undefined, response: '{"running_tasks":0}', value: { running_tasks: 0 } },
  {
    name: 'setModelTemperature',
    args: ['model/name', 0.2],
    method: 'POST',
    path: '/models/model%2Fname/temperature',
    body: '{"temperature":0.2}',
    response: '[]',
    value: [],
  },
];

for (const operation of operationCases) {
  test(`${operation.name} has an independent exact authenticated HTTP contract`, async () => {
    const seen = [];
    const apiBase = await listen(async (request, response) => {
      let body = '';
      for await (const chunk of request) body += chunk;
      seen.push({
        authorization: request.headers.authorization,
        body: body || undefined,
        method: request.method,
        path: request.url,
      });
      response.writeHead(operation.response === '' ? 204 : 200, {
        'Content-Type': 'application/json',
      });
      response.end(operation.response);
    });
    const client = createControlCenterClient({ apiBase, token: 'matrix-ephemeral-token' });

    assert.deepEqual(await client.invoke(operation.name, operation.args), operation.value);
    assert.deepEqual(seen, [{
      authorization: 'Bearer matrix-ephemeral-token',
      body: operation.body,
      method: operation.method,
      path: operation.path,
    }]);
  });
}

test('missing runtime token fails closed before a request is sent', async () => {
  let requests = 0;
  const apiBase = await listen((_request, response) => {
    requests += 1;
    response.end('{}');
  });

  assert.throws(
    () => createControlCenterClient({ apiBase, token: '   ' }),
    /KAIC_CONTROL_TOKEN.*не задан/,
  );
  assert.equal(requests, 0);

  const handler = createControlCenterHandler({ apiBase, token: undefined });
  assert.deepEqual(await handler('listTasks', []), {
    ok: false,
    error: {
      kind: 'configuration',
      status: 0,
      message: 'KAIC_CONTROL_TOKEN не задан; Control Center заблокирован',
    },
  });
  assert.equal(requests, 0);
});

for (const status of [401, 500]) {
  test(`HTTP ${status} is a safe HTTP rejection without backend body or token`, async () => {
    const token = 'secret-that-must-not-leak';
    const apiBase = await listen((request, response) => {
      response.writeHead(status);
      response.end(`private backend detail ${request.headers.authorization}`);
    });
    const handler = createControlCenterHandler({ apiBase, token });

    assert.deepEqual(await handler('listTasks', []), {
      ok: false,
      error: {
        kind: 'http',
        status,
        message: `Control Center API отклонил запрос (HTTP ${status})`,
      },
    });
  });
}

test('network failure is classified as transport failure', async () => {
  const token = 'secret-that-must-not-leak';
  const apiBase = await listen((_request, response) => response.end('{}'));
  const server = servers.pop();
  await new Promise((resolve) => server.close(resolve));
  const handler = createControlCenterHandler({ apiBase, token });

  assert.deepEqual(await handler('getStatus', []), {
    ok: false,
    error: {
      kind: 'transport',
      status: 0,
      message: 'Control Center API недоступен (backend не запущен?)',
    },
  });
});

test('malformed JSON is a protocol failure rather than a network failure', async () => {
  const apiBase = await listen((_request, response) => {
    response.writeHead(200, { 'Content-Type': 'application/json' });
    response.end('{not json');
  });
  const handler = createControlCenterHandler({ apiBase, token: 'protocol-ephemeral-token' });

  assert.deepEqual(await handler('getStatus', []), {
    ok: false,
    error: {
      kind: 'protocol',
      status: 200,
      message: 'Control Center API вернул некорректный ответ',
    },
  });
});

test('malformed privileged calls are rejected before HTTP', async () => {
  let requests = 0;
  const apiBase = await listen((_request, response) => {
    requests += 1;
    response.end('{}');
  });
  const handler = createControlCenterHandler({ apiBase, token: 'boundary-ephemeral-token' });
  const malformed = [
    ['listTasks', {}],
    ['listTasks', ['extra']],
    ['getTask', []],
    ['getTask', [17]],
    ['createTask', [null]],
    ['createTask', [{ text: 'hello', model: '  ' }]],
    ['continueTask', ['abc', null]],
    ['setModelTemperature', ['model', Number.NaN]],
    ['setModelTemperature', ['model', 3]],
    ['request', ['https://evil.example']],
  ];

  for (const [operation, args] of malformed) {
    const result = await handler(operation, args);
    assert.equal(result.ok, false, `${operation} unexpectedly passed`);
    assert.equal(result.error.kind, 'boundary', `${operation} has wrong error kind`);
    assert.equal(result.error.status, 0);
    assert.equal(result.error.message, 'Некорректный вызов Control Center');
  }
  assert.equal(requests, 0);
});

test('renderer cannot choose an arbitrary operation or destination', async () => {
  let requests = 0;
  const apiBase = await listen((_request, response) => {
    requests += 1;
    response.end('{}');
  });
  const client = createControlCenterClient({ apiBase, token: 'ephemeral-control-token' });

  await assert.rejects(client.invoke('request', ['https://evil.example/']), (error) => {
    assert.ok(error instanceof ControlCenterRequestError);
    assert.equal(error.kind, 'boundary');
    assert.equal(error.message, 'Некорректный вызов Control Center');
    return true;
  });
  assert.equal(requests, 0);
});

test('Linux Electron boundary smoke rejects anonymous access and accepts its read-only request', async () => {
  const token = 'linux-smoke-ephemeral-token';
  const apiBase = await listen((request, response) => {
    if (request.headers.authorization !== `Bearer ${token}`) {
      response.writeHead(401);
      response.end('missing or invalid bearer');
      return;
    }
    response.writeHead(200, { 'Content-Type': 'application/json' });
    response.end('{"running_tasks":0}');
  });

  const anonymous = await fetch(`${apiBase}/status`);
  assert.equal(anonymous.status, 401);

  const client = createControlCenterClient({ apiBase, token });
  assert.deepEqual(await client.invoke('getStatus', []), { running_tasks: 0 });
});
