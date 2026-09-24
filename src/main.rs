//! The command-line front end.
//!
//! It exists for two reasons: it is the whole of the app's behaviour in a
//! form that can be run and checked without a window, and it is a usable
//! tool in its own right for a scripted apply. Every command prints what it
//! did in lines a screen reader can read back — no columns, no colour, no
//! cursor movement.

use std::path::PathBuf;
use std::process::ExitCode;

use wireutils::apply::{self, FileResult};
use wireutils::hosts::HostStore;
use wireutils::paths;
use wireutils::resolve::{self, Outcome, SystemResolver};
use wireutils::templates;

const USAGE: &str = "\
wireutils — one host list, many WireGuard configs

Usage:
  wireutils <command> [options]

Commands:
  init                     create hosts.json if it is missing
  set-conf-dir <dir>       set the folder of .conf files
  show                     print the config dir and the host list
  add <target> [-g GROUP]  add a host by domain or IP (repeat -g for more)
  rm <target>              remove a host
  rename <old> <new>       rename a host
  group add <name>         create a group
  group rm <name>          remove a group (its hosts stay in the list)
  group apply <name>       add a template's hosts as a group
  resolve [--force]        fill in addresses for every domain
  apply [--dry-run]        write the list into every .conf in the folder
  templates [QUERY]        list the catalog, or search it
  templates-update [URL]   fetch a newer catalog from the internet

Options:
  -g, --group <name>       group to put a newly added host in
  --force                  re-resolve hosts that already have addresses
  --dry-run                show what apply would write, change nothing
  -h, --help               this text
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Load `hosts.json`, or an empty store when it does not exist yet. Creating
/// it on demand is why `init` is optional: the first `add` writes it.
fn load() -> Result<(HostStore, PathBuf), String> {
    let path = paths::store_path().ok_or("cannot find a config directory")?;
    if !path.exists() {
        return Ok((HostStore::default(), path));
    }
    HostStore::load(&path).map(|s| (s, path)).map_err(|e| e.to_string())
}

fn save(store: &HostStore, path: &PathBuf) -> Result<(), String> {
    store.save(path).map_err(|e| e.to_string())
}

/// The folder of `.conf` files: the `--dir` option wins, then the store.
fn conf_dir(store: &HostStore, opt: Option<String>) -> Result<PathBuf, String> {
    if let Some(d) = opt {
        return Ok(PathBuf::from(d));
    }
    store
        .conf_dir
        .clone()
        .map(PathBuf::from)
        .ok_or_else(|| "no conf folder set — run `wireutils set-conf-dir <dir>`".to_string())
}

/// Pull `-g`/`--group` values out of an argument list.
fn take_groups(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut groups = Vec::new();
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-g" || args[i] == "--group" {
            if i + 1 < args.len() {
                groups.push(args[i + 1].clone());
                i += 2;
                continue;
            }
        }
        rest.push(args[i].clone());
        i += 1;
    }
    (groups, rest)
}

fn run(args: &[String]) -> Result<(), String> {
    let cmd = args[0].as_str();
    let (groups, rest) = take_groups(&args[1..]);
    let (store, path) = load()?;

    match cmd {
        "init" => {
            if path.exists() {
                println!("{} already exists", path.display());
                return Ok(());
            }
            save(&store, &path)?;
            println!("created {}", path.display());
        }

        "set-conf-dir" => {
            let dir = rest.first().ok_or("set-conf-dir needs a folder")?;
            let dir = PathBuf::from(dir);
            if !dir.is_dir() {
                return Err(format!("{} is not a folder", dir.display()));
            }
            let mut store = store;
            store.conf_dir = Some(dir.to_string_lossy().to_string());
            save(&store, &path)?;
            let n = apply::conf_files(&dir).map_err(|e| e.to_string())?.len();
            println!("conf folder: {}", dir.display());
            println!("{n} .conf file(s) found");
        }

        "show" => {
            println!("store:      {}", path.display());
            println!(
                "conf dir:   {}",
                store.conf_dir.as_deref().unwrap_or("(not set)")
            );
            if store.hosts.is_empty() {
                println!("hosts:      (none)");
            } else {
                println!("hosts:      {}", store.hosts.len());
                for h in &store.hosts {
                    let group = if h.groups.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", h.groups.join(", "))
                    };
                    let addr = if h.ips.is_empty() {
                        "  (unresolved)".to_string()
                    } else {
                        format!("  {}", h.ips.join(" "))
                    };
                    println!("  {}{}{}", h.target, group, addr);
                }
            }
            if !store.groups.is_empty() {
                println!("groups:     {}", store.groups.join(", "));
            }
        }

        "add" => {
            let target = rest.first().ok_or("add needs a domain or IP")?;
            let mut store = store;
            store.add_host(target, &groups);
            save(&store, &path)?;
            println!("added {target}");
            if !groups.is_empty() {
                println!("group(s): {}", groups.join(", "));
            }
        }

        "rm" => {
            let target = rest.first().ok_or("rm needs a domain or IP")?;
            let mut store = store;
            store.remove_host(target).map_err(|e| e.to_string())?;
            save(&store, &path)?;
            println!("removed {target}");
        }

        "rename" => {
            let old = rest.first().ok_or("rename needs the old target")?;
            let new = rest.get(1).ok_or("rename needs the new target")?;
            let mut store = store;
            store.rename_host(old, new).map_err(|e| e.to_string())?;
            save(&store, &path)?;
            println!("renamed {old} to {new}");
        }

        "group" => {
            let sub = rest.first().ok_or("group needs add/rm/apply")?;
            let name = rest.get(1).ok_or("group needs a name")?;
            let mut store = store;
            match sub.as_str() {
                "add" => {
                    store.ensure_group(name);
                    save(&store, &path)?;
                    println!("group {name} added");
                }
                "rm" => {
                    let n = store.remove_group(name).map_err(|e| e.to_string())?;
                    save(&store, &path)?;
                    println!("group {name} removed; {n} host(s) kept in the list");
                }
                "apply" => {
                    let catalog = templates::builtin();
                    let n = catalog.apply(&mut store, name)?;
                    save(&store, &path)?;
                    println!("template {name}: {n} host(s) added");
                }
                other => return Err(format!("unknown group subcommand {other}")),
            }
        }

        "resolve" => {
            let force = rest.iter().any(|a| a == "--force");
            let mut store = store;
            let outcomes = resolve::resolve_all(&mut store, &SystemResolver, force);
            save(&store, &path)?;
            for (target, outcome) in outcomes {
                match outcome {
                    Outcome::Literal => {}
                    Outcome::Fresh => println!("{target}: cached"),
                    Outcome::Resolved(ips) => println!("{target}: {} address(es)", ips.len()),
                    Outcome::Failed(e) => println!("{target}: FAILED — {e}"),
                }
            }
            // A failed resolve is worth a non-zero exit: it means the config
            // would be written without that host.
            if store.unresolved().iter().any(|h| h.error.is_some()) {
                return Err("some hosts could not be resolved".to_string());
            }
        }

        "apply" => {
            let dry = rest.iter().any(|a| a == "--dry-run");
            let dir = conf_dir(&store, None)?;
            let value = apply::value_or_error(&store).map_err(|e| e.to_string())?;
            if dry {
                println!("AllowedIPs = {value}");
                for p in apply::conf_files(&dir).map_err(|e| e.to_string())? {
                    println!("would write {}", p.display());
                }
                return Ok(());
            }
            let report = apply::apply(&store, &dir).map_err(|e| e.to_string())?;
            for (p, r) in &report.files {
                let what = match r {
                    FileResult::Unchanged => "unchanged",
                    FileResult::Written { .. } => "written",
                    FileResult::NoAllowedIps => "SKIPPED — no AllowedIPs line",
                };
                println!("{}: {}", p.display(), what);
            }
            for (p, e) in &report.errors {
                println!("{}: ERROR — {e}", p.display());
            }
            println!(
                "{} written, {} unchanged",
                report.written(),
                report.unchanged()
            );
            if !report.errors.is_empty() {
                return Err(format!("{} file(s) failed", report.errors.len()));
            }
        }

        "templates" => {
            let q = rest.first().map(String::as_str).unwrap_or("");
            let catalog = templates::builtin();
            for t in catalog.search(q) {
                println!("{} — {} host(s)", t.name, t.domains.len());
                for d in &t.domains {
                    println!("  {d}");
                }
            }
        }

        "templates-update" => {
            let url = rest
                .first()
                .cloned()
                .unwrap_or_else(|| templates::DEFAULT_CATALOG_URL.to_string());
            let updated = templates::fetch(&url)?;
            let builtin = templates::builtin();
            let merged = templates::merge(updated, builtin);
            println!("catalog updated: {} template(s)", merged.templates.len());
            println!(
                "note: the catalog is compiled into this binary — rebuild to ship it, \
                 or point the GUI at {DEFAULT}",
                DEFAULT = templates::DEFAULT_CATALOG_URL
            );
        }

        other => return Err(format!("unknown command {other}\n\n{USAGE}")),
    }
    Ok(())
}
