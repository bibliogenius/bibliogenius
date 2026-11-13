//! The maintenance probe: find, for one publisher site, a recipe that leads
//! from an ISBN to the book's page, and prove it on books of the site.
//!
//! 1. Witnesses: books the site currently sells, taken from links of its home
//!    page (ISBN, title and author read on their own page), plus any given by
//!    hand.
//! 2. Product pattern: the witnesses' addresses with `{isbn}`, `{slug}`,
//!    `{author}` put back, kept when it rebuilds them; a deliberately wrong
//!    slug tells whether the server reads only the ISBN.
//! 3. Search: the search forms of the home page and of a few usual pages
//!    (GET kept as is, POST submitted once and its redirect kept), then usual
//!    addresses. Each is run on a nonsense query first: a page that already
//!    links the witnesses (the home page under another name) proves nothing,
//!    and an answer identical for every query means results drawn by
//!    JavaScript.
//!
//! The probe goes through [`SiteFetcher`], so robots.txt, pacing and
//! blocking are the fetcher's business, and tests run on an in-memory site.

use url::Url;

use super::fetch::{FetchError, Page, PostAnswer, SiteFetcher};
use super::page::{PageMatch, book_facts, page_matches, result_links, search_forms, witness_links};
use super::table::{
    ProbeStatus, ProductCheck, ProductRecipe, SearchCheck, SearchRecipe, SiteProbe, Witness,
};
use super::text::{Apostrophe, author_variants, slugify, title_variants};

/// Witnesses looked for on the home page.
const WANTED_WITNESSES: usize = 3;
/// Home page links opened to find them.
const MAX_WITNESS_LINKS: usize = WANTED_WITNESSES + 4;
/// Pages, besides the home page, whose forms are read.
const SEARCH_PAGES: [&str; 3] = ["/catalogue", "/recherche", "/search"];
/// Search forms tried per page: each costs a few requests, and a page full
/// of forms (filters, sorting) must not multiply them.
const MAX_FORMS_PER_PAGE: usize = 5;
/// Usual search addresses tried when no form gives one.
const GUESSES: [&str; 12] = [
    "/recherche?q={q}",
    "/search?q={q}",
    "/rechercher?q={q}",
    "/recherche?s={q}",
    "/recherche?global_s={q}",
    "/catalogue?s%5Bq%5D={q}",
    "/?s={q}",
    "/recherche?keys={q}",
    "/search?query={q}",
    "/catalogsearch/result/?q={q}",
    "/recherche/{q}",
    "/search/{q}",
];
/// The control query: no book has it in its title.
pub const CONTROL_QUERY: &str = "zzqxwv";
/// A real result page, even empty, carries the site's menus: less than this
/// without a link is an error message.
const MIN_RESULT_PAGE_BYTES: usize = 2000;

/// Probe `home`. `extra` witnesses (given by hand) are tested along with
/// those of the home page. `today` dates the result.
pub async fn probe_site(
    fetcher: &dyn SiteFetcher,
    home: &Url,
    extra: &[Witness],
    today: &str,
) -> SiteProbe {
    let mut out = SiteProbe {
        home: home.to_string(),
        status: ProbeStatus::Offline,
        probed_at: today.to_owned(),
        note: None,
        witnesses: Vec::new(),
        product: None,
        search: None,
        tried: Vec::new(),
    };
    match run(fetcher, home, extra, &mut out).await {
        Ok(status) => out.status = status,
        Err(e) => {
            out.status = match e {
                FetchError::Blocked(_) => ProbeStatus::Blocked,
                FetchError::Disallowed => ProbeStatus::RobotsForbid,
                _ => ProbeStatus::Offline,
            };
            out.note = Some(describe(&e));
        }
    }
    out
}

fn describe(e: &FetchError) -> String {
    match e {
        FetchError::Blocked(why) => format!("blocked: {why}"),
        FetchError::Disallowed => "forbidden by robots.txt".into(),
        FetchError::Status(s) => format!("HTTP {s}"),
        FetchError::Network => "unreachable".into(),
        FetchError::Refused => "address refused (not public https, or too large)".into(),
    }
}

/// The probe proper. An `Err` is a reason to stop probing the site (blocked,
/// or its home page unreachable); what was learnt so far stays in `out`.
async fn run(
    fetcher: &dyn SiteFetcher,
    home: &Url,
    extra: &[Witness],
    out: &mut SiteProbe,
) -> Result<ProbeStatus, FetchError> {
    let home_page = fetcher.get(home).await?;
    let mut witnesses = find_witnesses(fetcher, &home_page).await?;
    for w in extra {
        if !witnesses.iter().any(|x| x.isbn == w.isbn) {
            witnesses.push(w.clone());
        }
    }
    out.witnesses = witnesses.clone();
    if witnesses.is_empty() {
        return Ok(ProbeStatus::NoWitness);
    }

    let with_url: Vec<Witness> = witnesses
        .iter()
        .filter(|w| w.url.is_some())
        .cloned()
        .collect();
    out.product = probe_product(fetcher, &with_url)
        .await?
        .filter(|p| p.checked_on >= 2 || p.recipe.slug_ignored);

    let (best, tried) = probe_search(fetcher, &home_page, &witnesses).await?;
    out.tried = tried;
    out.search = best;

    if out.product.is_some() || out.search.is_some() {
        return Ok(ProbeStatus::Ok);
    }
    Ok(
        if !out.tried.is_empty() && out.tried.iter().all(|t| t.broken) {
            ProbeStatus::SearchDown
        } else if out.tried.iter().any(|t| t.js_results) {
            ProbeStatus::JsResults
        } else {
            ProbeStatus::NoRecipe
        },
    )
}

/// A failure that should stop the probe; others just mean "not this way".
fn stops(e: &FetchError) -> bool {
    matches!(e, FetchError::Blocked(_))
}

/// Books of the home page, read on their own page. A blocked site is an
/// error; a book page that fails is skipped.
pub async fn find_witnesses(
    fetcher: &dyn SiteFetcher,
    home: &Page,
) -> Result<Vec<Witness>, FetchError> {
    let mut out: Vec<Witness> = Vec::new();
    for link in witness_links(&home.html, &home.url)
        .into_iter()
        .take(MAX_WITNESS_LINKS)
    {
        if out.len() >= WANTED_WITNESSES {
            break;
        }
        let page = match fetcher.get(&link).await {
            Ok(p) => p,
            Err(e) if stops(&e) => return Err(e),
            Err(_) => continue,
        };
        if let Some(f) = book_facts(&page.html, &page.url)
            && !out.iter().any(|w| w.isbn == f.isbn)
        {
            out.push(Witness {
                isbn: f.isbn,
                title: f.title,
                authors: f.authors,
                url: Some(page.url.to_string()),
            });
        }
    }
    Ok(out)
}

/// A witness's address with its parts put back as placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WitnessPattern {
    template: String,
    /// The apostrophe rule the address shows; `None` when the title has no
    /// apostrophe (both rules give the same slug), so the witness has no say.
    apostrophe: Option<Apostrophe>,
}

/// The witness's address with `{isbn}`, `{slug}` and `{author}` put back,
/// trying both apostrophe rules, every title order and author order. When no
/// slug of the title is in it, the address with `{isbn}` alone (the page is
/// reached by its ISBN only; whether that holds for every book is for the
/// other witnesses to tell).
fn pattern_from_witness(w: &Witness) -> Option<WitnessPattern> {
    let raw = w.url.as_deref()?;
    let url = raw.split('?').next().unwrap_or(raw);
    if !url.contains(&w.isbn) {
        return None;
    }
    for rule in [Apostrophe::Drop, Apostrophe::Dash] {
        for title in title_variants(&w.title) {
            let slug = slugify(&title, rule);
            if slug.is_empty() {
                continue;
            }
            let authors = std::iter::once(String::new()).chain(author_variants(&w.authors));
            for author in authors {
                let full = if author.is_empty() {
                    slug.clone()
                } else {
                    format!("{slug}-{}", slugify(&author, rule))
                };
                if url.contains(&full) {
                    let placeholder = if author.is_empty() {
                        "{slug}"
                    } else {
                        "{slug}-{author}"
                    };
                    let other = match rule {
                        Apostrophe::Drop => Apostrophe::Dash,
                        Apostrophe::Dash => Apostrophe::Drop,
                    };
                    let telling = slugify(&title, other) != slug
                        || slugify(&author, other) != slugify(&author, rule);
                    return Some(WitnessPattern {
                        template: url
                            .replace(&w.isbn, "{isbn}")
                            .replacen(&full, placeholder, 1),
                        apostrophe: telling.then_some(rule),
                    });
                }
            }
        }
    }
    Some(WitnessPattern {
        template: url.replace(&w.isbn, "{isbn}"),
        apostrophe: None,
    })
}

/// The pattern most witnesses share (the first seen winning a tie), with
/// the apostrophe rule most of its telling witnesses show (`Drop` when none
/// tells).
fn vote(patterns: &[WitnessPattern]) -> Option<ProductRecipe> {
    let most_frequent = |items: Vec<&String>| -> Option<String> {
        items
            .iter()
            .max_by_key(|t| {
                let n = items.iter().filter(|u| u == t).count();
                (n, std::cmp::Reverse(items.iter().position(|u| u == *t)))
            })
            .map(|t| (*t).clone())
    };
    let template = most_frequent(patterns.iter().map(|p| &p.template).collect())?;
    let rules: Vec<Apostrophe> = patterns
        .iter()
        .filter(|p| p.template == template)
        .filter_map(|p| p.apostrophe)
        .collect();
    let dash = rules.iter().filter(|r| **r == Apostrophe::Dash).count();
    let apostrophe = if dash * 2 > rules.len() {
        Apostrophe::Dash
    } else {
        Apostrophe::Drop
    };
    Some(ProductRecipe {
        template,
        apostrophe,
        slug_ignored: false,
    })
}

/// The addresses a product recipe gives for a book, most likely first.
pub fn product_urls(recipe: &ProductRecipe, isbn: &str, title: &str, authors: &str) -> Vec<String> {
    let authors_tried = if recipe.template.contains("{author}") {
        author_variants(authors)
    } else {
        vec![String::new()]
    };
    let mut out: Vec<String> = Vec::new();
    for t in title_variants(title) {
        for a in &authors_tried {
            let u = recipe
                .template
                .replace("{isbn}", isbn)
                .replace("{slug}", &slugify(&t, recipe.apostrophe))
                .replace("{author}", &slugify(a, recipe.apostrophe));
            if !out.contains(&u) {
                out.push(u);
            }
        }
    }
    if recipe.slug_ignored {
        out.truncate(1);
    }
    out
}

async fn probe_product(
    fetcher: &dyn SiteFetcher,
    witnesses: &[Witness],
) -> Result<Option<ProductCheck>, FetchError> {
    let patterns: Vec<WitnessPattern> = witnesses.iter().filter_map(pattern_from_witness).collect();
    let Some(mut recipe) = vote(&patterns) else {
        return Ok(None);
    };
    let mismatches = witnesses
        .iter()
        .filter(|w| {
            let url = w.url.as_deref().unwrap_or("");
            let url = url.split('?').next().unwrap_or(url);
            !product_urls(&recipe, &w.isbn, &w.title, &w.authors)
                .iter()
                .any(|u| u == url)
        })
        .count();

    // Does the server ignore the slug? (Nothing to ask without one.)
    let w = &witnesses[0];
    if !recipe.template.contains("{slug}") {
        return Ok(Some(ProductCheck {
            recipe,
            checked_on: witnesses.len() - mismatches,
            mismatches,
        }));
    }
    let wrong = recipe
        .template
        .replace("{isbn}", &w.isbn)
        .replace("{slug}", "x")
        .replace("{author}", "x");
    if let Ok(wrong) = Url::parse(&wrong) {
        match fetcher.get(&wrong).await {
            Ok(page) => {
                recipe.slug_ignored =
                    page_matches(&page.html, &w.isbn, "") == Some(PageMatch::Isbn);
                if recipe.slug_ignored {
                    recipe.template = bare_slug_segment(&recipe.template);
                }
            }
            Err(e) if stops(&e) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(Some(ProductCheck {
        recipe,
        checked_on: witnesses.len() - mismatches,
        mismatches,
    }))
}

/// When the server ignores the slug, the path segment holding it is reduced
/// to `{slug}`: whatever the witness's address added there (an author's name
/// read nowhere else, a subtitle) would otherwise be frozen into every book's
/// address. A segment that also holds the ISBN is left as it is.
fn bare_slug_segment(template: &str) -> String {
    template
        .split('/')
        .map(|seg| {
            if seg.contains("{slug}") && !seg.contains("{isbn}") {
                "{slug}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A form as a search recipe. A POST form is submitted once with
/// `sample_isbn`: when it redirects to an address carrying it, that address
/// is the recipe (a GET); otherwise the POST itself is.
async fn form_recipe(
    fetcher: &dyn SiteFetcher,
    form: &super::page::SearchForm,
    sample_isbn: &str,
) -> Result<Option<SearchRecipe>, FetchError> {
    if !form.post {
        let query = form_urlencode(&form.fields);
        let mut url = form.action.to_string();
        url.push('?');
        url.push_str(&query);
        return Ok(Some(SearchRecipe {
            url,
            post_fields: None,
            by_isbn: false,
        }));
    }
    let filled: Vec<(String, String)> = form
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("{q}", sample_isbn)))
        .collect();
    match fetcher.post_form(&form.action, &filled).await {
        Ok(PostAnswer::Redirect(to)) if to.as_str().contains(sample_isbn) => {
            Ok(Some(SearchRecipe {
                url: to.as_str().replace(sample_isbn, "{q}"),
                post_fields: None,
                by_isbn: false,
            }))
        }
        Ok(_) => Ok(Some(SearchRecipe {
            url: form.action.to_string(),
            post_fields: Some(form.fields.clone()),
            by_isbn: false,
        })),
        Err(e) if stops(&e) => Err(e),
        // A form robots.txt forbids, or one that fails: not a recipe.
        Err(_) => Ok(None),
    }
}

/// Form-encode fields, leaving the `{q}` placeholder readable.
fn form_urlencode(fields: &[(String, String)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in fields {
        ser.append_pair(k, v);
    }
    ser.finish().replace("%7Bq%7D", "{q}")
}

/// Run a search. Returns the answer and the address asked, against which a
/// redirect is judged.
pub async fn run_search(
    fetcher: &dyn SiteFetcher,
    recipe: &SearchRecipe,
    query: &str,
) -> Result<(Page, Url), FetchError> {
    match &recipe.post_fields {
        None => {
            let encoded = urlencoding::encode(query);
            let url = Url::parse(&recipe.url.replace("{q}", &encoded))
                .map_err(|_| FetchError::Refused)?;
            let page = fetcher.get(&url).await?;
            Ok((page, url))
        }
        Some(fields) => {
            let action = Url::parse(&recipe.url).map_err(|_| FetchError::Refused)?;
            let filled: Vec<(String, String)> = fields
                .iter()
                .map(|(k, v)| (k.clone(), v.replace("{q}", query)))
                .collect();
            match fetcher.post_form(&action, &filled).await? {
                PostAnswer::Page(page) => Ok((page, action)),
                PostAnswer::Redirect(to) => {
                    let page = fetcher.get(&to).await?;
                    Ok((page, action))
                }
            }
        }
    }
}

/// The server sent us to the book itself: the final path is not the one
/// asked and carries the ISBN (a `/search/<isbn>` result page does not count).
pub fn redirected_to_book(page: &Page, asked: &Url, isbn: &str) -> bool {
    let fin = page.url.path().trim_end_matches('/');
    fin != asked.path().trim_end_matches('/') && fin.contains(isbn)
}

/// Whether a search for `query` finds `w`, and the size of the answer (an
/// answer whose size never changes is drawn by JavaScript).
async fn search_finds(
    fetcher: &dyn SiteFetcher,
    recipe: &SearchRecipe,
    query: &str,
    w: &Witness,
) -> Result<(bool, Option<usize>), FetchError> {
    match run_search(fetcher, recipe, query).await {
        Ok((page, asked)) => {
            let found = redirected_to_book(&page, &asked, &w.isbn)
                || !result_links(&page.html, &page.url, &w.isbn, &w.title, &w.authors).is_empty();
            Ok((found, Some(page.html.len())))
        }
        Err(e) if stops(&e) => Err(e),
        Err(_) => Ok((false, None)),
    }
}

async fn validate_search(
    fetcher: &dyn SiteFetcher,
    recipe: SearchRecipe,
    witnesses: &[Witness],
) -> Result<SearchCheck, FetchError> {
    let mut check = SearchCheck {
        recipe,
        found_by_isbn: 0,
        found_by_title: 0,
        js_results: false,
        broken: false,
        note: None,
    };
    let control = match run_search(fetcher, &check.recipe, CONTROL_QUERY).await {
        Ok((page, _)) => page,
        Err(e) if stops(&e) => return Err(e),
        Err(e) => {
            check.note = Some(format!("control query: {}", describe(&e)));
            return Ok(check);
        }
    };
    if control.html.len() < MIN_RESULT_PAGE_BYTES
        || !control.html.to_ascii_lowercase().contains("<a ")
    {
        check.broken = true;
        check.note = Some(control.html.chars().take(120).collect());
        return Ok(check);
    }
    let mut sizes = vec![control.html.len()];
    let testable: Vec<&Witness> = witnesses
        .iter()
        .filter(|w| {
            result_links(&control.html, &control.url, &w.isbn, &w.title, &w.authors).is_empty()
        })
        .collect();
    if testable.is_empty() {
        check.note = Some("the control page already links the witnesses".into());
        return Ok(check);
    }
    for (tried, w) in testable.iter().enumerate() {
        let (found, size) = search_finds(fetcher, &check.recipe, &w.isbn, w).await?;
        sizes.extend(size);
        if found {
            check.found_by_isbn += 1;
        } else {
            let (found, size) = search_finds(fetcher, &check.recipe, &w.title, w).await?;
            sizes.extend(size);
            check.found_by_title += usize::from(found);
        }
        // Two witnesses missed: drop this recipe.
        if check.found_by_isbn + check.found_by_title == 0 && tried + 1 >= 2 {
            break;
        }
    }
    sizes.sort_unstable();
    sizes.dedup();
    check.js_results = check.found_by_isbn + check.found_by_title == 0 && sizes.len() == 1;
    check.recipe.by_isbn = check.found_by_isbn > 0;
    Ok(check)
}

/// The best search recipe of the site, if one finds witnesses, and every
/// recipe tried.
async fn probe_search(
    fetcher: &dyn SiteFetcher,
    home: &Page,
    witnesses: &[Witness],
) -> Result<(Option<SearchCheck>, Vec<SearchCheck>), FetchError> {
    let mut pages = vec![home.clone()];
    for path in SEARCH_PAGES {
        let Ok(url) = home.url.join(path) else {
            continue;
        };
        match fetcher.get(&url).await {
            Ok(p) => pages.push(p),
            Err(e) if stops(&e) => return Err(e),
            Err(_) => {}
        }
    }
    // The same form on every page (a header search box) is sent once: a
    // POST form is submitted to turn it into a recipe.
    let mut forms: Vec<super::page::SearchForm> = Vec::new();
    for page in &pages {
        for form in search_forms(&page.html, &page.url)
            .into_iter()
            .take(MAX_FORMS_PER_PAGE)
        {
            if !forms.contains(&form) {
                forms.push(form);
            }
        }
    }
    let mut recipes: Vec<SearchRecipe> = Vec::new();
    for form in forms {
        if let Some(r) = form_recipe(fetcher, &form, &witnesses[0].isbn).await?
            && !recipes
                .iter()
                .any(|x| x.url == r.url && x.post_fields == r.post_fields)
        {
            recipes.push(r);
        }
    }
    let origin = home.url.origin().ascii_serialization();
    for guess in GUESSES {
        let url = format!("{origin}{guess}");
        if !recipes.iter().any(|r| r.url == url) {
            recipes.push(SearchRecipe {
                url,
                post_fields: None,
                by_isbn: false,
            });
        }
    }

    let mut tried = Vec::new();
    let mut best: Option<(usize, SearchCheck)> = None;
    for recipe in recipes {
        let check = validate_search(fetcher, recipe, witnesses).await?;
        let score = check.found_by_isbn * 2 + check.found_by_title;
        let all_by_isbn = check.found_by_isbn == witnesses.len();
        if score > 0 && best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, check.clone()));
        }
        tried.push(check);
        if all_by_isbn {
            break;
        }
    }
    Ok((best.map(|(_, c)| c), tried))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::integrations::publisher_sites::test_site::{FakeSite, book_page, padding};

    const HOME: &str = "https://editions.example/";

    fn site_with_books() -> FakeSite {
        let books = [
            ("9782000000011", "Le Grand Voyage", "le-grand-voyage"),
            ("9782000000028", "L'Hiver des loups", "lhiver-des-loups"),
            ("9782000000035", "Martin Eden", "martin-eden"),
        ];
        let home: String = books
            .iter()
            .map(|(isbn, _, slug)| format!(r#"<a href="/livre/{slug}-{isbn}">x</a>"#))
            .collect();
        let mut site = FakeSite::default().page(HOME, &format!("<main>{home}</main>"));
        for (isbn, title, slug) in books {
            site = site.page(
                &format!("https://editions.example/livre/{slug}-{isbn}"),
                &book_page(isbn, title),
            );
        }
        site
    }

    #[tokio::test]
    async fn finds_and_checks_a_product_pattern() {
        let site = site_with_books();
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::Ok);
        assert_eq!(probe.witnesses.len(), 3);
        let product = probe.product.unwrap();
        assert_eq!(
            product.recipe.template,
            "https://editions.example/livre/{slug}-{isbn}"
        );
        assert_eq!(product.recipe.apostrophe, Apostrophe::Drop);
        assert!(
            !product.recipe.slug_ignored,
            "the fake site 404s a wrong slug"
        );
        assert_eq!(product.checked_on, 3);
        assert!(probe.search.is_none());
        assert_eq!(probe.tried.len(), GUESSES.len());
    }

    fn w(isbn: &str, title: &str, url: &str) -> Witness {
        Witness {
            isbn: isbn.into(),
            title: title.into(),
            authors: String::new(),
            url: Some(url.into()),
        }
    }

    #[test]
    fn only_titles_with_an_apostrophe_vote_on_its_rule() {
        let patterns: Vec<WitnessPattern> = [
            w(
                "9782000000011",
                "Le Grand Voyage",
                "https://e.example/le-grand-voyage-9782000000011",
            ),
            w(
                "9782000000028",
                "L'Hiver des loups",
                "https://e.example/l-hiver-des-loups-9782000000028",
            ),
            w(
                "9782000000035",
                "Martin Eden",
                "https://e.example/martin-eden-9782000000035",
            ),
        ]
        .iter()
        .filter_map(pattern_from_witness)
        .collect();
        let recipe = vote(&patterns).unwrap();
        assert_eq!(recipe.template, "https://e.example/{slug}-{isbn}");
        assert_eq!(recipe.apostrophe, Apostrophe::Dash);
    }

    #[test]
    fn a_page_named_by_its_isbn_alone_is_a_pattern() {
        let patterns: Vec<WitnessPattern> = [
            w(
                "9782000000011",
                "Le Grand Voyage",
                "https://e.example/livre/9782000000011",
            ),
            w(
                "9782000000028",
                "Martin Eden",
                "https://e.example/livre/9782000000028",
            ),
        ]
        .iter()
        .filter_map(pattern_from_witness)
        .collect();
        let recipe = vote(&patterns).unwrap();
        assert_eq!(recipe.template, "https://e.example/livre/{isbn}");
        assert_eq!(
            product_urls(&recipe, "9782000000035", "Autre titre", ""),
            ["https://e.example/livre/9782000000035"]
        );
    }

    #[tokio::test]
    async fn a_search_page_that_ignores_the_query_is_not_a_recipe() {
        // `/?s=` answers the home page, which links every witness.
        let mut site = site_with_books();
        let home = site.pages[HOME].clone() + &padding();
        site.pages.insert(HOME.into(), home.clone());
        site.pages.insert(format!("{HOME}?s={CONTROL_QUERY}"), home);
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-13").await;
        let tried = probe
            .tried
            .iter()
            .find(|t| t.recipe.url.ends_with("/?s={q}"))
            .unwrap();
        assert_eq!(tried.found_by_isbn + tried.found_by_title, 0);
        assert!(tried.note.as_deref().unwrap().contains("already links"));
    }

    #[tokio::test]
    async fn finds_a_search_form_and_validates_it_by_isbn() {
        let mut site = FakeSite::default().page(
            HOME,
            &format!(
                r#"<form action="/recherche"><input type="search" name="texte"></form>{}"#,
                padding()
            ),
        );
        let isbn = "9782000000011";
        let w = Witness {
            isbn: isbn.into(),
            title: "Le Grand Voyage".into(),
            authors: String::new(),
            url: None,
        };
        site = site
            .page(
                &format!("{HOME}recherche?texte={CONTROL_QUERY}"),
                &padding(),
            )
            .page(
                &format!("{HOME}recherche?texte={isbn}"),
                &format!(
                    r#"{}<a href="/ouvrage/{isbn}">Le Grand Voyage</a>"#,
                    padding()
                ),
            );
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[w], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::Ok);
        let search = probe.search.unwrap();
        assert_eq!(
            search.recipe.url,
            "https://editions.example/recherche?texte={q}"
        );
        assert!(search.recipe.by_isbn);
        assert_eq!(search.found_by_isbn, 1);
        // Found on the first recipe: the guesses are not tried.
        assert_eq!(probe.tried.len(), 1);
    }

    #[tokio::test]
    async fn the_same_answer_to_every_query_means_javascript_results() {
        let shell = padding();
        let mut site = FakeSite::default().page(HOME, &shell);
        let w = Witness {
            isbn: "9782000000011".into(),
            title: "Le Grand Voyage".into(),
            authors: String::new(),
            url: None,
        };
        for q in [CONTROL_QUERY, "9782000000011", "Le%20Grand%20Voyage"] {
            for guess in GUESSES {
                site = site.page(
                    &format!("https://editions.example{}", guess.replace("{q}", q)),
                    &shell,
                );
            }
        }
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[w], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::JsResults);
    }

    #[tokio::test]
    async fn tries_a_bounded_number_of_forms_per_page() {
        let forms: String = (0..12)
            .map(|i| format!(r#"<form action="/f{i}"><input type="search" name="q"></form>"#))
            .collect();
        let site = FakeSite::default().page(HOME, &forms);
        let w = Witness {
            isbn: "9782000000011".into(),
            title: "Le Grand Voyage".into(),
            authors: String::new(),
            url: None,
        };
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[w], "2026-11-13").await;
        let from_forms = probe
            .tried
            .iter()
            .filter(|t| t.recipe.url.contains("/f"))
            .count();
        assert_eq!(from_forms, MAX_FORMS_PER_PAGE);
    }

    #[tokio::test]
    async fn a_blocked_site_stops_the_probe() {
        let site = FakeSite {
            blocked: true,
            ..FakeSite::default()
        };
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::Blocked);
        assert_eq!(site.asked.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_home_page_without_books_has_no_witness() {
        let site = FakeSite::default().page(HOME, "<a href='/contact'>Contact</a>");
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::NoWitness);
    }

    #[test]
    fn product_urls_try_title_orders_and_authors() {
        let recipe = ProductRecipe {
            template: "https://e.example/{slug}-{author}-{isbn}".into(),
            apostrophe: Apostrophe::Dash,
            slug_ignored: false,
        };
        let urls = product_urls(
            &recipe,
            "9782000000011",
            "Saga, tome 1 : L'Aube",
            "Anne Auteur",
        );
        assert_eq!(
            urls[0],
            "https://e.example/saga-tome-1-l-aube-anne-auteur-9782000000011"
        );
        assert!(urls.contains(
            &"https://e.example/l-aube-saga-tome-1-auteur-anne-9782000000011".to_string()
        ));

        let ignored = ProductRecipe {
            slug_ignored: true,
            ..recipe
        };
        assert_eq!(
            product_urls(
                &ignored,
                "9782000000011",
                "Saga, tome 1 : L'Aube",
                "Anne Auteur"
            )
            .len(),
            1
        );
    }

    #[test]
    fn an_ignored_slug_keeps_no_frozen_words() {
        assert_eq!(
            bare_slug_segment(
                "https://www.seuil.com/ouvrage/{slug}-contre-enquete-jennifer-tamas/{isbn}"
            ),
            "https://www.seuil.com/ouvrage/{slug}/{isbn}"
        );
        assert_eq!(
            bare_slug_segment("https://e.example/livre/{slug}-{isbn}/"),
            "https://e.example/livre/{slug}-{isbn}/"
        );
    }

    #[test]
    fn a_search_result_path_with_the_isbn_is_not_a_redirect_to_the_book() {
        let asked = Url::parse("https://e.example/search/9782000000011").unwrap();
        let same = Page {
            url: asked.clone(),
            html: String::new(),
        };
        assert!(!redirected_to_book(&same, &asked, "9782000000011"));
        let book = Page {
            url: Url::parse("https://e.example/livre/x-9782000000011").unwrap(),
            html: String::new(),
        };
        assert!(redirected_to_book(&book, &asked, "9782000000011"));
    }
}
