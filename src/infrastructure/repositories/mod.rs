//! Repository implementations using SeaORM

pub mod author_repository;
pub mod book_repository;

pub use author_repository::SeaOrmAuthorRepository;
pub use book_repository::SeaOrmBookRepository;
