//! Ограниченное делегирование подзадач (до 2 подзадач, права файлов и кода не выше родительских,
//! без гонок записи, с фиксацией SHA-256 артефактов для точечной верификации).
use super::{
    AgentLimits, AgentModelBackend, AgentPolicy, AgentStreamEvent, ApprovalGate, ExecPolicy,
    ProviderKind, WorkspaceGuard, run_agent_inner,
};
use crate::network::Cancel;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

pub const MAX_SUBTASKS: usize = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubtaskSpec {
    pub title: String,
    pub instructions: String,
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub exec_policy: Option<String>,
    #[serde(default)]
    pub owned_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubtaskRecord {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    pub requested_policy: AgentPolicy,
    pub effective_policy: AgentPolicy,
    #[serde(default)]
    pub owned_paths: Vec<String>,
    pub provider: ProviderKind,
    pub model: String,
    pub status: String,
    pub summary: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
    /// SHA-256 версий файлов на момент завершения подзадачи.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifact_hashes: BTreeMap<String, String>,
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_evidence: Option<String>,
    /// Точные SHA-256 версий файлов, к которым относится `verified = true`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub verified_file_hashes: BTreeMap<String, String>,
}

pub fn parse_subtasks(args: &Value) -> Result<Vec<SubtaskSpec>> {
    let raw = args["subtasks"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Для delegate_subtasks требуется массив subtasks"))?;
    if raw.is_empty() {
        bail!("Список подзадач не может быть пустым");
    }
    if raw.len() > MAX_SUBTASKS {
        bail!(
            "Превышен лимит подзадач: запрошено {}, максимум {MAX_SUBTASKS}",
            raw.len()
        );
    }
    let mut specs = Vec::with_capacity(raw.len());
    for item in raw {
        let spec: SubtaskSpec = serde_json::from_value(item.clone())
            .map_err(|e| anyhow::anyhow!("Некорректное описание подзадачи: {e}"))?;
        if spec.title.trim().is_empty() || spec.instructions.trim().is_empty() {
            bail!("Каждая подзадача должна иметь непустые title и instructions");
        }
        specs.push(spec);
    }
    Ok(specs)
}

/// Проверяем отсутствие гонки писателей: если подзадачи претендуют на запись,
/// их `owned_paths` должны быть явно заданы и не пересекаться.
pub fn validate_subtask_ownership(specs: &[SubtaskSpec]) -> Result<()> {
    let mut write_paths: HashSet<String> = HashSet::new();
    let mut writer_count = 0usize;
    for spec in specs {
        let req_policy = spec
            .policy
            .as_deref()
            .map(AgentPolicy::parse_str)
            .transpose()?
            .unwrap_or(AgentPolicy::ReadOnly);
        if req_policy > AgentPolicy::ReadOnly {
            writer_count += 1;
            if specs.len() > 1 && spec.owned_paths.is_empty() {
                bail!(
                    "Отклонено (конфликт владения путями): пишущая подзадача «{}» должна явно указать непересекающиеся owned_paths",
                    spec.title
                );
            }
            for p in &spec.owned_paths {
                let norm = normalize_rel(p);
                if (norm.is_empty() || norm == ".") && (writer_count > 1 || specs.len() > 1) {
                    bail!(
                        "Отклонено (конфликт владения путями): широкий путь «{p}» в подзадаче «{}»",
                        spec.title
                    );
                }
                for existing in &write_paths {
                    if paths_overlap(existing, &norm) {
                        bail!(
                            "Отклонено (конфликт владения путями): «{norm}» пересекается с «{existing}»; параллельная запись в один файл запрещена"
                        );
                    }
                }
                write_paths.insert(norm);
            }
        }
    }
    Ok(())
}

pub fn normalize_rel(p: &str) -> String {
    p.trim()
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

pub fn paths_overlap(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// Направленная проверка: находится ли целевой путь `target` строго внутри `owned`
/// (равен ему или является его подпутём), без симметричного ложного совпадения.
pub fn is_path_owned(owned: &str, target: &str) -> bool {
    target == owned || target.starts_with(&format!("{owned}/"))
}

#[allow(clippy::too_many_arguments)]
pub async fn run_subtasks(
    backend: &mut dyn AgentModelBackend,
    guard: &WorkspaceGuard,
    parent_id: &str,
    parent_file_policy: AgentPolicy,
    parent_exec_policy: ExecPolicy,
    specs: Vec<SubtaskSpec>,
    cancel: &Cancel,
    approvals: &ApprovalGate,
    emit: &mut dyn FnMut(AgentStreamEvent),
) -> Result<(Vec<SubtaskRecord>, Value)> {
    validate_subtask_ownership(&specs)?;
    let mut records = Vec::with_capacity(specs.len());
    let mut reports = Vec::with_capacity(specs.len());

    // Локальная GPU/RAM общая: дочерние задачи делят один резидентный backend последовательно.
    for (idx, spec) in specs.into_iter().enumerate() {
        if cancel.is_cancelled() {
            let rec = SubtaskRecord {
                id: format!("{parent_id}-sub-{}", idx + 1),
                parent_id: parent_id.to_string(),
                title: spec.title,
                requested_policy: spec
                    .policy
                    .as_deref()
                    .and_then(|s| AgentPolicy::parse_str(s).ok())
                    .unwrap_or(AgentPolicy::ReadOnly),
                effective_policy: AgentPolicy::ReadOnly,
                owned_paths: spec.owned_paths,
                provider: backend.provider_kind(),
                model: backend.model_label().to_string(),
                status: "cancelled".into(),
                summary: "Отменено вместе с родительской задачей".into(),
                changed_files: vec![],
                artifact_hashes: BTreeMap::new(),
                verified: false,
                verification_evidence: None,
                verified_file_hashes: BTreeMap::new(),
            };
            emit(AgentStreamEvent::Subtask(rec.clone()));
            records.push(rec);
            continue;
        }

        let requested_policy = spec
            .policy
            .as_deref()
            .map(AgentPolicy::parse_str)
            .transpose()?
            .unwrap_or(AgentPolicy::ReadOnly);
        let requested_exec = spec
            .exec_policy
            .as_deref()
            .map(ExecPolicy::parse_str)
            .transpose()?
            .unwrap_or(parent_exec_policy);
        // Жёсткое правило: дочерний агент НИКОГДА не получает больше прав (ни файловых, ни кода), чем родитель.
        let effective_policy = parent_file_policy.cap(requested_policy);
        let effective_exec = parent_exec_policy.cap(requested_exec);
        let sub_id = format!("{parent_id}-sub-{}", idx + 1);
        let normalized_owned: Vec<String> =
            spec.owned_paths.iter().map(|p| normalize_rel(p)).collect();

        let running_rec = SubtaskRecord {
            id: sub_id.clone(),
            parent_id: parent_id.to_string(),
            title: spec.title.clone(),
            requested_policy,
            effective_policy,
            owned_paths: normalized_owned.clone(),
            provider: backend.provider_kind(),
            model: backend.model_label().to_string(),
            status: "running".into(),
            summary: "Выполняется…".into(),
            changed_files: vec![],
            artifact_hashes: BTreeMap::new(),
            verified: false,
            verification_evidence: None,
            verified_file_hashes: BTreeMap::new(),
        };
        emit(AgentStreamEvent::Subtask(running_rec));

        let sub_limits = AgentLimits {
            max_steps: 5,
            max_wall_secs: 120,
            max_repeat_failures: 2,
            max_tool_output_bytes: 12 * 1024,
            ..AgentLimits::default()
        };
        let owned_filter = if normalized_owned.is_empty() {
            None
        } else {
            Some(normalized_owned.clone())
        };
        let sub_prompt = format!(
            "Подзадача {sub_id} ({title}).\nИнструкции: {instr}\nОграничение прав файлов: {pol}; исполнения кода: {epol}. Ответь кратко по фактам из инструментов.",
            title = spec.title,
            instr = spec.instructions,
            pol = effective_policy.label_en(),
            epol = effective_exec.label_en()
        );

        let sub_seat_label = format!("agent-{} ({})", idx + 2, spec.title);
        let sub_res = run_agent_inner(
            backend,
            guard,
            &sub_id,
            (idx + 1) as u8,
            &sub_seat_label,
            &sub_prompt,
            &[],
            effective_policy,
            effective_exec,
            sub_limits,
            false,
            owned_filter.as_deref(),
            cancel,
            approvals,
            emit,
        )
        .await;

        let rec = match sub_res {
            Ok(res) => {
                let status = if res.cancelled {
                    "cancelled"
                } else if res.blocked {
                    "blocked"
                } else {
                    "completed_unverified"
                };
                // Фиксируем точные SHA-256 версий файлов на момент завершения подзадачи.
                let mut artifact_hashes = BTreeMap::new();
                let targets: Vec<String> = if !res.changed_files.is_empty() {
                    res.changed_files.clone()
                } else {
                    normalized_owned.clone()
                };
                for t in &targets {
                    if let Ok((rel, sha)) = guard.hash_file_no_follow(t) {
                        artifact_hashes.insert(normalize_rel(&rel), sha);
                    }
                }
                SubtaskRecord {
                    id: sub_id.clone(),
                    parent_id: parent_id.to_string(),
                    title: spec.title.clone(),
                    requested_policy,
                    effective_policy,
                    owned_paths: normalized_owned,
                    provider: backend.provider_kind(),
                    model: backend.model_label().to_string(),
                    status: status.into(),
                    summary: res.final_text,
                    changed_files: res.changed_files,
                    artifact_hashes,
                    // Итог ребёнка всегда стартует как неподтверждённый до проверки оркестратором!
                    verified: false,
                    verification_evidence: None,
                    verified_file_hashes: BTreeMap::new(),
                }
            }
            Err(e) => SubtaskRecord {
                id: sub_id.clone(),
                parent_id: parent_id.to_string(),
                title: spec.title.clone(),
                requested_policy,
                effective_policy,
                owned_paths: normalized_owned,
                provider: backend.provider_kind(),
                model: backend.model_label().to_string(),
                status: "failed".into(),
                summary: format!("Ошибка подзадачи: {e:#}"),
                changed_files: vec![],
                artifact_hashes: BTreeMap::new(),
                verified: false,
                verification_evidence: None,
                verified_file_hashes: BTreeMap::new(),
            },
        };
        emit(AgentStreamEvent::Subtask(rec.clone()));
        reports.push(json!({
            "id": rec.id,
            "parent_id": rec.parent_id,
            "title": rec.title,
            "effective_policy": rec.effective_policy,
            "provider": rec.provider,
            "model": rec.model,
            "status": rec.status,
            "verified": rec.verified,
            "artifact_hashes": rec.artifact_hashes,
            "unverified_notice": "Итог дочерней задачи не подтверждён; оркестратор должен прочитать текущие версии файлов и прогнать целевую проверку, покрывающую эти файлы.",
            "changed_files": rec.changed_files,
            "summary": rec.summary,
        }));
        records.push(rec);
    }

    Ok((records, json!({ "subtask_results": reports })))
}
