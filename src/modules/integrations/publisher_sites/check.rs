//! Replaying a site's recipes on its witnesses, to tell a recipe that still
//! works from one the site's redesign broke, at the cost of a few requests
//! instead of a whole probe.
//!
//! A witness first proves it is still in the catalogue (its page, when known,
//! still shows its ISBN); a book the publisher dropped says nothing about the
//! recipe. Each recipe is then tried on the live witnesses the way the app
//! will use it, and holds when it reaches at least one of them. As in the
//! probe, a search is first run on a nonsense query: a witness its answer
//! already links (a "not found" page that lists the latest books) cannot
//! prove the search works.

use url::Url;

use super::fetch::{FetchError, Page, SiteFetcher};
use super::page::{page_matches, result_links};
use super::probe::{CONTROL_QUERY, product_urls, redirected_to_book, run_search};
use super::table::{
    CheckStatus, ProbeStatus, ProductRecipe, RecipeCheck, RecipeKind, SearchRecipe, SiteOverride,
    SiteProbe, SiteRecipes, Witness,
};

/// Witnesses each recipe is judged on, at most.
const MAX_TRIES_PER_RECIPE: usize = 3;
/// Witnesses looked at, at most: some cannot judge a search (its "no result"
/// page already links them), so more than the tries above may be needed.
const MAX_LIVE_WITNESSES: usize = 6;
/// Product addresses tried per witness (title and author orders).
const MAX_PRODUCT_TRIES: usize = 6;
/// Result links followed per search.
const MAX_RESULT_LINKS: usize = 2;

/// What a check found. `None`: the site has no such recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Witnesses still in the catalogue that the recipes were tried on.
    pub live_witnesses: usize,
    pub product_works: Option<bool>,
    pub search_works: Option<bool>,
}

impl CheckResult {
    /// The recipes that reached no witness.
    pub fn failed(&self) -> Vec<RecipeKind> {
        [
            (RecipeKind::Product, self.product_works),
            (RecipeKind::Search, self.search_works),
        ]
        .into_iter()
        .filter(|(_, works)| *works == Some(false))
        .map(|(kind, _)| kind)
        .collect()
    }

    /// Whether a failed recipe is the probe's (the site is then probed
    /// again) rather than a hand-written one (left for a person to fix).
    pub fn needs_reprobe(&self, correction: Option<&SiteOverride>) -> bool {
        self.failed().into_iter().any(|kind| match kind {
            RecipeKind::Product => correction.is_none_or(|o| o.product.is_none()),
            RecipeKind::Search => correction.is_none_or(|o| o.search.is_none()),
        })
    }

    /// Every recipe the site has reached a witness.
    pub fn all_work(&self) -> bool {
        self.live_witnesses > 0
            && self.product_works != Some(false)
            && self.search_works != Some(false)
    }
}

/// Whether a failure says the site is unusable now (rather than this page
/// missing): it ends the check. A robots.txt refusal is not one: the page is
/// out of bounds, so the recipe that leads there no longer serves, and the
/// probe that follows tells whether the whole site is.
fn site_down(e: &FetchError) -> bool {
    matches!(e, FetchError::Blocked(_) | FetchError::Network)
}

/// Whether the witness is still sold: its page, when known, still shows its
/// ISBN. A witness given without a page is taken as live, and so is one whose
/// page robots.txt now forbids: the refusal says nothing of the book, and
/// taken as gone it would leave the recipe untried, hence never failing,
/// when that same path is the recipe's.
async fn is_live(fetcher: &dyn SiteFetcher, w: &Witness) -> Result<bool, FetchError> {
    let Some(url) = w.url.as_deref().and_then(|u| Url::parse(u).ok()) else {
        return Ok(true);
    };
    match fetcher.get(&url).await {
        Ok(page) => Ok(page_matches(&page.html, &w.isbn, "").is_some()),
        Err(FetchError::Disallowed) => Ok(true),
        Err(e) if site_down(&e) => Err(e),
        Err(_) => Ok(false),
    }
}

async fn product_reaches(
    fetcher: &dyn SiteFetcher,
    recipe: &ProductRecipe,
    w: &Witness,
) -> Result<bool, FetchError> {
    for raw in product_urls(recipe, &w.isbn, &w.title, &w.authors)
        .into_iter()
        .take(MAX_PRODUCT_TRIES)
    {
        let Ok(url) = Url::parse(&raw) else { continue };
        match fetcher.get(&url).await {
            Ok(page) if page_matches(&page.html, &w.isbn, "").is_some() => return Ok(true),
            Ok(_) => {}
            Err(e) if site_down(&e) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(false)
}

/// The answer of a search to the nonsense query, `None` when it fails (a
/// search that cannot answer it does not work).
async fn control_page(
    fetcher: &dyn SiteFetcher,
    recipe: &SearchRecipe,
) -> Result<Option<Page>, FetchError> {
    match run_search(fetcher, recipe, CONTROL_QUERY).await {
        Ok((page, _)) => Ok(Some(page)),
        Err(e) if site_down(&e) => Err(e),
        Err(_) => Ok(None),
    }
}

async fn search_reaches(
    fetcher: &dyn SiteFetcher,
    recipe: &SearchRecipe,
    w: &Witness,
) -> Result<bool, FetchError> {
    let query = if recipe.by_isbn { &w.isbn } else { &w.title };
    let (page, asked) = match run_search(fetcher, recipe, query).await {
        Ok(answer) => answer,
        Err(e) if site_down(&e) => return Err(e),
        Err(_) => return Ok(false),
    };
    if redirected_to_book(&page, &asked, &w.isbn) {
        return Ok(true);
    }
    for link in result_links(&page.html, &page.url, &w.isbn, &w.title, &w.authors)
        .into_iter()
        .take(MAX_RESULT_LINKS)
    {
        match fetcher.get(&link).await {
            Ok(p) if page_matches(&p.html, &w.isbn, &w.title).is_some() => return Ok(true),
            Ok(_) => {}
            Err(e) if site_down(&e) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(false)
}

/// Try `recipes` on the live ones among `witnesses`. An `Err` means the site
/// is blocked or unreachable, which says nothing about the recipes.
pub async fn check_recipes(
    fetcher: &dyn SiteFetcher,
    recipes: &SiteRecipes,
    witnesses: &[Witness],
) -> Result<CheckResult, FetchError> {
    let mut result = CheckResult {
        live_witnesses: 0,
        product_works: recipes.product.as_ref().map(|_| false),
        search_works: recipes.search.as_ref().map(|_| false),
    };
    let control = match &recipes.search {
        Some(recipe) => control_page(fetcher, recipe).await?,
        None => None,
    };
    let (mut product_tries, mut search_tries) = (0, 0);
    for w in witnesses {
        let product_pending =
            result.product_works == Some(false) && product_tries < MAX_TRIES_PER_RECIPE;
        let search_pending =
            result.search_works == Some(false) && search_tries < MAX_TRIES_PER_RECIPE;
        if (!product_pending && !search_pending) || result.live_witnesses >= MAX_LIVE_WITNESSES {
            break;
        }
        if !is_live(fetcher, w).await? {
            continue;
        }
        result.live_witnesses += 1;
        if let (true, Some(recipe)) = (product_pending, &recipes.product) {
            product_tries += 1;
            result.product_works = Some(product_reaches(fetcher, recipe, w).await?);
        }
        // A witness the nonsense query already links cannot judge the search.
        let judged = control.as_ref().is_some_and(|c| {
            result_links(&c.html, &c.url, &w.isbn, &w.title, &w.authors).is_empty()
        });
        if let (true, true, Some(recipe)) = (search_pending, judged, &recipes.search) {
            search_tries += 1;
            result.search_works = Some(search_reaches(fetcher, recipe, w).await?);
        }
    }
    Ok(result)
}

/// What the check of a site whose probed recipe failed concludes from the
/// new probe. Blocked or offline says nothing about the recipes: the site is
/// counted unreachable and the previous probe, whose other recipe may well
/// work, is kept. Any other outcome replaces it.
pub fn after_reprobe(new: &SiteProbe) -> CheckStatus {
    match new.status {
        ProbeStatus::Blocked => CheckStatus::Blocked,
        ProbeStatus::Offline => CheckStatus::Offline,
        _ => CheckStatus::Reprobed,
    }
}

/// The record of a check of `tested`, given the site's previous one.
///
/// Blocked or offline counts one more day in a row the site could not be
/// reached; any other outcome resets the count. A check that could not try
/// the recipes (blocked, offline, no witness) proves nothing about them, so
/// the last conclusive result (what failed, and in which form the recipes
/// were tried) stands.
pub fn next_record(
    previous: Option<&RecipeCheck>,
    checked_at: &str,
    status: CheckStatus,
    note: Option<String>,
    failed: Vec<RecipeKind>,
    tested: &SiteRecipes,
) -> RecipeCheck {
    // Counted once a day: several runs on one bad day are one failure.
    let unreachable_in_a_row = if matches!(status, CheckStatus::Blocked | CheckStatus::Offline) {
        match previous {
            Some(p) if p.checked_at == checked_at && p.unreachable_in_a_row > 0 => {
                p.unreachable_in_a_row
            }
            Some(p) => p.unreachable_in_a_row + 1,
            None => 1,
        }
    } else {
        0
    };
    let conclusive = matches!(
        status,
        CheckStatus::Works | CheckStatus::Broken | CheckStatus::Reprobed
    );
    let (failed, tested_product, tested_search) = if conclusive {
        (failed, tested.product.clone(), tested.search.clone())
    } else {
        previous
            .map(|c| {
                (
                    c.failed.clone(),
                    c.tested_product.clone(),
                    c.tested_search.clone(),
                )
            })
            .unwrap_or_default()
    };
    RecipeCheck {
        checked_at: checked_at.to_owned(),
        status,
        unreachable_in_a_row,
        note,
        failed,
        tested_product,
        tested_search,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::integrations::publisher_sites::test_site::{FakeSite, book_page, padding};
    use crate::modules::integrations::publisher_sites::text::Apostrophe;

    const ISBN: &str = "9782000000011";
    const BOOK: &str = "https://editions.example/livre/le-grand-voyage-9782000000011";

    fn witness(url: Option<&str>) -> Witness {
        Witness {
            isbn: ISBN.into(),
            title: "Le Grand Voyage".into(),
            authors: String::new(),
            url: url.map(str::to_owned),
        }
    }

    fn product() -> ProductRecipe {
        ProductRecipe {
            template: "https://editions.example/livre/{slug}-{isbn}".into(),
            apostrophe: Apostrophe::Drop,
            slug_ignored: false,
        }
    }

    fn recipes(product: Option<ProductRecipe>, search: Option<SearchRecipe>) -> SiteRecipes {
        SiteRecipes {
            home: "https://editions.example/".into(),
            product,
            search,
        }
    }

    #[tokio::test]
    async fn a_working_product_recipe_holds() {
        let site = FakeSite::default().page(BOOK, &book_page(ISBN, "Le Grand Voyage"));
        let r = check_recipes(
            &site,
            &recipes(Some(product()), None),
            &[witness(Some(BOOK))],
        )
        .await
        .unwrap();
        assert_eq!(r.live_witnesses, 1);
        assert_eq!(r.product_works, Some(true));
        assert!(r.all_work());
    }

    #[tokio::test]
    async fn a_moved_book_page_breaks_the_product_recipe() {
        // The witness lives at its new address; the recipe builds the old one.
        let moved = "https://editions.example/ouvrage/9782000000011";
        let site = FakeSite::default().page(moved, &book_page(ISBN, "Le Grand Voyage"));
        let r = check_recipes(
            &site,
            &recipes(Some(product()), None),
            &[witness(Some(moved))],
        )
        .await
        .unwrap();
        assert_eq!(r.product_works, Some(false));
        assert!(!r.all_work());
    }

    #[tokio::test]
    async fn a_witness_gone_from_the_catalogue_proves_nothing() {
        let site = FakeSite::default();
        let r = check_recipes(
            &site,
            &recipes(Some(product()), None),
            &[witness(Some(BOOK))],
        )
        .await
        .unwrap();
        assert_eq!(r.live_witnesses, 0);
        assert!(!r.all_work());
        assert_eq!(
            site.asked.lock().unwrap().len(),
            1,
            "only the witness page is asked"
        );
    }

    #[tokio::test]
    async fn a_search_recipe_follows_its_result_link() {
        let search = SearchRecipe {
            url: "https://editions.example/recherche?q={q}".into(),
            post_fields: None,
            by_isbn: false,
        };
        let site = FakeSite::default()
            .page(
                &format!("https://editions.example/recherche?q={CONTROL_QUERY}"),
                &padding(),
            )
            .page(
                "https://editions.example/recherche?q=Le%20Grand%20Voyage",
                &format!(
                    r#"{}<main><a href="/ouvrage/le-grand-voyage">Le Grand Voyage</a></main>"#,
                    padding()
                ),
            )
            .page(
                "https://editions.example/ouvrage/le-grand-voyage",
                &book_page(ISBN, "Le Grand Voyage"),
            );
        let r = check_recipes(&site, &recipes(None, Some(search)), &[witness(None)])
            .await
            .unwrap();
        assert_eq!(r.search_works, Some(true));
    }

    #[tokio::test]
    async fn a_not_found_page_listing_the_witness_is_not_a_working_search() {
        // A wrong search address answers 200 with the latest books, whatever
        // the query.
        let search = SearchRecipe {
            url: "https://editions.example/introuvable?q={q}".into(),
            post_fields: None,
            by_isbn: false,
        };
        let listing = format!(
            r#"{}<main><a href="/ouvrage/le-grand-voyage">Le Grand Voyage</a></main>"#,
            padding()
        );
        let site = FakeSite::default()
            .page(
                &format!("https://editions.example/introuvable?q={CONTROL_QUERY}"),
                &listing,
            )
            .page(
                "https://editions.example/introuvable?q=Le%20Grand%20Voyage",
                &listing,
            )
            .page(
                "https://editions.example/ouvrage/le-grand-voyage",
                &book_page(ISBN, "Le Grand Voyage"),
            );
        let r = check_recipes(&site, &recipes(None, Some(search)), &[witness(None)])
            .await
            .unwrap();
        assert_eq!(r.search_works, Some(false));
        assert!(!r.all_work());
    }

    #[tokio::test]
    async fn a_page_robots_txt_now_forbids_breaks_the_recipe() {
        let site = FakeSite {
            forbidden: vec!["/livre/".into()],
            ..FakeSite::default()
        }
        .page(
            "https://editions.example/ouvrage/9782000000011",
            &book_page(ISBN, "Le Grand Voyage"),
        );
        let r = check_recipes(
            &site,
            &recipes(Some(product()), None),
            &[witness(Some(
                "https://editions.example/ouvrage/9782000000011",
            ))],
        )
        .await
        .unwrap();
        assert_eq!(r.live_witnesses, 1);
        assert_eq!(r.product_works, Some(false));
        assert_eq!(r.failed(), [RecipeKind::Product]);
    }

    #[tokio::test]
    async fn a_forbidden_witness_page_still_tests_the_recipe() {
        // robots.txt now forbids the book pages: the witness's own page and
        // the address the recipe builds alike.
        let site = FakeSite {
            forbidden: vec!["/livre/".into()],
            ..FakeSite::default()
        };
        let r = check_recipes(
            &site,
            &recipes(Some(product()), None),
            &[witness(Some(BOOK))],
        )
        .await
        .unwrap();
        assert_eq!(r.live_witnesses, 1);
        assert_eq!(r.failed(), [RecipeKind::Product]);
        assert!(r.needs_reprobe(None));
    }

    #[test]
    fn only_a_failed_probed_recipe_calls_for_a_new_probe() {
        let result = CheckResult {
            live_witnesses: 1,
            product_works: Some(false),
            search_works: Some(true),
        };
        assert!(result.needs_reprobe(None));
        let by_hand = SiteOverride {
            product: Some(product()),
            ..SiteOverride::default()
        };
        assert!(!result.needs_reprobe(Some(&by_hand)));
        let all_good = CheckResult {
            product_works: Some(true),
            ..result
        };
        assert!(!all_good.needs_reprobe(None));
    }

    #[test]
    fn records_count_unreachable_checks_and_keep_the_last_conclusive_result() {
        let tested = recipes(Some(product()), None);
        let broken = next_record(
            None,
            "d1",
            CheckStatus::Broken,
            None,
            vec![RecipeKind::Product],
            &tested,
        );
        assert_eq!(broken.tested_product, Some(product()));
        assert_eq!(broken.unreachable_in_a_row, 0);

        // Two unreachable checks: counted, and what failed is kept.
        let off1 = next_record(
            Some(&broken),
            "d2",
            CheckStatus::Offline,
            None,
            Vec::new(),
            &tested,
        );
        let off2 = next_record(
            Some(&off1),
            "d3",
            CheckStatus::Blocked,
            None,
            Vec::new(),
            &tested,
        );
        assert_eq!(off2.unreachable_in_a_row, 2);
        assert_eq!(off2.failed, [RecipeKind::Product]);
        assert_eq!(off2.tested_product, Some(product()));

        // A conclusive check resets the count and replaces the result.
        let works = next_record(
            Some(&off2),
            "d4",
            CheckStatus::Works,
            None,
            Vec::new(),
            &tested,
        );
        assert_eq!(works.unreachable_in_a_row, 0);
        assert!(works.failed.is_empty());
    }

    #[test]
    fn several_unreachable_runs_on_one_day_count_once() {
        let tested = recipes(Some(product()), None);
        let first = next_record(None, "d1", CheckStatus::Offline, None, Vec::new(), &tested);
        let again = next_record(
            Some(&first),
            "d1",
            CheckStatus::Offline,
            None,
            Vec::new(),
            &tested,
        );
        let third = next_record(
            Some(&again),
            "d1",
            CheckStatus::Blocked,
            None,
            Vec::new(),
            &tested,
        );
        assert_eq!(third.unreachable_in_a_row, 1);
        let next_day = next_record(
            Some(&third),
            "d2",
            CheckStatus::Offline,
            None,
            Vec::new(),
            &tested,
        );
        assert_eq!(next_day.unreachable_in_a_row, 2);
    }

    #[test]
    fn a_new_probe_blocked_on_the_way_keeps_the_old_one() {
        let probe = |status| SiteProbe {
            home: "https://editions.example/".into(),
            status,
            probed_at: "d".into(),
            note: None,
            witnesses: Vec::new(),
            product: None,
            search: None,
            tried: Vec::new(),
        };
        assert_eq!(
            after_reprobe(&probe(ProbeStatus::Blocked)),
            CheckStatus::Blocked
        );
        assert_eq!(
            after_reprobe(&probe(ProbeStatus::Offline)),
            CheckStatus::Offline
        );
        assert_eq!(
            after_reprobe(&probe(ProbeStatus::NoRecipe)),
            CheckStatus::Reprobed
        );
        assert_eq!(
            after_reprobe(&probe(ProbeStatus::RobotsForbid)),
            CheckStatus::Reprobed
        );
    }

    #[tokio::test]
    async fn a_hand_added_witness_still_judges_the_search() {
        // The "no result" page lists the latest books: the three home page
        // witnesses cannot judge the search, the fourth (added by hand) can.
        let search = SearchRecipe {
            url: "https://editions.example/recherche?q={q}".into(),
            post_fields: None,
            by_isbn: true,
        };
        let latest: String = (1..=3)
            .map(|i| format!(r#"<a href="/ouvrage/nouveaute-{i}">Nouveaute {i}</a>"#))
            .collect();
        let no_result = format!("{}<main>{latest}</main>", padding());
        let novelty = |i: usize| Witness {
            isbn: format!("978200000000{i}"),
            title: format!("Nouveaute {i}"),
            authors: String::new(),
            url: None,
        };
        let by_hand = Witness {
            isbn: ISBN.into(),
            title: "Le Grand Voyage".into(),
            authors: String::new(),
            url: None,
        };
        let site = FakeSite::default()
            .page(
                &format!("https://editions.example/recherche?q={CONTROL_QUERY}"),
                &no_result,
            )
            .page(
                &format!("https://editions.example/recherche?q={ISBN}"),
                &format!(
                    r#"{}<main><a href="/ouvrage/{ISBN}">Le Grand Voyage</a></main>"#,
                    padding()
                ),
            )
            .page(
                &format!("https://editions.example/ouvrage/{ISBN}"),
                &book_page(ISBN, "Le Grand Voyage"),
            );
        let r = check_recipes(
            &site,
            &recipes(None, Some(search)),
            &[novelty(1), novelty(2), novelty(3), by_hand],
        )
        .await
        .unwrap();
        assert_eq!(r.search_works, Some(true));
    }

    #[tokio::test]
    async fn a_blocked_site_is_an_error_not_a_broken_recipe() {
        let site = FakeSite {
            blocked: true,
            ..FakeSite::default()
        };
        let err = check_recipes(
            &site,
            &recipes(Some(product()), None),
            &[witness(Some(BOOK))],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, FetchError::Blocked(_)));
    }
}
