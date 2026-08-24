//! Inputs of the external discovery lookups (ADR-060).
//!
//! Turns the local library into what the Flutter side needs to query the
//! hub discovery resolver: one lookup per owned series collection and per
//! favorite author (each anchored by up to 3 checksum-valid ISBNs), plus
//! the library-wide identity index used to filter the answers. The
//! privacy contract is enforced by construction: only anchor ISBNs and a
//! name can transit, never the taste profile.
//!
//! The Dart side mirrors [`normalize_identity_text`] to key the resolver's
//! volumes/works before matching them against the index; the two
//! implementations must stay in sync (see `DiscoveryService` in the app).

use std::collections::{BTreeSet, HashMap};

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use unicode_normalization::UnicodeNormalization;

use crate::domain::recommendations::{
    DiscoveryAuthorLookup, DiscoveryLookupInputs, DiscoverySeriesLookup,
};
use crate::services::book_service::ServiceError;
use crate::services::recommendation_service::{
    self, MIN_SCORED_BOOKS, ScoringBook, build_taste_profile,
};
use crate::utils::isbn;

/// Up to 3 anchor ISBNs per lookup (ADR-060 decision 3): same precision as
/// one anchor, materially better recall on sub-series and sparse sources.
pub const ANCHOR_ISBNS_MAX: usize = 3;

/// At most this many favorite-author lookups per sweep, taken in profile
/// order (liked-book count desc). Bounds the request fan-out.
pub const AUTHOR_LOOKUPS_MAX: usize = 5;

/// A favorite author qualifies for a lookup from 2 liked books: a single
/// liked book is too weak a signal to call someone a favorite author.
pub const AUTHOR_MIN_LIKED_BOOKS: u32 = 2;

/// One series-typed collection with its membership rows, as loaded by
/// [`load_series_collections`]: `(book_id, volume_number, added_at)`.
#[derive(Debug, Clone)]
pub struct SeriesCollectionMembers {
    pub collection_id: String,
    pub name: String,
    pub members: Vec<(String, Option<i32>, String)>,
}

/// Normalization shared by both halves of the identity matching rule:
/// lowercase, fold diacritics (NFD, combining marks dropped), split on any
/// non-alphanumeric character, join the words with single spaces. The Dart
/// side applies the same steps to the resolver's titles and authors; keep
/// the algorithm boring so the mirror stays exact.
pub fn normalize_identity_text(s: &str) -> String {
    s.to_lowercase()
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect::<String>()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One "title|author" identity key per author. A book with no usable title
/// or no author yields no key: title-only matching would be too loose, and
/// the ISBN half of the rule still covers most of those books.
pub fn title_author_keys(title: &str, authors: &[String]) -> Vec<String> {
    let title = normalize_identity_text(title);
    if title.is_empty() {
        return Vec::new();
    }
    authors
        .iter()
        .map(|a| normalize_identity_text(a))
        .filter(|a| !a.is_empty())
        .map(|a| format!("{title}|{a}"))
        .collect()
}

/// Both comparable forms of a stored ISBN: canonical ISBN-13 plus the
/// ISBN-10 form when one exists. An unparseable value is kept as its
/// separator-stripped self so it only ever matches itself (the wishlist
/// canonical-ISBN convention).
fn expanded_isbn_forms(raw: &str) -> Vec<String> {
    match isbn::to_isbn13(raw) {
        Some(c13) => {
            let mut forms = vec![c13];
            if let Some(c10) = isbn::to_isbn10(raw) {
                forms.push(c10);
            }
            forms
        }
        None => {
            let cleaned = raw.trim().replace(['-', ' '], "").to_uppercase();
            if cleaned.is_empty() {
                Vec::new()
            } else {
                vec![cleaned]
            }
        }
    }
}

fn push_anchor(anchors: &mut Vec<String>, raw_isbn: Option<&str>) {
    if anchors.len() >= ANCHOR_ISBNS_MAX {
        return;
    }
    if let Some(c13) = raw_isbn.and_then(isbn::to_isbn13)
        && !anchors.contains(&c13)
    {
        anchors.push(c13);
    }
}

/// Pure builder: derive every discovery lookup and the identity index from
/// one already-loaded library pass. Returns the empty default below the
/// ADR-059 profile threshold: no external lookups without local signal.
pub fn build_discovery_lookup_inputs(
    books: &[ScoringBook],
    series: &[SeriesCollectionMembers],
) -> DiscoveryLookupInputs {
    let profile = build_taste_profile(books);
    if profile.scored_books_count < MIN_SCORED_BOOKS {
        return DiscoveryLookupInputs::default();
    }

    let by_id: HashMap<&str, &ScoringBook> = books
        .iter()
        .filter_map(|sb| sb.book.id.as_deref().map(|id| (id, sb)))
        .collect();

    let mut library_isbns = BTreeSet::new();
    let mut library_keys = BTreeSet::new();
    // The liked halves are a strict subset of the two above, filled in the
    // same pass (ADR-066). `is_liked` is the engine's own predicate, so the
    // editorial affinity tier and the recommendation engine cannot disagree
    // on what "liked" means.
    let mut liked_isbns = BTreeSet::new();
    let mut liked_keys = BTreeSet::new();
    for sb in books {
        let liked = sb.is_liked();
        if let Some(raw) = sb.book.isbn.as_deref() {
            let forms = expanded_isbn_forms(raw);
            if liked {
                liked_isbns.extend(forms.iter().cloned());
            }
            library_isbns.extend(forms);
        }
        let authors = sb.book.authors.as_deref().unwrap_or(&[]);
        let keys = title_author_keys(&sb.book.title, authors);
        if liked {
            liked_keys.extend(keys.iter().cloned());
        }
        library_keys.extend(keys);
    }

    let mut series_lookups = Vec::new();
    for sc in series {
        let mut members: Vec<(&ScoringBook, Option<i32>, &str)> = sc
            .members
            .iter()
            .filter_map(|(book_id, volume, added_at)| {
                by_id
                    .get(book_id.as_str())
                    .map(|sb| (*sb, *volume, added_at.as_str()))
            })
            .collect();
        // Numbered volumes first (ascending), unnumbered after by insertion
        // time: the collection repository's ordering, so anchors are the
        // earliest volumes and the output is deterministic.
        members.sort_by(|a, b| {
            (a.1.is_none(), a.1)
                .cmp(&(b.1.is_none(), b.1))
                .then_with(|| a.2.cmp(b.2))
        });

        let mut anchors = Vec::new();
        let mut member_isbns = BTreeSet::new();
        let mut member_keys = BTreeSet::new();
        for (sb, _, _) in &members {
            push_anchor(&mut anchors, sb.book.isbn.as_deref());
            if let Some(raw) = sb.book.isbn.as_deref() {
                member_isbns.extend(expanded_isbn_forms(raw));
            }
            let authors = sb.book.authors.as_deref().unwrap_or(&[]);
            member_keys.extend(title_author_keys(&sb.book.title, authors));
        }
        // A series none of whose members carries a valid ISBN produces no
        // lookup at all (ADR-060 section 3.2): no anchor, no request.
        if anchors.is_empty() {
            continue;
        }
        series_lookups.push(DiscoverySeriesLookup {
            collection_id: sc.collection_id.clone(),
            name: sc.name.clone(),
            anchor_isbns: anchors,
            member_isbns: member_isbns.into_iter().collect(),
            member_title_author_keys: member_keys.into_iter().collect(),
        });
    }

    let mut author_lookups = Vec::new();
    for (name, liked_count) in &profile.favorite_authors {
        if author_lookups.len() >= AUTHOR_LOOKUPS_MAX {
            break;
        }
        // favorite_authors is sorted by liked-book count descending, so the
        // first author below the threshold ends the sweep.
        if *liked_count < AUTHOR_MIN_LIKED_BOOKS {
            break;
        }
        let norm_name = recommendation_service::norm(name);
        let mut liked: Vec<&ScoringBook> = books
            .iter()
            .filter(|sb| sb.is_liked() && sb.norm_authors.contains(&norm_name))
            .collect();
        liked.sort_by(|a, b| a.book.title.cmp(&b.book.title));

        let mut anchors = Vec::new();
        for sb in &liked {
            push_anchor(&mut anchors, sb.book.isbn.as_deref());
        }
        // A favorite author whose liked books all lack a valid ISBN produces
        // no lookup: no anchor, no request, nothing shown.
        if anchors.is_empty() {
            continue;
        }
        author_lookups.push(DiscoveryAuthorLookup {
            name: name.clone(),
            anchor_isbns: anchors,
        });
    }

    DiscoveryLookupInputs {
        series: series_lookups,
        authors: author_lookups,
        library_isbns: library_isbns.into_iter().collect(),
        library_title_author_keys: library_keys.into_iter().collect(),
        liked_isbns: liked_isbns.into_iter().collect(),
        liked_title_author_keys: liked_keys.into_iter().collect(),
    }
}

/// Load every series-typed collection with its membership rows: one query
/// for the collections, one for the join table.
pub async fn load_series_collections(
    db: &DatabaseConnection,
) -> Result<Vec<SeriesCollectionMembers>, ServiceError> {
    use crate::models::{collection, collection_book};

    let collections = collection::Entity::find()
        .filter(collection::Column::Source.eq("series"))
        .all(db)
        .await?;
    if collections.is_empty() {
        return Ok(Vec::new());
    }

    let rows = collection_book::Entity::find().all(db).await?;
    let mut by_collection: HashMap<String, Vec<(String, Option<i32>, String)>> = HashMap::new();
    for row in rows {
        by_collection.entry(row.collection_id).or_default().push((
            row.book_id,
            row.volume_number,
            row.added_at,
        ));
    }

    Ok(collections
        .into_iter()
        .map(|c| {
            let members = by_collection.remove(&c.id).unwrap_or_default();
            SeriesCollectionMembers {
                collection_id: c.id,
                name: c.name,
                members,
            }
        })
        .collect())
}

/// DB entry point: one library pass (reusing the recommendation loader)
/// plus the series collections, then the pure builder.
pub async fn discovery_lookup_inputs(
    db: &DatabaseConnection,
) -> Result<DiscoveryLookupInputs, ServiceError> {
    let books = recommendation_service::load_scoring_books(db).await?;
    let series = load_series_collections(db).await?;
    Ok(build_discovery_lookup_inputs(&books, &series))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::book::Book;

    // Valid ISBN-13s (checksum-correct) used across the tests.
    const ISBN_A: &str = "9782070541270";
    const ISBN_B: &str = "9780306406157";
    const ISBN_C: &str = "9780441007318";
    const ISBN_D: &str = "9782253006329";

    fn scoring_book(id: &str, title: &str, authors: &[&str], status: &str) -> ScoringBook {
        let book = Book {
            id: Some(id.to_string()),
            title: title.to_string(),
            authors: Some(authors.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };
        ScoringBook::new(book, status.to_string())
    }

    fn with_isbn(mut sb: ScoringBook, isbn: &str) -> ScoringBook {
        sb.book.isbn = Some(isbn.to_string());
        sb
    }

    /// Five read books by the same author: enough profile signal to open
    /// the gate, and a 5-liked-book favorite author.
    fn gated_library() -> Vec<ScoringBook> {
        (1..=5)
            .map(|i| {
                let sb = scoring_book(
                    &format!("b{i}"),
                    &format!("Book {i}"),
                    &["Ursula K. Le Guin"],
                    "read",
                );
                if i == 1 { with_isbn(sb, ISBN_A) } else { sb }
            })
            .collect()
    }

    fn series(id: &str, name: &str, members: &[(&str, Option<i32>)]) -> SeriesCollectionMembers {
        SeriesCollectionMembers {
            collection_id: id.to_string(),
            name: name.to_string(),
            members: members
                .iter()
                .enumerate()
                .map(|(i, (book_id, vol))| {
                    (book_id.to_string(), *vol, format!("2026-01-0{}", i + 1))
                })
                .collect(),
        }
    }

    // ── normalization (mirrored in Dart, keep fixtures in sync) ─────

    #[test]
    fn identity_text_folds_case_diacritics_and_punctuation() {
        assert_eq!(normalize_identity_text("L'Étranger"), "l etranger");
        assert_eq!(
            normalize_identity_text("  Harry Potter, tome 3 "),
            "harry potter tome 3"
        );
        assert_eq!(normalize_identity_text("J. K. Rowling"), "j k rowling");
        assert_eq!(normalize_identity_text("çà-et-là"), "ca et la");
    }

    #[test]
    fn title_author_keys_one_per_author_skipping_empty() {
        let keys = title_author_keys("Dune", &["Frank Herbert".to_string(), "  ".to_string()]);
        assert_eq!(keys, vec!["dune|frank herbert".to_string()]);
        assert!(title_author_keys("", &["Frank Herbert".to_string()]).is_empty());
        assert!(title_author_keys("Dune", &[]).is_empty());
    }

    // ── gating ──────────────────────────────────────────────────────

    #[test]
    fn below_profile_threshold_everything_is_empty() {
        let books = vec![
            with_isbn(scoring_book("b1", "Book 1", &["A"], "read"), ISBN_A),
            scoring_book("b2", "Book 2", &["A"], "read"),
        ];
        let sc = series("c1", "Cycle", &[("b1", Some(1))]);
        let inputs = build_discovery_lookup_inputs(&books, &[sc]);
        assert!(inputs.series.is_empty());
        assert!(inputs.authors.is_empty());
        assert!(inputs.library_isbns.is_empty());
        assert!(inputs.library_title_author_keys.is_empty());
    }

    // ── identity index ──────────────────────────────────────────────

    #[test]
    fn identity_index_expands_isbn_forms_and_keys_all_statuses() {
        let mut books = gated_library();
        books.push(with_isbn(
            scoring_book("w1", "The Dispossessed", &["Ursula K. Le Guin"], "wanting"),
            "0-441-00731-7",
        ));
        let inputs = build_discovery_lookup_inputs(&books, &[]);
        // Both forms of the wanting book's ISBN-10 are present (no other
        // library book carries this ISBN, so both trace to w1).
        assert!(inputs.library_isbns.contains(&"9780441007318".to_string()));
        assert!(inputs.library_isbns.contains(&"0441007317".to_string()));
        assert!(
            inputs
                .library_title_author_keys
                .contains(&"the dispossessed|ursula k le guin".to_string())
        );
    }

    // ── series lookups ──────────────────────────────────────────────

    #[test]
    fn series_anchors_cap_at_three_in_volume_order() {
        let mut books = gated_library();
        books.push(with_isbn(
            scoring_book("v1", "Vol 1", &["A"], "read"),
            ISBN_A,
        ));
        books.push(with_isbn(
            scoring_book("v2", "Vol 2", &["A"], "read"),
            ISBN_B,
        ));
        books.push(with_isbn(
            scoring_book("v3", "Vol 3", &["A"], "read"),
            ISBN_C,
        ));
        books.push(with_isbn(
            scoring_book("v4", "Vol 4", &["A"], "read"),
            ISBN_D,
        ));
        // Deliberately shuffled member order; volume numbers must win.
        let sc = series(
            "c1",
            "Cycle",
            &[
                ("v4", Some(4)),
                ("v2", Some(2)),
                ("v1", Some(1)),
                ("v3", Some(3)),
            ],
        );
        let inputs = build_discovery_lookup_inputs(&books, &[sc]);
        assert_eq!(inputs.series.len(), 1);
        assert_eq!(
            inputs.series[0].anchor_isbns,
            vec![ISBN_A.to_string(), ISBN_B.to_string(), ISBN_C.to_string()]
        );
        assert_eq!(inputs.series[0].name, "Cycle");
        assert!(inputs.series[0].member_isbns.contains(&ISBN_D.to_string()));
        assert!(
            inputs.series[0]
                .member_title_author_keys
                .contains(&"vol 4|a".to_string())
        );
    }

    #[test]
    fn series_without_any_valid_member_isbn_produces_no_lookup() {
        let mut books = gated_library();
        books.push(scoring_book("v1", "Vol 1", &["A"], "read"));
        books.push(with_isbn(
            scoring_book("v2", "Vol 2", &["A"], "read"),
            "not-an-isbn",
        ));
        let sc = series("c1", "Cycle", &[("v1", Some(1)), ("v2", Some(2))]);
        let inputs = build_discovery_lookup_inputs(&books, &[sc]);
        assert!(inputs.series.is_empty());
    }

    #[test]
    fn series_member_missing_from_library_is_ignored() {
        let mut books = gated_library();
        books.push(with_isbn(
            scoring_book("v1", "Vol 1", &["A"], "read"),
            ISBN_A,
        ));
        let sc = series("c1", "Cycle", &[("v1", Some(1)), ("ghost", Some(2))]);
        let inputs = build_discovery_lookup_inputs(&books, &[sc]);
        assert_eq!(inputs.series.len(), 1);
        assert_eq!(inputs.series[0].anchor_isbns, vec![ISBN_A.to_string()]);
    }

    // ── author lookups ──────────────────────────────────────────────

    #[test]
    fn favorite_author_gets_anchors_from_liked_books_only() {
        let mut books = gated_library();
        // Unread book by the same author: must not anchor.
        books.push(with_isbn(
            scoring_book("u1", "Unread", &["Ursula K. Le Guin"], "to_read"),
            ISBN_A,
        ));
        let inputs = build_discovery_lookup_inputs(&books, &[]);
        assert_eq!(inputs.authors.len(), 1);
        assert_eq!(inputs.authors[0].name, "Ursula K. Le Guin");
        assert_eq!(inputs.authors[0].anchor_isbns, vec![ISBN_A.to_string()]);
    }

    #[test]
    fn single_liked_book_author_is_below_threshold() {
        let mut books = gated_library();
        books.push(with_isbn(
            scoring_book("s1", "Solo", &["One Hit"], "read"),
            ISBN_A,
        ));
        let inputs = build_discovery_lookup_inputs(&books, &[]);
        assert!(inputs.authors.iter().all(|a| a.name != "One Hit"));
    }

    #[test]
    fn author_without_valid_isbn_produces_no_lookup() {
        let books: Vec<ScoringBook> = (1..=5)
            .map(|i| scoring_book(&format!("b{i}"), &format!("Book {i}"), &["No Isbn"], "read"))
            .collect();
        let inputs = build_discovery_lookup_inputs(&books, &[]);
        assert!(inputs.authors.is_empty());
    }

    #[test]
    fn author_lookups_cap_at_five() {
        let mut books = Vec::new();
        for a in 0..8 {
            for i in 0..2 {
                books.push(with_isbn(
                    scoring_book(
                        &format!("a{a}b{i}"),
                        &format!("Book {a}-{i}"),
                        &[&format!("Author {a}")],
                        "read",
                    ),
                    ISBN_A,
                ));
            }
        }
        let inputs = build_discovery_lookup_inputs(&books, &[]);
        assert_eq!(inputs.authors.len(), AUTHOR_LOOKUPS_MAX);
    }

    // ── liked index halves (ADR-066) ────────────────────────────────

    #[test]
    fn liked_index_is_a_strict_subset_of_the_library_index() {
        let mut books = gated_library();
        // Owned but not liked: to_read, no rating, no favorites shelf.
        books.push(with_isbn(
            scoring_book("u1", "Unread One", &["Alia Sun"], "to_read"),
            ISBN_C,
        ));
        let inputs = build_discovery_lookup_inputs(&books, &[]);

        // The read book carrying ISBN_A is liked through the ADR-059
        // fallback (no ratings anywhere in this fixture).
        assert!(inputs.liked_isbns.contains(&"9782070541270".to_string()));
        assert!(
            inputs
                .liked_title_author_keys
                .contains(&"book 1|ursula k le guin".to_string())
        );

        // The unread book is in the library index and NOT in the liked one.
        assert!(inputs.library_isbns.contains(&"9780441007318".to_string()));
        assert!(!inputs.liked_isbns.contains(&"9780441007318".to_string()));
        assert!(
            inputs
                .library_title_author_keys
                .contains(&"unread one|alia sun".to_string())
        );
        assert!(
            !inputs
                .liked_title_author_keys
                .contains(&"unread one|alia sun".to_string())
        );

        // Subset, always: the affinity tier counts liked books among the
        // owned ones, so a liked key outside the library index would mean
        // an overlap that can exceed its own total.
        for isbn in &inputs.liked_isbns {
            assert!(inputs.library_isbns.contains(isbn));
        }
        for key in &inputs.liked_title_author_keys {
            assert!(inputs.library_title_author_keys.contains(key));
        }
    }

    #[test]
    fn liked_index_is_empty_below_the_profile_threshold() {
        let books = vec![with_isbn(
            scoring_book("b1", "Book 1", &["A"], "read"),
            ISBN_A,
        )];
        let inputs = build_discovery_lookup_inputs(&books, &[]);
        assert!(inputs.liked_isbns.is_empty());
        assert!(inputs.liked_title_author_keys.is_empty());
    }

    #[test]
    fn an_explicit_low_rating_vetoes_the_read_fallback_in_the_liked_index() {
        let mut books = gated_library();
        let mut vetoed = scoring_book("v1", "Disliked", &["Alia Sun"], "read");
        vetoed.book.user_rating = Some(2);
        books.push(with_isbn(vetoed, ISBN_D));
        let inputs = build_discovery_lookup_inputs(&books, &[]);
        assert!(inputs.library_isbns.contains(&"9782253006329".to_string()));
        assert!(!inputs.liked_isbns.contains(&"9782253006329".to_string()));
    }
}
