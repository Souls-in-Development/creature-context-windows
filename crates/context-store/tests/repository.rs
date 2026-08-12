use creature_context_store::AtlasRepository;
use creature_context_types::*;

fn entity(snapshot: &SnapshotId) -> AtlasEntity {
    AtlasEntity {
        id: EntityId::new(),
        scale: ScopeScale::Universe,
        kind: EntityKind::Registry,
        canonical_name: "Universe".into(),
        aliases: vec![],
        parent_id: None,
        relative_path: None,
        purpose_clauses: vec![],
        protected_decision_ids: vec![],
        responsibilities: vec![],
        interfaces: vec![],
        capabilities: vec![],
        sockets: vec![],
        source_spans: vec![],
        deterministic_summary: String::new(),
        local_evidence: vec![],
        inherited_evidence: vec![],
        green: None,
        open_conflict_ids: vec![],
        inferred_summaries: vec![],
        uncertainty: vec![],
        observed_at: "2026-08-03T00:00:00Z".to_string(),
        fresh_until: None,
        snapshot_id: snapshot.clone(),
        structural_fingerprint: String::new(),
    }
}

#[test]
fn snapshot_round_trips_transactionally() {
    let id = SnapshotId("snapshot-test".into());
    let root = entity(&id);
    let snapshot = AtlasSnapshot {
        id: id.clone(),
        timestamp: "2026-08-03T00:00:00Z".to_string(),
        entities: vec![root],
        edges: vec![],
        records: vec![],
        conflicts: vec![],
        sources: vec![],
    };
    let mut repository = AtlasRepository::in_memory().unwrap();
    repository.replace_snapshot(&snapshot).unwrap();
    assert_eq!(repository.load_snapshot().unwrap(), snapshot);
}
