//! The tray icon, as an executable of its own.
//!
//! Same reasoning as the window's own binary: the owner launches it by
//! double-clicking a file, so it must not open a console behind itself, and a
//! build without wxWidgets must refuse to produce it rather than ship an exe
//! that dies on start.
//!
//! Which config it keeps up to date, and where it fetches it from, are the two
//! things that differ per recipient — the binary compiled for Vasilisa is this
//! binary with `vasilisa` compiled in, which is the whole point of the exercise.
//! They come from the arguments, then the environment, then a compiled-in
//! default, so one build can serve several people without being rebuilt.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    let tunnel = args
        .first()
        .cloned()
        .or_else(|| std::env::var("WIREUTILS_TUNNEL").ok())
        .unwrap_or_else(|| "wireguard".to_string());
    let url = args
        .get(1)
        .cloned()
        .or_else(|| std::env::var("WIREUTILS_CONF_URL").ok())
        .unwrap_or_else(|| "https://vpn.denizsincar.ru/conf".to_string());

    let code = wireutils::tray::run(tunnel, url);
    std::process::ExitCode::from(code.clamp(0, 255) as u8)
}

const USAGE: &str = "\
wireutils-tray — an icon that keeps one AmnesiaWG config up to date

Usage:
  wireutils-tray [tunnel-name] [config-base-url]

With no arguments the name comes from WIREUTILS_TUNNEL and the URL from
WIREUTILS_CONF_URL. The icon fetches the config, writes it where the client
reads it, restarts the tunnel when it changed, and checks again every ten
minutes. Right-click it for the menu.
";
