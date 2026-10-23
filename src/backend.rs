use anyhow::Result;
use std::path::Path;
use trash::TrashItem;

pub trait Backend {
    fn supports_history(&self) -> bool {
        true
    }
    fn list(&self) -> Result<Vec<TrashItem>>;
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
