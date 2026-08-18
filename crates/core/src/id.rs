//! Strongly typed identifiers.
//!
//! The kernel distinguishes two kinds of identity:
//!
//! * **String identifiers** ([`ComponentId`], [`PluginId`], [`EventName`]) are stable and
//!   human-authored. They appear in configuration files and dependency declarations, so
//!   they must be readable and must not change between runs. They are backed by
//!   [`Arc<str>`](std::sync::Arc), which makes cloning free.
//! * **UUID identifiers** ([`EventId`], [`TaskId`], [`CorrelationId`]) are generated at
//!   runtime. They use UUID version 7, so they sort by creation time.
//!
//! Both families are produced by macros, so declaring a new identifier type downstream is
//! a single line:
//!
//! ```
//! aik_core::string_id! {
//!     /// Identifies a model provider.
//!     ProviderId
//! }
//!
//! aik_core::uuid_id! {
//!     /// Identifies one inference request.
//!     RequestId
//! }
//!
//! let provider = ProviderId::new("anthropic");
//! assert_eq!(provider.as_str(), "anthropic");
//! assert_ne!(RequestId::new(), RequestId::new());
//! ```
//!
//! The point of the macros is not the boilerplate they save but the type safety they buy:
//! a `ComponentId` can never be passed where a `PluginId` is expected.

/// Declares a newtype over an interned string identifier.
///
/// The generated type implements `Clone`, `Debug`, `Display`, `PartialEq`, `Eq`, `Hash`,
/// `Ord`, `FromStr`, `From<&str>`, `From<String>`, `AsRef<str>`, `Borrow<str>` and serde
/// `Serialize`/`Deserialize` (as a plain string).
///
/// `Borrow<str>` means a `HashMap` keyed by the identifier can be looked up with a `&str`
/// without allocating.
#[macro_export]
macro_rules! string_id {
    ($(#[$meta:meta])* $vis:vis $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis struct $name(::std::sync::Arc<str>);

        impl $name {
            /// Creates an identifier from anything string-like.
            pub fn new(value: impl AsRef<str>) -> Self {
                Self(::std::sync::Arc::from(value.as_ref()))
            }

            /// Borrows the identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, concat!(stringify!($name), "({:?})"), &*self.0)
            }
        }

        impl ::std::convert::From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl ::std::convert::From<::std::string::String> for $name {
            fn from(value: ::std::string::String) -> Self {
                Self::new(value)
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = ::std::convert::Infallible;

            fn from_str(value: &str) -> ::std::result::Result<Self, Self::Err> {
                ::std::result::Result::Ok(Self::new(value))
            }
        }

        impl ::std::convert::AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl ::std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl $crate::__private::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> ::std::result::Result<S::Ok, S::Error>
            where
                S: $crate::__private::serde::Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> $crate::__private::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> ::std::result::Result<Self, D::Error>
            where
                D: $crate::__private::serde::Deserializer<'de>,
            {
                let raw = <::std::string::String as $crate::__private::serde::Deserialize>::deserialize(
                    deserializer,
                )?;
                ::std::result::Result::Ok(Self::new(raw))
            }
        }
    };
}

/// Declares a newtype over a generated UUID (version 7, time-ordered).
///
/// The generated type implements `Clone`, `Copy`, `Debug`, `Display`, `PartialEq`, `Eq`,
/// `Hash`, `Ord`, `Default` (a fresh id), `FromStr` and serde `Serialize`/`Deserialize`.
#[macro_export]
macro_rules! uuid_id {
    ($(#[$meta:meta])* $vis:vis $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis struct $name($crate::__private::uuid::Uuid);

        impl $name {
            /// Generates a new, time-ordered identifier.
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self($crate::__private::uuid::Uuid::now_v7())
            }

            /// Wraps an existing UUID.
            pub const fn from_uuid(value: $crate::__private::uuid::Uuid) -> Self {
                Self(value)
            }

            /// Returns the underlying UUID.
            pub const fn as_uuid(&self) -> &$crate::__private::uuid::Uuid {
                &self.0
            }
        }

        impl ::std::default::Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                ::std::fmt::Display::fmt(&self.0, f)
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), &self.0)
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::__private::uuid::Error;

            fn from_str(value: &str) -> ::std::result::Result<Self, Self::Err> {
                ::std::result::Result::Ok(Self($crate::__private::uuid::Uuid::parse_str(value)?))
            }
        }

        impl $crate::__private::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> ::std::result::Result<S::Ok, S::Error>
            where
                S: $crate::__private::serde::Serializer,
            {
                $crate::__private::serde::Serialize::serialize(&self.0, serializer)
            }
        }

        impl<'de> $crate::__private::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> ::std::result::Result<Self, D::Error>
            where
                D: $crate::__private::serde::Deserializer<'de>,
            {
                ::std::result::Result::Ok(Self(
                    <$crate::__private::uuid::Uuid as $crate::__private::serde::Deserialize>::deserialize(
                        deserializer,
                    )?,
                ))
            }
        }
    };
}

string_id! {
    /// Names a lifecycle-managed component, and the services it publishes.
    ///
    /// Component identifiers appear in dependency declarations and in configuration under
    /// `components.<id>`, so they should be stable, lowercase and dotted, e.g.
    /// `platform.hyprland`.
    pub ComponentId
}

string_id! {
    /// Names a plugin.
    pub PluginId
}

string_id! {
    /// The stable wire name of an event type, e.g. `kernel.state_changed`.
    pub EventName
}

uuid_id! {
    /// Uniquely identifies one published event.
    pub EventId
}

uuid_id! {
    /// Uniquely identifies one spawned background task.
    pub TaskId
}

uuid_id! {
    /// Ties together everything that happens in service of one logical operation.
    ///
    /// Propagating a correlation id across events, tool calls and model requests is what
    /// makes a distributed AI system traceable. The kernel carries it on event envelopes;
    /// subsystems are expected to pass it along.
    pub CorrelationId
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn string_ids_are_cheap_to_clone_and_compare() {
        let a = ComponentId::new("platform.hyprland");
        let b = a.clone();
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "platform.hyprland");
        assert_eq!(a.to_string(), "platform.hyprland");
    }

    #[test]
    fn string_ids_can_be_looked_up_by_str() {
        let mut map = HashMap::new();
        map.insert(ComponentId::new("a"), 1);
        assert_eq!(map.get("a"), Some(&1));
    }

    #[test]
    fn string_ids_round_trip_as_plain_strings() {
        let id = ComponentId::new("events.bus");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"events.bus\"");
        assert_eq!(serde_json::from_str::<ComponentId>(&json).unwrap(), id);
    }

    #[test]
    fn uuid_ids_are_unique_and_ordered() {
        let first = EventId::new();
        let second = EventId::new();
        assert_ne!(first, second);
        // Version 7 UUIDs embed a timestamp, so later ids sort after earlier ones.
        assert!(first < second);
    }

    #[test]
    fn uuid_ids_round_trip() {
        let id = TaskId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<TaskId>(&json).unwrap(), id);
        assert_eq!(json.trim_matches('"').parse::<TaskId>().unwrap(), id);
    }
}
