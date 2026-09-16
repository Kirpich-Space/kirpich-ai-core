'use strict';

const { contextBridge, ipcRenderer } = require('electron');
// Sandboxed preload не может require'ить локальные CommonJS-модули.
// Канал — не секрет и не capability: main всё равно проверяет
// sender, operation и args до HTTP.
const IPC_CHANNEL = 'kaic:control-center';

const invoke = (operation, args = []) => ipcRenderer.invoke(IPC_CHANNEL, operation, args);

contextBridge.exposeInMainWorld('kaicControlCenter', Object.freeze({
  listTasks: () => invoke('listTasks'),
  createTask: (body) => invoke('createTask', [body]),
  getTask: (id) => invoke('getTask', [id]),
  continueTask: (id, body) => invoke('continueTask', [id, body]),
  pauseTask: (id) => invoke('pauseTask', [id]),
  cancelTask: (id) => invoke('cancelTask', [id]),
  listModels: () => invoke('listModels'),
  getStatus: () => invoke('getStatus'),
  setModelTemperature: (name, temperature) => invoke('setModelTemperature', [name, temperature]),
}));
