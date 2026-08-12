//! The shared deterministic index pipeline.
//!
//! One function performs the whole deterministic index of a project: scan,
//! enrich with parsed structure, reconcile identity against the previous
//! snapshot, and evaluate Green. Both entry points — the one-shot CLI `scan` and
//! the resident daemon — call it, so a scanned Atlas and a watched Atlas are
//! identical. It lives here because enrichment is the parser-specific step and
//! this crate already bridges the scanner (core) to it; the remaining steps are
//! core operations this crate can see.

use creature_context_core::{
    atlas::HierarchyError,
    green::evaluate_snapshot,
    identity::reconcile_identity,
    scan::{ScanError, scan_project_configured},
};
use creature_context_types::{AtlasSnapshot, GreenPolicy};
use std::path::Path;

/// Anything that can stop the pipeline before a snapshot is produced. Enrichment
/// and identity reconciliation are infallible (an unparseable file degrades to
/// its deterministic entity), so only the scan and the Green evaluation fail.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Hierarchy(#[from] HierarchyError),
}

/// Index `root` into a fully enriched, identity-reconciled, Green-evaluated
/// snapshot. `previous`, when given, is the prior snapshot to carry stable ids
/// forward from (spec §3, §6); the first index has none. The caller stores the
/// result and writes its projections.
pub fn index_project(
    root: &Path,
    previous: Option<&AtlasSnapshot>,
) -> Result<AtlasSnapshot, IndexError> {
    // A one-shot index has nothing to reuse: every file is parsed.
    let mut cache = crate::incremental::ParseCache::new();
    index_project_cached(root, previous, &mut cache)
}

/// As `index_project`, but reusing `cache` for files whose content has not
/// changed since a previous index. The result is identical — only the work done
/// to reach it differs — so the resident daemon and the one-shot CLI still
/// produce the same Atlas (spec §7.1). The daemon owns the cache across
/// reconciliations; the caller keeps it alive for as long as it wants the reuse.
pub fn index_project_cached(
    root: &Path,
    previous: Option<&AtlasSnapshot>,
    cache: &mut crate::incremental::ParseCache,
) -> Result<AtlasSnapshot, IndexError> {
    let mut snapshot = scan_project_configured(root)?;
    // Enrich with parsed structure: symbols as Moon entities, observed contains
    // edges, and provides/requires sockets.
    crate::enrich::enrich_snapshot_cached(root, &mut snapshot, cache);
    // Carry stable ids across the rescan before Green is computed, so a moved or
    // renamed entity keeps its assessment's identity.
    if let Some(previous) = previous {
        reconcile_identity(previous, &mut snapshot);
    }
    // Evaluate over the enriched structure, so symbols carry assessments and
    // sockets and contradictions darken the axes they belong to.
    evaluate_snapshot(&mut snapshot, &GreenPolicy::default())?;
    Ok(snapshot)
}
