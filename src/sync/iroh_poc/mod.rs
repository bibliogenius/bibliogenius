//! # Iroh P2P POC
//!
//! This module contains a Proof of Concept for using Iroh (by n0)
//! as an alternative to libp2p for BiblioGenius's global P2P sync.
//!
//! ## Goals
//! - Evaluate Iroh's simplicity vs libp2p
//! - Test document sync between two nodes
//! - Assess binary size and performance
//!
//! ## Status: 🚧 Experimental
//!
//! This is NOT integrated into the main app yet.

// TODO: Add iroh dependency to Cargo.toml when ready to test
// [dependencies]
// iroh = "0.28"  # Check latest version

pub mod node;

/// POC Status
pub const POC_STATUS: &str = "experimental";
