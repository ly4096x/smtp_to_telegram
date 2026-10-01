use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};

use smtp_to_telegram::config::{Cli, Config};
use smtp_to_telegram::smtp::bind_listener;

fn init_logging(level: tracing::Level) {
    let builder = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal());
    // journald adds its own timestamps.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.without_time().init();
    } else {
        builder.init();
    }
}

async fn wait_for_shutdown_signal() {
    let (Ok(mut term), Ok(mut int), Ok(mut quit)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::quit()),
    ) else {
        error!("cannot install signal handlers; stop the process with SIGKILL");
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => info!("received SIGTERM"),
        _ = int.recv() => info!("received SIGINT"),
        _ = quit.recv() => info!("received SIGQUIT"),
    }
}

async fn run(config: Config) -> anyhow::Result<()> {
    let mut listeners = Vec::new();
    for address in &config.smtp.listen {
        let listener =
            bind_listener(*address).with_context(|| format!("cannot listen on {address}"))?;
        info!("listening for SMTP on {address}");
        listeners.push(listener);
    }
    info!(
        hostname = %config.smtp.hostname,
        users = config.smtp.credentials.len(),
        allow_anonymous = config.smtp.allow_anonymous,
        plaintext_auth_networks = ?config.smtp.plaintext_auth_networks,
        chats = config.telegram.chat_ids.len(),
        "smtp_to_telegram {} ready",
        env!("CARGO_PKG_VERSION")
    );
    smtp_to_telegram::serve(Arc::new(config), listeners, wait_for_shutdown_signal()).await
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.log_level);
    let config = match Config::from_cli(cli, |name| std::env::var(name).ok()) {
        Ok(config) => config,
        Err(e) => {
            error!("configuration error: {e:#}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            error!("cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
