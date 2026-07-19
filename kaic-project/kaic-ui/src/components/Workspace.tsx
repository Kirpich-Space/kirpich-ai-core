import { useEffect, useState, type FormEvent } from 'react';
import { controlCenter, type Task, type TaskStatus } from '../api/controlCenter';

// Подписи роли для отображения — на backend'е role свободная строка
// (см. ContextEntry в task_store.rs), здесь только человекочитаемо
// переводим три известных значения, остальное — как есть.
function roleLabel(role: string): string {
  switch (role) {
    case 'user':
      return 'Вы';
    case 'assistant':
      return 'KAIC';
    case 'system':
      return 'Система';
    default:
      return role;
  }
}

// Continue имеет смысл только пока задача реально чего-то ждёт или
// встала из-за ошибки — не когда она уже выполняется, готова или отменена.
const CONTINUABLE_STATUSES: TaskStatus[] = ['waiting_for_human', 'paused', 'failed'];

interface WorkspaceProps {
  task: Task | null;
}

// Показывает всю историю задачи (Task.context) как простую хронологию
// сообщений, плюс форму продолжения снизу. Пока без Markdown-рендеринга,
// без редактирования прошлых сообщений — это следующие шаги.
export default function Workspace({ task }: WorkspaceProps) {
  const [replyText, setReplyText] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Сбрасываем локальное состояние формы при переключении на другую
  // задачу — иначе недописанный ответ для одной задачи мог бы всплыть
  // в поле ввода при просмотре совсем другой.
  useEffect(() => {
    setReplyText('');
    setError(null);
  }, [task?.id]);

  if (!task) {
    return (
      <main>
        <p>Выберите задачу слева, чтобы увидеть её историю.</p>
      </main>
    );
  }

  async function handleContinue(event: FormEvent) {
    event.preventDefault();
    if (!task) return;

    setSubmitting(true);
    setError(null);
    try {
      const trimmed = replyText.trim();
      // Пустой ответ — это просто "продолжай", без новой реплики от
      // пользователя; ContinueRequest.message на backend'е опционален.
      await controlCenter.continueTask(task.id, trimmed ? { message: trimmed } : {});
      setReplyText('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Неизвестная ошибка');
    } finally {
      setSubmitting(false);
    }
  }

  const canContinue = CONTINUABLE_STATUSES.includes(task.status);

  return (
    <main>
      <h2>История задачи</h2>

      {task.context.length === 0 && <p>В этой задаче пока нет сообщений.</p>}

      <ul style={{ listStyle: 'none', padding: 0, margin: 0 }}>
        {task.context.map((entry, index) => (
          // У ContextEntry нет собственного id — используем индекс: это
          // безопасно именно потому, что контекст только растёт в конец
          // и никогда не переупорядочивается (см. append_context).
          <li key={index} style={{ padding: '8px 0', borderBottom: '1px solid #eee' }}>
            <div style={{ fontWeight: 'bold' }}>
              {roleLabel(entry.role)}{' '}
              <span style={{ fontWeight: 'normal', fontSize: 12, color: '#888' }}>
                {new Date(entry.at).toLocaleTimeString('ru-RU')}
              </span>
            </div>
            <div style={{ whiteSpace: 'pre-wrap' }}>{entry.content}</div>
          </li>
        ))}
      </ul>

      <form onSubmit={handleContinue} style={{ marginTop: 16, display: 'flex', gap: 8 }}>
        <input
          type="text"
          value={replyText}
          onChange={(event) => setReplyText(event.target.value)}
          placeholder={canContinue ? 'Ваш ответ (необязательно)...' : 'Задача сейчас не ждёт продолжения'}
          disabled={!canContinue || submitting}
          style={{ flex: 1, padding: 4 }}
        />
        <button type="submit" disabled={!canContinue || submitting}>
          {submitting ? 'Отправляю...' : 'Продолжить'}
        </button>
      </form>

      {error && <p style={{ color: '#ef4444' }}>Ошибка: {error}</p>}
    </main>
  );
}
