//! Turning a domain into addresses, with the result cached on the host.
//!
//! Resolution happens here rather than in the GUI so it has exactly one
//! implementation, and so it can be tested without a window. Two properties
//! matter more than speed:
//!
//! * A domain that cannot be resolved is reported back, never silently
//!   dropped — a host missing from `AllowedIPs` means traffic that should go
//!   through the tunnel goes out in the clear, and the owner would have no
//!   way to notice.
//! * The cached addresses survive an offline run: the template catalog ships
//!   with addresses baked in, so a first start without internet still
//!   produces a usable config.

use crate::hosts::{Host, HostStore};

/// The resolver the app uses.
pub trait Resolver {
    /// Addresses for a target that is already known not to be a literal IP.
    fn lookup(&self, target: &str) -> Result<Vec<String>, String>;
}

/// The real one: the operating system's resolver, which is what the owner's
/// own `dig`/`nslookup` uses and therefore honours `/etc/hosts` and the
/// system DNS configuration.
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn lookup(&self, target: &str) -> Result<Vec<String>, String> {
        use std::net::ToSocketAddrs;
        let addrs = (target, 0u16)
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .map(|sa| sa.ip().to_string())
            .collect::<Vec<_>>();
        if addrs.is_empty() {
            return Err("no addresses returned".to_string());
        }
        Ok(addrs)
    }
}

/// A resolver that answers from a fixed table, for tests and for offline
/// builds.
pub struct FakeResolver {
    table: Vec<(String, Result<Vec<String>, String>)>,
}

impl FakeResolver {
    pub fn new(table: &[(&str, &[&str])]) -> Self {
        FakeResolver {
            table: table
                .iter()
                .map(|(d, ips)| {
                    (
                        d.to_string(),
                        Ok(ips.iter().map(|s| s.to_string()).collect()),
                    )
                })
                .collect(),
        }
    }

    pub fn failing(target: &str) -> Self {
        FakeResolver {
            table: vec![(target.to_string(), Err("no such host".to_string()))],
        }
    }
}

impl Resolver for FakeResolver {
    fn lookup(&self, target: &str) -> Result<Vec<String>, String> {
        match self.table.iter().find(|(d, _)| d == target) {
            Some((_, r)) => r.clone(),
            None => Err("no such host".to_string()),
        }
    }
}

/// What happened to one host during a resolve pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Its target is already an IP; nothing to do.
    Literal,
    /// Addresses were (re)filled in.
    Resolved(Vec<String>),
    Failed(String),
    /// Skipped because it was resolved recently enough.
    Fresh,
}

/// Resolve every host that needs it, writing the results back and clearing
/// the error of the ones that succeed. Returns one outcome per host, in list
/// order, so the caller can report exactly which lines failed.
///
/// `force` re-resolves even hosts that already have addresses: a CDN's
/// addresses change, and "I added the site last week, why is it blocked" is
/// answered by this flag.
pub fn resolve_all<R: Resolver>(
    store: &mut HostStore,
    resolver: &R,
    force: bool,
) -> Vec<(String, Outcome)> {
    let mut out = Vec::new();
    for host in &mut store.hosts {
        if host.is_literal_ip() {
            out.push((host.target.clone(), Outcome::Literal));
            continue;
        }
        if !force && !host.ips.is_empty() {
            out.push((host.target.clone(), Outcome::Fresh));
            continue;
        }
        match resolver.lookup(&host.target) {
            Ok(ips) => {
                let ips = dedup(ips);
                host.ips = ips.clone();
                host.error = None;
                out.push((host.target.clone(), Outcome::Resolved(ips)));
            }
            Err(e) => {
                // Deliberately keep whatever cached addresses we had: a
                // transient DNS failure must not empty a working config.
                host.error = Some(e.clone());
                out.push((host.target.clone(), Outcome::Failed(e)));
            }
        }
    }
    out
}

fn dedup(ips: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    ips.into_iter()
        .filter(|ip| seen.insert(ip.clone()))
        .collect()
}

/// Cache the addresses a group is known to answer on, so they are available
/// offline. Used when a template carrying baked-in addresses is added.
pub fn seed_addresses(host: &mut Host, ips: &[String]) {
    if host.ips.is_empty() && !host.is_literal_ip() {
        host.ips = ips.to_vec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(targets: &[&str]) -> HostStore {
        let mut s = HostStore::default();
        for t in targets {
            s.add_host(t, &[]);
        }
        s
    }

    #[test]
    fn a_domain_gets_its_addresses() {
        let mut s = store_with(&["api.openai.com"]);
        let r = FakeResolver::new(&[("api.openai.com", &["1.2.3.4", "5.6.7.8"])]);
        let out = resolve_all(&mut s, &r, false);
        assert_eq!(
            out[0].1,
            Outcome::Resolved(vec!["1.2.3.4".into(), "5.6.7.8".into()])
        );
        assert_eq!(s.hosts[0].ips.len(), 2);
        assert_eq!(s.hosts[0].error, None);
        assert_eq!(s.allowed_ips_value(), "1.2.3.4/32, 5.6.7.8/32");
    }

    #[test]
    fn a_failure_is_reported_and_the_reason_is_kept_on_the_host() {
        let mut s = store_with(&["nope.invalid"]);
        let out = resolve_all(&mut s, &FakeResolver::failing("nope.invalid"), false);
        assert_eq!(out[0].1, Outcome::Failed("no such host".into()));
        assert_eq!(s.hosts[0].error.as_deref(), Some("no such host"));
        // The host is still in the store, still visible — not dropped.
        assert_eq!(s.hosts.len(), 1);
        assert_eq!(s.unresolved().len(), 1);
    }

    #[test]
    fn a_failure_does_not_wipe_addresses_that_were_already_known() {
        let mut s = store_with(&["api.openai.com"]);
        s.hosts[0].ips = vec!["1.2.3.4".into()];
        let out = resolve_all(&mut s, &FakeResolver::failing("api.openai.com"), true);
        assert!(matches!(out[0].1, Outcome::Failed(_)));
        assert_eq!(s.hosts[0].ips, vec!["1.2.3.4"]);
    }

    #[test]
    fn a_cached_host_is_skipped_unless_forced() {
        let mut s = store_with(&["api.openai.com"]);
        s.hosts[0].ips = vec!["1.2.3.4".into()];
        let out = resolve_all(&mut s, &FakeResolver::new(&[]), false);
        assert_eq!(out[0].1, Outcome::Fresh);
        let out = resolve_all(&mut s, &FakeResolver::new(&[]), true);
        assert!(matches!(out[0].1, Outcome::Failed(_)));
    }

    #[test]
    fn a_literal_ip_is_not_resolved() {
        let mut s = store_with(&["1.2.3.4"]);
        let out = resolve_all(&mut s, &FakeResolver::new(&[]), true);
        assert_eq!(out[0].1, Outcome::Literal);
    }

    #[test]
    fn duplicate_answers_collapse() {
        let mut s = store_with(&["cdn.example.com"]);
        let r = FakeResolver::new(&[("cdn.example.com", &["1.1.1.1", "1.1.1.1", "2.2.2.2"])]);
        resolve_all(&mut s, &r, false);
        assert_eq!(s.hosts[0].ips, vec!["1.1.1.1", "2.2.2.2"]);
    }

    #[test]
    fn seeding_fills_an_empty_host_and_leaves_a_known_one_alone() {
        let mut a = Host::new("api.openai.com");
        seed_addresses(&mut a, &["1.2.3.4".into()]);
        assert_eq!(a.ips, vec!["1.2.3.4"]);

        let mut b = Host::new("api.openai.com");
        b.ips = vec!["9.9.9.9".into()];
        seed_addresses(&mut b, &["1.2.3.4".into()]);
        assert_eq!(b.ips, vec!["9.9.9.9"]);
    }
}
