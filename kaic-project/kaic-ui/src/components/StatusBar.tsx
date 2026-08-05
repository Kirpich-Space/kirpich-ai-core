import { useEffect, useState } from 'react';
import {
  controlCenter,
  TEMPERATURE_MAX,
  TEMPERATURE_MIN,
  type ModelStatusDto,
  type StatusDto,
} from '../api/controlCenter';

// Тот же интервал, что у поллинга задач в App.tsx и TaskList.tsx.
// Отдельной абстракции для поллинга по-прежнему нет — три независимых
// setInterval проще одного самодельного хука, пока их поведение не
// начало расходиться.
const POLL_INTERVAL_MS = 1000;

// GET /status и GET /models — единственные два метода API-клиента, у
// которых до сих пор не было ни одного вызывающего. Именно поэтому
// StatusBar и оставался заглушкой: данные были доступны, но никем
// не запрашивались.
export default function StatusBar() {
  const [status, setStatus] = useState<StatusDto | null>(null);
  const [models, setModels] = useState<ModelStatusDto[]>([]);
  const [offline, setOffline] = useState(false);
  const [showParams, setShowParams] = useState(false);

  useEffect(() => {
    let cancelled = false;

    async function load() {
      try {
        // Оба запроса параллельно: они независимы, и последовательное
        // ожидание удвоило бы задержку обновления строки состояния.
        const [nextStatus, nextModels] = await Promise.all([
          controlCenter.getStatus(),
          controlCenter.listModels(),
        ]);
        if (cancelled) return;
        setStatus(nextStatus);
        setModels(nextModels);
        setOffline(false);
      } catch {
        // Backend недоступен — показываем это явно, но НЕ стираем
        // последние известные данные: мигание пустотой на одном
        // неудачном тике хуже, чем слегка устаревшие числа.
        if (!cancelled) setOffline(true);
      }
    }

    load();
    const intervalId = setInterval(load, POLL_INTERVAL_MS);

    return () => {
      cancelled = true;
      clearInterval(intervalId);
    };
  }, []);

  if (offline && !status) {
    return (
      <footer style={{ display: 'flex', alignItems: 'center', gap: 16, padding: '0 12px' }}>
        <span style={{ color: '#ef4444' }}>● Backend недоступен</span>
      </footer>
    );
  }

  const loadedNames = status?.loaded_models.map((m) => m.model) ?? [];
  // always_loaded-модели показываем отдельно: они занимают VRAM
  // постоянно, и это объясняет базовый уровень занятости.
  const residentCount = models.filter((m) => m.always_loaded).length;
  const usedMb = status?.used_vram_mb ?? 0;
  const totalMb = status?.total_vram_mb ?? 0;
  const percent = totalMb > 0 ? Math.round((usedMb / totalMb) * 100) : 0;

  return (
    <footer
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 16,
        padding: '0 12px',
        fontSize: 13,
      }}
    >
      <span style={{ color: offline ? '#f59e0b' : '#22c55e' }}>
        {offline ? '● Нет связи (данные устарели)' : '● Backend на связи'}
      </span>

      <span>
        VRAM: {usedMb} / {totalMb} MB ({percent}%)
      </span>

      <span>Загружено: {loadedNames.length > 0 ? loadedNames.join(', ') : '—'}</span>

      <span style={{ color: '#888' }}>
        Моделей в реестре: {models.length}
        {residentCount > 0 && ` (резидентных: ${residentCount})`}
      </span>

      <button
        onClick={() => setShowParams((v) => !v)}
        style={{ marginLeft: 'auto', fontSize: 12 }}
      >
        {showParams ? 'Скрыть параметры' : 'Параметры моделей'}
      </button>

      {showParams && (
        <ModelParams
          models={models}
          onClose={() => setShowParams(false)}
          onModelsChanged={setModels}
        />
      )}
    </footer>
  );
}

// temperature редактируется прямо здесь — это параметр вызова, применяется
// со следующего обращения к модели, перезагрузка не нужна.
//
// context остаётся ТОЛЬКО ДЛЯ ЧТЕНИЯ намеренно: это параметр загрузки, и его
// смена требует unload+reload модели (установлено эмпирически, см.
// комментарии в resource_registry.yaml). UX такой перезагрузки — отдельная
// задача, и делать её «заодно» здесь было бы неверно.
function ModelParams({
  models,
  onClose,
  onModelsChanged,
}: {
  models: ModelStatusDto[];
  onClose: () => void;
  /** Сервер возвращает обновлённый список после записи — показываем его
   *  сразу, не дожидаясь следующего тика поллинга. */
  onModelsChanged: (models: ModelStatusDto[]) => void;
}) {
  return (
    <div
      style={{
        position: 'fixed',
        right: 12,
        bottom: 40,
        background: '#fff',
        border: '1px solid #ccc',
        borderRadius: 4,
        padding: 12,
        boxShadow: '0 2px 12px rgba(0,0,0,0.15)',
        zIndex: 10,
        maxHeight: '60vh',
        overflow: 'auto',
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 8 }}>
        <strong>Параметры моделей</strong>
        <span style={{ color: '#888', fontSize: 12 }}>context — только чтение</span>
        <button onClick={onClose} style={{ marginLeft: 'auto' }}>
          ×
        </button>
      </div>

      <table style={{ borderCollapse: 'collapse', fontSize: 12 }}>
        <thead>
          <tr style={{ textAlign: 'left', borderBottom: '1px solid #ccc' }}>
            <th style={{ padding: '4px 8px' }}>Модель</th>
            <th style={{ padding: '4px 8px' }}>temperature</th>
            <th style={{ padding: '4px 8px' }}>context</th>
            <th style={{ padding: '4px 8px' }}>максимум</th>
          </tr>
        </thead>
        <tbody>
          {models.map((m) => (
            <tr key={m.model} style={{ borderBottom: '1px solid #eee' }}>
              <td style={{ padding: '4px 8px' }}>
                {m.model}
                {m.loaded && <span style={{ color: '#22c55e' }}> ●</span>}
              </td>
              {/* null у temperature означает не "ноль", а "не задана" —
                  подписываем словами, чтобы это нельзя было прочитать
                  как значение 0. */}
              <td style={{ padding: '4px 8px' }}>
                <TemperatureCell model={m} onSaved={onModelsChanged} />
              </td>
              <td
                style={{ padding: '4px 8px', color: m.context_length === null ? '#888' : 'inherit' }}
              >
                {m.context_length ?? 'не измерено'}
              </td>
              <td style={{ padding: '4px 8px', color: '#888' }}>{m.max_context_length ?? '—'}</td>
            </tr>
          ))}
        </tbody>
      </table>

      <p style={{ fontSize: 11, color: '#888', margin: '8px 0 0', maxWidth: 420 }}>
        Низкая temperature обычно полезна там, где от модели ждут строгий формат
        ответа — например, роль Planner в агентном слое. Конкретное значение
        подбирается опытом, готового «правильного» нет. Допустимый диапазон —{' '}
        {TEMPERATURE_MIN}–{TEMPERATURE_MAX}; пустое поле означает «снять настройку»
        (не ноль).
        <br />
        <br />
        «не измерено» — модель не загружается на этой машине, эффективный контекст
        получить неоткуда. «по умолчанию» — значение не задано, действует умолчание
        LM Studio (через API не сообщается). Смена context требует перезагрузки модели,
        temperature применяется на каждый вызов.
      </p>
    </div>
  );
}

// Инпут + Сохранить прямо в ячейке. Отдельной библиотеки состояния не
// заводим — это форма из одного поля, useState достаточно.
function TemperatureCell({
  model,
  onSaved,
}: {
  model: ModelStatusDto;
  onSaved: (models: ModelStatusDto[]) => void;
}) {
  const [text, setText] = useState(model.temperature === null ? '' : String(model.temperature));
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Значение могло измениться на сервере (например, другим клиентом) —
  // подхватываем, но только пока поле не редактируется активно.
  useEffect(() => {
    if (!saving) {
      setText(model.temperature === null ? '' : String(model.temperature));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [model.temperature]);

  async function save() {
    const trimmed = text.trim();
    // Пустое поле — осмысленная операция «снять настройку», а не ошибка
    // ввода: после неё поле перестаёт отправляться провайдеру.
    const value = trimmed === '' ? null : Number(trimmed);
    if (value !== null && Number.isNaN(value)) {
      setError('не число');
      return;
    }
    // Диапазон проверяем до сети — ради понятной ошибки, а не вместо
    // серверной проверки: backend отвергает то же самое сам (400).
    if (value !== null && (value < TEMPERATURE_MIN || value > TEMPERATURE_MAX)) {
      setError(`допустимо ${TEMPERATURE_MIN}–${TEMPERATURE_MAX}`);
      return;
    }

    setSaving(true);
    setError(null);
    try {
      const updated = await controlCenter.setModelTemperature(model.model, value);
      onSaved(updated);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'ошибка');
    } finally {
      setSaving(false);
    }
  }

  return (
    <span style={{ display: 'inline-flex', alignItems: 'center', gap: 4 }}>
      <input
        type="text"
        value={text}
        onChange={(e) => setText(e.target.value)}
        placeholder="по умолчанию"
        disabled={saving}
        style={{ width: 80, padding: 2, fontSize: 12 }}
      />
      <button onClick={save} disabled={saving} style={{ fontSize: 11 }}>
        {saving ? '...' : 'Сохранить'}
      </button>
      {error && <span style={{ color: '#ef4444', fontSize: 11 }}>{error}</span>}
    </span>
  );
}
