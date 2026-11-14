//! Checks on the table and the probe results that need no network: the
//! defects a probe can leave in a recipe without failing, found by reading
//! what it wrote. Pure: the maintenance tool reads the files and prints.

use std::collections::BTreeMap;

use url::Url;

use super::page::names_the_site;
use super::table::{
    CheckStatus, ProbeReport, ProbeStatus, PublisherTable, RecipeKind, SiteRecipes,
};
use super::text::fold;

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth a look: often a narrower recipe than it could be.
    Warning,
    /// The recipe is wrong for most books: fix it before shipping the table.
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// A product template with one book's internal number frozen into every
    /// address: five digits or more outside its placeholders, or a number
    /// glued to one (`/2043-{slug}-{isbn}.html`, as shops number products).
    FrozenNumber,
    /// A witness whose title is the site's name: the page's title was not
    /// read, and every recipe judged on that witness is suspect.
    SiteNameTitle,
    /// A product template with a path segment of three words or more
    /// outside its placeholders: a fixed section ("/romans-policiers/"),
    /// which only serves that section's books.
    FixedSection,
    /// A site map recipe whose map was cut at the size cap when probed: the
    /// books listed after the cut cannot be found through it.
    SitemapTruncated,
    /// No conclusive success for a long time: neither a probe that found the
    /// recipe nor a check that passed.
    Stale,
}

impl Rule {
    pub fn severity(self) -> Severity {
        match self {
            Rule::FrozenNumber | Rule::SiteNameTitle => Severity::Error,
            Rule::FixedSection | Rule::SitemapTruncated | Rule::Stale => Severity::Warning,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Rule::FrozenNumber => "frozen-number",
            Rule::SiteNameTitle => "site-name-title",
            Rule::FixedSection => "fixed-section",
            Rule::SitemapTruncated => "sitemap-truncated",
            Rule::Stale => "stale",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub site: String,
    pub rule: Rule,
    pub detail: String,
}

/// Days without a conclusive success after which a shipped site is
/// [`Rule::Stale`].
pub const STALE_DAYS: i64 = 120;

/// Digits in a row that make a frozen number. Four would catch years
/// (`/2024/`), which some sites do put in their addresses.
const FROZEN_DIGITS: usize = 5;

/// Digits of a number glued to a placeholder that make it a product number:
/// fewer may be a volume or an edition (`tome-2-{slug}`).
const GLUED_DIGITS: usize = 3;

/// Words in a fixed path segment that make it a section rather than a
/// short label (`/livre/`, `/fr/`, `/glenat-bd/`).
const SECTION_WORDS: usize = 3;

/// Every finding on the shipped `table`, using `probe` for the witnesses and
/// the dates. `today` is `YYYY-MM-DD`. Sorted by site, then rule.
pub fn lint(table: &PublisherTable, probe: &ProbeReport, today: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for (key, recipes) in &table.sites {
        lint_recipes(key, recipes, &mut out);
        if let Some(p) = probe.sites.get(key) {
            if let (Some(map), Some(check)) = (&recipes.sitemap, &p.sitemap)
                && check.truncated
            {
                out.push(Finding {
                    site: key.clone(),
                    rule: Rule::SitemapTruncated,
                    detail: format!("{} is cut at the size cap", map.url),
                });
            }
            let home = Url::parse(&p.home).ok();
            for w in &p.witnesses {
                let folded = fold(&w.title);
                let words: Vec<&str> = folded.split(' ').filter(|w| !w.is_empty()).collect();
                let site_url = w
                    .url
                    .as_deref()
                    .and_then(|u| Url::parse(u).ok())
                    .or_else(|| home.clone());
                if site_url.is_some_and(|u| names_the_site(&words, &u)) {
                    out.push(Finding {
                        site: key.clone(),
                        rule: Rule::SiteNameTitle,
                        detail: format!("witness {} is titled \"{}\"", w.isbn, w.title),
                    });
                }
            }
        }
        if let Some(days) = days_since_success(key, probe, today)
            && days > STALE_DAYS
        {
            out.push(Finding {
                site: key.clone(),
                rule: Rule::Stale,
                detail: format!("last success {days} days ago"),
            });
        }
    }
    out.sort_by(|a, b| (&a.site, a.rule).cmp(&(&b.site, b.rule)));
    out
}

fn lint_recipes(key: &str, recipes: &SiteRecipes, out: &mut Vec<Finding>) {
    let Some(product) = &recipes.product else {
        return;
    };
    // The path as written, placeholders kept, query left out.
    let path = template_path(&product.template);
    for segment in path.split('/') {
        let fixed = strip_placeholders(segment);
        if longest_digit_run(&fixed) >= FROZEN_DIGITS || numbered_placeholder(segment) {
            out.push(Finding {
                site: key.to_owned(),
                rule: Rule::FrozenNumber,
                detail: format!("product template {}", product.template),
            });
            return;
        }
    }
    for segment in path.split('/') {
        if segment.contains('{') {
            continue;
        }
        let words = segment
            .trim_end_matches(".html")
            .split(['-', '_'])
            .filter(|w| !w.is_empty())
            .count();
        if words >= SECTION_WORDS {
            out.push(Finding {
                site: key.to_owned(),
                rule: Rule::FixedSection,
                detail: format!("product template {}", product.template),
            });
            return;
        }
    }
}

/// The path of a template: after the host, before any `?`.
fn template_path(template: &str) -> &str {
    let rest = template
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(template);
    let path = rest.find('/').map(|i| &rest[i..]).unwrap_or("");
    path.split('?').next().unwrap_or(path)
}

/// A segment with its `{...}` placeholders taken out.
fn strip_placeholders(segment: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in segment.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Whether a number of [`GLUED_DIGITS`] digits or more sits right before
/// a placeholder, joined by a dash: `2043-{slug}`.
fn numbered_placeholder(segment: &str) -> bool {
    segment.match_indices("-{").any(|(at, _)| {
        let digits = segment[..at]
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .count();
        let before = segment[..at].chars().rev().nth(digits);
        digits >= GLUED_DIGITS && before.is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

fn longest_digit_run(s: &str) -> usize {
    let (mut best, mut run) = (0, 0);
    for c in s.chars() {
        run = if c.is_ascii_digit() { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

/// Days since the site's last conclusive success: a probe that found a
/// recipe, or a check that found them working. `None` when neither date
/// can be read.
fn days_since_success(key: &str, probe: &ProbeReport, today: &str) -> Option<i64> {
    let probed = probe
        .sites
        .get(key)
        .filter(|p| p.status == ProbeStatus::Ok)
        .map(|p| p.probed_at.as_str());
    let checked = probe
        .checks
        .get(key)
        .filter(|c| c.status == CheckStatus::Works)
        .map(|c| c.checked_at.as_str());
    let last = probed.into_iter().chain(checked).max()?;
    let day = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
    Some((day(today)? - day(last)?).num_days())
}

/// What changed between two tables.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TableDiff {
    pub added_sites: Vec<String>,
    pub removed_sites: Vec<String>,
    /// Site, recipe kind, before, after (`None`: no such recipe).
    pub changed_recipes: Vec<(String, RecipeKind, Option<String>, Option<String>)>,
    /// Prefix, sites before, sites after (empty: no such prefix).
    pub changed_prefixes: Vec<(String, Vec<String>, Vec<String>)>,
}

impl TableDiff {
    pub fn is_empty(&self) -> bool {
        self.added_sites.is_empty()
            && self.removed_sites.is_empty()
            && self.changed_recipes.is_empty()
            && self.changed_prefixes.is_empty()
    }
}

/// The differences from `old` to `new`. A site that came or went is listed
/// once, not again for each of its recipes.
pub fn diff(old: &PublisherTable, new: &PublisherTable) -> TableDiff {
    let mut d = TableDiff::default();
    for key in new.sites.keys() {
        if !old.sites.contains_key(key) {
            d.added_sites.push(key.clone());
        }
    }
    for key in old.sites.keys() {
        if !new.sites.contains_key(key) {
            d.removed_sites.push(key.clone());
        }
    }
    for (key, after) in &new.sites {
        let Some(before) = old.sites.get(key) else {
            continue;
        };
        for kind in [RecipeKind::Product, RecipeKind::Search, RecipeKind::Sitemap] {
            let (b, a) = (recipe_text(before, kind), recipe_text(after, kind));
            if b != a {
                d.changed_recipes.push((key.clone(), kind, b, a));
            }
        }
    }
    let sites_of = |t: &PublisherTable| -> BTreeMap<String, Vec<String>> {
        t.prefixes
            .iter()
            .map(|e| {
                (
                    e.prefix.clone(),
                    e.publishers.iter().map(|p| p.site.clone()).collect(),
                )
            })
            .collect()
    };
    let (before, after) = (sites_of(old), sites_of(new));
    let mut prefixes: Vec<&String> = before.keys().chain(after.keys()).collect();
    prefixes.sort();
    prefixes.dedup();
    for prefix in prefixes {
        let b = before.get(prefix).cloned().unwrap_or_default();
        let a = after.get(prefix).cloned().unwrap_or_default();
        if b != a {
            d.changed_prefixes.push((prefix.clone(), b, a));
        }
    }
    d
}

/// A recipe as one line of text, for comparing and printing.
fn recipe_text(site: &SiteRecipes, kind: RecipeKind) -> Option<String> {
    match kind {
        RecipeKind::Product => site.product.as_ref().map(|p| {
            let mut s = p.template.clone();
            if p.slug_ignored {
                s.push_str(" (slug ignored)");
            }
            s
        }),
        RecipeKind::Search => site.search.as_ref().map(|s| {
            let mut t = s.url.clone();
            if s.post_fields.is_some() {
                t.push_str(" (POST)");
            }
            t
        }),
        RecipeKind::Sitemap => site.sitemap.as_ref().map(|m| m.url.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::integrations::publisher_sites::table::{
        PrefixEntry, PrefixPublisher, ProductRecipe, RecipeCheck, SiteProbe, TABLE_FORMAT, Witness,
    };
    use crate::modules::integrations::publisher_sites::text::Apostrophe;

    fn site(template: Option<&str>) -> SiteRecipes {
        SiteRecipes {
            home: "https://e.example/".into(),
            product: template.map(|t| ProductRecipe {
                template: t.into(),
                apostrophe: Apostrophe::Drop,
                slug_ignored: false,
            }),
            search: None,
            sitemap: None,
        }
    }

    fn table(sites: &[(&str, SiteRecipes)]) -> PublisherTable {
        PublisherTable {
            format: TABLE_FORMAT,
            generated: "2026-11-14".into(),
            prefixes: sites
                .iter()
                .enumerate()
                .map(|(i, (key, _))| PrefixEntry {
                    prefix: format!("97820{i}"),
                    publishers: vec![PrefixPublisher {
                        name: (*key).into(),
                        site: (*key).into(),
                    }],
                })
                .collect(),
            sites: sites
                .iter()
                .map(|(k, s)| ((*k).to_owned(), s.clone()))
                .collect(),
        }
    }

    fn probed(home: &str, at: &str, titles: &[&str]) -> SiteProbe {
        SiteProbe {
            home: home.into(),
            status: ProbeStatus::Ok,
            probed_at: at.into(),
            note: None,
            witnesses: titles
                .iter()
                .map(|t| Witness {
                    isbn: "9782000000011".into(),
                    title: (*t).into(),
                    authors: String::new(),
                    url: None,
                })
                .collect(),
            product: None,
            search: None,
            tried: Vec::new(),
            book_sitemap: None,
            sitemap: None,
        }
    }

    fn rules(findings: &[Finding]) -> Vec<(&str, Rule)> {
        findings.iter().map(|f| (f.site.as_str(), f.rule)).collect()
    }

    #[test]
    fn flags_frozen_numbers_and_fixed_sections_only() {
        let t = table(&[
            (
                "champion.example",
                site(Some(
                    "https://champion.example/fr/{slug}/13531-book-08536565-{isbn}.html",
                )),
            ),
            (
                "robert.example",
                site(Some(
                    "https://robert.example/loisirs-et-culture-generale/{slug}-{isbn}.html",
                )),
            ),
            // Short labels, a year, a query: fine.
            (
                "ok.example",
                site(Some(
                    "https://ok.example/livre/2024/{slug}-{isbn}?id=1234567",
                )),
            ),
            (
                "bd.example",
                site(Some("https://bd.example/glenat-bd/{slug}-{isbn}/")),
            ),
            (
                "tome.example",
                site(Some("https://tome.example/tome-2-{slug}-{isbn}")),
            ),
            (
                "shop.example",
                site(Some("https://shop.example/fr/2043-{slug}-{isbn}.html")),
            ),
            ("none.example", site(None)),
        ]);
        let findings = lint(&t, &ProbeReport::default(), "2026-11-14");
        assert_eq!(
            rules(&findings),
            [
                ("champion.example", Rule::FrozenNumber),
                ("robert.example", Rule::FixedSection),
                ("shop.example", Rule::FrozenNumber),
            ]
        );
        assert_eq!(Rule::FrozenNumber.severity(), Severity::Error);
    }

    #[test]
    fn flags_witnesses_titled_after_the_site_and_stale_sites() {
        let t = table(&[
            ("honorechampion.com", site(None)),
            ("old.example", site(None)),
        ]);
        let mut probe = ProbeReport::default();
        probe.sites.insert(
            "honorechampion.com".into(),
            probed(
                "https://www.honorechampion.com/",
                "2026-11-14",
                &["Honoré Champion", "Le Vampire dans la littérature"],
            ),
        );
        probe.sites.insert(
            "old.example".into(),
            probed("https://old.example/", "2026-01-02", &["Le Grand Voyage"]),
        );
        let findings = lint(&t, &probe, "2026-11-14");
        assert_eq!(
            rules(&findings),
            [
                ("honorechampion.com", Rule::SiteNameTitle),
                ("old.example", Rule::Stale),
            ]
        );
        // A check that passed since counts as a success.
        probe.checks.insert(
            "old.example".into(),
            RecipeCheck {
                checked_at: "2026-11-01".into(),
                status: CheckStatus::Works,
                unreachable_in_a_row: 0,
                note: None,
                failed: Vec::new(),
                tested_product: None,
                tested_search: None,
            },
        );
        assert_eq!(lint(&t, &probe, "2026-11-14").len(), 1);
    }

    #[test]
    fn flags_a_site_map_cut_at_the_size_cap() {
        use crate::modules::integrations::publisher_sites::table::{SitemapCheck, SitemapRecipe};
        let recipe = SitemapRecipe {
            url: "https://e.example/sitemap.xml".into(),
        };
        let mut shipped = site(None);
        shipped.sitemap = Some(recipe.clone());
        let t = table(&[("e.example", shipped)]);
        let mut p = probed("https://e.example/", "2026-11-14", &[]);
        p.sitemap = Some(SitemapCheck {
            recipe,
            found: 3,
            tried: 3,
            truncated: true,
        });
        let mut probe = ProbeReport::default();
        probe.sites.insert("e.example".into(), p);
        assert_eq!(
            rules(&lint(&t, &probe, "2026-11-14")),
            [("e.example", Rule::SitemapTruncated)]
        );
    }

    #[test]
    fn diff_lists_sites_recipes_and_prefixes_that_changed() {
        let old = table(&[
            ("a.example", site(Some("https://a.example/{isbn}"))),
            ("gone.example", site(Some("https://gone.example/{isbn}"))),
        ]);
        let mut new = table(&[
            ("a.example", site(Some("https://a.example/livre/{isbn}"))),
            ("new.example", site(Some("https://new.example/{isbn}"))),
        ]);
        let d = diff(&old, &new);
        assert_eq!(d.added_sites, ["new.example"]);
        assert_eq!(d.removed_sites, ["gone.example"]);
        assert_eq!(
            d.changed_recipes,
            [(
                "a.example".to_owned(),
                RecipeKind::Product,
                Some("https://a.example/{isbn}".to_owned()),
                Some("https://a.example/livre/{isbn}".to_owned()),
            )]
        );
        // Prefix 978201 went from gone.example to new.example.
        assert_eq!(
            d.changed_prefixes,
            [(
                "978201".to_owned(),
                vec!["gone.example".to_owned()],
                vec!["new.example".to_owned()],
            )]
        );
        new = old.clone();
        assert!(diff(&old, &new).is_empty());
    }
}
