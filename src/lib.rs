// lib.rs — Seakarr library root. Modules are declared here so integration
// tests (in tests/) can import from `seakarr::`.

pub mod client;
pub mod config;
pub mod db;
pub mod download;
pub mod error;
pub mod filter;
pub mod notifier;
pub mod organizer;
pub mod runner;
pub mod scanner;
pub mod search;
