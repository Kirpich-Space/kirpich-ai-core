use anyhow::Result;

use crate::agent::planner;
use crate::contract::{AgentIdentity, GateResult, ParsedPlannerOutput, PassReport};
use crate::core::engine::{ChatParams, KaicEngine};

const PLANNER_ID: &str = "planner_v1";
const PLANNER_VERSION: u32 = 1;

/// Runs one agent pass: Planner → gate → (Coder not implemented).
///
/// `model_label` fills `AgentIdentity.model` (KaicEngine does not expose a model id;
/// callers pass `Config.model_path` or another configured label).
pub fn run_pass(
    engine: &KaicEngine,
    task: &str,
    params: &ChatParams,
    model_label: &str,
) -> Result<PassReport> {
    let parsed = planner::plan(engine, task, params)?;
    Ok(run_pass_from_parsed(parsed, model_label))
}

/// Gate + PassReport assembly without calling the LLM (unit-testable).
pub fn run_pass_from_parsed(parsed: ParsedPlannerOutput, model_label: &str) -> PassReport {
    let report = PassReport {
        agent_identity: AgentIdentity {
            id: PLANNER_ID.to_string(),
            version: PLANNER_VERSION,
            model: model_label.to_string(),
        },
        plan: parsed.plan,
        gate_result: parsed.gate,
    };

    match &report.gate_result {
        GateResult::Ready => {
            // TODO: Coder ещё не реализован — точка вызова после Ready gate.
            // Do not simulate execution.
        }
        GateResult::NeedClarification { .. } => {
            // Pass ends here: Coder is not invoked.
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{GateResult, Plan};

    #[test]
    fn ready_gate_keeps_plan_and_does_not_pretend_coder_ran() {
        let parsed = ParsedPlannerOutput {
            plan: Some(Plan {
                steps: vec!["inspect README".into(), "propose edit".into()],
            }),
            gate: GateResult::Ready,
        };

        let report = run_pass_from_parsed(parsed, "test-model.gguf");

        assert_eq!(report.agent_identity.id, "planner_v1");
        assert_eq!(report.agent_identity.version, 1);
        assert_eq!(report.agent_identity.model, "test-model.gguf");
        assert!(report.plan.is_some());
        assert_eq!(report.gate_result, GateResult::Ready);
        // Coder is not implemented: report has no execution payload by design.
    }

    #[test]
    fn need_clarification_ends_pass_with_questions() {
        let questions = vec!["Which file?".into(), "What style?".into()];
        let parsed = ParsedPlannerOutput {
            plan: Some(Plan {
                steps: vec!["unclear draft".into()],
            }),
            gate: GateResult::NeedClarification {
                clarification_questions: questions.clone(),
            },
        };

        let report = run_pass_from_parsed(parsed, "test-model.gguf");

        assert_eq!(report.agent_identity.id, "planner_v1");
        assert!(report.plan.is_some());
        match report.gate_result {
            GateResult::NeedClarification {
                clarification_questions,
            } => assert_eq!(clarification_questions, questions),
            GateResult::Ready => panic!("Coder must not be considered for NeedClarification"),
        }
    }
}
