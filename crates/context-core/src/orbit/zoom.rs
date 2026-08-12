use crate::atlas::AtlasHierarchy;
use creature_context_types::{AtlasEntity, EntityId, OrbitScale, ScopeScale};
use std::collections::BTreeSet;

pub fn scope_for_orbit(scale: OrbitScale) -> Option<ScopeScale> {
    match scale {
        OrbitScale::Universe => Some(ScopeScale::Universe),
        OrbitScale::Galaxy => Some(ScopeScale::Galaxy),
        OrbitScale::System => Some(ScopeScale::System),
        OrbitScale::Planet => Some(ScopeScale::Planet),
        OrbitScale::Moon => Some(ScopeScale::Moon),
        OrbitScale::Adaptive => None,
    }
}

pub fn zoom_roots(
    hierarchy: &AtlasHierarchy,
    starts: &[EntityId],
    scale: OrbitScale,
) -> Vec<EntityId> {
    let Some(target) = scope_for_orbit(scale) else {
        return starts.to_vec();
    };
    let mut result = BTreeSet::new();
    for start in starts {
        let Some(entity) = hierarchy.entity(*start) else {
            continue;
        };
        if entity.scale == target {
            result.insert(entity.id);
            continue;
        }
        if entity.scale.rank() > target.rank() {
            if let Some(ancestor) = hierarchy
                .ancestors_of(entity.id)
                .into_iter()
                .find(|e| e.scale == target)
            {
                result.insert(ancestor.id);
            }
        } else {
            for descendant in hierarchy
                .descendants_of(entity.id)
                .into_iter()
                .filter(|e| e.scale == target)
            {
                result.insert(descendant.id);
            }
        }
    }
    result.into_iter().collect()
}

pub fn immediate_scale_contents(hierarchy: &AtlasHierarchy, root: EntityId) -> Vec<AtlasEntity> {
    let Some(entity) = hierarchy.entity(root) else {
        return Vec::new();
    };
    match entity.scale {
        ScopeScale::Universe | ScopeScale::Galaxy | ScopeScale::System => {
            hierarchy.children_of(root).into_iter().cloned().collect()
        }
        ScopeScale::Planet => hierarchy
            .descendants_of(root)
            .into_iter()
            .filter(|e| e.scale == ScopeScale::Moon)
            .cloned()
            .collect(),
        ScopeScale::Moon => Vec::new(),
    }
}
