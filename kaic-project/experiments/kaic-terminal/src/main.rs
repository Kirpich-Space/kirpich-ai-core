mod agent;
mod contract;
mod core;
mod git_diff;
mod git_log;
mod git_show;
mod git_status;
#[cfg(test)]
mod git_test_support;
mod repl;
mod tools;

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::contract::{GateResult, PassReport};
use crate::core::config::Config;
use crate::core::context;
use crate::core::engine::{ChatParams, KaicEngine};

#[derive(Debug, PartialEq, Eq)]
enum CliCommand {
    Repl,
    Plan { task: String },
}

#[derive(Debug, PartialEq, Eq)]
struct Cli {
    project_path: String,
    command: CliCommand,
}

fn parse_cli(args: &[&str]) -> Result<Cli> {
    let mut project_path: Option<String> = None;
    let mut command = CliCommand::Repl;
    let mut index = 0;

    while index < args.len() {
        match args[index] {
            "--plan" => {
                let task = args
                    .get(index + 1)
                    .filter(|task| !task.trim().is_empty())
                    .context("--plan требует задачу: kaic --plan \"<задача>\" [проект]")?;
                if !matches!(command, CliCommand::Repl) {
                    anyhow::bail!("команда --plan указана больше одного раза");
                }
                command = CliCommand::Plan {
                    task: (*task).to_string(),
                };
                index += 2;
            }
            value if value.starts_with('-') => {
                anyhow::bail!("неизвестный аргумент: {value}");
            }
            value => {
                if project_path.replace(value.to_string()).is_some() {
                    anyhow::bail!("можно указать только один путь к проекту");
                }
                index += 1;
            }
        }
    }

    Ok(Cli {
        project_path: project_path.unwrap_or_else(|| ".".to_string()),
        command,
    })
}

fn render_pass_report(report: &PassReport) -> String {
    let mut output = format!(
        "Planner: {} v{} ({})\n",
        report.agent_identity.id, report.agent_identity.version, report.agent_identity.model
    );

    match &report.gate_result {
        GateResult::Ready => {
            output.push_str("План готов:\n");
            if let Some(plan) = &report.plan {
                for (index, step) in plan.steps.iter().enumerate() {
                    output.push_str(&format!("{}. {}\n", index + 1, step));
                }
            }
            output.push_str("Coder ещё не запускался; изменения в файлы не внесены.\n");
        }
        GateResult::NeedClarification {
            clarification_questions,
        } => {
            output.push_str("Нужно уточнение:\n");
            for question in clarification_questions {
                output.push_str(&format!("- {question}\n"));
            }
        }
    }

    output
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("kaic {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let cli_args: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let cli = parse_cli(&cli_args)?;

    let project_root: PathBuf = PathBuf::from(&cli.project_path)
        .canonicalize()
        .with_context(|| format!("путь к проекту не найден: {}", cli.project_path))?;

    println!("[kaic] проект: {}", project_root.display());

    let config = Config::load(&project_root)?;
    let project_info = context::detect(&project_root)?;

    match cli.command {
        CliCommand::Repl => {
            let engine = KaicEngine::new(&config.models.repl, config.n_gpu_layers)
                .context("не удалось инициализировать REPL-движок")?;
            repl::run(&engine, &config, &project_info)
        }
        CliCommand::Plan { task } => {
            println!("{}\n", project_info.summary());
            let engine = KaicEngine::new(&config.models.planner, config.n_gpu_layers)
                .context("не удалось инициализировать Planner-движок")?;
            let params = ChatParams {
                temperature: config.temperature,
                max_tokens: config.max_tokens,
                context: config.context,
                grammar: None,
            };
            let report = agent::run_pass(&engine, &task, &params, &config.models.planner)
                .context("Planner не смог завершить проход")?;
            print!("{}", render_pass_report(&report));
            Ok(())
        }
    }
}

fn print_help() {
    println!(
        "kaic — локальный AI-ассистент в терминале для работы с проектом.\n\n\
         Использование:\n  \
         kaic .                  открыть текущую директорию как проект\n  \
         kaic /путь/к/проекту    открыть проект по указанному пути\n  \
         kaic --plan \"задача\" .  построить безопасный план без изменения файлов\n  \
         kaic --help             показать эту справку\n  \
         kaic --version          показать версию\n\n\
         В корне открываемого проекта должен лежать файл kaic.toml\n\
         (см. kaic.toml.example)."
    );
}

#[cfg(test)]
mod cli_tests {
    use super::{parse_cli, render_pass_report, CliCommand};
    use crate::contract::{AgentIdentity, GateResult, PassReport, Plan};

    #[test]
    fn plan_flag_consumes_task_instead_of_treating_it_as_project_path() {
        let cli =
            parse_cli(&["--plan", "inspect README", "/tmp/project"]).expect("plan command parses");

        assert_eq!(cli.project_path, "/tmp/project");
        assert_eq!(
            cli.command,
            CliCommand::Plan {
                task: "inspect README".to_string()
            }
        );
    }

    #[test]
    fn repl_without_project_defaults_to_current_directory() {
        let cli = parse_cli(&[]).expect("default command parses");

        assert_eq!(cli.project_path, ".");
        assert_eq!(cli.command, CliCommand::Repl);
    }

    #[test]
    fn plan_without_task_is_rejected() {
        let error = parse_cli(&["--plan"]).expect_err("missing task must fail");

        assert!(error.to_string().contains("--plan требует задачу"));
    }

    #[test]
    fn ready_report_renders_numbered_plan_without_claiming_execution() {
        let report = PassReport {
            agent_identity: AgentIdentity {
                id: "planner_v1".to_string(),
                version: 1,
                model: "planner.gguf".to_string(),
            },
            plan: Some(Plan {
                steps: vec!["Inspect README".to_string(), "Run tests".to_string()],
            }),
            gate_result: GateResult::Ready,
        };

        let rendered = render_pass_report(&report);

        assert!(rendered.contains("План готов"));
        assert!(rendered.contains("1. Inspect README"));
        assert!(rendered.contains("2. Run tests"));
        assert!(rendered.contains("Coder ещё не запускался"));
    }

    #[test]
    fn clarification_report_renders_questions() {
        let report = PassReport {
            agent_identity: AgentIdentity {
                id: "planner_v1".to_string(),
                version: 1,
                model: "planner.gguf".to_string(),
            },
            plan: None,
            gate_result: GateResult::NeedClarification {
                clarification_questions: vec!["Какой файл?".to_string()],
            },
        };

        let rendered = render_pass_report(&report);

        assert!(rendered.contains("Нужно уточнение"));
        assert!(rendered.contains("- Какой файл?"));
    }
}
