use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Move files to the system trash; undo by invocation.",
    after_help = "Examples:\n  bin -rf build/\n  bin -p             Restore the last invocation\n  bin -p 3           Restore the last three invocations\n  bin --empty --older-than '2mo 3d' --glob '*.txt'\n  bin --empty --all --dry-run\n\nPop never overwrites, even with -f. Emptying permanently deletes matching trash entries."
)]
pub struct Cli {
    /// Ignore nonexistent operands and never prompt (does not allow overwriting on pop)
    #[arg(short, long, overrides_with_all = ["each", "once", "interactive"])]
    pub force: bool,
    /// Prompt before each operand
    #[arg(short = 'i', overrides_with_all = ["force", "once", "interactive"])]
    pub each: bool,
    /// Prompt once for more than three operands or recursive trashing
    #[arg(short = 'I', overrides_with_all = ["force", "each", "interactive"])]
    pub once: bool,
    /// Prompt never, once, or always; without WHEN, prompt always
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "always",
        value_name = "WHEN", overrides_with_all = ["force", "each", "once"])]
    pub interactive: Option<Interactive>,
    /// Trash directories and their contents as whole trees
    #[arg(short = 'r', short_alias = 'R', long, conflicts_with_all = ["pop", "empty"])]
    pub recursive: bool,
    /// Trash empty directories
    #[arg(short, long = "dir", conflicts_with_all = ["pop", "empty"])]
    pub dir: bool,
    /// Explain what is being done
    #[arg(short, long)]
    pub verbose: bool,
    /// Restore the last N nonempty bin invocations (default: 1)
    #[arg(short = 'p', long, num_args = 0..=1, default_missing_value = "1",
        value_parser = clap::value_parser!(u32).range(1..), value_name = "N", conflicts_with = "empty")]
    pub pop: Option<u32>,
    /// Permanently delete trash entries; requires --all, --older-than, or --glob
    #[arg(long)]
    pub empty: bool,
    /// Select every item in the system trash
    #[arg(long, requires = "empty", conflicts_with_all = ["older_than", "glob"])]
    pub all: bool,
    /// Select entries trashed before this age (yr, mo, wk, d, h, min, s)
    #[arg(long, requires = "empty", value_name = "AGE")]
    pub older_than: Option<crate::age::Age>,
    /// Select entries by original basename; quote the pattern to prevent shell expansion
    #[arg(long, requires = "empty", value_name = "PATTERN")]
    pub glob: Option<String>,
    /// Preview actions without changing files, trash, or history
    #[arg(long)]
    pub dry_run: bool,
    /// Refuse roots; 'all' also rejects operands on a different device from their parent
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "root",
        value_enum, conflicts_with_all = ["pop", "empty"])]
    pub preserve_root: Option<PreserveRoot>,
    /// Files and directories to trash; use -- before names beginning with '-'
    #[arg(value_name = "FILE", conflicts_with_all = ["pop", "empty"])]
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Interactive {
    Never,
    Once,
    Always,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PreserveRoot {
    Root,
    All,
}

impl Cli {
    pub fn interaction(&self) -> Option<Interactive> {
        if self.force {
            Some(Interactive::Never)
        } else if self.each {
            Some(Interactive::Always)
        } else if self.once {
            Some(Interactive::Once)
        } else {
            self.interactive
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rm_prompt_flags_use_last_option() {
        for (flags, expected) in [
            ("-fi", Interactive::Always),
            ("-if", Interactive::Never),
            ("-fI", Interactive::Once),
            ("-If", Interactive::Never),
        ] {
            let cli = Cli::try_parse_from(["bin", flags, "file"]).unwrap();
            assert_eq!(cli.interaction(), Some(expected));
        }
        let cli = Cli::try_parse_from(["bin", "-f", "--interactive=always", "file"]).unwrap();
        assert_eq!(cli.interaction(), Some(Interactive::Always));
    }

    #[test]
    fn modes_and_selectors_are_validated() {
        for args in [
            vec!["bin", "--all"],
            vec!["bin", "--empty", "-p"],
            vec!["bin", "-p", "0"],
            vec!["bin", "-p", "file"],
            vec!["bin", "--empty", "--all", "--glob", "*"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        assert_eq!(Cli::try_parse_from(["bin", "-p"]).unwrap().pop, Some(1));
        assert_eq!(
            Cli::try_parse_from(["bin", "-p", "3"]).unwrap().pop,
            Some(3)
        );
        assert!(Cli::try_parse_from(["bin", "-rf", "--", "-file"]).is_ok());
    }
}
