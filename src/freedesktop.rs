//! FreeDesktop Trash storage owned by bin. Metadata and directory layout follow
//! https://specifications.freedesktop.org/trash/1.0/ and remain interoperable.
mod mounts;
mod transfer;

use crate::{
    backend::{Backend, TrashItem},
    config::TrashLocation,
};
use chrono::{Local, NaiveDateTime, TimeZone};
use eros::{Context, IntoAnyUnion, Result, bail};
use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

pub struct FreedesktopTrash {
    location: TrashLocation,
    home: OnceLock<PathBuf>,
    mounts: OnceLock<mounts::Mounts>,
}

#[derive(Clone)]
struct Directory {
    root: PathBuf,
    original_base: PathBuf,
}

impl FreedesktopTrash {
    pub fn new(location: TrashLocation) -> Self {
        Self {
            location,
            home: OnceLock::new(),
            mounts: OnceLock::new(),
        }
    }

    fn home(&self) -> Result<PathBuf> {
        if let Some(home) = self.home.get() {
            return Ok(home.clone());
        }
        let dirs = directories::BaseDirs::new().context("cannot determine home trash directory")?;
        let home = mounts::canonical_prefix(&dirs.data_dir().join("Trash"))?;
        let _ = self.home.set(home.clone());
        Ok(home)
    }

    fn mounts(&self) -> Result<&[PathBuf]> {
        if self.mounts.get().is_none() {
            let _ = self.mounts.set(mounts::mounted_roots()?);
        }
        Ok(&self.mounts.get().unwrap().roots)
    }

    fn directories(&self) -> Result<Vec<Directory>> {
        let home = self.home()?;
        let mut directories = Vec::new();
        if checked_directory(&home, false)? {
            directories.push(Directory {
                original_base: home.parent().unwrap().to_path_buf(),
                root: home,
            });
        }
        let uid = rustix::process::getuid().as_raw().to_string();
        self.mounts()?;
        for mount in &self.mounts.get().unwrap().trash_roots {
            if shared_trash(&mount.join(".Trash"))? {
                let root = mount.join(".Trash").join(&uid);
                if checked_directory(&root, false)? {
                    directories.push(Directory {
                        root,
                        original_base: mount.clone(),
                    });
                }
            }
            let root = mount.join(format!(".Trash-{uid}"));
            if checked_directory(&root, false)? {
                directories.push(Directory {
                    root,
                    original_base: mount.clone(),
                });
            }
        }
        let mut seen = HashSet::new();
        directories.retain(|directory| seen.insert(directory.root.clone()));
        Ok(directories)
    }

    fn destination(&self, source: &Path) -> Result<PathBuf> {
        let home = self.home()?;
        if self.location == TrashLocation::Home {
            return Ok(home);
        }
        let mounts = self.mounts()?;
        let source_mount = mounts::containing(mounts, source)?;
        if source_mount == mounts::containing(mounts, &home)? {
            return Ok(home);
        }
        let uid = rustix::process::getuid().as_raw().to_string();
        if shared_trash(&source_mount.join(".Trash"))? {
            let root = source_mount.join(".Trash").join(&uid);
            if prepare(&root).is_ok() {
                return Ok(root);
            }
        }
        let root = source_mount.join(format!(".Trash-{uid}"));
        match prepare(&root) {
            Ok(()) => Ok(root),
            Err(error)
                if error
                    .downcast_inner_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied) =>
            {
                Ok(home)
            }
            Err(error) => Err(error),
        }
    }

    fn payload(&self, item: &TrashItem) -> Result<PathBuf> {
        let info = Path::new(&item.id);
        if !info.is_absolute() || info.extension() != Some(OsStr::new("trashinfo")) {
            bail!("invalid trash receipt ID");
        }
        let info_dir = info.parent().context("trash receipt has no parent")?;
        if info_dir.file_name() != Some(OsStr::new("info")) {
            bail!("trash receipt is not in an info directory");
        }
        let root = info_dir
            .parent()
            .context("trash receipt has no trash directory")?;
        let uid = rustix::process::getuid().as_raw().to_string();
        let home = self.home()?;
        let mounts = self.mounts()?;
        let valid = root == home
            || mounts
                .iter()
                .any(|mount| root == mount.join(format!(".Trash-{uid}")))
            || (root.file_name() == Some(OsStr::new(&uid))
                && root
                    .parent()
                    .is_some_and(|parent| parent.file_name() == Some(OsStr::new(".Trash")))
                && root
                    .parent()
                    .and_then(Path::parent)
                    .is_some_and(|parent| mounts.iter().any(|mount| mount == parent))
                && shared_trash(root.parent().unwrap())?);
        if !valid {
            bail!("receipt is outside the user's known trash directories");
        }
        checked_directory(root, false)?;
        checked_directory(&root.join("files"), false)?;
        Ok(root
            .join("files")
            .join(info.file_stem().context("trash receipt has no filename")?))
    }
}

impl Backend for FreedesktopTrash {
    fn list(&self) -> Result<Vec<TrashItem>> {
        let mut items = Vec::new();
        for directory in self.directories()? {
            let info = directory.root.join("info");
            if !checked_directory(&info, false)? {
                continue;
            }
            let entries = fs::read_dir(&info)
                .with_context(|| format!("cannot list trash metadata {info:?}"))?;
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) if missing(&error) => continue,
                    Err(error) => return Err(error.into()),
                };
                let path = entry.path();
                if path.extension() != Some(OsStr::new("trashinfo")) {
                    continue;
                }
                let metadata = match entry.file_type() {
                    Ok(metadata) => metadata,
                    Err(error) if missing(&error) => continue,
                    Err(error) => return Err(error.into()),
                };
                if !metadata.is_file() {
                    continue;
                }
                let bytes = match fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(error) if missing(&error) => continue,
                    Err(error) => {
                        return Err(error)
                            .any_union()
                            .with_context(|| format!("cannot read trash receipt {path:?}"));
                    }
                };
                if bytes.is_empty() {
                    continue;
                } // Exclusive reservation / copy in progress.
                match parse_receipt(&path, &bytes, &directory.original_base) {
                    Ok(item) => items.push(item),
                    Err(error) => {
                        eprintln!("bin: skipping malformed trash receipt {path:?}: {error:#?}")
                    }
                }
            }
        }
        Ok(items)
    }

    fn available(&self, item: &TrashItem) -> Result<bool> {
        let payload = self.payload(item)?;
        match payload.symlink_metadata() {
            Ok(_) => Ok(true),
            Err(error) if missing(&error) => Ok(false),
            Err(error) => Err(error)
                .any_union()
                .with_context(|| format!("cannot inspect trash payload {payload:?}")),
        }
    }

    fn trash(&mut self, source: &Path) -> Result<()> {
        if !source.is_absolute() {
            bail!("trash source must be absolute");
        }
        let metadata = source.symlink_metadata()?;
        let roots = self.mounts()?;
        if roots.iter().any(|root| root == source) {
            bail!("filesystem mount roots cannot be trashed");
        }
        let root = self.destination(source)?;
        let uid = rustix::process::getuid().as_raw().to_string();
        let mut trash_roots = vec![self.home()?];
        for mount in roots {
            trash_roots.push(mount.join(format!(".Trash-{uid}")));
            trash_roots.push(mount.join(".Trash").join(&uid));
        }
        for trash_root in trash_roots {
            if source.starts_with(&trash_root)
                || (metadata.is_dir() && trash_root.starts_with(source))
            {
                bail!(
                    "cannot trash a directory containing trash, or trash an existing trash payload"
                );
            }
        }
        if metadata.is_dir() && root.starts_with(source) {
            bail!("trash destination is inside its source");
        }
        prepare(&root)?;
        let original = if root == self.home()? {
            source.to_path_buf()
        } else {
            // Per-volume metadata survives a change in mount location. Home
            // trash uses absolute paths because sources may be on any device.
            source
                .strip_prefix(mounts::containing(self.mounts()?, source)?)?
                .to_path_buf()
        };
        let original_name = source.file_name().context("source has no filename")?;
        for index in 0_u64.. {
            let name = storage_name(original_name, index);
            let target = root.join("files").join(&name);
            let mut info_name = name;
            info_name.push(".trashinfo");
            let info = root.join("info").join(info_name);
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&info)
            {
                Ok(file) => drop(file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .any_union()
                        .context("cannot reserve a trash receipt");
                }
            }
            let mut reservation = ReceiptReservation {
                path: info.clone(),
                keep: false,
            };
            // An orphaned payload from another tool must never be overwritten or
            // associated with our newly reserved receipt.
            match target.symlink_metadata() {
                Ok(_) => continue,
                Err(error) if missing(&error) => {}
                Err(error) => return Err(error.into()),
            }
            write_receipt(&info, &original)?;
            match transfer::rename_new(source, &target) {
                Ok(()) => {
                    reservation.keep = true;
                    return transfer::sync_directory(&root.join("files"));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                    // Until the complete copy is published, an empty receipt is
                    // invisible to bin and other standards-compliant readers.
                    let file = OpenOptions::new().write(true).truncate(true).open(&info)?;
                    file.sync_all()?;
                    return transfer::copy_then_remove(source, &target, || {
                        write_receipt(&info, &original)?;
                        reservation.keep = true;
                        Ok(())
                    });
                }
                Err(error) => {
                    return Err(error)
                        .any_union()
                        .context("cannot move source into trash");
                }
            }
        }
        unreachable!("all possible trash filenames exhausted")
    }

    fn restore(&mut self, item: TrashItem) -> Result<()> {
        let payload = self.payload(&item)?;
        let target = item.original_path();
        fs::create_dir_all(&item.original_parent)?;
        match transfer::rename_new(&payload, &target) {
            Ok(()) => transfer::sync_directory(&item.original_parent)?,
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                transfer::copy_then_remove(&payload, &target, || Ok(()))?
            }
            Err(error) => {
                return Err(error)
                    .any_union()
                    .with_context(|| format!("cannot restore without overwriting {target:?}"));
            }
        }
        remove_receipt(Path::new(&item.id))
    }

    fn purge(&mut self, item: &TrashItem) -> Result<()> {
        transfer::remove(&self.payload(item)?)?;
        remove_receipt(Path::new(&item.id))
    }
}

fn missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn checked_directory(path: &Path, create: bool) -> Result<bool> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if missing(&error) && !create => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
            path.symlink_metadata()?
        }
        Err(error) => {
            return Err(error)
                .any_union()
                .with_context(|| format!("cannot inspect trash directory {path:?}"));
        }
    };
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
    {
        bail!("trash directory {path:?} must be a real directory owned by the current user");
    }
    Ok(true)
}

fn prepare(root: &Path) -> Result<()> {
    for path in [root.to_path_buf(), root.join("files"), root.join("info")] {
        checked_directory(&path, true)?;
    }
    Ok(())
}

fn shared_trash(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(metadata) => Ok(metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.mode() & 0o1000 != 0),
        Err(error) if missing(&error) => Ok(false),
        Err(error) => Err(error)
            .any_union()
            .with_context(|| format!("cannot inspect shared trash {path:?}")),
    }
}

struct ReceiptReservation {
    path: PathBuf,
    keep: bool,
}
impl Drop for ReceiptReservation {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn write_receipt(path: &Path, original: &Path) -> Result<()> {
    let parent = path.parent().context("receipt has no parent")?;
    let mut temp = tempfile::Builder::new()
        .prefix(".bin-info-")
        .tempfile_in(parent)?;
    writeln!(
        temp,
        "[Trash Info]\nPath={}\nDeletionDate={}",
        encode_path(original),
        Local::now().format("%Y-%m-%dT%H:%M:%S")
    )?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .context("cannot publish trash metadata")?;
    transfer::sync_directory(parent)
}

fn remove_receipt(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if missing(&error) => {}
        Err(error) => return Err(error.into()),
    }
    transfer::sync_directory(path.parent().context("receipt has no parent")?)
}

fn storage_name(original: &OsStr, index: u64) -> OsString {
    let mut name = if original.as_bytes().len() > 220 {
        OsString::from(format!(
            "bin-{}-{}",
            std::process::id(),
            Local::now().timestamp_micros()
        ))
    } else {
        original.to_os_string()
    };
    if index > 0 {
        name.push(format!(".{}", index + 1));
    }
    name
}

fn encode_path(path: &Path) -> String {
    let mut encoded = String::new();
    const HEX: &[u8] = b"0123456789ABCDEF";
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 15) as usize] as char);
        }
    }
    encoded
}

fn decode_path(text: &str) -> Result<PathBuf> {
    let mut bytes = Vec::new();
    let input = text.as_bytes();
    let mut offset = 0;
    while offset < input.len() {
        if input[offset] == b'%' {
            let pair = input
                .get(offset + 1..offset + 3)
                .context("incomplete path escape")?;
            let high = (pair[0] as char)
                .to_digit(16)
                .context("invalid path escape")?;
            let low = (pair[1] as char)
                .to_digit(16)
                .context("invalid path escape")?;
            bytes.push((high * 16 + low) as u8);
            offset += 3;
        } else {
            bytes.push(input[offset]);
            offset += 1;
        }
    }
    if bytes.contains(&0) {
        bail!("trash path contains NUL");
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn parse_receipt(id: &Path, bytes: &[u8], base: &Path) -> Result<TrashItem> {
    let text = std::str::from_utf8(bytes).context("receipt is not UTF-8")?;
    let mut lines = text.lines();
    if lines.next() != Some("[Trash Info]") {
        bail!("missing [Trash Info] header");
    }
    let mut original = None;
    let mut deleted = None;
    for line in lines {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Path" if original.is_none() => original = Some(decode_path(value)?),
            "DeletionDate" if deleted.is_none() => deleted = Some(value),
            _ => {}
        }
    }
    let path = original.context("receipt has no original path")?;
    if path.as_os_str().is_empty() {
        bail!("receipt has an empty original path");
    }
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        bail!("trash path contains '..'");
    }
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    let name = path
        .file_name()
        .context("original path has no filename")?
        .to_os_string();
    let original_parent = path
        .parent()
        .context("original path has no parent")?
        .to_path_buf();
    let time_deleted = deleted
        .and_then(|text| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").ok())
        .and_then(|date| Local.from_local_datetime(&date).earliest())
        .map(|date| date.timestamp())
        .unwrap_or(-1);
    Ok(TrashItem {
        id: id.as_os_str().to_os_string(),
        name,
        original_parent,
        time_deleted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_paths_round_trip_and_relative_receipts_use_the_correct_base() {
        let original = PathBuf::from(OsString::from_vec(b"/path/a=b %\xff".to_vec()));
        assert_eq!(decode_path(&encode_path(&original)).unwrap(), original);
        let item = parse_receipt(Path::new("/trash/info/id.trashinfo"),
            b"[Trash Info]\n\n# comment\nUnknown=ignored\nPath=dir/a%3Db\nPath=wrong\nDeletionDate=2024-01-01T12:00:00\n", Path::new("/mount")).unwrap();
        assert_eq!(item.original_path(), Path::new("/mount/dir/a=b"));
    }
    #[test]
    fn malformed_receipts_are_errors_instead_of_panics() {
        for bytes in [
            b"".as_slice(),
            b"wrong header",
            b"[Trash Info]\nPath=%QQ\n",
            b"[Trash Info]\nPath=../escape\n",
            b"[Trash Info]\nPath=%00\n",
        ] {
            assert!(
                parse_receipt(
                    Path::new("/trash/info/id.trashinfo"),
                    bytes,
                    Path::new("/mount")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn shared_trash_requires_a_real_sticky_directory() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join(".Trash");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!shared_trash(&shared).unwrap());
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(shared_trash(&shared).unwrap());
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&shared, &alias).unwrap();
        assert!(!shared_trash(&alias).unwrap());
    }
}
