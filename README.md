# bin

`bin` is an `rm`-style Rust CLI that moves files into the operating system's trash.
It adds undo by **command invocation**, filtered permanent cleanup, and previews
for every operation. System trash operations use our
[fork of the trash crate](https://github.com/mcmah309/trash-rs), pinned as a Git
submodule while its FreeDesktop improvements are proposed upstream. Windows and
macOS retain the crate's native operating system implementations.
The upstream changes are tracked in [trash-rs#155](https://github.com/Byron/trash-rs/pull/155).

## Install

```sh
git submodule update --init --recursive
cargo install --path . --locked
```

When cloning a new checkout, use `git clone --recurse-submodules` to fetch the
fork together with `bin`.

Building requires Rust and a C compiler for the bundled SQLite library. SQLite
does not need to be installed separately. Cross-compilation also requires a C
toolchain for the target platform.

## Usage

```sh
bin report.txt other.txt       # One invocation containing two files
bin -r build/                  # Trash a directory and everything inside it
bin -d empty-directory/        # Refuse unless the directory is empty
bin -fi report.txt             # Last prompt flag wins, as with rm
bin -- -starts-with-a-dash

bin -p                         # Restore every remaining item from the last invocation
bin -p 3                       # Restore the last three restorable invocations
bin --pop=2 --dry-run

bin --empty --all
bin --empty --older-than '2mo'
bin --empty --older-than '2mo 3d'
bin --empty --glob '*.txt'
bin --empty --older-than '2mo 2d' --glob '*.txt' --dry-run
```

Normal trashing is silent; `-v` explains actions. `--dry-run` always prints what
would happen, performs the same validation, and changes no files, trash entries,
or history. It does not prompt. A dry run is a snapshot, not a reservation.

## Trash location and configuration

On Linux and other FreeDesktop systems, **all new deletions go to the user's
home trash by default**: `$XDG_DATA_HOME/Trash`, normally
`~/.local/share/Trash`. This includes files from other mounted filesystems and
bind-mounted workspaces. `bin` does not create a per-mount `.Trash-UID` directory
in this mode. If home trash cannot be used, the operation fails and reports the
error rather than silently choosing another trash directory.

Configuration is optional. On Linux, `bin` reads
`$XDG_CONFIG_HOME/bin/config.toml`, normally `~/.config/bin/config.toml`:

```toml
trash-location = "home"
```

To use the previous policy, which prefers trash on each file's own filesystem:

```toml
trash-location = "per-filesystem"
```

That policy uses the home trash for files on its filesystem, and otherwise uses
the source mount's `.Trash/$UID` or `.Trash-$UID` directory when available. It
falls back to home trash if the mount's trash cannot
be created due to permissions. See [config.example.toml](config.example.toml).

Use `bin --config /path/to/config.toml ...` to select another configuration file.
A missing default config uses `home`; an explicitly selected missing file, invalid
TOML, unknown keys, or an unknown location value fails before any trash operation.
Dry runs read configuration but never create or modify it.

Cross-filesystem trashing and restoration copy the complete payload before
removing the source, so large directories can take longer and require space on
the destination filesystem. Regular file/directory modes and timestamps are
preserved, and symlinks are copied without following their targets. Ownership,
hard-link relationships, ACLs, and extended attributes may differ after copying
between filesystems. Special files that cannot be copied (such as sockets) cause
the operation to fail. Failed copies leave the source intact and clean up partial
destinations. If source cleanup fails after a complete copy, the complete backup
is retained with its trash receipt and the error is reported.

Changing configuration affects **new deletions only**. Existing trash entries,
including old per-mount entries, keep their original locations and remain
available to pop and empty. All locations retain the standard FreeDesktop format
for interoperability with other trash utilities. Windows and macOS retain their
native operating system trash placement; `trash-location` applies to FreeDesktop
systems.

## rm compatibility

| Option | Behavior |
| --- | --- |
| `-f`, `--force` | Ignore missing operands and never prompt. Never enables overwrite on pop. |
| `-i` | Prompt for each operand. |
| `-I` | Prompt once if there are more than three operands or `-r` is used. |
| `--interactive[=never\|once\|always]` | Control prompting; no value means `always`. |
| `-r`, `-R`, `--recursive` | Trash a directory tree. |
| `-d`, `--dir` | Allow empty directories. |
| `-v`, `--verbose` | Explain actions. |
| `--preserve-root[=all]` | Always protect filesystem roots; `all` also rejects a Unix operand on a different device from its parent. |
| `--` | End options, allowing filenames starting with `-`. |

The last of `-f`, `-i`, `-I`, and `--interactive` wins. Without explicit prompting
options, write-protected operands prompt when stdin is a terminal. A declined
prompt is successful; an operand error returns exit status 1 while allowing other
operands to succeed. Invalid CLI syntax returns 2.

Directories move as whole trees, so `-ri` asks about each directory operand and
its entire contents, rather than each descendant. Symlinks move without following
their targets, including dangling links. `.` and `..` are refused, as are
filesystem roots and any directory containing `bin`'s history. GNU options such
as `--one-file-system`, `--no-preserve-root`, and `--interactive` abbreviations
are not implemented. Recursive trashing does not promise to skip mounted trees.
The history directory and its own files are also protected from trashing.

For pop and empty, `-i` asks per entry and `-I` asks once for the entire operation.

## Undo semantics

An invocation is one `bin FILE...` command with at least one successfully trashed
operand. Errors, skipped operands, dry runs, pops, and cleanup commands add no
invocations. A recursive directory is one entry. History persists across shells
and working directories, using absolute original paths and native trash IDs.

Before pop moves anything, it checks **every selected destination**. An existing
file, directory, or dangling symlink causes the whole pop to fail. Multiple
selected versions of the same path also fail; pop one invocation at a time and
move the restored version aside. `-f` never bypasses these checks. Missing parent
directories are recreated by the native backend; existing symlink parents are
refused. Parent directories restore before separately trashed children.

Empty or externally restored/purged invocations are skipped. If fewer than N
restorable invocations remain, `-p N` fails before restoring anything. If a
native I/O error happens after restoration starts, already restored entries stay
restored and the remaining entries stay in history for retry. Native restoration
also checks collisions, but a concurrent external process modifying restore paths
is outside the transaction; pop is not a filesystem-wide atomic operation.

History uses **SQLite** (`history.sqlite3`) through
[rusqlite](https://docs.rs/rusqlite/), with SQLite bundled into the executable.
Invocations, item receipts, and pending operations have separate tables. Native
trash IDs and original paths are stored as lossless BLOBs. Indexed inserts and
deletes update only affected rows; commands do not deserialize or rewrite the
entire history. Pop reads recent invocations in reverse order through an index
and skips receipts for items already restored or purged elsewhere. Stale receipts
are removed from the invocations visited during pop. Freed database pages are
reused by subsequent writes.
The native trash backend still enumerates system trash to identify receipts;
SQLite removes the separate cost of reading and rewriting the full history.

Transactions commit a pending receipt before each native trash operation, then
atomically replace it with the successful receipt. SQLite uses durable rollback
journaling with `synchronous=EXTRA`. Mutating `bin` processes sharing a history
directory also exclude one another with `history.lock` for the full filesystem
operation. The next invocation can recover an interrupted operation when its
native trash identity can be determined uniquely. Ambiguous recovery fails with
the pending history intact.

Filesystem changes outside `bin` are reconciled during pop. If a tracked trash
item was manually deleted, restored, or has different native receipt metadata,
its stale receipt is skipped and removed from the groups visited by pop. A group
with surviving items still counts as one invocation; completely missing groups
are skipped. Pop checks that enough restorable groups remain before restoring
anything. Dry runs preview this reconciliation without changing the database.

On FreeDesktop systems, a surviving `.trashinfo` file does not count as a
restorable item when its payload is missing. Emptying can still remove that
orphaned metadata. If metadata was deleted but its payload survives, the payload
is left untouched for manual recovery from the system trash. Deleted children
inside a trashed directory stay deleted; pop restores the directory's surviving
contents. Missing destination parents are recreated. Existing destination files
and parents replaced by files or symlinks cause pop to fail without overwriting.

If an interrupted operation has no source and no identifiable restorable trash
item left, `bin` reports and discards only its pending receipt so it cannot block
future commands. Inspection errors and ambiguous recovery preserve history for
retry. These cases reconcile tracking; permanently deleted file contents cannot
be reconstructed. An unreadable or corrupted SQLite database is preserved and
refused before trash operations; restore a database backup to recover its groups.

## Emptying and ages

`--empty` permanently deletes selected **top-level trash entries**, including
entries created by other applications. Trashed directories are deleted with all
their contents. A glob matches an entry's original **basename**, not its original
full path or individual files inside a trashed directory. Quote globs so the shell
does not expand them first. Patterns are case sensitive and support `*`, `?`,
character classes, and `{a,b}` alternatives.

At least one selector is required. `--older-than` and `--glob` combine with **AND**:
an entry must satisfy both. For example, `--empty --older-than '2mo' --glob '*.txt'`
deletes only `.txt` entries trashed more than two months ago.

For **OR**, run two separate commands:

```sh
bin --empty --older-than '2mo'
bin --empty --glob '*.txt'
```

These delete entries older than two months **or** matching `*.txt`.
`--all` cannot be combined with `--older-than` or `--glob`; the CLI rejects these
combinations. There is no implicit selection for bare `--empty`.

| Age unit | Meaning |
| --- | --- |
| `yr` | Calendar years (12 months each) |
| `mo` | Calendar months |
| `wk` | 7 days |
| `d` | 24 hours |
| `h` | 60 minutes |
| `min` | 60 seconds |
| `s` | Seconds |

Terms add together: `'1yr 2mo 3wk 4d 5h 6min 7s'`. Terms must be unsigned integers,
and the total age must be nonzero. Months and years are subtracted first in UTC,
clamping to the last valid day of the target month, followed by the fixed-length
units. Selection compares the native **deletion timestamp**, strictly before the
cutoff; it does not use file modification time. Successful cleanup also removes
matching receipts from pop history.

## Platforms and state

| Platform | Trash | Pop and filtered empty |
| --- | --- | --- |
| Linux / FreeDesktop trash | Yes | Yes |
| Windows Recycle Bin | Yes | Yes |
| macOS | Yes | Not exposed by the trash crate; commands report an explicit unsupported error |

The full workflow targets Linux and Windows. macOS trashing currently emits a
notice that undo history is unavailable. macOS pop/empty need a separate native
backend or upstream support; `bin` does not replace system trash with a private
directory to emulate them. Other Unix platforms use the fork's mount discovery;
BSD support is implemented but is not covered by the native CI matrix.
Configuration lives under the platform's user config directory (`%APPDATA%/bin`
on Windows, `~/Library/Application Support/bin` on macOS); `--config` overrides it.

The database normally lives at `$XDG_STATE_HOME/bin/history.sqlite3` (or
`~/.local/state/bin/history.sqlite3`) on Linux and in the local application data
directory under `bin` on Windows. Set `BIN_STATE_DIR` to use another history
directory. Keep it stable across commands if you want shared undo history.
Native filenames are stored losslessly, including non-UTF-8 paths on Unix.
Moving or deleting the database does not delete trashed data, but loses invocation
grouping. The database has a versioned schema; unknown versions are refused.

Existing `history.json` files are imported automatically in a single transaction
when the SQLite database is first initialized by a normal command. Invocation
order, grouping, and pending recovery records are preserved. The original JSON
is retained unchanged as a backup and is ignored after a successful import; it
can be removed once the migration is verified. Do not remove the SQLite database
while retaining this backup unless you intend to import the backup again.

Dry runs open an existing database read-only, never create the state directory or
database, and never change history. If only legacy JSON exists, its history is
loaded into an in-memory SQLite database for the preview without migrating it on
disk. Pending-operation recovery is also simulated in memory during a preview.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

Unit tests exercise the CLI and history with a filesystem-backed mock trash;
the fork's tests cover the storage primitives.
Linux integration tests exercise the fork through `bin` with isolated XDG data and
history directories; permanent cleanup tests select unique sandbox filenames
and never empty a global trash. Interoperability tests use the unmodified upstream
`trash` crate in a subprocess to inspect and restore entries written by `bin`,
and verify that `bin` can restore entries and existing SQLite receipts produced
by the old backend. Malformed metadata, orphaned payloads, binary and long names,
and private permissions are covered.
Cross-filesystem Linux tests use `/dev/shm` for a second filesystem and verify both
location policies, restore collisions, metadata preservation, and copy failures.

The `vendor/trash-rs` submodule is the normal `system_trash` path dependency;
the separate `trash` dev-dependency remains the unmodified published crate for
interoperability tests. Initialize the submodule before building or running checks.
The fork owns FreeDesktop storage, mount discovery, and transfer logic; `bin`
keeps configuration, CLI validation, and invocation history. Linux uses an atomic
no-replace rename for publication and restoration, and stages cross-filesystem
copies in a private destination directory
before publishing a complete payload. Interrupted copies can leave an unlisted
staging directory; their original source data is retained. Metadata uses standard
percent-encoded native paths and atomic receipt publication. Home receipts use
absolute original paths; per-filesystem receipts use paths relative to their
mount root, so they remain usable when a volume is mounted elsewhere. Linux discovery
reads `/proc/self/mountinfo` so bind mounts and escaped mount names are respected.
