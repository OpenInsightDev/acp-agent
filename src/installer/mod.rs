//! Agent package and local toolchain installation primitives.

/// Downloading, validating, and caching binary agent distributions.
pub mod binary;
/// Cache path layout and local cache inventory helpers.
pub(crate) mod cache;
/// Detection and installation of supported local toolchains.
pub mod environment;
