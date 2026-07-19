import { useEffect, useState } from 'react';
import { controlCenter, type Task, type TaskStatus } from '../api/controlCenter';
import { STATUS_LABEL } from '../statusLabels';

// В этих статусах у Pause/Cancel уже нет смысла — задача завершена
// в том или ином виде.
const TERMINAL_STATUSES: TaskStatus[] = ['done', 'failed', 'cancelled'];

interface DetailsPanelProps {
  task: Task | null;
}

// Техническая информация о выбранной задаче + действия Pause/Cancel.
// Continue (с текстом ответа) живёт в Workspace, рядом с историей,
// которую он продолжает — здесь только действия без параметров.
export default function DetailsPanel({ task }: DetailsPanelProps) {
  const [actionError, setActionError] = useState<string | null>(null);
  const [submittingAction, setSubmittingAction] = useState<'pause' | 'cancel' | null>(null);

  // Тот же принцип, что и в Workspace: не тащим состояние ошибки одной
  // задачи в просмотр другой при переключении.
  useEffect(() => {
    setActionError(null);
  }, [task?.id]);

  if (!task) {
    return (
      <aside>
        <p>Задача не выбрана.</p>
      </aside>
    );
  }

  const isTerminal = TERMINAL_STATUSES.includes(task.status);

  async function handlePause() {
    if (!task) return;
    setSubmittingAction('pause');
    setActionError(null);
    try {
      await controlCenter.pauseTask(task.id);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : 'Неизвестная ошибка');
    } finally {
      setSubmittingAction(null);
    }
  }

  async function handleCancel() {
    if (!task) return;
    // Отмена — разрушительное действие, требует подтверждения (в отличие
    // от паузы, которую легко отменить обратным Continue).
    if (!window.confirm('Отменить эту задачу?')) return;

    setSubmittingAction('cancel');
    setActionError(null);
    try {
      await controlCenter.cancelTask(task.id);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : 'Неизвестная ошибка');
    } finally {
      setSubmittingAction(null);
    }
  }

  return (
    <aside>
      <h2>Детали</h2>
      <dl>
        <dt>Статус</dt>
        <dd>{STATUS_LABEL[task.status]}</dd>

        <dt>Категория</dt>
        <dd>{task.category}</dd>

        <dt>Создана</dt>
        <dd>{new Date(task.created_at).toLocaleString('ru-RU')}</dd>

        <dt>Обновлена</dt>
        <dd>{new Date(task.updated_at).toLocaleString('ru-RU')}</dd>

        <dt>Сообщений</dt>
        <dd>{task.context.length}</dd>

        <dt>Модель</dt>
        {/* Task пока не хранит, какую именно модель выбрал Scheduler —
            см. обсуждение в чате. */}
        <dd>— (пока не отслеживается backend'ом)</dd>
      </dl>

      <div style={{ display: 'flex', gap: 8, marginTop: 12 }}>
        <button onClick={handlePause} disabled={isTerminal || submittingAction !== null}>
          {submittingAction === 'pause' ? 'Ставлю на паузу...' : 'Пауза'}
        </button>
        <button onClick={handleCancel} disabled={isTerminal || submittingAction !== null}>
          {submittingAction === 'cancel' ? 'Отменяю...' : 'Отменить'}
        </button>
      </div>

      {actionError && <p style={{ color: '#ef4444' }}>Ошибка: {actionError}</p>}
    </aside>
  );
}
