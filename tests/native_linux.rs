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
    let before = fs::read(f.state.join("history.json")).unwrap();
    f.ok(["-p", "2", "--dry-run"]);
    assert!(!a.exists());
    assert_eq!(before, fs::read(f.state.join("history.json")).unwrap());
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
    let history_path = f.state.join("history.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&fs::read(&history_path).unwrap()).unwrap();
    for item in saved["invocations"][0]["items"].as_array_mut().unwrap() {
        let original: Vec<u8> = serde_json::from_value(item["original"].clone()).unwrap();
        if String::from_utf8_lossy(&original).contains("-old.") {
            item["time_deleted"] = serde_json::json!(1_577_836_800);
        }
    }
    fs::write(history_path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let glob = format!("{}-*.txt", f.prefix);
    let history = fs::read(f.state.join("history.json")).unwrap();
    f.ok([
        "--empty",
        "--older-than",
        "2mo 2d",
        "--glob",
        &glob,
        "--dry-run",
    ]);
    assert_eq!(f.trash_files().len(), 3);
    assert_eq!(history, fs::read(f.state.join("history.json")).unwrap());
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
