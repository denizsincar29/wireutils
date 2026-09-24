//! Reading and writing `AllowedIPs` inside WireGuard `.conf` files.
//!
//! The hard rule for this module: **never lose a byte the user did not ask to
//! change.** A `.conf` in an `amnesiawg` folder carries private keys and
//! endpoint data; a tool that rewrites the file wholesale and gets one detail
//! wrong (a lost `[Peer]` section, a comment eaten, line order shuffled)
//! breaks the owner's VPN and is worse than no tool at all.
//!
//! So the rewrite is line-based: every line that is not the `AllowedIPs` line
//! is preserved byte for byte, including comments, blank lines and the
//! original `\n`/`\r\n` endings. Only the `AllowedIPs` line's *value* is
//! replaced; its indentation and its key spelling are kept as found.

/// One line of a parsed config.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    /// A line that must be kept verbatim.
    Verbatim(String),
    /// The `AllowedIPs` line, split so it can be rewritten in place.
    AllowedIps { prefix: String, suffix: String },
}

/// A `.conf` file with its `AllowedIPs` lines addressable.
#[derive(Debug, Clone)]
pub struct WgConfig {
    lines: Vec<Line>,
    /// The line ending this file uses; detected from the first one found.
    eol: &'static str,
    /// True when the file ended with a newline. A rewrite must not add a
    /// trailing newline to a file that did not have one.
    trailing_newline: bool,
}

/// True when `line`, with any indentation stripped, starts the `AllowedIPs`
/// key. Matching is case-insensitive because WireGuard's own parser is, and
/// the key must be followed by `=` — otherwise `AllowedIPsFoo` would match.
fn split_allowed_ips(line: &str) -> Option<(String, String)> {
    let trimmed_start = line.len() - line.trim_start().len();
    let body = &line[trimmed_start..];
    let (key, rest) = body.split_once('=')?;
    if !key.trim().eq_ignore_ascii_case("allowedips") {
        return None;
    }
    // `prefix` keeps the indentation and the key up to and including '='.
    let prefix = format!("{}{}", &line[..trimmed_start], &body[..key.len() + 1]);
    Some((prefix, rest.to_string()))
}

impl WgConfig {
    /// Parse the text of a `.conf`.
    pub fn parse(text: &str) -> Self {
        let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let trailing_newline = text.ends_with('\n');
        // `split` on '\n' leaves a trailing empty piece when the file ends
        // with a newline; that piece is the trailing newline itself, not a
        // line, so it is dropped here and re-added on write.
        let mut raw: Vec<&str> = text.split('\n').collect();
        if trailing_newline {
            raw.pop();
        }
        let lines = raw
            .into_iter()
            .map(|l| {
                let l = l.strip_suffix('\r').unwrap_or(l);
                match split_allowed_ips(l) {
                    Some((prefix, suffix)) => Line::AllowedIps { prefix, suffix },
                    None => Line::Verbatim(l.to_string()),
                }
            })
            .collect();
        WgConfig {
            lines,
            eol,
            trailing_newline,
        }
    }

    /// Every `AllowedIPs` value in the file, in order, as written (so a value
    /// that still contains a domain name is returned as that domain).
    pub fn allowed_ips(&self) -> Vec<String> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                Line::AllowedIps { suffix, .. } => {
                    Some(suffix.trim().trim_start_matches('=').trim().to_string())
                }
                Line::Verbatim(_) => None,
            })
            .collect()
    }

    /// True when the file has at least one `AllowedIPs` line to write into.
    pub fn has_allowed_ips(&self) -> bool {
        self.lines
            .iter()
            .any(|l| matches!(l, Line::AllowedIps { .. }))
    }

    /// Replace every `AllowedIPs` value with `value`.
    ///
    /// When the file has several `AllowedIPs` lines they all get the same
    /// value: the owner asked for one host list driving every config, and a
    /// half-applied list across a multi-peer file would be a silent
    /// inconsistency.
    pub fn set_allowed_ips(&mut self, value: &str) {
        for line in &mut self.lines {
            if let Line::AllowedIps { suffix, .. } = line {
                *suffix = format!(" {value}");
            }
        }
    }

    /// Render back to text.
    pub fn to_string(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push_str(self.eol);
            }
            match line {
                Line::Verbatim(s) => out.push_str(s),
                Line::AllowedIps { prefix, suffix } => {
                    out.push_str(prefix);
                    out.push_str(suffix.trim_end());
                }
            }
        }
        if self.trailing_newline && !self.lines.is_empty() {
            out.push_str(self.eol);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[Interface]
PrivateKey = aGVsbG8=
Address = 10.8.1.2/32
DNS = 10.8.0.1

[Peer]
PublicKey = d29ybGQ=
PresharedKey = c2VjcmV0
AllowedIPs = 0.0.0.0/0, ::/0
Endpoint = vpn.example.com:51820
";

    #[test]
    fn reads_the_allowed_ips_value() {
        let c = WgConfig::parse(SAMPLE);
        assert_eq!(c.allowed_ips(), vec!["0.0.0.0/0, ::/0".to_string()]);
        assert!(c.has_allowed_ips());
    }

    #[test]
    fn rewrites_only_the_allowed_ips_line() {
        let mut c = WgConfig::parse(SAMPLE);
        c.set_allowed_ips("1.2.3.4/32, 5.6.7.8/32");
        let out = c.to_string();
        assert!(out.contains("AllowedIPs = 1.2.3.4/32, 5.6.7.8/32\n"));
        // Everything the owner did not ask to change is untouched, including
        // the keys.
        assert!(out.contains("PrivateKey = aGVsbG8="));
        assert!(out.contains("PresharedKey = c2VjcmV0"));
        assert!(out.contains("Endpoint = vpn.example.com:51820"));
        assert_eq!(out.matches("AllowedIPs").count(), 1);
    }

    #[test]
    fn keeps_comments_blank_lines_and_order() {
        let text = "# my tunnel\n\n[Peer]\n# the only peer\nAllowedIPs = 10.0.0.0/8\n\n[Peer]\nAllowedIPs = 192.168.0.0/16\n";
        let mut c = WgConfig::parse(text);
        assert_eq!(c.allowed_ips().len(), 2);
        c.set_allowed_ips("1.1.1.1/32");
        let out = c.to_string();
        assert_eq!(
            out,
            "# my tunnel\n\n[Peer]\n# the only peer\nAllowedIPs = 1.1.1.1/32\n\n[Peer]\nAllowedIPs = 1.1.1.1/32\n"
        );
    }

    #[test]
    fn a_file_without_the_key_is_left_exactly_as_it_was() {
        let text = "[Interface]\nPrivateKey = aGVsbG8=\n";
        let mut c = WgConfig::parse(text);
        assert!(!c.has_allowed_ips());
        c.set_allowed_ips("1.2.3.4/32");
        assert_eq!(c.to_string(), text);
    }

    #[test]
    fn preserves_crlf_and_a_missing_trailing_newline() {
        let text = "[Peer]\r\nAllowedIPs = 0.0.0.0/0\r\n";
        let mut c = WgConfig::parse(text);
        c.set_allowed_ips("9.9.9.9/32");
        assert_eq!(c.to_string(), "[Peer]\r\nAllowedIPs = 9.9.9.9/32\r\n");

        let text = "[Peer]\nAllowedIPs = 0.0.0.0/0";
        let mut c = WgConfig::parse(text);
        c.set_allowed_ips("9.9.9.9/32");
        assert_eq!(c.to_string(), "[Peer]\nAllowedIPs = 9.9.9.9/32");
    }

    #[test]
    fn accepts_spacing_and_case_variants() {
        for text in [
            "[Peer]\nallowedips=0.0.0.0/0\n",
            "[Peer]\n  AllowedIPs   =   0.0.0.0/0  \n",
        ] {
            let mut c = WgConfig::parse(text);
            assert!(c.has_allowed_ips(), "not matched: {text:?}");
            c.set_allowed_ips("7.7.7.7/32");
            let out = c.to_string();
            assert!(out.contains("7.7.7.7/32"), "{out:?}");
            assert!(!out.contains("0.0.0.0/0"), "{out:?}");
        }
    }

    #[test]
    fn does_not_mistake_a_longer_key_for_allowed_ips() {
        let text = "[Peer]\nAllowedIPsList = nope\n";
        let c = WgConfig::parse(text);
        assert!(!c.has_allowed_ips());
        assert_eq!(c.to_string(), text);
    }
}
