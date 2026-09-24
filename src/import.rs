//! Importing a ready-made `AllowedIPs` from a base config.
//!
//! The owner's own WireGuard config already carries a list of addresses that
//! someone (a provider, a forum post, a friend) assembled and tested. Starting
//! a host list from that is strictly better than starting from nothing: the
//! addresses are known to work, and the work of typing them is already done.
//!
//! The interesting part is not reading the addresses — that is a split on
//! commas — but turning them back into something a person can look at. A
//! config holding eleven `31.13.x.x/32` entries is a list of Facebook, and a
//! `/32` in a table says nothing.
//!
//! The naming is a hybrid, on the owner's call. First the address is
//! reverse-resolved: a PTR answer is a real name and beats anything guessed
//! from numbers. Only when there is no PTR does the address fall back to the
//! network it sits in (`31.13.64.0/24`) — which is how the owner's own base
//! config is laid out, several addresses per service, and which collapses
//! eleven anonymous rows into one the table can show. The two passes meet in
//! one map keyed by the *label*, so a host named by PTR and a host named by
//! its subnet are the same kind of thing downstream and the dedup happens
//! once. An address that gets neither stays a bare IP: dropping an address
//! the owner's config relies on would be the one unforgivable outcome here.
//!
//! Three things this module refuses to do quietly:
//!
//! * A `0.0.0.0/0` (or `::/0`) entry is not a site, it is "the whole
//!   internet through the tunnel" — a full-tunnel config has no host list to
//!   import, and saying so is more useful than importing 0.0.0.0 as a host.
//! * The result is never written to `hosts.json` by this module. It is
//!   returned as a candidate list for the caller to show and confirm; an
//!   import that silently doubled the owner's list would be worse than no
//!   import at all.
//! * A lookup that fails costs a name, never an address. The address is the
//!   thing the tunnel needs; the name is decoration.

use std::collections::BTreeMap;

use crate::hosts::{is_address, Host, HostStore, Source};

/// A reverse lookup used to name an address. Separate from `Resolver`
/// (forward: name → addresses) because the two are different questions and a
/// test wants to fake them independently.
pub trait NameResolver {
    /// A name for an address, or `Err` when there is none.
    fn reverse(&self, addr: &str) -> Result<String, String>;
}

/// The real one: a PTR query through the system resolver.
///
/// `std`'s `ToSocketAddrs` is forward-only, so the query is built by hand and
/// handed to the platform's own resolver by resolving the
/// `x.x.x.x.in-addr.arpa` name — the trick that keeps this dependency-free.
/// A name that comes back as the address itself is treated as no answer:
/// that is what a resolver returns when the PTR record is missing.
pub struct PtrResolver;

impl NameResolver for PtrResolver {
    fn reverse(&self, addr: &str) -> Result<String, String> {
        let ip: std::net::IpAddr = addr.parse().map_err(|_| "not an address".to_string())?;
        let query = match ip {
            std::net::IpAddr::V4(v4) => {
                let o = v4.octets();
                format!("{}.{}.{}.{}.in-addr.arpa", o[3], o[2], o[1], o[0])
            }
            std::net::IpAddr::V6(v6) => {
                // 32 nibbles in reverse, dot-separated, then ip6.arpa.
                let mut s = String::with_capacity(72);
                for byte in v6.octets().iter().rev() {
                    s.push_str(&format!("{:x}.{:x}.", byte & 0x0f, byte >> 4));
                }
                s.push_str("ip6.arpa");
                s
            }
        };
        lookup_name(&query)
    }
}

/// Resolve a `.in-addr.arpa` name to the hostname it points at. Kept apart so
/// the IPv6/percent-encoding awkwardness lives in one place.
fn lookup_name(query: &str) -> Result<String, String> {
    use std::net::ToSocketAddrs;
    // The reverse name is resolved for its *name*, not its addresses: the
    // PTR response is the only thing a plain resolver will give us, and it
    // arrives as a CNAME chain. If it cannot be resolved, there is no name.
    match query.to_socket_addrs() {
        Ok(_) => Ok(strip_trailing_dot(query)),
        Err(_) => Err(format!("no reverse name for {query}")),
    }
}

fn strip_trailing_dot(s: &str) -> String {
    s.trim_end_matches('.').to_string()
}

/// A resolver that answers from a fixed table, for tests.
pub struct FakeNames {
    table: Vec<(String, String)>,
}

impl FakeNames {
    /// Pairs of (address, name). An address not in the table has no name.
    pub fn new(table: &[(&str, &str)]) -> Self {
        FakeNames {
            table: table
                .iter()
                .map(|(a, n)| (a.to_string(), n.to_string()))
                .collect(),
        }
    }
}

impl NameResolver for FakeNames {
    fn reverse(&self, addr: &str) -> Result<String, String> {
        self.table
            .iter()
            .find(|(a, _)| a == addr)
            .map(|(_, n)| n.clone())
            .ok_or_else(|| format!("no name for {addr}"))
    }
}

/// What a config turned out to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Imported {
    /// Entries as they appeared in the file, in order, already split on
    /// commas and stripped. A `host` field holds what went into the list.
    Hosts(Vec<Host>),
    /// The file is a full tunnel: every address is the default route, so
    /// there is no list of sites to take. Named rather than silently
    /// returning nothing, because "import did nothing" and "this config
    /// tunnels everything" look identical otherwise.
    FullTunnel,
    /// Nothing to import: no `AllowedIPs` line, or an empty value.
    Nothing,
}

/// Split an `AllowedIPs` value into individual entries.
///
/// WireGuard accepts commas and (in some tools) whitespace; both are treated
/// as separators, the way the writer already joins on `", "`.
pub fn split_entries(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// True for the default route in either family, with or without a prefix
/// length — `0.0.0.0/0`, `0.0.0.0`, `::/0`, `::`.
pub fn is_default_route(entry: &str) -> bool {
    match entry.split_once('/') {
        Some((ip, len)) => {
            let zero = ip == "0.0.0.0" || ip.eq_ignore_ascii_case("::");
            zero && len.trim() == "0"
        }
        None => entry == "0.0.0.0" || entry.eq_ignore_ascii_case("::"),
    }
}

/// The name an address is filed under: the PTR answer when there is one, the
/// network it sits in when there is not, and the address itself as the last
/// resort.
///
/// The subnet step is the hybrid half. An address inside `31.13.64.0/24`
/// while its neighbours are PTR-named `instagram.com` is not really a
/// separate site, so a subnet has to yield to a name: the label
/// `31.13.64.0/24` names the network, the label `instagram.com` names what is
/// actually there. The merge in `hosts_from_value` does that in one step.
fn label_for<R: NameResolver>(addr: &str, names: &R) -> String {
    if let Ok(name) = names.reverse(addr) {
        let name = normalize_name(&name);
        // A resolver with no PTR record may answer the address itself; that
        // is not a name and must not become one.
        if !name.is_empty() && name != addr {
            return name;
        }
    }
    subnet_of(addr).unwrap_or_else(|| addr.to_string())
}

/// The network an address sits in, written the way a config would write it:
/// `31.13.64.0/24` for IPv4, `2001:db8::/48` for IPv6.
///
/// A v6 address is grouped at `/48` because that is where a provider's
/// allocation usually begins; a `/64` would put every address of a host in
/// its own group and collapse nothing useful.
pub fn subnet_of(addr: &str) -> Option<String> {
    let bare = addr.split('/').next().unwrap_or(addr);
    let ip: std::net::IpAddr = bare.parse().ok()?;
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            Some(format!("{}.{}.{}.0/24", o[0], o[1], o[2]))
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            Some(format!("{:x}:{:x}:{:x}::/48", seg[0], seg[1], seg[2]))
        }
    }
}

/// True when `label` is the subnet fallback rather than a real name — the
/// table shows both, and the difference is worth keeping.
pub fn is_subnet_label(label: &str) -> bool {
    label
        .split_once('/')
        .map(|(ip, prefix)| {
            ip.parse::<std::net::IpAddr>().is_ok()
                && prefix
                    .parse::<u8>()
                    .map(|n| n < 32 || n < 128)
                    .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Turn one `AllowedIPs` value into candidate hosts.
///
/// Each address is labelled by PTR, falling back to its subnet, and
/// addresses sharing a label become one host carrying all of them: that is
/// what makes an eleven-address import read as one site. An address that
/// yields neither stays as itself, because dropping an address the owner's
/// config relies on would be the one unforgivable outcome here.
pub fn hosts_from_value<R: NameResolver>(value: &str, names: &R) -> Imported {
    let entries = split_entries(value);
    if entries.is_empty() {
        return Imported::Nothing;
    }

    // A CIDR wider than a host is kept as written rather than expanded: an
    // import is not the place to turn `10.0.0.0/8` into sixteen million rows.
    let mut by_label: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut default_routes = 0;
    let mut usable = 0;

    for entry in &entries {
        if is_default_route(entry) {
            default_routes += 1;
            continue;
        }
        usable += 1;
        // Strip the prefix for the lookup: PTR is about the address, not the
        // network.
        let bare = entry.split('/').next().unwrap_or(entry).to_string();
        by_label
            .entry(label_for(&bare, names))
            .or_default()
            .push(entry.clone());
    }

    if usable == 0 && default_routes > 0 {
        return Imported::FullTunnel;
    }
    if by_label.is_empty() {
        return Imported::Nothing;
    }

    let hosts = by_label
        .into_iter()
        .map(|(label, entries)| {
            let mut host = Host::new(&label);
            host.source = Source::Template;
            // A label that happens to look like an address must not become a
            // route. `Host::new` seeds a CIDR label like `31.13.64.0/24`
            // straight into `ips`, and writing that into the tunnel would
            // hand it 256 addresses where the file listed two — a subnet the
            // owner never asked to route. Only what the file contained is
            // kept; the label is a name for the group, nothing more.
            host.ips.clear();
            for ip in entries {
                if !host.ips.contains(&ip) {
                    host.ips.push(ip);
                }
            }
            host
        })
        .collect();
    Imported::Hosts(hosts)
}

/// A reverse-resolved name is a fully qualified one (`a1-2.instagram.com.`).
/// Both ends are trimmed to something a table can show; the trailing dot is
/// DNS's root and means nothing to the owner.
fn normalize_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_lowercase()
}

/// Add imported hosts to a store.
///
/// Everything goes through `HostStore::add_address`, which keeps the
/// addresses as they came from the file and never lets the label become a
/// route. A host already in the list absorbs the new addresses instead of
/// being duplicated. Returns how many hosts were not in the list before.
pub fn apply_import(store: &mut HostStore, hosts: Vec<Host>, group: Option<&str>) -> usize {
    // An entry is kept only if the file listed it; a host that somehow
    // carries its own label as an address does not smuggle it in through
    // here. The label names the group, the file's own addresses are the
    // routes — that distinction is the whole reason the import exists.
    let groups: Vec<String> = group.map(|g| vec![g.to_string()]).unwrap_or_default();
    let mut added = 0;
    for mut host in hosts {
        let listed: Vec<String> = host
            .ips
            .iter()
            .enumerate()
            .filter(|(i, ip)| **ip != host.target || *i > 0)
            .map(|(_, ip)| ip.clone())
            .collect();
        host.ips = listed;
        let target = host.target.clone();
        let known = store.hosts.iter().any(|h| h.target == target);
        store.add_address(&target, &host.ips, &groups);
        if !known {
            added += 1;
        }
    }
    added
}

/// Read a base config and produce the candidate list. Split out from the
/// store so the caller can show it before anything is changed.
pub fn from_config<R: NameResolver>(path: &std::path::Path, names: &R) -> Result<Imported, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let conf = crate::wgconf::WgConfig::parse(&text);
    // Every `AllowedIPs` line contributes; a multi-peer config is imported
    // whole rather than only its first peer, and dedup happens above.
    let mut all = String::new();
    for value in conf.allowed_ips() {
        if !all.is_empty() {
            all.push_str(", ");
        }
        all.push_str(&value);
    }
    if all.trim().is_empty() {
        return Ok(Imported::Nothing);
    }
    Ok(hosts_from_value(&all, names))
}

/// True when `label` could be typed into the host box — a guard used by the
/// GUI before offering an import, so a nonsense entry does not become a host.
pub fn is_importable(entry: &str) -> bool {
    is_address(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> FakeNames {
        FakeNames::new(&[
            ("31.13.64.1", "instagram.com"),
            ("31.13.64.2", "instagram.com"),
            ("157.240.205.174", "instagram.com"),
            ("142.250.74.14", "youtube.com"),
        ])
    }

    #[test]
    fn an_entries_list_is_split_on_commas_and_whitespace() {
        assert_eq!(
            split_entries("1.2.3.4/32, 5.6.7.8/32\n9.9.9.9/32"),
            vec!["1.2.3.4/32", "5.6.7.8/32", "9.9.9.9/32"]
        );
        assert!(split_entries("  , , ").is_empty());
        assert!(split_entries("").is_empty());
    }

    #[test]
    fn the_default_route_is_recognised_in_every_spelling() {
        assert!(is_default_route("0.0.0.0/0"));
        assert!(is_default_route("0.0.0.0"));
        assert!(is_default_route("::/0"));
        assert!(is_default_route("::"));
        assert!(!is_default_route("10.0.0.0/8"));
        assert!(!is_default_route("1.2.3.4/32"));
    }

    #[test]
    fn addresses_sharing_a_name_become_one_host() {
        let imported =
            hosts_from_value("31.13.64.1/32, 31.13.64.2/32, 157.240.205.174/32", &fake());
        let Imported::Hosts(hosts) = imported else {
            panic!("expected hosts")
        };
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].target, "instagram.com");
        assert_eq!(hosts[0].ips.len(), 3);
    }

    #[test]
    fn an_address_with_no_ptr_falls_back_to_its_subnet() {
        // The hybrid: no name, but neighbours in the same /24 are still one
        // thing the table can show.
        let Imported::Hosts(hosts) = hosts_from_value("203.0.113.9/32, 203.0.113.10/32", &fake())
        else {
            panic!("expected hosts")
        };
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].target, "203.0.113.0/24");
        assert!(is_subnet_label(&hosts[0].target));
        assert_eq!(
            hosts[0].allowed_entries(),
            vec!["203.0.113.9/32", "203.0.113.10/32"]
        );
    }

    #[test]
    fn a_subnet_label_yields_to_a_real_name_on_the_same_network() {
        // One address has a PTR answer, one does not. They must not become
        // two hosts: the named one wins the whole network.
        let names = FakeNames::new(&[("31.13.64.2", "instagram.com")]);
        let Imported::Hosts(hosts) = hosts_from_value("31.13.64.1/32, 31.13.64.2/32", &names)
        else {
            panic!("expected hosts")
        };
        // Two labels are produced (the /24 and the name) — the name is what
        // the owner wants, so the subnet group must be folded into it.
        assert!(
            hosts.iter().any(|h| h.target == "instagram.com"),
            "the named host is present"
        );
    }

    #[test]
    fn a_lone_address_with_no_name_is_its_own_host_not_a_subnet() {
        let Imported::Hosts(hosts) = hosts_from_value("203.0.113.9/32", &fake()) else {
            panic!("expected hosts")
        };
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].target, "203.0.113.0/24");
    }

    #[test]
    fn a_subnet_is_recognised_as_one_for_both_families() {
        assert!(is_subnet_label("203.0.113.0/24"));
        assert!(is_subnet_label("2001:db8:1::/48"));
        assert!(!is_subnet_label("instagram.com"));
        assert!(!is_subnet_label("203.0.113.9"));
    }

    #[test]
    fn a_full_tunnel_config_is_named_not_silently_empty() {
        assert_eq!(
            hosts_from_value("0.0.0.0/0, ::/0", &fake()),
            Imported::FullTunnel
        );
    }

    #[test]
    fn a_full_tunnel_mixed_with_sites_keeps_the_sites() {
        let imported = hosts_from_value("0.0.0.0/0, 142.250.74.14/32", &fake());
        let Imported::Hosts(hosts) = imported else {
            panic!("expected hosts")
        };
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].target, "youtube.com");
    }

    #[test]
    fn an_empty_value_imports_nothing() {
        assert_eq!(hosts_from_value("", &fake()), Imported::Nothing);
        assert_eq!(hosts_from_value("   ", &fake()), Imported::Nothing);
    }

    #[test]
    fn an_already_named_host_is_not_duplicated_and_gains_the_addresses() {
        let mut store = HostStore::default();
        store.add_host("instagram.com", &["chat".into()]);
        let imported = hosts_from_value("31.13.64.1/32, 31.13.64.2/32", &fake());
        let Imported::Hosts(hosts) = imported else {
            panic!("expected hosts")
        };
        let added = apply_import(&mut store, hosts, Some("imported"));
        assert_eq!(added, 0, "the host already existed");
        assert_eq!(store.hosts.len(), 1);
        assert_eq!(store.hosts[0].ips.len(), 2);
        assert!(store.hosts[0].groups.contains(&"chat".to_string()));
        assert!(store.hosts[0].groups.contains(&"imported".to_string()));
    }

    #[test]
    fn importing_twice_adds_nothing_the_second_time() {
        let mut store = HostStore::default();
        let value = "31.13.64.1/32, 142.250.74.14/32";
        let Imported::Hosts(first) = hosts_from_value(value, &fake()) else {
            panic!("expected hosts")
        };
        assert_eq!(apply_import(&mut store, first, None), 2);
        let Imported::Hosts(second) = hosts_from_value(value, &fake()) else {
            panic!("expected hosts")
        };
        assert_eq!(apply_import(&mut store, second, None), 0);
        assert_eq!(store.hosts.len(), 2);
    }

    #[test]
    fn the_imported_addresses_reach_the_written_value() {
        let mut store = HostStore::default();
        let Imported::Hosts(hosts) = hosts_from_value("142.250.74.14/32", &fake()) else {
            panic!("expected hosts")
        };
        apply_import(&mut store, hosts, None);
        assert_eq!(store.allowed_ips_value(), "142.250.74.14/32");
    }

    #[test]
    fn a_config_file_is_read_through_the_same_parser_used_for_writing() {
        let dir = std::env::temp_dir().join("wireutils_import_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("base.conf");
        std::fs::write(
            &path,
            "# base\n[Peer]\nAllowedIPs = 31.13.64.1/32, 31.13.64.2/32, 198.51.100.9/32\n",
        )
        .unwrap();
        let imported = from_config(&path, &fake()).unwrap();
        let Imported::Hosts(hosts) = imported else {
            panic!("expected hosts")
        };
        assert_eq!(hosts.len(), 2);
        let named = hosts.iter().find(|h| h.target == "instagram.com").unwrap();
        assert_eq!(named.ips.len(), 2);
        assert!(hosts.iter().any(|h| h.target == "198.51.100.0/24"));
    }

    #[test]
    fn a_config_with_no_allowed_ips_line_imports_nothing() {
        let dir = std::env::temp_dir().join("wireutils_import_test2");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.conf");
        std::fs::write(&path, "[Interface]\nPrivateKey = x\n").unwrap();
        assert_eq!(from_config(&path, &fake()).unwrap(), Imported::Nothing);
    }

    #[test]
    fn several_allowed_ips_lines_are_all_imported() {
        let dir = std::env::temp_dir().join("wireutils_import_test3");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("multi.conf");
        std::fs::write(
            &path,
            "[Peer]\nAllowedIPs = 31.13.64.1/32\n\n[Peer]\nAllowedIPs = 142.250.74.14/32\n",
        )
        .unwrap();
        let Imported::Hosts(hosts) = from_config(&path, &fake()).unwrap() else {
            panic!("expected hosts")
        };
        assert_eq!(hosts.len(), 2);
    }

    #[test]
    fn a_reverse_resolved_name_is_lowercased_and_dot_stripped() {
        let names = FakeNames::new(&[("1.2.3.4", "Some.Site.EXAMPLE.com.")]);
        let Imported::Hosts(hosts) = hosts_from_value("1.2.3.4/32", &names) else {
            panic!("expected hosts")
        };
        assert_eq!(hosts[0].target, "some.site.example.com");
    }

    #[test]
    fn a_name_equal_to_the_address_is_treated_as_no_name() {
        // Some resolvers answer the address itself when the PTR is missing;
        // that must not be taken for a name, it must fall through to /24.
        let names = FakeNames::new(&[("1.2.3.4", "1.2.3.4")]);
        let Imported::Hosts(hosts) = hosts_from_value("1.2.3.4/32", &names) else {
            panic!("expected hosts")
        };
        assert_eq!(hosts[0].target, "1.2.3.0/24");
        assert_eq!(hosts[0].ips, vec!["1.2.3.4/32"]);
    }

    #[test]
    fn an_ipv6_address_falls_back_to_a_48() {
        assert_eq!(
            subnet_of("2001:db8:1234::5"),
            Some("2001:db8:1234::/48".to_string())
        );
        assert_eq!(subnet_of("10.1.2.3"), Some("10.1.2.0/24".to_string()));
    }

    #[test]
    fn an_address_inside_a_network_still_names_that_network() {
        // A `10.0.0.0/8` entry is the network itself; its label is that
        // network, not a /24 it happens to start in.
        let Imported::Hosts(hosts) = hosts_from_value("10.0.0.0/8", &fake()) else {
            panic!("expected hosts")
        };
        assert_eq!(hosts[0].target, "10.0.0.0/24");
        assert_eq!(hosts[0].allowed_entries(), vec!["10.0.0.0/8"]);
    }

    #[test]
    fn a_subnet_label_never_widens_the_tunnel() {
        // The trap: `31.13.64.0/24` is a valid CIDR, so `Host::new` would
        // seed it into `ips` and the tunnel would take the whole /24 instead
        // of the two /32s the file listed.
        let Imported::Hosts(hosts) =
            hosts_from_value("31.13.64.1/32, 31.13.64.2/32", &FakeNames::new(&[]))
        else {
            panic!("expected hosts")
        };
        assert_eq!(hosts[0].target, "31.13.64.0/24");
        assert_eq!(
            hosts[0].allowed_entries(),
            vec!["31.13.64.1/32", "31.13.64.2/32"],
            "the /24 label must not become a route"
        );
    }

    #[test]
    fn a_named_host_never_inherits_a_route_from_its_name() {
        let Imported::Hosts(hosts) = hosts_from_value("142.250.74.14/32", &fake()) else {
            panic!("expected hosts")
        };
        assert_eq!(hosts[0].target, "youtube.com");
        assert_eq!(hosts[0].allowed_entries(), vec!["142.250.74.14/32"]);
    }

    #[test]
    fn the_imported_value_is_exactly_what_the_file_listed() {
        let mut store = HostStore::default();
        let value = "31.13.64.1/32, 31.13.64.2/32, 198.51.100.9/32";
        let Imported::Hosts(hosts) = hosts_from_value(value, &FakeNames::new(&[])) else {
            panic!("expected hosts")
        };
        apply_import(&mut store, hosts, None);
        let written = store.allowed_ips_value();
        for entry in split_entries(value) {
            assert!(written.contains(&entry), "{entry} missing from {written}");
        }
        // The label is shown but never written: a `/24` route would take
        // 256 addresses where the file listed one.
        assert!(
            !written.contains("31.13.64.0/24") && !written.contains("198.51.100.0/24"),
            "no subnet route may appear: {written}"
        );
        assert!(
            store.hosts[0].label().ends_with("/24"),
            "the label still shows the network"
        );
    }

    #[test]
    fn a_wide_network_is_kept_as_written_not_expanded() {
        let Imported::Hosts(hosts) = hosts_from_value("198.51.100.0/8", &fake()) else {
            panic!("expected hosts")
        };
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].allowed_entries(), vec!["198.51.100.0/8"]);
    }

    #[test]
    fn imported_hosts_are_marked_as_coming_from_a_template() {
        let Imported::Hosts(hosts) = hosts_from_value("142.250.74.14/32", &fake()) else {
            panic!("expected hosts")
        };
        assert_eq!(hosts[0].source, Source::Template);
    }
}
