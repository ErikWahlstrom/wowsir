//! Read-only mode: rules for which sites start read-only, and which requests count
//! as writes.
//!
//! `~/.wowsir/readonly` (or `$WOWSIR_READONLY_FILE`) lists sites, one per line:
//!
//! ```text
//! # sites that open read-only (subdomains included); * = every site except localhost
//! intranet.example.com
//! prod.example.com lock      # an agent can't turn it off
//! ```

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    /// the line's site, as written (`*` for all)
    pub site: String,
    /// `readonly off` is refused
    pub lock: bool,
    /// a blocked write fails the step that caused it
    pub strict: bool,
}

pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("WOWSIR_READONLY_FILE") {
        return PathBuf::from(p);
    }
    crate::platform::home_dir().join(".wowsir").join("readonly")
}

fn parse(text: &str) -> Vec<Rule> {
    text.lines()
        .filter_map(|l| {
            let l = l.split('#').next().unwrap_or("").trim();
            let mut w = l.split_whitespace();
            let site = w.next()?.to_lowercase();
            let rest: Vec<String> = w.map(|x| x.to_lowercase()).collect();
            Some(Rule {
                site,
                lock: rest.iter().any(|x| x == "lock"),
                strict: rest.iter().any(|x| x == "strict"),
            })
        })
        .collect()
}

fn local(host: &str) -> bool {
    host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host == "127.0.0.1"
        || host == "[::1]"
}

/// The rule for `url`, if its site is listed. The most specific line wins.
pub fn rule_for(url: &str) -> Option<Rule> {
    if !url.starts_with("http") {
        return None;
    }
    let text = std::fs::read_to_string(path()).ok()?;
    let host = crate::login::host_of(url).to_lowercase();
    let rules = parse(&text);
    rules
        .iter()
        .filter(|r| host == r.site || host.ends_with(&format!(".{}", r.site)))
        .max_by_key(|r| r.site.len())
        .or_else(|| rules.iter().find(|r| r.site == "*" && !local(&host)))
        .cloned()
}

/// Is this method a write? GET/HEAD/OPTIONS only read.
pub fn is_write_method(m: &str) -> bool {
    !matches!(m, "GET" | "HEAD" | "OPTIONS")
}

/// URL path segments that do something even as a GET (`/logout` would also end a
/// saved login). Matched whole, case-insensitively.
const ACTION_WORDS: &[&str] = &[
    "logout", "logoff", "log-out", "signout", "sign-out", "delete", "remove", "destroy",
];

/// A GET whose path reads like an action: `/api/lights/3/delete`, `/account/logout`.
pub fn action_get(url: &str) -> Option<&'static str> {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let path = rest.split(['?', '#']).next().unwrap_or("");
    path.split('/').skip(1).find_map(|seg| {
        let seg = seg.to_lowercase();
        let stem = seg.split('.').next().unwrap_or("");
        ACTION_WORDS.iter().find(|w| **w == stem).copied()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules() {
        let r = parse("# x\nprod.example.com lock\n\n*  # all\nstaging.example.com strict lock");
        assert_eq!(r.len(), 3);
        assert!(r[0].lock && !r[0].strict);
        assert_eq!(r[1].site, "*");
        assert!(r[2].lock && r[2].strict);
    }

    #[test]
    fn action_gets() {
        assert_eq!(
            action_get("https://a.b/api/lights/3/delete"),
            Some("delete")
        );
        assert_eq!(action_get("https://a.b/Account/Logout?x=1"), Some("logout"));
        assert_eq!(action_get("https://a.b/logout.aspx"), Some("logout"));
        assert_eq!(action_get("https://a.b/api/deleted-items"), None);
        assert_eq!(action_get("https://remove.example.com/items"), None);
        assert_eq!(action_get("https://a.b/search?q=delete"), None);
    }

    #[test]
    fn locals_are_not_star() {
        assert!(local("localhost") && local("127.0.0.1") && local("app.localhost"));
        assert!(!local("example.com"));
    }
}
