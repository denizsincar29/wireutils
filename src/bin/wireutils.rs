//! The window, as an executable of its own.
//!
//! It exists so the program can be *launched* the way the owner actually uses
//! it: a file on the desktop, double-clicked, no terminal in sight. Until this
//! target existed the only binary was `wireutils-cli`, so a shortcut on the
//! desktop opened a console window with a command line in it, and the window —
//! which is in the same code — took a `gui` argument to reach.
//!
//! Two things make that work, and both are about the absence of a console:
//!
//! * `windows_subsystem = "windows"` detaches the process from the console, so
//!   no black rectangle appears behind the window. It is set only for release
//!   builds: in a debug build a panic message and every `eprintln!` need the
//!   console to land in, and a detached GUI process has nowhere to print. The
//!   line is `cfg_attr`-gated rather than `cfg`-gated because the attribute
//!   only means something on Windows and Rust rejects an unknown subsystem on
//!   other platforms.
//! * The `gui` feature is required. A `wireutils.exe` built without wxWidgets
//!   would link, start, and die on the first call into wx — a dead shortcut,
//!   which is a worse thing to hand someone than a build that refuses to
//!   happen. `required-features` in `Cargo.toml` makes that refusal explicit.
//!
//! The exit code is passed through unchanged: the window returns 0 on a normal
//! close, and a non-zero code means something went wrong that the owner would
//! otherwise never see.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    let code = wireutils::gui::run();
    if code != 0 {
        // Nothing is shown to the owner for this — there is no console to show
        // it in — so the code is at least left where a shell can read it, and
        // the window itself has already reported the problem in its status bar
        // or its startup error dialog.
        eprintln!("wireutils: the window exited with status {code}");
    }
    std::process::ExitCode::from(code.clamp(0, 255) as u8)
}
