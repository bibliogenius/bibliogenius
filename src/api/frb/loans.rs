// Loan lifecycle: CRUD, settings, durations, reminders.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ Loans API ============

/// Simplified loan structure for FFI
#[frb(dart_metadata=("freezed"))]
pub struct FrbLoan {
    pub id: String,
    pub copy_id: String,
    pub contact_id: String,
    pub library_id: i32,
    pub loan_date: String,
    pub due_date: String,
    pub return_date: Option<String>,
    pub status: String,
    pub notes: Option<String>,
    pub contact_name: String,
    pub book_title: String,
    pub book_id: Option<String>,
    pub cover_url: Option<String>,
    pub isbn: Option<String>,
}

impl From<crate::services::loan_service::LoanWithDetails> for FrbLoan {
    fn from(l: crate::services::loan_service::LoanWithDetails) -> Self {
        FrbLoan {
            id: l.id,
            copy_id: l.copy_id,
            contact_id: l.contact_id,
            library_id: l.library_id,
            loan_date: l.loan_date,
            due_date: l.due_date,
            return_date: l.return_date,
            status: l.status,
            notes: l.notes,
            contact_name: l.contact_name,
            book_title: l.book_title,
            book_id: l.book_id,
            cover_url: l.cover_url,
            isbn: l.isbn,
        }
    }
}

/// Get all loans with optional filters
pub async fn get_all_loans(
    library_id: Option<i32>,
    status: Option<String>,
    contact_id: Option<i32>,
) -> Result<Vec<FrbLoan>, String> {
    let db = db().ok_or("Database not initialized")?;

    let filter = crate::services::loan_service::LoanFilter {
        library_id,
        status,
        contact_id,
        // The loans screen shows the full list; pagination is the MCP tools' concern.
        limit: None,
        offset: None,
    };

    match crate::services::loan_service::list_loans(db, filter).await {
        Ok(loans) => Ok(loans.into_iter().map(FrbLoan::from).collect()),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Count active loans
pub async fn count_active_loans() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    match crate::services::loan_service::count_active_loans(db).await {
        Ok(count) => Ok(count),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Create a new loan
pub async fn create_loan(
    copy_id: String,
    contact_id: String,
    library_id: i32,
    loan_date: String,
    due_date: String,
    notes: Option<String>,
) -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;

    // Resolve library_id if 0 (sentinel for "not provided"): FK references libraries(id)
    let resolved_library_id = if library_id > 0 {
        library_id
    } else {
        crate::utils::library_helpers::resolve_library_id(db)
            .await
            .map_err(|e| format!("No library found: {e}"))?
    };

    let dto = crate::models::loan::LoanDto {
        id: None,
        copy_id,
        contact_id,
        library_id: resolved_library_id,
        loan_date,
        due_date,
        return_date: None,
        status: None,
        notes,
    };

    match crate::services::loan_service::create_loan(db, dto).await {
        Ok(loan) => Ok(loan.id),
        Err(crate::services::loan_service::ServiceError::NotFound) => {
            Err("Copy not found".to_string())
        }
        Err(crate::services::loan_service::ServiceError::InvalidState(msg)) => Err(msg),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Count returned loans (for cleanup confirmation dialog)
pub async fn count_returned_loans() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::count_returned_loans(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Delete all returned loans, returns the number of deleted rows
pub async fn delete_returned_loans() -> Result<u64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::delete_returned_loans(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Count closed incoming P2P requests (not pending)
pub async fn count_closed_incoming_requests() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::count_closed_incoming_requests(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Delete all closed incoming P2P requests (not pending)
pub async fn delete_closed_incoming_requests() -> Result<u64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::delete_closed_incoming_requests(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Count closed outgoing P2P requests (not pending)
pub async fn count_closed_outgoing_requests() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::count_closed_outgoing_requests(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Delete all closed outgoing P2P requests (not pending)
pub async fn delete_closed_outgoing_requests() -> Result<u64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::delete_closed_outgoing_requests(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Return a loan
pub async fn return_loan(id: String) -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;

    match crate::services::loan_service::return_loan(db, &id).await {
        Ok(_) => {
            // Dismiss any pending due-date reminders for this loan
            use crate::domain::NotificationRepository;
            let notif_repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
            let _ = notif_repo.dismiss_by_ref("loan", &id).await;
            Ok("Loan returned successfully".to_string())
        }
        Err(crate::services::loan_service::ServiceError::NotFound) => {
            Err("Loan not found".to_string())
        }
        Err(crate::services::loan_service::ServiceError::InvalidState(msg)) => Err(msg),
        Err(e) => Err(format!("{:?}", e)),
    }
}

// ============ Loan Settings API ============

/// Loan settings for FFI
#[frb(dart_metadata=("freezed"))]
pub struct FrbLoanSettings {
    pub default_loan_duration_days: i32,
    pub per_book_duration_enabled: bool,
    pub reminder_days_before_due: i32,
}

/// Get the current loan settings
pub async fn get_loan_settings() -> Result<FrbLoanSettings, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmLoanSettingsRepository::new(db.clone());
    use crate::domain::LoanSettingsRepository;

    let settings = repo.get_settings().await.map_err(|e| e.to_string())?;
    Ok(FrbLoanSettings {
        default_loan_duration_days: settings.default_loan_duration_days,
        per_book_duration_enabled: settings.per_book_duration_enabled,
        reminder_days_before_due: settings.reminder_days_before_due,
    })
}

/// Update the global loan settings
pub async fn update_loan_settings(
    default_loan_duration_days: i32,
    per_book_duration_enabled: bool,
    reminder_days_before_due: i32,
) -> Result<FrbLoanSettings, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmLoanSettingsRepository::new(db.clone());
    use crate::domain::LoanSettingsRepository;

    let updated = repo
        .update_settings(crate::domain::LoanSettings {
            default_loan_duration_days,
            per_book_duration_enabled,
            reminder_days_before_due,
        })
        .await
        .map_err(|e| e.to_string())?;

    Ok(FrbLoanSettings {
        default_loan_duration_days: updated.default_loan_duration_days,
        per_book_duration_enabled: updated.per_book_duration_enabled,
        reminder_days_before_due: updated.reminder_days_before_due,
    })
}

/// Check active loans for upcoming due dates and emit reminder notifications.
///
/// Emits:
/// - `LoanDueReminder` when `0 < days_until_due <= reminder_days_before_due`
/// - `LoanDueToday` when `days_until_due <= 0` (due today or overdue)
///
/// Deduplication is enforced: no duplicate notification per loan per type.
/// Returns the number of new notifications created.
pub async fn check_loan_reminders(language: String) -> Result<i32, String> {
    use crate::domain::notification_repository::{CreateNotification, NotificationEventType};
    use crate::domain::{LoanSettingsRepository, NotificationRepository};
    use crate::infrastructure::{SeaOrmLoanSettingsRepository, SeaOrmNotificationRepository};
    use crate::services::loan_service::{LoanFilter, list_loans};
    use chrono::{Local, NaiveDate};

    let db = db().ok_or("Database not initialized")?;

    let settings_repo = SeaOrmLoanSettingsRepository::new(db.clone());
    let settings = settings_repo
        .get_settings()
        .await
        .map_err(|e| e.to_string())?;
    let reminder_days = settings.reminder_days_before_due;

    let loans = list_loans(
        db,
        LoanFilter {
            status: Some("active".to_string()),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| format!("{:?}", e))?;

    let notif_repo = SeaOrmNotificationRepository::new(db.clone());
    let today = Local::now().date_naive();
    let lang = language.as_str();
    let mut created = 0i32;

    for loan in loans {
        // Parse due date (stored as "YYYY-MM-DD" or "YYYY-MM-DD HH:MM:SS")
        let due_date_str = loan.due_date.get(..10).unwrap_or(&loan.due_date);
        let due_date = match NaiveDate::parse_from_str(due_date_str, "%Y-%m-%d") {
            Ok(d) => d,
            Err(_) => continue,
        };
        let days_left = (due_date - today).num_days();
        let ref_id = loan.id.to_string();

        if days_left <= 0 {
            // Due today or overdue - emit LoanDueToday if not already present
            let already = notif_repo
                .exists(
                    NotificationEventType::LoanDueToday.as_str(),
                    "loan",
                    &ref_id,
                )
                .await
                .unwrap_or(true);
            if !already {
                let (title, body) = loan_due_today_text(lang, &loan.book_title, &loan.contact_name);
                if notif_repo
                    .create(CreateNotification {
                        event_type: NotificationEventType::LoanDueToday,
                        title,
                        body: Some(body),
                        ref_type: Some("loan".to_string()),
                        ref_id: Some(ref_id),
                    })
                    .await
                    .is_ok()
                {
                    created += 1;
                }
            }
        } else if days_left <= reminder_days as i64 {
            // Approaching due date - emit LoanDueReminder if not already present
            let already = notif_repo
                .exists(
                    NotificationEventType::LoanDueReminder.as_str(),
                    "loan",
                    &ref_id,
                )
                .await
                .unwrap_or(true);
            if !already {
                let (title, body) = loan_due_reminder_text(
                    lang,
                    &loan.book_title,
                    &loan.contact_name,
                    days_left as i32,
                );
                if notif_repo
                    .create(CreateNotification {
                        event_type: NotificationEventType::LoanDueReminder,
                        title,
                        body: Some(body),
                        ref_type: Some("loan".to_string()),
                        ref_id: Some(ref_id),
                    })
                    .await
                    .is_ok()
                {
                    created += 1;
                }
            }
        }
    }

    Ok(created)
}

fn loan_due_today_text(lang: &str, title: &str, borrower: &str) -> (String, String) {
    match lang {
        "fr" => (
            "Retour prévu aujourd'hui".to_string(),
            format!("«{}» doit être rendu aujourd'hui - {}", title, borrower),
        ),
        "es" => (
            "Devolución prevista hoy".to_string(),
            format!("«{}» debe devolverse hoy - {}", title, borrower),
        ),
        "de" => (
            "Rückgabe heute fällig".to_string(),
            format!("«{}» · Heute fällig - {}", title, borrower),
        ),
        _ => (
            "Return due today".to_string(),
            format!("«{}» · Due today - {}", title, borrower),
        ),
    }
}

fn loan_due_reminder_text(lang: &str, title: &str, borrower: &str, days: i32) -> (String, String) {
    match lang {
        "fr" => (
            "Rappel de prêt".to_string(),
            format!(
                "«{}» · Retour dans {} jour{} - {}",
                title,
                days,
                if days > 1 { "s" } else { "" },
                borrower
            ),
        ),
        "es" => (
            "Recordatorio de préstamo".to_string(),
            format!(
                "«{}» · Vence en {} día{} - {}",
                title,
                days,
                if days > 1 { "s" } else { "" },
                borrower
            ),
        ),
        "de" => (
            "Leih-Erinnerung".to_string(),
            format!(
                "«{}» · Fällig in {} Tag{} - {}",
                title,
                days,
                if days > 1 { "en" } else { "" },
                borrower
            ),
        ),
        _ => (
            "Loan reminder".to_string(),
            format!(
                "«{}» · Due in {} day{} - {}",
                title,
                days,
                if days > 1 { "s" } else { "" },
                borrower
            ),
        ),
    }
}

/// Get the effective loan duration for a specific book (in days).
/// Returns the per-book override if enabled and set, otherwise the global default.
pub async fn get_effective_loan_duration(book_id: String) -> Result<i32, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmLoanSettingsRepository::new(db.clone());
    use crate::domain::LoanSettingsRepository;

    repo.get_effective_duration(&book_id)
        .await
        .map_err(|e| e.to_string())
}

/// Get the per-book loan duration override (None = uses global default)
pub async fn get_book_loan_duration(book_id: String) -> Result<Option<i32>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmLoanSettingsRepository::new(db.clone());
    use crate::domain::LoanSettingsRepository;

    repo.get_book_loan_duration(&book_id)
        .await
        .map_err(|e| e.to_string())
}

/// Set the per-book loan duration override (pass None to clear and use global default)
pub async fn set_book_loan_duration(book_id: String, days: Option<i32>) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmLoanSettingsRepository::new(db.clone());
    use crate::domain::LoanSettingsRepository;

    repo.set_book_loan_duration(&book_id, days)
        .await
        .map_err(|e| e.to_string())
}
