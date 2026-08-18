//! Layered, immutable configuration.
//!
//! A [`Config`] is a snapshot of a JSON tree assembled from ordered layers that are
//! deep-merged: later layers win, objects merge recursively, everything else replaces.
//!
//! The kernel deliberately does **not** read files. It accepts [`serde_json::Value`]
//! layers, so whoever builds the kernel decides whether configuration comes from TOML on
//! disk, a database, a UI or a test fixture. An environment-variable layer is built in
//! because environment variables are portable and universally available.
//!
//! ```
//! use aik_core::config::Config;
//! use serde_json::json;
//!
//! let config = Config::builder()
//!     .layer(json!({ "kernel": { "shutdown_timeout_ms": 5000 } }))
//!     .layer(json!({ "kernel": { "event_capacity": 512 } }))
//!     .build();
//!
//! assert_eq!(config.get::<u64>("kernel.shutdown_timeout_ms").unwrap(), 5000);
//! assert_eq!(config.get::<usize>("kernel.event_capacity").unwrap(), 512);
//! ```

use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// An immutable configuration snapshot.
///
/// Cloning is cheap: the underlying tree is shared.
#[derive(Debug, Clone)]
pub struct Config {
    root: Arc<Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self::empty()
    }
}

impl Config {
    /// Returns an empty configuration.
    pub fn empty() -> Self {
        Self {
            root: Arc::new(Value::Object(Map::new())),
        }
    }

    /// Starts building a configuration from layers.
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::new()
    }

    /// Wraps an already-assembled JSON tree.
    pub fn from_value(value: Value) -> Self {
        Self {
            root: Arc::new(value),
        }
    }

    /// Returns the whole tree.
    pub fn as_value(&self) -> &Value {
        &self.root
    }

    /// Looks up a raw value by dotted path.
    ///
    /// Path segments index objects by key and arrays by numeric index, e.g.
    /// `providers.0.name`. An empty path returns the whole tree.
    pub fn value(&self, path: &str) -> Option<&Value> {
        let mut current = self.root.as_ref();
        if path.is_empty() {
            return Some(current);
        }
        for segment in path.split('.') {
            current = match current {
                Value::Object(map) => map.get(segment)?,
                Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(current)
    }

    /// Returns true if something is present at `path`.
    pub fn contains(&self, path: &str) -> bool {
        self.value(path).is_some_and(|value| !value.is_null())
    }

    /// Deserialises the value at `path`, failing if it is absent.
    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        match self.get_optional(path)? {
            Some(value) => Ok(value),
            None => Err(Error::config(path, "required setting is missing")),
        }
    }

    /// Deserialises the value at `path`, returning `None` if it is absent or null.
    pub fn get_optional<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        let Some(value) = self.value(path) else {
            return Ok(None);
        };
        if value.is_null() {
            return Ok(None);
        }
        serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| Error::config(path, error.to_string()))
    }

    /// Deserialises the value at `path`, falling back to `T::default()` if absent.
    ///
    /// This is the usual way for a component to read its own settings: define a settings
    /// struct with `#[derive(Default, Deserialize)]` and let missing configuration mean
    /// "use the defaults".
    pub fn get_or_default<T: DeserializeOwned + Default>(&self, path: &str) -> Result<T> {
        Ok(self.get_optional(path)?.unwrap_or_default())
    }

    /// Returns the subtree at `path` as its own [`Config`].
    ///
    /// An absent path yields an empty configuration rather than an error, so callers can
    /// treat "no section" and "empty section" alike.
    pub fn section(&self, path: &str) -> Self {
        match self.value(path) {
            Some(value) => Self::from_value(value.clone()),
            None => Self::empty(),
        }
    }
}

/// Assembles a [`Config`] from ordered layers.
#[derive(Debug, Default)]
pub struct ConfigBuilder {
    root: Value,
}

impl ConfigBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self {
            root: Value::Object(Map::new()),
        }
    }

    /// Deep-merges a layer on top of everything added so far.
    ///
    /// Objects merge key by key; arrays, scalars and nulls replace wholesale.
    #[must_use]
    pub fn layer(mut self, layer: Value) -> Self {
        merge(&mut self.root, layer);
        self
    }

    /// Deep-merges a layer produced by serialising `value`.
    ///
    /// Fails if `value` does not serialise to JSON.
    pub fn typed_layer<T: Serialize>(self, value: &T) -> Result<Self> {
        Ok(self.layer(serde_json::to_value(value)?))
    }

    /// Sets a single dotted path, creating intermediate objects as needed.
    #[must_use]
    pub fn set(mut self, path: &str, value: impl Into<Value>) -> Self {
        set_path(&mut self.root, path, value.into());
        self
    }

    /// Merges a layer built from process environment variables.
    ///
    /// See [`ConfigBuilder::env_from`] for the naming convention.
    #[must_use]
    pub fn env(self, prefix: &str) -> Self {
        self.env_from(prefix, std::env::vars())
    }

    /// Merges a layer built from an arbitrary set of environment-style variables.
    ///
    /// Variables that start with `prefix` are stripped of it, lowercased, and split on
    /// `__` into a dotted path. Values that parse as JSON are used as-is, everything else
    /// is treated as a string. So with prefix `AIK_`:
    ///
    /// ```text
    /// AIK_KERNEL__EVENT_CAPACITY=512      → kernel.event_capacity = 512
    /// AIK_COMPONENTS__PLATFORM__ENABLED=true → components.platform.enabled = true
    /// AIK_LOG=debug                       → log = "debug"
    /// ```
    ///
    /// Taking the variables as an iterator keeps this testable without mutating process
    /// state.
    #[must_use]
    pub fn env_from<I, K, V>(mut self, prefix: &str, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        for (key, value) in vars {
            let Some(rest) = key.as_ref().strip_prefix(prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            let path = rest
                .split("__")
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
                .join(".");
            let raw = value.as_ref();
            let parsed = serde_json::from_str::<Value>(raw)
                .unwrap_or_else(|_| Value::String(raw.to_owned()));
            set_path(&mut self.root, &path, parsed);
        }
        self
    }

    /// Finalises the snapshot.
    pub fn build(self) -> Config {
        Config::from_value(self.root)
    }
}

fn merge(target: &mut Value, layer: Value) {
    match (target, layer) {
        (Value::Object(target), Value::Object(layer)) => {
            for (key, value) in layer {
                match target.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, layer) => *target = layer,
    }
}

fn set_path(root: &mut Value, path: &str, value: Value) {
    if path.is_empty() {
        *root = value;
        return;
    }
    let mut current = root;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        if !current.is_object() {
            *current = Value::Object(Map::new());
        }
        let map = current.as_object_mut().expect("just ensured object");
        if segments.peek().is_none() {
            map.insert(segment.to_owned(), value);
            return;
        }
        current = map.entry(segment.to_owned()).or_insert(Value::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn later_layers_win_and_objects_merge() {
        let config = Config::builder()
            .layer(json!({ "a": { "x": 1, "y": 2 }, "b": 1 }))
            .layer(json!({ "a": { "y": 20, "z": 30 } }))
            .build();

        assert_eq!(config.get::<i64>("a.x").unwrap(), 1);
        assert_eq!(config.get::<i64>("a.y").unwrap(), 20);
        assert_eq!(config.get::<i64>("a.z").unwrap(), 30);
        assert_eq!(config.get::<i64>("b").unwrap(), 1);
    }

    #[test]
    fn arrays_replace_rather_than_merge() {
        let config = Config::builder()
            .layer(json!({ "list": [1, 2, 3] }))
            .layer(json!({ "list": [9] }))
            .build();
        assert_eq!(config.get::<Vec<i64>>("list").unwrap(), vec![9]);
    }

    #[test]
    fn paths_index_into_arrays() {
        let config = Config::from_value(json!({ "providers": [{ "name": "local" }] }));
        assert_eq!(config.get::<String>("providers.0.name").unwrap(), "local");
        assert!(config.value("providers.7.name").is_none());
    }

    #[test]
    fn missing_required_settings_name_their_path() {
        let config = Config::empty();
        let error = config.get::<i64>("kernel.nope").unwrap_err();
        assert!(error.to_string().contains("kernel.nope"), "{error}");
    }

    #[test]
    fn absent_sections_deserialise_to_defaults() {
        #[derive(Debug, Default, Deserialize, PartialEq)]
        #[serde(default)]
        struct Settings {
            retries: u32,
        }

        let config = Config::empty();
        assert_eq!(
            config
                .get_or_default::<Settings>("components.demo")
                .unwrap(),
            Settings { retries: 0 }
        );
    }

    #[test]
    fn env_vars_become_nested_paths_with_typed_values() {
        let config = Config::builder()
            .env_from(
                "AIK_",
                [
                    ("AIK_KERNEL__EVENT_CAPACITY", "512"),
                    ("AIK_COMPONENTS__PLATFORM__ENABLED", "true"),
                    ("AIK_LOG", "debug"),
                    ("UNRELATED", "ignored"),
                ],
            )
            .build();

        assert_eq!(config.get::<usize>("kernel.event_capacity").unwrap(), 512);
        assert!(config.get::<bool>("components.platform.enabled").unwrap());
        assert_eq!(config.get::<String>("log").unwrap(), "debug");
        assert!(!config.contains("unrelated"));
    }

    #[test]
    fn env_layers_merge_over_earlier_layers() {
        let config = Config::builder()
            .layer(json!({ "kernel": { "event_capacity": 16, "shutdown_timeout_ms": 1000 } }))
            .env_from("AIK_", [("AIK_KERNEL__EVENT_CAPACITY", "512")])
            .build();

        assert_eq!(config.get::<usize>("kernel.event_capacity").unwrap(), 512);
        assert_eq!(
            config.get::<u64>("kernel.shutdown_timeout_ms").unwrap(),
            1000
        );
    }

    #[test]
    fn sections_are_configs_of_their_own() {
        let config = Config::from_value(json!({ "components": { "demo": { "retries": 3 } } }));
        let section = config.section("components.demo");
        assert_eq!(section.get::<u32>("retries").unwrap(), 3);
        assert!(config.section("components.missing").as_value().is_object());
    }
}
