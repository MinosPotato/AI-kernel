//! Dependency resolution for components.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::component::ComponentDescriptor;
use crate::error::{Error, Result};
use crate::id::ComponentId;

/// Orders components so that every component comes after its dependencies.
///
/// The order is deterministic: among components that could equally well go next, the one
/// with the lowest id wins. Reproducible startup order matters — a system that boots in a
/// different order each run is a system whose bugs cannot be reproduced.
///
/// Fails on a missing required dependency or a cycle. Missing *optional* dependencies are
/// ignored.
pub(crate) fn resolve_order(descriptors: &[ComponentDescriptor]) -> Result<Vec<ComponentId>> {
    let known: BTreeSet<ComponentId> = descriptors.iter().map(|d| d.id.clone()).collect();

    // Edges point from a dependency to the components that need it.
    let mut dependents: HashMap<ComponentId, Vec<ComponentId>> = HashMap::new();
    let mut remaining: BTreeMap<ComponentId, usize> = BTreeMap::new();

    for descriptor in descriptors {
        let mut count = 0;
        for dependency in &descriptor.dependencies {
            if !known.contains(&dependency.id) {
                if dependency.optional {
                    tracing::debug!(
                        component = %descriptor.id,
                        dependency = %dependency.id,
                        "optional dependency is not registered"
                    );
                    continue;
                }
                return Err(Error::MissingDependency {
                    component: descriptor.id.clone(),
                    dependency: dependency.id.clone(),
                });
            }
            if dependency.id == descriptor.id {
                return Err(Error::DependencyCycle(vec![descriptor.id.clone()]));
            }
            dependents
                .entry(dependency.id.clone())
                .or_default()
                .push(descriptor.id.clone());
            count += 1;
        }
        remaining.insert(descriptor.id.clone(), count);
    }

    let mut ready: BTreeSet<ComponentId> = remaining
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| id.clone())
        .collect();

    let mut ordered = Vec::with_capacity(descriptors.len());
    while let Some(next) = ready.iter().next().cloned() {
        ready.remove(&next);
        remaining.remove(&next);
        for dependent in dependents.get(&next).into_iter().flatten() {
            if let Some(count) = remaining.get_mut(dependent) {
                *count -= 1;
                if *count == 0 {
                    ready.insert(dependent.clone());
                }
            }
        }
        ordered.push(next);
    }

    if !remaining.is_empty() {
        return Err(Error::DependencyCycle(remaining.into_keys().collect()));
    }

    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(id: &str, requires: &[&str]) -> ComponentDescriptor {
        requires
            .iter()
            .fold(ComponentDescriptor::new(id), |acc, dep| acc.requires(*dep))
    }

    fn order(descriptors: &[ComponentDescriptor]) -> Vec<String> {
        resolve_order(descriptors)
            .unwrap()
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    #[test]
    fn dependencies_come_first() {
        let descriptors = [
            descriptor("app", &["db", "cache"]),
            descriptor("cache", &["config"]),
            descriptor("db", &["config"]),
            descriptor("config", &[]),
        ];
        assert_eq!(order(&descriptors), ["config", "cache", "db", "app"]);
    }

    #[test]
    fn order_is_deterministic_regardless_of_registration_order() {
        let forwards = [
            descriptor("a", &[]),
            descriptor("b", &[]),
            descriptor("c", &[]),
        ];
        let backwards = [
            descriptor("c", &[]),
            descriptor("b", &[]),
            descriptor("a", &[]),
        ];
        assert_eq!(order(&forwards), order(&backwards));
        assert_eq!(order(&forwards), ["a", "b", "c"]);
    }

    #[test]
    fn missing_required_dependencies_are_rejected() {
        let descriptors = [descriptor("app", &["ghost"])];
        let error = resolve_order(&descriptors).unwrap_err();
        assert!(matches!(error, Error::MissingDependency { .. }), "{error}");
    }

    #[test]
    fn missing_optional_dependencies_are_ignored() {
        let descriptors = [ComponentDescriptor::new("app").optionally_requires("ghost")];
        assert_eq!(order(&descriptors), ["app"]);
    }

    #[test]
    fn present_optional_dependencies_still_order() {
        let descriptors = [
            ComponentDescriptor::new("app").optionally_requires("zzz"),
            ComponentDescriptor::new("zzz"),
        ];
        assert_eq!(order(&descriptors), ["zzz", "app"]);
    }

    #[test]
    fn cycles_are_rejected() {
        let descriptors = [descriptor("a", &["b"]), descriptor("b", &["a"])];
        let error = resolve_order(&descriptors).unwrap_err();
        match error {
            Error::DependencyCycle(members) => {
                assert_eq!(members.len(), 2);
            }
            other => panic!("expected a cycle, got {other}"),
        }
    }

    #[test]
    fn self_dependency_is_a_cycle() {
        let descriptors = [descriptor("a", &["a"])];
        let error = resolve_order(&descriptors).unwrap_err();
        assert!(matches!(error, Error::DependencyCycle(_)), "{error}");
    }
}
