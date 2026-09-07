use super::*;

pub(crate) fn validate_archive_path(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow!("archive path is not valid UTF-8"))?;
    if text.contains('\\') || text.contains('\0') {
        bail!("unsafe archive path: {text:?}");
    }
    let mut has_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("unsafe archive path: {text:?}");
            }
        }
    }
    if !has_normal {
        bail!("empty archive path");
    }
    Ok(())
}

pub(crate) fn validate_archive_component(component: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.contains('\\')
        || component.contains('\0')
    {
        bail!("unsafe archive filename: {component:?}");
    }
    Ok(())
}

pub(crate) fn resolve_cmd_path(extracted_dir: &Path, cmd: &str) -> Result<PathBuf> {
    let sanitized = cmd.trim();
    let candidate = PathBuf::from(sanitized);
    if candidate.is_absolute() {
        bail!("binary command path must be relative to the extracted payload: {cmd}");
    }

    let mut resolved = extracted_dir.to_path_buf();
    for component in candidate.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                bail!("binary command path must stay within the extracted payload: {cmd}");
            }
            Component::Prefix(_) | Component::RootDir => {
                bail!("binary command path must be relative to the extracted payload: {cmd}");
            }
            other => resolved.push(other.as_os_str()),
        }
    }

    Ok(resolved)
}
