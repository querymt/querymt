//! Session domain types and storage contracts.

pub mod backend;
pub mod domain;
pub mod domain_schedule;
pub mod error;
mod event_journal;
pub mod lifecycle;
pub mod location;
pub mod projection;
pub mod ranges;
pub mod repository;
mod schedule_repository;
mod session_store;
mod storage_backend;
pub mod store;
mod view_store;

pub use error::{SessionError, SessionResult};
