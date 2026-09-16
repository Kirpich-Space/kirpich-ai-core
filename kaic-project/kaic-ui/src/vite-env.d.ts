/// <reference types="vite/client" />

type ControlCenterIpcResult =
  | { ok: true; value: unknown }
  | {
      ok: false;
      error: {
        kind: 'configuration' | 'transport' | 'http' | 'protocol' | 'boundary';
        status: number;
        message: string;
      };
    };

interface KaicControlCenterBridge {
  listTasks(): Promise<ControlCenterIpcResult>;
  createTask(body: unknown): Promise<ControlCenterIpcResult>;
  getTask(id: string): Promise<ControlCenterIpcResult>;
  continueTask(id: string, body: unknown): Promise<ControlCenterIpcResult>;
  pauseTask(id: string): Promise<ControlCenterIpcResult>;
  cancelTask(id: string): Promise<ControlCenterIpcResult>;
  listModels(): Promise<ControlCenterIpcResult>;
  getStatus(): Promise<ControlCenterIpcResult>;
  setModelTemperature(name: string, temperature: number | null): Promise<ControlCenterIpcResult>;
}

interface Window {
  readonly kaicControlCenter?: KaicControlCenterBridge;
}
