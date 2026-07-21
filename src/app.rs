use anyhow::Context;

pub mod command;
pub mod config;
pub mod error;

pub async fn run() -> anyhow::Result<()> {
    tracing::info!(event = "application_started", "lavis is running");

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl-C shutdown signal")?;

    tracing::info!(event = "application_stopped", "lavis stopped");
    Ok(())
}
