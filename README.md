# bin

`bin` is an `rm`-style Rust CLI that moves files into the operating system's trash.
It adds undo by **command invocation**, filtered permanent cleanup, and previews
for every operation. Native trash operations use the [trash crate](https://docs.rs/trash/).

## Install

```sh
cargo install --path . --locked
```

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

History writes are atomic and synced. Mutating `bin` processes sharing a history
directory exclude one another with a lock. A pending receipt is written before
each native trash operation, allowing the next invocation to recover an
interrupted operation when its native trash identity can be determined uniquely.
Ambiguous recovery fails with the pending history intact.

## Emptying and ages

`--empty` permanently deletes selected **top-level trash entries**, including
entries created by other applications. Trashed directories are deleted with all
their contents. A glob matches an entry's original **basename**, not its original
full path or individual files inside a trashed directory. Quote globs so the shell
does not expand them first. Patterns are case sensitive and support `*`, `?`,
character classes, and `{a,b}` alternatives.

At least one selector is required. `--older-than` and `--glob` combine with **AND**:
an entry must satisfy both. `--all` is exclusive with filters to avoid silently
broadening a selection. There is no implicit selection for bare `--empty`.

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
directory to emulate them. Other Unix platforms depend on `trash`'s mount support.

History normally lives in `$XDG_STATE_HOME/bin` (or `~/.local/state/bin`) on Linux
and the local application data directory on Windows. Set `BIN_STATE_DIR` to use
another history directory. Keep it stable across commands if you want shared
undo history. Native filenames are stored losslessly, including non-UTF-8 paths
on Unix. Moving or deleting the history file does not delete trashed data, but
loses invocation grouping. Dry runs never create the state directory.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

Unit tests use a filesystem-backed mock trash. Linux integration tests exercise
the actual `trash` crate with isolated XDG data and history directories; permanent
cleanup tests select unique sandbox filenames and never empty a global trash.
