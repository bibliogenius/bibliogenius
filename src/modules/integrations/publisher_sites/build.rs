//! Assembling the shipped table from Wikidata, the probe and the hand-made
//! corrections. Pure: the maintenance tool reads and writes the files.

use std::collections::BTreeMap;

use url::Url;

use super::page::bare_host;
use super::table::{
    MAX_UNREACHABLE_CHECKS, Overrides, PrefixEntry, PrefixPublisher, ProbeReport, ProbeStatus,
    PublisherTable, RecipeKind, SITEMAP_RECIPES, SiteOverride, SiteRecipes, TABLE_FORMAT,
    WikidataPublisher,
};

/// A site to probe, and who publishes there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteSource {
    pub home: String,
    /// Publisher names, as Wikidata or the corrections give them.
    pub publishers: Vec<String>,
    pub prefixes: Vec<String>,
    pub excluded: bool,
}

/// The key of a site: its host without `www.`, or `None` for an address we
/// cannot use.
pub fn site_key(home: &str) -> Option<String> {
    let url = Url::parse(home.trim()).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(())?;
    let key = bare_host(&url);
    (!key.is_empty()).then_some(key)
}

/// ISBN-13 prefix digits from a Wikidata value (`978-2-07`, or an old
/// ISBN-10 style `2-07`), or `None` when it is not a publisher prefix. The
/// shortest publisher prefix is a group and two digits (`978-2-07`, six
/// digits); anything shorter would claim a whole language group.
pub fn normalize_prefix(raw: &str) -> Option<String> {
    const MIN_DIGITS: usize = 6;
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    let prefix = if digits.starts_with("978") || digits.starts_with("979") {
        digits
    } else {
        format!("978{digits}")
    };
    (MIN_DIGITS..=12).contains(&prefix.len()).then_some(prefix)
}

/// Registrant ranges of the French-language ISBN groups, from the ISBN
/// agency's range table: (first seven digits after the group, registrant
/// length). A range runs up to the next one.
const RANGES_978_2: &[(u32, usize)] = &[
    (0, 2),
    (2_000_000, 3),
    (3_500_000, 5),
    (4_000_000, 3),
    (4_870_000, 6),
    (4_950_000, 3),
    (4_960_000, 4),
    (4_967_000, 5),
    (4_970_000, 3),
    (5_280_000, 4),
    (5_300_000, 3),
    (7_000_000, 4),
    (8_400_000, 5),
    (9_000_000, 6),
    (9_198_000, 5),
    (9_198_100, 6),
    (9_199_430, 7),
    (9_199_690, 6),
    (9_500_000, 7),
];
const RANGES_979_10: &[(u32, usize)] = &[
    (0, 2),
    (2_000_000, 3),
    (7_000_000, 4),
    (9_000_000, 5),
    (9_760_000, 6),
];

/// The publisher prefix of an ISBN-13 (`9782070360024` -> `978207`), or
/// `None` outside the French-language groups, whose ranges are the only ones
/// known here.
pub fn publisher_prefix(isbn: &str) -> Option<String> {
    let (group, ranges) = if isbn.starts_with("9782") {
        (4, RANGES_978_2)
    } else if isbn.starts_with("97910") {
        (5, RANGES_979_10)
    } else {
        return None;
    };
    if isbn.len() != 13 || !isbn.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest: u32 = isbn[group..group + 7].parse().ok()?;
    let (_, len) = ranges.iter().rev().find(|(start, _)| *start <= rest)?;
    Some(isbn[..group + len].to_owned())
}

/// Every prefix -> (publisher, site key) pair, Wikidata first, the
/// corrections replacing what Wikidata says for their exact prefix.
fn prefix_publishers(
    wikidata: &[WikidataPublisher],
    overrides: &Overrides,
) -> BTreeMap<String, Vec<(String, String, String)>> {
    let mut map: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    for publisher in wikidata {
        let Some((home, key)) = publisher
            .sites
            .iter()
            .find_map(|s| site_key(s).map(|k| (s.clone(), k)))
        else {
            continue;
        };
        for prefix in &publisher.prefixes {
            let entry = map.entry(prefix.clone()).or_default();
            if !entry
                .iter()
                .any(|(n, _, k)| *n == publisher.name && *k == key)
            {
                entry.push((publisher.name.clone(), home.clone(), key.clone()));
            }
        }
    }
    for (prefix, o) in &overrides.prefixes {
        if let Some(key) = site_key(&o.home) {
            map.insert(prefix.clone(), vec![(o.name.clone(), o.home.clone(), key)]);
        }
    }
    map
}

/// Prefix -> (publisher, site key) for the publishers Wikidata has no
/// prefix for: the prefixes of the books the probe found on their site.
fn read_prefixes(
    probe: &ProbeReport,
    overrides: &Overrides,
) -> BTreeMap<String, Vec<(String, String)>> {
    let mut map: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for extra in &overrides.publishers {
        let Some(key) = site_key(&extra.home) else {
            continue;
        };
        let Some(probed) = probe.sites.get(&key) else {
            continue;
        };
        for prefix in probed
            .witnesses
            .iter()
            .filter_map(|w| publisher_prefix(&w.isbn))
        {
            let entry = map.entry(prefix).or_default();
            if !entry.iter().any(|(_, k)| *k == key) {
                entry.push((extra.name.clone(), key.clone()));
            }
        }
    }
    map
}

/// The sites to probe, keyed like the table.
pub fn site_sources(
    wikidata: &[WikidataPublisher],
    overrides: &Overrides,
) -> BTreeMap<String, SiteSource> {
    let mut sources: BTreeMap<String, SiteSource> = BTreeMap::new();
    for (prefix, publishers) in prefix_publishers(wikidata, overrides) {
        for (name, home, key) in publishers {
            let source = sources.entry(key).or_insert_with(|| SiteSource {
                home,
                publishers: Vec::new(),
                prefixes: Vec::new(),
                excluded: false,
            });
            if !source.publishers.contains(&name) {
                source.publishers.push(name);
            }
            if !source.prefixes.contains(&prefix) {
                source.prefixes.push(prefix.clone());
            }
        }
    }
    for extra in &overrides.publishers {
        let Some(key) = site_key(&extra.home) else {
            continue;
        };
        let source = sources.entry(key).or_insert_with(|| SiteSource {
            home: extra.home.clone(),
            publishers: Vec::new(),
            prefixes: Vec::new(),
            excluded: false,
        });
        if !source.publishers.contains(&extra.name) {
            source.publishers.push(extra.name.clone());
        }
    }
    for (key, o) in &overrides.sites {
        let Some(source) = sources.get_mut(key) else {
            continue;
        };
        if let Some(home) = &o.home {
            source.home = home.clone();
        }
        source.excluded = o.excluded;
    }
    sources
}

/// Where a hand-written recipe stands against the site's last check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionState {
    /// Checked in its current form, and it worked.
    Verified,
    /// Checked in its current form, and it failed.
    Failed,
    /// Never checked, or changed since its last check.
    Unverified,
}

/// A hand-written recipe that is not [`CorrectionState::Verified`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCorrection {
    pub site: String,
    pub kind: RecipeKind,
    pub state: CorrectionState,
}

/// The state of `correction`'s `kind` recipe, `None` when it has none.
fn correction_state(
    probe: &ProbeReport,
    key: &str,
    correction: &SiteOverride,
    kind: RecipeKind,
) -> Option<CorrectionState> {
    let check = probe.checks.get(key);
    let same_as_tested = match kind {
        RecipeKind::Product => {
            correction.product.as_ref()?;
            check.and_then(|c| c.tested_product.as_ref()) == correction.product.as_ref()
        }
        RecipeKind::Search => {
            correction.search.as_ref()?;
            check.and_then(|c| c.tested_search.as_ref()) == correction.search.as_ref()
        }
        // Never written by hand.
        RecipeKind::Sitemap => return None,
    };
    Some(if !same_as_tested {
        CorrectionState::Unverified
    } else if check.is_some_and(|c| c.failed.contains(&kind)) {
        CorrectionState::Failed
    } else {
        CorrectionState::Verified
    })
}

/// Every hand-written recipe that [`build_table`] leaves out: failed at its
/// last check, or not checked in its current form.
pub fn pending_corrections(probe: &ProbeReport, overrides: &Overrides) -> Vec<PendingCorrection> {
    let mut out = Vec::new();
    for (key, correction) in &overrides.sites {
        if correction.excluded {
            continue;
        }
        for kind in [RecipeKind::Product, RecipeKind::Search] {
            match correction_state(probe, key, correction, kind) {
                None | Some(CorrectionState::Verified) => {}
                Some(state) => out.push(PendingCorrection {
                    site: key.clone(),
                    kind,
                    state,
                }),
            }
        }
    }
    out
}

/// The table to ship: every usable site, and the prefixes leading to them.
/// A site's recipe is its hand-written one once a check has found it works
/// in its current form, else the one of a successful probe. A site blocked
/// or offline at several checks in a row is left out. A prefix whose
/// publishers have no usable site is kept, empty, when a shorter prefix would
/// otherwise send its books to another publisher's site.
pub fn build_table(
    wikidata: &[WikidataPublisher],
    probe: &ProbeReport,
    overrides: &Overrides,
    today: &str,
) -> PublisherTable {
    assemble(wikidata, probe, overrides, today, false)
}

/// The recipes a check should try: hand-written ones whatever their state,
/// and sites left out for being unreachable, so they can come back.
pub fn candidate_table(
    wikidata: &[WikidataPublisher],
    probe: &ProbeReport,
    overrides: &Overrides,
    today: &str,
) -> PublisherTable {
    assemble(wikidata, probe, overrides, today, true)
}

fn assemble(
    wikidata: &[WikidataPublisher],
    probe: &ProbeReport,
    overrides: &Overrides,
    today: &str,
    candidates: bool,
) -> PublisherTable {
    let mut sites = BTreeMap::new();
    for (key, source) in site_sources(wikidata, overrides) {
        if source.excluded {
            continue;
        }
        // Blocked or offline at several checks in a row: gone for now.
        if !candidates
            && probe
                .checks
                .get(&key)
                .is_some_and(|c| c.unreachable_in_a_row >= MAX_UNREACHABLE_CHECKS)
        {
            continue;
        }
        let correction = overrides.sites.get(&key);
        let probed = probe
            .sites
            .get(&key)
            .filter(|p| p.status == ProbeStatus::Ok);
        let usable = |kind| {
            candidates
                || correction.and_then(|o| correction_state(probe, &key, o, kind))
                    == Some(CorrectionState::Verified)
        };
        let product = correction
            .and_then(|o| o.product.clone())
            .filter(|_| usable(RecipeKind::Product))
            .or_else(|| {
                probed
                    .and_then(|p| p.product.as_ref())
                    .map(|c| c.recipe.clone())
            });
        let search = correction
            .and_then(|o| o.search.clone())
            .filter(|_| usable(RecipeKind::Search))
            .or_else(|| {
                probed
                    .and_then(|p| p.search.as_ref())
                    .map(|c| c.recipe.clone())
            });
        let sitemap = probed
            .and_then(|p| p.sitemap.as_ref())
            .filter(|_| SITEMAP_RECIPES)
            .map(|c| c.recipe.clone());
        if product.is_some() || search.is_some() || sitemap.is_some() {
            sites.insert(
                key,
                SiteRecipes {
                    home: source.home,
                    product,
                    search,
                    sitemap,
                },
            );
        }
    }

    let mut by_prefix = prefix_publishers(wikidata, overrides);
    let mut read = read_prefixes(probe, overrides);
    for prefix in read.keys() {
        by_prefix.entry(prefix.clone()).or_default();
    }
    let mut prefixes: Vec<PrefixEntry> = Vec::new();
    for (prefix, publishers) in by_prefix {
        let mut usable: Vec<PrefixPublisher> = publishers
            .into_iter()
            .filter(|(_, _, key)| sites.contains_key(key))
            .map(|(name, _, site)| PrefixPublisher { name, site })
            .collect();
        // The default is the shortest name: the parent house rather than
        // one of its imprints ("Gallimard" before "Gallimard Jeunesse").
        // A publisher Wikidata gives no label for comes last.
        usable.sort_by_key(|p| {
            (
                p.name.trim().is_empty(),
                p.name.chars().count(),
                p.name.clone(),
            )
        });
        // A publisher known only by the books on its site comes after those
        // Wikidata or a correction names for the prefix.
        for (name, site) in read.remove(&prefix).unwrap_or_default() {
            if sites.contains_key(&site) && !usable.iter().any(|p| p.site == site) {
                usable.push(PrefixPublisher { name, site });
            }
        }
        prefixes.push(PrefixEntry {
            prefix,
            publishers: usable,
        });
    }
    // Drop an empty prefix unless a shorter, non-empty one covers it.
    let covered = |e: &PrefixEntry, all: &[PrefixEntry]| {
        all.iter().any(|o| {
            !o.publishers.is_empty()
                && o.prefix.len() < e.prefix.len()
                && e.prefix.starts_with(&o.prefix)
        })
    };
    let snapshot = prefixes.clone();
    prefixes.retain(|e| !e.publishers.is_empty() || covered(e, &snapshot));

    PublisherTable {
        format: TABLE_FORMAT,
        generated: today.to_owned(),
        prefixes,
        sites,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::integrations::publisher_sites::table::{
        PrefixPublisherOverride, ProductCheck, ProductRecipe, SiteOverride, SiteProbe,
    };
    use crate::modules::integrations::publisher_sites::text::Apostrophe;

    fn publisher(
        qid: &str,
        name: &str,
        prefixes: &[&str],
        site: Option<&str>,
    ) -> WikidataPublisher {
        WikidataPublisher {
            qid: qid.into(),
            name: name.into(),
            prefixes: prefixes.iter().map(|p| p.to_string()).collect(),
            sites: site.into_iter().map(str::to_owned).collect(),
        }
    }

    fn ok_probe(home: &str, template: &str) -> SiteProbe {
        SiteProbe {
            home: home.into(),
            status: ProbeStatus::Ok,
            probed_at: "2026-11-13".into(),
            note: None,
            witnesses: Vec::new(),
            product: Some(ProductCheck {
                recipe: ProductRecipe {
                    template: template.into(),
                    apostrophe: Apostrophe::Drop,
                    slug_ignored: true,
                },
                checked_on: 3,
                mismatches: 0,
            }),
            search: None,
            tried: Vec::new(),
            book_sitemap: None,
            sitemap: None,
        }
    }

    /// A conclusive check that tried `product` and saw `failed` fail.
    fn check(
        product: Option<ProductRecipe>,
        failed: Vec<RecipeKind>,
    ) -> crate::modules::integrations::publisher_sites::table::RecipeCheck {
        use crate::modules::integrations::publisher_sites::table::{CheckStatus, RecipeCheck};
        RecipeCheck {
            checked_at: "2026-11-13".into(),
            status: if failed.is_empty() {
                CheckStatus::Works
            } else {
                CheckStatus::Broken
            },
            unreachable_in_a_row: 0,
            note: None,
            failed,
            tested_product: product,
            tested_search: None,
        }
    }

    #[test]
    fn normalizes_wikidata_prefixes() {
        assert_eq!(normalize_prefix("978-2-07").as_deref(), Some("978207"));
        assert_eq!(
            normalize_prefix("979-10-90648").as_deref(),
            Some("9791090648")
        );
        assert_eq!(normalize_prefix("2-253").as_deref(), Some("9782253"));
        assert_eq!(normalize_prefix("2-07").as_deref(), Some("978207"));
        assert_eq!(normalize_prefix("2-02").as_deref(), Some("978202"));
        assert_eq!(normalize_prefix("978-2"), None, "a whole language group");
        assert_eq!(normalize_prefix("2"), None);
        assert_eq!(normalize_prefix("97"), None);
        assert_eq!(normalize_prefix("978-2-07-012345-2"), None, "a whole ISBN");
        assert_eq!(
            site_key("https://www.Gallimard.fr/catalogue").as_deref(),
            Some("gallimard.fr")
        );
        assert_eq!(site_key("mailto:x@y.fr"), None);
    }

    #[test]
    fn builds_the_table_from_the_three_sources() {
        let wikidata = vec![
            publisher(
                "Q1",
                "Gallimard Jeunesse",
                &["978207"],
                Some("https://www.gallimard-jeunesse.fr/"),
            ),
            publisher(
                "Q2",
                "Gallimard",
                &["978207"],
                Some("https://www.gallimard.fr/"),
            ),
            publisher(
                "Q3",
                "Albin Michel",
                &["978222", "9782253"],
                Some("https://www.albin-michel.fr/"),
            ),
            publisher("Q4", "Zulma", &["97828"], Some("https://www.zulma.fr/")),
            publisher("Q5", "No Site", &["97829"], None),
        ];
        let probe = ProbeReport {
            checks: BTreeMap::new(),
            sites: BTreeMap::from([
                (
                    "gallimard.fr".into(),
                    ok_probe(
                        "https://www.gallimard.fr/",
                        "https://www.gallimard.fr/catalogue/{slug}/{isbn}",
                    ),
                ),
                (
                    "gallimard-jeunesse.fr".into(),
                    ok_probe(
                        "https://www.gallimard-jeunesse.fr/",
                        "https://www.gallimard-jeunesse.fr/{isbn}",
                    ),
                ),
                (
                    "albin-michel.fr".into(),
                    ok_probe(
                        "https://www.albin-michel.fr/",
                        "https://www.albin-michel.fr/{slug}-{isbn}",
                    ),
                ),
                (
                    "zulma.fr".into(),
                    ok_probe("https://www.zulma.fr/", "https://www.zulma.fr/{isbn}"),
                ),
            ]),
        };
        let overrides = Overrides {
            // Wikidata ties 978-2-253 to Albin Michel; it is Le Livre de Poche.
            prefixes: BTreeMap::from([(
                "9782253".into(),
                PrefixPublisherOverride {
                    name: "Le Livre de Poche".into(),
                    home: "https://www.livredepoche.com/".into(),
                },
            )]),
            sites: BTreeMap::from([
                (
                    "livredepoche.com".into(),
                    SiteOverride {
                        product: Some(ProductRecipe {
                            template: "https://www.livredepoche.com/livre/{slug}-{isbn}/".into(),
                            apostrophe: Apostrophe::Drop,
                            slug_ignored: false,
                        }),
                        ..SiteOverride::default()
                    },
                ),
                (
                    "zulma.fr".into(),
                    SiteOverride {
                        excluded: true,
                        note: Some("asked to be left out".into()),
                        ..SiteOverride::default()
                    },
                ),
            ]),
            ..Overrides::default()
        };
        // Not checked yet: the hand-written Livre de Poche recipe stays out.
        let unchecked = build_table(&wikidata, &probe, &overrides, "2026-11-13");
        assert!(!unchecked.sites.contains_key("livredepoche.com"));
        assert_eq!(
            pending_corrections(&probe, &overrides),
            [PendingCorrection {
                site: "livredepoche.com".into(),
                kind: RecipeKind::Product,
                state: CorrectionState::Unverified,
            }]
        );
        assert!(
            candidate_table(&wikidata, &probe, &overrides, "d")
                .sites
                .contains_key("livredepoche.com")
        );

        let mut probe = probe;
        probe.checks.insert(
            "livredepoche.com".into(),
            check(
                overrides.sites["livredepoche.com"].product.clone(),
                Vec::new(),
            ),
        );
        let table = build_table(&wikidata, &probe, &overrides, "2026-11-13");

        let keys: Vec<&str> = table.sites.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "albin-michel.fr",
                "gallimard-jeunesse.fr",
                "gallimard.fr",
                "livredepoche.com"
            ]
        );

        let prefixes: Vec<&str> = table.prefixes.iter().map(|e| e.prefix.as_str()).collect();
        // Zulma excluded and No Site unusable: their prefixes go, nothing
        // shorter would send them elsewhere.
        assert_eq!(prefixes, ["978207", "978222", "9782253"]);
        let gallimard = &table.prefixes[0].publishers;
        assert_eq!(gallimard[0].name, "Gallimard");
        assert_eq!(gallimard[1].name, "Gallimard Jeunesse");
        assert_eq!(
            table.site_for("9782253000000", None).unwrap().0,
            "livredepoche.com"
        );
        assert_eq!(
            table.site_for("9782220000000", None).unwrap().0,
            "albin-michel.fr"
        );
    }

    #[test]
    fn a_site_unreachable_at_several_checks_leaves_the_table() {
        use crate::modules::integrations::publisher_sites::table::{CheckStatus, RecipeCheck};
        let wikidata = vec![publisher(
            "Q1",
            "Parent",
            &["97820"],
            Some("https://parent.example/"),
        )];
        let mut probe = ProbeReport {
            sites: BTreeMap::from([(
                "parent.example".into(),
                ok_probe("https://parent.example/", "https://parent.example/{isbn}"),
            )]),
            checks: BTreeMap::new(),
        };
        let check = |n| RecipeCheck {
            checked_at: "2026-11-13".into(),
            status: CheckStatus::Offline,
            unreachable_in_a_row: n,
            note: None,
            failed: Vec::new(),
            tested_product: None,
            tested_search: None,
        };
        probe
            .checks
            .insert("parent.example".into(), check(MAX_UNREACHABLE_CHECKS - 1));
        assert_eq!(
            build_table(&wikidata, &probe, &Overrides::default(), "d")
                .sites
                .len(),
            1
        );
        probe
            .checks
            .insert("parent.example".into(), check(MAX_UNREACHABLE_CHECKS));
        assert!(
            build_table(&wikidata, &probe, &Overrides::default(), "d")
                .sites
                .is_empty()
        );
    }

    #[test]
    fn a_correction_ships_only_once_checked_in_its_current_form() {
        let wikidata = vec![publisher(
            "Q1",
            "Parent",
            &["97820"],
            Some("https://parent.example/"),
        )];
        let mut probe = ProbeReport {
            sites: BTreeMap::from([(
                "parent.example".into(),
                ok_probe(
                    "https://parent.example/",
                    "https://parent.example/probed/{isbn}",
                ),
            )]),
            checks: BTreeMap::new(),
        };
        let overrides = Overrides {
            sites: BTreeMap::from([(
                "parent.example".into(),
                SiteOverride {
                    product: Some(ProductRecipe {
                        template: "https://parent.example/by-hand/{isbn}".into(),
                        apostrophe: Apostrophe::Drop,
                        slug_ignored: true,
                    }),
                    ..SiteOverride::default()
                },
            )]),
            ..Overrides::default()
        };
        let template = |p: &ProbeReport| {
            build_table(&wikidata, p, &overrides, "d").sites["parent.example"]
                .product
                .clone()
                .unwrap()
                .template
        };
        let by_hand = overrides.sites["parent.example"].product.clone();
        let state = |p: &ProbeReport| pending_corrections(p, &overrides).first().map(|c| c.state);

        // Never checked: the probed recipe ships.
        assert_eq!(template(&probe), "https://parent.example/probed/{isbn}");
        assert_eq!(state(&probe), Some(CorrectionState::Unverified));

        // Checked and working: the correction ships.
        probe
            .checks
            .insert("parent.example".into(), check(by_hand.clone(), Vec::new()));
        assert_eq!(template(&probe), "https://parent.example/by-hand/{isbn}");
        assert_eq!(state(&probe), None);

        // Checked and failing: the probed recipe again.
        probe.checks.insert(
            "parent.example".into(),
            check(by_hand.clone(), vec![RecipeKind::Product]),
        );
        assert_eq!(template(&probe), "https://parent.example/probed/{isbn}");
        assert_eq!(state(&probe), Some(CorrectionState::Failed));

        // Edited since a working check: unverified until checked again.
        let mut edited = by_hand.unwrap();
        edited.template = "https://parent.example/elsewhere/{isbn}".into();
        probe
            .checks
            .insert("parent.example".into(), check(Some(edited), Vec::new()));
        assert_eq!(template(&probe), "https://parent.example/probed/{isbn}");
        assert_eq!(state(&probe), Some(CorrectionState::Unverified));
    }

    #[test]
    fn keeps_an_empty_prefix_that_a_shorter_one_would_capture() {
        let wikidata = vec![
            publisher("Q1", "Parent", &["97820"], Some("https://parent.example/")),
            publisher(
                "Q2",
                "Imprint",
                &["978201"],
                Some("https://imprint.example/"),
            ),
        ];
        let probe = ProbeReport {
            checks: BTreeMap::new(),
            sites: BTreeMap::from([(
                "parent.example".into(),
                ok_probe("https://parent.example/", "https://parent.example/{isbn}"),
            )]),
        };
        let table = build_table(&wikidata, &probe, &Overrides::default(), "2026-11-13");
        assert_eq!(table.prefixes.len(), 2);
        assert!(
            table.site_for("9782010000000", None).is_none(),
            "the imprint is known, without a usable site"
        );
        assert!(table.site_for("9782020000000", None).is_some());
    }

    #[test]
    fn reads_a_publisher_prefix_from_an_isbn() {
        assert_eq!(publisher_prefix("9782070360024").as_deref(), Some("978207"));
        assert_eq!(
            publisher_prefix("9782864329404").as_deref(),
            Some("978286432")
        );
        assert_eq!(
            publisher_prefix("9782919421589").as_deref(),
            Some("9782919421")
        );
        assert_eq!(
            publisher_prefix("9791032900123").as_deref(),
            Some("97910329")
        );
        assert_eq!(
            publisher_prefix("9791097160012").as_deref(),
            Some("9791097160")
        );
        // Another language group, or not an ISBN-13.
        assert_eq!(publisher_prefix("9780141439518"), None);
        assert_eq!(publisher_prefix("978207036002X"), None);
        assert_eq!(publisher_prefix("978207"), None);
    }

    #[test]
    fn a_publisher_unknown_to_wikidata_gets_the_prefixes_of_its_books() {
        use crate::modules::integrations::publisher_sites::table::{ExtraPublisher, Witness};
        let witness = |isbn: &str| Witness {
            isbn: isbn.into(),
            title: "A Book".into(),
            authors: String::new(),
            url: None,
        };
        let wikidata = vec![publisher(
            "Q1",
            "Gallimard",
            &["978207"],
            Some("https://www.gallimard.fr/"),
        )];
        let mut verdier = ok_probe("https://verdier.example/", "https://verdier.example/{isbn}");
        // Two books of its own, one of a publisher Wikidata already knows.
        verdier.witnesses = vec![
            witness("9782864329404"),
            witness("9782864320012"),
            witness("9782070360024"),
        ];
        let probe = ProbeReport {
            checks: BTreeMap::new(),
            sites: BTreeMap::from([
                ("verdier.example".into(), verdier),
                (
                    "gallimard.fr".into(),
                    ok_probe(
                        "https://www.gallimard.fr/",
                        "https://www.gallimard.fr/{isbn}",
                    ),
                ),
            ]),
        };
        let overrides = Overrides {
            publishers: vec![ExtraPublisher {
                name: "Verdier".into(),
                home: "https://verdier.example/".into(),
            }],
            ..Overrides::default()
        };
        assert!(site_sources(&wikidata, &overrides).contains_key("verdier.example"));
        let table = build_table(&wikidata, &probe, &overrides, "d");
        let owner = |prefix: &str| {
            table
                .prefixes
                .iter()
                .find(|e| e.prefix == prefix)
                .map(|e| e.publishers[0].site.clone())
        };
        assert_eq!(owner("978286432").as_deref(), Some("verdier.example"));
        assert_eq!(owner("978207").as_deref(), Some("gallimard.fr"));
        assert_eq!(table.prefixes.len(), 2);
        // The Gallimard book seen at Verdier adds its site after Gallimard's.
        let shared: Vec<&str> = table.prefixes[0]
            .publishers
            .iter()
            .map(|p| p.site.as_str())
            .collect();
        assert_eq!(shared, ["gallimard.fr", "verdier.example"]);
    }
}
