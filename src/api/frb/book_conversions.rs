// FrbBook to Model conversion and its unit tests.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ Initializers & Converters ============

impl From<FrbBook> for crate::models::Book {
    fn from(frb_book: FrbBook) -> Self {
        let subjects: Option<Vec<String>> = frb_book
            .subjects
            .and_then(|s| serde_json::from_str(&s).ok());

        crate::models::Book {
            id: frb_book.id,
            title: frb_book.title,
            isbn: frb_book.isbn,
            summary: frb_book.summary,
            publisher: frb_book.publisher,
            publication_year: frb_book.publication_year,
            subjects,
            reading_status: frb_book.reading_status,
            user_rating: frb_book.user_rating,
            shelf_position: frb_book.shelf_position,
            author: frb_book.author.clone(),
            authors: frb_book.author.map(|a| {
                a.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }),
            cover_url: frb_book.cover_url,
            large_cover_url: frb_book.large_cover_url,
            // Default other fields
            dewey_decimal: None,
            lcc: None,
            marc_record: None,
            cataloguing_notes: None,
            source_data: None,
            finished_reading_at: frb_book.finished_reading_at.map(Some),
            started_reading_at: frb_book.started_reading_at.map(Some),
            source: None,
            owned: Some(frb_book.owned),
            price: frb_book.price, // Price now exposed in FFI layer
            language: None,
            digital_formats: frb_book.digital_formats,
            available_copies: None,
            private: Some(frb_book.private),
            page_count: frb_book.page_count,
            loan_duration_days: None,
            added_at: frb_book.added_at,
            // FrbBook (FFI DTO) doesn't carry updated_at; the cover
            // versioning pipeline only needs it on the catalog-push side
            // where books are read directly from the Model.
            updated_at: None,
            hub_cover_upload_failed_at: frb_book.hub_cover_upload_failed_at,
            // Inbound write path: possession is derived from `copies` on read,
            // never dictated by the client. Whatever Dart sent is discarded.
            is_borrowed: None,
            is_lent: None,
            // Peer-facing only (redact_for_peer); never accepted on write.
            wanted: None,
        }
    }
}

/// Turn the FFI DTO into the payload expected by `book_service::update_book`.
///
/// `From<FrbBook>` maps an absent reading date to `None`, which the update
/// service reads as "leave the stored value alone" - the right default for
/// the create path, where every field is optional. The update path is the
/// opposite: Dart always sends a complete book (each field is either the
/// reader's new value or the one carried over from the stored row), so an
/// absent date means the reader removed it. Flatten it into an explicit
/// clear (`Some(None)`) or the date can never be taken back off a book.
pub(crate) fn frb_book_into_update_payload(frb_book: FrbBook) -> crate::models::Book {
    let mut book: crate::models::Book = frb_book.into();
    book.finished_reading_at = Some(book.finished_reading_at.flatten());
    book.started_reading_at = Some(book.started_reading_at.flatten());
    book
}

#[cfg(test)]
mod frb_book_conversion_tests {
    use super::*;
    use crate::models::Book;

    #[test]
    fn added_at_roundtrips_through_frb_book() {
        let book = Book {
            title: "Martin Eden".to_string(),
            added_at: Some("2026-04-13T08:00:00Z".to_string()),
            ..Default::default()
        };

        let frb: FrbBook = book.into();
        assert_eq!(frb.added_at.as_deref(), Some("2026-04-13T08:00:00Z"));

        let back: Book = frb.into();
        assert_eq!(back.added_at.as_deref(), Some("2026-04-13T08:00:00Z"));
    }

    #[test]
    fn added_at_none_propagates_both_directions() {
        let book = Book {
            title: "Sans date".to_string(),
            added_at: None,
            ..Default::default()
        };

        let frb: FrbBook = book.into();
        assert!(frb.added_at.is_none());

        let back: Book = frb.into();
        assert!(back.added_at.is_none());
    }

    /// The create path must keep treating an absent date as "no opinion":
    /// `create_book` maps `None` to `NotSet`, and nothing should start
    /// writing explicit NULLs into a fresh row.
    #[test]
    fn create_conversion_leaves_absent_reading_dates_untouched() {
        let frb: FrbBook = Book {
            title: "Sans date".to_string(),
            ..Default::default()
        }
        .into();
        assert!(frb.finished_reading_at.is_none());

        let book: Book = frb.into();
        assert!(book.finished_reading_at.is_none());
        assert!(book.started_reading_at.is_none());
    }

    /// Regression: a reader who clears the "finished reading" date in the
    /// edit form sends a book with no date at all. That has to reach the
    /// service as an explicit clear, not as an absent field.
    #[test]
    fn update_payload_turns_an_absent_reading_date_into_an_explicit_clear() {
        let frb: FrbBook = Book {
            title: "Martin Eden".to_string(),
            ..Default::default()
        }
        .into();

        let book = frb_book_into_update_payload(frb);
        assert_eq!(book.finished_reading_at, Some(None));
        assert_eq!(book.started_reading_at, Some(None));
    }

    #[test]
    fn update_payload_keeps_a_date_the_reader_did_set() {
        let mut frb: FrbBook = Book {
            title: "Martin Eden".to_string(),
            ..Default::default()
        }
        .into();
        frb.finished_reading_at = Some("2026-07-01".to_string());
        frb.started_reading_at = Some("2026-06-01".to_string());

        let book = frb_book_into_update_payload(frb);
        assert_eq!(
            book.finished_reading_at,
            Some(Some("2026-07-01".to_string()))
        );
        assert_eq!(
            book.started_reading_at,
            Some(Some("2026-06-01".to_string()))
        );
    }
}
