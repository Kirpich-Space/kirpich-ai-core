import { useEffect, useRef, useState } from 'react';
import { controlCenter, type Task } from '../api/controlCenter';
import { STATUS_LABEL, STATUS_COLOR } from '../statusLabels';
import type { PendingTask } from '../types';

const POLL_INTERVAL_MS = 1000;
const TITLE_MAX_LENGTH = 60;

// KAIC не хранит отдельное поле "название задачи" — названием считается
// первое сообщение пользователя (см. обсуждение). Без обрезки — нужно
// для точного сравнения с PendingTask.textPreview в другом месте,
// с обрезкой — для компактного отображения здесь.
function getFirstUserMessage(task: Task): string | undefined {
  return task.context.find((entry) => entry.role === 'user')?.content.trim();
}

function getTaskTitle(task: Task): string {
  const text = getFirstUserMessage(task) ?? '(без текста)';
  return text.length > TITLE_MAX_LENGTH ? `${text.slice(0, TITLE_MAX_LENGTH)}…` : text;
}

interface TaskListProps {
  pendingTasks: PendingTask[];
  /** Вызывается, когда среди задач с backend'а нашлась та, что
   * соответствует ожидающему placeholder-у — тогда его пора убрать. */
  onPendingArrived: (id: string) => void;
  selectedTaskId: string | null;
  onSelectTask: (id: string) => void;
}

// Список задач (чтение + создание через оптимистичные placeholder-ы +
// выбор строки). Продолжение/пауза/отмена — следующие шаги.
export default function TaskList({
  pendingTasks,
  onPendingArrived,
  selectedTaskId,
  onSelectTask,
}: TaskListProps) {
  const [tasks, setTasks] = useState<Task[]>([]);
  const [error, setError] = useState<string | null>(null);

  // Поллинг живёт в эффекте с пустым списком зависимостей (запускается
  // один раз), поэтому актуальный pendingTasks читаем через ref, а не
  // через замыкание — иначе пришлось бы пересоздавать interval при
  // каждом изменении pendingTasks.
  const pendingTasksRef = useRef<PendingTask[]>(pendingTasks);
  useEffect(() => {
    pendingTasksRef.current = pendingTasks;
  }, [pendingTasks]);

  useEffect(() => {
    let cancelled = false;

    async function loadTasks() {
      try {
        const result = await controlCenter.listTasks();
        if (cancelled) return;

        setTasks(result);
        setError(null);

        // Совпадение по id — однозначно (см. обсуждение выше по чату).
        // Пока backend ещё не ответил на POST /tasks, realTaskId у
        // placeholder-а не задан — такие пропускаем, сравнивать не с чем.
        for (const pending of pendingTasksRef.current) {
          if (!pending.realTaskId) continue;
          const arrived = result.some((task) => task.id === pending.realTaskId);
          if (arrived) {
            onPendingArrived(pending.id);
          }
        }
      } catch (err) {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : 'Неизвестная ошибка');
        }
      }
    }

    loadTasks(); // сразу при монтировании, не ждём первый тик интервала
    const intervalId = setInterval(loadTasks, POLL_INTERVAL_MS);

    return () => {
      cancelled = true;
      clearInterval(intervalId);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <aside>
      <h2>Задачи</h2>

      {error && <p style={{ color: STATUS_COLOR.failed }}>Ошибка: {error}</p>}

      {!error && tasks.length === 0 && pendingTasks.length === 0 && <p>Нет задач</p>}

      <ul style={{ listStyle: 'none', padding: 0, margin: 0 }}>
        {pendingTasks.map((pending) => (
          <li key={pending.id} style={{ padding: '8px 0', borderBottom: '1px solid #eee', opacity: 0.6 }}>
            <div>
              <span
                style={{
                  display: 'inline-block',
                  width: 8,
                  height: 8,
                  borderRadius: '50%',
                  backgroundColor: STATUS_COLOR.running,
                  marginRight: 6,
                }}
              />
              Создание задачи...
            </div>
            <div style={{ fontStyle: 'italic' }}>{pending.textPreview}</div>
          </li>
        ))}

        {tasks.map((task) => (
          <li
            key={task.id}
            onClick={() => onSelectTask(task.id)}
            style={{
              padding: '8px 0',
              borderBottom: '1px solid #eee',
              cursor: 'pointer',
              backgroundColor: task.id === selectedTaskId ? '#eef4ff' : 'transparent',
            }}
          >
            <div>
              <span
                style={{
                  display: 'inline-block',
                  width: 8,
                  height: 8,
                  borderRadius: '50%',
                  backgroundColor: STATUS_COLOR[task.status],
                  marginRight: 6,
                }}
              />
              {STATUS_LABEL[task.status]}
            </div>
            <div>{getTaskTitle(task)}</div>
            <div style={{ fontSize: 12, color: '#888' }}>
              {task.category} · {new Date(task.updated_at).toLocaleTimeString('ru-RU')}
            </div>
          </li>
        ))}
      </ul>
    </aside>
  );
}
