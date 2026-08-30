#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("StatsAI menu bar app is only supported on macOS.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
fn main() {
    if std::env::args().len() > 1 {
        eprintln!("statsai-menubar does not accept command-line arguments.");
        eprintln!("Use the `statsai` binary for CLI commands.");
        std::process::exit(1);
    }

    if let Err(message) = macos::run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
