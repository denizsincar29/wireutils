//! Taking addresses back out of the `.conf` files.
//!
//! The list in `hosts.json` is what the owner wants in the tunnel; the files
//! are the truth on disk, and the two drift the moment anything changes a
//! config by hand or a domain resolves differently. Removing a whole group
//! from the list is not enough for that: the addresses are already written
//! into twenty files, and the only way to get them out is to edit those
//! files.
//!
//! So this module is the inverse of [`crate::apply`], and it works the same
//! way: line-based, every other byte preserved, a `.bak` before the first
//! change to a file. What it removes is a *set of entries*, matched per
//! entry rather than per line — an `AllowedIPs` line with three addresses on
//! it loses one and keeps two, because dropping the line would take two
//! routes the owner never asked to remove.
//!
//! Two rules that are more important than the removal itself:
//!
//! * An empty result is never written. `AllowedIPs =` with nothing after it
//!   is a config WireGuard accepts and routes nothing through — the exact
//!   silent failure this program exists to prevent. The file is reported and
//!   left alone.
//! * The set is matched as written, so `/32` and `/128` are stripped before
//!   comparing: a config written by hand says `10.1.2.3`, and the list says
//!   `10.1.2.3/32`. Those are the same route and must not both survive.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::apply::{write_with_backup, ConfResult, Report};
use crate::wgconf::WgConfig;
use crate::Result;

/// The address part of an entry, without the prefix length: `10.0.0.1/32`
/// and `10.0.0.1` are one route written two ways.
fn bare(entry: &str) -> &str {
    entry.split('/').next().unwrap_or(entry).trim()
}

/// A set of addresses to remove, in the form the files are compared in.
///
/// Built from a group's hosts — see [`entries_from_group`] — and used as a
/// set rather than a list because membership is the only question asked.
#[derive(Debug, Clone, Default)]
pub struct Removals {
    addrs: HashSet<String>,
}

impl Removals {
    pub fn new<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Removals {
            addrs: entries
                .into_iter()
                .map(|e| bare(e.as_ref()).to_string())
                .filter(|e| !e.is_empty())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.addrs.len()
    }

    /// The entries of one host in the matching form: the cached addresses,
    /// or the target itself when that is already an address.
    pub fn from_host(host: &crate::Host) -> Self {
        Removals::new(host.allowed_entries())
    }

    /// True when this entry means one of the addresses being removed.
    fn matches(&self, entry: &str) -> bool {
        self.addrs.contains(bare(entry))
    }
}

/// The addresses every host in `group` would put into a config.
///
/// A host the folder never resolved contributes nothing: it has no addresses
/// to match, and inventing one from its name would remove a route by name,
/// which is not a thing that exists in a config.
pub fn entries_from_group(store: &crate::HostStore, group: &str) -> Vec<String> {
    store
        .hosts_in_group(group)
        .flat_map(|h| h.allowed_entries())
        .collect()
}

/// Remove `removals` from the `AllowedIPs` of every `.conf` in `dir`.
///
/// Writes only the files that actually change. A file whose every entry is
/// being removed is reported as [`ConfResult::Emptied`] and left as it was —
/// see the note at the top of the module for why that is not a write.
pub fn subtract(dir: &Path, removals: &Removals) -> Result<Report> {
    if removals.is_empty() {
        return Err(crate::Error::Invalid(
            "nothing to remove: the selection has no addresses".to_string(),
        ));
    }

    let mut report = Report {
        files: Vec::new(),
        errors: Vec::new(),
    };

    for path in crate::apply::conf_files(dir)? {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                report.errors.push((path, e.to_string()));
                continue;
            }
        };
        let mut conf = WgConfig::parse(&text);
        if !conf.has_allowed_ips() {
            report
                .files
                .push((path, ConfResult::NoAllowedIps { removed: 0 }));
            continue;
        }

        let before = addresses(&conf);
        let kept: Vec<String> = before
            .iter()
            .filter(|e| !removals.matches(e))
            .cloned()
            .collect();
        let removed = before.len() - kept.len();

        if removed == 0 {
            report
                .files
                .push((path, ConfResult::Unchanged { removed: 0 }));
            continue;
        }
        if kept.is_empty() {
            // The whole line would go empty. That is a config with no routes
            // in it, not a config with fewer, and it is not what "remove this
            // group" means. Leave the file and let the caller decide.
            report.files.push((path, ConfResult::Emptied { removed }));
            continue;
        }

        conf.set_allowed_ips(&kept.join(", "));
        let after = conf.to_string();
        match write_with_backup(&path, &after) {
            Ok(backup) => report
                .files
                .push((path, ConfResult::Written { removed, backup })),
            Err(e) => report.errors.push((path, e.to_string())),
        }
    }

    Ok(report)
}

/// Read one file's addresses, or `None` when it has no `AllowedIPs` line.
/// The GUI uses this to show what a removal *would* do before doing it.
pub fn entries_in(path: &Path) -> std::io::Result<Option<Vec<String>>> {
    let text = std::fs::read_to_string(path)?;
    let conf = WgConfig::parse(&text);
    Ok(conf.has_allowed_ips().then(|| addresses(&conf)))
}

/// The addresses in a file, one per entry.
///
/// `WgConfig::allowed_ips` returns one *value* per line — the whole
/// `10.0.0.1/32, 10.0.0.2/32, 10.0.0.3/32` as a single string — because that
/// is the shape a rewrite replaces. Removal works per address, so this splits
/// on the comma and drops the empties: `AllowedIPs = a,,b` has two routes in
/// it, not three.
fn addresses(conf: &WgConfig) -> Vec<String> {
    conf.allowed_ips()
        .iter()
        .flat_map(|value| value.split(','))
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// How many entries of `removals` are present in a folder, and in how many
/// files — the numbers a confirmation dialog needs.
pub fn survey(dir: &Path, removals: &Removals) -> Result<(usize, usize)> {
    let mut entries = 0;
    let mut files = 0;
    for path in crate::apply::conf_files(dir)? {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let conf = WgConfig::parse(&text);
        let hits = addresses(&conf)
            .iter()
            .filter(|e| removals.matches(e))
            .count();
        if hits > 0 {
            entries += hits;
            files += 1;
        }
    }
    Ok((entries, files))
}

/// Where a backup of `path` would be written, for reporting.
pub fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("conf.bak")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::HostStore;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wireutils_subtract_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn conf(ips: &str) -> String {
        format!(
            "[Interface]
PrivateKey = secret
Address = 10.9.0.2/32

[Peer]
PublicKey = abc
AllowedIPs = {ips}
Endpoint = vpn.example:51820
"
        )
    }

    #[test]
    fn one_address_leaves_the_others_on_the_line() {
        let dir = tmpdir("partial");
        let path = dir.join("a.conf");
        std::fs::write(&path, conf("10.0.0.1/32, 10.0.0.2/32, 10.0.0.3/32")).unwrap();

        let r = subtract(&dir, &Removals::new(["10.0.0.2/32"])).unwrap();
        assert_eq!(r.written(), 1);
        assert_eq!(r.removed(), 1);

        // `allowed_ips()` returns one value per line, so the survivors come
        // back as the single rewritten value — which is the point: the line
        // keeps the two addresses that were not named and loses the one that
        // was.
        let after = WgConfig::parse(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(
            after.allowed_ips(),
            vec!["10.0.0.1/32, 10.0.0.3/32".to_string()]
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("PrivateKey = secret"),
            "the rest of the file is untouched"
        );
        assert!(text.contains("Endpoint = vpn.example:51820"));
    }

    #[test]
    fn a_prefix_of_the_same_length_is_the_same_route() {
        let dir = tmpdir("bare");
        let path = dir.join("a.conf");
        std::fs::write(&path, conf("10.0.0.5, 10.0.0.6/32")).unwrap();

        let r = subtract(&dir, &Removals::new(["10.0.0.5/32"])).unwrap();
        assert_eq!(r.removed(), 1, "the file says 10.0.0.5, the list says /32");
        let after = WgConfig::parse(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(after.allowed_ips(), vec!["10.0.0.6/32".to_string()]);
    }

    #[test]
    fn a_file_with_nothing_to_remove_is_not_written() {
        let dir = tmpdir("untouched");
        let path = dir.join("a.conf");
        let before = conf("10.0.0.1/32");
        std::fs::write(&path, &before).unwrap();

        let r = subtract(&dir, &Removals::new(["192.168.1.1/32"])).unwrap();
        assert_eq!(r.written(), 0);
        assert_eq!(r.unchanged(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(
            !dir.join("a.conf.bak").exists(),
            "no backup for a file we did not touch"
        );
    }

    #[test]
    fn a_file_whose_every_entry_is_removed_is_left_alone() {
        let dir = tmpdir("emptied");
        let path = dir.join("a.conf");
        let before = conf("10.0.0.1/32");
        std::fs::write(&path, &before).unwrap();

        let r = subtract(&dir, &Removals::new(["10.0.0.1/32"])).unwrap();
        assert_eq!(r.written(), 0, "an empty AllowedIPs routes nothing");
        assert_eq!(r.emptied(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn the_first_removal_keeps_a_backup_and_a_second_does_not_overwrite_it() {
        let dir = tmpdir("backup");
        let path = dir.join("a.conf");
        std::fs::write(&path, conf("10.0.0.1/32, 10.0.0.2/32")).unwrap();

        subtract(&dir, &Removals::new(["10.0.0.1/32"])).unwrap();
        let bak = dir.join("a.conf.bak");
        assert!(bak.exists());
        let first = std::fs::read_to_string(&bak).unwrap();
        assert!(
            first.contains("10.0.0.1/32"),
            "the backup holds the original"
        );

        subtract(&dir, &Removals::new(["10.0.0.2/32"])).unwrap();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), first);
    }

    #[test]
    fn a_folder_without_allowed_ips_is_reported_not_edited() {
        let dir = tmpdir("no_ips");
        let path = dir.join("a.conf");
        std::fs::write(
            &path,
            "[Interface]
PrivateKey = x
",
        )
        .unwrap();

        let r = subtract(&dir, &Removals::new(["10.0.0.1/32"])).unwrap();
        assert_eq!(r.files.len(), 1);
        assert!(matches!(r.files[0].1, ConfResult::NoAllowedIps { .. }));
        assert!(!dir.join("a.conf.bak").exists());
    }

    #[test]
    fn an_empty_selection_is_refused_rather_than_deleting_nothing_quietly() {
        let dir = tmpdir("empty_selection");
        std::fs::write(dir.join("a.conf"), conf("10.0.0.1/32")).unwrap();
        assert!(subtract(&dir, &Removals::new(Vec::<String>::new())).is_err());
    }

    #[test]
    fn a_group_contributes_every_address_its_hosts_would_write() {
        let mut store = HostStore::default();
        store.add_host("example.com", &["github".to_string()]);
        {
            let h = store
                .hosts
                .iter_mut()
                .find(|h| h.target == "example.com")
                .unwrap();
            h.ips = vec!["10.1.1.1".into(), "10.1.1.2".into()];
        }
        store.add_host("10.2.2.2", &["github".to_string()]);
        store.add_host("10.3.3.3", &["other".to_string()]);

        let entries = entries_from_group(&store, "github");
        let r = Removals::new(entries);
        // `allowed_entries` appends the prefix the owner did not type — `/32`,
        // or `/128` for a v6 address — and matching strips it, so a config
        // that spells a route differently still matches the set.
        assert!(r.matches("10.1.1.1/32"));
        assert!(r.matches("10.1.1.2/32"));
        assert!(
            r.matches("10.1.1.1"),
            "a prefix in the config is not required"
        );
        // `10.2.2.2` was added with `add_host`, which deliberately does not
        // seed the target into `ips`: a literal IP is only a route once
        // something put it there, and nothing has. So the group has the
        // domain's two addresses and no more — and removing the group must
        // not touch an address it never contributed.
        assert!(
            !r.matches("10.2.2.2"),
            "an unresolved literal is not in the list"
        );
        assert!(!r.matches("10.3.3.3/32"), "another group's address stays");
        assert!(!r.matches("10.3.3.3"));
    }

    #[test]
    fn the_survey_counts_what_a_removal_would_touch() {
        let dir = tmpdir("survey");
        std::fs::write(dir.join("a.conf"), conf("10.0.0.1/32, 10.0.0.2/32")).unwrap();
        std::fs::write(dir.join("b.conf"), conf("10.0.0.2/32")).unwrap();

        let (entries, files) = survey(&dir, &Removals::new(["10.0.0.2/32"])).unwrap();
        assert_eq!((entries, files), (2, 2));
    }
}
