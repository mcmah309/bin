use crate::backend::TrashItem;
use eros::{Context, IntoAnyUnion, Result, bail};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    path::PathBuf,
};

const APPLICATION_ID: i64 = 0x4249_4e31;
const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = "
    CREATE TABLE invocations (id INTEGER PRIMARY KEY AUTOINCREMENT);
    CREATE TABLE items (
        invocation_id INTEGER NOT NULL REFERENCES invocations(id) ON DELETE CASCADE,
        native_id BLOB NOT NULL,
        original BLOB NOT NULL,
        deleted_at INTEGER NOT NULL,
        PRIMARY KEY (native_id, original, deleted_at)
    ) WITHOUT ROWID;
    CREATE INDEX items_by_invocation ON items(invocation_id);
    CREATE TABLE pending (
        invocation_id INTEGER PRIMARY KEY REFERENCES invocations(id) ON DELETE CASCADE,
        original BLOB NOT NULL
    );
    CREATE TABLE pending_before (
        invocation_id INTEGER NOT NULL REFERENCES pending(invocation_id) ON DELETE CASCADE,
        native_id BLOB NOT NULL,
        original BLOB NOT NULL,
        deleted_at INTEGER NOT NULL,
        PRIMARY KEY (invocation_id, native_id, original, deleted_at)
    ) WITHOUT ROWID;
    CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
";

// Deserialize native-unit arrays only for legacy JSON migration. SQLite uses
// raw byte BLOBs on Unix and little-endian UTF-16 BLOBs on Windows.
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

    pub fn path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(OsString::from_vec(self.units.clone()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            PathBuf::from(OsString::from_wide(&self.units))
        }
    }

    fn encoding() -> &'static str {
        if cfg!(windows) {
            "utf16le"
        } else {
            "unix-bytes"
        }
    }
}

impl rusqlite::types::ToSql for NativeString {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::ToSqlOutput;
        #[cfg(unix)]
        {
            Ok(ToSqlOutput::Borrowed(rusqlite::types::ValueRef::Blob(
                &self.units,
            )))
        }
        #[cfg(windows)]
        {
            let bytes: Vec<u8> = self
                .units
                .iter()
                .flat_map(|unit| unit.to_le_bytes())
                .collect();
            Ok(ToSqlOutput::from(bytes))
        }
    }
}

impl rusqlite::types::FromSql for NativeString {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let bytes = value.as_blob()?;
        #[cfg(unix)]
        {
            Ok(Self {
                units: bytes.to_vec(),
            })
        }
        #[cfg(windows)]
        {
            if !bytes.len().is_multiple_of(2) {
                return Err(rusqlite::types::FromSqlError::Other(Box::new(
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid UTF-16 path BLOB",
                    ),
                )));
            }
            Ok(Self {
                units: bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair))
                    .collect(),
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemKey {
    id: NativeString,
    original: NativeString,
    time_deleted: i64,
}

impl ItemKey {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            original: row.get(1)?,
            time_deleted: row.get(2)?,
        })
    }
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

#[derive(Deserialize)]
struct LegacyPending {
    original: NativeString,
    before: Vec<ItemKey>,
}
#[derive(Deserialize)]
struct LegacyInvocation {
    items: Vec<ItemKey>,
    pending: Option<LegacyPending>,
}
#[derive(Deserialize)]
struct LegacyHistory {
    version: u32,
    invocations: Vec<LegacyInvocation>,
}

pub fn same_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    dunce::simplified(left) == dunce::simplified(right)
}

pub struct Store {
    // Close the database before releasing the lock, including on error paths.
    connection: Connection,
    _lock: Option<File>,
    pub dir: PathBuf,
    read_only: bool,
    recovered: HashMap<i64, Vec<ItemKey>>,
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

    pub fn open(dir: PathBuf, read_only: bool) -> Result<Self> {
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
            let lock = private_file(&dir.join("history.lock"))?;
            lock.try_lock_exclusive()
                .context("another bin process is using this history; retry after it finishes")?;
            Some(lock)
        };
        let path = dir.join("history.sqlite3");
        let in_memory = read_only && !path.try_exists()?;
        let mut connection = if in_memory {
            Connection::open_in_memory()?
        } else if read_only {
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?
        } else {
            drop(private_file(&path)?);
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)?
        };
        connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA temp_store = MEMORY;")?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("cannot read history.sqlite3; refusing to change trash")?;
        if version == 0 && (!read_only || in_memory) {
            initialize(&mut connection, &dir)?;
        } else if version != SCHEMA_VERSION {
            bail!("unsupported SQLite history version {version}; expected {SCHEMA_VERSION}");
        }
        let application_id: i64 =
            connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        if application_id != APPLICATION_ID {
            bail!("history.sqlite3 is not a bin history database");
        }
        let encoding: String = connection.query_row(
            "SELECT value FROM metadata WHERE key = 'path_encoding'",
            [],
            |row| row.get(0),
        )?;
        if encoding != NativeString::encoding() {
            bail!("history.sqlite3 uses a different platform's filename encoding");
        }
        if !read_only {
            // Rollback journaling allows existing databases to be opened strictly
            // read-only on dry runs without creating WAL/SHM files.
            connection.pragma_update(None, "journal_mode", "DELETE")?;
            connection.pragma_update(None, "synchronous", "EXTRA")?;
        }
        Ok(Self {
            connection,
            _lock: lock,
            dir,
            read_only,
            recovered: HashMap::new(),
        })
    }

    fn writable(&self) -> Result<()> {
        if self.read_only {
            bail!("cannot modify history during a dry run");
        }
        Ok(())
    }

    /// Durably record intent before touching the filesystem. Each transaction
    /// changes only the current invocation and its pending receipt.
    pub fn begin_trash(
        &mut self,
        invocation: Option<i64>,
        path: &std::path::Path,
        before: &[ItemKey],
    ) -> Result<i64> {
        self.writable()?;
        let transaction = self.connection.transaction()?;
        let invocation = match invocation {
            Some(id) => id,
            None => {
                transaction.execute("INSERT INTO invocations DEFAULT VALUES", [])?;
                transaction.last_insert_rowid()
            }
        };
        insert_pending(
            &transaction,
            invocation,
            &NativeString::new(path.as_os_str()),
            before,
        )?;
        transaction.commit()?;
        Ok(invocation)
    }

    pub fn finish_trash(&mut self, invocation: i64, item: Option<&ItemKey>) -> Result<()> {
        self.writable()?;
        let transaction = self.connection.transaction()?;
        if let Some(key) = item {
            insert_item(&transaction, invocation, key)?;
        }
        transaction.execute("DELETE FROM pending WHERE invocation_id = ?1", [invocation])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn has_pending(&self) -> Result<bool> {
        Ok(self
            .connection
            .query_row("SELECT EXISTS(SELECT 1 FROM pending)", [], |row| row.get(0))?)
    }

    pub fn finish_invocation(&self, invocation: i64) -> Result<()> {
        self.writable()?;
        delete_empty_invocation(&self.connection, invocation)
    }

    /// Only pending operations are read on startup, not the entire history.
    /// A dry run keeps recovered receipts in a small in-memory overlay.
    pub fn recover_pending(&mut self, live: &[TrashItem]) -> Result<()> {
        let pending: Vec<(i64, NativeString)> = self
            .connection
            .prepare_cached("SELECT invocation_id, original FROM pending")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (invocation, original) in pending {
            let before: HashSet<ItemKey> = self.connection.prepare_cached(
                "SELECT native_id, original, deleted_at FROM pending_before WHERE invocation_id = ?1"
            )?.query_map([invocation], ItemKey::from_row)?.collect::<rusqlite::Result<_>>()?;
            let path = original.path();
            let candidates: Vec<_> = live
                .iter()
                .filter(|item| same_path(&item.original_path(), &path))
                .map(ItemKey::from)
                .filter(|key| !before.contains(key))
                .collect();
            let recovered = match candidates.as_slice() {
                [key] => Some(key.clone()),
                [] => {
                    match fs::symlink_metadata(&path) {
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                            ) =>
                        {
                            eprintln!(
                                "bin: {} pending receipt for {path:?}: the source and restorable trash item are both missing",
                                if self.read_only {
                                    "would discard"
                                } else {
                                    "discarding"
                                }
                            );
                        }
                        Err(error) => {
                            return Err(error).any_union().with_context(|| {
                                format!("cannot inspect interrupted trash source {path:?}")
                            });
                        }
                    }
                    None
                }
                _ => bail!(
                    "cannot reconcile an interrupted trash operation for {path:?}; inspect the system trash and history.sqlite3 before retrying"
                ),
            };
            if self.read_only {
                if let Some(key) = recovered {
                    self.recovered.entry(invocation).or_default().push(key);
                }
            } else {
                self.finish_trash(invocation, recovered.as_ref())?;
                self.finish_invocation(invocation)?;
            }
        }
        Ok(())
    }

    /// Walk the invocation index backwards and load only as many groups as pop
    /// needs. Stale receipts are pruned lazily from the groups visited here.
    pub fn latest_keys(&mut self, count: usize, live: &[TrashItem]) -> Result<HashSet<ItemKey>> {
        let available: HashSet<_> = live.iter().map(ItemKey::from).collect();
        let mut selected = HashSet::new();
        let mut found = 0;
        let mut cursor = i64::MAX;
        let mut stale = Vec::new();
        let mut visited = Vec::new();
        while found < count {
            let ids: Vec<i64> = self
                .connection
                .prepare_cached(
                    "SELECT id FROM invocations WHERE id <= ?1 ORDER BY id DESC LIMIT 128",
                )?
                .query_map([cursor], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            if ids.is_empty() {
                break;
            }
            for id in ids {
                cursor = id - 1;
                visited.push(id);
                let mut keys: Vec<_> = self.connection.prepare_cached(
                    "SELECT native_id, original, deleted_at FROM items WHERE invocation_id = ?1"
                )?.query_map([id], ItemKey::from_row)?.collect::<rusqlite::Result<_>>()?;
                if let Some(recovered) = self.recovered.get(&id) {
                    keys.extend(recovered.iter().cloned());
                }
                let previous = selected.len();
                for key in keys {
                    if available.contains(&key) {
                        selected.insert(key);
                    } else {
                        stale.push(key);
                    }
                }
                if selected.len() > previous {
                    found += 1;
                }
                if found == count {
                    break;
                }
            }
        }
        if !self.read_only && (!stale.is_empty() || !visited.is_empty()) {
            let transaction = self.connection.transaction()?;
            for key in stale {
                delete_item(&transaction, &key)?;
            }
            for id in visited {
                delete_empty_invocation(&transaction, id)?;
            }
            transaction.commit()?;
        }
        if found < count {
            bail!(
                "requested {count} invocation(s), but only {found} restorable invocation(s) remain"
            );
        }
        Ok(selected)
    }

    pub fn remove_item(&mut self, key: &ItemKey) -> Result<()> {
        self.writable()?;
        let transaction = self.connection.transaction()?;
        if let Some(id) = delete_item(&transaction, key)? {
            delete_empty_invocation(&transaction, id)?;
        }
        transaction.commit()?;
        Ok(())
    }
}

fn private_file(path: &std::path::Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn insert_item(connection: &Connection, invocation: i64, key: &ItemKey) -> Result<()> {
    connection.prepare_cached("INSERT INTO items(invocation_id, native_id, original, deleted_at) VALUES (?1, ?2, ?3, ?4)")?
        .execute(params![invocation, key.id, key.original, key.time_deleted])?;
    Ok(())
}

fn insert_pending(
    connection: &Connection,
    invocation: i64,
    original: &NativeString,
    before: &[ItemKey],
) -> Result<()> {
    connection.execute(
        "INSERT INTO pending(invocation_id, original) VALUES (?1, ?2)",
        params![invocation, original],
    )?;
    let mut statement = connection.prepare_cached(
        "INSERT INTO pending_before(invocation_id, native_id, original, deleted_at) VALUES (?1, ?2, ?3, ?4)"
    )?;
    for key in before {
        statement.execute(params![invocation, key.id, key.original, key.time_deleted])?;
    }
    Ok(())
}

fn delete_item(connection: &Connection, key: &ItemKey) -> Result<Option<i64>> {
    Ok(connection.prepare_cached("DELETE FROM items WHERE native_id = ?1 AND original = ?2 AND deleted_at = ?3 RETURNING invocation_id")?
        .query_row(params![key.id, key.original, key.time_deleted], |row| row.get(0)).optional()?)
}

fn delete_empty_invocation(connection: &Connection, invocation: i64) -> Result<()> {
    connection.execute(
        "DELETE FROM invocations WHERE id = ?1
        AND NOT EXISTS(SELECT 1 FROM items WHERE invocation_id = ?1)
        AND NOT EXISTS(SELECT 1 FROM pending WHERE invocation_id = ?1)",
        [invocation],
    )?;
    Ok(())
}

fn initialize(connection: &mut Connection, dir: &std::path::Path) -> Result<()> {
    let legacy = match fs::read(dir.join("history.json")) {
        Ok(bytes) => {
            let history: LegacyHistory = serde_json::from_slice(&bytes)
                .context("cannot migrate history.json; refusing to change trash")?;
            if history.version != 1 {
                bail!(
                    "unsupported JSON history format version {}",
                    history.version
                );
            }
            Some(history)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .any_union()
                .context("cannot read legacy history.json");
        }
    };
    let transaction = connection.transaction()?;
    transaction.execute_batch(SCHEMA)?;
    transaction.execute(
        "INSERT INTO metadata(key, value) VALUES ('path_encoding', ?1)",
        [NativeString::encoding()],
    )?;
    if let Some(history) = legacy {
        for invocation in history.invocations {
            if invocation.items.is_empty() && invocation.pending.is_none() {
                continue;
            }
            transaction.execute("INSERT INTO invocations DEFAULT VALUES", [])?;
            let id = transaction.last_insert_rowid();
            for key in invocation.items {
                insert_item(&transaction, id, &key)?;
            }
            if let Some(pending) = invocation.pending {
                insert_pending(&transaction, id, &pending.original, &pending.before)?;
            }
        }
    }
    transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(root: &std::path::Path, name: &str) -> TrashItem {
        TrashItem {
            id: root.join("trash").join(name).into_os_string(),
            name: name.into(),
            original_parent: root.join("work"),
            time_deleted: 1_700_000_000,
        }
    }

    fn record(store: &mut Store, item: &TrashItem) -> i64 {
        let id = store.begin_trash(None, &item.original_path(), &[]).unwrap();
        store.finish_trash(id, Some(&ItemKey::from(item))).unwrap();
        id
    }

    fn snapshot(dir: &std::path::Path) -> Vec<(OsString, Vec<u8>)> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name();
                let contents = if name == "history.lock" {
                    // Windows locks block reads of this empty coordination file.
                    assert_eq!(entry.metadata().unwrap().len(), 0);
                    Vec::new()
                } else {
                    fs::read(entry.path()).unwrap()
                };
                (name, contents)
            })
            .collect();
        entries.sort();
        entries
    }

    #[test]
    fn read_only_store_does_not_create_anything() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("absent");
        let mut store = Store::open(dir.clone(), true).unwrap();
        assert!(store.latest_keys(1, &[]).is_err());
        assert!(
            store
                .begin_trash(None, &root.path().join("file"), &[])
                .is_err()
        );
        drop(store);
        assert!(!dir.exists());
    }

    #[test]
    fn concurrent_writers_are_excluded_but_readers_can_preview() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        let mut writer = Store::open(dir.clone(), false).unwrap();
        let a = item(root.path(), "a");
        record(&mut writer, &a);
        assert!(Store::open(dir.clone(), false).is_err());
        let before = snapshot(&dir);
        let mut reader = Store::open(dir.clone(), true).unwrap();
        assert_eq!(reader.latest_keys(1, &[a]).unwrap().len(), 1);
        drop(reader);
        assert_eq!(snapshot(&dir), before);
    }

    #[test]
    fn writes_touch_only_current_rows_and_pop_reads_only_recent_groups() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path().join("state"), false).unwrap();
        {
            let transaction = store.connection.transaction().unwrap();
            for number in 0..10_000 {
                transaction
                    .execute("INSERT INTO invocations DEFAULT VALUES", [])
                    .unwrap();
                let id = transaction.last_insert_rowid();
                insert_item(
                    &transaction,
                    id,
                    &ItemKey::from(&item(root.path(), &number.to_string())),
                )
                .unwrap();
            }
            // A previous row deliberately cannot decode as a native-path BLOB.
            // Recent pop must not deserialize unrelated historical invocations.
            transaction
                .execute(
                    "UPDATE items SET original = 'invalid historical path' WHERE invocation_id = 1",
                    [],
                )
                .unwrap();
            transaction.commit().unwrap();
        }
        let before = store.connection.total_changes();
        let a = item(root.path(), "recent");
        record(&mut store, &a);
        assert_eq!(store.connection.total_changes() - before, 4);
        assert_eq!(
            store.latest_keys(1, std::slice::from_ref(&a)).unwrap(),
            HashSet::from([ItemKey::from(&a)])
        );
        store.remove_item(&ItemKey::from(&a)).unwrap();
        let old_rows: i64 = store
            .connection
            .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(old_rows, 10_000);
    }

    #[test]
    fn failed_receipt_transaction_preserves_pending_intent() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path().join("state"), false).unwrap();
        let a = item(root.path(), "a");
        let b = item(root.path(), "b");
        record(&mut store, &a);
        let id = store.begin_trash(None, &b.original_path(), &[]).unwrap();
        // Simulate an invalid duplicate receipt: neither its insertion nor the
        // pending intent's removal may be partially committed.
        assert!(store.finish_trash(id, Some(&ItemKey::from(&a))).is_err());
        assert!(store.has_pending().unwrap());
        store.finish_trash(id, Some(&ItemKey::from(&b))).unwrap();
        assert!(!store.has_pending().unwrap());
        assert_eq!(store.latest_keys(2, &[a, b]).unwrap().len(), 2);
    }

    #[test]
    fn pending_recovery_preview_leaves_database_and_directory_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        let a = item(root.path(), "a");
        {
            let mut store = Store::open(dir.clone(), false).unwrap();
            store.begin_trash(None, &a.original_path(), &[]).unwrap();
        }
        let before = snapshot(&dir);
        {
            let mut store = Store::open(dir.clone(), true).unwrap();
            store.recover_pending(std::slice::from_ref(&a)).unwrap();
            assert_eq!(
                store
                    .latest_keys(1, std::slice::from_ref(&a))
                    .unwrap()
                    .len(),
                1
            );
            assert!(store.has_pending().unwrap());
        }
        assert_eq!(snapshot(&dir), before);
        let mut store = Store::open(dir, false).unwrap();
        store.recover_pending(std::slice::from_ref(&a)).unwrap();
        assert!(!store.has_pending().unwrap());
        assert_eq!(store.latest_keys(1, &[a]).unwrap().len(), 1);
    }

    #[test]
    fn recovery_excludes_prior_ids_and_preserves_ambiguous_pending_intent() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        let mut store = Store::open(dir.clone(), false).unwrap();
        let prior = item(root.path(), "same-name");
        let mut recent = prior.clone();
        recent.id.push("-new-id");
        store
            .begin_trash(None, &prior.original_path(), &[ItemKey::from(&prior)])
            .unwrap();
        store
            .recover_pending(&[prior.clone(), recent.clone()])
            .unwrap();
        assert_eq!(
            store
                .latest_keys(1, &[prior.clone(), recent.clone()])
                .unwrap(),
            HashSet::from([ItemKey::from(&recent)])
        );
        let remaining_before: i64 = store
            .connection
            .query_row("SELECT count(*) FROM pending_before", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining_before, 0);
        store
            .begin_trash(None, &prior.original_path(), &[])
            .unwrap();
        let before = snapshot(&dir);
        assert!(store.recover_pending(&[prior, recent]).is_err());
        assert!(store.has_pending().unwrap());
        assert_eq!(snapshot(&dir), before);
    }

    #[test]
    fn stale_invocations_are_skipped_and_pruned_without_losing_live_groups() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path().join("state"), false).unwrap();
        let live = item(root.path(), "live");
        record(&mut store, &live);
        // Cross the pagination boundary with receipts purged by another app.
        for number in 0..130 {
            record(&mut store, &item(root.path(), &number.to_string()));
        }
        assert_eq!(store.latest_keys(1, &[live]).unwrap().len(), 1);
        let invocations: i64 = store
            .connection
            .query_row("SELECT count(*) FROM invocations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(invocations, 1);
    }

    #[test]
    fn a_reused_native_id_with_different_path_or_timestamp_is_not_claimed() {
        for change_path in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut store = Store::open(root.path().join("state"), false).unwrap();
            let original = item(root.path(), "original");
            let survivor = item(root.path(), "survivor");
            record(&mut store, &original);
            record(&mut store, &survivor);
            let mut replacement = original.clone();
            if change_path {
                replacement.name = "untracked-new-path".into();
            } else {
                replacement.time_deleted += 1;
            }
            let live = [replacement, survivor.clone()];
            assert!(store.latest_keys(2, &live).is_err());
            assert_eq!(
                store.latest_keys(1, &live).unwrap(),
                HashSet::from([ItemKey::from(&survivor)])
            );
            store.remove_item(&ItemKey::from(&survivor)).unwrap();
            assert!(store.latest_keys(1, &live).is_err());
        }
    }

    #[test]
    fn legacy_json_migrates_once_with_grouping_and_pending_receipts_intact() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        fs::create_dir(&dir).unwrap();
        let a = item(root.path(), "a");
        let b = item(root.path(), "b");
        let c = item(root.path(), "c");
        let legacy = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "invocations": [
                {"items": [ItemKey::from(&a), ItemKey::from(&b)], "pending": null},
                {"items": [], "pending": {"original": NativeString::new(c.original_path().as_os_str()), "before": []}}
            ]
        })).unwrap();
        fs::write(dir.join("history.json"), &legacy).unwrap();
        let live = [a, b, c];
        {
            let mut store = Store::open(dir.clone(), true).unwrap();
            store.recover_pending(&live).unwrap();
            assert_eq!(
                store.latest_keys(1, &live).unwrap(),
                HashSet::from([ItemKey::from(&live[2])])
            );
            assert!(!dir.join("history.sqlite3").exists());
        }
        {
            let mut store = Store::open(dir.clone(), false).unwrap();
            store.recover_pending(&live).unwrap();
            assert_eq!(store.latest_keys(2, &live).unwrap().len(), 3);
            assert_eq!(fs::read(dir.join("history.json")).unwrap(), legacy);
            for entry in &live {
                store.remove_item(&ItemKey::from(entry)).unwrap();
            }
        }
        // Once the database is initialized, the retained backup is never read or
        // reimported, even if every migrated receipt has since been consumed.
        fs::write(dir.join("history.json"), b"invalid backup").unwrap();
        let mut store = Store::open(dir.clone(), false).unwrap();
        assert!(store.latest_keys(1, &live).is_err());
        assert!(
            fs::read(dir.join("history.sqlite3"))
                .unwrap()
                .starts_with(b"SQLite format 3\0")
        );
    }

    #[test]
    fn failed_migration_rolls_back_schema_and_can_be_retried() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        fs::create_dir(&dir).unwrap();
        let a = item(root.path(), "a");
        let key = ItemKey::from(&a);
        let history = serde_json::json!({ "version": 1, "invocations": [{"items": [key, key], "pending": null}] });
        let legacy = serde_json::to_vec(&history).unwrap();
        fs::write(dir.join("history.json"), &legacy).unwrap();
        assert!(Store::open(dir.clone(), false).is_err());
        assert_eq!(fs::read(dir.join("history.json")).unwrap(), legacy);
        {
            let connection = Connection::open(dir.join("history.sqlite3")).unwrap();
            let version: i64 = connection
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap();
            assert_eq!(version, 0);
            let tables: i64 = connection
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(tables, 0);
        }
        let repaired =
            serde_json::json!({ "version": 1, "invocations": [{"items": [key], "pending": null}] });
        fs::write(
            dir.join("history.json"),
            serde_json::to_vec(&repaired).unwrap(),
        )
        .unwrap();
        let mut store = Store::open(dir, false).unwrap();
        assert_eq!(store.latest_keys(1, &[a]).unwrap().len(), 1);
    }

    #[test]
    fn unknown_database_version_and_corrupt_history_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        {
            let store = Store::open(dir.clone(), false).unwrap();
            store
                .connection
                .pragma_update(None, "user_version", 999)
                .unwrap();
        }
        let before = snapshot(&dir);
        assert!(Store::open(dir.clone(), true).is_err());
        assert!(Store::open(dir.clone(), false).is_err());
        assert_eq!(snapshot(&dir), before);
        fs::write(dir.join("history.sqlite3"), b"corrupt").unwrap();
        assert!(Store::open(dir, false).is_err());
    }

    #[test]
    fn native_filename_blobs_round_trip_without_unicode_conversion() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path().join("state"), false).unwrap();
        let mut a = item(root.path(), "a");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            a.name = OsString::from_vec(b"binary-\xff".to_vec());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            a.name = OsString::from_wide(&[0x61, 0xd800]);
        }
        record(&mut store, &a);
        let key = store
            .latest_keys(1, std::slice::from_ref(&a))
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(key.original.path(), a.original_path());
        let kind: String = store
            .connection
            .query_row("SELECT typeof(original) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kind, "blob");
    }
}
