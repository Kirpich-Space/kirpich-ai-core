import { useState, type FormEvent } from 'react';
import { controlCenter } from '../api/controlCenter';
import type { PendingTask } from '../types';

interface HeaderProps {
  onTaskCreated: (pending: PendingTask) => void;
  /** Backend вернул реальный id созданной задачи — TaskList будет искать
   * совпадение именно по нему, а не по тексту (однозначно, в отличие
   * от сравнения строк, которое ломается на одинаковых текстах). */
  onTaskIdKnown: (tempId: string, realTaskId: string) => void;
  /** Вызывается, только если сама отправка не удалась — тогда ждать
   * нечего, и placeholder убирается сразу же. */
  onTaskFailed: (id: string) => void;
}

// Минимальная форма создания задачи: одна строка ввода, одна кнопка.
// Никакого выбора категории/модели — Router на backend'е определяет
// категорию сам (см. router.rs).
export default function Header({ onTaskCreated, onTaskIdKnown, onTaskFailed }: HeaderProps) {
  const [text, setText] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleSubmit(event: FormEvent) {
    event.preventDefault();

    const trimmed = text.trim();
    if (!trimmed) {
      return; // пустой текст задачи не отправляем
    }

    // Оптимистичное обновление: placeholder появляется в списке и поле
    // очищается сразу, не дожидаясь ответа backend'а.
    const tempId = crypto.randomUUID();
    onTaskCreated({ id: tempId, textPreview: trimmed });
    setText('');
    setSubmitting(true);
    setError(null);

    try {
      const created = await controlCenter.createTask({ text: trimmed });
      // POST /tasks уже возвращает созданную задачу с её реальным id —
      // передаём его наверх, TaskList уберёт placeholder, когда увидит
      // задачу с этим id в ответе GET /tasks.
      onTaskIdKnown(tempId, created.id);
    } catch (err) {
      onTaskFailed(tempId);
      setText(trimmed); // возвращаем текст, чтобы не потерять ввод
      setError(err instanceof Error ? err.message : 'Неизвестная ошибка');
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <header style={{ display: 'flex', alignItems: 'center', gap: 12, padding: '0 12px' }}>
      <strong>KAIC Control Center</strong>

      <form onSubmit={handleSubmit} style={{ display: 'flex', gap: 8, flex: 1 }}>
        <input
          type="text"
          value={text}
          onChange={(event) => setText(event.target.value)}
          placeholder="Текст задачи..."
          disabled={submitting}
          style={{ flex: 1, padding: 4 }}
        />
        <button type="submit" disabled={submitting}>
          {submitting ? 'Создаю...' : 'Создать задачу'}
        </button>
      </form>

      {error && <span style={{ color: '#ef4444' }}>Ошибка: {error}</span>}
    </header>
  );
}
