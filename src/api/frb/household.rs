// Household readers: one shared library, one reading state per person.
// Everything delegates to services::household_service.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ── Household readers ────────────────────────────────────────────────

/// A person of the household.
pub struct FrbReader {
    pub id: String,
    pub name: String,
}

impl From<crate::domain::Reader> for FrbReader {
    fn from(reader: crate::domain::Reader) -> Self {
        Self {
            id: reader.id,
            name: reader.name,
        }
    }
}

/// Every reader of the household, oldest first. Empty until someone opts in.
pub async fn list_household_readers() -> Result<Vec<FrbReader>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::household_service::list_readers(db)
        .await
        .map(|readers| readers.into_iter().map(FrbReader::from).collect())
        .map_err(|e| format!("{e:?}"))
}

/// Who reads on this device. None: the device shows the shared book columns,
/// exactly as before households existed.
pub async fn get_current_household_reader() -> Result<Option<FrbReader>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::household_service::current_reader(db)
        .await
        .map(|reader| reader.map(FrbReader::from))
        .map_err(|e| format!("{e:?}"))
}

/// Add a reader to the household and make them the reader of this device. The
/// first reader of a household inherits the reading state already on the books.
pub async fn create_household_reader(name: String) -> Result<FrbReader, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::household_service::create_reader(db, &name)
        .await
        .map(FrbReader::from)
        .map_err(|e| format!("{e:?}"))
}

/// Make an existing reader the reader of this device.
pub async fn set_current_household_reader(reader_id: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::household_service::set_current_reader(db, &reader_id)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// Put this device back on the shared reading state. Readers and their
/// readings are kept.
pub async fn clear_current_household_reader() -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::household_service::clear_current_reader(db)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// Remove a reader and every reading of theirs, on every device of the
/// account. Their wishes stay on the books, unnamed.
pub async fn delete_household_reader(reader_id: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::household_service::remove_reader(db, &reader_id)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// What "import my readings" did, for the summary shown to the reader.
pub struct FrbReadingImportReport {
    pub matched: u32,
    pub created: u32,
    pub ambiguous: u32,
    pub ambiguous_titles: Vec<String>,
    pub skipped: u32,
}

/// Merge the readings of a catalogue export (the JSON "Exporter mon catalogue"
/// writes) into the shared library, for the reader of this device. Adds and
/// records only: unlike the catalogue restore, nothing is wiped.
pub async fn import_household_readings(json: String) -> Result<FrbReadingImportReport, String> {
    let db = db().ok_or("Database not initialized")?;
    let report = crate::services::household_import::import_readings(db, &json)
        .await
        .map_err(|e| match e {
            crate::services::book_service::ServiceError::InvalidInput(msg) => msg,
            other => format!("{other:?}"),
        })?;
    if report.created > 0
        && let Some(state) = global_app_state()
    {
        crate::services::catalog_notification::schedule_catalog_changed_notification(
            state.clone(),
        );
    }
    Ok(FrbReadingImportReport {
        matched: report.matched as u32,
        created: report.created as u32,
        ambiguous: report.ambiguous as u32,
        ambiguous_titles: report.ambiguous_titles,
        skipped: report.skipped as u32,
    })
}

pub async fn rename_household_reader(reader_id: String, name: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::household_service::rename_reader(db, &reader_id, &name)
        .await
        .map_err(|e| format!("{e:?}"))
}
