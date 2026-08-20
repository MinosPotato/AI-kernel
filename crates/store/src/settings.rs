//! Configuration for the store, and where its database file lives by default.

use std::path::PathBuf;

use aik_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// The file name used under the resolved data directory when no path is configured.
pub const DEFAULT_FILE_NAME: &str = "aik.redb";

/// The directory created under the resolved data root when no path is configured.
pub const DEFAULT_DIRECTORY_NAME: &str = "aik";

/// Settings read from a component's configuration section.
///
/// With no configuration at all, the database lands in the XDG data directory — see
/// [`default_path`] — which is the only default that does not leak conversation transcripts
/// into whatever directory the process happened to start in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreSettings {
    /// Where the database file lives.
    ///
    /// Absent means [`default_path`]. A relative path is resolved against the process's
    /// working directory, which is almost never what an operator wants; prefer an absolute
    /// path when setting this explicitly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

impl StoreSettings {
    /// Resolves the database path, consulting the process environment for the default.
    pub fn resolve_path(&self) -> Result<PathBuf> {
        self.resolve_path_from(std::env::vars())
    }

    /// As [`StoreSettings::resolve_path`], with the environment supplied explicitly.
    ///
    /// The explicit-environment variant is what tests use, matching how `aik-cli` resolves
    /// its own settings in `Settings::resolve_from`; reading the real environment inside a
    /// test would make the result depend on the machine running it.
    pub fn resolve_path_from<I, K, V>(&self, vars: I) -> Result<PathBuf>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        match &self.path {
            Some(path) => Ok(path.clone()),
            None => default_path(vars),
        }
    }
}

/// The database path to use when none is configured: `$XDG_DATA_HOME/aik/aik.redb`,
/// falling back to `$HOME/.local/share/aik/aik.redb`.
///
/// # Why this fails rather than picking something
///
/// With neither variable set there is no defensible default. The working directory would
/// put a file holding every conversation transcript wherever the process was launched from,
/// somewhere an operator is not expecting it and a backup or a repository might pick it up;
/// a temporary directory would silently lose data on reboot. Both are worse than refusing
/// to start, so this returns [`Error::Config`] and the operator sets the path explicitly.
///
/// Empty variables are treated as unset, which is how the XDG base directory specification
/// defines them, and a relative `XDG_DATA_HOME` is ignored for the same reason — the
/// specification requires it be absolute.
pub fn default_path<I, K, V>(vars: I) -> Result<PathBuf>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut xdg_data_home = None;
    let mut home = None;
    for (key, value) in vars {
        let value = value.as_ref();
        if value.is_empty() {
            continue;
        }
        match key.as_ref() {
            "XDG_DATA_HOME" => xdg_data_home = Some(PathBuf::from(value)),
            "HOME" => home = Some(PathBuf::from(value)),
            _ => {}
        }
    }

    let root = xdg_data_home
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| home.join(".local").join("share")))
        .ok_or_else(|| {
            Error::config(
                "components.store.db.path",
                "neither XDG_DATA_HOME nor HOME is set, so there is no default location for \
                 the database; set the path explicitly",
            )
        })?;

    Ok(root.join(DEFAULT_DIRECTORY_NAME).join(DEFAULT_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn an_explicit_path_wins_over_the_environment() {
        let settings = StoreSettings {
            path: Some(PathBuf::from("/srv/aik/custom.redb")),
        };
        let resolved = settings
            .resolve_path_from(env(&[("XDG_DATA_HOME", "/home/someone/.local/share")]))
            .unwrap();
        assert_eq!(resolved, PathBuf::from("/srv/aik/custom.redb"));
    }

    #[test]
    fn xdg_data_home_is_preferred() {
        let path = default_path(env(&[
            ("XDG_DATA_HOME", "/home/someone/data"),
            ("HOME", "/home/someone"),
        ]))
        .unwrap();
        assert_eq!(path, PathBuf::from("/home/someone/data/aik/aik.redb"));
    }

    #[test]
    fn home_supplies_the_documented_xdg_fallback() {
        let path = default_path(env(&[("HOME", "/home/someone")])).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/someone/.local/share/aik/aik.redb")
        );
    }

    #[test]
    fn empty_variables_count_as_unset() {
        let path = default_path(env(&[("XDG_DATA_HOME", ""), ("HOME", "/home/someone")])).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/someone/.local/share/aik/aik.redb")
        );
    }

    #[test]
    fn a_relative_xdg_data_home_is_ignored() {
        let path = default_path(env(&[
            ("XDG_DATA_HOME", "relative/data"),
            ("HOME", "/home/someone"),
        ]))
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/someone/.local/share/aik/aik.redb")
        );
    }

    #[test]
    fn with_nothing_to_go_on_it_refuses_rather_than_guessing() {
        let error = default_path(env(&[("PATH", "/usr/bin")])).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(error.to_string().contains("set the path explicitly"));
    }

    #[test]
    fn absent_configuration_deserialises_to_the_default() {
        let settings: StoreSettings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(settings, StoreSettings::default());
        assert!(settings.path.is_none());
    }

    #[test]
    fn an_unknown_setting_is_rejected_rather_than_ignored() {
        let error =
            serde_json::from_value::<StoreSettings>(serde_json::json!({ "pathh": "/tmp/x" }))
                .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }
}
