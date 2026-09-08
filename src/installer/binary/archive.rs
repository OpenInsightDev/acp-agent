use std::fs::File;
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result, anyhow, bail};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;

use zip::ZipArchive;

use super::paths::{validate_archive_component, validate_archive_path};
use super::{ArchiveLimits, BinaryCacheLock};
#[cfg(test)]
#[allow(dead_code)]
pub(crate) async fn extract_archive(archive_path: PathBuf, destination: PathBuf) -> Result<()> {
    extract_archive_with_limits(archive_path, destination, ArchiveLimits::default()).await
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) async fn extract_archive_with_limits(
    archive_path: PathBuf,
    destination: PathBuf,
    limits: ArchiveLimits,
) -> Result<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    let task_cancel = Arc::clone(&cancel);
    let handle = tokio::task::spawn_blocking(move || {
        extract_archive_blocking(&archive_path, &destination, &task_cancel, limits)
    });
    // Dropping this future (an aborted or cancelled install) sets the flag, so
    // the detached blocking extraction stops before the staging guard removes
    // the directory instead of racing it.
    CancellableExtraction {
        handle: Some(handle),
        cancel,
        cleanup_path: None,
        cache_lock: None,
    }
    .await
}

/// Extraction variant used by installation staging. Once extraction starts,
/// ownership of cleanup moves into the blocking task so cancellation cannot
/// delete a directory while an entry is still being written.
pub(crate) async fn extract_archive_with_cleanup(
    archive_path: PathBuf,
    destination: PathBuf,
    limits: ArchiveLimits,
    cleanup_path: PathBuf,
    cache_lock: Arc<BinaryCacheLock>,
) -> Result<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    let task_cancel = Arc::clone(&cancel);
    let cleanup_on_error = cleanup_path.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let result = extract_archive_blocking(&archive_path, &destination, &task_cancel, limits);
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&cleanup_on_error);
        }
        result
    });
    let mut extraction = CancellableExtraction {
        handle: Some(handle),
        cancel,
        cleanup_path: Some(cleanup_path),
        cache_lock: Some(cache_lock),
    };
    let result = (&mut extraction).await;
    if result.is_ok() {
        extraction.cleanup_path = None;
    }
    result
}

/// [`spawn_blocking`] join that signals a cancellation flag when dropped.
///
/// [`tokio::task::spawn_blocking`] tasks cannot be cancelled directly: dropping
/// the [`JoinHandle`] merely detaches them, so an aborted install would leave a
/// writer running against a directory the staging guard is deleting. Setting
/// the flag from `Drop` lets the blocking extraction cooperate with the
/// cancellation before the guard's removal runs.
struct CancellableExtraction {
    handle: Option<tokio::task::JoinHandle<Result<()>>>,
    cancel: Arc<AtomicBool>,
    cleanup_path: Option<PathBuf>,
    cache_lock: Option<Arc<BinaryCacheLock>>,
}

impl Future for CancellableExtraction {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let handle = self
            .handle
            .as_mut()
            .expect("extraction future polled after completion");
        match Pin::new(handle).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => {
                self.handle.take();
                Poll::Ready(result)
            }
            Poll::Ready(Err(error)) => {
                self.handle.take();
                Poll::Ready(Err(anyhow!("extraction task failed: {error}")))
            }
        }
    }
}

impl Drop for CancellableExtraction {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let Some(path) = self.cleanup_path.take() else {
            return;
        };
        let Some(handle) = self.handle.take() else {
            let _ = std::fs::remove_dir_all(path);
            return;
        };
        let cache_lock = self.cache_lock.take();
        // A blocking extractor cannot be aborted by dropping its JoinHandle.
        // Keep the handle alive until it observes the flag, then remove the
        // staging directory after the last write has completed.
        tokio::spawn(async move {
            let _ = handle.await;
            let _ = tokio::fs::remove_dir_all(path).await;
            drop(cache_lock);
        });
    }
}

pub(crate) fn extract_archive_blocking(
    archive_path: &Path,
    destination: &Path,
    cancel: &AtomicBool,
    limits: ArchiveLimits,
) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("extraction cancelled");
    }
    let file_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if file_name.ends_with(".zip") {
        return extract_zip_with_limits(archive_path, destination, cancel, limits);
    }

    if file_name.ends_with(".tar.gz") || file_name.ends_with(".tgz") {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
        let decoder = CancellableReader::new(GzDecoder::new(file), cancel);
        return extract_tar(decoder, destination, cancel, limits);
    }

    if file_name.ends_with(".tar.bz2") || file_name.ends_with(".tbz2") {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
        let decoder = CancellableReader::new(BzDecoder::new(file), cancel);
        return extract_tar(decoder, destination, cancel, limits);
    }

    let file_name = archive_path
        .file_name()
        .ok_or_else(|| anyhow!("unsupported archive format for {}", archive_path.display()))?;
    if cancel.load(Ordering::Relaxed) {
        bail!("extraction cancelled");
    }
    let size = std::fs::metadata(archive_path)
        .with_context(|| format!("failed to inspect archive {}", archive_path.display()))?
        .len();
    if size > limits.max_expanded_bytes {
        bail!("expanded archive size limit exceeded");
    }
    validate_archive_component(
        file_name
            .to_str()
            .ok_or_else(|| anyhow!("archive filename is not valid UTF-8"))?,
    )?;
    let fallback_path = destination.join(file_name);
    let mut source = File::open(archive_path)
        .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
    let mut output = File::create(&fallback_path)
        .with_context(|| format!("failed to create {}", fallback_path.display()))?;
    copy_with_limit(&mut source, &mut output, cancel, limits.max_expanded_bytes).with_context(
        || {
            format!(
                "failed to copy archive {} to {}",
                archive_path.display(),
                fallback_path.display()
            )
        },
    )?;
    Ok(())
}

struct CancellableReader<'a, R> {
    inner: R,
    cancel: &'a AtomicBool,
}

impl<'a, R> CancellableReader<'a, R> {
    fn new(inner: R, cancel: &'a AtomicBool) -> Self {
        Self { inner, cancel }
    }
}

impl<R: Read> Read for CancellableReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        check_extraction_cancelled(self.cancel)?;
        self.inner.read(buffer)
    }
}

pub(crate) fn check_extraction_cancelled(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "extraction cancelled",
        ))
    } else {
        Ok(())
    }
}

pub(super) fn copy_with_limit<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    cancel: &AtomicBool,
    max_bytes: u64,
) -> io::Result<u64> {
    let mut buffer = [0u8; 32 * 1024];
    let mut copied = 0u64;
    loop {
        check_extraction_cancelled(cancel)?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(copied);
        }
        copied = copied.checked_add(read as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "expanded archive size overflow")
        })?;
        if copied > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expanded archive size limit exceeded",
            ));
        }
        writer.write_all(&buffer[..read])?;
    }
}

/// Extracts a tar archive entry by entry, checking the cancellation flag
/// between entries so an aborted install stops writing promptly.
pub(crate) fn extract_tar<R: io::Read>(
    reader: R,
    destination: &Path,
    cancel: &AtomicBool,
    limits: ArchiveLimits,
) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().with_context(|| {
        format!(
            "failed to read archive entries for {}",
            destination.display()
        )
    })?;
    let mut entries_seen = 0u64;
    let mut expanded = 0u64;
    let mut files = 0u64;
    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            bail!("extraction cancelled");
        }
        let mut entry = entry.with_context(|| {
            format!(
                "failed to read an archive entry for {}",
                destination.display()
            )
        })?;
        let entry_path = entry.path()?;
        validate_archive_path(&entry_path)?;
        if let Some(link) = entry.link_name()? {
            validate_archive_path(&link)?;
        }
        entries_seen += 1;
        if entries_seen > limits.max_entries {
            bail!("archive entry limit exceeded");
        }
        let size = entry.size();
        expanded = expanded
            .checked_add(size)
            .ok_or_else(|| anyhow!("expanded archive size overflow"))?;
        if expanded > limits.max_expanded_bytes {
            bail!("expanded archive size limit exceeded");
        }
        if !entry.header().entry_type().is_dir() {
            files += 1;
            if files > limits.max_files {
                bail!("archive file limit exceeded");
            }
        }
        entry
            .unpack_in(destination)
            .with_context(|| format!("failed to unpack archive into {}", destination.display()))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn extract_zip(
    archive_path: &Path,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    extract_zip_with_limits(archive_path, destination, cancel, ArchiveLimits::default())
}

pub(crate) fn extract_zip_with_limits(
    archive_path: &Path,
    destination: &Path,
    cancel: &AtomicBool,
    limits: ArchiveLimits,
) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("failed to read ZIP archive {}", archive_path.display()))?;

    // Modes are applied in a second pass (children first) so a read-only
    // directory entry cannot prevent its own contents from being extracted.
    let mut unix_modes: Vec<(PathBuf, u32)> = Vec::new();

    let mut expanded = 0u64;
    let mut files = 0u64;
    if archive.len() as u64 > limits.max_entries {
        bail!("archive entry limit exceeded");
    }
    for index in 0..archive.len() {
        if cancel.load(Ordering::Relaxed) {
            bail!("extraction cancelled");
        }
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("failed to read ZIP entry {index}"))?;
        let enclosed = entry.enclosed_name().ok_or_else(|| {
            anyhow!(
                "unsafe path in ZIP archive {}: entry {index}",
                archive_path.display()
            )
        })?;
        validate_archive_path(&enclosed)?;
        let outpath = destination.join(enclosed);
        let expanded_before_entry = expanded;
        let declared_size = entry.size();
        expanded = expanded
            .checked_add(declared_size)
            .ok_or_else(|| anyhow!("expanded archive size overflow"))?;
        if expanded > limits.max_expanded_bytes {
            bail!("expanded archive size limit exceeded");
        }

        if entry.is_dir() {
            std::fs::create_dir_all(&outpath)
                .with_context(|| format!("failed to create directory {}", outpath.display()))?;
            if let Some(mode) = entry.unix_mode() {
                unix_modes.push((outpath, mode));
            }
            continue;
        }

        files += 1;
        if files > limits.max_files {
            bail!("archive file limit exceeded");
        }

        if entry.is_symlink() {
            extract_zip_symlink(
                &mut entry,
                &outpath,
                cancel,
                limits.max_expanded_bytes - expanded_before_entry,
            )?;
            continue;
        }

        if let Some(parent) = outpath.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut outfile = File::create(&outpath)
            .with_context(|| format!("failed to create {}", outpath.display()))?;
        let copied = copy_with_limit(
            &mut entry,
            &mut outfile,
            cancel,
            limits.max_expanded_bytes - expanded_before_entry,
        )
        .with_context(|| format!("failed to write {}", outpath.display()))?;
        if copied != declared_size {
            bail!(
                "ZIP entry size mismatch for {}: declared {declared_size}, extracted {copied}",
                outpath.display()
            );
        }

        if let Some(mode) = entry.unix_mode() {
            unix_modes.push((outpath, mode));
        }
    }

    {
        use std::os::unix::fs::PermissionsExt;

        for (path, mode) in unix_modes.into_iter().rev() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("failed to set permissions on {}", path.display()))?;
        }
    }

    Ok(())
}

/// Creates a symlink entry from a ZIP archive. Symbolic links require the
/// archive to record the target as the entry body; on unsupported platforms
/// the entry is rejected.
pub(crate) fn extract_zip_symlink<R: Read>(
    entry: &mut zip::read::ZipFile<'_, R>,
    outpath: &Path,
    cancel: &AtomicBool,
    max_bytes: u64,
) -> Result<()> {
    let declared_size = entry.size();
    let mut target = Vec::new();
    let copied = copy_with_limit(entry, &mut target, cancel, max_bytes)
        .with_context(|| format!("failed to read symlink target for {}", outpath.display()))?;
    if copied != declared_size {
        bail!(
            "ZIP symlink size mismatch for {}: declared {declared_size}, extracted {copied}",
            outpath.display()
        );
    }
    let target = String::from_utf8(target).with_context(|| {
        format!(
            "symlink target for {} is not valid UTF-8",
            outpath.display()
        )
    })?;
    validate_archive_path(Path::new(&target))
        .with_context(|| format!("unsafe symlink target for {}", outpath.display()))?;

    if let Some(parent) = outpath.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    std::os::unix::fs::symlink(&target, outpath)
        .with_context(|| format!("failed to create symlink {}", outpath.display()))?;
    Ok(())
}
