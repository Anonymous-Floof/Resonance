//! Resonance - a modern, local-first music player.
//!
//! This binary is deliberately thin: resolve paths, set up logging, load the
//! config, and hand off to the UI crate.

// Detach the console on Windows release builds so launching from Explorer does
// not flash a terminal. Debug builds keep it for `tracing` output.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use anyhow::{Context, Result};
use mp_core::{AppPaths, Config};
use mp_ui::ResonanceApp;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

fn main() -> Result<()> {
    let paths = AppPaths::resolve().context("resolving application directories")?;

    // Keep the guard alive for the process lifetime or the log file is
    // truncated on exit.
    let _log_guard = init_logging(&paths);

    tracing::info!(
        "{} {} starting{} (config: {})",
        mp_core::APP_NAME,
        mp_core::APP_VERSION,
        if AppPaths::is_portable() {
            " in portable mode"
        } else {
            ""
        },
        paths.config_dir().display()
    );

    // Checked before loading, because loading writes the file when it is
    // missing — after that there is no way to tell a first run from any other.
    let first_run = !paths.config_file().exists();

    let config = Config::load(&paths).context("loading settings")?;

    let viewport = egui::ViewportBuilder::default()
        .with_title(mp_core::APP_NAME)
        .with_inner_size([config.window.width, config.window.height])
        .with_min_inner_size(mp_ui::MIN_WINDOW_SIZE)
        .with_maximized(config.window.maximized)
        .with_app_id("resonance")
        .with_decorations(false);

    let viewport = match window_icon() {
        Some(icon) => viewport.with_icon(icon),
        None => viewport,
    };

    let options = eframe::NativeOptions {
        viewport,
        // The config file is the source of truth for settings, so eframe's own
        // persistence would only duplicate it.
        persist_window: false,
        ..Default::default()
    };

    eframe::run_native(
        mp_core::APP_NAME,
        options,
        Box::new(move |cc| Ok(Box::new(ResonanceApp::new(cc, paths, config, first_run)))),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))
    .context("running the application")?;

    Ok(())
}

/// The icon shown in the taskbar and the Alt-Tab switcher.
///
/// Separate from the icon `build.rs` compiles into the executable, which is
/// what Explorer shows. Windows will fall back to that one if this is missing,
/// but only after the window has already appeared without it.
fn window_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon.png");
    let image = match image::load_from_memory(bytes) {
        Ok(image) => image.into_rgba8(),
        Err(err) => {
            tracing::warn!("could not decode the window icon: {err}");
            return None;
        }
    };

    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

/// Log to stderr and to a rolling file under the data directory.
///
/// Returns the appender guard, which must outlive the app.
fn init_logging(paths: &AppPaths) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    // `RUST_LOG` wins if set; otherwise be quiet about dependencies and
    // chatty about our own crates.
    // `lofty` logs a warning for every VBR-less MP3 it has to estimate the
    // duration of, which is hundreds of lines for an ordinary collection and
    // says nothing actionable. Only its errors are worth surfacing.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("warn,lofty=error,resonance=info,mp_core=info,mp_ui=info,mp_audio=info")
    });

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);

    let file_appender = tracing_appender::rolling::daily(paths.log_dir(), "resonance.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false);

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    Some(guard)
}
