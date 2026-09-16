'use strict';

const API_BASE = 'http://127.0.0.1:4545';
const IPC_CHANNEL = 'kaic:control-center';
const MISSING_TOKEN_MESSAGE = 'KAIC_CONTROL_TOKEN не задан; Control Center заблокирован';
const NETWORK_ERROR_MESSAGE = 'Control Center API недоступен (backend не запущен?)';
const BOUNDARY_ERROR_MESSAGE = 'Некорректный вызов Control Center';
const PROTOCOL_ERROR_MESSAGE = 'Control Center API вернул некорректный ответ';

class ControlCenterRequestError extends Error {
  constructor(kind, status, message) {
    super(message);
    this.name = 'ControlCenterRequestError';
    this.kind = kind;
    this.status = status;
  }
}

function boundaryError() {
  return new ControlCenterRequestError('boundary', 0, BOUNDARY_ERROR_MESSAGE);
}

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isNonEmptyString(value) {
  return typeof value === 'string' && value.trim().length > 0;
}

function hasValidOptionalFields(value, fields) {
  return Object.entries(fields).every(([name, predicate]) => (
    value[name] === undefined || predicate(value[name])
  ));
}

function defineOperation(arity, validate, response, build) {
  return Object.freeze({ arity, build, response, validate });
}

const operations = Object.freeze({
  listTasks: defineOperation(0, () => true, 'json', () => ['/tasks']),
  createTask: defineOperation(1, ([body]) => (
    isRecord(body)
      && typeof body.text === 'string'
      && hasValidOptionalFields(body, {
        allow_manual: (value) => typeof value === 'boolean',
        category: (value) => typeof value === 'string',
        model: (value) => typeof value === 'string' && value.trim().length > 0,
      })
  ), 'json', ([body]) => ['/tasks', { method: 'POST', body: JSON.stringify(body) }]),
  getTask: defineOperation(1, ([id]) => isNonEmptyString(id), 'json', ([id]) => [
    `/tasks/${encodeURIComponent(id)}`,
  ]),
  continueTask: defineOperation(2, ([id, body]) => (
    isNonEmptyString(id)
      && isRecord(body)
      && hasValidOptionalFields(body, {
        allow_manual: (value) => typeof value === 'boolean',
        message: (value) => typeof value === 'string',
      })
  ), 'empty', ([id, body]) => [
    `/tasks/${encodeURIComponent(id)}/continue`,
    { method: 'POST', body: JSON.stringify(body) },
  ]),
  pauseTask: defineOperation(1, ([id]) => isNonEmptyString(id), 'empty', ([id]) => [
    `/tasks/${encodeURIComponent(id)}/pause`, { method: 'POST' },
  ]),
  cancelTask: defineOperation(1, ([id]) => isNonEmptyString(id), 'empty', ([id]) => [
    `/tasks/${encodeURIComponent(id)}/cancel`, { method: 'POST' },
  ]),
  listModels: defineOperation(0, () => true, 'json', () => ['/models']),
  getStatus: defineOperation(0, () => true, 'json', () => ['/status']),
  setModelTemperature: defineOperation(2, ([name, temperature]) => (
    isNonEmptyString(name)
      && (temperature === null
        || (typeof temperature === 'number'
          && Number.isFinite(temperature)
          && temperature >= 0
          && temperature <= 2))
  ), 'json', ([name, temperature]) => [
    `/models/${encodeURIComponent(name)}/temperature`,
    { method: 'POST', body: JSON.stringify({ temperature }) },
  ]),
});

function createControlCenterClient({
  apiBase = API_BASE,
  fetchImpl = globalThis.fetch,
  token,
} = {}) {
  if (typeof token !== 'string' || token.trim().length === 0) {
    throw new ControlCenterRequestError('configuration', 0, MISSING_TOKEN_MESSAGE);
  }

  async function request(path, init, responseKind) {
    let response;
    try {
      response = await fetchImpl(`${apiBase}${path}`, {
        ...init,
        headers: {
          'Content-Type': 'application/json',
          Authorization: `Bearer ${token}`,
        },
      });
    } catch {
      throw new ControlCenterRequestError('transport', 0, NETWORK_ERROR_MESSAGE);
    }

    if (!response.ok) {
      throw new ControlCenterRequestError(
        'http',
        response.status,
        `Control Center API отклонил запрос (HTTP ${response.status})`,
      );
    }

    let text;
    try {
      text = await response.text();
    } catch {
      throw new ControlCenterRequestError('transport', 0, NETWORK_ERROR_MESSAGE);
    }

    if (responseKind === 'empty') {
      if (text.length === 0) return undefined;
      throw new ControlCenterRequestError('protocol', response.status, PROTOCOL_ERROR_MESSAGE);
    }
    if (text.trim().length === 0) {
      throw new ControlCenterRequestError('protocol', response.status, PROTOCOL_ERROR_MESSAGE);
    }
    try {
      return JSON.parse(text);
    } catch {
      throw new ControlCenterRequestError('protocol', response.status, PROTOCOL_ERROR_MESSAGE);
    }
  }

  return Object.freeze({
    async invoke(operation, args) {
      if (typeof operation !== 'string' || !Object.hasOwn(operations, operation)) {
        throw boundaryError();
      }
      const definition = operations[operation];
      if (!Array.isArray(args)
        || args.length !== definition.arity
        || !definition.validate(args)) {
        throw boundaryError();
      }
      const [path, init = {}] = definition.build(args);
      return request(path, init, definition.response);
    },
  });
}

function createControlCenterHandler(options) {
  let client;
  let setupError;
  try {
    client = createControlCenterClient(options);
  } catch (error) {
    setupError = error;
  }

  return async (operation, args) => {
    try {
      if (setupError) throw setupError;
      return { ok: true, value: await client.invoke(operation, args) };
    } catch (error) {
      const safe = error instanceof ControlCenterRequestError
        ? error
        : new ControlCenterRequestError('protocol', 0, PROTOCOL_ERROR_MESSAGE);
      return {
        ok: false,
        error: { kind: safe.kind, status: safe.status, message: safe.message },
      };
    }
  };
}

function createTrustedControlCenterHandler(handler, isTrustedSender) {
  return (event, operation, args) => {
    if (!isTrustedSender(event)) {
      return {
        ok: false,
        error: {
          kind: 'boundary',
          status: 0,
          message: 'Недоверенный renderer Control Center',
        },
      };
    }
    return handler(operation, args);
  };
}

module.exports = {
  API_BASE,
  IPC_CHANNEL,
  BOUNDARY_ERROR_MESSAGE,
  ControlCenterRequestError,
  createControlCenterClient,
  createControlCenterHandler,
  createTrustedControlCenterHandler,
};
