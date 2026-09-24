//! The catalog of known sites.
//!
//! A template is a named group of hosts ("chatgpt", "instagram") that the
//! owner can pull in by typing a few letters. It ships **inside the binary**
//! as the fallback, and can also be fetched from the internet so the list
//! stays useful as sites move: an online catalog that parses and covers at
//! least a few templates replaces the built-in one, and anything else is
//! ignored with the built-in kept.

use serde::{Deserialize, Serialize};

use crate::hosts::{Host, HostStore, Source};

/// Where `templates-update` looks when no URL is given. The owner's own
/// server, so the catalog can be corrected without shipping a new binary.
pub const DEFAULT_CATALOG_URL: &str =
    "https://ai.denizsincar.ru/pipelines/wireutils/templates.json";

/// The catalog file format. Kept deliberately small and forgiving: an
/// unknown field is ignored, a missing field takes a default, so a catalog
/// published by a newer version of the app still loads here.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Catalog {
    #[serde(default = "catalog_version")]
    pub version: u32,
    #[serde(default)]
    pub templates: Vec<Template>,
}

fn catalog_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Template {
    /// Group name, also the key typed in the search box.
    pub name: String,
    /// Words the search matches besides the name, so `gpt` finds `chatgpt`.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Domains to add. Resolved at apply time unless `ips` already covers
    /// them.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Addresses known to answer for this group, used when DNS is
    /// unavailable. Optional: an empty list just means "resolve online".
    #[serde(default)]
    pub ips: Vec<String>,
}

impl Template {
    /// True when `needle` (lowercased) appears in the name or a keyword.
    pub fn matches(&self, needle: &str) -> bool {
        let n = needle.trim().to_lowercase();
        if n.is_empty() {
            return true;
        }
        self.name.to_lowercase().contains(&n)
            || self.keywords.iter().any(|k| k.to_lowercase().contains(&n))
    }
}

impl Catalog {
    /// Templates matching a search string, best match first: a name that
    /// starts with the needle beats one that merely contains it.
    pub fn search(&self, needle: &str) -> Vec<&Template> {
        let n = needle.trim().to_lowercase();
        let mut hits: Vec<&Template> =
            self.templates.iter().filter(|t| t.matches(&n)).collect();
        hits.sort_by_key(|t| {
            let name = t.name.to_lowercase();
            if name == n {
                0
            } else if name.starts_with(&n) {
                1
            } else if name.contains(&n) {
                2
            } else {
                3
            }
        });
        hits
    }

    pub fn get(&self, name: &str) -> Option<&Template> {
        self.templates.iter().find(|t| t.name == name)
    }

    /// Add a template's domains to the store as one group, seeding the
    /// addresses it ships with so an offline apply still covers the site.
    pub fn apply(&self, store: &mut HostStore, name: &str) -> Result<usize, String> {
        let t = self.get(name).ok_or_else(|| format!("no template named {name}"))?;
        store.ensure_group(&t.name);
        let mut added = 0;
        for domain in &t.domains {
            let known = store.hosts.iter().any(|h| h.target == *domain);
            let host = store.add_host(domain, &[t.name.clone()]);
            host.source = Source::Template;
            if !known {
                added += 1;
            }
        }
        // Seed after insertion so a host that already had addresses keeps
        // them; the catalog's addresses are a fallback, not an override.
        for domain in &t.domains {
            if let Some(h) = store.hosts.iter_mut().find(|h| h.target == *domain) {
                if h.ips.is_empty() && !t.ips.is_empty() {
                    let literal = Host::new(domain);
                    if !literal.is_literal_ip() {
                        h.ips = t.ips.clone();
                    }
                }
            }
        }
        Ok(added)
    }
}

/// Fetch a catalog over HTTP, from a `file://` path, or from a local path.
/// The container's tests use the local-path branch; the GUI on Windows uses
/// the http branch.
pub fn fetch(url: &str) -> Result<Catalog, String> {
    let text = if let Some(path) = url.strip_prefix("file://") {
        std::fs::read_to_string(path).map_err(|e| e.to_string())?
    } else if url.starts_with("http://") || url.starts_with("https://") {
        ureq::get(url)
            .call()
            .map_err(|e| e.to_string())?
            .into_string()
            .map_err(|e| e.to_string())?
    } else {
        std::fs::read_to_string(url).map_err(|e| e.to_string())?
    };
    parse(&text)
}

/// Parse a catalog and sanity-check it. A response that parses but carries
/// nothing usable (empty, or full of nameless templates) is refused so the
/// caller keeps the built-in catalog instead of replacing a good list with
/// an empty one.
pub fn parse(text: &str) -> Result<Catalog, String> {
    let mut catalog: Catalog = serde_json::from_str(text).map_err(|e| e.to_string())?;
    catalog.templates.retain(|t| !t.name.trim().is_empty() && !t.domains.is_empty());
    if catalog.templates.is_empty() {
        return Err("catalog has no usable templates".to_string());
    }
    Ok(catalog)
}

/// The catalog compiled into the binary. Small on purpose: it covers the
/// services whose blocking people actually hit, and the online update fills
/// in the rest.
pub fn builtin() -> Catalog {
    let raw = include_str!("../data/templates.json");
    serde_json::from_str(raw).expect("built-in templates.json is valid")
}

/// Merge an updated catalog into the store's view: the updated one wins
/// wholesale, but the built-in templates it does not mention are kept, so an
/// update can add and refine without being able to delete a group the owner
/// relies on.
pub fn merge(updated: Catalog, builtin: Catalog) -> Catalog {
    let mut out = updated;
    for t in builtin.templates {
        if out.get(&t.name).is_none() {
            out.templates.push(t);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        serde_json::from_str(
            r#"{
              "version": 1,
              "templates": [
                {"name":"chatgpt","keywords":["openai","gpt"],"domains":["chat.openai.com","api.openai.com"]},
                {"name":"instagram","keywords":["meta","insta"],"domains":["instagram.com","cdninstagram.com"]},
                {"name":"github","keywords":["git"],"domains":["github.com"]}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn a_partial_name_finds_the_group() {
        let c = catalog();
        let hits = c.search("instagr");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "instagram");
    }

    #[test]
    fn a_keyword_finds_the_group() {
        let c = catalog();
        let hits = c.search("gpt");
        assert_eq!(hits[0].name, "chatgpt");
    }

    #[test]
    fn an_exact_name_sorts_first() {
        let c = catalog();
        let hits = c.search("git");
        assert_eq!(hits[0].name, "github");
    }

    #[test]
    fn an_empty_search_lists_everything() {
        assert_eq!(catalog().search("").len(), 3);
    }

    #[test]
    fn applying_a_template_makes_one_group_of_several_domains() {
        let c = catalog();
        let mut s = HostStore::default();
        let added = c.apply(&mut s, "chatgpt").unwrap();
        assert_eq!(added, 2);
        assert_eq!(s.hosts_in_group("chatgpt").count(), 2);
        assert!(s.hosts.iter().all(|h| h.source == Source::Template));
    }

    #[test]
    fn applying_twice_does_not_duplicate() {
        let c = catalog();
        let mut s = HostStore::default();
        c.apply(&mut s, "chatgpt").unwrap();
        let added = c.apply(&mut s, "chatgpt").unwrap();
        assert_eq!(added, 0);
        assert_eq!(s.hosts.len(), 2);
    }

    #[test]
    fn a_template_ships_addresses_for_offline_use() {
        let c: Catalog = serde_json::from_str(
            r#"{"templates":[{"name":"x","domains":["x.example.com"],"ips":["1.2.3.4"]}]}"#,
        )
        .unwrap();
        let mut s = HostStore::default();
        c.apply(&mut s, "x").unwrap();
        assert_eq!(s.hosts[0].ips, vec!["1.2.3.4"]);
        assert_eq!(s.allowed_ips_value(), "1.2.3.4/32");
    }

    #[test]
    fn an_unknown_template_is_an_error_not_a_silent_no_op() {
        let mut s = HostStore::default();
        assert!(catalog().apply(&mut s, "nope").is_err());
        assert!(s.hosts.is_empty());
    }

    #[test]
    fn a_broken_update_is_refused() {
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"templates":[]}"#).is_err());
        assert!(parse(r#"{"templates":[{"name":"","domains":["a.com"]}]}"#).is_err());
        assert!(parse(r#"{"templates":[{"name":"ok","domains":[]}]}"#).is_err());
    }

    #[test]
    fn a_good_update_parses() {
        let c = parse(r#"{"templates":[{"name":"ok","domains":["a.com"]}]}"#).unwrap();
        assert_eq!(c.templates.len(), 1);
    }

    #[test]
    fn an_update_cannot_delete_a_builtin_group() {
        let builtin = catalog();
        let updated: Catalog = serde_json::from_str(
            r#"{"templates":[{"name":"chatgpt","domains":["chatgpt.com"]}]}"#,
        )
        .unwrap();
        let merged = merge(updated, builtin);
        assert!(merged.get("chatgpt").is_some());
        assert_eq!(merged.get("chatgpt").unwrap().domains, vec!["chatgpt.com"]);
        assert!(merged.get("instagram").is_some());
        assert!(merged.get("github").is_some());
    }

    #[test]
    fn the_builtin_catalog_is_valid_and_searchable() {
        let c = builtin();
        assert!(c.templates.len() >= 8, "only {} templates", c.templates.len());
        assert!(c.search("instagr").iter().any(|t| t.name == "instagram"));
        assert!(c.search("openai").iter().any(|t| t.name == "chatgpt"));
        for t in &c.templates {
            assert!(!t.domains.is_empty(), "{} has no domains", t.name);
            for d in &t.domains {
                assert!(!d.contains(char::is_whitespace), "{d} in {}", t.name);
            }
        }
    }

    #[test]
    fn fetching_from_a_local_path_works() {
        let dir = std::env::temp_dir().join("wireutils_catalog_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("catalog.json");
        std::fs::write(&path, r#"{"templates":[{"name":"ok","domains":["a.com"]}]}"#).unwrap();
        let c = fetch(path.to_str().unwrap()).unwrap();
        assert_eq!(c.templates.len(), 1);
    }
}
