// Тонкая типизированная обёртка над preload/IPC-мостом Control Center.
// Никакой бизнес-логики здесь нет: только типы ответов backend'а,
// построение запросов и единообразная обработка ошибок.
// Renderer не знает URL, HTTP-методы, заголовки и runtime-секрет.

// --- Типы данных ---
// Соответствуют структурам, которые сериализует backend (Task, TaskStatus,
// ContextEntry в task_store.rs; ModelStatusDto, StatusDto в control_center_api.rs).

/** Статус задачи — ровно те же значения, что в TaskStatus (serde rename_all="snake_case"). */
export type TaskStatus =
  | 'running'
  | 'waiting_for_human'
  | 'paused'
  | 'done'
  | 'failed'
  | 'cancelled';

/** Один элемент накопленного контекста задачи. */
export interface ContextEntry {
  role: string;
  content: string;
  at: string; // ISO 8601, как отдаёт chrono::DateTime<Utc>
}

/** Structured backend/model provenance, absent for old or not-yet-run tasks. */
export interface ExecutionMetadata {
  backend: string;
  model: string;
}

/** Задача целиком — то, что возвращают GET /tasks, GET /tasks/:id, POST /tasks. */
export interface Task {
  id: string;
  category: string;
  status: TaskStatus;
  created_at: string;
  updated_at: string;
  context: ContextEntry[];
  pending_telegram_message_id: number | null;
  execution?: ExecutionMetadata;
}

/** Тело запроса на создание задачи (см. CreateTaskRequest в control_center_api.rs). */
export interface CreateTaskRequest {
  text: string;
  category?: string;
  allow_manual?: boolean;
  model?: string;
}

/** Тело запроса на продолжение задачи (см. ContinueRequest). */
export interface ContinueRequest {
  message?: string;
  allow_manual?: boolean;
}

/** Одна модель из Resource Registry вместе с текущим состоянием загрузки. */
export interface ModelStatusDto {
  model: string;
  vram_mb: number;
  ram_offload_mb: number;
  always_loaded: boolean;
  preferred_device: 'gpu' | 'cpu' | 'hybrid';
  loaded: boolean;
  last_used: string | null;
  /** Длина контекста загруженной модели. Параметр ЗАГРУЗКИ: смена требует
   *  перезагрузки модели. null — не измерено (модель не грузится на этой машине). */
  context_length: number | null;
  /** Потолок контекста, заявленный моделью. Известен без загрузки. */
  max_context_length: number | null;
  /** Температура сэмплинга. Параметр ВЫЗОВА, перезагрузка не нужна.
   *  null — не задана, действует умолчание провайдера. */
  temperature: number | null;
}

interface LoadedModelDto {
  model: string;
  last_used: string;
}

/** Общее состояние Scheduler'а — то, что возвращает GET /status. */
export interface StatusDto {
  /** Бюджет памяти под модели: RAM + VRAM минус резерв под ОС. Это НЕ объём
   *  видеопамяти — величина сменила смысл вместе с именем. */
  total_model_memory_mb: number;
  /** Сумма табличных значений по загруженным моделям. Учётная величина:
   *  backend не спрашивает у GPU, сколько занято на самом деле. */
  used_model_memory_mb: number;
  loaded_models: LoadedModelDto[];
  /** Сколько генераций идёт прямо сейчас. Нужно, чтобы «система свободна»
   *  и «идут две задачи, третья будет отвергнута» не выглядели одинаково. */
  running_tasks: number;
  /** Предел, после которого задачи получают отказ (а не встают в очередь). */
  max_concurrent_tasks: number;
  busy_models: BusyModelDto[];
  /** Queue depth is always zero because Scheduler rejects over-capacity work. */
  queue_depth: number;
  /** idle, loading, unloading, or generating. */
  inference_state: 'idle' | 'loading' | 'unloading' | 'generating';
  /** Backend lifecycle limitations, reported rather than guessed. */
  cancellation_supported: boolean;
  timeout_supported: boolean;
  /** Что backend делает прямо сейчас, или null в покое.
   *  Во время загрузки модели `loaded_models` её ещё не содержит — она
   *  появится там только после завершения. Это поле объясняет промежуток:
   *  без него панель показывала бы «не загружено» и была бы формально права,
   *  но непонятна. */
  active_operation: ActiveOperationDto | null;
}

/** Модель, на которой прямо сейчас идёт генерация. Такие модели не
 *  вытесняются: выгрузить их значило бы оборвать работающий запрос. */
export interface BusyModelDto {
  model: string;
  generations: number;
}

/** Выполняемая прямо сейчас операция Scheduler'а (см. ActiveOperationDto). */
export interface ActiveOperationDto {
  /** «загрузка» или «выгрузка». */
  kind: string;
  model: string;
  /** Категория задачи или ярлык модели — ради чего идёт операция. */
  reason: string;
  started_at: string;
  elapsed_ms: number;
}

/** Допустимый диапазон temperature. Держится в одном месте с backend'ом
 *  (TEMPERATURE_MIN/TEMPERATURE_MAX в resource_registry.rs): нижняя граница —
 *  ограничение провайдера, верхняя — наше решение. Проверка в UI существует
 *  ради понятной ошибки до сети; гарантией остаётся проверка на маршруте API. */
export const TEMPERATURE_MIN = 0;
export const TEMPERATURE_MAX = 2;

// --- Обработка ошибок ---

/**
 * Ошибка ответа Control Center API. `status` — HTTP-код, `message` — текст
 * тела ответа (backend возвращает простой текст, не JSON, см. ApiError
 * в control_center_api.rs: "задача не найдена" / текст anyhow-ошибки).
 */
export class ControlCenterError extends Error {
  constructor(
    public readonly status: number,
    message: string,
    public readonly kind: 'configuration' | 'transport' | 'http' | 'protocol' | 'boundary' = 'protocol',
  ) {
    super(message);
    this.name = 'ControlCenterError';
  }
}

type BridgeMethod = keyof KaicControlCenterBridge;

async function request<T>(method: BridgeMethod, ...args: unknown[]): Promise<T> {
  const bridge = window.kaicControlCenter;
  if (!bridge) {
    throw new ControlCenterError(0, 'Control Center доступен только в Electron');
  }
  let result: ControlCenterIpcResult;
  try {
    result = await (bridge[method] as (...values: unknown[]) => Promise<ControlCenterIpcResult>)(...args);
  } catch {
    throw new ControlCenterError(0, 'Control Center IPC недоступен', 'transport');
  }
  if (!result || typeof result !== 'object' || typeof result.ok !== 'boolean') {
    throw new ControlCenterError(0, 'Control Center вернул некорректный IPC-ответ', 'protocol');
  }
  if (!result.ok) {
    throw new ControlCenterError(result.error.status, result.error.message, result.error.kind);
  }
  return result.value as T;
}

// --- Методы API ---
// По одному на каждый роут control_center_api.rs.

export const controlCenter = {
  /** GET /tasks — список всех задач. */
  listTasks: (): Promise<Task[]> => request<Task[]>('listTasks'),

  /** POST /tasks — создать задачу; пайплайн запускается на backend'е в фоне. */
  createTask: (body: CreateTaskRequest): Promise<Task> =>
    request<Task>('createTask', body),

  /** GET /tasks/:id — одна задача. */
  getTask: (id: string): Promise<Task> => request<Task>('getTask', id),

  /** POST /tasks/:id/continue — продолжить задачу (после WaitingForHuman/Paused). */
  continueTask: (id: string, body: ContinueRequest = {}): Promise<void> =>
    request<void>('continueTask', id, body),

  /** POST /tasks/:id/pause — поставить задачу на паузу. */
  pauseTask: (id: string): Promise<void> =>
    request<void>('pauseTask', id),

  /** POST /tasks/:id/cancel — отменить задачу. */
  cancelTask: (id: string): Promise<void> =>
    request<void>('cancelTask', id),

  /** GET /models — все известные модели и их текущее состояние загрузки. */
  listModels: (): Promise<ModelStatusDto[]> => request<ModelStatusDto[]>('listModels'),

  /** GET /status — сводное состояние Scheduler'а (VRAM, загруженные модели). */
  getStatus: (): Promise<StatusDto> => request<StatusDto>('getStatus'),

  /** POST /models/:name/temperature — задать температуру модели.
   *  Диапазон значения — TEMPERATURE_MIN..TEMPERATURE_MAX; проверка здесь
   *  не делается, её обязан делать вызывающий (см. TemperatureCell) и,
   *  окончательно, backend.
   *  `null` — снять настройку: поле перестанет отправляться провайдеру,
   *  и снова начнёт действовать его умолчание. Модель не обязана быть
   *  загружена. Возвращает обновлённый список моделей. */
  setModelTemperature: (name: string, temperature: number | null): Promise<ModelStatusDto[]> =>
    request<ModelStatusDto[]>('setModelTemperature', name, temperature),
};
