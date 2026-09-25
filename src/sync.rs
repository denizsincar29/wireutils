//! Pull one AmnesiaWG config down from a server and put it where the client
//! reads it.
//!
//! The owner's shape for this: «хочу сделать мелкий бинарь, который из
//! vpn.denizsincar.ru берёт конфиг определённый, например если я бинарь
//! скомпилировал для Василисы, то её vasilisa.conf фетчится и ты просто
//! перезапускаешь амнезию и всё».
//!
//! Why this has to exist at all: the AmnesiaWG client imports a config once,
//! by hand, and then never looks at the file it was imported from again. Its
//! manager service re-reads only its **own** copy, under the client's data
//! directory. So a config that changes on the server does not reach the
//! tunnel until something writes that copy and restarts the tunnel service.
//! That something is this module.
//!
//! Two platform notes, both load-bearing:
//!
//! * The store path differs by platform ([`config_store_dir`]). On Windows it
//!   is the Program Files path the `amneziawg-windows` package uses; on
//!   Linux — where the recipient binaries can actually be run and checked
//!   without a window — it is the client's per-user directory.
//! * Replacing the file is not enough on Windows. The tunnel service reads
//!   its copy at start, so the tunnel has to be **restarted** after a
//!   changed write. That is [`restart_tunnel`], and it is a shell-out to the
//!   client's own `amnesiawg.exe`, not a self-invented mechanism.

use std::path::{Path, PathBuf};

/// Where the AmnesiaWG client keeps the copy it actually dials.
///
/// Windows: `C:\Program Files\AmnesiaWG\Data\Configurations`, which is the
/// `amneziawg-windows` fork of `conf/path_windows.go` — `FOLDERID_ProgramFiles`
/// + `AmnesiaWG` + `Data/Configurations`. The upstream WireGuard-windows
/// package would say `WireGuard` there, and the fork says `AmnesiaWG`; that
/// one directory name is the whole difference and it is why this function
/// cannot be shared with the upstream path.
///
/// Linux: the client is not packaged by the vendor, so the recipients run the
/// config from a plain directory. `$WIREUTILS_CONF_STORE` overrides both.
pub fn config_store_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("WIREUTILS_CONF_STORE") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(windows)]
    {
        return PathBuf::from(r"C:\Program Files\AmnesiaWG\Data\Configurations");
    }
    #[cfg(not(windows))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("amneziawg").join("Configurations");
            }
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        PathBuf::from(home)
            .join(".config")
            .join("amneziawg")
            .join("Configurations")
    }
}

/// The file a named tunnel is stored in.
///
/// The store keys files by tunnel name — `filepath.Join(configFileDir,
/// name + ".conf")` in the client's own `conf/store.go` — so the name the
/// server serves is the name that has to land here. A name that would escape
/// the directory is refused rather than sanitised: silently writing
/// `vasilisa.conf` when the server asked for `../../vasilisa` is exactly the
/// kind of quiet wrong answer this app exists not to give.
pub fn config_path(tunnel_name: &str) -> Result<PathBuf, String> {
    let name = tunnel_name.trim();
    if name.is_empty() {
        return Err("the tunnel name is empty".to_string());
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(format!(
            "the tunnel name {name:?} names a path, not a tunnel — refusing it"
        ));
    }
    Ok(config_store_dir().join(format!("{name}.conf")))
}

/// Fetch the text of a config.
///
/// Same three sources as the catalog fetcher, and for the same reason: the
/// container's tests and the owner's own checks use the local-path and
/// `file://` branches, the shipped binary uses the http one.
pub fn fetch_text(base_url: &str, tunnel_name: &str) -> Result<String, String> {
    let name = tunnel_name.trim();
    if name.is_empty() {
        return Err("the tunnel name is empty".to_string());
    }
    // A name that names a path is refused here too, before it is pasted onto
    // a URL.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(format!(
            "the tunnel name {name:?} names a path, not a tunnel — refusing it"
        ));
    }
    let url = format!("{}/{name}.conf", base_url.trim_end_matches('/'));

    let text = if let Some(path) = url.strip_prefix("file://") {
        std::fs::read_to_string(path).map_err(|e| format!("{url}: {e}"))?
    } else if url.starts_with("http://") || url.starts_with("https://") {
        let response = ureq::get(&url).call().map_err(|e| format!("{url}: {e}"))?;
        if response.status() != 200 {
            return Err(format!("{url}: HTTP {}", response.status()));
        }
        response
            .into_string()
            .map_err(|e| format!("{url}: {e}"))?
    } else {
        // A bare directory or file path, for local use.
        let p = PathBuf::from(&url);
        std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?
    };

    // A config that is not a WireGuard/AmnesiaWG config at all is worse than
    // no config: written to the store it makes the tunnel fail to dial, and
    // the reason is invisible from the outside. `WgConfig::parse` tells us
    // whether there is anything to dial at all.
    let parsed = crate::wgconf::WgConfig::parse(&text);
    if !parsed.has_allowed_ips() {
        return Err(format!(
            "{url}: fetched, but it has no AllowedIPs line — refusing to install it"
        ));
    }
    Ok(text)
}

/// Fetch `tunnel_name` from `base_url` and write it into the store.
///
/// Returns the path written and whether the bytes actually changed. An
/// unchanged fetch is a real outcome and is reported as one: the caller polls
/// every ten minutes, and "nothing changed" must not look like "I did
/// something" — nor must it restart a working tunnel for nothing.
pub fn install(base_url: &str, tunnel_name: &str) -> Result<Install, String> {
    let text = fetch_text(base_url, tunnel_name)?;
    let path = config_path(tunnel_name)?;
    let changed = match std::fs::read_to_string(&path) {
        Ok(existing) => existing != text,
        Err(_) => true,
    };
    if changed {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        // Write beside the target and rename over it, so a fetch that dies
        // mid-write cannot leave the client with half a config.
        let tmp = path.with_extension("conf.tmp");
        std::fs::write(&tmp, &text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })?;
    }
    Ok(Install { path, changed })
}

#[derive(Debug, Clone)]
pub struct Install {
    pub path: PathBuf,
    /// False when the store already held exactly these bytes.
    pub changed: bool,
}

/// Whether the tunnel service for `tunnel_name` is running, and the result of
/// asking it to restart.
///
/// Restarting is a shell-out to the client's own binary rather than a
/// re-implementation of the service protocol: `/uninstalltunnelservice NAME`
/// then `/installtunnelservice PATH` is the pair the client itself uses, it
/// needs elevation on Windows (the binary asks for it on start), and building
/// our own `sc` calls would be a second implementation of something that
/// already exists and is tested by its authors.
#[cfg(windows)]
pub fn restart_tunnel(tunnel_name: &str) -> Result<String, String> {
    let exe = amnesiawg_exe()?;
    let path = config_path(tunnel_name)?;
    if !path.exists() {
        return Err(format!(
            "{} does not exist yet — fetch a config before restarting the tunnel",
            path.display()
        ));
    }
    let mut steps: Vec<String> = Vec::new();
    let down = std::process::Command::new(&exe)
        .arg("/uninstalltunnelservice")
        .arg(tunnel_name)
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    if down.status.success() {
        steps.push("tunnel service stopped".to_string());
    } else {
        // Not running is the normal case on a fresh fetch, not a failure.
        steps.push("no tunnel service was running".to_string());
    }
    let up = std::process::Command::new(&exe)
        .arg("/installtunnelservice")
        .arg(&path)
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    if !up.status.success() {
        return Err(format!(
            "{} /installtunnelservice {} failed: {}",
            exe.display(),
            path.display(),
            String::from_utf8_lossy(&up.stderr).trim()
        ));
    }
    steps.push("tunnel service started".to_string());
    Ok(steps.join(", "))
}

#[cfg(windows)]
fn amnesiawg_exe() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("WIREUTILS_AMNESIAWG") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    for candidate in [
        r"C:\Program Files\AmnesiaWG\amnesiawg.exe",
        r"C:\Program Files\AmnesiaWG\AmnesiaWG.exe",
    ] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(
        "amnesiawg.exe was not found in C:\\Program Files\\AmnesiaWG — set \
         WIREUTILS_AMNESIAWG to its full path"
            .to_string(),
    )
}

/// On platforms without the Windows client there is no tunnel service to
/// restart: the recipient runs `wg-quick` or the app by hand. Saying so is
/// better than pretending to have restarted something.
#[cfg(not(windows))]
pub fn restart_tunnel(_tunnel_name: &str) -> Result<String, String> {
    Ok("no tunnel service on this platform — restart the client yourself".to_string())
}

/// Stop the tunnel service without starting it again — the menu's "off".
#[cfg(windows)]
pub fn stop_tunnel(tunnel_name: &str) -> Result<(), String> {
    let exe = amnesiawg_exe()?;
    let out = std::process::Command::new(&exe)
        .arg("/uninstalltunnelservice")
        .arg(tunnel_name)
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{}: stopping the tunnel failed: {}",
            exe.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(not(windows))]
pub fn stop_tunnel(_tunnel_name: &str) -> Result<(), String> {
    Ok(())
}

/// The store path as a string, for printing.
pub fn store_dir_label() -> String {
    config_store_dir().display().to_string()
}

/// True when `dir` looks like a config store we can write into.
pub fn store_is_usable(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        if !dir.is_dir() {
            return Err(format!("{} is not a directory", dir.display()));
        }
        Ok(())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_names_a_path_is_refused() {
        for bad in ["../etc/passwd", "a/b", "a\\b", ".."] {
            assert!(config_path(bad).is_err(), "{bad} should be refused");
            assert!(fetch_text("http://x", bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn a_plain_name_becomes_a_conf_file() {
        let p = config_path("vasilisa").unwrap();
        assert!(p.ends_with("vasilisa.conf"), "{p:?}");
    }

    #[test]
    fn the_store_override_wins() {
        // The override's spelling has to be the platform's own. A hard-coded
        // `/tmp/…` is a POSIX path, and `config_path` joins a suffix to it —
        // so on Windows the assertion would fail on a difference that says
        // nothing about the code under test. `temp_dir` is the honest stand-in
        // for "wherever the owner points this". (This test passed here and
        // failed on CI, which is exactly what a hard-coded separator buys.)
        let _guard = STORE_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("wireutils_store_test");
        std::env::set_var("WIREUTILS_CONF_STORE", &dir);
        assert_eq!(config_store_dir(), dir);
        assert_eq!(config_path("vasilisa").unwrap(), dir.join("vasilisa.conf"));
        std::env::remove_var("WIREUTILS_CONF_STORE");
    }

    #[test]
    fn a_fetch_that_is_not_a_config_is_refused() {
        let dir = std::env::temp_dir().join("wireutils_sync_notaconfig");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("served.conf");
        std::fs::write(&file, "hello, this is HTML, not a tunnel").unwrap();
        let base = format!("file://{}", dir.display());
        let err = fetch_text(&base, "served").unwrap_err();
        assert!(err.contains("no AllowedIPs"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both tests that touch `$WIREUTILS_CONF_STORE` hold this first.
    ///
    /// The variable is process-wide and `cargo test` runs the tests on
    /// threads, so without a lock one test's override is read by another's
    /// `config_path` — which is not a hypothetical: it turned CI red on a
    /// commit that changed nothing about either test. The lock is also why
    /// neither of these tests needs the old value saved and restored; while
    /// it is held, nobody else is looking at the variable.
    static STORE_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn install_writes_and_then_reports_unchanged() {
        let _guard = STORE_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("wireutils_sync_install");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("vasilisa.conf"),
            "[Interface]\nPrivateKey = a\n\n[Peer]\nAllowedIPs = 1.2.3.4/32\n",
        )
        .unwrap();

        let base = format!("file://{}", dir.display());
        let store = dir.join("store");
        std::env::set_var("WIREUTILS_CONF_STORE", &store);

        let first = install(&base, "vasilisa").unwrap();
        assert!(first.changed, "a first install must report a change");
        assert!(first.path.exists());

        let second = install(&base, "vasilisa").unwrap();
        assert!(!second.changed, "the same bytes must report no change");

        std::env::remove_var("WIREUTILS_CONF_STORE");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
