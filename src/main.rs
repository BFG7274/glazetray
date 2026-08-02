//! GlazeTray — GlazeWM 的轻量系统托盘状态与控制工具 (Windows 11)
//!
//! GUI subsystem, no console window. Logs go to %LOCALAPPDATA%\glazetray\logs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(clippy::too_many_arguments)]

mod app;
mod config;
mod flyout;
mod fonts;
mod icon;
mod ipc;
mod protocol;
mod reducer;
mod render;
mod startup;
mod state;
mod theme;
mod tray;
mod win32;

use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

fn log_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("glazetray")
        .join("logs")
}

fn init_logging(retention_days: u32, config_level: &str) {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);

    // Cleanup old logs beyond the retention window.
    if retention_days > 0
        && let Ok(entries) = std::fs::read_dir(&dir) {
            let cutoff = std::time::SystemTime::now()
                - std::time::Duration::from_secs(retention_days as u64 * 24 * 3600);
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata()
                    && let Ok(modified) = meta.modified()
                        && modified < cutoff {
                            let _ = std::fs::remove_file(entry.path());
                        }
            }
        }

    let appender = tracing_appender::rolling::daily(&dir, "glazetray");
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    // Keep the writer guard alive for the process lifetime.
    std::mem::forget(guard);

    // Priority: GLAZETRAY_LOG > RUST_LOG > config logging.level > "info".
    // The configured level applies at startup (a restart is required to
    // change it at runtime).
    let level = std::env::var("GLAZETRAY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| config_level.to_string());
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(false)
        .with_file(false)
        .with_line_number(false)
        .init();
}

fn init_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        tracing::error!(panic = %msg, "panic");
        // Crash log alongside the normal logs.
        if std::fs::create_dir_all(log_dir()).is_ok() {
            let path = log_dir().join("crash.log");
            let line = format!("{} — {}\n", chrono_like_now(), msg);
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map(|mut f| {
                    use std::io::Write;
                    let _ = f.write_all(line.as_bytes());
                });
        }
    }));
}

fn chrono_like_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = now / 86400;
    let (y, m, d) = civil_from_days(days as i64);
    let secs = now % 86400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Convert days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i64;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as i64;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn main() {
    init_panic_hook();

    // Load config early for the log level; missing/invalid files are handled
    // inside (defaults are used).
    let (cfg, cfg_error) = match config::load_config(&config::config_path()) {
        Ok(c) => (c, None),
        Err(config::ConfigError::Missing) => (config::Config::default(), None),
        Err(e) => (config::Config::default(), Some(e.to_string())),
    };
    let log_level = cfg.logging.level.clone();
    let retention = cfg.logging.retention_days;
    let _ = log_level;

    init_logging(retention, &log_level);

    if !app::ensure_single_instance() {
        tracing::info!("another instance is running; exiting");
        return;
    }

    let mut app = app::App::new(cfg);
    if let Some(err) = cfg_error {
        tracing::warn!(error = %err, "config invalid at startup");
    }
    app.run();
}
