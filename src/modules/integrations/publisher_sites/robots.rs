//! robots.txt, read as RFC 9309 says.
//!
//! The rules that apply are those of the groups naming our product token
//! (`BiblioGenius`), or, when no group does, those of the `*` groups: a
//! publisher can shut the app out with two lines without writing to us.
//! Within them the longest matching pattern wins, `Allow` winning a tie;
//! `*` matches any run of characters and a final `$` anchors the end.
//! Groups run from a `User-agent` line to the next `User-agent` line that
//! follows a rule; blank lines do not end them (Python's robotparser does,
//! and drops the `?` of `Disallow: /catalogue?`: both mislead on real sites).

use std::collections::HashSet;
use std::time::Duration;

use url::Url;

/// The product token we look for in `User-agent` lines, lowercase.
pub const PRODUCT_TOKEN: &str = "bibliogenius";

/// Longest Crawl-delay honoured; a larger value is clamped, not obeyed.
const MAX_CRAWL_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
struct Rule {
    allow: bool,
    pattern: String,
}

/// The rules of one site that apply to us.
#[derive(Debug, Clone, Default)]
pub struct Robots {
    rules: Vec<Rule>,
    crawl_delay: Option<Duration>,
    /// The file could not be read for a reason that is the server's
    /// (5xx, unreachable): everything is forbidden until it can (RFC 9309, 2.3.1.4).
    unreachable: bool,
    /// The site maps the file declares, whatever the group: they are not
    /// rules.
    sitemaps: Vec<String>,
}

impl Robots {
    /// No robots.txt (4xx): everything is allowed.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// The server failed to answer for its robots.txt: nothing is allowed.
    pub fn unreachable() -> Self {
        Self {
            unreachable: true,
            ..Self::default()
        }
    }

    pub fn parse(text: &str) -> Self {
        // A byte order mark would glue itself to the first `User-agent`.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut ours = Vec::new();
        let mut any = Vec::new();
        let (mut ours_delay, mut any_delay) = (None, None);

        // Whether the group being read names us, or `*`, and whether a rule
        // has been seen since its last User-agent line (a new User-agent
        // then starts a new group). Kept up to date line by line: a file of
        // thousands of User-agent lines is read in one pass.
        let (mut names_us, mut names_any) = (false, false);
        let mut in_rules = false;
        let mut sitemaps: Vec<String> = Vec::new();
        let mut seen_sitemaps: HashSet<String> = HashSet::new();
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => {
                    if in_rules {
                        (names_us, names_any) = (false, false);
                        in_rules = false;
                    }
                    let agent = value.to_ascii_lowercase();
                    names_us |= is_our_token(&agent);
                    names_any |= agent == "*";
                }
                "allow" | "disallow" => {
                    in_rules = true;
                    // An empty Disallow allows everything: no rule to keep.
                    if value.is_empty() || !(names_us || names_any) {
                        continue;
                    }
                    let rule = Rule {
                        allow: key == "allow",
                        pattern: normalize_path(value),
                    };
                    if names_us {
                        ours.push(rule.clone());
                    }
                    if names_any {
                        any.push(rule);
                    }
                }
                "crawl-delay" => {
                    in_rules = true;
                    let delay = value
                        .parse::<f64>()
                        .ok()
                        .filter(|d| d.is_finite() && *d >= 0.0)
                        // Clamped before conversion: a huge value would not fit.
                        .map(|d| Duration::from_secs_f64(d.min(MAX_CRAWL_DELAY.as_secs_f64())));
                    if names_us {
                        ours_delay = ours_delay.or(delay);
                    }
                    if names_any {
                        any_delay = any_delay.or(delay);
                    }
                }
                "sitemap" if !value.is_empty() && seen_sitemaps.insert(value.to_owned()) => {
                    sitemaps.push(value.to_owned());
                }
                _ => {}
            }
        }
        let has_our_group = text.lines().any(|raw| {
            let line = raw.split('#').next().unwrap_or("").trim();
            line.split_once(':').is_some_and(|(k, v)| {
                k.trim().eq_ignore_ascii_case("user-agent")
                    && is_our_token(&v.trim().to_ascii_lowercase())
            })
        });
        if has_our_group {
            Self {
                rules: ours,
                crawl_delay: ours_delay,
                unreachable: false,
                sitemaps,
            }
        } else {
            Self {
                rules: any,
                crawl_delay: any_delay,
                unreachable: false,
                sitemaps,
            }
        }
    }

    /// Whether we may fetch `url`.
    pub fn allows(&self, url: &Url) -> bool {
        if self.unreachable {
            return false;
        }
        let mut target = url.path().to_owned();
        if let Some(q) = url.query() {
            target.push('?');
            target.push_str(q);
        }
        let target = normalize_path(&target);
        // robots.txt itself is always allowed.
        if target == "/robots.txt" {
            return true;
        }
        let best = self
            .rules
            .iter()
            .filter(|r| pattern_matches(&r.pattern, &target))
            .max_by_key(|r| (r.pattern.len(), r.allow));
        best.is_none_or(|r| r.allow)
    }

    /// The pause the site asks for between two requests, if any.
    pub fn crawl_delay(&self) -> Option<Duration> {
        self.crawl_delay
    }

    /// The addresses of the site maps the file declares.
    pub fn sitemaps(&self) -> &[String] {
        &self.sitemaps
    }

    /// Whether robots.txt could not be read for a reason that is the
    /// server's (5xx): the site is to be taken as unavailable for now.
    pub fn is_unreachable(&self) -> bool {
        self.unreachable
    }

    /// Whether the site forbids us everything (an opt-out, or an unreadable
    /// robots.txt).
    pub fn forbids_everything(&self) -> bool {
        self.unreachable
            || self
                .rules
                .iter()
                .any(|r| !r.allow && (r.pattern == "/" || r.pattern == "/*"))
                && !self.rules.iter().any(|r| r.allow)
    }
}

fn is_our_token(agent: &str) -> bool {
    agent
        .strip_prefix(PRODUCT_TOKEN)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Characters RFC 3986 reserves: percent-encoded, they mean something else
/// than written out, so they stay encoded for comparison.
const RESERVED: &[u8] = b":/?#[]@!$&'()*+,;=";

/// A path, or a rule's pattern, in the form RFC 9309 (2.2.2) compares:
/// octets outside US-ASCII percent-encoded, a percent-encoded US-ASCII octet
/// decoded unless it is reserved (or `%` itself), hex digits in capitals. So
/// `Disallow: /recherche-avancée` matches the address a browser sends,
/// `/recherche-avanc%C3%A9e`.
fn normalize_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let escaped = (b == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match escaped {
            Some(v) if v.is_ascii() && v != b'%' && !RESERVED.contains(&v) => {
                out.push(char::from(v));
                i += 3;
            }
            Some(v) => {
                out.push_str(&format!("%{v:02X}"));
                i += 3;
            }
            None if !b.is_ascii() => {
                out.push_str(&format!("%{b:02X}"));
                i += 1;
            }
            None => {
                out.push(char::from(b));
                i += 1;
            }
        }
    }
    out
}

/// Whether a robots.txt path pattern matches the start of `target`.
fn pattern_matches(pattern: &str, target: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let pieces: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0;
    for (i, piece) in pieces.iter().enumerate() {
        if i == 0 {
            if !target.starts_with(piece) {
                return false;
            }
            pos = piece.len();
            continue;
        }
        let is_last = i == pieces.len() - 1;
        if is_last && anchored {
            // The last piece must end the target, after the current position.
            return target.len() >= pos + piece.len() && target.ends_with(piece);
        }
        match target[pos..].find(piece) {
            Some(found) => pos += found + piece.len(),
            None => return false,
        }
    }
    !anchored || pos == target.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allows(robots: &Robots, path: &str) -> bool {
        robots.allows(&Url::parse(&format!("https://example.org{path}")).unwrap())
    }

    // The real Gallimard file, abridged: a blank line inside the group, a
    // query-string rule, a crawl delay, and named bots shut out.
    const GALLIMARD: &str = "User-agent: *\nCrawl-delay: 10\n\nDisallow: /recherche-au-coeur-des-livres\nDisallow: /catalogue?\n\nUser-agent: GPTBot\nDisallow: /\n\nSitemap: https://www.gallimard.fr/sitemap.xml\n";

    #[test]
    fn site_maps_are_read_whatever_the_group() {
        let robots = Robots::parse(GALLIMARD);
        assert_eq!(robots.sitemaps(), ["https://www.gallimard.fr/sitemap.xml"]);
        assert!(Robots::allow_all().sitemaps().is_empty());
    }

    #[test]
    fn a_blank_line_does_not_end_a_group() {
        let robots = Robots::parse(GALLIMARD);
        assert_eq!(robots.crawl_delay(), Some(Duration::from_secs(10)));
        assert!(!allows(&robots, "/recherche-au-coeur-des-livres?q=x"));
        assert!(!allows(&robots, "/catalogue?search=martin"));
        assert!(allows(&robots, "/catalogue/martin-eden/9782070000000"));
        assert!(allows(&robots, "/"));
        assert!(!robots.forbids_everything());
    }

    #[test]
    fn our_own_group_replaces_the_star_group() {
        let robots = Robots::parse(
            "User-agent: *\nDisallow: /private\n\nUser-agent: BiblioGenius\nDisallow: /\n",
        );
        assert!(!allows(&robots, "/livre/x"));
        assert!(robots.forbids_everything());

        let robots =
            Robots::parse("User-agent: BiblioGenius/1.0\nAllow: /\n\nUser-agent: *\nDisallow: /\n");
        assert!(allows(&robots, "/livre/x"));
    }

    #[test]
    fn several_agents_can_share_a_group() {
        let robots = Robots::parse("User-agent: GPTBot\nUser-agent: *\nDisallow: /search\n");
        assert!(!allows(&robots, "/search?q=1"));
        assert!(allows(&robots, "/livre"));
    }

    #[test]
    fn longest_match_wins_and_allow_wins_a_tie() {
        let robots = Robots::parse(
            "User-agent: *\nDisallow: /catalogue\nAllow: /catalogue/\nDisallow: /a\nAllow: /a\n",
        );
        assert!(!allows(&robots, "/catalogue?x=1"));
        assert!(allows(&robots, "/catalogue/livre"));
        assert!(allows(&robots, "/a"));
    }

    #[test]
    fn wildcards_and_end_anchor() {
        let robots = Robots::parse("User-agent: *\nDisallow: /*.pdf$\nDisallow: /*?sort=\n");
        assert!(!allows(&robots, "/files/book.pdf"));
        assert!(allows(&robots, "/files/book.pdf?download=1"));
        assert!(!allows(&robots, "/list?page=2&x=1?sort=asc"));
        assert!(!allows(&robots, "/list?sort=asc"));
        assert!(allows(&robots, "/list?page=2"));
    }

    #[test]
    fn an_empty_disallow_allows_everything() {
        let robots = Robots::parse("User-agent: *\nDisallow:\n");
        assert!(allows(&robots, "/anything"));
    }

    #[test]
    fn rules_for_other_bots_do_not_apply() {
        let robots = Robots::parse("User-agent: GPTBot\nDisallow: /\n");
        assert!(allows(&robots, "/livre"));
        assert!(!robots.forbids_everything());
    }

    #[test]
    fn an_unreachable_file_forbids_everything_but_itself() {
        let robots = Robots::unreachable();
        assert!(!allows(&robots, "/"));
        assert!(robots.forbids_everything());
        assert!(allows(&Robots::allow_all(), "/"));
    }

    #[test]
    fn a_huge_crawl_delay_is_clamped() {
        let robots = Robots::parse("User-agent: *\nCrawl-delay: 86400\n");
        assert_eq!(robots.crawl_delay(), Some(MAX_CRAWL_DELAY));
        let robots = Robots::parse("User-agent: *\nCrawl-delay: 1e20\n");
        assert_eq!(robots.crawl_delay(), Some(MAX_CRAWL_DELAY));
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_first_group() {
        let robots = Robots::parse("\u{feff}User-agent: *\nDisallow: /\n");
        assert!(!allows(&robots, "/livre"));
    }

    #[test]
    fn paths_and_patterns_are_compared_normalized() {
        // A pattern written with an accent, an address percent-encoded.
        let robots = Robots::parse("User-agent: *\nDisallow: /recherche-avancée\n");
        assert!(!allows(&robots, "/recherche-avanc%C3%A9e?q=x"));
        assert!(!allows(&robots, "/recherche-avancée"));
        // Lowercase escapes in the file, an unreserved letter escaped.
        let robots = Robots::parse("User-agent: *\nDisallow: /x%c3%a9\nDisallow: /a%62c\n");
        assert!(!allows(&robots, "/x%C3%A9"));
        assert!(!allows(&robots, "/abc"));
        // A reserved character escaped means something else than written.
        let robots = Robots::parse("User-agent: *\nDisallow: /a%2Fb\n");
        assert!(allows(&robots, "/a/b"));
        assert!(!allows(&robots, "/a%2fb"));
        assert_eq!(normalize_path("/50%25"), "/50%25");
        assert_eq!(normalize_path("/100%"), "/100%");
    }

    #[test]
    fn a_file_of_many_agents_is_read_in_one_pass() {
        // 512 KiB of User-agent lines took over a minute when every line
        // rescanned the agents of the group.
        let text = "User-agent: a\n".repeat(512 * 1024 / 14) + "Disallow: /\n";
        let started = std::time::Instant::now();
        let robots = Robots::parse(&text);
        assert!(allows(&robots, "/livre"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }
}
