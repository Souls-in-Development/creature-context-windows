//! Enrich a scanned snapshot with parsed structure.
//!
//! Applied *after* `scan_project`, and from this crate rather than the scanner:
//! the scanner lives in `context-core`, and `context-core` does not depend on
//! `context-parsers` — the dependency runs the other way. So the CLI scans
//! (core), then enriches (here), then commits.
//!
//! For each source file the scanner produced as a Moon entity, the file is
//! parsed and its top-level declarations are added as Moon entities under it,
//! joined by an `observed` `contains` edge. A file Moon containing symbol Moons
//! is valid same-scale nesting (specification 3.4, hierarchy `allowed`).
//! Parsing is enrichment: an unsupported language or a parse failure leaves the
//! deterministic file entity exactly as the scanner produced it (spec §17).

use crate::adapter::{Construct, ParsedImport, macro_defined_names, parse, parse_imports};
use crate::incremental::{ParseCache, ParsedFile};
use crate::languages::language_for_extension;
use creature_context_types::{
    AtlasEdge, AtlasEntity, AtlasSnapshot, AtlasSocket, EdgeId, EntityId, EntityKind, Evidence,
    EvidenceOutcome, FactSource, GreenAxis, HoleReason, ProofStrength, RelationshipKind,
    RelationshipPlane, ScopeScale, SnapshotId, SocketDirection, SocketId, SocketResolution,
    SocketShape, SourceSpan,
};
use std::collections::HashSet;
use std::path::Path;

/// Enrich `snapshot` in place with parsed symbols, reading sources under `root`.
/// Returns the number of symbol entities added. Every file is parsed; this is
/// what a one-shot `scan` wants.
pub fn enrich_snapshot(root: &Path, snapshot: &mut AtlasSnapshot) -> usize {
    let mut cache = ParseCache::new();
    enrich_snapshot_cached(root, snapshot, &mut cache)
}

/// Enrich `snapshot`, reusing `cache` for any file whose content fingerprint has
/// been parsed before. Identical in output to `enrich_snapshot` — the cache holds
/// parses, and entities are rebuilt from them against the current snapshot id —
/// but a file whose content has not changed is not read or parsed again. This is
/// the entry point the resident daemon uses (spec §7.1).
pub fn enrich_snapshot_cached(
    root: &Path,
    snapshot: &mut AtlasSnapshot,
    cache: &mut ParseCache,
) -> usize {
    let snapshot_id = snapshot.id.clone();

    // Snapshot the file entities first; we push new entities as we go. The
    // fingerprint is the scanner's blake3 of the file's bytes, which is the
    // cache key — content, not an event, decides whether a re-parse is needed.
    let files: Vec<(EntityId, String, String)> = snapshot
        .entities
        .iter()
        .filter(|e| e.scale == ScopeScale::Moon)
        .filter_map(|e| {
            e.relative_path
                .clone()
                .map(|p| (e.id, p, e.structural_fingerprint.clone()))
        })
        .collect();

    // Bound the cache to content the project still contains, before this pass
    // adds to it.
    let live: HashSet<String> = files
        .iter()
        .map(|(_, _, fingerprint)| fingerprint.clone())
        .collect();
    cache.retain_fingerprints(&live);

    // Required sockets attach to file entities, which already exist in the
    // snapshot; collect them while iterating and attach afterwards, since the
    // loop is pushing new symbol entities at the same time.
    let mut pending_requires: Vec<(EntityId, AtlasSocket)> = Vec::new();

    // Every name the repository defines that a required socket might target —
    // declarations (public or not) and identifiers a macro expands from. The
    // provides index sees only parsed public declarations, so a required import
    // of a macro-generated or private name would otherwise look like proof of
    // absence. This set is the humility guard: a `no_match` for a name that is
    // defined-but-invisible here is downgraded to Unknown rather than reported
    // as a broken link (spec §6.4, §17 — degrade explicitly, never fabricate).
    let mut defined_names: HashSet<String> = HashSet::new();

    let mut added = 0;
    for (file_id, relative_path, fingerprint) in files {
        let Some(language) = Path::new(&relative_path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(language_for_extension)
        else {
            continue; // no grammar for this file — leave the file entity as-is
        };

        // Parse only content this cache has not seen. A miss reads and parses
        // exactly as before; a hit skips both the read and the tree-sitter pass.
        // All three products come from one parse, so they are cached together.
        // A file with no content fingerprint cannot be proven unchanged, so it is
        // always parsed and never cached. An empty key would otherwise collide
        // across every such file and serve one file's parse for another — which
        // is exactly what a hand-built snapshot (or any scanner that omits the
        // hash) would hit. Cloned so the borrow ends before the miss arm writes.
        let cached = if fingerprint.is_empty() {
            None
        } else {
            cache.get(&fingerprint).cloned()
        };
        // Exactly one cache lookup per file, so the hit/miss counts mean what
        // they say. A miss reads and parses as before; a hit skips both.
        let parsed = match cached {
            Some(parsed) => parsed,
            None => {
                let Ok(source) = std::fs::read_to_string(root.join(&relative_path)) else {
                    continue;
                };
                let Ok(symbols) = parse(&source, language) else {
                    continue; // parse failure degrades to the deterministic entity
                };
                let parsed = ParsedFile {
                    symbols,
                    imports: parse_imports(&source, language).unwrap_or_default(),
                    macro_names: macro_defined_names(&source, language).unwrap_or_default(),
                };
                if !fingerprint.is_empty() {
                    cache.insert(fingerprint.clone(), parsed.clone());
                }
                parsed
            }
        };

        for symbol in &parsed.symbols {
            defined_names.insert(symbol.name.clone());
            let symbol_id = symbol_entity_id(file_id, &symbol.name, symbol.start_line);
            snapshot.entities.push(symbol_entity(
                symbol_id,
                file_id,
                &relative_path,
                symbol,
                &snapshot_id,
            ));
            snapshot
                .edges
                .push(contains_edge(file_id, symbol_id, &snapshot_id));
            added += 1;
        }

        defined_names.extend(parsed.macro_names.iter().cloned());

        // An intra-repo import is a shape this file requires. External imports
        // are not extracted (adapter::parse_imports), so a required socket here
        // always names something the repository itself is expected to provide.
        for import in &parsed.imports {
            pending_requires.push((file_id, requires_socket(file_id, import, &snapshot_id)));
        }
    }

    for (file_id, socket) in pending_requires {
        if let Some(file) = snapshot.entities.iter_mut().find(|e| e.id == file_id) {
            file.sockets.push(socket);
        }
    }

    // The deterministic reconciler decides which required shapes fit which
    // provided ones (spec §6.4). The Milestone 2 evaluator then darkens the
    // integration axis from these resolutions when Green is next computed.
    creature_context_core::sockets::resolve_sockets(snapshot);

    // Humility pass: a `no_match` is only proof of absence when the provides
    // index is authoritative. It is not — Tree-sitter cannot see macro-expanded
    // or private declarations — so a required name that the repository defines
    // by some means invisible here is Unknown, not a broken link. A name absent
    // everywhere stays a `no_match`, which is what makes the hole trustworthy.
    for entity in &mut snapshot.entities {
        for socket in &mut entity.sockets {
            if socket.direction == SocketDirection::Requires
                && matches!(
                    &socket.resolution,
                    SocketResolution::Hole(hole) if hole.reason == HoleReason::NoMatch
                )
                && defined_names.contains(leaf_name(&socket.shape.qualified_name))
            {
                socket.resolution = SocketResolution::Unresolved;
            }
        }
    }

    added
}

/// The item name a socket shape is matched on: the final `::`/`.`/`/` segment.
fn leaf_name(qualified_name: &str) -> &str {
    qualified_name
        .rsplit([':', '.', '/'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(qualified_name)
}

/// A UUID derived deterministically from a key, so a rescan produces the same
/// ids (blake3, since the workspace uuid has no v5 feature).
fn deterministic_uuid(key: &str) -> uuid::Uuid {
    let hash = blake3::hash(key.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    uuid::Uuid::from_bytes(bytes)
}

/// Deterministic id: the same file, symbol and line always yield the same
/// entity id, so a rescan is stable.
fn symbol_entity_id(file: EntityId, name: &str, start_line: usize) -> EntityId {
    EntityId(deterministic_uuid(&format!("{file}/{name}/{start_line}")))
}

fn entity_kind(construct: &Construct) -> EntityKind {
    match construct {
        Construct::Shared(canonical) => match canonical.as_str() {
            "function" | "closure" | "procedure" | "method" => EntityKind::Function,
            "test" => EntityKind::Test,
            "product_type" | "class" | "enumeration" | "behavioral_contract" | "type_alias" => {
                EntityKind::Type
            }
            _ => EntityKind::Component,
        },
        Construct::Native(_) => EntityKind::Component,
    }
}

fn construct_label(construct: &Construct) -> String {
    match construct {
        Construct::Shared(c) => c.clone(),
        Construct::Native(n) => format!("native:{}", n.name),
    }
}

fn symbol_entity(
    id: EntityId,
    parent: EntityId,
    file_path: &str,
    symbol: &crate::adapter::ParsedSymbol,
    snapshot: &SnapshotId,
) -> AtlasEntity {
    let span = SourceSpan {
        source_id: file_path.to_string(),
        relative_path: file_path.to_string(),
        start_line: symbol.start_line as u32,
        start_column: 1,
        end_line: symbol.end_line as u32,
        end_column: 1,
        content_hash: String::new(),
    };
    // Content and Structure, both from parsing — the same two axes the scanner
    // asserts for a file, so a symbol starts on equal footing with the file that
    // contains it. The other axes (integration, verification, freshness,
    // coherence) remain Unknown until evidence is recorded, so a symbol is not
    // Green merely for having been parsed.
    let evidence: Vec<Evidence> = [GreenAxis::Content, GreenAxis::Structure]
        .into_iter()
        .map(|axis| Evidence {
            axis,
            source: FactSource::Parsed,
            proof: ProofStrength::Syntax,
            outcome: EvidenceOutcome::Pass,
            confidence: 1.0,
            fingerprint: snapshot.0.clone(),
            observed_at: "2026-08-07T00:00:00Z".into(),
            producer: "creature-context-parsers".into(),
            snapshot_id: snapshot.clone(),
            message: String::new(),
        })
        .collect();
    AtlasEntity {
        id,
        scale: ScopeScale::Moon,
        kind: entity_kind(&symbol.construct),
        canonical_name: symbol.name.clone(),
        aliases: vec![],
        parent_id: Some(parent),
        relative_path: Some(file_path.to_string()),
        purpose_clauses: vec![],
        protected_decision_ids: vec![],
        responsibilities: vec![],
        interfaces: vec![],
        capabilities: vec![],
        // An exported declaration provides its shape for others to require; a
        // private one exposes nothing to match against.
        sockets: if symbol.exported {
            vec![provides_socket(id, symbol, snapshot)]
        } else {
            vec![]
        },
        source_spans: vec![span],
        structural_fingerprint: construct_label(&symbol.construct),
        local_evidence: evidence,
        inherited_evidence: vec![],
        green: None,
        open_conflict_ids: vec![],
        deterministic_summary: String::new(),
        inferred_summaries: vec![],
        uncertainty: vec![],
        snapshot_id: snapshot.clone(),
        observed_at: "2026-08-07T00:00:00Z".into(),
        fresh_until: None,
    }
}

fn contains_edge(file: EntityId, symbol: EntityId, snapshot: &SnapshotId) -> AtlasEdge {
    let key = format!("contains/{file}/{symbol}");
    AtlasEdge {
        id: EdgeId(deterministic_uuid(&key)),
        source_entity_id: file,
        target_entity_id: symbol,
        kind: RelationshipKind::Contains,
        // Observed: a parser saw the file contain this symbol.
        plane: RelationshipPlane::Observed,
        proof_record_ids: vec![],
        evidence: vec![Evidence {
            axis: GreenAxis::Integration,
            source: FactSource::Parsed,
            proof: ProofStrength::Syntax,
            outcome: EvidenceOutcome::Pass,
            confidence: 1.0,
            fingerprint: snapshot.0.clone(),
            observed_at: "2026-08-07T00:00:00Z".into(),
            producer: "creature-context-parsers".into(),
            snapshot_id: snapshot.clone(),
            message: String::new(),
        }],
        source_id: "creature-context-parsers".into(),
        confidence: 1.0,
        observed_at: "2026-08-07T00:00:00Z".into(),
        fresh_until: None,
        required: false,
        snapshot_id: snapshot.clone(),
    }
}

/// A shape for socket matching. Matching keys on the name (spec §6.4), so the
/// hash spans all three fields to keep distinct shapes distinct in the IDX.
fn socket_shape(qualified_name: &str, signature: &str) -> SocketShape {
    let version = "1";
    let hash = blake3::hash(format!("{qualified_name}|{signature}|{version}").as_bytes())
        .to_hex()
        .to_string();
    SocketShape {
        qualified_name: qualified_name.to_string(),
        structural_signature: signature.to_string(),
        version: version.to_string(),
        hash,
    }
}

/// The `provides` socket for an exported declaration: the shape it exposes. The
/// name is the declaration's own; the signature is its construct, which is what
/// Tree-sitter can see without a type checker.
fn provides_socket(
    entity: EntityId,
    symbol: &crate::adapter::ParsedSymbol,
    snapshot: &SnapshotId,
) -> AtlasSocket {
    AtlasSocket {
        id: SocketId(deterministic_uuid(&format!(
            "provides/{entity}/{}",
            symbol.name
        ))),
        entity_id: entity,
        direction: SocketDirection::Provides,
        shape: socket_shape(&symbol.name, &construct_label(&symbol.construct)),
        optional: false,
        resolution: SocketResolution::Unresolved,
        source_id: "creature-context-parsers".into(),
        confidence: 1.0,
        observed_at: "2026-08-07T00:00:00Z".into(),
        snapshot_id: snapshot.clone(),
    }
}

/// The `requires` socket for an intra-repo import: the shape a file needs. An
/// import does not reveal its target's signature, so the shape carries only the
/// name (the load-bearing field for matching, spec §6.4). Not optional — an
/// unmet intra-repo import is a real integration finding, so it must be able to
/// darken the axis.
fn requires_socket(file: EntityId, import: &ParsedImport, snapshot: &SnapshotId) -> AtlasSocket {
    AtlasSocket {
        id: SocketId(deterministic_uuid(&format!(
            "requires/{file}/{}/{}",
            import.path, import.start_line
        ))),
        entity_id: file,
        direction: SocketDirection::Requires,
        shape: socket_shape(&import.path, ""),
        optional: false,
        resolution: SocketResolution::Unresolved,
        source_id: "creature-context-parsers".into(),
        confidence: 1.0,
        observed_at: "2026-08-07T00:00:00Z".into(),
        snapshot_id: snapshot.clone(),
    }
}
