use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

/// Linux's no-replace rename closes the collision-check race for files,
/// directories and symlinks without creating an empty destination placeholder.
pub fn rename_new(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        renameat_with(CWD, source, CWD, target, RenameFlags::NOREPLACE).map_err(Into::into)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Portable Unix fallback reserves the destination before rename. Linux
        // uses the atomic API above; BSD concurrency with external writers has
        // the same limits as other filesystem changes during an invocation.
        if source.symlink_metadata()?.is_dir() {
            fs::DirBuilder::new().mode(0o700).create(target)?;
        } else {
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(target)?;
        }
        let result = fs::rename(source, target);
        if result.is_err() {
            let _ = discard(target);
        }
        result
    }
}

pub fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .with_context(|| format!("cannot sync directory {path:?}"))
}

/// Stage on the destination filesystem, publish the complete copy without
/// overwrite, commit its receipt, and only then remove source data. Copy or
/// publish failures cannot leave a partial original/restore destination.
pub fn copy_then_remove(
    source: &Path,
    target: &Path,
    before_remove: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let parent = target.parent().context("destination has no parent")?;
    let stage = tempfile::Builder::new()
        .prefix(".bin-transfer-")
        .tempdir_in(parent)?;
    let payload = stage.path().join("payload");
    let copied = copy_tree(source, &payload).and_then(|_| sync_directory(stage.path()));
    if let Err(error) = copied {
        let _ = discard(&payload);
        return Err(error);
    }
    if let Err(error) = rename_new(&payload, target) {
        let _ = discard(&payload);
        return Err(error).with_context(|| format!("cannot publish copied item at {target:?}"));
    }
    let ready = sync_directory(parent).and_then(|_| before_remove());
    if let Err(error) = ready {
        // The source has not been touched. Remove only our copied destination.
        let _ = discard(target);
        return Err(error);
    }
    // A failure here must keep the complete, committed destination: removing a
    // directory can fail after some source children have already been removed.
    remove(source).with_context(|| {
        format!("copy is complete at {target:?}, but source cleanup failed for {source:?}")
    })?;
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    let metadata = source
        .symlink_metadata()
        .with_context(|| format!("cannot inspect {source:?}"))?;
    if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
        return Ok(());
    }
    if metadata.is_dir() {
        fs::DirBuilder::new().mode(0o700).create(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        // The staging parent is private. Reserve each copied file exclusively.
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(target)?;
        fs::copy(source, target).with_context(|| format!("cannot copy {source:?}"))?;
    } else {
        bail!("cannot copy special file {source:?} across filesystems; source left intact");
    }
    let file = File::open(target)?;
    let mut times = fs::FileTimes::new();
    if let Ok(time) = metadata.accessed() {
        times = times.set_accessed(time);
    }
    if let Ok(time) = metadata.modified() {
        times = times.set_modified(time);
    }
    file.set_times(times)?;
    file.set_permissions(metadata.permissions())?;
    file.sync_all()?;
    Ok(())
}

pub fn remove(path: &Path) -> io::Result<()> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Remove an owned staging/copy tree, allowing cleanup of read-only directories
/// whose original modes were preserved. Never follows symlinks or edits a source.
fn discard(path: &Path) -> io::Result<()> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => {
            fs::set_permissions(
                path,
                fs::Permissions::from_mode(metadata.permissions().mode() | 0o700),
            )?;
            for entry in fs::read_dir(path)? {
                discard(&entry?.path())?;
            }
            fs::remove_dir(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publish_never_overwrites_and_failed_commit_leaves_source_intact() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::write(&source, "source").unwrap();
        fs::write(&target, "existing").unwrap();
        assert!(rename_new(&source, &target).is_err());
        assert_eq!(fs::read_to_string(&source).unwrap(), "source");
        assert_eq!(fs::read_to_string(&target).unwrap(), "existing");
        fs::remove_file(&target).unwrap();
        assert!(copy_then_remove(&source, &target, || bail!("simulated receipt failure")).is_err());
        assert_eq!(fs::read_to_string(&source).unwrap(), "source");
        assert!(!target.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_publishers_have_one_winner_without_losing_the_other_source() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        let target = root.path().join("target");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                rename_new(&a, &target)
            });
            let second = scope.spawn(|| {
                barrier.wait();
                rename_new(&b, &target)
            });
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(usize::from(a.exists()) + usize::from(b.exists()), 1);
        assert!(matches!(
            fs::read_to_string(target).unwrap().as_str(),
            "a" | "b"
        ));
    }
}
