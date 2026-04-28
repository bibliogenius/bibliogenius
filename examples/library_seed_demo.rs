//! Smoke test for the `library_config` migration seed.
//!
//! Initializes a fresh SQLite file at the given path and prints the seeded
//! `library_config.name`. Used by `scripts/qa_library_seed.sh` to verify the
//! seed under different `LC_ALL` / `LANG` configurations.
//!
//! Usage:
//!   cargo run --quiet --example library_seed_demo -- /tmp/seed.db

use rust_lib_app::db;
use rust_lib_app::models::library_config;
use sea_orm::EntityTrait;

#[tokio::main]
async fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: library_seed_demo <db_path>");
    let url = format!("sqlite:{path}?mode=rwc");
    let conn = db::init_db(&url).await.expect("init_db failed");
    let cfg = library_config::Entity::find()
        .one(&conn)
        .await
        .expect("query failed")
        .expect("library_config row missing after migration");
    println!("{}", cfg.name);
}
