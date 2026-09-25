//! wireutils — edit one host list and write it into many WireGuard configs.
//!
//! The problem this solves: an `amnesiawg` folder with twenty `.conf` files,
//! each with its own `AllowedIPs`, and one site to add to all of them. Doing
//! it by hand is twenty edits and twenty chances to typo a key.
//!
//! The library is deliberately free of any GUI dependency so the whole of
//! its behaviour is testable without a display: the wx front end is behind
//! the `gui` feature and is a thin layer over these modules.
//!
//! * [`wgconf`] — read and rewrite `AllowedIPs` without touching anything
//!   else in the file.
//! * [`hosts`] — the owner's host list and `hosts.json`.
//! * [`resolve`] — domains to addresses, cached, with failures surfaced.
//! * [`templates`] — the catalog of known sites, updatable from the internet.
//! * [`apply`] — the folder-wide write, with backups.
//! * [`subtract`] — the inverse: taking addresses back out of the folder,
//!   one entry at a time, when the list and the files have drifted.
//! * [`import`] — building a starting list out of an existing base config.
//! * [`paths`] — where `hosts.json` lives on Windows and Linux.

pub mod apply;
pub mod hosts;
pub mod import;
pub mod paths;
pub mod resolve;
pub mod subtract;
pub mod sync;
pub mod templates;
pub mod wgconf;

pub use hosts::{Error, Host, HostStore, Result, Source};
pub use import::{Imported, NameResolver, PtrResolver};
pub use resolve::{resolve_all, Outcome, Resolver, SystemResolver};
pub use templates::Catalog;

#[cfg(feature = "gui")]
pub mod gui;
#[cfg(feature = "gui")]
pub mod tray;
#[cfg(feature = "gui")]
pub mod guiassert;
