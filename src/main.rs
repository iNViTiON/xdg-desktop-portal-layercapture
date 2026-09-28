//! xdg-desktop-portal InputCapture backend for niri.
//!
//! `serve` (default) runs the portal backend; `probe` and `eis-demo` are test tools.

mod core;
mod eis;
mod eisdemo;
mod keymap;
mod keynames;
mod portal;
mod probe;
mod runtime;
mod serve;
mod watchdog;
mod wayland;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "xdg-desktop-portal-layercapture", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the portal backend (the default when no command is given).
    Serve(serve::ServeArgs),
    /// Prototype: map a barrier strip on the left edge of an output and print what a grab
    /// captures. Grabs auto-release after a few seconds.
    Probe(probe::ProbeArgs),
    /// Phase 2 test: play scripted input over our EIS server to a real libei receiver
    /// (no grab).
    EisDemo(eisdemo::EisDemoArgs),
    /// Ask the running instance to release any capture (SIGUSR1 via its PID file).
    Release,
    /// Internal: minimal EIS receiver used by `eis-demo --client dump`.
    #[command(hide = true)]
    EisDump {
        #[arg(long)]
        socketfd: i32,
        #[arg(long)]
        keymap_out: Option<std::path::PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cmd = match cli.cmd {
        Some(Cmd::Release) => return runtime::send_release(),
        Some(Cmd::EisDump { socketfd, keymap_out }) => {
            return match eisdemo::dump(socketfd, keymap_out) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("eis-dump: {e:#}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(cmd) => cmd,
        None => Cmd::Serve(serve::ServeArgs::default()),
    };

    // Logging goes through a lossy background writer: the event loop must never block on a
    // slow or stalled stdout (e.g. `| less`), or the auto-release and heartbeat stop with it.
    let (writer, _log_guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(true)
        .finish(std::io::stdout());
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,zbus=warn")))
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_target(false)
        .with_writer(writer)
        .init();

    // Must happen on the main thread before tokio spawns its workers.
    runtime::set_process_name();

    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("starting tokio: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match cmd {
        Cmd::Serve(args) => rt.block_on(serve::run(args)),
        Cmd::Probe(args) => rt.block_on(probe::run(args)),
        Cmd::EisDemo(args) => rt.block_on(eisdemo::run(args)),
        Cmd::Release | Cmd::EisDump { .. } => unreachable!(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
