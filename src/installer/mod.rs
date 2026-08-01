//! Agent package and local toolchain installation primitives.

/// Downloading, validating, and caching binary agent distributions.
pub mod binary;
mod cache;
/// Detection and installation of supported local toolchains.
pub mod environment;
