//! Правая панель инспектора в стиле LM Studio (100% нативный Rust / egui).
use super::{Studio, anim, icons, palette};
use crate::{catalog, desktop::skills, service::Job};
use eframe::egui::{self, Color32, RichText, Stroke};

fn skill_start(id: &str) -> String {
    format!("<!-- skill:{id} -->")
}

fn skill_end(id: &str) -> String {
    format!("<!-- /skill:{id} -->")
}

fn has_skill(text: &str, id: &str) -> bool {
    text.contains(&skill_start(id))
}

fn remove_skill_block(text: &str, id: &str) -> String {
    let start_marker = skill_start(id);
    let Some(start) = text.find(&start_marker) else {
        return text.to_string();
    };
    let end_marker = skill_end(id);
    let cut_end = text[start..]
        .find(&end_marker)
        .map(|rel| start + rel + end_marker.len())
        .unwrap_or(text.len());
    let before = text[..start].trim_end();
    let after = text[cut_end..].trim_start();
    match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (false, true) => before.to_string(),
        (true, false) => after.to_string(),
        (false, false) => format!("{before}\n\n{after}"),
    }
}

fn append_skill_block(text: &str, id: &str, content: &str) -> String {
    let block = format!("{}\n{}\n{}", skill_start(id), content.trim(), skill_end(id));
    if text.trim().is_empty() {
        block
    } else {
        format!("{}\n\n{block}", text.trim_end())
    }
}

fn chip(ui: &mut egui::Ui, label: &str, fg: Color32, bg: Color32) {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(11.0).color(fg));
        });
}

fn kv_row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(key).size(12.0).color(palette::INK_MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(value)
                    .monospace()
                    .size(11.5)
                    .color(palette::INK),
            );
        });
    });
}

impl Studio {
    pub(super) fn right_panel_ui(&mut self, ui: &mut egui::Ui) {
        let pal = palette::p();
        let lang = self.lang();
        const INNER_W: f32 = 280.0;
        ui.set_width(INNER_W);
        ui.set_max_width(INNER_W);

        // Шапка правой панели: заголовок + sentence-case контекст + кейкап Ctrl+.
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(lang.t("Inspector", "Инспектор"))
                    .strong()
                    .size(13.5)
                    .color(pal.ink),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                anim::keycap(ui, "Ctrl+.");
                ui.add_space(4.0);
                let tag = if self.state.chats[self.active].messages.is_empty() {
                    lang.t("New chat", "Новый диалог")
                } else {
                    lang.t("This chat", "Этот диалог")
                };
                ui.label(RichText::new(tag).size(11.0).color(pal.ink_muted));
            });
        });
        ui.add_space(6.0);
        ui.separator();

        let mut changed = false;
        let mut do_unload = false;
        let mut skill_err: Option<String> = None;

        egui::ScrollArea::vertical()
            .id_salt("правая-панель-скролл")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Убираем вертикальные линии-направляющие, сохраняя чистый зазор между треугольником и заголовком
                ui.visuals_mut().indent_has_left_vline = false;
                ui.spacing_mut().indent = 16.0;
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                let content_w = (INNER_W - 22.0).max(220.0);

                // ── 1. Модель ──────────────────────────────────────────────
                let selected_repo = self.state.settings.selected_repo.clone();
                let model = catalog::resolve_selection(&self.models, &selected_repo).cloned();
                let model_kind = model
                    .as_ref()
                    .map(|m| m.kind())
                    .unwrap_or(catalog::ModelKind::Text);
                let is_loaded = self.loaded.as_deref().is_some_and(|l| {
                    model
                        .as_ref()
                        .is_some_and(|m| l == m.alias() || l == m.repo || l == m.selection_id())
                });
                let profile = model
                    .as_ref()
                    .and_then(|m| catalog::live_profile(&m.repo).ok());

                egui::CollapsingHeader::new(
                    RichText::new(lang.t("Model", "Модель")).strong().size(13.0),
                )
                .id_salt("секция-модель")
                .default_open(true)
                .show(ui, |ui| {
                    egui::Frame::new()
                        .fill(pal.surface_0)
                        .stroke(Stroke::new(1.0, pal.line))
                        .corner_radius(8)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.set_width((content_w - 22.0).max(180.0));
                            let title = model
                                .as_ref()
                                .map(|m| m.display_name())
                                .unwrap_or_else(|| {
                                    if selected_repo.is_empty() {
                                        lang.t("No model selected", "Модель не выбрана").into()
                                    } else {
                                        selected_repo.clone()
                                    }
                                });
                            ui.add(
                                egui::Label::new(
                                    RichText::new(title).strong().size(13.5).color(pal.ink),
                                )
                                .truncate(),
                            );
                            if let Some(m) = &model {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&m.filename)
                                            .monospace()
                                            .size(11.0)
                                            .color(pal.ink_muted),
                                    )
                                    .truncate(),
                                );
                            }
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if model_kind != catalog::ModelKind::Text {
                                    let kind_lbl = match model_kind {
                                        catalog::ModelKind::Video => {
                                            lang.t("Video · sd-cli", "Видео · sd-cli")
                                        }
                                        _ => lang.t("Image · sd-cli", "Фото · sd-cli"),
                                    };
                                    chip(ui, kind_lbl, pal.brand_text, pal.surface_2);
                                } else if is_loaded {
                                    chip(
                                        ui,
                                        lang.t("Loaded", "Загружена"),
                                        pal.brand_text,
                                        pal.surface_2,
                                    );
                                } else {
                                    chip(
                                        ui,
                                        lang.t("Not loaded", "Не загружена"),
                                        pal.ink_muted,
                                        pal.surface_2,
                                    );
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let locality = match &model {
                                            Some(m) => {
                                                let gb = m.size as f64 / 1_073_741_824.0;
                                                if gb <= 6.2 {
                                                    lang.t("Local · GPU", "Локально · GPU")
                                                } else if gb <= 10.5 {
                                                    lang.t("Local · GPU+CPU", "Локально · GPU+CPU")
                                                } else {
                                                    lang.t(
                                                        "Local · CPU offload",
                                                        "Локально · CPU offload",
                                                    )
                                                }
                                            }
                                            None => lang.t("Local", "Локально"),
                                        };
                                        ui.label(
                                            RichText::new(locality)
                                                .monospace()
                                                .size(10.5)
                                                .color(pal.ink_muted),
                                        );
                                    },
                                );
                            });
                            ui.add_space(6.0);
                            kv_row(
                                ui,
                                lang.t("Size", "Размер"),
                                &model
                                    .as_ref()
                                    .map(|m| lang.format_gb(m.size))
                                    .unwrap_or_else(|| "—".into()),
                            );
                            kv_row(
                                ui,
                                lang.t("Speed", "Скорость"),
                                &model
                                    .as_ref()
                                    .and_then(|m| m.generation_tps)
                                    .map(|t| format!("{t:.1} {}", lang.t("tok/s", "t/c")))
                                    .unwrap_or_else(|| "—".into()),
                            );
                            kv_row(
                                ui,
                                lang.t("Profile context", "Контекст профиля"),
                                &profile
                                    .as_ref()
                                    .map(|p| {
                                        format!("{}K {}", p.context / 1024, lang.t("tok", "ток"))
                                    })
                                    .unwrap_or_else(|| "—".into()),
                            );
                            kv_row(
                                ui,
                                lang.t("CPU threads", "Потоки CPU"),
                                &profile
                                    .as_ref()
                                    .map(|p| p.threads.to_string())
                                    .unwrap_or_else(|| "—".into()),
                            );
                            kv_row(
                                ui,
                                lang.t("KV cache", "KV-кэш"),
                                profile.as_ref().map(|p| p.cache.as_str()).unwrap_or("—"),
                            );
                            if is_loaded {
                                ui.add_space(6.0);
                                if ui
                                    .add_enabled(
                                        !self.busy,
                                        egui::Button::new(lang.t(
                                            "Unload from memory",
                                            "Выгрузить из памяти",
                                        ))
                                        .min_size(egui::vec2(ui.available_width(), 28.0)),
                                    )
                                    .clicked()
                                {
                                    do_unload = true;
                                }
                            }
                        });
                });

                ui.add_space(2.0);
                ui.separator();
                ui.add_space(2.0);

                // ── 1.5. Динамичный агент и провайдеры ──────────────────────
                let agent_on = self.state.chats[self.active].options.agent_enabled;
                let agent_heading = if agent_on {
                    format!(
                        "{} · {}",
                        lang.t("Agent", "Агент"),
                        lang.t("ON", "ВКЛ")
                    )
                } else {
                    lang.t("Agent", "Агент").to_string()
                };
                egui::CollapsingHeader::new(RichText::new(agent_heading).strong().size(13.0))
                    .id_salt("секция-агент")
                    .default_open(true)
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .fill(pal.surface_0)
                            .stroke(Stroke::new(1.0, pal.line))
                            .corner_radius(8)
                            .inner_margin(10)
                            .show(ui, |ui| {
                                ui.set_width((content_w - 22.0).max(180.0));
                                let opts = &mut self.state.chats[self.active].options;
                                if ui
                                    .add_enabled(
                                        !self.busy,
                                        egui::Checkbox::new(
                                            &mut opts.agent_enabled,
                                            RichText::new(lang.t(
                                                "Dynamic agent mode (tools & checks)",
                                                "Режим динамичного агента (инструменты)",
                                            ))
                                            .size(12.0),
                                        ),
                                    )
                                    .changed()
                                {
                                    changed = true;
                                }

                                if opts.agent_enabled {
                                    ui.add_space(6.0);
                                    ui.label(
                                        RichText::new(lang.t("Provider", "Провайдер"))
                                            .size(11.5)
                                            .color(pal.ink_muted),
                                    );
                                    ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                                        for kind in [
                                            crate::agent::ProviderKind::LocalLlama,
                                            crate::agent::ProviderKind::OllamaLocal,
                                            crate::agent::ProviderKind::OpenAiCompat,
                                            crate::agent::ProviderKind::AnthropicApi,
                                            crate::agent::ProviderKind::CodexCli,
                                            crate::agent::ProviderKind::ClaudeCli,
                                            crate::agent::ProviderKind::AgyCli,
                                        ] {
                                            let cap = self
                                                .provider_caps
                                                .iter()
                                                .find(|c| c.provider == kind);
                                            let avail = cap.is_some_and(|c| c.available);
                                            let sel = opts.provider == kind;
                                            let tag = match kind.execution_origin() {
                                                crate::agent::ExecutionOrigin::KaicNativeAgent => {
                                                    "native"
                                                }
                                                crate::agent::ExecutionOrigin::ExternallyManagedCli => {
                                                    "CLI"
                                                }
                                            };
                                            let lbl = format!("{} [{tag}]", kind.short_label());
                                            let btn = egui::Button::new(
                                                RichText::new(lbl).size(11.0).color(if sel {
                                                    pal.ink
                                                } else {
                                                    pal.ink_muted
                                                }),
                                            )
                                            .fill(if sel { pal.surface_2 } else { pal.surface_0 })
                                            .stroke(Stroke::new(
                                                1.0,
                                                if sel { pal.brand } else { pal.line },
                                            ))
                                            .corner_radius(5);
                                            if ui.add_enabled(!self.busy && avail, btn).clicked() {
                                                opts.provider = kind;
                                                changed = true;
                                            }
                                        }
                                    });

                                    if let Some(cap) = self
                                        .provider_caps
                                        .iter()
                                        .find(|c| c.provider == opts.provider)
                                    {
                                        ui.add_space(4.0);
                                        kv_row(
                                            ui,
                                            lang.t("Runtime", "Среда"),
                                            match cap.execution_origin {
                                                crate::agent::ExecutionOrigin::KaicNativeAgent => {
                                                    lang.t("KAIC native", "KAIC native")
                                                }
                                                crate::agent::ExecutionOrigin::ExternallyManagedCli => {
                                                    lang.t("External CLI", "Внешний CLI")
                                                }
                                            },
                                        );
                                        kv_row(
                                            ui,
                                            lang.t("Approval", "Подтверждение"),
                                            match cap.approval_granularity {
                                                crate::agent::ApprovalGranularity::PerAction => {
                                                    lang.t("Per action", "На действие")
                                                }
                                                crate::agent::ApprovalGranularity::LaunchConsentAndSandbox => {
                                                    lang.t("Launch+sandbox", "Запуск+sandbox")
                                                }
                                                crate::agent::ApprovalGranularity::Unavailable => {
                                                    lang.t("Unavailable", "Недоступно")
                                                }
                                            },
                                        );
                                        kv_row(
                                            ui,
                                            "OS",
                                            "Linux: ok · FreeBSD: unverified",
                                        );
                                    }

                                    if matches!(
                                        opts.provider,
                                        crate::agent::ProviderKind::OllamaLocal
                                            | crate::agent::ProviderKind::OpenAiCompat
                                            | crate::agent::ProviderKind::AnthropicApi
                                    ) {
                                        ui.add_space(6.0);
                                        let ep_hint = match opts.provider {
                                            crate::agent::ProviderKind::OllamaLocal => {
                                                "http://127.0.0.1:11434"
                                            }
                                            crate::agent::ProviderKind::AnthropicApi => {
                                                "https://api.anthropic.com"
                                            }
                                            _ => "http://127.0.0.1:1234",
                                        };
                                        ui.label(
                                            RichText::new(lang.t("API Endpoint", "URL endpoint API"))
                                                .size(11.0)
                                                .color(pal.ink_muted),
                                        );
                                        if ui
                                            .add_enabled(
                                                !self.busy,
                                                egui::TextEdit::singleline(&mut opts.provider_endpoint)
                                                    .hint_text(ep_hint)
                                                    .margin(egui::Margin::symmetric(8, 4))
                                                    .desired_width(f32::INFINITY)
                                                    .font(egui::TextStyle::Monospace),
                                            )
                                            .changed()
                                        {
                                            changed = true;
                                        }
                                        ui.label(
                                            RichText::new(lang.t("Provider model", "Модель провайдера"))
                                                .size(11.0)
                                                .color(pal.ink_muted),
                                        );
                                        if ui
                                            .add_enabled(
                                                !self.busy,
                                                egui::TextEdit::singleline(&mut opts.provider_model)
                                                    .hint_text(match opts.provider {
                                                        crate::agent::ProviderKind::AnthropicApi => {
                                                            "claude-sonnet-4-5"
                                                        }
                                                        crate::agent::ProviderKind::OllamaLocal => {
                                                            "qwen2.5-coder:14b"
                                                        }
                                                        _ => "model-id",
                                                    })
                                                    .margin(egui::Margin::symmetric(8, 4))
                                                    .desired_width(f32::INFINITY)
                                                    .font(egui::TextStyle::Monospace),
                                            )
                                            .changed()
                                        {
                                            changed = true;
                                        }
                                        if matches!(
                                            opts.provider,
                                            crate::agent::ProviderKind::OpenAiCompat
                                                | crate::agent::ProviderKind::AnthropicApi
                                        ) {
                                            ui.label(
                                                RichText::new(lang.t(
                                                    "API key is read only from OPENAI_API_KEY / ANTHROPIC_API_KEY env vars.",
                                                    "Ключ читается только из OPENAI_API_KEY / ANTHROPIC_API_KEY (не сохраняется на диск).",
                                                ))
                                                .size(10.0)
                                                .color(pal.ink_muted),
                                            );
                                        }
                                    }

                                    ui.add_space(6.0);
                                    ui.label(
                                        RichText::new(lang.t("Workspace directory", "Рабочий каталог (workspace)"))
                                            .size(11.5)
                                            .color(pal.ink_muted),
                                    );
                                    if ui
                                        .add_enabled(
                                            !self.busy,
                                            egui::TextEdit::singleline(&mut opts.workspace_path)
                                                .hint_text("/abs/path/to/workspace")
                                                .margin(egui::Margin::symmetric(8, 5))
                                                .desired_width(f32::INFINITY)
                                                .font(egui::TextStyle::Monospace),
                                        )
                                        .changed()
                                    {
                                        changed = true;
                                    }
                                    if opts.workspace_path.trim().is_empty() {
                                        ui.label(
                                            RichText::new(lang.t(
                                                "Select an absolute project folder to enable tools.",
                                                "Укажите абсолютный путь к каталогу задачи.",
                                            ))
                                            .size(10.5)
                                            .color(pal.ink_muted),
                                        );
                                    } else {
                                        match crate::agent::WorkspaceGuard::new(
                                            std::path::Path::new(opts.workspace_path.trim()),
                                        ) {
                                            Ok(_) => {
                                                ui.label(
                                                    RichText::new(lang.t(
                                                        "Workspace validated",
                                                        "Каталог проверен",
                                                    ))
                                                    .size(10.5)
                                                    .color(pal.brand_text),
                                                );
                                            }
                                            Err(e) => {
                                                ui.label(
                                                    RichText::new(format!("{e:#}"))
                                                        .size(10.5)
                                                        .color(pal.err),
                                                );
                                            }
                                        }
                                    }

                                    ui.add_space(6.0);
                                    ui.label(
                                        RichText::new(lang.t(
                                            "File tools policy (workspace)",
                                            "Политика файлов (workspace)",
                                        ))
                                        .size(11.5)
                                        .color(pal.ink_muted),
                                    );
                                    ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                                        for (pol, lbl) in [
                                            (crate::agent::AgentPolicy::ReadOnly, "read-only"),
                                            (
                                                crate::agent::AgentPolicy::ApprovalRequired,
                                                "approval",
                                            ),
                                            (
                                                crate::agent::AgentPolicy::WorkspaceWrite,
                                                "workspace-write",
                                            ),
                                        ] {
                                            let sel = opts.agent_policy == pol;
                                            let btn = egui::Button::new(
                                                RichText::new(lbl)
                                                    .monospace()
                                                    .size(10.5)
                                                    .color(if sel {
                                                        pal.ink
                                                    } else {
                                                        pal.ink_muted
                                                    }),
                                            )
                                            .fill(if sel { pal.surface_2 } else { pal.surface_0 })
                                            .stroke(Stroke::new(
                                                1.0,
                                                if sel { pal.brand } else { pal.line },
                                            ))
                                            .corner_radius(5);
                                            if ui.add_enabled(!self.busy, btn).clicked() {
                                                opts.agent_policy = pol;
                                                changed = true;
                                            }
                                        }
                                    });

                                    if !opts.provider.is_external() {
                                        ui.add_space(6.0);
                                        ui.label(
                                            RichText::new(lang.t(
                                                "Code execution policy (no OS sandbox)",
                                                "Исполнение кода (без OS-sandbox)",
                                            ))
                                            .size(11.5)
                                            .color(pal.ink_muted),
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                                            for (epol, lbl) in [
                                                (crate::agent::ExecPolicy::Disabled, "disabled"),
                                                (
                                                    crate::agent::ExecPolicy::SafeInspectOnly,
                                                    "safe-inspect",
                                                ),
                                                (
                                                    crate::agent::ExecPolicy::ConfirmUserCode,
                                                    "confirm-code",
                                                ),
                                                (
                                                    crate::agent::ExecPolicy::AllowWorkspaceBuilds,
                                                    "allow-builds",
                                                ),
                                            ] {
                                                let sel = opts.exec_policy == epol;
                                                let btn = egui::Button::new(
                                                    RichText::new(lbl)
                                                        .monospace()
                                                        .size(10.5)
                                                        .color(if sel {
                                                            pal.ink
                                                        } else {
                                                            pal.ink_muted
                                                        }),
                                                )
                                                .fill(if sel {
                                                    pal.surface_2
                                                } else {
                                                    pal.surface_0
                                                })
                                                .stroke(Stroke::new(
                                                    1.0,
                                                    if sel { pal.brand } else { pal.line },
                                                ))
                                                .corner_radius(5);
                                                if ui.add_enabled(!self.busy, btn).clicked() {
                                                    opts.exec_policy = epol;
                                                    changed = true;
                                                }
                                            }
                                        });
                                        ui.label(
                                            RichText::new(lang.t(
                                                "Builds/tests (cargo, make, pytest, npm) run with current user privileges.",
                                                "Сборки/тесты (cargo, make, pytest, npm) идут с правами текущего пользователя.",
                                            ))
                                            .size(10.0)
                                            .color(pal.ink_muted),
                                        );
                                    }

                                    if opts.provider.is_external() {
                                        ui.add_space(6.0);
                                        if ui
                                            .add_enabled(
                                                !self.busy,
                                                egui::Checkbox::new(
                                                    &mut opts.external_provider_consent,
                                                    RichText::new(lang.t(
                                                        "Allow sending workspace context externally",
                                                        "Разрешить внешнюю передачу контекста",
                                                    ))
                                                    .size(11.0),
                                                ),
                                            )
                                            .changed()
                                        {
                                            changed = true;
                                        }
                                    }

                                    ui.add_space(4.0);
                                    if ui
                                        .add_enabled(
                                            !self.busy,
                                            egui::Checkbox::new(
                                                &mut opts.dynamic_interaction_enabled,
                                                RichText::new(lang.t(
                                                    "Enable dynamic multi-seat interaction (OFF by default)",
                                                    "Динамическое взаимодействие с местами (выкл. по умолчанию)",
                                                ))
                                                .size(10.5),
                                            ),
                                        )
                                        .changed()
                                    {
                                        changed = true;
                                    }

                                    ui.add_space(6.0);
                                    egui::CollapsingHeader::new(
                                        RichText::new(lang.t(
                                            "Provider matrix",
                                            "Матрица провайдеров",
                                        ))
                                        .size(11.5)
                                        .color(pal.ink_muted),
                                    )
                                    .id_salt("секция-матрица-провайдеров")
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for c in &self.provider_caps {
                                            let dot = if c.status
                                                == crate::agent::FeatureStatus::Unavailable
                                            {
                                                pal.line_strong
                                            } else if c.provider.is_external() {
                                                pal.cloud
                                            } else {
                                                pal.local
                                            };
                                            ui.horizontal(|ui| {
                                                icons::dot(ui, dot);
                                                ui.label(
                                                    RichText::new(&c.label)
                                                        .size(11.0)
                                                        .color(pal.ink),
                                                );
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        ui.label(
                                                            RichText::new(c.status.label_ru())
                                                                .monospace()
                                                                .size(10.0)
                                                                .color(dot),
                                                        );
                                                    },
                                                );
                                            });
                                        }
                                        if ui
                                            .add_enabled(
                                                !self.busy,
                                                egui::Button::new(
                                                    RichText::new(lang.t(
                                                        "Re-probe host",
                                                        "Перепроверить хост",
                                                    ))
                                                    .size(10.5),
                                                )
                                                .corner_radius(5),
                                            )
                                            .clicked()
                                        {
                                            self.provider_caps = crate::agent::probe_providers();
                                        }
                                    });
                                }
                            });
                    });

                ui.add_space(2.0);
                ui.separator();
                ui.add_space(2.0);

                // ── 2. SKILLS (из памяти ПК) ───────────────────────────────
                let attached = self
                    .disk_skills
                    .iter()
                    .filter(|s| has_skill(&self.state.chats[self.active].options.skills, &s.id))
                    .count();
                let skills_heading = if attached > 0 {
                    format!(
                        "{} · {attached} {}",
                        lang.t("Skills", "Скиллы"),
                        lang.t("active", "подкл.")
                    )
                } else {
                    format!("{} · {}", lang.t("Skills", "Скиллы"), self.disk_skills.len())
                };
                egui::CollapsingHeader::new(RichText::new(skills_heading).strong().size(13.0))
                    .id_salt("секция-скиллы")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.skill_search)
                                    .hint_text(lang.t("Search skills", "Поиск скиллов"))
                                    .margin(egui::Margin::symmetric(8, 5))
                                    .desired_width((content_w - 40.0).max(140.0)),
                            );
                            if ui
                                .add_sized([28.0, 28.0], egui::Button::new("↻"))
                                .on_hover_text(lang.t(
                                    "Refresh skills from disk",
                                    "Обновить список скиллов с диска",
                                ))
                                .clicked()
                            {
                                self.disk_skills = skills::discover();
                            }
                        });
                        let q = self.skill_search.trim().to_lowercase();
                        let mut toggled: Option<String> = None;
                        egui::Frame::new()
                            .fill(pal.surface_0)
                            .stroke(Stroke::new(1.0, pal.line))
                            .corner_radius(6)
                            .inner_margin(4)
                            .show(ui, |ui| {
                                ui.set_width((content_w - 10.0).max(180.0));
                                egui::ScrollArea::vertical()
                                    .id_salt("список-скиллов-диск")
                                    .max_height(146.0)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        let mut shown = 0usize;
                                        for s in &self.disk_skills {
                                            if !q.is_empty()
                                                && !s.name.to_lowercase().contains(&q)
                                                && !s.id.to_lowercase().contains(&q)
                                                && !s.description.to_lowercase().contains(&q)
                                            {
                                                continue;
                                            }
                                            shown += 1;
                                            let active = has_skill(
                                                &self.state.chats[self.active].options.skills,
                                                &s.id,
                                            );
                                            let row_fill = if active {
                                                pal.surface_2
                                            } else {
                                                Color32::TRANSPARENT
                                            };
                                            let resp = egui::Frame::new()
                                                .fill(row_fill)
                                                .corner_radius(5)
                                                .inner_margin(egui::Margin::symmetric(6, 4))
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        let badge = if active { "*" } else { "+" };
                                                        let badge_color = if active {
                                                            pal.brand_text
                                                        } else {
                                                            pal.ink_muted
                                                        };
                                                        ui.label(
                                                            RichText::new(badge)
                                                                .monospace()
                                                                .strong()
                                                                .color(badge_color),
                                                        );
                                                        ui.vertical(|ui| {
                                                            ui.set_max_width(
                                                                (content_w - 92.0).max(120.0),
                                                            );
                                                            ui.spacing_mut().item_spacing.y = 1.0;
                                                            ui.add(
                                                                egui::Label::new(
                                                                    RichText::new(&s.name)
                                                                        .size(12.0)
                                                                        .color(pal.ink),
                                                                )
                                                                .truncate(),
                                                            );
                                                            if !s.description.is_empty() {
                                                                ui.add(
                                                                    egui::Label::new(
                                                                        RichText::new(&s.description)
                                                                            .size(10.5)
                                                                            .color(pal.ink_muted),
                                                                    )
                                                                    .truncate(),
                                                                );
                                                            }
                                                        });
                                                        ui.with_layout(
                                                            egui::Layout::right_to_left(
                                                                egui::Align::Center,
                                                            ),
                                                            |ui| {
                                                                ui.label(
                                                                    RichText::new(lang.format_kb(
                                                                        s.bytes as usize,
                                                                    ))
                                                                    .monospace()
                                                                    .size(10.5)
                                                                    .color(pal.ink_muted),
                                                                );
                                                            },
                                                        );
                                                    });
                                                })
                                                .response
                                                .interact(egui::Sense::click())
                                                .on_hover_text(&s.description);
                                            if resp.hovered() && !active {
                                                ui.painter().rect_filled(
                                                    resp.rect,
                                                    5,
                                                    anim::with_alpha(pal.ink, 12),
                                                );
                                            }
                                            if resp.clicked() {
                                                toggled = Some(s.id.clone());
                                            }
                                        }
                                        if shown == 0 {
                                            ui.label(
                                                RichText::new(lang.t(
                                                    "No skills found",
                                                    "Скиллы не найдены",
                                                ))
                                                .size(12.0)
                                                .color(pal.ink_muted),
                                            );
                                        }
                                    });
                            });

                        if let Some(id) = toggled {
                            let cur = self.state.chats[self.active].options.skills.clone();
                            if has_skill(&cur, &id) {
                                self.state.chats[self.active].options.skills =
                                    remove_skill_block(&cur, &id);
                                changed = true;
                            } else {
                                match skills::read_json(&id) {
                                    Ok(v) => {
                                        let content = v["content"].as_str().unwrap_or("");
                                        let next = append_skill_block(&cur, &id, content);
                                        self.state.chats[self.active].options.skills = next;
                                        changed = true;
                                    }
                                    Err(e) => skill_err = Some(format!("{e:#}")),
                                }
                            }
                        }

                        let used = self.state.chats[self.active].options.skills.len();
                        let manual_id = egui::Id::new("kaic-skills-manual");
                        let mut manual_open = ui
                            .ctx()
                            .data_mut(|d| d.get_temp::<bool>(manual_id))
                            .unwrap_or(false);
                        let show_editor = manual_open || used > 0;
                        if show_editor
                            && ui
                                .add(
                                    egui::TextEdit::multiline(
                                        &mut self.state.chats[self.active].options.skills,
                                    )
                                    .hint_text(lang.t(
                                        "Attached skills prompt text",
                                        "Текст скиллов для промпта",
                                    ))
                                    .margin(egui::Margin::symmetric(8, 6))
                                    .desired_rows(3)
                                    .desired_width(content_w)
                                    .font(egui::TextStyle::Monospace),
                                )
                                .changed()
                        {
                            changed = true;
                        }
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(lang.format_kb(used))
                                    .monospace()
                                    .size(11.0)
                                    .color(pal.ink_muted),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if used > 0 {
                                        if ui
                                            .small_button(lang.t("Clear", "Очистить"))
                                            .clicked()
                                        {
                                            self.state.chats[self.active].options.skills.clear();
                                            manual_open = false;
                                            changed = true;
                                        }
                                    } else {
                                        let lbl = if manual_open {
                                            lang.t("Hide", "Скрыть")
                                        } else {
                                            lang.t("Manual", "Вручную")
                                        };
                                        if ui.small_button(lbl).clicked() {
                                            manual_open = !manual_open;
                                        }
                                    }
                                },
                            );
                        });
                        ui.ctx().data_mut(|d| d.insert_temp(manual_id, manual_open));
                    });

                ui.add_space(2.0);
                ui.separator();
                ui.add_space(2.0);

                // ── 3. Системный промпт ────────────────────────────────────
                egui::CollapsingHeader::new(
                    RichText::new(lang.t("System Prompt", "Системный промпт"))
                        .strong()
                        .size(13.0),
                )
                .id_salt("секция-системный-промпт")
                .default_open(!self.state.chats[self.active].options.system_prompt.is_empty())
                .show(ui, |ui| {
                    if ui
                        .add(
                            egui::TextEdit::multiline(
                                &mut self.state.chats[self.active].options.system_prompt,
                            )
                            .hint_text(lang.t(
                                "e.g. You are an engineering assistant. Be concise.",
                                "Например: Ты — инженерный ассистент. Отвечай кратко.",
                            ))
                            .margin(egui::Margin::symmetric(8, 6))
                            .desired_rows(4)
                            .desired_width(content_w),
                        )
                        .changed()
                    {
                        changed = true;
                    }
                });

                ui.add_space(2.0);
                ui.separator();
                ui.add_space(2.0);

                // ── 4. Параметры генерации (адаптивно: LLM vs Photo/Video) ─
                if model_kind != catalog::ModelKind::Text {
                    let title = match model_kind {
                        catalog::ModelKind::Video => {
                            lang.t("Video Generation", "Генерация видео")
                        }
                        _ => lang.t("Image Generation", "Генерация фото"),
                    };
                    egui::CollapsingHeader::new(RichText::new(title).strong().size(13.0))
                        .id_salt("секция-генерация-медиа")
                        .default_open(true)
                        .show(ui, |ui| {
                            let mg = &mut self.state.chats[self.active].options.media_gen;
                            ui.label(
                                RichText::new(lang.t("Resolution", "Разрешение"))
                                    .size(12.0)
                                    .color(pal.ink_muted),
                            );
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                                let presets: &[(u32, u32)] = if model_kind == catalog::ModelKind::Video
                                {
                                    &[(512, 512), (832, 480), (480, 832), (640, 384)]
                                } else {
                                    &[(512, 512), (768, 768), (1024, 1024), (832, 480)]
                                };
                                for &(w, h) in presets {
                                    let active = mg.width == w && mg.height == h;
                                    let btn = egui::Button::new(
                                        RichText::new(format!("{w}×{h}"))
                                            .monospace()
                                            .size(11.0)
                                            .color(if active { pal.ink } else { pal.ink_muted }),
                                    )
                                    .fill(if active { pal.surface_2 } else { pal.surface_0 })
                                    .stroke(Stroke::new(
                                        1.0,
                                        if active { pal.brand } else { pal.line },
                                    ))
                                    .corner_radius(5);
                                    if ui.add(btn).clicked() {
                                        mg.width = w;
                                        mg.height = h;
                                        changed = true;
                                    }
                                }
                            });
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("W").size(11.5).color(pal.ink_muted));
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut mg.width)
                                            .range(64..=2048)
                                            .speed(8.0),
                                    )
                                    .changed()
                                {
                                    changed = true;
                                }
                                ui.label(RichText::new("× H").size(11.5).color(pal.ink_muted));
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut mg.height)
                                            .range(64..=2048)
                                            .speed(8.0),
                                    )
                                    .changed()
                                {
                                    changed = true;
                                }
                            });
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(lang.t("Steps", "Шаги")).size(12.5));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            RichText::new(mg.steps.to_string())
                                                .monospace()
                                                .size(12.0)
                                                .color(pal.ink_muted),
                                        );
                                    },
                                );
                            });
                            if ui
                                .add(egui::Slider::new(&mut mg.steps, 1..=100).show_value(false))
                                .changed()
                            {
                                changed = true;
                            }
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("CFG scale").size(12.5));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            RichText::new(format!("{:.1}", mg.cfg_scale))
                                                .monospace()
                                                .size(12.0)
                                                .color(pal.ink_muted),
                                        );
                                    },
                                );
                            });
                            if ui
                                .add(
                                    egui::Slider::new(&mut mg.cfg_scale, 0.5..=20.0)
                                        .step_by(0.5)
                                        .show_value(false),
                                )
                                .changed()
                            {
                                changed = true;
                            }
                            if model_kind == catalog::ModelKind::Video {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(lang.t("Frames", "Кадры")).size(12.5));
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(format!(
                                                    "{}f @ {}fps",
                                                    mg.video_frames, mg.fps
                                                ))
                                                .monospace()
                                                .size(12.0)
                                                .color(pal.ink_muted),
                                            );
                                        },
                                    );
                                });
                                if ui
                                    .add(
                                        egui::Slider::new(&mut mg.video_frames, 5..=129)
                                            .step_by(4.0)
                                            .show_value(false),
                                    )
                                    .changed()
                                {
                                    changed = true;
                                }
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("FPS").size(12.0).color(pal.ink_muted));
                                    for fps in [8u32, 16, 24] {
                                        let sel = mg.fps == fps;
                                        let btn = egui::Button::new(
                                            RichText::new(fps.to_string()).monospace().size(11.0),
                                        )
                                        .fill(if sel { pal.surface_2 } else { pal.surface_0 })
                                        .stroke(Stroke::new(
                                            1.0,
                                            if sel { pal.brand } else { pal.line },
                                        ));
                                        if ui.add(btn).clicked() {
                                            mg.fps = fps;
                                            changed = true;
                                        }
                                    }
                                });
                            }
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Seed (-1 = auto)").size(12.0));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add(
                                                egui::DragValue::new(&mut mg.seed)
                                                    .range(-1..=i64::MAX),
                                            )
                                            .changed()
                                        {
                                            changed = true;
                                        }
                                    },
                                );
                            });
                            if ui
                                .checkbox(
                                    &mut mg.offload_to_cpu,
                                    RichText::new(lang.t(
                                        "CPU offload (8 GB VRAM)",
                                        "Выгрузка на CPU (8 ГБ VRAM)",
                                    ))
                                    .size(12.0),
                                )
                                .changed()
                            {
                                changed = true;
                            }
                            if ui
                                .checkbox(
                                    &mut mg.vae_tiling,
                                    RichText::new("VAE tiling").size(12.0),
                                )
                                .changed()
                            {
                                changed = true;
                            }
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(lang.t("Negative prompt", "Негативный промпт"))
                                    .size(11.5)
                                    .color(pal.ink_muted),
                            );
                            if ui
                                .add(
                                    egui::TextEdit::multiline(&mut mg.negative_prompt)
                                        .hint_text("blurry, low quality, artifacts")
                                        .margin(egui::Margin::symmetric(8, 5))
                                        .desired_rows(2)
                                        .desired_width(content_w),
                                )
                                .changed()
                            {
                                changed = true;
                            }
                            egui::CollapsingHeader::new(
                                RichText::new(lang.t(
                                    "VAE / Text Encoder (optional)",
                                    "VAE / Текстовый энкодер (опц.)",
                                ))
                                .size(11.5)
                                .color(pal.ink_muted),
                            )
                            .id_salt("секция-медиа-допы")
                            .default_open(false)
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(lang.t(
                                        "Auto-detected from model folder if blank.",
                                        "Подхватываются автоматически из папки модели, если пусто.",
                                    ))
                                    .size(10.5)
                                    .color(pal.ink_muted),
                                );
                                if ui
                                    .add(
                                        egui::TextEdit::singleline(&mut mg.vae_path)
                                            .hint_text("VAE path (.safetensors)")
                                            .margin(egui::Margin::symmetric(8, 5))
                                            .desired_width(content_w)
                                            .font(egui::TextStyle::Monospace),
                                    )
                                    .changed()
                                {
                                    changed = true;
                                }
                                if ui
                                    .add(
                                        egui::TextEdit::singleline(&mut mg.text_encoder_path)
                                            .hint_text("T5 / CLIP / LLM path")
                                            .margin(egui::Margin::symmetric(8, 5))
                                            .desired_width(content_w)
                                            .font(egui::TextStyle::Monospace),
                                    )
                                    .changed()
                                {
                                    changed = true;
                                }
                            });
                        });
                } else {
                egui::CollapsingHeader::new(
                    RichText::new(lang.t("Parameters", "Параметры"))
                        .strong()
                        .size(13.0),
                )
                .id_salt("секция-параметры")
                .default_open(true)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(lang.t("Chat context", "Контекст диалога")).size(12.5),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let label = if self.state.chats[self.active].options.context == 0 {
                                lang.t("profile", "профиль").into()
                            } else {
                                format!(
                                    "{}K",
                                    self.state.chats[self.active].options.context / 1024
                                )
                            };
                            ui.label(
                                RichText::new(label)
                                    .monospace()
                                    .size(12.0)
                                    .color(pal.ink_muted),
                            );
                        });
                    });
                    if ui
                        .add(
                            egui::Slider::new(
                                &mut self.state.chats[self.active].options.context,
                                0..=262_144,
                            )
                            .step_by(2048.0)
                            .show_value(false),
                        )
                        .changed()
                    {
                        changed = true;
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        for (val, lbl) in [
                            (0, lang.t("Profile", "Профиль")),
                            (4096, "4K"),
                            (8192, "8K"),
                            (16384, "16K"),
                            (32768, "32K"),
                            (65536, "64K"),
                            (131072, "128K"),
                            (262144, "256K"),
                        ] {
                            let sel = self.state.chats[self.active].options.context == val;
                            let btn = egui::Button::new(
                                RichText::new(lbl)
                                    .monospace()
                                    .size(11.0)
                                    .color(if sel { pal.ink } else { pal.ink_muted }),
                            )
                            .fill(if sel { pal.surface_2 } else { pal.surface_0 })
                            .stroke(Stroke::new(
                                1.0,
                                if sel { pal.brand } else { pal.line },
                            ))
                            .corner_radius(5);
                            if ui.add(btn).clicked() {
                                self.state.chats[self.active].options.context = val;
                                self.state.settings.context_override = 0;
                                changed = true;
                            }
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(lang.t("Max output", "Лимит ответа")).size(12.5));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    self.state.settings.max_tokens,
                                    lang.t("tok", "ток")
                                ))
                                .monospace()
                                .size(12.0)
                                .color(pal.ink_muted),
                            );
                        });
                    });
                    if ui
                        .add(
                            egui::Slider::new(
                                &mut self.state.settings.max_tokens,
                                64..=65_536,
                            )
                            .logarithmic(true)
                            .show_value(false),
                        )
                        .changed()
                    {
                        changed = true;
                    }
                    ui.add_space(2.0);
                    kv_row(ui, "Temperature", "0.6");
                });
                }

                ui.add_space(2.0);
                ui.separator();
                ui.add_space(2.0);

                // ── 5. Сервер llama.cpp ────────────────────────────────────
                egui::CollapsingHeader::new(
                    RichText::new(lang.t("llama.cpp Server", "Сервер llama.cpp"))
                        .strong()
                        .size(13.0),
                )
                .id_salt("секция-сервер")
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let dot_color = if self.busy {
                            pal.brand
                        } else if self.error.is_some() {
                            pal.err
                        } else {
                            pal.ceramic_200
                        };
                        icons::dot(ui, dot_color);
                        ui.label(RichText::new(&self.status).size(12.0));
                    });
                    ui.label(
                        RichText::new(lang.t(
                            "Loopback 127.0.0.1 only. Child process exits with KAIC Studio.",
                            "Только 127.0.0.1. Дочерний процесс завершается вместе с KAIC Studio.",
                        ))
                        .size(11.0)
                        .color(pal.ink_muted),
                    );
                });
            });

        if let Some(e) = skill_err {
            self.error = Some(e);
        }
        if do_unload {
            self.start(Job::Unload);
        }
        if changed {
            self.mark_dirty();
        }
    }
}
