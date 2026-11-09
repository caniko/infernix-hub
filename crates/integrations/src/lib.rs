//! Adapters for systems the hub does not own. `live` talks to deployed
//! services over their supported APIs; `fixture` simulates them in-process so
//! the whole journey runs without credentials or a GPU. Fixture success is
//! never evidence that a live integration works.

pub mod assistant;
pub mod fixture;
pub mod gpu_peer;
pub mod live;
pub mod store;
