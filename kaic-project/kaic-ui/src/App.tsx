import { useEffect, useState } from 'react';
import './App.css';
import Header from './components/Header';
import TaskList from './components/TaskList';
import Workspace from './components/Workspace';
import DetailsPanel from './components/DetailsPanel';
import StatusBar from './components/StatusBar';
import { controlCenter, type Task } from './api/controlCenter';
import type { PendingTask } from './types';

const POLL_INTERVAL_MS = 1000;

// Каркас Layout'а по утверждённому wireframe:
//   Header
//   TaskList | Workspace | DetailsPanel
//   StatusBar
//
// Общее состояние между компонентами — ровно два случая, оба возникли
// по факту реальной необходимости, а не заранее:
//   1. pendingTasks — Header создаёт placeholder, TaskList его снимает.
//   2. selectedTask(Id) — TaskList выбирает задачу, Workspace и
//      DetailsPanel показывают одни и те же её данные. Именно поэтому
//      данные выбранной задачи опрашиваются здесь, в одном месте,
//      а не дублируются в Workspace и DetailsPanel по отдельности.
export default function App() {
  const [pendingTasks, setPendingTasks] = useState<PendingTask[]>([]);
  const [selectedTaskId, setSelectedTaskId] = useState<string | null>(null);
  const [selectedTask, setSelectedTask] = useState<Task | null>(null);

  function addPendingTask(pending: PendingTask) {
    setPendingTasks((prev) => [pending, ...prev]);
  }

  function setPendingTaskRealId(tempId: string, realTaskId: string) {
    setPendingTasks((prev) =>
      prev.map((p) => (p.id === tempId ? { ...p, realTaskId } : p)),
    );
  }

  function removePendingTask(id: string) {
    setPendingTasks((prev) => prev.filter((p) => p.id !== id));
  }

  // Поллинг выбранной задачи целиком — отдельно от списка в TaskList,
  // потому что это другой запрос (GET /tasks/:id) с другим объёмом
  // данных (полный context конкретной задачи, а не сводка по всем).
  useEffect(() => {
    if (!selectedTaskId) {
      setSelectedTask(null);
      return;
    }

    let cancelled = false;

    async function loadSelectedTask() {
      try {
        const task = await controlCenter.getTask(selectedTaskId as string);
        if (!cancelled) setSelectedTask(task);
      } catch {
        // Молча оставляем прежние данные до следующего тика — TaskList
        // уже показывает общую ошибку соединения, дублировать её здесь
        // не нужно.
      }
    }

    loadSelectedTask();
    const intervalId = setInterval(loadSelectedTask, POLL_INTERVAL_MS);

    return () => {
      cancelled = true;
      clearInterval(intervalId);
    };
  }, [selectedTaskId]);

  return (
    <div className="app-layout">
      <Header
        onTaskCreated={addPendingTask}
        onTaskIdKnown={setPendingTaskRealId}
        onTaskFailed={removePendingTask}
      />
      <TaskList
        pendingTasks={pendingTasks}
        onPendingArrived={removePendingTask}
        selectedTaskId={selectedTaskId}
        onSelectTask={setSelectedTaskId}
      />
      <Workspace task={selectedTask} />
      <DetailsPanel task={selectedTask} />
      <StatusBar />
    </div>
  );
}
