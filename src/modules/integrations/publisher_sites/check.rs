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
use super::lookup::{MAX_PRODUCT_TRIES, MAX_RESULT_LINKS, shows_isbn};
use super::page::{page_matches, result_links, sitemap_book_links, sitemap_locs};
use super::probe::{
    CONTROL_QUERY, MAX_SITEMAP_CANDIDATES, product_urls, redirected_to_book, run_search,
};
use super::table::{
    CheckStatus, ProbeStatus, ProductRecipe, RecipeCheck, RecipeKind, SearchRecipe, SiteOverride,
    SiteProbe, SiteRecipes, SitemapRecipe, Witness,
};

/// Witnesses each recipe is judged on, at most.
const MAX_TRIES_PER_RECIPE: usize = 3;
/// Witnesses looked at, at most: some cannot judge a search (its "no result"
/// page already links them), so more than the tries above may be needed.
const MAX_LIVE_WITNESSES: usize = 6;

/// What a check found. `None`: the site has no such recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Witnesses still in the catalogue that the recipes were tried on.
    pub live_witnesses: usize,
    pub product_works: Option<bool>,
    pub search_works: Option<bool>,
    pub sitemap_works: Option<bool>,
}

impl CheckResult {
    /// The recipes that reached no witness.
    pub fn failed(&self) -> Vec<RecipeKind> {
        [
            (RecipeKind::Product, self.product_works),
            (RecipeKind::Search, self.search_works),
            (RecipeKind::Sitemap, self.sitemap_works),
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
            // Never written by hand.
            RecipeKind::Sitemap => true,
        })
    }

    /// Every recipe the site has reached a witness.
    pub fn all_work(&self) -> bool {
        self.live_witnesses > 0
            && self.product_works != Some(false)
            && self.search_works != Some(false)
            && self.sitemap_works != Some(false)
    }
}

/// Whether a failure says the site is unusable now (rather than this page
/// missing): it ends the check. A robots.txt refusal is not one: the page is
/// out of bounds, so the recipe that leads there no longer serves, and the
/// probe that follows tells whether the whole site is.
fn site_down(e: &FetchError) -> bool {
    matches!(e, FetchError::Blocked(_) | FetchError::Network)
}

/// Whether `e`, met on a recipe's address, ends the check. A timeout or a
/// lost connection once the site has `answered` in this check is that
/// address failing, not the site: some servers take longer than our timeout
/// to say 404 (lisez.com, for a wrong slug).
fn gives_up(e: &FetchError, answered: bool) -> bool {
    match e {
        FetchError::Network => !answered,
        e => site_down(e),
    }
}

/// What a witness's page tells.
struct Witnessed {
    /// Still sold, as far as we can tell.
    live: bool,
    /// The site served the page: it is up.
    answered: bool,
}

/// Whether the witness is still sold: its page, when known, still shows its
/// ISBN. A witness given without a page is taken as live, and so is one whose
/// page robots.txt now forbids: the refusal says nothing of the book, and
/// taken as gone it would leave the recipe untried, hence never failing,
/// when that same path is the recipe's.
async fn is_live(
    fetcher: &dyn SiteFetcher,
    w: &Witness,
    answered: bool,
) -> Result<Witnessed, FetchError> {
    let unseen = |live| Witnessed {
        live,
        answered: false,
    };
    let Some(url) = w.url.as_deref().and_then(|u| Url::parse(u).ok()) else {
        return Ok(unseen(true));
    };
    match fetcher.get(&url).await {
        Ok(page) => Ok(Witnessed {
            live: page_matches(&page.html, &w.isbn, "").is_some(),
            answered: true,
        }),
        Err(FetchError::Disallowed) => Ok(unseen(true)),
        Err(e) if gives_up(&e, answered) => Err(e),
        Err(_) => Ok(unseen(false)),
    }
}

async fn product_reaches(
    fetcher: &dyn SiteFetcher,
    recipe: &ProductRecipe,
    w: &Witness,
    answered: bool,
) -> Result<bool, FetchError> {
    for raw in product_urls(recipe, &w.isbn, &w.title, &w.authors)
        .into_iter()
        .take(MAX_PRODUCT_TRIES)
    {
        let Ok(url) = Url::parse(&raw) else { continue };
        match fetcher.get(&url).await {
            Ok(page) if page_matches(&page.html, &w.isbn, "").is_some() => return Ok(true),
            Ok(_) => {}
            Err(e) if gives_up(&e, answered) => return Err(e),
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
    answered: bool,
) -> Result<Option<Page>, FetchError> {
    match run_search(fetcher, recipe, CONTROL_QUERY).await {
        Ok((page, _)) => Ok(Some(page)),
        Err(e) if gives_up(&e, answered) => Err(e),
        Err(_) => Ok(None),
    }
}

async fn search_reaches(
    fetcher: &dyn SiteFetcher,
    recipe: &SearchRecipe,
    w: &Witness,
    answered: bool,
) -> Result<bool, FetchError> {
    let query = if recipe.by_isbn { &w.isbn } else { &w.title };
    let (page, asked) = match run_search(fetcher, recipe, query).await {
        Ok(answer) => answer,
        Err(e) if gives_up(&e, answered) => return Err(e),
        Err(_) => return Ok(false),
    };
    // Judged as the app will find books: the ISBN on the page, nothing less.
    if redirected_to_book(&page, &asked, &w.isbn) && shows_isbn(&page, &w.isbn) {
        return Ok(true);
    }
    for link in result_links(&page.html, &page.url, &w.isbn, &w.title, &w.authors)
        .into_iter()
        .take(MAX_RESULT_LINKS)
    {
        match fetcher.get(&link).await {
            Ok(p) if shows_isbn(&p, &w.isbn) => return Ok(true),
            Ok(_) => {}
            Err(e) if gives_up(&e, answered) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(false)
}

/// The pages the recipe's site map lists, `None` when it cannot be read (a
/// map that is gone leads nowhere).
async fn sitemap_pages(
    fetcher: &dyn SiteFetcher,
    recipe: &SitemapRecipe,
) -> Result<Option<Vec<Url>>, FetchError> {
    let Ok(url) = Url::parse(&recipe.url) else {
        return Ok(None);
    };
    match fetcher.get(&url).await {
        Ok(map) => Ok(Some(sitemap_locs(&map.html, &url).1)),
        Err(e) if site_down(&e) => Err(e),
        Err(_) => Ok(None),
    }
}

async fn sitemap_reaches(
    fetcher: &dyn SiteFetcher,
    pages: &[Url],
    w: &Witness,
    answered: bool,
) -> Result<bool, FetchError> {
    for link in sitemap_book_links(pages, &w.isbn, &w.title)
        .into_iter()
        .take(MAX_SITEMAP_CANDIDATES)
    {
        match fetcher.get(&link).await {
            Ok(p) if page_matches(&p.html, &w.isbn, "").is_some() => return Ok(true),
            Ok(_) => {}
            Err(e) if gives_up(&e, answered) => return Err(e),
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
        sitemap_works: recipes.sitemap.as_ref().map(|_| false),
    };
    let mapped = match &recipes.sitemap {
        Some(recipe) => sitemap_pages(fetcher, recipe).await?,
        None => None,
    };
    // Whether the site is up, for `gives_up`: its home page first (a
    // witness's page may hang as well as a recipe's), then any witness's.
    let mut answered = match Url::parse(&recipes.home) {
        Ok(home) => match fetcher.get(&home).await {
            Ok(_) => true,
            Err(e) if site_down(&e) => return Err(e),
            Err(_) => false,
        },
        Err(_) => false,
    };
    // Asked once the site is known to be up.
    let mut control: Option<Option<Page>> = None;
    let (mut product_tries, mut search_tries, mut sitemap_tries) = (0, 0, 0);
    for w in witnesses {
        let product_pending =
            result.product_works == Some(false) && product_tries < MAX_TRIES_PER_RECIPE;
        let search_pending =
            result.search_works == Some(false) && search_tries < MAX_TRIES_PER_RECIPE;
        let sitemap_pending = mapped.is_some()
            && result.sitemap_works == Some(false)
            && sitemap_tries < MAX_TRIES_PER_RECIPE;
        if (!product_pending && !search_pending && !sitemap_pending)
            || result.live_witnesses >= MAX_LIVE_WITNESSES
        {
            break;
        }
        let seen = is_live(fetcher, w, answered).await?;
        answered |= seen.answered;
        if !seen.live {
            continue;
        }
        result.live_witnesses += 1;
        if let (true, Some(recipe)) = (product_pending, &recipes.product) {
            product_tries += 1;
            result.product_works = Some(product_reaches(fetcher, recipe, w, answered).await?);
        }
        if let (true, None, Some(recipe)) = (search_pending, &control, &recipes.search) {
            control = Some(control_page(fetcher, recipe, answered).await?);
        }
        // A witness the nonsense query already links cannot judge the search.
        let judged = control.as_ref().and_then(Option::as_ref).is_some_and(|c| {
            result_links(&c.html, &c.url, &w.isbn, &w.title, &w.authors).is_empty()
        });
        if let (true, true, Some(recipe)) = (search_pending, judged, &recipes.search) {
            search_tries += 1;
            result.search_works = Some(search_reaches(fetcher, recipe, w, answered).await?);
        }
        if let (true, Some(pages)) = (sitemap_pending, &mapped) {
            sitemap_tries += 1;
            result.sitemap_works = Some(sitemap_reaches(fetcher, pages, w, answered).await?);
        }
    }
    Ok(result)
}

/// What the check of a site whose probed recipe failed concludes from the
/// new probe. Blocked or offline says nothing about the recipes: the site is
/// counted unreachable and the previous probe, whose other recipe may well
/// work, is kept. Any other outcome replaces it.
/// Whether a new probe of a site should be dropped for the last one: it
/// could not reach the site (blocked, offline) while the last one, on the
/// same home page, found recipes. A bad day says nothing about them; the
/// checks count the days a site cannot be reached and take it out of the
/// table after a few.
pub fn keeps_previous(previous: Option<&SiteProbe>, new: &SiteProbe) -> bool {
    matches!(new.status, ProbeStatus::Blocked | ProbeStatus::Offline)
        && previous.is_some_and(|p| p.status == ProbeStatus::Ok && p.home == new.home)
}

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
            sitemap: None,
        }
    }

    fn with_sitemap() -> SiteRecipes {
        SiteRecipes {
            sitemap: Some(SitemapRecipe {
                url: "https://editions.example/sitemap.xml".into(),
            }),
            ..recipes(None, None)
        }
    }

    #[tokio::test]
    async fn a_site_map_recipe_finds_the_page_named_after_the_title() {
        let book = "https://editions.example/livres/recits/le-grand-voyage/";
        let site = FakeSite::default()
            .page(
                "https://editions.example/sitemap.xml",
                &format!(
                    "<urlset><url><loc>https://editions.example/contact</loc></url>\
                     <url><loc>{book}</loc></url></urlset>"
                ),
            )
            .page(book, &book_page(ISBN, "Le Grand Voyage"));
        let r = check_recipes(&site, &with_sitemap(), &[witness(Some(book))])
            .await
            .unwrap();
        assert_eq!(r.sitemap_works, Some(true));
        assert!(r.all_work());
    }

    #[tokio::test]
    async fn a_slow_answer_after_the_site_answered_fails_the_address_only() {
        // The witness's page comes, the recipe's address never does (a 404
        // slower than our timeout): the recipe fails, the site is up.
        let witness_page = "https://editions.example/fiche/le-grand-voyage";
        let site = FakeSite {
            slow: vec!["/livre/".into()],
            ..FakeSite::default()
        }
        .page(witness_page, &book_page(ISBN, "Le Grand Voyage"));
        let r = check_recipes(
            &site,
            &recipes(Some(product()), None),
            &[witness(Some(witness_page))],
        )
        .await
        .unwrap();
        assert_eq!(r.product_works, Some(false));
        // The home page answers, the witness's page hangs: the witness is
        // taken as gone, the site as up.
        let hanging = FakeSite {
            slow: vec!["/fiche/".into()],
            ..FakeSite::default()
        }
        .page("https://editions.example/", "<p>Accueil</p>");
        let r = check_recipes(
            &hanging,
            &recipes(Some(product()), None),
            &[witness(Some(witness_page))],
        )
        .await
        .unwrap();
        assert_eq!(r.live_witnesses, 0);
        // Nothing has answered yet: the same timeout is the site's.
        let r = check_recipes(&site, &recipes(Some(product()), None), &[witness(None)]).await;
        assert_eq!(r.unwrap_err(), FetchError::Network);
    }

    #[tokio::test]
    async fn a_site_map_that_is_gone_breaks_its_recipe() {
        let book = "https://editions.example/livres/recits/le-grand-voyage/";
        let site = FakeSite::default().page(book, &book_page(ISBN, "Le Grand Voyage"));
        let r = check_recipes(&site, &with_sitemap(), &[witness(Some(book))])
            .await
            .unwrap();
        assert_eq!(r.sitemap_works, Some(false));
        assert_eq!(r.failed(), [RecipeKind::Sitemap]);
        assert!(r.needs_reprobe(None));
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
            2,
            "only the home page and the witness page are asked"
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
            sitemap_works: None,
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
            book_sitemap: None,
            sitemap: None,
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
    async fn a_search_that_finds_only_another_edition_does_not_work() {
        // The search answers with the title's other edition: the app would
        // not take it (no ISBN of the book on it), so the check does not.
        let search = SearchRecipe {
            url: "https://editions.example/recherche?q={q}".into(),
            post_fields: None,
            by_isbn: true,
        };
        let other = "https://editions.example/ouvrage/le-grand-voyage-poche";
        let site = FakeSite::default()
            .page(
                &format!("https://editions.example/recherche?q={CONTROL_QUERY}"),
                &format!("{}<main><p>Aucun résultat</p></main>", padding()),
            )
            .page(
                &format!("https://editions.example/recherche?q={ISBN}"),
                &format!(
                    r#"{}<main><a href="/ouvrage/le-grand-voyage-poche">Le Grand Voyage</a></main>"#,
                    padding()
                ),
            )
            .page(other, &book_page("9782000000028", "Le Grand Voyage"));
        let r = check_recipes(&site, &recipes(None, Some(search)), &[witness(None)])
            .await
            .unwrap();
        assert_eq!(r.search_works, Some(false));
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

    #[test]
    fn a_probe_that_cannot_reach_the_site_keeps_the_last_good_one() {
        let probe = |status: ProbeStatus, home: &str| SiteProbe {
            home: home.into(),
            status,
            probed_at: "2026-11-14".into(),
            note: None,
            witnesses: Vec::new(),
            product: None,
            search: None,
            tried: Vec::new(),
            book_sitemap: None,
            sitemap: None,
        };
        let good = probe(ProbeStatus::Ok, "https://e.example/");
        let blocked = probe(ProbeStatus::Blocked, "https://e.example/");
        assert!(keeps_previous(Some(&good), &blocked));
        assert!(keeps_previous(
            Some(&good),
            &probe(ProbeStatus::Offline, "https://e.example/")
        ));
        // Anything the site itself answered replaces it.
        assert!(!keeps_previous(
            Some(&good),
            &probe(ProbeStatus::NoRecipe, "https://e.example/")
        ));
        // So does a probe of a new home page, or one with nothing to keep.
        assert!(!keeps_previous(
            Some(&good),
            &probe(ProbeStatus::Blocked, "https://new.example/")
        ));
        assert!(!keeps_previous(Some(&blocked), &blocked));
        assert!(!keeps_previous(None, &blocked));
    }
}
