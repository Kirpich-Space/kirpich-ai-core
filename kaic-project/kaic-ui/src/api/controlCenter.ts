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
}

interface LoadedModelDto {
  model: string;
  last_used: string;
}

/** Общее состояние Scheduler'а — то, что возвращает GET /status. */
export interface StatusDto {
  total_vram_mb: number;
  used_vram_mb: number;
  loaded_models: LoadedModelDto[];
}

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
};
