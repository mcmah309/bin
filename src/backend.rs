#[cfg(not(target_os = "macos"))]
use anyhow::Context;
use anyhow::Result;
use std::path::Path;
use trash::TrashItem;

pub trait Backend {
    fn supports_history(&self) -> bool {
        true
    }
    fn list(&self) -> Result<Vec<TrashItem>>;
    /// Confirm that a listed receipt still has a payload. Missing data is stale;
    /// permission and other I/O errors must not make history disappear.
    fn available(&self, _item: &TrashItem) -> Result<bool> {
        Ok(true)
    }
    fn trash(&mut self, path: &Path) -> Result<()>;
    fn restore(&mut self, item: TrashItem) -> Result<()>;
    fn purge(&mut self, item: &TrashItem) -> Result<()>;
}

pub struct SystemTrash;

#[cfg(not(target_os = "macos"))]
impl Backend for SystemTrash {
    fn list(&self) -> Result<Vec<TrashItem>> {
        Ok(trash::os_limited::list()?)
    }

    fn available(&self, item: &TrashItem) -> Result<bool> {
        #[cfg(unix)]
        {
            // FreeDesktop list() enumerates .trashinfo files, even when someone
            // has manually deleted their corresponding payloads. Check the
            // entry itself without following dangling symlinks or walking trees.
            let info = Path::new(&item.id);
            let trash = info
                .parent()
                .and_then(Path::parent)
                .context("invalid native trash ID")?;
            let name = info.file_stem().context("invalid native trash filename")?;
            let payload = trash.join("files").join(name);
            match std::fs::symlink_metadata(&payload) {
                Ok(_) => Ok(true),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    Ok(false)
                }
                Err(error) => {
                    Err(error).with_context(|| format!("cannot inspect trash payload {payload:?}"))
                }
            }
        }
        #[cfg(windows)]
        {
            match trash::os_limited::metadata(item) {
                Ok(_) => Ok(true),
                // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND).
                Err(trash::Error::Os { code, .. })
                    if matches!(code as u32, 0x80070002 | 0x80070003) =>
                {
                    Ok(false)
                }
                Err(error) => Err(error).context("cannot inspect Recycle Bin item"),
            }
        }
    }

    fn trash(&mut self, path: &Path) -> Result<()> {
        Ok(trash::delete(path)?)
    }

    fn restore(&mut self, item: TrashItem) -> Result<()> {
        std::fs::create_dir_all(&item.original_parent)?;
        Ok(trash::os_limited::restore_all([item])?)
    }

    fn purge(&mut self, item: &TrashItem) -> Result<()> {
        Ok(trash::os_limited::purge_all([item])?)
    }
}

#[cfg(target_os = "macos")]
impl Backend for SystemTrash {
    fn supports_history(&self) -> bool {
        false
    }

    fn list(&self) -> Result<Vec<TrashItem>> {
        anyhow::bail!("the trash crate does not support listing, pop, or empty on macOS")
    }

    fn trash(&mut self, path: &Path) -> Result<()> {
        Ok(trash::delete(path)?)
    }

    fn restore(&mut self, _: TrashItem) -> Result<()> {
        anyhow::bail!("the trash crate does not support pop on macOS")
    }

    fn purge(&mut self, _: &TrashItem) -> Result<()> {
        anyhow::bail!("the trash crate does not support empty on macOS")
    }
}
