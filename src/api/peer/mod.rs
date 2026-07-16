#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()
//! P2P peer HTTP API, one file per concern (split from a single 11k-line file).
//! Every item is re-exported below so callers keep using
//! `crate::api::peer::<item>` unchanged.

mod admin;
mod books_cache;
mod connection;
mod helpers;
mod loan_offer;
mod loan_shared;
mod messaging;
mod relay_config;
mod requests_incoming;
mod requests_outgoing;
mod returns;
mod search;
mod sync;

#[cfg(test)]
mod loan_flow_tests;

pub use admin::*;
pub use books_cache::*;
pub use connection::*;
pub use helpers::*;
pub use loan_offer::*;
pub(crate) use loan_shared::*;
pub use messaging::*;
pub use relay_config::*;
pub use requests_incoming::*;
pub use requests_outgoing::*;
pub use returns::*;
pub use search::*;
pub use sync::*;
