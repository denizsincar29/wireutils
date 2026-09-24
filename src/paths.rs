//! Where things live on disk, on each of the two platforms this app runs on.
//!
//! The owner's ask was "appdata or ~/.config". On Windows the first, on
//! Linux the second; both are resolved here so the GUI and the CLI agree and
//! so no code above this module has to know which platform it is on.

use std::path::PathBuf;

/// The file name of the single shared config.
pub const STORE_FILE: &str = "hosts.json";

/// The folder holding `hosts.json`.
///
/// Honours `WIREUTILS_CONFIG_DIR` when set, which is how the tests keep off
/// the real config and how the owner can run several lists side by side.
pub fn config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("WIREUTILS_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.is_empty() {
                return Some(PathBuf::from(appdata).join("wireutils"));
            }
        }
        // A Windows box without APPDATA is unusual but possible in a
        // service context; fall back to the home directory rather than
        // failing to start.
        return std::env::var("USERPROFILE")
            .ok()
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".config").join("wireutils"));
    }
    #[cfg(not(windows))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join("wireutils"));
            }
        }
        std::env::var("HOME")
            .ok()
            .filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".config").join("wireutils"))
    }
}

/// The full path of `hosts.json`.
pub fn store_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(STORE_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_override_wins() {
        let old = std::env::var("WIREUTILS_CONFIG_DIR").ok();
        std::env::set_var("WIREUTILS_CONFIG_DIR", "/tmp/wireutils_cfg_test");
        assert_eq!(
            config_dir().unwrap(),
            PathBuf::from("/tmp/wireutils_cfg_test")
        );
        assert_eq!(
            store_path().unwrap(),
            PathBuf::from("/tmp/wireutils_cfg_test/hosts.json")
        );
        match old {
            Some(v) => std::env::set_var("WIREUTILS_CONFIG_DIR", v),
            None => std::env::remove_var("WIREUTILS_CONFIG_DIR"),
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn without_the_override_it_lands_under_xdg_or_home() {
        let old = std::env::var("WIREUTILS_CONFIG_DIR").ok();
        std::env::remove_var("WIREUTILS_CONFIG_DIR");
        let dir = config_dir().unwrap();
        assert!(dir.ends_with("wireutils"), "{dir:?}");
        assert!(dir.parent().is_some());
        if let Some(v) = old {
            std::env::set_var("WIREUTILS_CONFIG_DIR", v);
        }
    }
}
