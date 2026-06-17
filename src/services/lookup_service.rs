//! Lookup Service — multi-source ISBN metadata resolution
//!
//! Extracted from `api/lookup.rs` so it can be called from both
//! the HTTP handler and FFI (metadata refresh feature).

use sea_orm::DatabaseConnection;

use crate::openlibrary::BookMetadata;

/// Resolve book metadata by ISBN, querying multiple sources in priority order.
///
/// Source priority depends on ISBN origin and user language:
/// - French ISBNs (978-2…): BNF SPARQL → SUDOC → BNF SRU → Inventaire → OpenLibrary → Google
/// - Others: Inventaire → OpenLibrary → BNF (if user lang is French) → Google
///
/// Returns `Ok(None)` when no source has data for this ISBN.
pub async fn lookup_metadata_by_isbn(
    db: &DatabaseConnection,
    isbn: &str,
    user_lang: Option<&str>,
) -> Result<Option<BookMetadata>, String> {
    use crate::models::installation_profile::Entity as ProfileEntity;
    use sea_orm::EntityTrait;

    // Load profile config to check enabled providers and API keys
    let (enable_openlibrary, enable_google, enable_inventaire, enable_bnf, google_api_key) =
        match ProfileEntity::find_by_id(1).one(db).await {
            Ok(Some(profile_model)) => {
                let modules: Vec<String> =
                    serde_json::from_str(&profile_model.enabled_modules).unwrap_or_default();
                let api_keys: std::collections::HashMap<String, String> = profile_model
                    .api_keys
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                (
                    !modules.contains(&"disable_fallback:openlibrary".to_string()),
                    modules.contains(&"enable_google_books".to_string()),
                    !modules.contains(&"disable_fallback:inventaire".to_string()),
                    !modules.contains(&"disable_fallback:bnf".to_string()),
                    api_keys.get("google_books").cloned(),
                )
            }
            _ => (true, false, true, true, None),
        };

    let clean_isbn = isbn.replace('-', "");
    let is_french_isbn = clean_isbn.starts_with("9782") || clean_isbn.starts_with("97910");

    // `user_lang` now carries the user's reading languages comma-separated (the
    // Flutter clients send `userLanguages`, mirroring federated search); older
    // callers still pass a single interface code, which parses to a one-item list.
    let user_langs: Vec<String> = user_lang
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let user_lang_is_french = user_langs.iter().any(|l| l.starts_with("fr"));

    // ── Phase 1: pick the PRIMARY source via the existing priority chain ──
    // The order is unchanged from the historical first-wins behaviour: the first
    // source that responds becomes the authoritative primary. `source` records
    // which one won so the gap-fill round below can skip re-querying it.
    let mut primary: Option<(BookMetadata, Source)> = None;

    // For French ISBNs, try BNF first (better coverage for French publishers)
    if enable_bnf && is_french_isbn {
        if primary.is_none()
            && let Some(m) = try_bnf_sparql(
                &clean_isbn,
                isbn,
                enable_openlibrary,
                enable_google,
                enable_inventaire,
                google_api_key.as_deref(),
            )
            .await
        {
            primary = Some((m, Source::Bnf));
        }
        if primary.is_none()
            && let Some(m) = try_sudoc(
                &clean_isbn,
                isbn,
                enable_openlibrary,
                enable_google,
                enable_inventaire,
                google_api_key.as_deref(),
            )
            .await
        {
            primary = Some((m, Source::Sudoc));
        }
        if primary.is_none()
            && let Some(m) = try_bnf_sru(
                &clean_isbn,
                isbn,
                enable_openlibrary,
                enable_google,
                enable_inventaire,
                google_api_key.as_deref(),
            )
            .await
        {
            primary = Some((m, Source::BnfSru));
        }
    }

    // 1. Try Inventaire
    if primary.is_none()
        && enable_inventaire
        && let Some(m) = try_inventaire(
            isbn,
            enable_openlibrary,
            enable_google,
            google_api_key.as_deref(),
        )
        .await
    {
        primary = Some((m, Source::Inventaire));
    }

    // 2. Fallback to OpenLibrary
    if primary.is_none()
        && enable_openlibrary
        && let Some(m) = try_openlibrary(isbn, enable_google, google_api_key.as_deref()).await
    {
        primary = Some((m, Source::OpenLibrary));
    }

    // 3. BNF for non-French ISBNs if user language is French
    if primary.is_none()
        && enable_bnf
        && user_lang_is_french
        && !is_french_isbn
        && let Some(m) = try_bnf_sparql(
            &clean_isbn,
            isbn,
            enable_openlibrary,
            enable_google,
            enable_inventaire,
            google_api_key.as_deref(),
        )
        .await
    {
        primary = Some((m, Source::Bnf));
    }

    // 4. Fallback to Google Books
    if primary.is_none() && enable_google {
        match crate::google_books::fetch_book_metadata(isbn, google_api_key.as_deref()).await {
            Ok(metadata) => primary = Some((metadata, Source::Google)),
            Err(e) => {
                tracing::debug!("Google Books lookup failed for {}: {}", isbn, e);
            }
        }
    }

    let Some((primary, source)) = primary else {
        return Ok(None);
    };

    // Single target language for summary coherence (ADR-040 cascade): ISBN
    // registration group → title language → first reading language. Used below to
    // reject a wrong-language summary recovered during gap-fill.
    let target_lang =
        crate::utils::lang::target_summary_language(isbn, &primary.title, &user_langs);

    // ── Phase 2: gap-fill the fields the primary left empty ──
    // Fills ONLY empty light-metadata fields from secondary sources; never
    // overwrites a value the primary set. No network call when nothing is missing.
    let filled = gap_fill_metadata(
        primary,
        source,
        isbn,
        target_lang.as_deref(),
        enable_openlibrary,
        enable_google,
        enable_inventaire,
        google_api_key.as_deref(),
    )
    .await;

    Ok(Some(filled))
}

/// Which source produced the primary record, so the gap-fill round can skip it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Bnf,
    Sudoc,
    BnfSru,
    Inventaire,
    OpenLibrary,
    Google,
}

/// The subset of metadata fields the gap-fill round can recover from a secondary
/// source. Kept separate from `BookMetadata` so secondaries need not reconstruct
/// the authoritative fields (title/authors) that gap-fill never touches.
#[derive(Default)]
struct GapFields {
    summary: Option<String>,
    publisher: Option<String>,
    page_count: Option<u32>,
    publication_year: Option<String>,
    cover_url: Option<String>,
}

fn gap_from_book(m: BookMetadata) -> GapFields {
    GapFields {
        summary: m.summary,
        publisher: m.publisher,
        page_count: m.page_count,
        publication_year: m.publication_year,
        cover_url: m.cover_url,
    }
}

fn gap_from_inventaire(m: crate::inventaire_client::InventaireMetadata) -> GapFields {
    GapFields {
        // Inventaire is deliberately NOT a summary source: its `summary` is the
        // one-line Wikidata *description* (e.g. "roman d'Albert Camus"), not prose.
        // It still contributes publisher / year / page count / cover here (its
        // publisher URI is already resolved to a human-readable label upstream).
        summary: None,
        publisher: m.publisher,
        page_count: m.page_count,
        publication_year: m.publication_year,
        cover_url: m.cover_url,
    }
}

/// Drop a gap candidate's summary when it is reliably detected to be in a language
/// other than the target. When the target is unknown or the text language cannot be
/// reliably detected, the summary is kept (no basis to reject → no regression).
///
/// This is the ADR-040 language-coherence gate: a wrong-language auto-summary is
/// worse than none (the add/scan form opens immediately for manual entry anyway).
fn enforce_summary_language(gap: &mut Option<GapFields>, target: Option<&str>) {
    let (Some(g), Some(target)) = (gap.as_mut(), target) else {
        return;
    };
    let Some(text) = g.summary.as_deref() else {
        return;
    };
    if let Some(detected) = crate::utils::lang::detect_text_lang(text)
        && !crate::utils::lang::lang_matches(&detected, target)
    {
        g.summary = None;
    }
}

/// Fill ONLY the empty fields of `primary` from `gap` (None → Some only).
///
/// This is the gap-fill invariant: the authoritative primary source keeps every
/// field it populated; a secondary can only turn a `None` into a `Some`. Accuracy
/// therefore cannot regress relative to the previous first-wins behaviour.
fn fill_empty_fields(primary: &mut BookMetadata, gap: GapFields) {
    if primary.summary.is_none() {
        primary.summary = gap.summary;
    }
    if primary.publisher.is_none() {
        primary.publisher = gap.publisher;
    }
    if primary.page_count.is_none() {
        primary.page_count = gap.page_count;
    }
    if primary.publication_year.is_none() {
        primary.publication_year = gap.publication_year;
    }
    if primary.cover_url.is_none() {
        primary.cover_url = gap.cover_url;
    }
}

/// Recover the light-metadata fields the primary left empty from the fast
/// secondary sources (Inventaire / OpenLibrary / Google), in a single bounded
/// parallel round. None-only fill, so the primary's values are never overwritten.
///
/// SUDOC is deliberately excluded from this round: it never returns a summary
/// (the field driving the user reports), is a slow two-call lookup that would
/// dominate the parallel round's latency, and the publication year it could add
/// is already covered by the faster trio. Covers are already maximised on the
/// primary by `enrich_cover`, so the only practical gains here are summary /
/// page count / year.
///
/// `target_lang` gates recovered summaries: an OpenLibrary/Google summary is only
/// accepted when its detected language matches the target (ADR-040). It does NOT
/// touch the primary's own summary, which stays authoritative.
// The arguments are the same flat provider toggles every sibling `try_*` helper in
// this module already threads through; bundling them into a struct here alone would
// diverge from that established shape for no real readability gain.
#[allow(clippy::too_many_arguments)]
async fn gap_fill_metadata(
    mut primary: BookMetadata,
    source: Source,
    isbn: &str,
    target_lang: Option<&str>,
    enable_openlibrary: bool,
    enable_google: bool,
    enable_inventaire: bool,
    google_api_key: Option<&str>,
) -> BookMetadata {
    // Short-circuit (zero network) when the primary already carries every field.
    if primary.summary.is_some()
        && primary.publisher.is_some()
        && primary.page_count.is_some()
        && primary.publication_year.is_some()
        && primary.cover_url.is_some()
    {
        return primary;
    }

    let query_inventaire = enable_inventaire && source != Source::Inventaire;
    let query_openlibrary = enable_openlibrary && source != Source::OpenLibrary;
    let query_google = enable_google && source != Source::Google;

    let inv_fut = async {
        if query_inventaire {
            crate::inventaire_client::fetch_inventaire_metadata(isbn)
                .await
                .ok()
                .map(gap_from_inventaire)
        } else {
            None
        }
    };
    let ol_fut = async {
        if query_openlibrary {
            crate::openlibrary::fetch_book_metadata(isbn)
                .await
                .ok()
                .map(gap_from_book)
        } else {
            None
        }
    };
    let gb_fut = async {
        if query_google {
            crate::google_books::fetch_book_metadata(isbn, google_api_key)
                .await
                .ok()
                .map(gap_from_book)
        } else {
            None
        }
    };

    let (inv, mut ol, mut gb) = tokio::join!(inv_fut, ol_fut, gb_fut);

    // Language gate: reject an OpenLibrary/Google summary that is not in the target
    // language before it can fill the gap (Inventaire carries no summary here).
    enforce_summary_language(&mut ol, target_lang);
    enforce_summary_language(&mut gb, target_lang);

    // Merge precedence: OpenLibrary → Google → Inventaire. OpenLibrary and Google
    // are the only prose-summary sources; Inventaire contributes year / page count
    // / cover only. Since fill is None-only (first value for a field wins) the order
    // matters solely for fields more than one source provides — here the primary
    // usually already set year / page count / cover, so fill touches only leftovers.
    for gap in [ol, gb, inv].into_iter().flatten() {
        fill_empty_fields(&mut primary, gap);
    }
    primary
}

// ─── Internal helpers ───────────────────────────────────────────────

fn make_authors_from_name(name: Option<String>) -> Vec<crate::inventaire_client::AuthorMetadata> {
    name.map(|n| {
        vec![crate::inventaire_client::AuthorMetadata {
            name: n,
            birth_year: None,
            death_year: None,
            image_url: None,
            bio: None,
        }]
    })
    .unwrap_or_default()
}

async fn enrich_cover(
    isbn: &str,
    cover_url: Option<String>,
    enable_openlibrary: bool,
    enable_google: bool,
    enable_inventaire: bool,
    google_api_key: Option<&str>,
) -> Option<String> {
    if cover_url.is_some() {
        return cover_url;
    }

    // Try cover sources in parallel to maximize chances of finding one. Each
    // source is queried with the scanned ISBN and, on a miss, the alternate
    // ISBN-10/13 form (sources index covers under one specific form).
    let inv_fut = async {
        if enable_inventaire {
            cover_with_isbn_fallback(isbn, |i| async move {
                crate::inventaire_client::fetch_cover_url(&i).await
            })
            .await
        } else {
            None
        }
    };
    let ol_fut = async {
        if enable_openlibrary {
            cover_with_isbn_fallback(isbn, |i| async move {
                crate::openlibrary::fetch_cover_url(&i).await
            })
            .await
        } else {
            None
        }
    };
    let gb_fut = async {
        if enable_google {
            let key = google_api_key.map(|k| k.to_string());
            cover_with_isbn_fallback(isbn, move |i| {
                let key = key.clone();
                async move { crate::google_books::fetch_cover_url(&i, key.as_deref()).await }
            })
            .await
        } else {
            None
        }
    };

    let (inv_result, ol_result, gb_result) = tokio::join!(inv_fut, ol_fut, gb_fut);
    inv_result.or(ol_result).or(gb_result)
}

/// Run a cover sub-lookup with the scanned ISBN, then (on a miss) retry with the
/// alternate ISBN-10/13 form. Returns the first form that yields a cover.
async fn cover_with_isbn_fallback<F, Fut>(isbn: &str, fetch: F) -> Option<String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    if let Some(url) = fetch(isbn.to_string()).await {
        return Some(url);
    }
    match crate::utils::isbn::alternate_isbn(isbn) {
        Some(alt) => fetch(alt).await,
        None => None,
    }
}

async fn try_bnf_sparql(
    clean_isbn: &str,
    isbn: &str,
    enable_openlibrary: bool,
    enable_google: bool,
    enable_inventaire: bool,
    google_api_key: Option<&str>,
) -> Option<BookMetadata> {
    tracing::debug!("Trying BNF SPARQL for ISBN {}", isbn);
    match crate::modules::integrations::bnf::lookup_bnf_isbn(clean_isbn).await {
        Ok(Some(bnf_book)) => {
            tracing::info!("BNF found book for ISBN {}: {}", isbn, bnf_book.title);
            let cover_url = enrich_cover(
                isbn,
                bnf_book.cover_url,
                enable_openlibrary,
                enable_google,
                enable_inventaire,
                google_api_key,
            )
            .await;
            Some(BookMetadata {
                title: bnf_book.title,
                authors: make_authors_from_name(bnf_book.author),
                publisher: bnf_book.publisher,
                publication_year: bnf_book.publication_year.map(|y| y.to_string()),
                cover_url,
                summary: bnf_book.description,
                page_count: None,
            })
        }
        Ok(None) => {
            tracing::debug!("BNF returned no result for ISBN {}", isbn);
            None
        }
        Err(e) => {
            tracing::warn!("BNF lookup failed for {}: {}", isbn, e);
            None
        }
    }
}

async fn try_sudoc(
    clean_isbn: &str,
    isbn: &str,
    enable_openlibrary: bool,
    enable_google: bool,
    enable_inventaire: bool,
    google_api_key: Option<&str>,
) -> Option<BookMetadata> {
    tracing::debug!("Trying SUDOC for French ISBN {}", isbn);
    match crate::modules::integrations::sudoc::fetch_by_isbn(clean_isbn).await {
        Ok(sudoc_book) => {
            tracing::info!("SUDOC found book for ISBN {}: {}", isbn, sudoc_book.title);
            let cover_url = enrich_cover(
                isbn,
                None,
                enable_openlibrary,
                enable_google,
                enable_inventaire,
                google_api_key,
            )
            .await;
            Some(BookMetadata {
                title: sudoc_book.title,
                authors: make_authors_from_name(sudoc_book.author),
                publisher: sudoc_book.publisher,
                publication_year: sudoc_book.publication_year.map(|y| y.to_string()),
                cover_url,
                summary: None,
                page_count: None,
            })
        }
        Err(e) => {
            tracing::debug!("SUDOC lookup failed for {}: {}", isbn, e);
            None
        }
    }
}

async fn try_bnf_sru(
    clean_isbn: &str,
    isbn: &str,
    enable_openlibrary: bool,
    enable_google: bool,
    enable_inventaire: bool,
    google_api_key: Option<&str>,
) -> Option<BookMetadata> {
    tracing::debug!("Trying BNF SRU for French ISBN {}", isbn);
    match crate::modules::integrations::bnf::lookup_bnf_sru(clean_isbn).await {
        Ok(Some(bnf_book)) => {
            tracing::info!("BNF SRU found book for ISBN {}: {}", isbn, bnf_book.title);
            // BNF SRU cover URLs (catalogue.bnf.fr/couverture?...) are speculative:
            // they are generated from the ARK ID but often return a placeholder,
            // not a real cover. Pass None so enrich_cover() can try Inventaire/OpenLibrary/Google.
            let cover_url = enrich_cover(
                isbn,
                None,
                enable_openlibrary,
                enable_google,
                enable_inventaire,
                google_api_key,
            )
            .await;
            Some(BookMetadata {
                title: bnf_book.title,
                authors: make_authors_from_name(bnf_book.author),
                publisher: bnf_book.publisher,
                publication_year: bnf_book.publication_year.map(|y| y.to_string()),
                cover_url,
                summary: bnf_book.description,
                page_count: None,
            })
        }
        Ok(None) => {
            tracing::debug!("BNF SRU returned no result for ISBN {}", isbn);
            None
        }
        Err(e) => {
            tracing::debug!("BNF SRU lookup failed for {}: {}", isbn, e);
            None
        }
    }
}

async fn try_inventaire(
    isbn: &str,
    enable_openlibrary: bool,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Option<BookMetadata> {
    tracing::debug!("Trying Inventaire for ISBN {}", isbn);
    match crate::inventaire_client::fetch_inventaire_metadata(isbn).await {
        Ok(inv_metadata) => {
            tracing::info!(
                "Inventaire found book for ISBN {}: {}",
                isbn,
                inv_metadata.title
            );
            let cover_url = enrich_cover(
                isbn,
                inv_metadata.cover_url,
                enable_openlibrary,
                enable_google,
                false, // Inventaire is the source, no need to re-query its cover
                google_api_key,
            )
            .await;
            Some(BookMetadata {
                title: inv_metadata.title,
                authors: inv_metadata.authors,
                publisher: inv_metadata.publisher,
                publication_year: inv_metadata.publication_year,
                cover_url,
                summary: inv_metadata.summary,
                page_count: inv_metadata.page_count,
            })
        }
        Err(e) => {
            tracing::debug!("Inventaire lookup failed for {}: {}", isbn, e);
            None
        }
    }
}

async fn try_openlibrary(
    isbn: &str,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Option<BookMetadata> {
    tracing::debug!("Trying OpenLibrary for ISBN {}", isbn);
    match crate::openlibrary::fetch_book_metadata(isbn).await {
        Ok(metadata) => {
            tracing::info!(
                "OpenLibrary found book for ISBN {}: {}",
                isbn,
                metadata.title
            );
            let cover_url = enrich_cover(
                isbn,
                metadata.cover_url,
                false,
                enable_google,
                false, // Inventaire was already tried as metadata source
                google_api_key,
            )
            .await;
            Some(BookMetadata {
                cover_url,
                ..metadata
            })
        }
        Err(e) => {
            tracing::debug!("OpenLibrary lookup failed for {}: {}", isbn, e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(
        summary: Option<&str>,
        page_count: Option<u32>,
        year: Option<&str>,
        cover: Option<&str>,
    ) -> BookMetadata {
        BookMetadata {
            title: "Primary Title".to_string(),
            authors: vec![],
            publisher: Some("Primary Publisher".to_string()),
            publication_year: year.map(str::to_string),
            cover_url: cover.map(str::to_string),
            summary: summary.map(str::to_string),
            page_count,
        }
    }

    #[test]
    fn fill_populates_only_empty_fields() {
        let mut primary = book(None, None, None, None);
        fill_empty_fields(
            &mut primary,
            GapFields {
                summary: Some("Recovered summary".to_string()),
                publisher: None,
                page_count: Some(321),
                publication_year: Some("1999".to_string()),
                cover_url: Some("http://cover".to_string()),
            },
        );
        assert_eq!(primary.summary.as_deref(), Some("Recovered summary"));
        assert_eq!(primary.page_count, Some(321));
        assert_eq!(primary.publication_year.as_deref(), Some("1999"));
        assert_eq!(primary.cover_url.as_deref(), Some("http://cover"));
    }

    #[test]
    fn fill_populates_publisher_when_primary_lacks_it() {
        // Primary (e.g. a BNF record) returned no publisher → recover it.
        let mut primary = book(None, None, None, None);
        primary.publisher = None;
        fill_empty_fields(
            &mut primary,
            GapFields {
                publisher: Some("Gallimard".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(primary.publisher.as_deref(), Some("Gallimard"));
    }

    #[test]
    fn fill_never_overwrites_existing_fields() {
        let mut primary = book(
            Some("Authoritative"),
            Some(100),
            Some("2010"),
            Some("http://primary-cover"),
        );
        fill_empty_fields(
            &mut primary,
            GapFields {
                summary: Some("Secondary summary".to_string()),
                publisher: Some("Secondary Publisher".to_string()),
                page_count: Some(999),
                publication_year: Some("1800".to_string()),
                cover_url: Some("http://secondary-cover".to_string()),
            },
        );
        // Every field the primary set must be untouched.
        assert_eq!(primary.summary.as_deref(), Some("Authoritative"));
        assert_eq!(primary.publisher.as_deref(), Some("Primary Publisher"));
        assert_eq!(primary.page_count, Some(100));
        assert_eq!(primary.publication_year.as_deref(), Some("2010"));
        assert_eq!(primary.cover_url.as_deref(), Some("http://primary-cover"));
    }

    #[test]
    fn fill_handles_partial_gaps_independently() {
        // Primary has a summary and a cover but is missing page count and year.
        let mut primary = book(Some("Has summary"), None, None, Some("http://has-cover"));
        fill_empty_fields(
            &mut primary,
            GapFields {
                summary: Some("Should be ignored".to_string()),
                publisher: None,
                page_count: Some(250),
                publication_year: Some("2021".to_string()),
                cover_url: Some("http://ignored".to_string()),
            },
        );
        assert_eq!(primary.summary.as_deref(), Some("Has summary"));
        assert_eq!(primary.cover_url.as_deref(), Some("http://has-cover"));
        assert_eq!(primary.page_count, Some(250));
        assert_eq!(primary.publication_year.as_deref(), Some("2021"));
    }

    #[test]
    fn fill_with_empty_gap_leaves_primary_unchanged() {
        let mut primary = book(None, None, Some("2005"), None);
        fill_empty_fields(&mut primary, GapFields::default());
        assert_eq!(primary.summary, None);
        assert_eq!(primary.page_count, None);
        assert_eq!(primary.publication_year.as_deref(), Some("2005"));
        assert_eq!(primary.cover_url, None);
    }

    #[test]
    fn fill_applied_in_priority_order_first_non_none_wins() {
        // Simulate the round merge order: Inventaire → OpenLibrary → Google.
        let mut primary = book(None, None, None, None);
        // Inventaire fills summary only.
        fill_empty_fields(
            &mut primary,
            GapFields {
                summary: Some("From Inventaire".to_string()),
                ..Default::default()
            },
        );
        // OpenLibrary would also have a summary but must not override; it fills year.
        fill_empty_fields(
            &mut primary,
            GapFields {
                summary: Some("From OpenLibrary".to_string()),
                publication_year: Some("1984".to_string()),
                ..Default::default()
            },
        );
        // Google fills the page count that nobody had yet.
        fill_empty_fields(
            &mut primary,
            GapFields {
                page_count: Some(412),
                ..Default::default()
            },
        );
        assert_eq!(primary.summary.as_deref(), Some("From Inventaire"));
        assert_eq!(primary.publication_year.as_deref(), Some("1984"));
        assert_eq!(primary.page_count, Some(412));
    }

    // ── Summary language coherence (ADR-040) ──────────────────────────

    const FR_SUMMARY: &str = "Ce roman raconte l'histoire d'une famille de mineurs \
         dans le nord de la France, leur misère quotidienne et la grande grève qui \
         embrase le bassin houiller au dix-neuvième siècle.";
    const EN_SUMMARY: &str = "This novel tells the story of a family of coal miners \
         in the north of France, their daily hardship and the great strike that \
         sets the mining district ablaze in the nineteenth century.";

    fn gap_with_summary(summary: &str) -> Option<GapFields> {
        Some(GapFields {
            summary: Some(summary.to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn enforce_drops_wrong_language_summary() {
        // Target French, candidate English → summary rejected.
        let mut gap = gap_with_summary(EN_SUMMARY);
        enforce_summary_language(&mut gap, Some("fr"));
        assert_eq!(gap.unwrap().summary, None);
    }

    #[test]
    fn enforce_keeps_matching_language_summary() {
        let mut gap = gap_with_summary(FR_SUMMARY);
        enforce_summary_language(&mut gap, Some("fr"));
        assert_eq!(gap.unwrap().summary.as_deref(), Some(FR_SUMMARY));
    }

    #[test]
    fn enforce_keeps_summary_when_target_unknown() {
        // No target → no basis to reject (preserves pre-ADR-040 behaviour).
        let mut gap = gap_with_summary(EN_SUMMARY);
        enforce_summary_language(&mut gap, None);
        assert_eq!(gap.unwrap().summary.as_deref(), Some(EN_SUMMARY));
    }

    #[test]
    fn enforce_keeps_undetectable_summary() {
        // Too short for a reliable detection → kept rather than wrongly dropped.
        let mut gap = gap_with_summary("Roman.");
        enforce_summary_language(&mut gap, Some("fr"));
        assert!(gap.unwrap().summary.is_some());
    }

    #[test]
    fn inventaire_gap_never_carries_summary() {
        let inv = crate::inventaire_client::InventaireMetadata {
            title: "L'Étranger".to_string(),
            authors: vec![],
            publisher: Some("Gallimard".to_string()),
            publication_year: Some("1942".to_string()),
            cover_url: Some("http://cover".to_string()),
            inventaire_uri: "wd:Q163297".to_string(),
            summary: Some("roman d'Albert Camus".to_string()),
            page_count: Some(159),
        };
        let gap = gap_from_inventaire(inv);
        assert_eq!(gap.summary, None);
        // But the other fields survive for gap-fill (publisher label included).
        assert_eq!(gap.publisher.as_deref(), Some("Gallimard"));
        assert_eq!(gap.publication_year.as_deref(), Some("1942"));
        assert_eq!(gap.page_count, Some(159));
        assert_eq!(gap.cover_url.as_deref(), Some("http://cover"));
    }
}
