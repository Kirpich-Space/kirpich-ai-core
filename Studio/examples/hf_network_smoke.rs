use anyhow::{Context, Result};
use kaic_studio::desktop::hf::{Manager, validate_payload};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
fn body(v: Value) -> Vec<u8> {
    serde_json::to_vec(&v).unwrap()
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("absolute isolated verification directory required")?,
    );
    anyhow::ensure!(root.is_absolute() && root.to_string_lossy().contains("verification"));
    std::fs::create_dir_all(&root)?;
    let m = Manager::open(root.join("state"));
    let repo = "ggml-org/models";
    let file = "tinyllamas/stories260K.gguf";
    let found = m
        .api("/api/hf/search", &body(json!({"query":"ggml-org/models"})))
        .await?;
    let listed = m.api("/api/hf/files", &body(json!({"repo":repo}))).await?;
    let spec = listed["files"]
        .as_array()
        .context("files")?
        .iter()
        .find(|v| v["filename"] == file)
        .context("fixture")?
        .clone();
    anyhow::ensure!(spec["size"] == 1185376);
    anyhow::ensure!(
        spec["sha256"] == "270cba1bd5109f42d03350f60406024560464db173c0e387d91f0426d3bd256d"
    );
    let start = m
        .api(
            "/api/hf/start",
            &body(json!({"repo":repo,"filename":file,"directory":root.join("models")})),
        )
        .await?;
    let id = start["id"].as_str().context("id")?;
    let cancelled = m.api("/api/hf/cancel", &body(json!({"id":id}))).await?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while m.snapshot()["active"] == true {
        anyhow::ensure!(Instant::now() < deadline, "cancel timeout");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let paused = m.snapshot();
    anyhow::ensure!(paused["downloads"][0]["state"] == "paused");
    // Reopen the persisted manager: resume must use stored revision, not mutable main.
    let restored = Manager::open(root.join("state"));
    restored
        .api("/api/hf/resume", &body(json!({"id":id})))
        .await?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while restored.snapshot()["active"] == true {
        anyhow::ensure!(Instant::now() < deadline, "download timeout");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let state = restored.snapshot();
    let j = &state["downloads"][0];
    anyhow::ensure!(j["state"] == "ready", "{}", state);
    let path = PathBuf::from(j["path"].as_str().context("path")?);
    let hash = validate_payload(
        &path,
        1185376,
        Some("270cba1bd5109f42d03350f60406024560464db173c0e387d91f0426d3bd256d"),
    )?;
    let models = kaic_studio::desktop::hf::registered(&root.join("state"));
    anyhow::ensure!(models.len() == 1 && models[0].loadable());
    // Force a genuine nonzero HTTP Range using bytes from the just-verified real HF fixture.
    let second = restored
        .api(
            "/api/hf/start",
            &body(json!({"repo":repo,"filename":file,"directory":root.join("range-models")})),
        )
        .await?;
    let second_id = second["id"].as_str().context("range id")?;
    restored
        .api("/api/hf/cancel", &body(json!({"id":second_id})))
        .await?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while restored.snapshot()["active"] == true {
        anyhow::ensure!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let part = root
        .join("range-models")
        .join(second_id)
        .join("stories260K.gguf.part");
    std::fs::create_dir_all(part.parent().unwrap())?;
    let source = std::fs::read(&path)?;
    std::fs::write(&part, &source[..65536])?;
    restored
        .api("/api/hf/resume", &body(json!({"id":second_id})))
        .await?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while restored.snapshot()["active"] == true {
        anyhow::ensure!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ranged = restored.snapshot();
    anyhow::ensure!(ranged["downloads"][1]["state"] == "ready", "{}", ranged);
    anyhow::ensure!(
        ranged["downloads"][1]["resumed_from"] == 65536
            && ranged["downloads"][1]["response_status"] == 206,
        "Expected real HTTP 206 resume: {}",
        ranged
    );
    let model_sel = models[0].selection_id();
    let state_file = root.join("state/state.json");
    let mut saved = kaic_studio::storage::SavedState::default();
    saved.settings.selected_repo = model_sel.clone();
    kaic_studio::storage::save_json(&state_file, &saved)?;
    let reloaded: kaic_studio::storage::SavedState = kaic_studio::storage::load_json(&state_file)?;
    anyhow::ensure!(reloaded.settings.selected_repo == model_sel);
    let report = json!({"kind":"real HF network fixture, NOT performance model","search":found,"file":spec,"cancel":cancelled,"paused":paused,"ready":state,"sha256":hash,"catalog_loadable":models[0].loadable(),"catalog_selection_id":model_sel,"range_resume_seed_bytes_from_verified_payload":65536,"range_resume":ranged,"native_selected_exact_download":reloaded.settings.selected_repo});
    std::fs::write(
        root.join("network-result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
