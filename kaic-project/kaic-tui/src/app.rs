//! Детерминированное состояние интерфейса. Разрешение всегда хранит исходный снимок.
use crate::protocol::{BackendStatus, ModelStatus, Task, TaskStatus, ToolApproval};
use uuid::Uuid;

pub const MAX_INPUT_BYTES: usize = 32_768;
pub const AGENT_STATUS: &str = "API не предоставляет статусы субагентов";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Focus {
    Sessions,
    Conversation,
    #[default]
    Input,
    Activity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Refresh,
    Create(String),
    Continue {
        id: Uuid,
        text: String,
    },
    Pause(Uuid),
    Cancel(Uuid),
    Decide {
        snapshot: ToolApproval,
        approve: bool,
    },
}

#[derive(Debug, Clone)]
pub struct ApprovalDialog {
    pub snapshot: ToolApproval,
    pub scroll: usize,
    pub reviewed: bool,
    pub prepared: Option<bool>,
    pub stale: bool,
}

#[derive(Debug)]
pub struct App {
    pub tasks: Vec<Task>,
    pub models: Vec<ModelStatus>,
    pub backend_status: Option<BackendStatus>,
    pub selected_task_id: Option<Uuid>,
    pub input: String,
    pub connected: bool,
    pub error: Option<String>,
    pub busy: bool,
    pub focus: Focus,
    pub help: bool,
    pub conversation_scroll: usize,
    pub activity_scroll: usize,
    pub new_conversation: bool,
    pub approval: Option<ApprovalDialog>,
    received_snapshot: bool,
    sent_draft: Option<String>,
    selection_epoch: u64,
    submission_epoch: u64,
    decision_in_flight: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            tasks: vec![],
            models: vec![],
            backend_status: None,
            selected_task_id: None,
            input: String::new(),
            connected: false,
            error: None,
            busy: false,
            focus: Focus::Input,
            help: false,
            conversation_scroll: 0,
            activity_scroll: 0,
            new_conversation: true,
            approval: None,
            received_snapshot: false,
            sent_draft: None,
            selection_epoch: 0,
            submission_epoch: 0,
            decision_in_flight: false,
        }
    }
}

impl App {
    pub fn receive_snapshot(
        &mut self,
        tasks: Vec<Task>,
        status: BackendStatus,
        models: Vec<ModelStatus>,
    ) {
        if !self.received_snapshot {
            self.selected_task_id = tasks.first().map(|task| task.id);
            self.new_conversation = self.selected_task_id.is_none();
        }
        self.received_snapshot = true;
        self.tasks = tasks;
        self.models = models;
        self.backend_status = Some(status);
        self.connected = true;
        self.invalidate_approval();
    }

    pub fn selected_task(&self) -> Option<&Task> {
        let id = self.selected_task_id?;
        self.tasks.iter().find(|task| task.id == id)
    }

    /// Обновление одной задачи не завершает текущую команду и не меняет выбор.
    pub fn receive_task(&mut self, task: Task) {
        if let Some(previous) = self
            .tasks
            .iter_mut()
            .find(|previous| previous.id == task.id)
        {
            *previous = task;
        } else {
            self.tasks.insert(0, task);
        }
        self.connected = true;
        self.invalidate_approval();
    }

    pub fn select_task(&mut self, id: Uuid) {
        if self.tasks.iter().any(|task| task.id == id) {
            self.selection_epoch = self.selection_epoch.wrapping_add(1);
            self.selected_task_id = Some(id);
            self.new_conversation = false;
            self.conversation_scroll = 0;
        }
    }

    pub fn select_next(&mut self) {
        self.move_selection(1);
    }

    pub fn select_previous(&mut self) {
        self.move_selection(-1);
    }

    fn move_selection(&mut self, offset: isize) {
        if self.tasks.is_empty() {
            return;
        }
        let position = self
            .tasks
            .iter()
            .position(|task| Some(task.id) == self.selected_task_id);
        let next = match position {
            Some(position) => {
                (position as isize + offset).rem_euclid(self.tasks.len() as isize) as usize
            }
            None if offset < 0 => self.tasks.len() - 1,
            None => 0,
        };
        self.select_task(self.tasks[next].id);
    }

    pub fn new_session(&mut self) {
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        self.received_snapshot = true;
        self.selected_task_id = None;
        self.new_conversation = true;
        self.input.clear();
        self.conversation_scroll = 0;
        self.focus = Focus::Input;
    }

    pub fn submit(&mut self) -> Option<Command> {
        if !self.connected || self.busy || self.approval.is_some() {
            return None;
        }
        if self.input.trim().is_empty()
            || self.input.len() > MAX_INPUT_BYTES
            || self.input.contains('\0')
        {
            self.error = Some("Введите непустой текст без NUL, не более 32768 байт".into());
            return None;
        }
        let command = if self.selected_task_id.is_none() {
            Command::Create(self.input.clone())
        } else {
            let task = self.selected_task()?;
            if task.pending_tool_approval.is_some()
                || task
                    .recovery
                    .as_ref()
                    .is_some_and(|r| r.kind == "unknown_outcome" || !r.unresolved_tools.is_empty())
                || !matches!(
                    task.status,
                    TaskStatus::Done | TaskStatus::Failed | TaskStatus::Paused
                )
            {
                self.error = Some("Текущая сессия не принимает продолжение".into());
                return None;
            }
            Command::Continue {
                id: task.id,
                text: self.input.clone(),
            }
        };
        self.sent_draft = Some(self.input.clone());
        self.submission_epoch = self.selection_epoch;
        self.begin();
        Some(command)
    }

    pub fn success(&mut self, task: Option<Task>) {
        if let Some(task) = task {
            if self.sent_draft.is_some() && self.selection_epoch == self.submission_epoch {
                self.selected_task_id = Some(task.id);
                self.new_conversation = false;
            }
            if let Some(previous) = self
                .tasks
                .iter_mut()
                .find(|previous| previous.id == task.id)
            {
                *previous = task;
            } else {
                self.tasks.insert(0, task);
            }
        }
        if let Some(sent) = self.sent_draft.take() {
            if self.input == sent && self.selection_epoch == self.submission_epoch {
                self.input.clear();
            }
        }
        if self.decision_in_flight {
            self.approval = None;
        }
        self.decision_in_flight = false;
        self.busy = false;
        self.error = None;
        self.invalidate_approval();
    }

    /// Ошибка сохраняет последнюю историю и черновик; разрешения требуют нового открытия.
    pub fn fail(&mut self, error: String) {
        self.connected = false;
        self.busy = false;
        self.error = Some(error);
        self.sent_draft = None;
        self.decision_in_flight = false;
        if let Some(dialog) = &mut self.approval {
            dialog.stale = true;
            dialog.prepared = None;
        }
    }

    pub fn open_approval(&mut self) -> bool {
        if !self.connected || self.busy || self.approval.is_some() {
            return false;
        }
        let Some(task) = self.selected_task() else {
            return false;
        };
        let Some(snapshot) = &task.pending_tool_approval else {
            return false;
        };
        if task.status != TaskStatus::WaitingForHuman || snapshot.task_id != task.id {
            return false;
        }
        self.approval = Some(ApprovalDialog {
            snapshot: snapshot.clone(),
            scroll: 0,
            reviewed: false,
            prepared: None,
            stale: false,
        });
        true
    }

    pub fn mark_approval_reviewed(&mut self) {
        if let Some(dialog) = &mut self.approval {
            if !dialog.stale {
                dialog.reviewed = true;
            }
        }
    }

    pub fn prepare_approval(&mut self, approve: bool) -> bool {
        self.invalidate_approval();
        if !self.connected || self.busy {
            return false;
        }
        let Some(dialog) = &mut self.approval else {
            return false;
        };
        if dialog.stale || (approve && !dialog.reviewed) {
            return false;
        }
        dialog.prepared = Some(approve);
        true
    }

    pub fn confirm_approval(&mut self) -> Option<Command> {
        self.invalidate_approval();
        if !self.connected || self.busy {
            return None;
        }
        let dialog = self.approval.as_ref()?;
        let approve = dialog.prepared?;
        if dialog.stale || (approve && !dialog.reviewed) {
            return None;
        }
        let command = Command::Decide {
            snapshot: dialog.snapshot.clone(),
            approve,
        };
        self.begin();
        self.decision_in_flight = true;
        Some(command)
    }

    pub fn close_approval(&mut self) {
        if !self.busy {
            self.approval = None;
        }
    }

    pub fn pause(&mut self) -> Option<Command> {
        if !self.connected || self.busy {
            return None;
        }
        let task = self.selected_task()?;
        if task.status != TaskStatus::Running {
            return None;
        }
        let command = Command::Pause(task.id);
        self.begin();
        Some(command)
    }

    pub fn cancel(&mut self) -> Option<Command> {
        if !self.connected || self.busy {
            return None;
        }
        let task = self.selected_task()?;
        if !matches!(
            task.status,
            TaskStatus::Running | TaskStatus::Paused | TaskStatus::WaitingForHuman
        ) {
            return None;
        }
        let command = Command::Cancel(task.id);
        self.begin();
        Some(command)
    }

    fn begin(&mut self) {
        self.busy = true;
        self.error = None;
    }

    fn invalidate_approval(&mut self) {
        let Some(dialog) = self.approval.as_mut() else {
            return;
        };
        let current = self
            .tasks
            .iter()
            .find(|task| task.id == dialog.snapshot.task_id);
        if !self.connected
            || !current.is_some_and(|task| {
                task.status == TaskStatus::WaitingForHuman
                    && task.pending_tool_approval.as_ref() == Some(&dialog.snapshot)
            })
        {
            dialog.stale = true;
            dialog.prepared = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ToolServerIdentity;

    fn task(id: u128, status: TaskStatus) -> Task {
        Task {
            id: Uuid::from_u128(id),
            category: "Simple".into(),
            status,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            context: vec![],
            pending_tool_approval: None,
            recovery: None,
        }
    }
    fn waiting(id: u128) -> Task {
        let mut task = task(id, TaskStatus::WaitingForHuman);
        task.pending_tool_approval = Some(ToolApproval {
            request_id: Uuid::from_u128(id + 100),
            task_id: task.id,
            execution_id: Uuid::from_u128(id + 200),
            tool_call_id: format!("call-{id}"),
            tool_name: "read_file".into(),
            arguments: "{\"path\":\"a.rs\"}".into(),
            server: ToolServerIdentity {
                command: "local-mcp".into(),
                args: vec!["--safe".into()],
                peer_name: "files".into(),
            },
        });
        task
    }
    fn snapshot(app: &mut App, tasks: Vec<Task>) {
        app.receive_snapshot(
            tasks,
            BackendStatus {
                total_model_memory_mb: 10,
                used_model_memory_mb: 0,
                loaded_models: vec![],
                running_tasks: 0,
                max_concurrent_tasks: 2,
                busy_models: vec![],
                active_operation: None,
                recovery_warnings: vec![],
            },
            vec![],
        );
    }
    fn ready(tasks: Vec<Task>) -> App {
        let mut app = App::default();
        snapshot(&mut app, tasks);
        app
    }

    #[test]
    fn unknown_outcome_blocks_continuation_even_when_paused() {
        let mut task = waiting(1);
        task.status = TaskStatus::Paused;
        task.recovery = Some(crate::protocol::TaskRecovery {
            kind: "unknown_outcome".into(),
            message: "Проверьте результат".into(),
            unresolved_tools: vec![task.pending_tool_approval.take().unwrap()],
            acknowledgement_id: None,
        });
        let mut app = ready(vec![task.clone()]);
        app.input = "продолжить".into();
        assert!(app.submit().is_none());
        task.recovery = Some(crate::protocol::TaskRecovery {
            kind: "interrupted".into(),
            message: "Исполнение прервано".into(),
            unresolved_tools: vec![],
            acknowledgement_id: None,
        });
        app.receive_task(task);
        assert!(matches!(app.submit(), Some(Command::Continue { .. })));
    }

    #[test]
    fn single_task_poll_invalidates_changed_target_without_retargeting_or_finishing_command() {
        let original = waiting(1);
        let mut app = ready(vec![original.clone(), waiting(2)]);
        assert!(app.open_approval());
        app.select_task(Uuid::from_u128(2));
        app.busy = true;
        let mut changed = original;
        changed.pending_tool_approval.as_mut().unwrap().arguments = "другие аргументы".into();
        app.receive_task(changed);
        assert!(app.approval.as_ref().unwrap().stale);
        assert_eq!(
            app.approval.as_ref().unwrap().snapshot.task_id,
            Uuid::from_u128(1)
        );
        assert_eq!(app.selected_task_id, Some(Uuid::from_u128(2)));
        assert!(app.busy);
    }

    #[test]
    fn first_snapshot_selects_latest_then_new_session_survives_refresh() {
        let mut app = ready(vec![task(2, TaskStatus::Done), task(1, TaskStatus::Done)]);
        assert_eq!(app.selected_task_id, Some(Uuid::from_u128(2)));
        app.new_session();
        snapshot(&mut app, vec![task(3, TaskStatus::Done)]);
        assert!(app.selected_task().is_none());
        assert!(app.new_conversation);
        app.select_task(Uuid::from_u128(3));
        assert_eq!(app.selected_task().unwrap().id, Uuid::from_u128(3));
    }

    #[test]
    fn selected_session_is_preserved_when_list_order_changes() {
        let mut app = ready(vec![task(1, TaskStatus::Done), task(2, TaskStatus::Done)]);
        app.select_next();
        snapshot(
            &mut app,
            vec![task(3, TaskStatus::Done), task(2, TaskStatus::Done)],
        );
        assert_eq!(app.selected_task_id, Some(Uuid::from_u128(2)));
        app.select_previous();
        assert_eq!(app.selected_task_id, Some(Uuid::from_u128(3)));
    }

    #[test]
    fn disconnect_keeps_history_and_draft() {
        let mut app = ready(vec![task(1, TaskStatus::Done)]);
        app.input = "Черновик".into();
        app.fail("Недоступно".into());
        assert!(!app.connected);
        assert_eq!(app.tasks.len(), 1);
        assert_eq!(app.input, "Черновик");
        assert!(app.submit().is_none());
        assert!(AGENT_STATUS.contains("API не предоставляет"));
    }

    #[test]
    fn invalid_input_is_never_submitted_and_limit_counts_utf8_bytes() {
        let mut app = ready(vec![]);
        for input in [" ", "a\0b"] {
            app.input = input.into();
            assert!(app.submit().is_none());
        }
        app.input = "я".repeat(MAX_INPUT_BYTES / 2 + 1);
        assert!(app.submit().is_none());
        app.input = "я".repeat(MAX_INPUT_BYTES / 2);
        assert!(matches!(app.submit(), Some(Command::Create(_))));
    }

    #[test]
    fn failures_preserve_draft_and_success_does_not_erase_new_typing() {
        let mut app = ready(vec![]);
        app.input = "first".into();
        app.submit().unwrap();
        app.fail("Ошибка".into());
        assert_eq!(app.input, "first");
        snapshot(&mut app, vec![]);
        app.submit().unwrap();
        app.input.push_str(" and second");
        app.success(Some(task(1, TaskStatus::Running)));
        assert_eq!(app.input, "first and second");
        assert!(!app.busy);
        assert_eq!(app.selected_task_id, Some(Uuid::from_u128(1)));
    }

    #[test]
    fn new_session_during_create_is_not_retargeted_by_late_success() {
        let mut app = ready(vec![]);
        app.input = "first".into();
        app.submit().unwrap();
        app.new_session();
        app.input = "first".into();
        app.success(Some(task(1, TaskStatus::Running)));
        assert!(app.selected_task_id.is_none());
        assert_eq!(app.input, "first");
        assert!(app.new_conversation);
    }

    #[test]
    fn successful_submission_clears_only_sent_draft() {
        let mut app = ready(vec![task(1, TaskStatus::Done)]);
        app.input = "continue".into();
        assert!(
            matches!(app.submit(), Some(Command::Continue { id, .. }) if id == Uuid::from_u128(1))
        );
        app.success(Some(task(1, TaskStatus::Running)));
        assert_eq!(app.input, "");
    }

    #[test]
    fn running_cancelled_and_waiting_do_not_accept_continuation() {
        for status in [
            TaskStatus::Running,
            TaskStatus::Cancelled,
            TaskStatus::WaitingForHuman,
        ] {
            let mut app = ready(vec![task(1, status)]);
            app.input = "hello".into();
            assert!(app.submit().is_none());
            assert!(!app.busy);
        }
        for status in [TaskStatus::Done, TaskStatus::Failed, TaskStatus::Paused] {
            let mut app = ready(vec![task(1, status)]);
            app.input = "hello".into();
            assert!(matches!(app.submit(), Some(Command::Continue { .. })));
        }
    }

    #[test]
    fn approval_stays_bound_to_task_a_after_selection_of_task_b() {
        let mut app = ready(vec![waiting(1), waiting(2)]);
        assert!(app.open_approval());
        let original = app.approval.as_ref().unwrap().snapshot.clone();
        app.select_next();
        app.mark_approval_reviewed();
        assert!(app.prepare_approval(true));
        assert_eq!(
            app.confirm_approval(),
            Some(Command::Decide {
                snapshot: original,
                approve: true
            })
        );
        assert!(app.confirm_approval().is_none());
    }

    #[test]
    fn allow_requires_review_and_two_separate_actions() {
        let mut app = ready(vec![waiting(1)]);
        app.open_approval();
        assert!(!app.prepare_approval(true));
        assert!(app.confirm_approval().is_none());
        app.mark_approval_reviewed();
        assert!(app.confirm_approval().is_none());
        assert!(app.prepare_approval(true));
        assert!(app.confirm_approval().is_some());
    }

    #[test]
    fn denial_does_not_require_viewing_all_arguments() {
        let mut app = ready(vec![waiting(1)]);
        app.open_approval();
        assert!(app.prepare_approval(false));
        assert!(matches!(
            app.confirm_approval(),
            Some(Command::Decide { approve: false, .. })
        ));
    }

    #[test]
    fn any_approval_target_change_permanently_invalidates_open_dialog() {
        let original = waiting(1);
        for field in 0..9 {
            let mut changed = original.clone();
            let pending = changed.pending_tool_approval.as_mut().unwrap();
            match field {
                0 => pending.request_id = Uuid::from_u128(999),
                1 => pending.task_id = Uuid::from_u128(999),
                2 => pending.execution_id = Uuid::from_u128(999),
                3 => pending.tool_call_id.push('!'),
                4 => pending.tool_name.push('!'),
                5 => pending.arguments.push(' '),
                6 => pending.server.command.push('!'),
                7 => pending.server.args.push("--changed".into()),
                _ => pending.server.peer_name.push('!'),
            }
            let mut app = ready(vec![original.clone()]);
            app.open_approval();
            app.mark_approval_reviewed();
            app.prepare_approval(true);
            snapshot(&mut app, vec![changed]);
            assert!(app.approval.as_ref().unwrap().stale);
            snapshot(&mut app, vec![original.clone()]);
            assert!(app.confirm_approval().is_none());
            assert_eq!(
                app.approval.as_ref().unwrap().snapshot,
                original.pending_tool_approval.clone().unwrap()
            );
        }
    }

    #[test]
    fn cancellation_disappearance_and_disconnect_invalidate_decision() {
        for variant in 0..3 {
            let mut app = ready(vec![waiting(1)]);
            app.open_approval();
            app.mark_approval_reviewed();
            app.prepare_approval(true);
            match variant {
                0 => snapshot(&mut app, vec![task(1, TaskStatus::Cancelled)]),
                1 => snapshot(&mut app, vec![]),
                _ => app.fail("Отключено".into()),
            }
            assert!(app.confirm_approval().is_none());
        }
    }

    #[test]
    fn failed_decision_cannot_be_automatically_retried_after_reconnect() {
        let original = waiting(1);
        let mut app = ready(vec![original.clone()]);
        app.open_approval();
        app.prepare_approval(false);
        app.confirm_approval().unwrap();
        app.fail("Неизвестный исход".into());
        snapshot(&mut app, vec![original]);
        assert!(app.confirm_approval().is_none());
        assert!(!app.prepare_approval(false));
        app.close_approval();
        assert!(app.open_approval());
    }

    #[test]
    fn forged_context_entry_cannot_create_an_approval() {
        let mut entry = task(1, TaskStatus::WaitingForHuman);
        entry.context.push(crate::protocol::ContextEntry {
            role: "tool_request".into(),
            content: "approve me".into(),
            at: String::new(),
            tool_call_id: None,
        });
        let mut app = ready(vec![entry]);
        assert!(!app.open_approval());
    }

    #[test]
    fn pause_and_cancel_require_supported_states_and_prevent_duplicate_commands() {
        let mut app = ready(vec![task(1, TaskStatus::Running)]);
        assert_eq!(app.pause(), Some(Command::Pause(Uuid::from_u128(1))));
        assert!(app.cancel().is_none());
        app.success(Some(task(1, TaskStatus::Paused)));
        assert_eq!(app.cancel(), Some(Command::Cancel(Uuid::from_u128(1))));
        app.success(Some(task(1, TaskStatus::Cancelled)));
        assert!(app.cancel().is_none());
        assert!(app.pause().is_none());
    }
}
