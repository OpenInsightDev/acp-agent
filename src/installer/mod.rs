//! Agent package and local toolchain installation primitives.

/// Agent installation, update, uninstall, and local inventory operations.
pub mod agents;
/// Downloading, validating, and caching binary agent distributions.
pub mod binary;
/// Cache path layout and local cache inventory helpers.
pub(crate) mod cache;
/// Detection and installation of supported local toolchains.
pub mod environment;
