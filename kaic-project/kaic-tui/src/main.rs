#[tokio::main]
async fn main() -> std::process::ExitCode {
    kaic_tui::cli::run(std::env::args().skip(1).collect())
        .await
        .into()
}
