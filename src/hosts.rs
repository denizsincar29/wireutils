//! The host list: what the owner edits, and what turns into `AllowedIPs`.
//!
//! One `hosts.json` holds the folder of `.conf` files and every host the
//! owner keeps. Each host is either a domain or a literal IP, optionally
//! inside groups (a group is how "chatgpt" gets one row in the list instead
//! of four subdomains). At apply time the list is flattened: every host
//! contributes its IPs, and the result is written into every `.conf`.

use serde::{Deserialize, Serialize};

/// The rule this file enforces: an address reaches `ips` only through
/// `resolve` (a real DNS answer) or through `add_address` (a config that
/// listed it). A *label* — the CIDR an import uses to name a group — is
/// never a route, however much it looks like one.

/// Where the list came from, for the owner's benefit — a host he typed is
/// not the same as one poured in from a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    Manual,
    Template,
}

/// A single host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// What the owner typed: a domain (`api.openai.com`) or an IP.
    pub target: String,
    /// Which groups it belongs to; empty means it stands alone.
    #[serde(default)]
    pub groups: Vec<String>,
    /// IPs resolved from `target`, cached so an offline run still works.
    /// A host whose target is already an IP keeps it here too, so the list
    /// has exactly one place to read addresses from.
    #[serde(default)]
    pub ips: Vec<String>,
    #[serde(default)]
    pub source: Source,
    /// Set when the last resolve attempt failed; the address is shown in the
    /// UI so an unresolvable domain is visible rather than silently missing
    /// from the tunnel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// True when `target` is a literal address or a network in CIDR form —
/// something that needs no DNS and can go into `AllowedIPs` as written.
///
/// CIDR counts because the owner may want a whole subnet rather than a host;
/// treating `10.0.0.0/8` as a domain would send it to the resolver and come
/// back empty.
pub fn is_address(target: &str) -> bool {
    if target.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    match target.split_once('/') {
        Some((ip, prefix)) => match ip.parse::<std::net::IpAddr>() {
            // A v4 prefix is at most 32, a v6 prefix at most 128. Writing
            // `1.2.3.4/99` into a config would produce a tunnel that takes
            // the file but routes nothing, so it is not accepted here.
            Ok(std::net::IpAddr::V4(_)) => prefix.parse::<u8>().map(|n| n <= 32).unwrap_or(false),
            Ok(std::net::IpAddr::V6(_)) => prefix.parse::<u8>().map(|n| n <= 128).unwrap_or(false),
            Err(_) => false,
        },
        None => false,
    }
}

impl Host {
    pub fn new(target: impl Into<String>) -> Self {
        let target = target.into();
        let is_ip = is_address(&target);
        Host {
            ips: if is_ip {
                vec![target.clone()]
            } else {
                Vec::new()
            },
            target,
            groups: Vec::new(),
            source: Source::Manual,
            error: None,
        }
    }

    /// True when the target is an address or network and needs no resolution.
    pub fn is_literal_ip(&self) -> bool {
        is_address(&self.target)
    }

    /// What the table shows: the domain when we have one, the IP otherwise.
    pub fn label(&self) -> &str {
        if self.target.is_empty() {
            self.ips.first().map(String::as_str).unwrap_or("")
        } else {
            &self.target
        }
    }

    /// The address to write for this host, or `None` when nothing is known.
    /// `/32` and `/128` are appended only when the owner did not already
    /// give a prefix — a plain `1.2.3.4` in a config is host-routing, which
    /// is what the owner wants from a per-site host list.
    pub fn allowed_entries(&self) -> Vec<String> {
        self.ips
            .iter()
            .map(|ip| {
                if ip.contains('/') {
                    ip.clone()
                } else if ip.contains(':') {
                    format!("{ip}/128")
                } else {
                    format!("{ip}/32")
                }
            })
            .collect()
    }
}

/// The whole `hosts.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStore {
    #[serde(default = "schema_version")]
    pub version: u32,
    /// The folder of `.conf` files, chosen on first run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conf_dir: Option<String>,
    /// A config the owner already trusts, used as the source for the import
    /// button. Remembered so the second import does not ask again — the file
    /// changes rarely and hunting for it twice is the kind of friction that
    /// stops a feature being used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_conf: Option<String>,
    /// Known groups, kept even when empty so a group created by a template
    /// does not vanish when its last host is deleted.
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<Host>,
    /// When the template catalog was last pulled, RFC3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub templates_updated: Option<String>,
}

fn schema_version() -> u32 {
    1
}

/// Errors the store can raise. Kept in one place so the CLI and the GUI show
/// the same words for the same problem.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// A host or group name did not match anything.
    NotFound(String),
    /// Something the owner asked for contradicts the data on disk.
    Invalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Json(e) => write!(f, "hosts.json is corrupt: {e}"),
            Error::NotFound(s) => write!(f, "not found: {s}"),
            Error::Invalid(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

impl Default for HostStore {
    fn default() -> Self {
        HostStore {
            version: schema_version(),
            conf_dir: None,
            base_conf: None,
            groups: Vec::new(),
            hosts: Vec::new(),
            templates_updated: None,
        }
    }
}

impl HostStore {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let store: HostStore = serde_json::from_str(&text)?;
        Ok(store)
    }

    /// Write the store. Written via a temporary file and a rename so a crash
    /// mid-write cannot leave a truncated `hosts.json` — it is the only copy
    /// of the owner's host list.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Add a host by domain or IP. A target already in the list is updated
    /// (gaining the new group) rather than duplicated — the same subnet twice
    /// in `AllowedIPs` is noise the owner would have to clean by hand.
    ///
    /// A host with a *name* is created with no addresses: the name is the
    /// thing the owner typed, and its addresses come from resolution. Only a
    /// target that is itself an address carries one. Writing a name into
    /// `ips` would put a bare domain into `AllowedIPs`, which the tunnel
    /// cannot route.
    ///
    /// Use [`HostStore::add_address`] when the addresses are already known
    /// and must not be thrown away — the import path does exactly that.
    pub fn add_host(&mut self, target: &str, groups: &[String]) -> &mut Host {
        let target = target.trim().to_string();
        let address = is_address(&target);
        for g in groups {
            self.ensure_group(g);
        }
        if let Some(i) = self.hosts.iter().position(|h| h.target == target) {
            for g in groups {
                if !self.hosts[i].groups.contains(g) {
                    self.hosts[i].groups.push(g.clone());
                }
            }
            // A CIDR typed by the owner is itself the address wanted; a
            // domain can only be resolved. The seed goes when the addresses
            // do, so a label never outlives the reason it was there.
            if address {
                if !self.hosts[i].error.take().is_some() {
                    self.hosts[i].ips.retain(|ip| ip != &target);
                }
            }
            return &mut self.hosts[i];
        }
        let mut host = Host::new("");
        host.target = target;
        // Note the deliberate omission: the target is *not* seeded into
        // `ips` even when it is a CIDR. Seeding it here is how a subnet that
        // exists only as a label — the `31.13.64.0/24` an import gives a
        // group — turns into a route covering 256 addresses the owner's
        // config never listed. An address reaches `ips` through `resolve`
        // (for a domain) or `add_address` (for a file's own list), and both
        // of those mean it.
        host.groups = groups.to_vec();
        self.hosts.push(host);
        self.hosts.last_mut().unwrap()
    }

    /// Add a host together with the addresses that belong to it.
    ///
    /// This is the door for data that arrives from somewhere else — an
    /// imported config, a template — where the addresses are the substance
    /// and the name is only a label for them. The name is *never* copied into
    /// `ips`: an import that turned its `31.13.64.0/24` label into a route
    /// would hand the tunnel 256 addresses the owner's config never listed.
    ///
    /// An existing host of the same name absorbs the addresses rather than
    /// being duplicated, and keeps the groups it already had.
    pub fn add_address(&mut self, target: &str, ips: &[String], groups: &[String]) -> &mut Host {
        for g in groups {
            self.ensure_group(g);
        }
        let existing = self.hosts.iter().position(|h| h.target == target);
        let i = match existing {
            Some(i) => i,
            None => {
                let mut host = Host::new(target);
                // `Host::new` seeds a CIDR target into `ips` — right for a
                // network the owner typed, wrong for a label that names a
                // group. The distinction belongs to the caller, and this
                // method's contract is that addresses arrive in `ips`; so the
                // seed goes and the file's own list is what the host keeps.
                let _ = host.ips.pop();
                host.source = Source::Template;
                host.groups = groups.to_vec();
                self.hosts.push(host);
                self.hosts.len() - 1
            }
        };
        // The same argument applies to a host that was already in the list.
        // A store written before the rule above existed may still carry the
        // label as an address — and that is the store that gets opened next,
        // so the correction has to happen here rather than at import time.
        // The label is a name for the group; a route comes only from the
        // addresses a config actually listed.
        //
        // The label goes even when the caller brings nothing to replace it.
        // The earlier version kept it in that case, on the reasoning that a
        // typed network must survive — and the legacy shape is precisely a
        // store whose *only* entry under that target is the label, so the
        // test written for the rule passed while the rule did nothing. A
        // target that is an address loses its seed here, always; a network
        // the owner wants routed comes back through `resolve`.
        self.hosts[i].ips.retain(|ip| ip != target);
        for g in groups {
            if !self.hosts[i].groups.contains(g) {
                self.hosts[i].groups.push(g.clone());
            }
        }
        for ip in ips {
            if !self.hosts[i].ips.contains(ip) {
                self.hosts[i].ips.push(ip.clone());
            }
        }
        self.hosts[i].error = None;
        &mut self.hosts[i]
    }

    /// Remove a host by its target.
    pub fn remove_host(&mut self, target: &str) -> Result<Host> {
        let i = self
            .hosts
            .iter()
            .position(|h| h.target == target)
            .ok_or_else(|| Error::NotFound(target.to_string()))?;
        Ok(self.hosts.remove(i))
    }

    /// Rename a host's target, refusing a name already taken.
    pub fn rename_host(&mut self, from: &str, to: &str) -> Result<()> {
        let to = to.trim().to_string();
        if self.hosts.iter().any(|h| h.target == to) {
            return Err(Error::Invalid(format!("{to} is already in the list")));
        }
        let i = self
            .hosts
            .iter()
            .position(|h| h.target == from)
            .ok_or_else(|| Error::NotFound(from.to_string()))?;
        if self.hosts[i].is_literal_ip() != is_address(&to) {
            // Domain became an IP or the other way round: the cached
            // addresses belong to the old target and must be re-resolved.
            self.hosts[i].ips.clear();
        }
        self.hosts[i].target = to;
        self.hosts[i].error = None;
        Ok(())
    }

    pub fn ensure_group(&mut self, group: &str) {
        if !self.groups.iter().any(|g| g == group) {
            self.groups.push(group.to_string());
        }
    }

    /// Remove a group. Its hosts stay in the list, just ungrouped — deleting
    /// a group must never delete addresses the owner wanted in the tunnel.
    pub fn remove_group(&mut self, group: &str) -> Result<usize> {
        let before = self.groups.len();
        self.groups.retain(|g| g != group);
        if self.groups.len() == before {
            return Err(Error::NotFound(format!("group {group}")));
        }
        let mut touched = 0;
        for h in &mut self.hosts {
            if h.groups.iter().any(|g| g == group) {
                h.groups.retain(|g| g != group);
                touched += 1;
            }
        }
        Ok(touched)
    }

    /// Hosts in a group, in list order.
    pub fn hosts_in_group<'a>(&'a self, group: &'a str) -> impl Iterator<Item = &'a Host> {
        self.hosts
            .iter()
            .filter(move |h| h.groups.iter().any(|g| g == group))
    }

    /// The value for `AllowedIPs`: every address of every host, deduplicated,
    /// in list order. Literal IPs keep their `/32`; domains contribute all
    /// resolved addresses, because a CDN host answers on several and the
    /// tunnel has to cover whichever one is up.
    pub fn allowed_ips_value(&self) -> String {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for host in &self.hosts {
            for entry in host.allowed_entries() {
                if seen.insert(entry.clone()) {
                    out.push(entry);
                }
            }
        }
        out.join(", ")
    }

    /// Hosts whose target is a domain with no cached address — the caller
    /// must resolve them before applying, or those addresses never reach the
    /// config. Surfaced rather than skipped precisely because a missing host
    /// fails silently inside the tunnel.
    pub fn unresolved(&self) -> Vec<&Host> {
        self.hosts
            .iter()
            .filter(|h| !h.is_literal_ip() && h.ips.is_empty())
            .collect()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn a_literal_ip_is_its_own_address() {
        let h = Host::new("1.2.3.4");
        assert_eq!(h.ips, vec!["1.2.3.4"]);
        assert_eq!(h.allowed_entries(), vec!["1.2.3.4/32"]);
        assert!(h.is_literal_ip());
    }

    #[test]
    fn an_ipv6_literal_gets_a_128() {
        let h = Host::new("2a00:1450:4001::1");
        assert_eq!(h.allowed_entries(), vec!["2a00:1450:4001::1/128"]);
    }

    #[test]
    fn an_explicit_prefix_is_left_alone() {
        let h = Host::new("10.0.0.0/8");
        assert_eq!(h.allowed_entries(), vec!["10.0.0.0/8"]);
        assert!(h.is_literal_ip());
        assert_eq!(h.ips, vec!["10.0.0.0/8"]);
    }

    #[test]
    fn a_cidr_with_an_absurd_prefix_is_treated_as_a_domain() {
        // `1.2.3.4/99` is not a network; it must not be written as one.
        assert!(!is_address("1.2.3.4/99"));
        assert!(!is_address("1.2.3.4/"));
        assert!(!is_address("example.com/32"));
        assert!(is_address("2001:db8::/32"));
        assert!(is_address("10.0.0.0/8"));
    }

    #[test]
    fn a_domain_is_shown_by_name_and_resolved_later() {
        let h = Host::new("api.openai.com");
        assert_eq!(h.label(), "api.openai.com");
        assert!(!h.is_literal_ip());
        assert!(h.ips.is_empty());
        assert!(h.allowed_entries().is_empty());
    }

    #[test]
    fn adding_the_same_target_twice_updates_instead_of_duplicating() {
        let mut s = HostStore::default();
        s.add_host("api.openai.com", &["chatgpt".into()]);
        s.add_host("api.openai.com", &["openai".into()]);
        assert_eq!(s.hosts.len(), 1);
        assert_eq!(s.hosts[0].groups, vec!["chatgpt", "openai"]);
        assert_eq!(s.groups, vec!["chatgpt", "openai"]);
    }

    #[test]
    fn the_value_deduplicates_and_keeps_list_order() {
        let mut s = HostStore::default();
        s.add_host("1.2.3.4", &[]);
        s.add_host("5.6.7.8", &[]);
        s.add_host("1.2.3.4", &["g".into()]);
        // A typed address is not in `ips` until it is resolved, so the
        // resolve step is what the value reads from.
        for i in 0..s.hosts.len() {
            s.hosts[i].ips = vec![s.hosts[i].target.clone()];
        }
        assert_eq!(s.allowed_ips_value(), "1.2.3.4/32, 5.6.7.8/32");
    }

    #[test]
    fn removing_a_group_keeps_its_hosts() {
        let mut s = HostStore::default();
        s.add_host("api.openai.com", &["chatgpt".into()]);
        s.add_host("chat.openai.com", &["chatgpt".into()]);
        assert_eq!(s.remove_group("chatgpt").unwrap(), 2);
        assert_eq!(s.hosts.len(), 2);
        assert!(s.hosts.iter().all(|h| h.groups.is_empty()));
        assert!(s.remove_group("chatgpt").is_err());
    }

    #[test]
    fn renaming_across_a_kind_clears_stale_addresses() {
        let mut s = HostStore::default();
        s.add_host("api.openai.com", &[]);
        s.hosts[0].ips = vec!["1.2.3.4".into()];
        s.rename_host("api.openai.com", "1.1.1.1").unwrap();
        assert!(s.hosts[0].ips.is_empty());
    }

    #[test]
    fn renaming_to_a_taken_name_is_refused() {
        let mut s = HostStore::default();
        s.add_host("a.example.com", &[]);
        s.add_host("b.example.com", &[]);
        assert!(matches!(
            s.rename_host("a.example.com", "b.example.com"),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn a_domain_without_addresses_is_reported_as_unresolved() {
        let mut s = HostStore::default();
        s.add_host("a.example.com", &[]);
        s.add_host("1.2.3.4", &[]);
        let u = s.unresolved();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].target, "a.example.com");
    }

    #[test]
    fn a_host_that_failed_keeps_its_reason_in_the_file() {
        let mut s = HostStore::default();
        s.add_host("nope.invalid", &[]);
        s.hosts[0].error = Some("no such host".into());
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"error\":\"no such host\""));
        let back: HostStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hosts[0].error.as_deref(), Some("no such host"));
    }

    #[test]
    fn a_store_round_trips_through_json() {
        let mut s = HostStore::default();
        s.conf_dir = Some("/home/deniz/wg".into());
        s.add_host("1.2.3.4", &["vpn".into()]);
        let text = serde_json::to_string_pretty(&s).unwrap();
        let back: HostStore = serde_json::from_str(&text).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn an_older_file_without_the_optional_fields_still_loads() {
        let json = r#"{"hosts":[{"target":"1.2.3.4"}]}"#;
        let s: HostStore = serde_json::from_str(json).unwrap();
        assert_eq!(s.version, 1);
        assert_eq!(s.hosts[0].target, "1.2.3.4");
        assert_eq!(s.hosts[0].ips, Vec::<String>::new());
    }
}
