// No console window for the release GUI build.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod crash;
mod ui;

use winsearch::config::{self, AppConfig};
use winsearch::preview::helper;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Preview helper mode. To keep crashes in Windows' PDF engine and in
    // third-party thumbnail providers away from the search window, WinSearch
    // starts a second, windowless copy of itself with this flag for each PDF
    // page or thumbnail. That copy renders one image, writes it to its
    // standard output for the parent and exits; it never does anything else.
    if args.get(1).map(String::as_str) == Some(helper::HELPER_FLAG) {
        std::process::exit(helper::helper_main(&args[2..]));
    }

    let _log_guard = init_logging();
    crash::install();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "WinSearch starting");

    let start = ui::StartOptions::from_args(&args);
    let cfg = AppConfig::load();
    if let Err(e) = cfg.save() {
        tracing::warn!("could not write settings: {e}");
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WinSearch")
            .with_inner_size([1280.0, 780.0])
            .with_min_inner_size([760.0, 420.0])
            .with_icon(ui::theme::window_icon()),
        ..Default::default()
    };
    let result = eframe::run_native(
        "WinSearch",
        options,
        Box::new(move |cc| Ok(Box::new(ui::WinSearchApp::new(cc, cfg, start)))),
    );
    if let Err(e) = result {
        tracing::error!("window failed: {e}");
        show_fatal(&format!("WinSearch could not open its window:\n{e}"));
    }
}

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::EnvFilter;
    let dir = config::log_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("winsearch")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("WINSEARCH_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .with_thread_names(true)
        .init();
    Some(guard)
}

pub(crate) fn show_fatal(msg: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::HSTRING;
    unsafe {
        MessageBoxW(None, &HSTRING::from(msg), &HSTRING::from("WinSearch"), MB_OK | MB_ICONERROR);
    }
}
