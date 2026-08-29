// Duplicate-book merge (ADR-070): preview, automatic merge, one-group merge.
// Everything delegates to services::book_merge_service.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ── Duplicate merge ──────────────────────────────────────────────────

/// One book row inside a duplicate group, as the preview lists it.
pub struct FrbDuplicateBook {
    pub id: String,
    pub title: String,
    pub isbn: Option<String>,
    pub author: Option<String>,
    pub created_at: String,
    pub cover_url: Option<String>,
}

/// A set of rows describing the same book. `automatic` tells the two families
/// apart: an ISBN group merges without asking, a title/author/year group is a
/// proposal the reader accepts one at a time (ADR-070 D2).
pub struct FrbDuplicateGroup {
    /// Opaque handle to pass back to `merge_duplicate_group`.
    pub key: String,
    pub automatic: bool,
    /// The row that survives: the oldest (ADR-070 D3).
    pub canonical: FrbDuplicateBook,
    /// The rows folded into it. Never empty.
    pub duplicates: Vec<FrbDuplicateBook>,
}

/// What a repair would do. Computed without writing anything.
pub struct FrbDuplicateScan {
    pub automatic: Vec<FrbDuplicateGroup>,
    pub proposed: Vec<FrbDuplicateGroup>,
    /// Book rows `merge_duplicate_books` would remove.
    pub books_removed_by_automatic: u32,
}

/// What a repair actually did.
pub struct FrbMergeReport {
    pub groups_merged: u32,
    pub books_removed: u32,
    pub copies_collapsed: u32,
    pub covers_recovered: u32,
}

impl From<crate::services::book_merge_service::DuplicateBook> for FrbDuplicateBook {
    fn from(b: crate::services::book_merge_service::DuplicateBook) -> Self {
        FrbDuplicateBook {
            id: b.id,
            title: b.title,
            isbn: b.isbn,
            author: b.author,
            created_at: b.created_at,
            cover_url: b.cover_url,
        }
    }
}

impl From<crate::services::book_merge_service::DuplicateGroup> for FrbDuplicateGroup {
    fn from(g: crate::services::book_merge_service::DuplicateGroup) -> Self {
        FrbDuplicateGroup {
            automatic: g.kind == crate::services::book_merge_service::MatchKind::Isbn,
            key: g.key,
            canonical: g.canonical.into(),
            duplicates: g.duplicates.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<crate::services::book_merge_service::MergeReport> for FrbMergeReport {
    fn from(r: crate::services::book_merge_service::MergeReport) -> Self {
        FrbMergeReport {
            groups_merged: r.groups_merged,
            books_removed: r.books_removed,
            copies_collapsed: r.copies_collapsed,
            covers_recovered: r.covers_recovered,
        }
    }
}

/// Preview the duplicates in this library. Writes nothing, so it is safe to
/// call to decide whether to offer the repair at all.
pub async fn scan_duplicate_books() -> Result<FrbDuplicateScan, String> {
    let db = db().ok_or("Database not initialized")?;
    let scan = crate::services::book_merge_service::scan_duplicates(db)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(FrbDuplicateScan {
        automatic: scan.automatic.into_iter().map(Into::into).collect(),
        proposed: scan.proposed.into_iter().map(Into::into).collect(),
        books_removed_by_automatic: scan.books_removed_by_automatic,
    })
}

/// Merge every ISBN-correlated group. Destructive and replicated: the removed
/// rows disappear from every device of the account on its next sync, so the
/// caller must have shown the preview and taken a confirmation first.
pub async fn merge_duplicate_books() -> Result<FrbMergeReport, String> {
    let db = db().ok_or("Database not initialized")?;
    let report =
        crate::services::book_merge_service::merge_automatic(db, covers_dir().map(|p| p.as_path()))
            .await
            .map_err(|e| format!("{e:?}"))?;
    Ok(report.into())
}

/// Merge the single group carrying `key`. This is how a proposed
/// (title/author/year) group is accepted, one confirmation at a time.
pub async fn merge_duplicate_group(key: String) -> Result<FrbMergeReport, String> {
    let db = db().ok_or("Database not initialized")?;
    let report = crate::services::book_merge_service::merge_group(
        db,
        covers_dir().map(|p| p.as_path()),
        &key,
    )
    .await
    .map_err(|e| format!("{e:?}"))?;
    Ok(report.into())
}

/// How many surplus rows a repair would remove, and nothing else. The account
/// screen polls this on every visit and after every sync cycle to decide whether
/// to offer the repair at all, so it must stay far cheaper than the preview:
/// `count_surplus` skips the schema walk and the per-row payload.
pub async fn count_duplicate_surplus() -> Result<u32, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::book_merge_service::count_surplus(db)
        .await
        .map_err(|e| format!("{e:?}"))
}
