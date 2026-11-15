//! The data model of the publisher site table and of the files the
//! maintenance tool keeps next to it.
//!
//! - `table.json` ([`PublisherTable`]) ships with the app: ISBN prefixes,
//!   their publishers, and the recipe of each usable site. Nothing else.
//! - `probe.json` ([`ProbeReport`]) is the tool's full account of every site
//!   it looked at, for developers; it does not ship.
//! - `overrides.json` ([`Overrides`]) holds corrections made by hand, applied
//!   on top of Wikidata and of the probe at every refresh.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::text::Apostrophe;

/// Version of the `table.json` format; bumped on any incompatible change.
pub const TABLE_FORMAT: u32 = 1;

/// How to build a book page's address from its ISBN, title and author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductRecipe {
    /// Address with `{isbn}` or `{isbn_hyphens}` (`978-2-8180-5511-3`), and
    /// optionally `{slug}` (from the title) and `{author}` (from the first
    /// author's name).
    pub template: String,
    pub apostrophe: Apostrophe,
    /// The server finds the book by its ISBN whatever the slug: one try is
    /// enough.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub slug_ignored: bool,
}

/// How to search a site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRecipe {
    /// For a GET search, the address with `{q}`; for a POST, the form's action.
    pub url: String,
    /// The form fields of a POST search, the query's value being `{q}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_fields: Option<Vec<(String, String)>>,
    /// The search finds books by ISBN; otherwise only by title.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub by_isbn: bool,
}

/// Whether site map recipes ([`SitemapRecipe`]) are in use. Set to `false`,
/// the probe looks for none and `build` ships none, whatever `probe.json`
/// holds.
pub const SITEMAP_RECIPES: bool = true;

/// How to find a book's page in the site map that lists the site's books:
/// the address carrying the ISBN, else the one named after the title (see
/// `page::sitemap_book_links`). The ISBN the page shows decides, as for the
/// other recipes. It costs the download of the map, to be kept for the next
/// books of the same site: the recipe of last resort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitemapRecipe {
    /// Address of the site map (not of an index of site maps).
    pub url: String,
}

/// What the app needs to look for a book on one site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteRecipes {
    /// The site's home page.
    pub home: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<ProductRecipe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<SearchRecipe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sitemap: Option<SitemapRecipe>,
}

/// A publisher an ISBN prefix belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixPublisher {
    pub name: String,
    /// Key of its site in [`PublisherTable::sites`].
    pub site: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixEntry {
    /// ISBN-13 prefix, digits only (`"978207"`).
    pub prefix: String,
    /// Publishers sharing the prefix (an imprint and its parent), the one to
    /// use when the book names none of them first.
    pub publishers: Vec<PrefixPublisher>,
}

/// The table shipped with the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublisherTable {
    pub format: u32,
    /// Date of the refresh that wrote it (`YYYY-MM-DD`).
    pub generated: String,
    /// Sorted by prefix.
    pub prefixes: Vec<PrefixEntry>,
    /// Keyed by the site's host without `www.`.
    pub sites: BTreeMap<String, SiteRecipes>,
}

/// How many sites [`PublisherTable::sites_for`] gives for one book.
pub const MAX_SITES_PER_BOOK: usize = 3;

impl PublisherTable {
    /// The site to look on first for a book: see [`Self::sites_for`].
    pub fn site_for(&self, isbn13: &str, publisher: Option<&str>) -> Option<(&str, &SiteRecipes)> {
        self.sites_for(isbn13, publisher).into_iter().next()
    }

    /// The sites to look on for a book, best first, [`MAX_SITES_PER_BOOK`]
    /// at most: those of the longest prefix of `isbn13`, starting with the
    /// publisher whose name matches the book's `publisher` field, then in
    /// the prefix's order. Several publishers can share a prefix, and the
    /// book's publisher field is often missing or spelled another way.
    pub fn sites_for(&self, isbn13: &str, publisher: Option<&str>) -> Vec<(&str, &SiteRecipes)> {
        let Some(entry) = self
            .prefixes
            .iter()
            .filter(|e| isbn13.starts_with(&e.prefix))
            .max_by_key(|e| e.prefix.len())
        else {
            return Vec::new();
        };
        let wanted = publisher.map(compact).filter(|p| !p.is_empty());
        // The exact name first, then the longest name that one contains
        // the other ("Gallimard Jeunesse" over "Gallimard").
        let named = wanted.and_then(|w| {
            entry
                .publishers
                .iter()
                .map(|p| (compact(&p.name), p))
                .filter(|(name, _)| !name.is_empty() && (name.contains(&w) || w.contains(name)))
                .max_by_key(|(name, _)| (*name == w, name.len()))
                .map(|(_, p)| p)
        });
        let mut out: Vec<(&str, &SiteRecipes)> = Vec::new();
        for p in named.into_iter().chain(&entry.publishers) {
            if out.len() == MAX_SITES_PER_BOOK {
                break;
            }
            if let Some((key, site)) = self.sites.get_key_value(&p.site)
                && !out.iter().any(|(k, _)| *k == key.as_str())
            {
                out.push((key.as_str(), site));
            }
        }
        out
    }
}

/// A name reduced to its lowercase letters and digits, for comparison.
fn compact(s: &str) -> String {
    super::text::fold(s).replace(' ', "")
}

/// A book the probe tested a site's recipes on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    pub isbn: String,
    pub title: String,
    #[serde(default)]
    pub authors: String,
    /// The book's page, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Outcome of probing one site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// A product pattern or a search was validated.
    Ok,
    /// No book found on the home page to test recipes on.
    NoWitness,
    /// Witnesses, but no recipe found them.
    NoRecipe,
    /// The search answers the same page whatever the query: results are
    /// drawn by JavaScript.
    JsResults,
    /// The search is out of order (error message instead of a page).
    SearchDown,
    /// The site turns scripts away (403, firewall, rate limit).
    Blocked,
    /// robots.txt forbids us the site (or the page the probe starts from).
    RobotsForbid,
    /// Unreachable, or an HTTP error on the home page.
    Offline,
}

/// The product pattern the probe settled on, and how well it held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductCheck {
    pub recipe: ProductRecipe,
    /// Witnesses whose address the pattern rebuilds exactly.
    pub checked_on: usize,
    pub mismatches: usize,
}

/// A search recipe and how it did on the witnesses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchCheck {
    pub recipe: SearchRecipe,
    pub found_by_isbn: usize,
    pub found_by_title: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub js_results: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub broken: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A site map recipe and how it did on the witnesses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitemapCheck {
    pub recipe: SitemapRecipe,
    /// Witnesses whose page the map leads to.
    pub found: usize,
    /// Witnesses it was tried on.
    pub tried: usize,
    /// The map was cut at the size cap: books listed after the cut cannot be
    /// found through it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Everything the probe learnt about one site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteProbe {
    pub home: String,
    pub status: ProbeStatus,
    /// Date of the probe (`YYYY-MM-DD`).
    pub probed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default)]
    pub witnesses: Vec<Witness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<ProductCheck>,
    /// The search kept, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<SearchCheck>,
    /// Every search tried, in order, for whoever wants to improve on it.
    #[serde(default)]
    pub tried: Vec<SearchCheck>,
    /// The site map listing the witnesses' pages, hence the site's books.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book_sitemap: Option<String>,
    /// That map as a recipe, when it leads to enough of the witnesses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sitemap: Option<SitemapCheck>,
}

/// Outcome of replaying a site's recipes on its witnesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Every recipe found at least one witness still in the catalogue.
    Works,
    /// A probed recipe failed: the site was probed again.
    Reprobed,
    /// A recipe written by hand failed: left as it is, for a person to look at.
    Broken,
    /// No witness is in the catalogue any more, even after looking for new ones.
    NoWitness,
    /// The site turns scripts away.
    Blocked,
    /// The site does not answer.
    Offline,
}

/// Which of a site's recipes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeKind {
    Product,
    Search,
    Sitemap,
}

/// The last check of one site's recipes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeCheck {
    /// `YYYY-MM-DD`.
    pub checked_at: String,
    pub status: CheckStatus,
    /// Checks in a row that ended blocked or offline. A site leaves the
    /// table only after [`MAX_UNREACHABLE_CHECKS`] of them: one failure may
    /// be a bad day.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unreachable_in_a_row: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The recipes, as shipped in the table, that failed. A hand-written one
    /// listed here is left out of the table until a check passes again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<RecipeKind>,
    /// The recipes the last conclusive check tried, as they were then: a
    /// hand-written recipe that differs from these has not been verified in
    /// its current form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tested_product: Option<ProductRecipe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tested_search: Option<SearchRecipe>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Checks in a row a site may fail (blocked, offline) and stay in the table.
pub const MAX_UNREACHABLE_CHECKS: u32 = 3;

/// The probe's full account, keyed like [`PublisherTable::sites`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeReport {
    pub sites: BTreeMap<String, SiteProbe>,
    /// The last check of each site of the table.
    #[serde(default)]
    pub checks: BTreeMap<String, RecipeCheck>,
}

impl ProbeReport {
    /// The report as `probe.json` keeps it: valid JSON, one site per line,
    /// sites in key order. A probe or a check rewrites only the lines of the
    /// sites it looked at, and the history shows which ones changed.
    pub fn to_json_by_site(&self) -> serde_json::Result<String> {
        fn section<V: Serialize>(
            out: &mut String,
            name: &str,
            map: &BTreeMap<String, V>,
        ) -> serde_json::Result<()> {
            out.push_str(&format!("{}: {{\n", serde_json::to_string(name)?));
            let lines: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    Ok(format!(
                        "{}: {}",
                        serde_json::to_string(k)?,
                        serde_json::to_string(v)?
                    ))
                })
                .collect::<serde_json::Result<_>>()?;
            out.push_str(&lines.join(",\n"));
            out.push_str("\n}");
            Ok(())
        }
        let mut out = String::from("{\n");
        section(&mut out, "sites", &self.sites)?;
        out.push_str(",\n");
        section(&mut out, "checks", &self.checks)?;
        out.push_str("\n}\n");
        Ok(out)
    }
}

/// One publisher as Wikidata knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikidataPublisher {
    /// Wikidata item id (`Q...`).
    pub qid: String,
    pub name: String,
    /// ISBN prefixes, digits only.
    pub prefixes: Vec<String>,
    /// Official websites (P856), possibly none.
    #[serde(default)]
    pub sites: Vec<String>,
}

/// Corrections made by hand. Each wins over Wikidata and over the probe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overrides {
    /// ISBN prefix (digits only) -> its publisher, replacing whatever
    /// Wikidata says for that exact prefix (wrong or missing publisher).
    #[serde(default)]
    pub prefixes: BTreeMap<String, PrefixPublisherOverride>,
    /// Site key (host without `www.`) -> corrections for that site.
    #[serde(default)]
    pub sites: BTreeMap<String, SiteOverride>,
    /// Publishers Wikidata lacks, or gives no usable site for. They are
    /// added to their prefixes, after the publishers Wikidata names there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publishers: Vec<ExtraPublisher>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraPublisher {
    pub name: String,
    /// The publisher's home page, or the site its books are sold on.
    pub home: String,
    /// Its ISBN prefixes (digits only). Empty: they are read from the books
    /// the probe finds on the site, which only suits a site of its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefixes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixPublisherOverride {
    pub name: String,
    /// The publisher's home page.
    pub home: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteOverride {
    /// Why the correction was made, and by whom if it matters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Never probe nor list this site (publisher's request, wrong site).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub excluded: bool,
    /// Home page to probe instead of the one Wikidata gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// Recipes to use whatever the probe finds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<ProductRecipe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<SearchRecipe>,
    /// Books to test recipes on, added to those of the home page.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub witnesses: Vec<Witness>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(home: &str) -> SiteRecipes {
        SiteRecipes {
            home: home.into(),
            product: None,
            search: None,
            sitemap: None,
        }
    }

    fn table() -> PublisherTable {
        let publisher = |name: &str, site: &str| PrefixPublisher {
            name: name.into(),
            site: site.into(),
        };
        PublisherTable {
            format: TABLE_FORMAT,
            generated: "2026-11-13".into(),
            prefixes: vec![
                PrefixEntry {
                    prefix: "97820".into(),
                    publishers: vec![publisher("Somebody", "somebody.example")],
                },
                PrefixEntry {
                    prefix: "978207".into(),
                    publishers: vec![
                        publisher("Gallimard", "gallimard.fr"),
                        publisher("Gallimard Jeunesse", "gallimard-jeunesse.fr"),
                    ],
                },
            ],
            sites: BTreeMap::from([
                ("gallimard.fr".into(), site("https://www.gallimard.fr/")),
                (
                    "gallimard-jeunesse.fr".into(),
                    site("https://www.gallimard-jeunesse.fr/"),
                ),
                ("somebody.example".into(), site("https://somebody.example/")),
            ]),
        }
    }

    #[test]
    fn the_longest_prefix_wins() {
        let t = table();
        assert_eq!(t.site_for("9782070000000", None).unwrap().0, "gallimard.fr");
        assert_eq!(
            t.site_for("9782010000000", None).unwrap().0,
            "somebody.example"
        );
        assert!(t.site_for("9791000000000", None).is_none());
    }

    #[test]
    fn the_book_publisher_picks_among_a_shared_prefix() {
        let t = table();
        let pick = |p: &str| t.site_for("9782070000000", Some(p)).unwrap().0;
        assert_eq!(pick("Gallimard Jeunesse"), "gallimard-jeunesse.fr");
        assert_eq!(pick("GALLIMARD"), "gallimard.fr");
        assert_eq!(pick("Folio"), "gallimard.fr");
        // Every site of the prefix, the named one first, each once.
        let all = |p: Option<&str>| -> Vec<&str> {
            t.sites_for("9782070000000", p)
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        assert_eq!(
            all(Some("Gallimard Jeunesse")),
            ["gallimard-jeunesse.fr", "gallimard.fr"]
        );
        assert_eq!(all(None), ["gallimard.fr", "gallimard-jeunesse.fr"]);
        assert!(t.sites_for("9791000000000", None).is_empty());
    }

    #[test]
    fn the_table_round_trips_through_json() {
        let t = table();
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<PublisherTable>(&json).unwrap(), t);
    }

    #[test]
    fn the_probe_report_is_written_one_site_per_line() {
        let probe = |home: &str| SiteProbe {
            home: home.into(),
            status: ProbeStatus::Ok,
            probed_at: "2026-11-15".into(),
            note: None,
            witnesses: Vec::new(),
            product: None,
            search: None,
            tried: Vec::new(),
            book_sitemap: None,
            sitemap: None,
        };
        let report = ProbeReport {
            sites: BTreeMap::from([
                ("b.example".into(), probe("https://b.example/")),
                ("a.example".into(), probe("https://a.example/")),
            ]),
            checks: BTreeMap::new(),
        };
        let text = report.to_json_by_site().unwrap();
        assert_eq!(serde_json::from_str::<ProbeReport>(&text).unwrap(), report);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[2].starts_with("\"a.example\": {"), "{text}");
        assert!(lines[3].starts_with("\"b.example\": {"), "{text}");
        assert_eq!(lines.len(), 9, "{text}");
        // The committed file is in the form the tool writes: the next probe
        // rewrites only the sites it looks at.
        let committed = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/publisher_sites/probe.json"
        ));
        let read: ProbeReport = serde_json::from_str(committed).unwrap();
        assert!(read.to_json_by_site().unwrap() == committed);
        // Nothing yet: still valid JSON.
        let empty = ProbeReport::default().to_json_by_site().unwrap();
        assert_eq!(
            serde_json::from_str::<ProbeReport>(&empty).unwrap(),
            ProbeReport::default()
        );
    }
}
