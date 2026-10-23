use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TrashLocation {
    #[default]
    Home,
    PerFilesystem,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub trash_location: TrashLocation,
}

impl Config {
    pub fn default_path() -> Result<PathBuf> {
        let dirs = directories::BaseDirs::new()
            .context("cannot determine the config directory; use --config FILE")?;
        Ok(dirs.config_dir().join("bin/config.toml"))
    }

    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => Self::default_path()?,
        };
        Self::read(&path, explicit.is_some())
    }

    fn read(path: &Path, required: bool) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read config {path:?}"));
            }
        };
        toml::from_str(&text).with_context(|| format!("invalid config {path:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_optional_config_defaults_to_home_without_creating_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent/config.toml");
        assert_eq!(
            Config::read(&path, false).unwrap().trash_location,
            TrashLocation::Home
        );
        assert!(!path.parent().unwrap().exists());
        assert!(Config::read(&path, true).is_err());
    }

    #[test]
    fn valid_policies_and_invalid_config_are_explicit() {
        for (text, expected) in [
            ("", TrashLocation::Home),
            ("trash-location = 'home'", TrashLocation::Home),
            (
                "trash-location = 'per-filesystem'",
                TrashLocation::PerFilesystem,
            ),
        ] {
            assert_eq!(
                toml::from_str::<Config>(text).unwrap().trash_location,
                expected
            );
        }
        for text in [
            "trash-location = 'unknown'",
            "trash_location = 'home'",
            "unexpected = true",
            "trash-location = true",
            "[broken",
        ] {
            assert!(toml::from_str::<Config>(text).is_err(), "{text}");
        }
    }
}
