use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
};
use trash::TrashItem;

// JSON strings cannot represent all Unix paths or unpaired Windows surrogates.
// Store native units, without unsafe conversions or lossy display-name matching.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NativeString {
    #[cfg(unix)]
    units: Vec<u8>,
    #[cfg(windows)]
    units: Vec<u16>,
}

impl NativeString {
    pub fn new(value: &OsStr) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self {
                units: value.as_bytes().to_vec(),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self {
                units: value.encode_wide().collect(),
            }
        }
    }

    pub fn to_os_string(&self) -> OsString {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(self.units.clone())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            OsString::from_wide(&self.units)
        }
    }

    pub fn path(&self) -> PathBuf {
        PathBuf::from(self.to_os_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemKey {
    id: NativeString,
    original: NativeString,
    time_deleted: i64,
}

impl From<&TrashItem> for ItemKey {
    fn from(item: &TrashItem) -> Self {
        Self {
            id: NativeString::new(&item.id),
            original: NativeString::new(item.original_path().as_os_str()),
            time_deleted: item.time_deleted,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Pending {
    pub original: NativeString,
    pub before: Vec<ItemKey>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Invocation {
    pub items: Vec<ItemKey>,
    pub pending: Option<Pending>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct History {
    version: u32,
    pub invocations: Vec<Invocation>,
}

impl Default for History {
    fn default() -> Self {
        Self {
            version: 1,
            invocations: Vec::new(),
        }
    }
}

impl History {
    /// Recover the small window between a successful native trash operation and
    /// saving its receipt. Never guess if external trash activity is ambiguous.
    pub fn reconcile(&mut self, live: &[TrashItem]) -> Result<()> {
        let available: std::collections::HashSet<_> = live.iter().map(ItemKey::from).collect();
        for invocation in &mut self.invocations {
            if let Some(pending) = &invocation.pending {
                let path = pending.original.path();
                let candidates: Vec<_> = live
                    .iter()
                    .filter(|item| same_path(&item.original_path(), &path))
                    .filter(|item| !pending.before.contains(&ItemKey::from(*item)))
                    .collect();
                match candidates.as_slice() {
                    [item] => invocation.items.push(ItemKey::from(*item)),
                    [] if fs::symlink_metadata(&path).is_ok() => {}
                    _ => bail!(
                        "cannot reconcile an interrupted trash operation for {:?}; inspect the system trash and history.json before retrying",
                        path
                    ),
                }
                invocation.pending = None;
            }
            invocation.items.retain(|key| available.contains(key));
        }
        self.invocations
            .retain(|invocation| !invocation.items.is_empty());
        Ok(())
    }
}

pub fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    // Windows canonicalize returns verbatim paths (\\?\C:\...), whereas
    // Recycle Bin receipts return ordinary DOS/UNC paths.
    dunce::simplified(left) == dunce::simplified(right)
}

pub struct Store {
    pub dir: PathBuf,
    _lock: Option<File>,
    read_only: bool,
}

impl Store {
    pub fn default_dir() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("BIN_STATE_DIR") {
            if path.is_empty() {
                bail!("BIN_STATE_DIR must not be empty");
            }
            return Ok(std::path::absolute(PathBuf::from(path))?);
        }
        let dirs = directories::BaseDirs::new()
            .context("cannot determine the state directory; set BIN_STATE_DIR")?;
        Ok(dirs
            .state_dir()
            .unwrap_or(dirs.data_local_dir())
            .join("bin"))
    }

    pub fn open(dir: PathBuf, read_only: bool) -> Result<(Self, History)> {
        let lock = if read_only {
            None
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&dir)?;
            }
            #[cfg(not(unix))]
            fs::create_dir_all(&dir)?;
            let mut options = OpenOptions::new();
            options.create(true).truncate(false).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let lock = options.open(dir.join("history.lock"))?;
            lock.try_lock_exclusive()
                .context("another bin process is using this history; retry after it finishes")?;
            Some(lock)
        };
        let history = match fs::read(dir.join("history.json")) {
            Ok(bytes) => {
                let history: History = serde_json::from_slice(&bytes)
                    .context("cannot read history.json; refusing to change trash")?;
                if history.version != 1 {
                    bail!("unsupported history format version {}", history.version);
                }
                history
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => History::default(),
            Err(error) => return Err(error).context("cannot read history.json"),
        };
        Ok((
            Self {
                dir,
                _lock: lock,
                read_only,
            },
            history,
        ))
    }

    pub fn save(&self, history: &History) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(&mut file, history)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(self.dir.join("history.json"))
            .context("cannot atomically save history.json")?;
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_store_does_not_create_anything() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("absent");
        let (store, history) = Store::open(dir.clone(), true).unwrap();
        store.save(&history).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn concurrent_writers_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let (_store, _) = Store::open(dir.path().to_path_buf(), false).unwrap();
        assert!(Store::open(dir.path().to_path_buf(), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn binary_paths_round_trip_through_json() {
        use std::os::unix::ffi::OsStringExt;
        let value = OsString::from_vec(b"/tmp/invalid-\xff".to_vec());
        let encoded = serde_json::to_vec(&NativeString::new(&value)).unwrap();
        let decoded: NativeString = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.to_os_string(), value);
    }
}
