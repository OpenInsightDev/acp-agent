use super::archive::check_extraction_cancelled;
use super::*;
pub(crate) async fn prepare_staging_directory(
    staging: tempfile::TempDir,
    target: &BinaryTarget,
    metadata: &BinaryCacheMetadata,
    cache_lock: Arc<BinaryCacheLock>,
) -> Result<PreparedStaging> {
    let staging_dir = staging.path().to_path_buf();
    let archive_path = download_archive(target, &staging_dir).await?;
    let extracted_dir = staging_dir.join(EXTRACTED_DIR_NAME);
    fs::create_dir_all(&extracted_dir)
        .await
        .with_context(|| format!("failed to create {}", extracted_dir.display()))?;
    // The blocking extractor now owns cleanup if cancellation occurs. Keep
    // the TempDir from racing it, then promotion takes ownership on success.
    let staging_path = staging.keep();
    let extraction = extract_archive_with_cleanup(
        archive_path,
        extracted_dir.clone(),
        ArchiveLimits::default(),
        staging_path.clone(),
        Arc::clone(&cache_lock),
    )
    .await;
    if let Err(error) = extraction {
        cleanup_dir(&staging_path).await;
        return Err(error);
    }
    let executable_path = match resolve_cmd_path(&extracted_dir, &target.cmd) {
        Ok(path) => path,
        Err(error) => {
            cleanup_dir(&staging_path).await;
            return Err(error);
        }
    };
    let metadata_path = staging_dir.join(METADATA_FILE_NAME);
    // Keep every post-extraction filesystem operation owned by one blocking
    // task. If this future is cancelled, its Drop implementation waits for
    // that task before removing staging, including on platforms that keep
    // files open while hashing or writing metadata.
    let mut validation = PostExtractionValidation::new(
        staging_path,
        executable_path,
        extracted_dir.clone(),
        metadata_path,
        metadata.clone(),
        cache_lock,
    );
    (&mut validation).await?;
    Ok(validation.disarm())
}
/// Owns a fully prepared staging directory between extraction and publication.
///
/// `TempDir::keep` is required while the cooperative blocking tasks are
/// running, but returning its path alone would reopen a cancellation leak
/// while the publisher waits for the payload-use writer lease. This guard keeps
/// both the path and publication lock alive until cleanup has finished.
pub(crate) struct PreparedStaging {
    path: Option<PathBuf>,
    cache_lock: Option<Arc<BinaryCacheLock>>,
}

impl PreparedStaging {
    pub(crate) fn new(path: PathBuf, cache_lock: Arc<BinaryCacheLock>) -> Self {
        Self {
            path: Some(path),
            cache_lock: Some(cache_lock),
        }
    }

    #[cfg(test)]
    pub(crate) fn without_lock(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            cache_lock: None,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("prepared staging path must remain owned")
    }

    pub(crate) fn disarm(&mut self) {
        self.path.take();
    }

    /// Removes the directory synchronously from a blocking worker. Keeping
    /// this operation synchronous is important: a dropped `tokio::fs` future
    /// can detach its own blocking worker and race a later publisher.
    pub(crate) fn cleanup_blocking(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.take() else {
            return Ok(());
        };
        match std::fs::remove_dir_all(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                self.path = Some(path);
                Err(error)
            }
        }
    }
}

impl Drop for PreparedStaging {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let cache_lock = self.cache_lock.take();
        let cleanup = move || {
            let _ = std::fs::remove_dir_all(path);
            drop(cache_lock);
        };
        // The guard is normally dropped inside a Tokio task while waiting for
        // the use-write lease. Move cleanup to a blocking worker so a large
        // abandoned payload cannot stall the async runtime. The fallback is
        // for tests or teardown after the runtime has already gone away.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            drop(handle.spawn_blocking(cleanup));
        } else {
            cleanup();
        }
    }
}
/// Owns all post-extraction validation work and the staging directory it
/// operates on. A dropped join handle is detached by Tokio, so cleanup waits
/// for the task explicitly before deleting the directory.
struct PostExtractionValidation {
    handle: Option<tokio::task::JoinHandle<Result<()>>>,
    cancel: Arc<AtomicBool>,
    cleanup_path: Option<PathBuf>,
    cache_lock: Option<Arc<BinaryCacheLock>>,
}

impl PostExtractionValidation {
    fn new(
        cleanup_path: PathBuf,
        executable_path: PathBuf,
        extracted_dir: PathBuf,
        metadata_path: PathBuf,
        mut metadata: BinaryCacheMetadata,
        cache_lock: Arc<BinaryCacheLock>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let task_cancel = Arc::clone(&cancel);
        let handle = tokio::task::spawn_blocking(move || {
            let file_metadata = std::fs::metadata(&executable_path);
            if file_metadata
                .as_ref()
                .map(|metadata| !metadata.is_file())
                .unwrap_or(true)
            {
                bail!(
                    "downloaded payload does not contain executable {}",
                    executable_path.display()
                );
            }
            make_executable_blocking(&executable_path).with_context(|| {
                format!("failed to mark {} executable", executable_path.display())
            })?;
            let digest = hash_file_sha256_blocking(&executable_path, &task_cancel)?;
            metadata.executable_sha256 = Some(digest);
            metadata.payload_sha256 =
                Some(hash_payload_sha256_blocking(&extracted_dir, &task_cancel)?);
            check_extraction_cancelled(&task_cancel)?;
            let metadata_bytes =
                to_vec_pretty(&metadata).context("failed to encode cached binary metadata")?;
            check_extraction_cancelled(&task_cancel)?;
            std::fs::write(&metadata_path, metadata_bytes)
                .with_context(|| format!("failed to write {}", metadata_path.display()))?;
            Ok(())
        });
        Self {
            handle: Some(handle),
            cancel,
            cleanup_path: Some(cleanup_path),
            cache_lock: Some(cache_lock),
        }
    }

    fn disarm(mut self) -> PreparedStaging {
        let path = self
            .cleanup_path
            .take()
            .expect("post-extraction validation must own a path");
        let cache_lock = self
            .cache_lock
            .take()
            .expect("post-extraction validation must own the cache lock");
        PreparedStaging::new(path, cache_lock)
    }
}

impl Future for PostExtractionValidation {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let handle = self
            .handle
            .as_mut()
            .expect("post-extraction validation polled after completion");
        match Pin::new(handle).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => {
                self.handle.take();
                Poll::Ready(result)
            }
            Poll::Ready(Err(error)) => {
                self.handle.take();
                Poll::Ready(Err(anyhow!(
                    "post-extraction validation task failed: {error}"
                )))
            }
        }
    }
}

impl Drop for PostExtractionValidation {
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
        tokio::spawn(async move {
            let _ = handle.await;
            let _ = tokio::fs::remove_dir_all(path).await;
            drop(cache_lock);
        });
    }
}
pub(crate) async fn cleanup_dir(path: &Path) {
    let _ = fs::remove_dir_all(path).await;
}
