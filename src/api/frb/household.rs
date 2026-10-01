// Household readers: one shared library, one reading state per person.
// Everything delegates to infrastructure::household.
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

impl From<crate::infrastructure::household::Reader> for FrbReader {
    fn from(reader: crate::infrastructure::household::Reader) -> Self {
        Self {
            id: reader.id,
            name: reader.name,
        }
    }
}

/// Every reader of the household, oldest first. Empty until someone opts in.
pub async fn list_household_readers() -> Result<Vec<FrbReader>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::infrastructure::household::list_readers(db)
        .await
        .map(|readers| readers.into_iter().map(FrbReader::from).collect())
        .map_err(|e| format!("{e:?}"))
}

/// Who reads on this device. None: the device shows the shared book columns,
/// exactly as before households existed.
pub async fn get_current_household_reader() -> Result<Option<FrbReader>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::infrastructure::household::current_reader(db)
        .await
        .map(|reader| reader.map(FrbReader::from))
        .map_err(|e| format!("{e:?}"))
}

/// Add a reader to the household and make them the reader of this device. The
/// first reader of a household inherits the reading state already on the books.
pub async fn create_household_reader(name: String) -> Result<FrbReader, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::infrastructure::household::create_reader(db, &name)
        .await
        .map(FrbReader::from)
        .map_err(|e| format!("{e:?}"))
}

/// Make an existing reader the reader of this device.
pub async fn set_current_household_reader(reader_id: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::infrastructure::household::set_current_reader(db, &reader_id)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// Put this device back on the shared reading state. Readers and their
/// readings are kept.
pub async fn clear_current_household_reader() -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::infrastructure::household::clear_current_reader(db)
        .await
        .map_err(|e| format!("{e:?}"))
}

pub async fn rename_household_reader(reader_id: String, name: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::infrastructure::household::rename_reader(db, &reader_id, &name)
        .await
        .map_err(|e| format!("{e:?}"))
}
