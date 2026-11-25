//! DTO существующего Control Center API; неизвестные возможности не выдумываются.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
pub struct InferenceBackend {
    pub name: String,
    pub externally_managed: bool,
    pub discovery_supported: bool,
    pub connected: Option<bool>,
    pub models: Vec<DiscoveredModel>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub architecture: Option<serde_json::Value>,
    pub meta: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocalModels {
    pub root: String,
    pub models: Vec<LocalModel>,
    pub rejected: Vec<RejectedLocalModel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocalModel {
    pub id: String,
    pub path: String,
    pub size_bytes: u64,
    pub gguf_version: u32,
    pub tensor_count: u64,
    pub architecture: Option<String>,
    pub file_type: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RejectedLocalModel {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    WaitingForHuman,
    Paused,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContextEntry {
    pub role: String,
    pub content: String,
    pub at: String,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolServerIdentity {
    pub command: String,
    pub args: Vec<String>,
    pub peer_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolApproval {
    pub request_id: Uuid,
    pub task_id: Uuid,
    pub execution_id: Uuid,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: String,
    pub server: ToolServerIdentity,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Task {
    pub id: Uuid,
    pub category: String,
    pub status: TaskStatus,
    pub created_at: String,
    pub updated_at: String,
    pub context: Vec<ContextEntry>,
    pub pending_tool_approval: Option<ToolApproval>,
    #[serde(default)]
    pub recovery: Option<TaskRecovery>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TaskRecovery {
    pub kind: String,
    pub message: String,
    pub unresolved_tools: Vec<ToolApproval>,
    #[serde(default)]
    pub acknowledgement_id: Option<Uuid>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelStatus {
    pub model: String,
    pub vram_mb: u32,
    pub ram_offload_mb: u32,
    pub always_loaded: bool,
    pub preferred_device: String,
    pub loaded: bool,
    pub last_used: Option<String>,
    pub context_length: Option<u32>,
    pub max_context_length: Option<u32>,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoadedModel {
    pub model: String,
    pub last_used: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BusyModel {
    pub model: String,
    pub generations: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActiveOperation {
    pub kind: String,
    pub model: String,
    pub reason: String,
    pub started_at: String,
    pub elapsed_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackendStatus {
    pub total_model_memory_mb: u32,
    pub used_model_memory_mb: u32,
    pub loaded_models: Vec<LoadedModel>,
    pub running_tasks: usize,
    pub max_concurrent_tasks: usize,
    pub busy_models: Vec<BusyModel>,
    pub active_operation: Option<ActiveOperation>,
    #[serde(default)]
    pub recovery_warnings: Vec<String>,
}
