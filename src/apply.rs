//! Writing the host list into every `.conf` in the folder.
//!
//! Two rules, both from watching how this can go wrong:
//!
//! * The folder is written in one pass, and a file whose content would not
//!   change is not written at all. An editor or a sync client noticing a
//!   touched file is a real cost; a no-op rewrite is a lie about what
//!   happened.
//! * Before the first overwrite of a file we do not have a backup of, a
//!   `.bak` is written next to it. The configs hold keys that exist nowhere
//!   else, so an unrecoverable wrong write is not an acceptable failure mode.

use std::path::{Path, PathBuf};

use crate::hosts::{Error, HostStore, Result};
use crate::wgconf::WgConfig;

/// What one file's write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileResult {
    /// The value was already correct; nothing was written.
    Unchanged,
    /// The value was replaced.
    Written { backup: Option<PathBuf> },
    /// The file has no `AllowedIPs` line, so there is nowhere to put the
    /// list. Reported, never invented — adding a `[Peer]` to a file that
    /// has none is a guess about the owner's tunnel.
    NoAllowedIps,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub files: Vec<(PathBuf, FileResult)>,
    /// Non-fatal problems: an unreadable file, a write that failed. The pass
    /// continues so one broken config does not stop the other nineteen.
    pub errors: Vec<(PathBuf, String)>,
}

impl Report {
    pub fn written(&self) -> usize {
        self.files.iter().filter(|(_, r)| matches!(r, FileResult::Written { .. })).count()
    }
    pub fn unchanged(&self) -> usize {
        self.files.iter().filter(|(_, r)| *r == FileResult::Unchanged).count()
    }
}

/// Every `.conf` file in a folder, sorted by name so runs are reproducible.
pub fn conf_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("conf"))
                .unwrap_or(false)
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// The value to write: the store's list, or an error naming the hosts that
/// have no addresses yet.
///
/// Refusing here is the point. Writing a partial list because a domain did
/// not resolve would put traffic outside the tunnel with no visible sign
/// that anything is wrong; better to stop and say which host is unresolved.
pub fn value_or_error(store: &HostStore) -> Result<String> {
    let missing: Vec<&str> = store.unresolved().iter().map(|h| h.target.as_str()).collect();
    if !missing.is_empty() {
        return Err(Error::Invalid(format!(
            "these hosts have no addresses yet, run `resolve` first: {}",
            missing.join(", ")
        )));
    }
    if store.hosts.is_empty() {
        return Err(Error::Invalid(
            "the host list is empty — applying it would remove every route".to_string(),
        ));
    }
    Ok(store.allowed_ips_value())
}

/// Apply the store's `AllowedIPs` value to every `.conf` in its folder.
pub fn apply(store: &HostStore, dir: &Path) -> Result<Report> {
    let value = value_or_error(store)?;
    apply_value(dir, &value)
}

/// Apply an explicit value, for tests and for `apply --dry-run` output.
pub fn apply_value(dir: &Path, value: &str) -> Result<Report> {
    let mut report = Report { files: Vec::new(), errors: Vec::new() };
    for path in conf_files(dir)? {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                report.errors.push((path, e.to_string()));
                continue;
            }
        };
        let mut conf = WgConfig::parse(&text);
        if !conf.has_allowed_ips() {
            report.files.push((path, FileResult::NoAllowedIps));
            continue;
        }
        let before = conf.to_string();
        conf.set_allowed_ips(value);
        let after = conf.to_string();
        if before == after {
            report.files.push((path, FileResult::Unchanged));
            continue;
        }
        let backup = match write_with_backup(&path, &after) {
            Ok(b) => b,
            Err(e) => {
                report.errors.push((path, e.to_string()));
                continue;
            }
        };
        report.files.push((path, FileResult::Written { backup }));
    }
    Ok(report)
}

/// Write `text` to `path`, saving the previous content to `path.bak` the
/// first time — an existing `.bak` is the owner's, or an earlier rescue, and
/// is not overwritten.
fn write_with_backup(path: &Path, text: &str) -> std::io::Result<Option<PathBuf>> {
    let bak = path.with_extension("conf.bak");
    let backup = if bak.exists() {
        None
    } else {
        std::fs::copy(path, &bak)?;
        Some(bak)
    };
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wireutils_apply_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const CONF: &str = "[Interface]\nPrivateKey = aGVsbG8=\n\n[Peer]\nAllowedIPs = 0.0.0.0/0\nEndpoint = vpn.example.com:51820\n";

    #[test]
    fn every_conf_in_the_folder_is_written_the_same() {
        let dir = tmpdir("all");
        for n in ["a", "b", "c"] {
            std::fs::write(dir.join(format!("{n}.conf")), CONF).unwrap();
        }
        let report = apply_value(&dir, "1.2.3.4/32").unwrap();
        assert_eq!(report.written(), 3);
        for n in ["a", "b", "c"] {
            let t = std::fs::read_to_string(dir.join(format!("{n}.conf"))).unwrap();
            assert!(t.contains("AllowedIPs = 1.2.3.4/32"), "{n}: {t}");
            assert!(t.contains("PrivateKey = aGVsbG8="));
        }
    }

    #[test]
    fn non_conf_files_are_left_alone() {
        let dir = tmpdir("nonconf");
        std::fs::write(dir.join("a.conf"), CONF).unwrap();
        std::fs::write(dir.join("notes.txt"), "# AllowedIPs = 9.9.9.9\n").unwrap();
        apply_value(&dir, "1.2.3.4/32").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
            "# AllowedIPs = 9.9.9.9\n"
        );
    }

    #[test]
    fn a_second_run_changes_nothing_on_disk() {
        let dir = tmpdir("idempotent");
        std::fs::write(dir.join("a.conf"), CONF).unwrap();
        apply_value(&dir, "1.2.3.4/32").unwrap();
        let first = std::fs::read_to_string(dir.join("a.conf")).unwrap();
        let report = apply_value(&dir, "1.2.3.4/32").unwrap();
        assert_eq!(report.unchanged(), 1);
        assert_eq!(report.written(), 0);
        assert_eq!(std::fs::read_to_string(dir.join("a.conf")).unwrap(), first);
    }

    #[test]
    fn the_first_write_keeps_a_backup() {
        let dir = tmpdir("backup");
        std::fs::write(dir.join("a.conf"), CONF).unwrap();
        let report = apply_value(&dir, "1.2.3.4/32").unwrap();
        let bak = match &report.files[0].1 {
            FileResult::Written { backup } => backup.clone().unwrap(),
            other => panic!("{other:?}"),
        };
        assert_eq!(std::fs::read_to_string(bak).unwrap(), CONF);
    }

    #[test]
    fn an_existing_backup_is_not_overwritten() {
        let dir = tmpdir("keep_bak");
        std::fs::write(dir.join("a.conf"), CONF).unwrap();
        std::fs::write(dir.join("a.conf.bak"), "the owner's own rescue copy\n").unwrap();
        apply_value(&dir, "1.2.3.4/32").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("a.conf.bak")).unwrap(),
            "the owner's own rescue copy\n"
        );
    }

    #[test]
    fn a_file_without_allowed_ips_is_reported_not_edited() {
        let dir = tmpdir("no_allowed");
        std::fs::write(dir.join("a.conf"), "[Interface]\nPrivateKey = x\n").unwrap();
        let report = apply_value(&dir, "1.2.3.4/32").unwrap();
        assert_eq!(report.files[0].1, FileResult::NoAllowedIps);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.conf")).unwrap(),
            "[Interface]\nPrivateKey = x\n"
        );
        assert!(!dir.join("a.conf.bak").exists());
    }

    #[test]
    fn an_empty_host_list_is_refused_rather_than_writing_nothing() {
        let store = HostStore::default();
        assert!(matches!(value_or_error(&store), Err(Error::Invalid(_))));
    }

    #[test]
    fn an_unresolved_host_stops_the_apply_and_is_named() {
        let mut store = HostStore::default();
        store.add_host("api.openai.com", &[]);
        store.add_host("1.2.3.4", &[]);
        let err = value_or_error(&store).unwrap_err().to_string();
        assert!(err.contains("api.openai.com"), "{err}");
    }

    #[test]
    fn a_fully_resolved_store_applies() {
        let dir = tmpdir("resolved");
        std::fs::write(dir.join("a.conf"), CONF).unwrap();
        let mut store = HostStore::default();
        store.add_host("1.2.3.4", &[]);
        store.add_host("api.openai.com", &[]);
        store.hosts[1].ips = vec!["5.6.7.8".into(), "9.9.9.9".into()];
        let report = apply(&store, &dir).unwrap();
        assert_eq!(report.written(), 1);
        let t = std::fs::read_to_string(dir.join("a.conf")).unwrap();
        assert!(t.contains("AllowedIPs = 1.2.3.4/32, 5.6.7.8/32, 9.9.9.9/32"), "{t}");
    }

    #[test]
    fn a_missing_folder_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join("wireutils_does_not_exist_1234");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(apply_value(&dir, "1.2.3.4/32").is_err());
    }

    #[test]
    fn conf_files_are_listed_sorted() {
        let dir = tmpdir("sorted");
        for n in ["c", "a", "b"] {
            std::fs::write(dir.join(format!("{n}.conf")), CONF).unwrap();
        }
        let names: Vec<String> = conf_files(&dir)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.conf", "b.conf", "c.conf"]);
    }
}
