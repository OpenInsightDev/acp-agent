//! Agent package and local toolchain installation primitives.

/// Downloading, validating, and caching binary agent distributions.
pub mod binary;
/// Cache path layout and local cache inventory helpers.
pub(crate) mod cache;
/// Detection and installation of supported local toolchains.
pub mod environment;
/// Agent installation, update, uninstall, and local inventory operations.
pub mod lifecycle;

#[cfg(test)]
pub(crate) mod test_support {
    /// Serializes tests that temporarily mutate the process-wide `PATH`
    /// environment variable, which would otherwise race when the test binary
    /// runs them in parallel.
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
