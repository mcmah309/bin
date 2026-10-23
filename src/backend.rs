use eros::Result;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// Backend-neutral receipt. IDs remain native and stable for existing SQLite
/// history, while storage policy belongs to bin rather than a third-party type.
#[derive(Clone, Debug)]
pub struct TrashItem {
    pub id: OsString,
    pub name: OsString,
    pub original_parent: PathBuf,
    pub time_deleted: i64,
}

impl TrashItem {
    pub fn original_path(&self) -> PathBuf {
        self.original_parent.join(&self.name)
    }
}

pub trait Backend {
    fn supports_history(&self) -> bool {
        true
    }
    fn list(&self) -> Result<Vec<TrashItem>>;
    fn available(&self, _item: &TrashItem) -> Result<bool> {
        Ok(true)
    }
    fn trash(&mut self, path: &Path) -> Result<()>;
    fn restore(&mut self, item: TrashItem) -> Result<()>;
    fn purge(&mut self, item: &TrashItem) -> Result<()>;
}

pub struct SystemTrash {
    context: system_trash::TrashContext,
}

impl SystemTrash {
    pub fn new(_location: crate::config::TrashLocation) -> Self {
        let context = system_trash::TrashContext::new();
        #[cfg(all(unix, not(target_os = "macos")))]
        let context = {
            use system_trash::freedesktop::{TrashContextExtFreedesktop, TrashLocation};
            let mut context = context;
            context.set_trash_location(match _location {
                crate::config::TrashLocation::Home => TrashLocation::Home,
                crate::config::TrashLocation::PerFilesystem => TrashLocation::PerFilesystem,
            });
            context
        };
        Self { context }
    }
}

#[cfg(not(target_os = "macos"))]
impl From<system_trash::TrashItem> for TrashItem {
    fn from(item: system_trash::TrashItem) -> Self {
        Self {
            id: item.id,
            name: item.name,
            original_parent: item.original_parent,
            time_deleted: item.time_deleted,
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl From<TrashItem> for system_trash::TrashItem {
    fn from(item: TrashItem) -> Self {
        Self {
            id: item.id,
            name: item.name,
            original_parent: item.original_parent,
            time_deleted: item.time_deleted,
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl Backend for SystemTrash {
    fn list(&self) -> Result<Vec<TrashItem>> {
        Ok(system_trash::os_limited::list()?
            .into_iter()
            .map(TrashItem::from)
            .collect())
    }

    fn available(&self, item: &TrashItem) -> Result<bool> {
        use eros::{Context, IntoAnyUnion};
        match system_trash::os_limited::metadata(&item.clone().into()) {
            Ok(_) => Ok(true),
            #[cfg(windows)]
            Err(system_trash::Error::Os { code, .. })
                if matches!(code as u32, 0x80070002 | 0x80070003) =>
            {
                Ok(false)
            }
            #[cfg(unix)]
            Err(system_trash::Error::FileSystem { source, .. })
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error)
                .any_union()
                .context("cannot inspect system trash item"),
        }
    }

    fn trash(&mut self, path: &Path) -> Result<()> {
        Ok(self.context.delete(path)?)
    }

    fn restore(&mut self, item: TrashItem) -> Result<()> {
        #[cfg(windows)]
        std::fs::create_dir_all(&item.original_parent)?;
        Ok(system_trash::os_limited::restore_all([item.into()])?)
    }

    fn purge(&mut self, item: &TrashItem) -> Result<()> {
        Ok(system_trash::os_limited::purge_all([
            system_trash::TrashItem::from(item.clone()),
        ])?)
    }
}

#[cfg(target_os = "macos")]
impl Backend for SystemTrash {
    fn supports_history(&self) -> bool {
        false
    }
    fn list(&self) -> Result<Vec<TrashItem>> {
        eros::bail!("listing, pop, and empty are not yet supported on macOS")
    }
    fn trash(&mut self, path: &Path) -> Result<()> {
        Ok(self.context.delete(path)?)
    }
    fn restore(&mut self, _: TrashItem) -> Result<()> {
        eros::bail!("pop is not yet supported on macOS")
    }
    fn purge(&mut self, _: &TrashItem) -> Result<()> {
        eros::bail!("empty is not yet supported on macOS")
    }
}
