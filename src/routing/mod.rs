//! Routing internals split out of the proxy core (v0.4 M0): candidate
//! selection, circuit breaking, per-model metrics, and decision/trace
//! recording. `forward()` and the HTTP layer stay in `src/proxy.rs`.

pub(crate) mod circuit;
pub(crate) mod decision;
pub(crate) mod metrics;
pub(crate) mod select;
