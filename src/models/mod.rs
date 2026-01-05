pub mod author;
pub mod book;
pub mod book_authors;
pub mod book_tags;
pub mod collection;
pub mod collection_book;
pub mod contact;
pub mod copy;
pub mod gamification_achievements;
pub mod gamification_config;
pub mod gamification_progress;
pub mod gamification_streaks;
pub mod installation_profile;
pub mod library;
pub mod library_config;
pub mod loan;
pub mod operation_log;
pub mod p2p_outgoing_request;
pub mod p2p_request;
pub mod peer;
pub mod peer_book;
pub mod sale; // Nouveau module pour les ventes (profil Libraire)
pub mod tag;
pub mod user;

pub use book::Book;
pub use installation_profile::ProfileConfig;
pub use library_config::LibraryConfig;
