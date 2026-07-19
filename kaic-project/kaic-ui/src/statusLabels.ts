import type { TaskStatus } from './api/controlCenter';

// Вынесено сюда из TaskList в тот момент, когда DetailsPanel тоже стал
// нуждаться в этих подписях — до этого жило локально в одном файле.
export const STATUS_LABEL: Record<TaskStatus, string> = {
  running: 'Выполняется',
  waiting_for_human: 'Ждёт решения',
  paused: 'Пауза',
  done: 'Готово',
  failed: 'Ошибка',
  cancelled: 'Отменено',
};

export const STATUS_COLOR: Record<TaskStatus, string> = {
  running: '#3b82f6',
  waiting_for_human: '#f59e0b',
  paused: '#9ca3af',
  done: '#22c55e',
  failed: '#ef4444',
  cancelled: '#6b7280',
};
