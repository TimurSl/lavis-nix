use anyhow::Context;

pub mod auth;
pub mod client;
pub mod command;
pub mod config;
pub mod error;

pub async fn run() -> anyhow::Result<()> {
    let config = config::Config::load().context("failed to load configuration")?;
    let client = client::TelegramClient::connect(&config)
        .await
        .context("failed to open the Telegram session")?;

    let run_result = async {
        auth::authorize(client.client(), &config)
            .await
            .context("Telegram authorization failed")?;

        tracing::info!(event = "application_started", "lavis is running");
        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for Ctrl-C shutdown signal")?;
        Ok(())
    }
    .await;

    let shutdown_result = client.shutdown().await;
    match (run_result, shutdown_result) {
        (Ok(()), Ok(())) => {
            tracing::info!(event = "application_stopped", "lavis stopped");
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(shutdown_error)) => {
            tracing::error!(
                event = "application_shutdown_failed",
                %shutdown_error,
                "Telegram runner shutdown failed"
            );
            Err(error.context("Telegram runner shutdown also failed"))
        }
    }
}
