//! Repository implementations using SeaORM

pub mod author_repository;
pub mod book_repository;
pub mod collection_repository;
pub mod copy_repository;
pub mod gamification_repository;
pub mod linked_device_repository;
pub mod loan_settings_repository;
pub mod metadata_fill_repository;
pub mod notification_repository;

pub use author_repository::SeaOrmAuthorRepository;
pub use book_repository::SeaOrmBookRepository;
pub use collection_repository::SeaOrmCollectionRepository;
pub use copy_repository::SeaOrmCopyRepository;
pub use gamification_repository::SeaOrmGamificationRepository;
pub use linked_device_repository::SeaOrmLinkedDeviceRepository;
pub use loan_settings_repository::SeaOrmLoanSettingsRepository;
pub use metadata_fill_repository::SeaOrmMetadataFillRepository;
pub use notification_repository::SeaOrmNotificationRepository;
