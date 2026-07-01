//! xembsni daemon.
//!
//! Bridges legacy XEmbed system-tray icons (as spawned by Wine/Proton apps and
//! other X11 programs) to the modern StatusNotifierItem (SNI) D-Bus protocol
//! consumed by Wayland bars (waybar, etc.).
//!
//! Designed to run as a user `systemd` service under compositors like niri and
//! Hyprland. It runs in the foreground and logs to stderr so journald captures
//! output; set `RUST_LOG` (e.g. `RUST_LOG=debug`) to adjust verbosity.

use std::thread;

use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use xembsni_tray_host::{IconEvent, TrayHost};

const HELP: &str = "\
xembsni — bridge XEmbed system-tray icons to StatusNotifierItem (SNI)

USAGE:
    xembsni [OPTIONS]

OPTIONS:
    -h, --help       Print this help and exit
    -V, --version    Print version and exit

The daemon runs in the foreground and logs to stderr. Set RUST_LOG to control
verbosity (e.g. RUST_LOG=debug). It exits cleanly on SIGINT/SIGTERM.
";

enum CliAction {
    Help,
    Version,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<CliAction>, String> {
    match args.next().as_deref() {
        None => Ok(None),
        Some("-h" | "--help") => Ok(Some(CliAction::Help)),
        Some("-V" | "--version") => Ok(Some(CliAction::Version)),
        Some(other) => Err(format!("unknown argument: {other}")),
    }
}

/// Build the Tokio runtime and run the daemon to completion.
///
/// Async is used only at the outer lifecycle boundary (signals and D-Bus). The
/// X11 event loop is synchronous and runs on a dedicated thread.
pub fn run() -> anyhow::Result<()> {
    match parse_args(std::env::args().skip(1)) {
        Ok(Some(CliAction::Help)) => {
            print!("{HELP}");
            return Ok(());
        }
        Ok(Some(CliAction::Version)) => {
            println!("xembsni {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Ok(None) => {}
        Err(err) => {
            eprintln!("{err}\n\n{HELP}");
            std::process::exit(2);
        }
    }

    init_tracing();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

async fn async_main() -> anyhow::Result<()> {
    info!(version = env!("CARGO_PKG_VERSION"), "starting xembsni");

    // Acquire the X11 tray selection before anything else — fail fast if
    // another tray owns it.
    let host = TrayHost::acquire()?;
    let control = host.control()?;
    let waker = host.waker()?;
    info!(selection = host.selection_name(), "tray host ready");

    // The blocking X11 event loop runs on its own thread and forwards events
    // to the async bridge over a channel. When the thread exits it drops `tx`,
    // which ends the bridge.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<IconEvent>();
    let x11_thread = thread::Builder::new()
        .name("x11-tray-host".into())
        .spawn(move || {
            host.run(|event| {
                let _ = tx.send(event);
            })
        })?;

    let mut bridge = tokio::spawn(xembsni_bridge::run(rx, control));

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut signalled = false;
    tokio::select! {
        _ = sigterm.recv() => { info!("received SIGTERM; shutting down"); signalled = true; }
        _ = sigint.recv() => { info!("received SIGINT; shutting down"); signalled = true; }
        result = &mut bridge => report_bridge(result),
    }

    if signalled {
        let _ = waker.wake();
        report_bridge(bridge.await);
    }

    // Ensure the X11 loop is unblocked even if the bridge exited on its own
    // (e.g. because it failed to reach the bus), then join it.
    let _ = waker.wake();
    match x11_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => error!(%err, "X11 event loop exited with an error"),
        Err(_) => error!("X11 thread panicked"),
    }

    info!("xembsni stopped");
    Ok(())
}

fn report_bridge(result: Result<anyhow::Result<()>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => error!(%err, "bridge exited with an error"),
        Err(err) if err.is_cancelled() => {}
        Err(err) => error!(%err, "bridge task panicked"),
    }
}
