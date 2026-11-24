//! Adapters for systems the hub does not own. `live` talks to deployed
//! services over their supported APIs; `fixture` simulates them in-process so
//! the whole journey runs without credentials or a GPU. Fixture success is
//! never evidence that a live integration works.

pub mod assistant;
pub mod calendar_sync;
pub mod fixture;
pub mod gpu_peer;
pub mod humanizer;
pub mod imap_sync;
pub mod live;
pub mod local_llm_bi;
pub mod mail_parse;
pub mod providers;
pub mod store;
