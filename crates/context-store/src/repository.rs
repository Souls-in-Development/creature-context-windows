use creature_context_types::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use thiserror::Error;

const MIGRATION: &str = include_str!("../migrations/0004_multiscale_atlas.sql");

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Uuid(#[from] uuid::Error),
    #[error("database has no current Atlas snapshot")]
    Empty,
    #[error("stored root entity is missing")]
    MissingRoot,
}

pub struct AtlasRepository {
    connection: Connection,
}

impl AtlasRepository {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        connection.execute_batch(MIGRATION)?;
        Ok(Self { connection })
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(MIGRATION)?;
        Ok(Self { connection })
    }

    pub fn replace_snapshot(&mut self, snapshot: &AtlasSnapshot) -> Result<(), StoreError> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM atlas_edges", [])?;
        transaction.execute("DELETE FROM atlas_entities", [])?;
        for entity in &snapshot.entities {
            transaction.execute(
                "INSERT INTO atlas_entities (id, scale, kind, canonical_name, parent_id, payload_json, snapshot_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![entity.id.to_string(), serde_json::to_value(entity.scale)?.as_str(), serde_json::to_value(entity.kind)?.as_str(), entity.canonical_name, entity.parent_id.map(|id| id.to_string()), serde_json::to_string(entity)?, snapshot.id.0],
            )?;
        }
        for edge in &snapshot.edges {
            transaction.execute(
                "INSERT INTO atlas_edges (id, source_id, target_id, relationship_kind, relationship_plane, required, payload_json, snapshot_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![edge.id.to_string(), edge.source_entity_id.to_string(), edge.target_entity_id.to_string(), serde_json::to_value(edge.kind)?.as_str(), serde_json::to_value(edge.plane)?.as_str(), edge.required, serde_json::to_string(edge)?, snapshot.id.0],
            )?;
        }
        transaction.execute("INSERT INTO metadata (key, value) VALUES ('current_snapshot', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&snapshot.id.0])?;
        transaction.execute("INSERT INTO metadata (key, value) VALUES ('root_id', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [snapshot.entities.first().map(|e| e.id).unwrap_or_else(|| EntityId(uuid::Uuid::nil())).to_string()])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn load_snapshot(&self) -> Result<AtlasSnapshot, StoreError> {
        let snapshot_id: String = self
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key='current_snapshot'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::Empty)?;

        let mut entity_statement = self.connection.prepare(
            "SELECT payload_json FROM atlas_entities ORDER BY scale, canonical_name, id",
        )?;
        let entities = entity_statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|value| Ok(serde_json::from_str(&value?)?))
            .collect::<Result<Vec<AtlasEntity>, StoreError>>()?;
        let mut edge_statement = self.connection.prepare("SELECT payload_json FROM atlas_edges ORDER BY relationship_kind, source_id, target_id, id")?;
        let edges = edge_statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|value| Ok(serde_json::from_str(&value?)?))
            .collect::<Result<Vec<AtlasEdge>, StoreError>>()?;
        Ok(AtlasSnapshot {
            id: SnapshotId(snapshot_id),
            timestamp: "2026-08-03T00:00:00Z".to_string(),
            entities,
            edges,
            records: vec![],
            conflicts: vec![],
            sources: vec![],
        })
    }
}
