//! Running a site's recipes: the addresses a product template gives for a
//! book, a search sent the way the site expects it, and whether its answer
//! is the book's own page. What the app runs ([`super::lookup`]) and what
//! the maintenance tool checks (`probe`, `check`) share this code, so a
//! change made for the tool shows in what the app does.

use url::Url;

use super::fetch::{FetchError, Page, PostAnswer, SiteFetcher};
use super::table::{ProductRecipe, SearchRecipe};
use super::text::{author_variants, hyphenated_isbn, slugify, title_variants};

/// Pages a site map recipe may open for one book: the best matches of
/// [`super::page::sitemap_book_links`].
pub const MAX_SITEMAP_CANDIDATES: usize = 2;

/// In a product template, the ISBN with its hyphens (`978-2-8180-5511-3`).
pub const HYPHENATED_ISBN: &str = "{isbn_hyphens}";

/// The addresses a product recipe gives for a book, most likely first. None
/// when the template wants the hyphenated ISBN of a book whose ISBN group
/// we cannot hyphenate.
pub fn product_urls(recipe: &ProductRecipe, isbn: &str, title: &str, authors: &str) -> Vec<String> {
    let hyphenated = if recipe.template.contains(HYPHENATED_ISBN) {
        match hyphenated_isbn(isbn) {
            Some(h) => h,
            None => return Vec::new(),
        }
    } else {
        String::new()
    };
    let authors_tried = if recipe.template.contains("{author}") {
        author_variants(authors)
    } else {
        vec![String::new()]
    };
    // A book without a title yet (scanned before its details came) still
    // has an address under a template that needs no slug.
    let mut titles = title_variants(title);
    if titles.is_empty() && !recipe.template.contains("{slug}") {
        titles.push(String::new());
    }
    let mut out: Vec<String> = Vec::new();
    for t in titles {
        for a in &authors_tried {
            let u = recipe
                .template
                .replace("{isbn}", isbn)
                .replace(HYPHENATED_ISBN, &hyphenated)
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
/// asked, carries the ISBN, and is not a search's own result page (`/?s=978…`
/// redirected to `/search/978…/` still lists results).
pub fn redirected_to_book(page: &Page, asked: &Url, isbn: &str) -> bool {
    let fin = page.url.path().trim_end_matches('/');
    let searching = fin.split('/').any(|segment| {
        let segment = segment.to_ascii_lowercase();
        SEARCH_SEGMENTS.iter().any(|w| segment.contains(w))
    });
    fin != asked.path().trim_end_matches('/') && fin.contains(isbn) && !searching
}

/// Path words of a search's result page, in the languages of the table.
const SEARCH_SEGMENTS: [&str; 4] = ["search", "recherche", "resultat", "result"];

#[cfg(test)]
mod tests {
    use super::super::lookup::MAX_PRODUCT_TRIES;
    use super::super::text::Apostrophe;
    use super::*;

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
    fn a_search_result_path_with_the_isbn_is_not_a_redirect_to_the_book() {
        let asked = Url::parse("https://e.example/search/9782000000011").unwrap();
        let same = Page {
            url: asked.clone(),
            html: String::new(),
            truncated: false,
        };
        assert!(!redirected_to_book(&same, &asked, "9782000000011"));
        let book = Page {
            url: Url::parse("https://e.example/livre/x-9782000000011").unwrap(),
            html: String::new(),
            truncated: false,
        };
        assert!(redirected_to_book(&book, &asked, "9782000000011"));
        // A query sent to its result page under another path.
        let wp = Url::parse("https://e.example/?s=9782000000011").unwrap();
        let results = Page {
            url: Url::parse("https://e.example/search/9782000000011/").unwrap(),
            html: String::new(),
            truncated: false,
        };
        assert!(!redirected_to_book(&results, &wp, "9782000000011"));
    }

    #[test]
    fn a_lookup_tries_each_part_of_a_cut_title_alone() {
        let recipe = ProductRecipe {
            template: "https://e.example/livre/{slug}".into(),
            apostrophe: Apostrophe::Drop,
            slug_ignored: false,
        };
        let tried = |title: &str| {
            let urls = product_urls(&recipe, "9782000000011", title, "Joëlle Zask");
            urls.into_iter().take(MAX_PRODUCT_TRIES).collect::<Vec<_>>()
        };
        let main = "https://e.example/livre/donner-a-manger".to_owned();
        let subtitle = "https://e.example/livre/politique-dun-geste-ordinaire".to_owned();
        for title in [
            "Donner à manger - Politique d'un geste ordinaire",
            "Donner à manger : Politique d'un geste ordinaire",
        ] {
            let urls = tried(title);
            assert!(urls.contains(&main) && urls.contains(&subtitle), "{title}");
        }
        // A series named first, without a volume number, still has the
        // book's own title tried.
        assert!(
            tried("Une enquête du commissaire Brunetti : L'Épreuve du feu")
                .contains(&"https://e.example/livre/lepreuve-du-feu".to_owned())
        );
    }

    #[test]
    fn a_book_without_a_title_still_has_an_isbn_address() {
        let recipe = |template: &str| ProductRecipe {
            template: template.into(),
            apostrophe: Apostrophe::Drop,
            slug_ignored: false,
        };
        assert_eq!(
            product_urls(
                &recipe("https://e.example/product/{isbn}"),
                "9782000000011",
                "",
                ""
            ),
            ["https://e.example/product/9782000000011"]
        );
        assert!(
            product_urls(
                &recipe("https://e.example/{slug}-{isbn}"),
                "9782000000011",
                "",
                ""
            )
            .is_empty()
        );
    }

    #[test]
    fn a_template_can_ask_for_the_hyphenated_isbn() {
        let recipe = ProductRecipe {
            template: "https://e.example/index.php?spec=livre&ISBN={isbn_hyphens}".into(),
            apostrophe: Apostrophe::Drop,
            slug_ignored: false,
        };
        assert_eq!(
            product_urls(&recipe, "9782818055113", "Finale Fantaisie", ""),
            ["https://e.example/index.php?spec=livre&ISBN=978-2-8180-5511-3"]
        );
        // Another ISBN group: no hyphens known, no address.
        assert!(product_urls(&recipe, "9780141439518", "Emma", "").is_empty());
    }
}
