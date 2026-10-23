use crate::{
    backend::{Backend, SystemTrash, TrashItem},
    cli::{Cli, Interactive, PreserveRoot},
    history::{ItemKey, Store, same_path},
};
use chrono::Utc;
use eros::{Context, IntoAnyUnion, Result, bail};
use globset::{GlobBuilder, GlobMatcher};
use std::{
    collections::HashSet,
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

pub fn run(cli: Cli) -> Result<bool> {
    let config = crate::config::Config::load(cli.config.as_deref())?;
    run_with(
        &cli,
        &mut SystemTrash::new(config.trash_location),
        Store::default_dir()?,
    )
}

fn run_with(cli: &Cli, backend: &mut impl Backend, state_dir: PathBuf) -> Result<bool> {
    if !cli.empty && cli.pop.is_none() && cli.paths.is_empty() {
        if cli.force {
            return Ok(true);
        }
        bail!("missing operand; try 'bin --help'");
    }
    if cli.empty && !cli.all && cli.older_than.is_none() && cli.glob.is_none() {
        bail!("--empty requires --all, --older-than AGE, or --glob PATTERN");
    }
    let matcher = cli
        .glob
        .as_ref()
        .map(|pattern| {
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .map(|glob| glob.compile_matcher())
                .context("invalid --glob pattern")
        })
        .transpose()?;
    let cutoff = cli
        .older_than
        .as_ref()
        .map(|age| age.cutoff(Utc::now()))
        .transpose()?;
    let mut store = Store::open(state_dir, cli.dry_run)?;
    let mut live = Vec::new();
    if cli.pop.is_some() || cli.empty || !cli.dry_run {
        // The crate exposes only trashing on macOS, so don't require a list for
        // its basic delete operation. Undo/cleanup fail explicitly there.
        let must_list = backend.supports_history() || cli.pop.is_some() || cli.empty;
        if must_list {
            live = backend.list().context("cannot list the system trash")?;
            let available = if cli.pop.is_some() || store.has_pending()? {
                Some(
                    live.iter()
                        .filter_map(|item| match backend.available(item) {
                            Ok(true) => Some(Ok(item.clone())),
                            Ok(false) => None,
                            Err(error) => Some(Err(error)),
                        })
                        .collect::<Result<Vec<_>>>()?,
                )
            } else {
                None
            };
            store.recover_pending(available.as_deref().unwrap_or(&live))?;
            if let Some(count) = cli.pop {
                return pop(
                    cli,
                    count as usize,
                    backend,
                    &mut store,
                    available.as_deref().unwrap(),
                );
            }
            if cli.empty {
                return empty(
                    cli,
                    backend,
                    &mut store,
                    &live,
                    matcher.as_ref(),
                    cutoff.map(|date| date.timestamp()),
                );
            }
        }
    }
    trash_paths(cli, backend, &mut store, &mut live)
}

fn ask(message: &str) -> Result<bool> {
    eprint!("bin: {message} [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn explain(cli: &Cli, verb: &str, path: &Path) {
    if cli.dry_run {
        println!("would {verb} {path:?}");
    } else if cli.verbose {
        println!("{verb} {path:?}");
    }
}

fn trash_paths(
    cli: &Cli,
    backend: &mut impl Backend,
    store: &mut Store,
    live: &mut Vec<TrashItem>,
) -> Result<bool> {
    if !cli.dry_run
        && cli.interaction() == Some(Interactive::Once)
        && (cli.paths.len() > 3 || cli.recursive)
        && !ask(&format!("trash {} operand(s)?", cli.paths.len()))?
    {
        return Ok(true);
    }
    let mut success = true;
    let mut invocation_index = None;
    for operand in &cli.paths {
        let result = (|| -> Result<()> {
            let (path, metadata) = match check_operand(operand, cli, &store.dir) {
                Ok(value) => value,
                Err(error)
                    if cli.force
                        && error.downcast_inner_ref::<io::Error>().is_some_and(|e| {
                            matches!(
                                e.kind(),
                                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                            )
                        }) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            if cli.dry_run {
                explain(cli, "trash", &path);
                return Ok(());
            }
            let prompt = cli.interaction() == Some(Interactive::Always)
                || (cli.interaction().is_none()
                    && io::stdin().is_terminal()
                    && !metadata.file_type().is_symlink()
                    && metadata.permissions().readonly());
            if prompt
                && !ask(&format!(
                    "trash {operand:?}{}?",
                    if metadata.is_dir() {
                        " and its contents"
                    } else {
                        ""
                    }
                ))?
            {
                return Ok(());
            }
            if !backend.supports_history() {
                backend.trash(&path)?;
                eprintln!(
                    "bin: macOS trashing succeeded; pop history is unavailable on this platform"
                );
            } else {
                let before: Vec<_> = live
                    .iter()
                    .filter(|item| same_path(&item.original_path(), &path))
                    .map(ItemKey::from)
                    .collect();
                let index = store.begin_trash(invocation_index, &path, &before)?;
                invocation_index = Some(index);
                let deletion = backend.trash(&path);
                let after = backend.list().context("cannot record the trash receipt; the pending operation will be reconciled next time")?;
                let candidates: Vec<_> = after
                    .iter()
                    .filter(|item| same_path(&item.original_path(), &path))
                    .map(ItemKey::from)
                    .filter(|key| !before.contains(key))
                    .collect();
                match candidates.as_slice() {
                    [key] => store.finish_trash(index, Some(key))?,
                    [] if deletion.is_err() && fs::symlink_metadata(&path).is_ok() => {
                        store.finish_trash(index, None)?;
                    }
                    _ => bail!(
                        "could not identify the trash receipt for {path:?}; pending history preserved for recovery"
                    ),
                }
                *live = after;
                deletion?;
            }
            explain(cli, "trashed", &path);
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("bin: cannot trash {operand:?}: {error:#?}");
            success = false;
            // Continuing could replace a pending receipt and lose undo history.
            if store.has_pending()? {
                break;
            }
        }
    }
    if let Some(index) = invocation_index {
        store.finish_invocation(index)?;
    }
    Ok(success)
}

fn check_operand(operand: &Path, cli: &Cli, state_dir: &Path) -> Result<(PathBuf, fs::Metadata)> {
    let bytes = operand.as_os_str().as_encoded_bytes();
    let final_component = bytes
        .rsplit(|byte| *byte == b'/' || (cfg!(windows) && *byte == b'\\'))
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    if final_component == b"." || final_component == b".." {
        bail!("refusing to trash '.' or '..'");
    }
    // Resolve parent symlinks but never dereference the final symlink, including
    // dangling symlinks. Passing this exact path also avoids trailing-slash traps.
    let absolute = std::path::absolute(operand)?;
    let name = absolute
        .file_name()
        .context("filesystem roots cannot be moved to trash")?;
    let parent = absolute
        .parent()
        .context("operand has no parent")?
        .canonicalize()?;
    let path = parent.join(name);
    let metadata = fs::symlink_metadata(&path)?;
    let protected_state = resolve_existing_ancestor(state_dir)?;
    if path.starts_with(&protected_state)
        || (metadata.is_dir() && protected_state.starts_with(&path))
    {
        bail!("operand is inside or contains bin's history directory {state_dir:?}");
    }
    if metadata.is_dir() {
        if path.canonicalize()?.parent().is_none() {
            bail!("filesystem roots cannot be moved to trash");
        }
        if !cli.recursive {
            if !cli.dir {
                bail!("is a directory (use -r or -d)");
            }
            if fs::read_dir(&path)?.next().transpose()?.is_some() {
                bail!("directory is not empty (use -r)");
            }
        }
    }
    if cli.preserve_root == Some(PreserveRoot::All) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.dev() != fs::metadata(parent)?.dev() {
                bail!("operand is on a different device from its parent (--preserve-root=all)");
            }
        }
        #[cfg(windows)]
        bail!("--preserve-root=all device checking is only supported on Unix");
    }
    Ok((path, metadata))
}

/// Canonicalize the existing prefix so that state protection also works before
/// the first invocation and when XDG_STATE_HOME is reached through a symlink.
fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut prefix = path;
    let mut suffix = Vec::new();
    loop {
        match prefix.canonicalize() {
            Ok(mut canonical) => {
                for component in suffix.into_iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                suffix.push(
                    prefix
                        .file_name()
                        .context("cannot resolve state directory")?
                        .to_os_string(),
                );
                prefix = prefix.parent().context("cannot resolve state directory")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn pop(
    cli: &Cli,
    count: usize,
    backend: &mut impl Backend,
    store: &mut Store,
    live: &[TrashItem],
) -> Result<bool> {
    let keys = store.latest_keys(count, live)?;
    let mut selected: Vec<_> = live
        .iter()
        .filter(|item| keys.contains(&ItemKey::from(*item)))
        .cloned()
        .collect();
    // Restore directory ancestors first, then their children. This also handles
    // trashing a file and later trashing its parent in a separate invocation.
    selected.sort_by_key(|item| {
        (
            item.original_path().components().count(),
            item.original_path(),
        )
    });
    preflight_restore(&selected)?;
    if !cli.dry_run
        && cli.interaction() == Some(Interactive::Once)
        && !ask(&format!(
            "restore {count} invocation(s), {} item(s)?",
            selected.len()
        ))?
    {
        return Ok(true);
    }
    for item in selected {
        let path = item.original_path();
        if !cli.dry_run
            && cli.interaction() == Some(Interactive::Always)
            && !ask(&format!("restore {path:?}?"))?
        {
            continue;
        }
        if !cli.dry_run {
            backend.restore(item.clone()).with_context(|| {
                format!("cannot restore {path:?}; remaining history is preserved")
            })?;
            store.remove_item(&ItemKey::from(&item))?;
        }
        explain(cli, if cli.dry_run { "restore" } else { "restored" }, &path);
    }
    Ok(true)
}

fn preflight_restore(items: &[TrashItem]) -> Result<()> {
    let mut destinations = HashSet::new();
    for item in items {
        let path = item.original_path();
        if !destinations.insert(dunce::simplified(&path).to_path_buf()) {
            bail!(
                "pop would restore multiple versions of {path:?}; restore one invocation at a time"
            );
        }
        match fs::symlink_metadata(&path) {
            Ok(_) => bail!("pop would overwrite {path:?}; no items were restored"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .any_union()
                    .with_context(|| format!("cannot check restore destination {path:?}"));
            }
        }
        for parent in item.original_parent.ancestors() {
            match fs::symlink_metadata(parent) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    bail!(
                        "restore parent {parent:?} is a symlink or is not a directory; no items were restored"
                    );
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .any_union()
                        .with_context(|| format!("cannot check restore parent {parent:?}"));
                }
            }
        }
    }
    Ok(())
}

fn empty(
    cli: &Cli,
    backend: &mut impl Backend,
    store: &mut Store,
    live: &[TrashItem],
    matcher: Option<&GlobMatcher>,
    cutoff: Option<i64>,
) -> Result<bool> {
    if cutoff.is_some() && live.iter().any(|item| item.time_deleted < 0) {
        bail!("trash contains entries without deletion timestamps; refusing age-based cleanup");
    }
    let mut selected: Vec<_> = live
        .iter()
        .filter(|item| {
            cutoff.is_none_or(|cutoff| item.time_deleted < cutoff)
                && matcher.is_none_or(|glob| glob.is_match(Path::new(&item.name)))
        })
        .collect();
    selected.sort_by_key(|item| (item.original_path(), item.time_deleted));
    if !cli.dry_run
        && cli.interaction() == Some(Interactive::Once)
        && !selected.is_empty()
        && !ask(&format!(
            "permanently delete {} trash item(s)?",
            selected.len()
        ))?
    {
        return Ok(true);
    }
    let mut success = true;
    for item in selected {
        let path = item.original_path();
        if !cli.dry_run
            && cli.interaction() == Some(Interactive::Always)
            && !ask(&format!("permanently delete trashed {path:?}?"))?
        {
            continue;
        }
        if !cli.dry_run {
            if let Err(error) = backend.purge(item) {
                eprintln!("bin: cannot permanently delete trashed {path:?}: {error:#?}");
                success = false;
                continue;
            }
            store.remove_item(&ItemKey::from(item))?;
        }
        explain(
            cli,
            if cli.dry_run {
                "permanently delete trashed"
            } else {
                "permanently deleted trashed"
            },
            &path,
        );
    }
    Ok(success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::ffi::OsString;

    struct FakeTrash {
        root: PathBuf,
        items: Vec<TrashItem>,
        next: usize,
        inspection_error: Option<OsString>,
    }

    impl Backend for FakeTrash {
        fn list(&self) -> Result<Vec<TrashItem>> {
            Ok(self.items.clone())
        }

        fn available(&self, item: &TrashItem) -> Result<bool> {
            if self.inspection_error.as_ref() == Some(&item.id) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "simulated trash inspection denial",
                )
                .into());
            }
            match fs::symlink_metadata(Path::new(&item.id)) {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error.into()),
            }
        }

        fn trash(&mut self, path: &Path) -> Result<()> {
            let id = self.root.join(self.next.to_string());
            self.next += 1;
            fs::rename(path, &id)?;
            self.items.push(TrashItem {
                id: id.into_os_string(),
                name: path.file_name().unwrap().into(),
                original_parent: path.parent().unwrap().into(),
                time_deleted: Utc::now().timestamp(),
            });
            Ok(())
        }

        fn restore(&mut self, item: TrashItem) -> Result<()> {
            let path = item.original_path();
            if fs::symlink_metadata(&path).is_ok() {
                bail!("collision");
            }
            fs::create_dir_all(&item.original_parent)?;
            fs::rename(Path::new(&item.id), &path)?;
            self.items.retain(|entry| entry.id != item.id);
            Ok(())
        }

        fn purge(&mut self, item: &TrashItem) -> Result<()> {
            let path = Path::new(&item.id);
            if fs::symlink_metadata(path)?.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
            self.items.retain(|entry| entry.id != item.id);
            Ok(())
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        work: PathBuf,
        state: PathBuf,
        backend: FakeTrash,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            // Windows/macOS temp paths can contain symlinked parents.
            let base = root.path().canonicalize().unwrap();
            let work = base.join("work");
            let trash = base.join("trash");
            fs::create_dir(&work).unwrap();
            fs::create_dir(&trash).unwrap();
            Self {
                work,
                state: base.join("state"),
                backend: FakeTrash {
                    root: trash,
                    items: vec![],
                    next: 0,
                    inspection_error: None,
                },
                _root: root,
            }
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.work.join(name);
            fs::write(&path, name).unwrap();
            path
        }

        fn run(&mut self, flags: &[&str], files: &[&Path]) -> Result<bool> {
            let args = std::iter::once(OsString::from("bin"))
                .chain(flags.iter().map(OsString::from))
                .chain(files.iter().map(|path| path.as_os_str().to_os_string()));
            let cli = Cli::try_parse_from(args).unwrap();
            run_with(&cli, &mut self.backend, self.state.clone())
        }
    }

    #[test]
    fn pop_counts_invocations_and_restores_all_operands() {
        let mut f = Fixture::new();
        let a = f.file("a");
        let b = f.file("b");
        let c = f.file("c");
        assert!(f.run(&[], &[&a, &b]).unwrap());
        assert!(f.run(&[], &[&c]).unwrap());
        f.run(&["-p"], &[]).unwrap();
        assert!(c.exists());
        assert!(!a.exists());
        assert!(!b.exists());
        f.run(&["-p"], &[]).unwrap();
        assert_eq!(fs::read_to_string(a).unwrap(), "a");
        assert_eq!(fs::read_to_string(b).unwrap(), "b");
        assert!(f.run(&["-p"], &[]).is_err());
    }

    #[test]
    fn pop_preflights_every_destination_even_with_force_or_dry_run() {
        let mut f = Fixture::new();
        let a = f.file("a");
        let b = f.file("b");
        f.run(&[], &[&a]).unwrap();
        f.run(&[], &[&b]).unwrap();
        fs::write(&a, "new contents").unwrap();
        for flags in [&["-p", "2", "-f"][..], &["-p", "2", "--dry-run"][..]] {
            assert!(
                f.run(flags, &[])
                    .unwrap_err()
                    .to_string()
                    .contains("overwrite")
            );
            assert!(!b.exists());
            assert_eq!(f.backend.items.len(), 2);
            assert_eq!(fs::read_to_string(&a).unwrap(), "new contents");
        }
    }

    #[test]
    fn pop_rejects_duplicate_versions_and_excessive_count_before_restoring() {
        let mut f = Fixture::new();
        let a = f.file("a");
        f.run(&[], &[&a]).unwrap();
        f.file("a");
        f.run(&[], &[&a]).unwrap();
        assert!(f.run(&["-p", "3"], &[]).is_err());
        assert!(
            f.run(&["-p", "2"], &[])
                .unwrap_err()
                .to_string()
                .contains("multiple versions")
        );
        assert!(!a.exists());
        assert_eq!(f.backend.items.len(), 2);
    }

    #[test]
    fn combined_filters_use_deletion_age_and_basename_and_prune_history() {
        let mut f = Fixture::new();
        let a = f.file("old.txt");
        let b = f.file("new.txt");
        let c = f.file("old.log");
        f.run(&[], &[&a, &b, &c]).unwrap();
        let old = Utc::now().timestamp() - 10 * 86_400;
        f.backend.items[0].time_deleted = old;
        f.backend.items[2].time_deleted = old;
        // Simulate older native trash entries, including entries from other apps.
        f.run(&["--empty", "--older-than", "1wk", "--glob", "*.txt"], &[])
            .unwrap();
        assert_eq!(f.backend.items.len(), 2);
        assert!(f.backend.items.iter().all(|item| item.name != "old.txt"));
        f.run(&["-p"], &[]).unwrap();
        assert!(b.exists());
        assert!(!a.exists());
    }

    #[test]
    fn dry_run_changes_neither_files_trash_nor_history() {
        let mut f = Fixture::new();
        let a = f.file("a");
        f.run(&["--dry-run"], &[&a]).unwrap();
        assert!(a.exists());
        assert!(!f.state.exists());
        f.run(&[], &[&a]).unwrap();
        let before = fs::read(f.state.join("history.sqlite3")).unwrap();
        f.run(&["-p", "--dry-run"], &[]).unwrap();
        f.run(&["--empty", "--all", "--dry-run"], &[]).unwrap();
        assert!(!a.exists());
        assert_eq!(f.backend.items.len(), 1);
        assert_eq!(before, fs::read(f.state.join("history.sqlite3")).unwrap());
    }

    #[test]
    fn directory_rules_partial_failure_and_force_missing_match_rm() {
        let mut f = Fixture::new();
        let dir = f.work.join("dir");
        fs::create_dir(&dir).unwrap();
        assert!(!f.run(&[], &[&dir]).unwrap());
        f.run(&["-d"], &[&dir]).unwrap();
        f.run(&["-p"], &[]).unwrap();
        fs::write(dir.join("child"), "child").unwrap();
        assert!(!f.run(&["-d"], &[&dir]).unwrap());
        f.run(&["-r"], &[&dir]).unwrap();
        f.run(&["-p"], &[]).unwrap();
        assert!(dir.join("child").exists());
        let missing = f.work.join("missing");
        let file = f.file("good");
        assert!(!f.run(&[], &[&missing, &file]).unwrap());
        assert!(!file.exists());
        f.run(&["-p"], &[]).unwrap();
        assert!(file.exists());
        assert!(f.run(&["-f"], &[&missing]).unwrap());
    }

    #[test]
    fn pop_restores_parent_before_separately_trashed_child() {
        let mut f = Fixture::new();
        let dir = f.work.join("dir");
        fs::create_dir(&dir).unwrap();
        let file = dir.join("child");
        fs::write(&file, "contents").unwrap();
        f.run(&[], &[&file]).unwrap();
        f.run(&["-r"], &[&dir]).unwrap();
        f.run(&["-p", "2"], &[]).unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "contents");
    }

    #[test]
    fn interrupted_trash_receipt_is_recovered() {
        let mut f = Fixture::new();
        let a = f.file("a");
        {
            let mut store = Store::open(f.state.clone(), false).unwrap();
            store.begin_trash(None, &a, &[]).unwrap();
            f.backend.trash(&a).unwrap();
        }
        f.run(&["-p"], &[]).unwrap();
        assert_eq!(fs::read_to_string(a).unwrap(), "a");
    }

    #[test]
    fn manually_deleted_pending_payload_does_not_block_older_or_future_invocations() {
        let mut f = Fixture::new();
        let older = f.file("older");
        let lost = f.file("lost");
        f.run(&[], &[&older]).unwrap();
        {
            let mut store = Store::open(f.state.clone(), false).unwrap();
            store.begin_trash(None, &lost, &[]).unwrap();
            f.backend.trash(&lost).unwrap();
            fs::remove_file(Path::new(&f.backend.items.last().unwrap().id)).unwrap();
        }
        let before = fs::read(f.state.join("history.sqlite3")).unwrap();
        f.run(&["-p", "--dry-run"], &[]).unwrap();
        assert!(!older.exists());
        assert!(!lost.exists());
        assert_eq!(before, fs::read(f.state.join("history.sqlite3")).unwrap());
        f.run(&["-p"], &[]).unwrap();
        assert_eq!(fs::read_to_string(older).unwrap(), "older");
        assert!(!lost.exists());
        {
            let store = Store::open(f.state.clone(), false).unwrap();
            assert!(!store.has_pending().unwrap());
        }
        let next = f.file("next");
        f.run(&[], &[&next]).unwrap();
        f.run(&["-p"], &[]).unwrap();
        assert_eq!(fs::read_to_string(next).unwrap(), "next");
    }

    #[test]
    fn inspection_errors_preserve_every_receipt_and_can_be_retried() {
        let mut f = Fixture::new();
        let a = f.file("a");
        let b = f.file("b");
        f.run(&[], &[&a, &b]).unwrap();
        f.backend.inspection_error = Some(f.backend.items[0].id.clone());
        let before = fs::read(f.state.join("history.sqlite3")).unwrap();
        assert!(f.run(&["-p"], &[]).is_err());
        assert_eq!(before, fs::read(f.state.join("history.sqlite3")).unwrap());
        assert!(!a.exists());
        assert!(!b.exists());
        assert_eq!(f.backend.items.len(), 2);
        f.backend.inspection_error = None;
        f.run(&["-p"], &[]).unwrap();
        assert_eq!(fs::read_to_string(a).unwrap(), "a");
        assert_eq!(fs::read_to_string(b).unwrap(), "b");
    }

    #[test]
    fn selectors_and_history_protection_fail_before_removing_data() {
        let mut f = Fixture::new();
        assert!(f.run(&["--empty"], &[]).is_err());
        let a = f.file("a");
        f.run(&[], &[&a]).unwrap();
        assert!(f.run(&["--empty", "--glob", "["], &[]).is_err());
        let state = f.state.clone();
        assert!(!f.run(&["-r"], &[&state]).unwrap());
        assert!(!f.run(&[], &[&state.join("history.lock")]).unwrap());
        assert!(!f.run(&[], &[&state.join("history.sqlite3")]).unwrap());
        assert!(state.exists());
        assert_eq!(f.backend.items.len(), 1);
    }

    #[test]
    fn empty_all_removes_owned_and_external_items_and_consumes_history() {
        let mut f = Fixture::new();
        let a = f.file("owned");
        let b = f.file("external");
        f.run(&[], &[&a]).unwrap();
        f.backend.trash(&b).unwrap();
        f.run(&["--empty", "--all"], &[]).unwrap();
        assert!(f.backend.items.is_empty());
        assert!(!a.exists());
        assert!(!b.exists());
        assert!(f.run(&["-p"], &[]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_and_broken_links_block_pop() {
        use std::os::unix::fs::symlink;
        let mut f = Fixture::new();
        let target = f.file("target");
        let link = f.work.join("link");
        symlink(&target, &link).unwrap();
        f.run(&[], &[&link]).unwrap();
        assert!(target.exists());
        assert!(fs::symlink_metadata(&link).is_err());
        symlink(f.work.join("missing"), &link).unwrap();
        assert!(f.run(&["-p"], &[]).is_err());
        fs::remove_file(&link).unwrap();
        f.run(&["-p"], &[]).unwrap();
        assert_eq!(fs::read_link(link).unwrap(), target);
    }
}
