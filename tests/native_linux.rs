#![cfg(target_os = "linux")]

use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct Sandbox {
    _root: tempfile::TempDir,
    _other_filesystem: Option<tempfile::TempDir>,
    work: PathBuf,
    state: PathBuf,
    data: PathBuf,
    prefix: String,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        fs::create_dir(&work).unwrap();
        Self {
            state: root.path().join("state"),
            data: root.path().join("data"),
            prefix: format!(
                "bin-native-{}",
                root.path().file_name().unwrap().to_string_lossy()
            ),
            work,
            _root: root,
            _other_filesystem: None,
        }
    }

    fn file(&self, suffix: &str) -> PathBuf {
        let path = self.work.join(format!("{}-{suffix}", self.prefix));
        fs::write(&path, suffix).unwrap();
        path
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bin"));
        command
            .current_dir(&self.work)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CONFIG_HOME", self._root.path().join("config"))
            .env("BIN_STATE_DIR", &self.state)
            .env("TZ", "UTC");
        command
    }

    fn run(&self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn ok(&self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn prompt(&self, args: &[&OsStr], input: &[u8]) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn trash_files(&self) -> Vec<PathBuf> {
        let path = self.data.join("Trash/files");
        if !path.exists() {
            return vec![];
        }
        fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    fn trashed(&self, original: &Path) -> PathBuf {
        self.data
            .join("Trash/files")
            .join(original.file_name().unwrap())
    }

    fn info(&self, original: &Path) -> PathBuf {
        let mut name = original.file_name().unwrap().to_os_string();
        name.push(".trashinfo");
        self.data.join("Trash/info").join(name)
    }

    fn receipt_count(&self) -> i64 {
        let database = rusqlite::Connection::open(self.state.join("history.sqlite3")).unwrap();
        database
            .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
            .unwrap()
    }

    fn with_home_on_another_filesystem() -> Self {
        use std::os::unix::fs::MetadataExt;
        let mut sandbox = Self::new();
        let other = tempfile::Builder::new()
            .prefix("bin-home-trash-")
            .tempdir_in("/dev/shm")
            .unwrap();
        assert_ne!(
            fs::metadata(&sandbox.work).unwrap().dev(),
            fs::metadata(other.path()).unwrap().dev(),
            "this test requires distinct filesystems"
        );
        sandbox.data = other.path().join("data");
        sandbox._other_filesystem = Some(other);
        sandbox
    }

    fn config(&self, text: &str) -> PathBuf {
        let path = self._root.path().join("config/bin/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    fn upstream(&self, action: &str, path: &Path) {
        // A subprocess isolates the native library's environment without
        // mutating process-global environment variables in parallel tests.
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "upstream_worker", "--nocapture"])
            .env("XDG_DATA_HOME", &self.data)
            .env("TZ", "UTC")
            .env("BIN_INTEROP_ACTION", action)
            .env("BIN_INTEROP_PATH", path)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "upstream interoperability failed: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn upstream_worker() {
    let Ok(action) = std::env::var("BIN_INTEROP_ACTION") else {
        return;
    };
    let path = PathBuf::from(std::env::var_os("BIN_INTEROP_PATH").unwrap());
    if action == "trash" {
        trash::delete(path).unwrap();
        return;
    }
    let items: Vec<_> = trash::os_limited::list()
        .unwrap()
        .into_iter()
        .filter(|item| item.original_path() == path)
        .collect();
    assert_eq!(items.len(), 1);
    trash::os_limited::metadata(&items[0]).unwrap();
    if action == "restore" {
        trash::os_limited::restore_all(items).unwrap();
    } else {
        assert_eq!(action, "inspect");
    }
}

#[test]
fn upstream_trash_and_existing_sqlite_history_remain_interoperable() {
    use std::os::unix::ffi::OsStrExt;
    let f = Sandbox::new();
    let path = f.file("interop");
    f.ok([path.as_os_str()]);
    f.upstream("inspect", &path);
    f.upstream("restore", &path);
    assert_eq!(fs::read_to_string(&path).unwrap(), "interop");
    assert!(!f.run(["-p"]).status.success()); // External restore reconciles history.
    f.upstream("trash", &path);
    assert!(!f.run(["-p"]).status.success()); // No invented invocation for another utility.
    {
        // Simulate history recorded by the old native backend: unchanged IDs,
        // original-path BLOBs, timestamps, and invocation grouping must work.
        let text = fs::read_to_string(f.info(&path)).unwrap();
        let date = text
            .lines()
            .find_map(|line| line.strip_prefix("DeletionDate="))
            .unwrap();
        let deleted_at = chrono::NaiveDateTime::parse_from_str(date, "%Y-%m-%dT%H:%M:%S")
            .unwrap()
            .and_utc()
            .timestamp();
        let database = rusqlite::Connection::open(f.state.join("history.sqlite3")).unwrap();
        database
            .execute("INSERT INTO invocations DEFAULT VALUES", [])
            .unwrap();
        let id = database.last_insert_rowid();
        database.execute("INSERT INTO items(invocation_id, native_id, original, deleted_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, f.info(&path).as_os_str().as_bytes(), path.as_os_str().as_bytes(), deleted_at]).unwrap();
    }
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(&path).unwrap(), "interop");
    f.upstream("trash", &path);
    let glob = format!("{}-*", f.prefix);
    f.ok(["--empty", "--glob", &glob]);
    assert!(!path.exists());
    assert!(f.trash_files().is_empty());
}

#[test]
fn orphaned_payloads_are_never_overwritten_or_claimed_by_new_receipts() {
    let f = Sandbox::new();
    let seed = f.file("seed");
    f.ok([seed.as_os_str()]);
    f.ok(["-p"]);
    let path = f.file("collision");
    fs::write(f.trashed(&path), "untracked orphan").unwrap();
    f.ok([path.as_os_str()]);
    assert_eq!(f.trash_files().len(), 2);
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(path.clone()).unwrap(), "collision");
    assert_eq!(
        fs::read_to_string(f.trashed(&path)).unwrap(),
        "untracked orphan"
    );
    assert!(!f.info(&path).exists());
}

#[test]
fn malformed_metadata_is_reported_without_blocking_other_files_or_erasing_data() {
    let f = Sandbox::new();
    let seed = f.file("seed");
    f.ok([seed.as_os_str()]);
    f.ok(["-p"]);
    let broken = f.file("broken");
    fs::rename(&broken, f.trashed(&broken)).unwrap();
    let invalid = "[Trash Info]\nPath=%QQ\nnot a key/value\n";
    fs::write(f.info(&broken), invalid).unwrap();
    let valid = f.file("valid");
    let output = f.ok([valid.as_os_str()]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("skipping malformed trash receipt"));
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(valid).unwrap(), "valid");
    assert_eq!(fs::read_to_string(f.trashed(&broken)).unwrap(), "broken");
    assert_eq!(fs::read_to_string(f.info(&broken)).unwrap(), invalid);
}

#[test]
fn long_names_round_trip_and_new_trash_metadata_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let f = Sandbox::new();
    let path = f.work.join("x".repeat(255));
    fs::write(&path, "long name").unwrap();
    f.ok([path.as_os_str()]);
    assert_eq!(f.trash_files().len(), 1);
    for dir in [
        f.data.join("Trash"),
        f.data.join("Trash/info"),
        f.data.join("Trash/files"),
    ] {
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let info = fs::read_dir(f.data.join("Trash/info"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::metadata(info).unwrap().permissions().mode() & 0o777,
        0o600
    );
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(path).unwrap(), "long name");
}

#[test]
fn home_is_the_default_across_filesystems_and_versions_restore_without_overwriting() {
    let f = Sandbox::with_home_on_another_filesystem();
    let path = f.file("versioned");
    let preview = f.ok([OsStr::new("--dry-run"), path.as_os_str()]);
    assert!(preview.status.success());
    assert!(path.exists());
    assert!(!f.data.exists());
    assert!(!f.state.exists());
    f.ok([path.as_os_str()]);
    assert_eq!(fs::read_to_string(f.trashed(&path)).unwrap(), "versioned");
    fs::write(&path, "second version").unwrap();
    f.ok([path.as_os_str()]);
    assert_eq!(f.trash_files().len(), 2);
    assert!(!f.run(["-p", "2"]).status.success());
    assert!(!path.exists());
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(&path).unwrap(), "second version");
    assert!(!f.run(["-fp"]).status.success());
    let aside = f.work.join("restored-version");
    fs::rename(&path, &aside).unwrap();
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(path).unwrap(), "versioned");
    assert_eq!(fs::read_to_string(aside).unwrap(), "second version");
    assert!(f.trash_files().is_empty());
}

#[test]
fn cross_filesystem_trees_and_top_level_symlinks_preserve_contents_and_metadata() {
    use std::{
        os::unix::{
            ffi::OsStringExt,
            fs::{PermissionsExt, symlink},
        },
        time::{Duration, UNIX_EPOCH},
    };
    let f = Sandbox::with_home_on_another_filesystem();
    let outside = f.file("outside");
    let outside_dir = f.work.join("outside-directory");
    fs::create_dir(&outside_dir).unwrap();
    let tree = f.work.join(format!("{}-tree", f.prefix));
    fs::create_dir(&tree).unwrap();
    let child = tree.join("child");
    fs::write(&child, b"binary\0content").unwrap();
    let modified = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    fs::File::open(&child)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(&tree, fs::Permissions::from_mode(0o750)).unwrap();
    symlink(&outside, tree.join("external-link")).unwrap();
    let directory_link = f.work.join(format!("{}-directory-link", f.prefix));
    symlink(&outside_dir, &directory_link).unwrap();
    let binary_link = f.work.join(OsString::from_vec(
        [f.prefix.as_bytes(), b"-binary-\xff"].concat(),
    ));
    symlink("missing-target", &binary_link).unwrap();
    f.ok([
        OsStr::new("-r"),
        tree.as_os_str(),
        directory_link.as_os_str(),
        binary_link.as_os_str(),
    ]);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
    assert!(outside_dir.exists());
    assert_eq!(
        fs::read_link(f.trashed(&directory_link)).unwrap(),
        outside_dir
    );
    assert_eq!(
        fs::read_link(f.trashed(&binary_link)).unwrap(),
        Path::new("missing-target")
    );
    f.ok(["-p"]);
    assert_eq!(fs::read(&child).unwrap(), b"binary\0content");
    assert_eq!(fs::metadata(&child).unwrap().modified().unwrap(), modified);
    assert_eq!(
        fs::metadata(&child).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        fs::metadata(&tree).unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert_eq!(fs::read_link(tree.join("external-link")).unwrap(), outside);
    assert_eq!(fs::read_link(directory_link).unwrap(), outside_dir);
    assert_eq!(
        fs::read_link(binary_link).unwrap(),
        Path::new("missing-target")
    );
}

#[test]
fn failed_cross_filesystem_copy_keeps_source_and_retryable_restore_keeps_trash() {
    use std::os::unix::net::UnixListener;
    let f = Sandbox::with_home_on_another_filesystem();
    let tree = f.work.join(format!("{}-tree", f.prefix));
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("data"), "keep this data").unwrap();
    let socket = tree.join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    assert!(!f.run([OsStr::new("-r"), tree.as_os_str()]).status.success());
    assert_eq!(
        fs::read_to_string(tree.join("data")).unwrap(),
        "keep this data"
    );
    assert!(socket.exists());
    assert!(f.trash_files().is_empty());
    assert_eq!(f.receipt_count(), 0);
    drop(listener);
    fs::remove_file(socket).unwrap();
    f.ok([OsStr::new("-r"), tree.as_os_str()]);
    // An unsupported entry added to a trashed directory must not leave a partial
    // destination or consume history when restoration fails partway through.
    let bad = f.trashed(&tree).join("socket");
    let listener = UnixListener::bind(&bad).unwrap();
    assert!(!f.run(["-p"]).status.success());
    assert!(!tree.exists());
    assert_eq!(
        fs::read_to_string(f.trashed(&tree).join("data")).unwrap(),
        "keep this data"
    );
    assert_eq!(f.receipt_count(), 1);
    drop(listener);
    fs::remove_file(bad).unwrap();
    f.ok(["-p"]);
    assert_eq!(
        fs::read_to_string(tree.join("data")).unwrap(),
        "keep this data"
    );
}

#[test]
fn invalid_or_explicitly_missing_config_fails_before_trashing_or_creating_state() {
    let f = Sandbox::new();
    let path = f.file("keep");
    for text in [
        "trash-location = 'unknown'",
        "trash_location = 'home'",
        "not valid TOML",
    ] {
        f.config(text);
        assert!(!f.run([path.as_os_str()]).status.success());
        assert!(path.exists());
        assert!(!f.state.exists());
        assert!(!f.data.exists());
    }
    let missing = f.work.join("missing-config");
    assert!(
        !f.run([
            OsStr::new("--config"),
            missing.as_os_str(),
            path.as_os_str()
        ])
        .status
        .success()
    );
    assert!(path.exists());
    assert!(!f.state.exists());
}

#[test]
fn unusable_home_trash_fails_without_falling_back_to_the_source_mount() {
    let f = Sandbox::with_home_on_another_filesystem();
    fs::write(&f.data, "this blocks the home trash directory").unwrap();
    let path = f.file("keep-at-source");
    assert!(!f.run([path.as_os_str()]).status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), "keep-at-source");
    assert_eq!(
        fs::read_to_string(&f.data).unwrap(),
        "this blocks the home trash directory"
    );
    assert_eq!(f.receipt_count(), 0);
}

#[test]
fn per_filesystem_config_and_explicit_override_only_affect_new_trash_locations() {
    use std::os::unix::fs::MetadataExt;
    let f = Sandbox::new();
    let source = tempfile::Builder::new()
        .prefix("bin-volume-policy-")
        .tempdir_in("/dev/shm")
        .unwrap();
    assert_ne!(
        fs::metadata(source.path()).unwrap().dev(),
        fs::metadata(&f.work).unwrap().dev()
    );
    f.config("trash-location = 'per-filesystem'");
    let file = source.path().join(format!("{}-volume", f.prefix));
    fs::write(&file, "volume").unwrap();
    f.ok([file.as_os_str()]);
    assert!(!f.trashed(&file).exists());
    let database = rusqlite::Connection::open(f.state.join("history.sqlite3")).unwrap();
    let native: Vec<u8> = database
        .query_row("SELECT native_id FROM items", [], |row| row.get(0))
        .unwrap();
    use std::os::unix::ffi::OsStringExt;
    let info = PathBuf::from(OsString::from_vec(native));
    assert!(info.starts_with("/dev/shm"));
    let receipt = fs::read_to_string(&info).unwrap();
    assert!(
        !receipt
            .lines()
            .find_map(|line| line.strip_prefix("Path="))
            .unwrap()
            .starts_with('/')
    );
    drop(database);
    // Switch policy before undo; existing receipts retain their native locations.
    let home_config = f.work.join("home.toml");
    fs::write(&home_config, "trash-location = 'home'").unwrap();
    f.ok([
        OsStr::new("--config"),
        home_config.as_os_str(),
        OsStr::new("-p"),
    ]);
    assert_eq!(fs::read_to_string(&file).unwrap(), "volume");
    f.ok([
        OsStr::new("--config"),
        home_config.as_os_str(),
        file.as_os_str(),
    ]);
    assert_eq!(fs::read_to_string(f.trashed(&file)).unwrap(), "volume");
    // The default config is still per-filesystem; pop works for home entries too.
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(file).unwrap(), "volume");
}

#[test]
fn pop_skips_manually_deleted_payload_even_when_its_metadata_survives() {
    let f = Sandbox::new();
    let older = f.file("older");
    let deleted = f.file("deleted");
    f.ok([older.as_os_str()]);
    f.ok([deleted.as_os_str()]);
    fs::remove_file(f.trashed(&deleted)).unwrap();
    assert!(f.info(&deleted).exists());
    let database = fs::read(f.state.join("history.sqlite3")).unwrap();
    let preview = f.ok(["-p", "--dry-run"]);
    assert!(String::from_utf8_lossy(&preview.stdout).contains("-older"));
    assert!(!String::from_utf8_lossy(&preview.stdout).contains("-deleted"));
    assert_eq!(database, fs::read(f.state.join("history.sqlite3")).unwrap());
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(&older).unwrap(), "older");
    assert!(!deleted.exists());
    assert_eq!(f.receipt_count(), 0);
    // Empty can still remove orphaned metadata, without attempting restoration.
    let glob = format!("{}-*", f.prefix);
    f.ok(["--empty", "--glob", &glob]);
    assert!(!f.info(&deleted).exists());
}

#[test]
fn pop_restores_survivors_when_payloads_or_metadata_are_deleted_from_a_group() {
    let f = Sandbox::new();
    let payload_lost = f.file("payload-lost");
    let info_lost = f.file("info-lost");
    let survivor = f.file("survivor");
    f.ok([
        payload_lost.as_os_str(),
        info_lost.as_os_str(),
        survivor.as_os_str(),
    ]);
    fs::remove_file(f.trashed(&payload_lost)).unwrap();
    fs::remove_file(f.info(&info_lost)).unwrap();
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(&survivor).unwrap(), "survivor");
    assert!(!payload_lost.exists());
    assert!(!info_lost.exists());
    assert_eq!(
        fs::read_to_string(f.trashed(&info_lost)).unwrap(),
        "info-lost"
    );
    assert_eq!(f.receipt_count(), 0);
    // Fresh deletes and pop must still work after reconciliation.
    let next = f.file("next");
    f.ok([next.as_os_str()]);
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(next).unwrap(), "next");
}

#[test]
fn insufficient_pop_count_after_external_purge_restores_nothing() {
    let f = Sandbox::new();
    let older = f.file("older");
    let gone = f.file("gone");
    f.ok([older.as_os_str()]);
    f.ok([gone.as_os_str()]);
    fs::remove_file(f.trashed(&gone)).unwrap();
    fs::remove_file(f.info(&gone)).unwrap();
    assert!(!f.run(["-p", "2"]).status.success());
    assert!(!older.exists());
    assert!(!gone.exists());
    assert_eq!(f.receipt_count(), 1);
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(older).unwrap(), "older");
}

#[test]
fn externally_restored_members_are_skipped_without_overwriting_their_contents() {
    let f = Sandbox::new();
    let restored = f.file("restored");
    let remaining = f.file("remaining");
    f.ok([restored.as_os_str(), remaining.as_os_str()]);
    fs::rename(f.trashed(&restored), &restored).unwrap();
    fs::remove_file(f.info(&restored)).unwrap();
    fs::write(&restored, "edited after manual restore").unwrap();
    f.ok(["-p"]);
    assert_eq!(
        fs::read_to_string(restored).unwrap(),
        "edited after manual restore"
    );
    assert_eq!(fs::read_to_string(remaining).unwrap(), "remaining");
    assert_eq!(f.receipt_count(), 0);
}

#[test]
fn missing_parents_are_recreated_and_deleted_trashed_children_stay_deleted() {
    let f = Sandbox::new();
    let parent = f.work.join("parent/nested");
    fs::create_dir_all(&parent).unwrap();
    let file = parent.join(format!("{}-file", f.prefix));
    fs::write(&file, "file").unwrap();
    let tree = f.work.join(format!("{}-tree", f.prefix));
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("keep"), "keep").unwrap();
    fs::write(tree.join("gone"), "gone").unwrap();
    f.ok([file.as_os_str(), OsStr::new("-r"), tree.as_os_str()]);
    fs::remove_dir_all(f.work.join("parent")).unwrap();
    fs::remove_file(f.trashed(&tree).join("gone")).unwrap();
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(file).unwrap(), "file");
    assert_eq!(fs::read_to_string(tree.join("keep")).unwrap(), "keep");
    assert!(!tree.join("gone").exists());
    assert_eq!(f.receipt_count(), 0);
}

#[test]
fn replaced_parent_blocks_the_entire_pop_until_the_conflict_is_removed() {
    use std::os::unix::fs::symlink;
    for use_symlink in [false, true] {
        let f = Sandbox::new();
        let parent = f.work.join("parent");
        fs::create_dir(&parent).unwrap();
        let child = parent.join(format!("{}-child", f.prefix));
        fs::write(&child, "child").unwrap();
        let other = f.file("other");
        f.ok([child.as_os_str(), other.as_os_str()]);
        fs::remove_dir(&parent).unwrap();
        if use_symlink {
            symlink(&f.work, &parent).unwrap();
        } else {
            fs::write(&parent, "replacement parent").unwrap();
        }
        assert!(!f.run(["-p"]).status.success());
        assert!(!other.exists());
        assert_eq!(f.receipt_count(), 2);
        assert!(f.trashed(&child).exists());
        assert!(f.trashed(&other).exists());
        fs::remove_file(parent).unwrap();
        f.ok(["-p"]);
        assert_eq!(fs::read_to_string(child).unwrap(), "child");
        assert_eq!(fs::read_to_string(other).unwrap(), "other");
    }
}

#[test]
fn corrupted_sqlite_is_preserved_and_refused_before_any_files_are_changed() {
    let f = Sandbox::new();
    let trashed = f.file("trashed");
    f.ok([trashed.as_os_str()]);
    let untouched = f.file("untouched");
    let database = f.state.join("history.sqlite3");
    fs::write(&database, "manually corrupted database").unwrap();
    for args in [
        vec![untouched.as_os_str()],
        vec![OsStr::new("-p")],
        vec![OsStr::new("--empty"), OsStr::new("--glob"), OsStr::new("*")],
    ] {
        assert!(!f.run(args).status.success());
        assert_eq!(fs::read_to_string(&untouched).unwrap(), "untouched");
        assert_eq!(fs::read_to_string(f.trashed(&trashed)).unwrap(), "trashed");
        assert_eq!(
            fs::read_to_string(&database).unwrap(),
            "manually corrupted database"
        );
    }
}

#[test]
fn deleting_the_entire_native_trash_does_not_block_future_deletes_or_pop() {
    let f = Sandbox::new();
    let a = f.file("a");
    let b = f.file("b");
    f.ok([a.as_os_str()]);
    f.ok([b.as_os_str()]);
    fs::remove_dir_all(f.data.join("Trash")).unwrap();
    assert!(!f.run(["-p"]).status.success());
    assert_eq!(f.receipt_count(), 0);
    let next = f.file("next");
    f.ok([next.as_os_str()]);
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(next).unwrap(), "next");
    assert!(!a.exists());
    assert!(!b.exists());
}

#[test]
fn a_deleted_history_database_starts_fresh_without_claiming_existing_trash() {
    let f = Sandbox::new();
    let old = f.file("old");
    f.ok([old.as_os_str()]);
    fs::remove_file(f.state.join("history.sqlite3")).unwrap();
    let next = f.file("next");
    f.ok([next.as_os_str()]);
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(next).unwrap(), "next");
    assert_eq!(fs::read_to_string(f.trashed(&old)).unwrap(), "old");
    assert!(!old.exists());
    assert!(!f.run(["-p"]).status.success());
}

#[test]
fn manually_edited_native_receipts_are_not_restored_using_stale_history() {
    let f = Sandbox::new();
    let changed = f.file("changed");
    let good = f.file("good");
    f.ok([changed.as_os_str(), good.as_os_str()]);
    let destination = f.work.join("manually-edited-destination");
    let text = fs::read_to_string(f.info(&changed)).unwrap();
    let edited = text
        .lines()
        .map(|line| {
            if line.starts_with("Path=") {
                format!("Path={}", destination.display())
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(f.info(&changed), edited).unwrap();
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(good).unwrap(), "good");
    assert!(!destination.exists());
    assert!(!changed.exists());
    assert_eq!(fs::read_to_string(f.trashed(&changed)).unwrap(), "changed");
    assert_eq!(f.receipt_count(), 0);
}

#[test]
fn native_round_trip_tracks_invocations_and_preflights_collisions() {
    let f = Sandbox::new();
    let a = f.file("a");
    let b = f.file("b");
    let c = f.file("c");
    f.ok([a.as_os_str(), b.as_os_str()]);
    f.ok([c.as_os_str()]);
    assert_eq!(f.trash_files().len(), 3);
    fs::write(&a, "replacement").unwrap();
    let failed = f.run(["-fp", "2"]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("overwrite"));
    assert!(!b.exists());
    assert!(!c.exists());
    assert_eq!(fs::read_to_string(&a).unwrap(), "replacement");
    fs::remove_file(&a).unwrap();
    let before = fs::read(f.state.join("history.sqlite3")).unwrap();
    f.ok(["-p", "2", "--dry-run"]);
    assert!(!a.exists());
    assert_eq!(before, fs::read(f.state.join("history.sqlite3")).unwrap());
    f.ok(["-p", "2"]);
    for (path, text) in [(a, "a"), (b, "b"), (c, "c")] {
        assert_eq!(fs::read_to_string(path).unwrap(), text);
    }
    assert!(f.trash_files().is_empty());
}

#[test]
fn native_filtered_purge_combines_age_and_glob() {
    let f = Sandbox::new();
    let old_txt = f.file("old.txt");
    let new_txt = f.file("new.txt");
    let old_log = f.file("old.log");
    f.ok([
        old_txt.as_os_str(),
        new_txt.as_os_str(),
        old_log.as_os_str(),
    ]);
    // Adjust only this sandbox's metadata. Match unique names even if another
    // mounted volume happens to have a real trash bin: never purge it with --all.
    let info = f.data.join("Trash/info");
    for entry in fs::read_dir(info).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("-old.")
        {
            let text = fs::read_to_string(&path).unwrap();
            let text = text
                .lines()
                .map(|line| {
                    if line.starts_with("DeletionDate=") {
                        "DeletionDate=2020-01-01T00:00:00"
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            fs::write(path, text).unwrap();
        }
    }
    // Simulate the same clock shift in the receipts: deletion time is part of
    // an item's identity so recycled native IDs cannot restore unrelated files.
    let history_path = f.state.join("history.sqlite3");
    {
        use std::os::unix::ffi::OsStrExt;
        let database = rusqlite::Connection::open(&history_path).unwrap();
        for path in [&old_txt, &old_log] {
            database
                .execute(
                    "UPDATE items SET deleted_at = ?1 WHERE original = ?2",
                    rusqlite::params![1_577_836_800_i64, path.as_os_str().as_bytes()],
                )
                .unwrap();
        }
    }
    let glob = format!("{}-*.txt", f.prefix);
    let history = fs::read(&history_path).unwrap();
    f.ok([
        "--empty",
        "--older-than",
        "2mo 2d",
        "--glob",
        &glob,
        "--dry-run",
    ]);
    assert_eq!(f.trash_files().len(), 3);
    assert_eq!(history, fs::read(&history_path).unwrap());
    f.ok(["--empty", "--older-than", "2mo 2d", "--glob", &glob]);
    let names: Vec<_> = f
        .trash_files()
        .into_iter()
        .map(|path| path.file_name().unwrap().to_owned())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(
        names
            .iter()
            .all(|name| !name.to_string_lossy().contains("-old.txt"))
    );
    f.ok(["-p"]);
    assert!(new_txt.exists());
    assert!(!old_txt.exists());
    assert!(old_log.exists());
}

#[test]
fn prompts_accept_yes_decline_no_and_preserve_last_flag_wins() {
    let f = Sandbox::new();
    let a = f.file("a");
    let output = f.prompt(&[OsStr::new("-i"), a.as_os_str()], b"n\n");
    assert!(output.status.success());
    assert!(a.exists());
    let output = f.prompt(&[OsStr::new("-fi"), a.as_os_str()], b"yes\n");
    assert!(output.status.success());
    assert!(!a.exists());
    f.ok(["-p"]);
    let output = f.prompt(&[OsStr::new("-if"), a.as_os_str()], b"n\n");
    assert!(output.status.success());
    assert!(!a.exists());
    assert!(output.stderr.is_empty());
    f.ok(["-p"]);
    let output = f.prompt(&[OsStr::new("-I"), OsStr::new("-r"), a.as_os_str()], b"n\n");
    assert!(output.status.success());
    assert!(a.exists());
}

#[test]
fn native_recursive_directories_and_binary_symlinks_round_trip() {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let f = Sandbox::new();
    let dir = f.work.join(format!("{}-dir", f.prefix));
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("child"), "child").unwrap();
    assert!(!f.run([OsStr::new("-d"), dir.as_os_str()]).status.success());
    f.ok([OsStr::new("-r"), dir.as_os_str()]);
    f.ok(["-p"]);
    assert_eq!(fs::read_to_string(dir.join("child")).unwrap(), "child");
    let mut bytes = f.prefix.as_bytes().to_vec();
    bytes.extend_from_slice(b"-binary-\xff");
    let link = f.work.join(OsString::from_vec(bytes));
    symlink(Path::new("absent"), &link).unwrap();
    f.ok([link.as_os_str()]);
    assert!(fs::symlink_metadata(&link).is_err());
    f.ok(["-p"]);
    assert_eq!(fs::read_link(link).unwrap(), Path::new("absent"));
}

#[test]
fn invalid_selectors_and_dry_run_never_create_state_or_trash() {
    let f = Sandbox::new();
    let a = f.file("a");
    f.ok([OsStr::new("--dry-run"), a.as_os_str()]);
    assert!(a.exists());
    assert!(!f.state.exists());
    assert!(!f.data.exists());
    for flags in [
        vec!["--empty"],
        vec!["--empty", "--older-than", "2m"],
        vec!["--empty", "--glob", "["],
        vec!["-p", "0"],
    ] {
        assert!(!f.run(flags).status.success());
    }
    assert!(a.exists());
    assert!(!f.state.exists());
    assert!(!f.data.exists());
    f.ok(["-f"]);
}
