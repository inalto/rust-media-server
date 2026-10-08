//! rust-media-server (rms): single-binary live media server.
//!
//! See `docs/ARCHITECTURE.md` for the binding module contract. The binary
//! (`src/main.rs`) only wires these modules together; exposing them from a
//! library target also lets integration tests in `tests/` use them.

pub mod api;
pub mod config;
pub mod db;
pub mod egress;
pub mod error;
pub mod hub;
pub mod ingest;
pub mod metrics;
pub mod packager;
pub mod recorder;
pub mod state;
pub mod web;

pub use state::AppState;
