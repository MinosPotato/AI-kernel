//! The service registry: the kernel's dependency-injection container.
//!
//! Services are stored by *capability* (a trait object type) and *name* (a
//! [`ComponentId`]), and retrieved the same way. Callers therefore depend on traits, never
//! on concrete types, which is what makes every future subsystem — model providers, memory
//! stores, platform backends — replaceable without touching the kernel.
//!
//! ```
//! use std::sync::Arc;
//! use aik_core::{ComponentId, registry::Registry};
//!
//! trait Greeter: Send + Sync {
//!     fn greet(&self) -> String;
//! }
//!
//! struct Polite;
//! impl Greeter for Polite {
//!     fn greet(&self) -> String { "hello".into() }
//! }
//!
//! let registry = Registry::new();
//! registry.register::<dyn Greeter>(ComponentId::new("polite"), Arc::new(Polite)).unwrap();
//!
//! // Resolved by capability, not by concrete type.
//! let greeter: Arc<dyn Greeter> = registry.resolve::<dyn Greeter>().unwrap();
//! assert_eq!(greeter.greet(), "hello");
//! ```
//!
//! The registry is thread-safe and internally mutable, so components can publish services
//! during `init` while holding only a shared reference to the context.

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::{Error, Result};
use crate::id::ComponentId;

/// A registered service, type-erased.
///
/// The stored value is an `Arc<T>` boxed inside an `Arc<dyn Any>`. `Arc<dyn Trait>` cannot
/// itself be coerced to `Arc<dyn Any>` (trait objects are not `Sized`), so the handle is
/// wrapped one level deeper and downcast back on retrieval.
type Erased = Arc<dyn Any + Send + Sync>;

#[derive(Default)]
struct RegistryState {
    services: HashMap<TypeId, HashMap<ComponentId, Erased>>,
    defaults: HashMap<TypeId, ComponentId>,
}

/// A thread-safe map from `(capability, name)` to service handle.
#[derive(Default)]
pub struct Registry {
    state: RwLock<RegistryState>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.read().expect("registry lock poisoned");
        f.debug_struct("Registry")
            .field("capabilities", &state.services.len())
            .field(
                "services",
                &state.services.values().map(HashMap::len).sum::<usize>(),
            )
            .finish()
    }
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `service` under the capability `T` and the name `id`.
    ///
    /// `T` is normally an unsized trait object type, written explicitly:
    /// `register::<dyn ModelProvider>(id, provider)`.
    ///
    /// Fails with [`Error::AlreadyExists`] if that pair is already taken; use
    /// [`Registry::replace`] to override deliberately.
    pub fn register<T>(&self, id: ComponentId, service: Arc<T>) -> Result<()>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let mut state = self.state.write().expect("registry lock poisoned");
        let slot = state.services.entry(TypeId::of::<T>()).or_default();
        if slot.contains_key(&id) {
            return Err(Error::already_exists(
                "service",
                format!("{}/{id}", type_name::<T>()),
            ));
        }
        slot.insert(id, Arc::new(service));
        Ok(())
    }

    /// Registers `service` and makes it the default for the capability `T`.
    pub fn register_default<T>(&self, id: ComponentId, service: Arc<T>) -> Result<()>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.register::<T>(id.clone(), service)?;
        self.set_default::<T>(&id)
    }

    /// Registers `service`, replacing any existing entry for that capability and name.
    ///
    /// Returns whether an entry was replaced.
    pub fn replace<T>(&self, id: ComponentId, service: Arc<T>) -> bool
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let mut state = self.state.write().expect("registry lock poisoned");
        state
            .services
            .entry(TypeId::of::<T>())
            .or_default()
            .insert(id, Arc::new(service))
            .is_some()
    }

    /// Marks an already-registered service as the default for the capability `T`.
    pub fn set_default<T>(&self, id: &ComponentId) -> Result<()>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let mut state = self.state.write().expect("registry lock poisoned");
        let exists = state
            .services
            .get(&TypeId::of::<T>())
            .is_some_and(|slot| slot.contains_key(id));
        if !exists {
            return Err(Error::not_found(
                "service",
                format!("{}/{id}", type_name::<T>()),
            ));
        }
        state.defaults.insert(TypeId::of::<T>(), id.clone());
        Ok(())
    }

    /// Looks up a service by capability and name.
    pub fn try_get<T>(&self, id: &ComponentId) -> Option<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let state = self.state.read().expect("registry lock poisoned");
        let erased = state.services.get(&TypeId::of::<T>())?.get(id)?;
        erased.downcast_ref::<Arc<T>>().cloned()
    }

    /// Looks up a service by capability and name, failing if it is absent.
    pub fn get<T>(&self, id: &ComponentId) -> Result<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.try_get::<T>(id).ok_or_else(|| {
            Error::not_found("service", format!("{}/{id}", type_name::<T>()))
        })
    }

    /// Resolves the single service for a capability.
    ///
    /// Returns the explicit default if one was set, otherwise the only registered service.
    /// Fails with [`Error::Ambiguous`] if several are registered and none is the default,
    /// which forces the ambiguity to be resolved in configuration rather than silently.
    pub fn resolve<T>(&self) -> Result<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let state = self.state.read().expect("registry lock poisoned");
        let Some(slot) = state.services.get(&TypeId::of::<T>()) else {
            return Err(Error::not_found("service", type_name::<T>()));
        };

        let erased = match state.defaults.get(&TypeId::of::<T>()) {
            Some(default) => slot
                .get(default)
                .ok_or_else(|| Error::not_found("service", format!("{}/{default}", type_name::<T>())))?,
            None => match slot.len() {
                0 => return Err(Error::not_found("service", type_name::<T>())),
                1 => slot.values().next().expect("length checked"),
                _ => {
                    let mut candidates: Vec<String> =
                        slot.keys().map(ToString::to_string).collect();
                    candidates.sort();
                    return Err(Error::Ambiguous {
                        service: type_name::<T>(),
                        candidates,
                    });
                }
            },
        };

        erased
            .downcast_ref::<Arc<T>>()
            .cloned()
            .ok_or_else(|| Error::other(format!("service `{}` has the wrong type", type_name::<T>())))
    }

    /// Returns every service registered for a capability, sorted by name.
    ///
    /// This is the discovery mechanism: a tool catalogue or model router enumerates its
    /// providers here without knowing which ones were compiled in.
    pub fn list<T>(&self) -> Vec<(ComponentId, Arc<T>)>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let state = self.state.read().expect("registry lock poisoned");
        let Some(slot) = state.services.get(&TypeId::of::<T>()) else {
            return Vec::new();
        };
        let mut found: Vec<(ComponentId, Arc<T>)> = slot
            .iter()
            .filter_map(|(id, erased)| {
                erased.downcast_ref::<Arc<T>>().map(|service| (id.clone(), service.clone()))
            })
            .collect();
        found.sort_by(|(left, _), (right, _)| left.cmp(right));
        found
    }

    /// Returns the names registered for a capability, sorted.
    pub fn names<T>(&self) -> Vec<ComponentId>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let state = self.state.read().expect("registry lock poisoned");
        let mut names: Vec<ComponentId> = state
            .services
            .get(&TypeId::of::<T>())
            .map(|slot| slot.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Removes a service, returning whether it was present.
    ///
    /// Also clears the default for that capability if it pointed at the removed entry.
    pub fn remove<T>(&self, id: &ComponentId) -> bool
    where
        T: ?Sized + Send + Sync + 'static,
    {
        let mut state = self.state.write().expect("registry lock poisoned");
        let removed = state
            .services
            .get_mut(&TypeId::of::<T>())
            .is_some_and(|slot| slot.remove(id).is_some());
        if removed && state.defaults.get(&TypeId::of::<T>()) == Some(id) {
            state.defaults.remove(&TypeId::of::<T>());
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    trait Greeter: Send + Sync + std::fmt::Debug {
        fn greet(&self) -> String;
    }

    #[derive(Debug)]
    struct Named(&'static str);

    impl Greeter for Named {
        fn greet(&self) -> String {
            self.0.to_owned()
        }
    }

    trait Other: Send + Sync {}

    #[test]
    fn a_sole_service_resolves_without_a_default() {
        let registry = Registry::new();
        registry
            .register::<dyn Greeter>(ComponentId::new("a"), Arc::new(Named("a")))
            .unwrap();
        assert_eq!(registry.resolve::<dyn Greeter>().unwrap().greet(), "a");
    }

    #[test]
    fn several_services_require_an_explicit_default() {
        let registry = Registry::new();
        registry
            .register::<dyn Greeter>(ComponentId::new("a"), Arc::new(Named("a")))
            .unwrap();
        registry
            .register::<dyn Greeter>(ComponentId::new("b"), Arc::new(Named("b")))
            .unwrap();

        let error = registry.resolve::<dyn Greeter>().unwrap_err();
        assert!(matches!(error, Error::Ambiguous { .. }), "{error}");

        registry.set_default::<dyn Greeter>(&ComponentId::new("b")).unwrap();
        assert_eq!(registry.resolve::<dyn Greeter>().unwrap().greet(), "b");
    }

    #[test]
    fn capabilities_are_isolated_from_one_another() {
        let registry = Registry::new();
        registry
            .register::<dyn Greeter>(ComponentId::new("a"), Arc::new(Named("a")))
            .unwrap();
        assert!(registry.resolve::<dyn Other>().is_err());
        assert!(registry.names::<dyn Other>().is_empty());
    }

    #[test]
    fn duplicate_registration_is_rejected_but_replacement_is_allowed() {
        let registry = Registry::new();
        let id = ComponentId::new("a");
        registry.register::<dyn Greeter>(id.clone(), Arc::new(Named("first"))).unwrap();

        let error = registry
            .register::<dyn Greeter>(id.clone(), Arc::new(Named("second")))
            .unwrap_err();
        assert!(matches!(error, Error::AlreadyExists { .. }), "{error}");

        assert!(registry.replace::<dyn Greeter>(id.clone(), Arc::new(Named("second"))));
        assert_eq!(registry.get::<dyn Greeter>(&id).unwrap().greet(), "second");
    }

    #[test]
    fn listing_is_sorted_and_removal_clears_the_default() {
        let registry = Registry::new();
        registry
            .register::<dyn Greeter>(ComponentId::new("z"), Arc::new(Named("z")))
            .unwrap();
        registry
            .register_default::<dyn Greeter>(ComponentId::new("a"), Arc::new(Named("a")))
            .unwrap();

        let names: Vec<String> = registry
            .list::<dyn Greeter>()
            .into_iter()
            .map(|(id, _)| id.to_string())
            .collect();
        assert_eq!(names, ["a", "z"]);

        assert!(registry.remove::<dyn Greeter>(&ComponentId::new("a")));
        // The default is gone, and only one candidate remains, so resolution still works.
        assert_eq!(registry.resolve::<dyn Greeter>().unwrap().greet(), "z");
    }

    #[test]
    fn setting_a_default_for_an_unregistered_name_fails() {
        let registry = Registry::new();
        let error = registry.set_default::<dyn Greeter>(&ComponentId::new("ghost")).unwrap_err();
        assert!(matches!(error, Error::NotFound { .. }), "{error}");
    }
}
