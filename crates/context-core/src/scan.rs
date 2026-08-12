use crate::{
    green::evaluate_snapshot,
    project::{ProjectPaths, atomic_write, init_project},
    purpose::read_purpose,
};
use creature_context_types::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

const SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    ".creature",
    "target",
    ".build",
    ".release-build",
    "node_modules",
    ".swiftpm",
    "dist",
    "build",
    ".cache",
    "vendor",
];

pub use crate::config::{ScanConfig, ScanLimits, ScanScope};

#[derive(Debug, Error)]
pub enum ScanError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("hierarchy error: {0}")]
    Hierarchy(String),
}

/// How a walk ended: what it collected, and whether a ceiling cut it short.
///
/// Truncation is data, not an error. A scan that stopped at a limit still
/// produced a real, usable Atlas of what it did see — failing the whole scan
/// instead would leave the project with nothing and, in the daemon, kill the
/// process. What must never happen is a truncated Atlas that *looks* complete,
/// so the reason is carried out of the walk and recorded on the root entity.
#[derive(Clone, Debug, Default)]
struct WalkOutcome {
    truncated: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct IdentityRecord {
    id: EntityId,
    #[serde(default = "file_record_kind")]
    record_kind: String,
    relative_path: String,
    content_fingerprint: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct IdentityRegistry {
    records: Vec<IdentityRecord>,
    #[serde(default)]
    last_snapshot: Option<SnapshotId>,
    #[serde(default)]
    observed_at: Option<String>,
}

#[derive(Clone, Debug)]
struct ScannedFile {
    relative_path: String,
    bytes: Vec<u8>,
    fingerprint: String,
    id: EntityId,
}

struct ScanEvidenceContext<'a> {
    snapshot: &'a SnapshotId,
    observed_at: &'a str,
}

/// Scan `root` with its own `.creature/config.toml`. This is what the pipeline
/// calls, so editing that file changes what is indexed — the limits and scope are
/// configuration, not constants baked into the binary.
pub fn scan_project_configured(root: &Path) -> Result<AtlasSnapshot, ScanError> {
    scan_project_with(root, &ScanConfig::load(root))
}

/// Scan with an explicit configuration, which is what a caller supplying its own
/// scope or ceilings uses.
pub fn scan_project_with(root: &Path, config: &ScanConfig) -> Result<AtlasSnapshot, ScanError> {
    let identity = init_project(root)?;
    let paths = ProjectPaths::new(root);
    let previous = load_registry(&paths.registry)?;
    let mut raw_files = Vec::new();
    let mut outcome = WalkOutcome::default();

    // With no `include`, the root itself is the subject. With one, only the named
    // subtrees are walked — which is what lets a home directory or a Library
    // folder be a root without indexing everything under it.
    if config.scope.include.is_empty() {
        collect_files(root, root, config, &mut raw_files, &mut 0u64, &mut outcome)?;
    } else {
        let mut total = 0u64;
        for included in &config.scope.include {
            let directory = root.join(included);
            if !directory.is_dir() {
                continue; // a named scope that is absent is not an error
            }
            collect_files(
                root,
                &directory,
                config,
                &mut raw_files,
                &mut total,
                &mut outcome,
            )?;
        }
    }
    let truncated = outcome.truncated.clone();
    raw_files.sort_by(|a, b| a.0.cmp(&b.0));
    let current_paths: BTreeSet<_> = raw_files.iter().map(|(path, _)| path.clone()).collect();
    let mut files = Vec::new();
    for (relative_path, bytes) in raw_files {
        let fingerprint = blake3::hash(&bytes).to_hex().to_string();
        let id = reconcile_id(
            &previous,
            "file",
            &relative_path,
            &fingerprint,
            &current_paths,
            identity.project_id,
        );
        files.push(ScannedFile {
            relative_path,
            bytes,
            fingerprint,
            id,
        });
    }
    let snapshot_id = snapshot_id(&files);
    let observed_at = if previous.last_snapshot.as_ref() == Some(&snapshot_id) {
        previous.observed_at.clone().unwrap_or_else(current_rfc3339)
    } else {
        current_rfc3339()
    };
    let evidence_context = ScanEvidenceContext {
        snapshot: &snapshot_id,
        observed_at: &observed_at,
    };
    let purpose = read_purpose(root)?.unwrap_or_default();
    let canonical_root = fs::canonicalize(root)?;
    let project_name = canonical_root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let mut entities = Vec::new();
    let mut edges = Vec::new();
    let universe = atlas_entity(
        identity.universe_id,
        "Local Universe",
        ScopeScale::Universe,
        EntityKind::Registry,
        None,
        None,
        &evidence_context,
    );
    let mut galaxy = atlas_entity(
        identity.galaxy_id,
        &project_name,
        ScopeScale::Galaxy,
        EntityKind::Product,
        Some(identity.universe_id),
        None,
        &evidence_context,
    );
    galaxy.purpose_clauses = purpose.goals;
    galaxy.protected_decision_ids = purpose
        .protected_decisions
        .into_iter()
        .filter_map(|id| uuid::Uuid::parse_str(&id).ok().map(RecordId))
        .collect();
    if galaxy.purpose_clauses.is_empty() {
        galaxy
            .uncertainty
            .push("PURPOSE.md is missing or contains no project goals".into());
    }
    // A truncated scan produced a real Atlas of part of the root. It must never
    // pass for a complete one, so the project entity carries the reason — visible
    // in status, in the IDX projection, and in any Orbit built from it.
    if let Some(reason) = &truncated {
        galaxy.uncertainty.push(reason.clone());
    }
    entities.extend([universe, galaxy]);
    edges.push(contains(
        identity.universe_id,
        identity.galaxy_id,
        &snapshot_id,
        &observed_at,
    ));

    let mut system_groups: BTreeMap<String, Vec<EntityId>> = BTreeMap::new();
    let mut planet_groups: BTreeMap<String, Vec<EntityId>> = BTreeMap::new();
    for file in &files {
        let (system_name, _, planet_key) = group_keys(&file.relative_path);
        system_groups.entry(system_name).or_default().push(file.id);
        planet_groups.entry(planet_key).or_default().push(file.id);
    }
    let current_system_paths: BTreeSet<_> = system_groups.keys().cloned().collect();
    let current_planet_paths: BTreeSet<_> = planet_groups.keys().cloned().collect();
    let system_ids: BTreeMap<_, _> = system_groups
        .iter()
        .map(|(path, ids)| {
            let fingerprint = group_fingerprint(ids);
            (
                path.clone(),
                reconcile_id(
                    &previous,
                    "system",
                    path,
                    &fingerprint,
                    &current_system_paths,
                    identity.project_id,
                ),
            )
        })
        .collect();
    let planet_ids: BTreeMap<_, _> = planet_groups
        .iter()
        .map(|(path, ids)| {
            let fingerprint = group_fingerprint(ids);
            (
                path.clone(),
                reconcile_id(
                    &previous,
                    "planet",
                    path,
                    &fingerprint,
                    &current_planet_paths,
                    identity.project_id,
                ),
            )
        })
        .collect();
    for file in &files {
        let path = Path::new(&file.relative_path);
        let (system_name, planet_name, planet_key) = group_keys(&file.relative_path);
        let system_id = system_ids[&system_name];
        let planet_id = planet_ids[&planet_key];
        if !entities.iter().any(|e| e.id == system_id) {
            entities.push(atlas_entity(
                system_id,
                &system_name,
                ScopeScale::System,
                EntityKind::Subsystem,
                Some(identity.galaxy_id),
                Some(&system_name),
                &evidence_context,
            ));
            edges.push(contains(
                identity.galaxy_id,
                system_id,
                &snapshot_id,
                &observed_at,
            ));
        }
        if !entities.iter().any(|e| e.id == planet_id) {
            entities.push(atlas_entity(
                planet_id,
                &planet_name,
                ScopeScale::Planet,
                EntityKind::Module,
                Some(system_id),
                Some(&planet_name),
                &evidence_context,
            ));
            edges.push(contains(system_id, planet_id, &snapshot_id, &observed_at));
        }
        let mut moon = atlas_entity(
            file.id,
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
            ScopeScale::Moon,
            file_kind(path),
            Some(planet_id),
            Some(&file.relative_path),
            &evidence_context,
        );
        moon.structural_fingerprint = file.fingerprint.clone();
        moon.deterministic_summary = summarize_file(path, &file.bytes);
        entities.push(moon);
        edges.push(contains(planet_id, file.id, &snapshot_id, &observed_at));
    }
    edges.extend(import_edges(&files, &snapshot_id, &observed_at));
    let mut snapshot = AtlasSnapshot {
        id: snapshot_id.clone(),
        timestamp: current_rfc3339(),
        entities,
        edges,
        records: vec![],
        conflicts: vec![],
        sources: vec![],
    };
    merge_recorded_evidence(&paths.evidence, &mut snapshot)?;
    evaluate_snapshot(&mut snapshot, &GreenPolicy::default())
        .map_err(|e| ScanError::Hierarchy(e.to_string()))?;
    let mut records: Vec<_> = files
        .iter()
        .map(|f| IdentityRecord {
            id: f.id,
            record_kind: "file".into(),
            relative_path: f.relative_path.clone(),
            content_fingerprint: f.fingerprint.clone(),
        })
        .collect();
    records.extend(system_groups.iter().map(|(path, ids)| IdentityRecord {
        id: system_ids[path],
        record_kind: "system".into(),
        relative_path: path.clone(),
        content_fingerprint: group_fingerprint(ids),
    }));
    records.extend(planet_groups.iter().map(|(path, ids)| IdentityRecord {
        id: planet_ids[path],
        record_kind: "planet".into(),
        relative_path: path.clone(),
        content_fingerprint: group_fingerprint(ids),
    }));
    records.sort_by_key(|record| (record.record_kind.clone(), record.relative_path.clone()));
    let registry = IdentityRegistry {
        records,
        last_snapshot: Some(snapshot_id),
        observed_at: Some(observed_at),
    };
    atomic_write(
        &paths.registry,
        &serde_json::to_vec_pretty(&registry).map_err(io::Error::other)?,
    )?;
    Ok(snapshot)
}

fn collect_files(
    root: &Path,
    directory: &Path,
    config: &ScanConfig,
    output: &mut Vec<(String, Vec<u8>)>,
    total: &mut u64,
    outcome: &mut WalkOutcome,
) -> Result<(), ScanError> {
    let limits = config.limits();
    // A directory that cannot be read — a permission-denied Library subfolder, a
    // vanished temp dir — is skipped rather than fatal. Walking a general root
    // like a home directory means meeting these routinely, and one unreadable
    // folder must not cost the whole Atlas.
    let Ok(read) = fs::read_dir(directory) else {
        return Ok(());
    };
    let mut entries: Vec<_> = read.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if outcome.truncated.is_some() {
            return Ok(()); // a ceiling was reached; stop walking, keep what we have
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if SKIPPED_DIRECTORIES.contains(&name.as_str()) || config.scope.excludes(&name) {
                continue;
            }
            collect_files(root, &path, config, output, total, outcome)?;
            continue;
        }
        if !metadata.is_file() || metadata.len() > limits.max_file_bytes {
            continue;
        }
        if matches!(
            entry.file_name().to_string_lossy().as_ref(),
            "ATLAS.idx" | ".atlas.yaml" | ".module-map.yaml" | ".DS_Store"
        ) {
            continue;
        }
        if limits.files_exhausted(output.len()) {
            outcome.truncated = Some(format!(
                "scan truncated at the configured max_files ({}); the Atlas covers \
                 only part of this root",
                limits.max_files
            ));
            return Ok(());
        }
        if limits.bytes_exhausted(*total + metadata.len()) {
            outcome.truncated = Some(format!(
                "scan truncated at the configured max_total_bytes ({}); the Atlas \
                 covers only part of this root",
                limits.max_total_bytes
            ));
            return Ok(());
        }
        *total += metadata.len();
        let relative = path
            .strip_prefix(root)
            .map_err(io::Error::other)?
            .to_string_lossy()
            .replace('\\', "/");
        output.push((relative, fs::read(path)?));
    }
    Ok(())
}

fn load_registry(path: &Path) -> io::Result<IdentityRegistry> {
    if !path.exists() {
        return Ok(IdentityRegistry::default());
    }
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

fn snapshot_id(files: &[ScannedFile]) -> SnapshotId {
    let mut hasher = blake3::Hasher::new();
    for file in files {
        hasher.update(file.relative_path.as_bytes());
        hasher.update(file.fingerprint.as_bytes());
    }
    SnapshotId(format!("blake3:{}", hasher.finalize().to_hex()))
}

fn stable_id(project: ProjectId, kind: &str, key: &str) -> EntityId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(project.0.as_bytes());
    hasher.update(kind.as_bytes());
    hasher.update(key.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    EntityId(uuid::Uuid::from_bytes(bytes))
}

fn reconcile_id(
    previous: &IdentityRegistry,
    kind: &str,
    path: &str,
    fingerprint: &str,
    current_paths: &BTreeSet<String>,
    project: ProjectId,
) -> EntityId {
    if let Some(record) = previous
        .records
        .iter()
        .find(|record| record.record_kind == kind && record.relative_path == path)
    {
        return record.id;
    }
    let candidates: Vec<_> = previous
        .records
        .iter()
        .filter(|record| {
            record.record_kind == kind
                && record.content_fingerprint == fingerprint
                && !current_paths.contains(&record.relative_path)
        })
        .collect();
    if candidates.len() == 1 {
        candidates[0].id
    } else {
        stable_id(project, kind, path)
    }
}

fn group_keys(relative_path: &str) -> (String, String, String) {
    let path = Path::new(relative_path);
    let components: Vec<_> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    let system_name = if components.len() > 1 {
        components[0].clone()
    } else {
        "root".into()
    };
    let planet_name = if components.len() > 2 {
        components[..components.len() - 1].join("/")
    } else {
        system_name.clone()
    };
    let planet_key = format!("{system_name}/{planet_name}");
    (system_name, planet_name, planet_key)
}

fn group_fingerprint(ids: &[EntityId]) -> String {
    let mut ids = ids.to_vec();
    ids.sort();
    let mut hasher = blake3::Hasher::new();
    for id in ids {
        hasher.update(id.0.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn file_record_kind() -> String {
    "file".into()
}

fn atlas_entity(
    id: EntityId,
    name: &str,
    scale: ScopeScale,
    kind: EntityKind,
    parent_id: Option<EntityId>,
    path: Option<&str>,
    context: &ScanEvidenceContext<'_>,
) -> AtlasEntity {
    let evidence = [GreenAxis::Content, GreenAxis::Structure]
        .into_iter()
        .map(|axis| Evidence {
            axis,
            source: FactSource::Parsed,
            proof: ProofStrength::Metadata,
            outcome: EvidenceOutcome::Pass,
            confidence: 1.0,
            fingerprint: context.snapshot.0.clone(),
            observed_at: context.observed_at.into(),
            producer: "creature-context-scanner".into(),
            snapshot_id: context.snapshot.clone(),
            message: String::new(),
        })
        .collect();
    AtlasEntity {
        id,
        scale,
        kind,
        canonical_name: name.into(),
        aliases: vec![],
        parent_id,
        relative_path: path.map(str::to_owned),
        purpose_clauses: vec![],
        protected_decision_ids: vec![],
        responsibilities: vec![],
        interfaces: vec![],
        capabilities: vec![],
        sockets: vec![],
        source_spans: vec![],
        deterministic_summary: String::new(),
        local_evidence: evidence,
        inherited_evidence: vec![],
        green: None,
        open_conflict_ids: vec![],
        inferred_summaries: vec![],
        uncertainty: vec![],
        observed_at: context.observed_at.into(),
        fresh_until: None,
        snapshot_id: context.snapshot.clone(),
        structural_fingerprint: String::new(),
    }
}

fn contains(
    source: EntityId,
    target: EntityId,
    snapshot: &SnapshotId,
    observed_at: &str,
) -> AtlasEdge {
    AtlasEdge {
        id: edge_id(source, target, "contains"),
        source_entity_id: source,
        target_entity_id: target,
        kind: RelationshipKind::Contains,
        plane: RelationshipPlane::Declared,
        proof_record_ids: vec![],
        required: true,
        evidence: vec![Evidence {
            axis: GreenAxis::Integration,
            source: FactSource::Parsed,
            proof: ProofStrength::Metadata,
            outcome: EvidenceOutcome::Pass,
            confidence: 1.0,
            fingerprint: snapshot.0.clone(),
            observed_at: observed_at.into(),
            producer: "creature-context-scanner".into(),
            snapshot_id: snapshot.clone(),
            message: String::new(),
        }],
        source_id: "scanner".into(),
        confidence: 1.0,
        observed_at: observed_at.into(),
        fresh_until: None,
        snapshot_id: snapshot.clone(),
    }
}

fn edge_id(source: EntityId, target: EntityId, kind: &str) -> EdgeId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(source.0.as_bytes());
    hasher.update(target.0.as_bytes());
    hasher.update(kind.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    EdgeId(uuid::Uuid::from_bytes(bytes))
}

fn file_kind(path: &Path) -> EntityKind {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    if name.contains("test") || name.contains("spec") {
        EntityKind::Test
    } else if matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "json" | "yaml" | "yml" | "toml")
    ) {
        EntityKind::Resource
    } else {
        EntityKind::File
    }
}

fn summarize_file(path: &Path, bytes: &[u8]) -> String {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("unknown");
    let lines =
        bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(!bytes.is_empty());
    format!("{extension} file, {lines} line(s), {} byte(s)", bytes.len())
}

fn import_edges(files: &[ScannedFile], snapshot: &SnapshotId, observed_at: &str) -> Vec<AtlasEdge> {
    let by_stem: BTreeMap<_, _> = files
        .iter()
        .filter_map(|file| {
            Path::new(&file.relative_path)
                .file_stem()
                .map(|stem| (stem.to_string_lossy().to_lowercase(), file.id))
        })
        .collect();
    let mut edges = BTreeMap::new();
    for file in files {
        let Ok(text) = std::str::from_utf8(&file.bytes) else {
            continue;
        };
        for token in text
            .lines()
            .filter(|line| {
                let value = line.trim_start();
                value.starts_with("import ")
                    || value.starts_with("from ")
                    || value.starts_with("use ")
                    || value.starts_with("#include")
            })
            .flat_map(|line| line.split(|c: char| !c.is_alphanumeric() && c != '_'))
            .filter(|token| token.len() > 1)
        {
            if let Some(target) = by_stem
                .get(&token.to_lowercase())
                .copied()
                .filter(|id| *id != file.id)
            {
                let edge = AtlasEdge {
                    id: edge_id(file.id, target, "imports"),
                    source_entity_id: file.id,
                    target_entity_id: target,
                    kind: RelationshipKind::Imports,
                    plane: RelationshipPlane::Observed,
                    proof_record_ids: vec![],
                    required: false,
                    evidence: vec![Evidence {
                        axis: GreenAxis::Integration,
                        source: FactSource::Parsed,
                        proof: ProofStrength::Syntax,
                        outcome: EvidenceOutcome::Pass,
                        confidence: 0.7,
                        fingerprint: snapshot.0.clone(),
                        observed_at: observed_at.into(),
                        producer: "creature-context-import-scanner".into(),
                        snapshot_id: snapshot.clone(),
                        message: "lexical import match".into(),
                    }],
                    source_id: "scanner".into(),
                    confidence: 0.7,
                    observed_at: observed_at.into(),
                    fresh_until: None,
                    snapshot_id: snapshot.clone(),
                };
                edges.insert(edge.id, edge);
            }
        }
    }
    edges.into_values().collect()
}

pub fn current_rfc3339() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn merge_recorded_evidence(path: &Path, snapshot: &mut AtlasSnapshot) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let recorded: Vec<RecordedEvidence> =
        serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
    let mut by_id: BTreeMap<_, _> = snapshot
        .entities
        .iter_mut()
        .map(|entity| (entity.id, entity))
        .collect();
    for record in recorded {
        if record.evidence.snapshot_id != snapshot.id {
            continue;
        }
        if let Some(entity) = by_id.get_mut(&record.entity_id) {
            entity.local_evidence.push(record.evidence);
        }
    }
    Ok(())
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u64, u64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u64, day as u64)
}

#[allow(dead_code)]
fn _normalise(path: PathBuf) -> String {
    path.to_string_lossy().replace('\\', "/")
}
