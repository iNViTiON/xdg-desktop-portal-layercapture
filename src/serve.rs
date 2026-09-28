//! `serve`: the portal backend service (the default command).

use std::time::Duration;

use anyhow::Result;

use crate::core::{Config, Core};
use crate::portal;
use crate::runtime::PidFile;
use crate::watchdog::Watchdog;

#[derive(clap::Args, Clone, Debug)]
pub struct ServeArgs {
    /// Outward push (px of relative motion) against a barrier needed to start capturing.
    #[arg(long, default_value_t = 24.0, value_parser = positive_f64)]
    pressure: f64,
    /// A motion only counts as a push if |along-edge| <= max_slope * |outward|.
    #[arg(long, default_value_t = 0.5, value_parser = positive_f64)]
    max_slope: f64,
    /// Keep barrier strips this many px away from output corners (niri's hot corner).
    #[arg(long, default_value_t = 8)]
    corner_margin: i32,
    /// End a capture after this many seconds without captured input (0 = never).
    #[arg(long, default_value_t = 120)]
    idle_release: u64,
    /// End any capture after this many seconds (0 = never).
    #[arg(long, default_value_t = 1800)]
    max_activation: u64,
    /// Development cap: end any capture after this many seconds; the watchdog also cuts the
    /// Wayland connection shortly after.
    #[arg(long)]
    max_grab: Option<u64>,
    /// Also allow this app id to create sessions (repeatable; for testing).
    #[arg(long)]
    allow_app_id: Vec<String>,
    /// Accept any caller and any app id (isolated tests only).
    #[arg(long)]
    trust_any_caller: bool,
    /// Don't use Wayland; report one zone of this size (e.g. 2880x1800). For tests.
    #[arg(long, value_parser = parse_wxh)]
    fake_zones: Option<(u32, u32)>,
    /// Export a dev-only control interface (SimulateActivation, StallCore).
    #[arg(long)]
    dev_control: bool,
}

impl Default for ServeArgs {
    fn default() -> Self {
        #[derive(clap::Parser)]
        struct Only {
            #[command(flatten)]
            args: ServeArgs,
        }
        <Only as clap::Parser>::parse_from(["serve"]).args
    }
}

fn positive_f64(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Ok(v),
        _ => Err(format!("{s:?} is not a positive number")),
    }
}

fn parse_wxh(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    match (w.parse(), h.parse()) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err(format!("{s:?} is not WIDTHxHEIGHT")),
    }
}

fn secs(v: u64) -> Option<Duration> {
    (v > 0).then(|| Duration::from_secs(v))
}

pub async fn run(args: ServeArgs) -> Result<()> {
    let _pid = PidFile::create()?;
    let (conn, cmds, sig_tx) = portal::start(portal::Options {
        allow_app_ids: args.allow_app_id.clone(),
        trust_any_caller: args.trust_any_caller,
        dev_control: args.dev_control,
    })
    .await?;

    let wayland_display = if std::env::var_os("WAYLAND_DISPLAY").is_none() && args.fake_zones.is_none() {
        let d = systemd_env(&conn, "WAYLAND_DISPLAY").await;
        tracing::warn!("WAYLAND_DISPLAY unset; systemd user environment has {d:?}");
        d
    } else {
        None
    };

    let max_grab = args.max_grab.and_then(secs);
    // With a development cap, the watchdog cuts the Wayland connection 2 s after it.
    let watchdog = Watchdog::start(max_grab.map(|d| d + Duration::from_secs(2)), Duration::from_secs(30));
    let cfg = Config {
        pressure: args.pressure,
        max_slope: args.max_slope,
        corner_margin: args.corner_margin,
        idle_release: secs(args.idle_release),
        max_activation: secs(args.max_activation),
        max_grab,
        fake_zones: args.fake_zones,
        wayland_display,
    };
    tracing::info!("config: {cfg:?}");
    let core = Core::new(cfg, watchdog, sig_tx);
    let result = core.run(cmds).await;
    // Let the emitter send any last Deactivated.
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(conn);
    result
}

/// Reads one variable from the systemd user manager's environment.
async fn systemd_env(conn: &zbus::Connection, key: &str) -> Option<String> {
    let lookup = async {
        let proxy = zbus::Proxy::new(
            conn,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await
        .ok()?;
        let env: Vec<String> = proxy.get_property("Environment").await.ok()?;
        env.iter().find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=').map(str::to_owned))
    };
    tokio::time::timeout(Duration::from_secs(1), lookup).await.ok().flatten()
}
