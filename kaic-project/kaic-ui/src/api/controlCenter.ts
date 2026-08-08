// Тонкая обёртка над fetch() для Control Center API (см. control_center_api.rs).
// Никакой бизнес-логики здесь нет: только типы ответов backend'а,
// построение запросов и единообразная обработка ошибок.
// Явно не используем axios/react-query/redux/zustand — обычного fetch()
// достаточно для того объёма запросов, что есть у KAIC сейчас.

// Адрес backend'а. По умолчанию — порт, зашитый в main.rs (4545).
// Переопределяется переменной окружения VITE_API_BASE при сборке/запуске
// Vite, если когда-нибудь понадобится указать другой адрес — без правки
// кода и без отдельной системы .env-файлов.
export const API_BASE = import.meta.env.VITE_API_BASE ?? 'http://127.0.0.1:4545';

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

/** Задача целиком — то, что возвращают GET /tasks, GET /tasks/:id, POST /tasks. */
export interface Task {
  id: string;
  category: string;
  status: TaskStatus;
  created_at: string;
  updated_at: string;
  context: ContextEntry[];
  pending_telegram_message_id: number | null;
}

/** Тело запроса на создание задачи (см. CreateTaskRequest в control_center_api.rs). */
export interface CreateTaskRequest {
  text: string;
  category?: string;
  allow_manual?: boolean;
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
  /** Что backend делает прямо сейчас, или null в покое.
   *  Во время загрузки модели `loaded_models` её ещё не содержит — она
   *  появится там только после завершения. Это поле объясняет промежуток:
   *  без него панель показывала бы «не загружено» и была бы формально права,
   *  но непонятна. */
  active_operation: ActiveOperationDto | null;
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
  ) {
    super(message);
    this.name = 'ControlCenterError';
  }
}

/** Общий каркас запроса: строит URL, шлёт JSON, разбирает ответ или ошибку. */
async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  let response: Response;
  try {
    response = await fetch(`${API_BASE}${path}`, {
      ...init,
      headers: { 'Content-Type': 'application/json', ...init.headers },
    });
  } catch {
    // fetch() бросает TypeError, если backend недоступен (не запущен,
    // неверный порт и т.п.) — заворачиваем в тот же тип ошибки, чтобы
    // вызывающему коду не нужно было различать два разных вида исключений.
    throw new ControlCenterError(0, 'Control Center API недоступен (backend не запущен?)');
  }

  if (!response.ok) {
    const message = await response.text().catch(() => response.statusText);
    throw new ControlCenterError(response.status, message || response.statusText);
  }

  // Роуты continue/pause/cancel не возвращают тело — в этом случае просто
  // ничего не парсим и отдаём undefined.
  const text = await response.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

// --- Методы API ---
// По одному на каждый роут control_center_api.rs.

export const controlCenter = {
  /** GET /tasks — список всех задач. */
  listTasks: (): Promise<Task[]> => request<Task[]>('/tasks'),

  /** POST /tasks — создать задачу; пайплайн запускается на backend'е в фоне. */
  createTask: (body: CreateTaskRequest): Promise<Task> =>
    request<Task>('/tasks', { method: 'POST', body: JSON.stringify(body) }),

  /** GET /tasks/:id — одна задача. */
  getTask: (id: string): Promise<Task> => request<Task>(`/tasks/${id}`),

  /** POST /tasks/:id/continue — продолжить задачу (после WaitingForHuman/Paused). */
  continueTask: (id: string, body: ContinueRequest = {}): Promise<void> =>
    request<void>(`/tasks/${id}/continue`, { method: 'POST', body: JSON.stringify(body) }),

  /** POST /tasks/:id/pause — поставить задачу на паузу. */
  pauseTask: (id: string): Promise<void> =>
    request<void>(`/tasks/${id}/pause`, { method: 'POST' }),

  /** POST /tasks/:id/cancel — отменить задачу. */
  cancelTask: (id: string): Promise<void> =>
    request<void>(`/tasks/${id}/cancel`, { method: 'POST' }),

  /** GET /models — все известные модели и их текущее состояние загрузки. */
  listModels: (): Promise<ModelStatusDto[]> => request<ModelStatusDto[]>('/models'),

  /** GET /status — сводное состояние Scheduler'а (VRAM, загруженные модели). */
  getStatus: (): Promise<StatusDto> => request<StatusDto>('/status'),

  /** POST /models/:name/temperature — задать температуру модели.
   *  Диапазон значения — TEMPERATURE_MIN..TEMPERATURE_MAX; проверка здесь
   *  не делается, её обязан делать вызывающий (см. TemperatureCell) и,
   *  окончательно, backend.
   *  `null` — снять настройку: поле перестанет отправляться провайдеру,
   *  и снова начнёт действовать его умолчание. Модель не обязана быть
   *  загружена. Возвращает обновлённый список моделей. */
  setModelTemperature: (name: string, temperature: number | null): Promise<ModelStatusDto[]> =>
    request<ModelStatusDto[]>(`/models/${encodeURIComponent(name)}/temperature`, {
      method: 'POST',
      body: JSON.stringify({ temperature }),
    }),
};
