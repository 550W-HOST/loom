//! The entity view, as rows in the store.
//!
//! Until now the entity view — projects, threads, hosts, environments, queued
//! messages, interactions, sidebar sections, runs, settings and automations —
//! was written as one fenced file with a log watermark in it. This is the same
//! view in tables, in the store that already holds the conversations, so that a
//! start no longer depends on a file written every thirty seconds and a bounded
//! log to fill the gap.
//!
//! Two rules the file could not state, and this can:
//!
//! * **Replacing the view is one transaction.** A view is replaced whole, never
//!   updated in place: a crash leaves the previous view or the new one, and a
//!   start can never find half of each. What that costs is one rewrite of a
//!   small table per snapshot, which is what the file did anyway.
//! * **A row that cannot be read is an error, not a gap.** An entity whose JSON
//!   no longer parses, or a singleton that is missing, fails the read: an entity
//!   view with a hole in it is a view that would silently lose a project.
//!
//! The kinds are the snapshot's own fields. `settings` and `automations` are
//! singletons — one row each, under a fixed id — because that is what they are.

use loom_domain::{Environment, Host, Interaction, Project, QueuedMessage, Thread, ThreadSection};
use serde::de::DeserializeOwned;
use serde::Serialize;
use turso::params;

use super::{block_on, column_text, Store, StoreError};
use crate::persistence::{DomainSnapshot, SNAPSHOT_VERSION};
use crate::runs::RunRecord;
use crate::settings::SettingsSnapshot;

/// The id a singleton entity is stored under.
const SINGLETON: &str = "current";

/// The `entity_meta` key holding the log position the view was taken at.
const WATERMARK: &str = "watermark";
/// The `entity_meta` key holding the reserved personal project's id.
const PERSONAL_PROJECT: &str = "personal_project_id";

/// One entity, as it is written.
struct Row {
    kind: &'static str,
    id: String,
    parent: Option<String>,
    json: String,
}

impl Store {
    /// Replaces the whole entity view, in one transaction.
    pub fn replace_entities(&self, snapshot: &DomainSnapshot) -> Result<(), StoreError> {
        let rows = rows_of(snapshot)?;
        let transaction = block_on(self.connection().unchecked_transaction())?;
        block_on(transaction.execute("DELETE FROM entity", ()))?;
        block_on(transaction.execute("DELETE FROM entity_meta", ()))?;
        for row in &rows {
            block_on(transaction.execute(
                "INSERT INTO entity (kind, id, parent_id, json) VALUES (?1, ?2, ?3, ?4)",
                params![
                    row.kind,
                    row.id.clone(),
                    row.parent.clone(),
                    row.json.clone()
                ],
            ))?;
        }
        block_on(transaction.execute(
            "INSERT INTO entity_meta (key, value) VALUES (?1, ?2)",
            params![
                PERSONAL_PROJECT,
                snapshot.registry.personal_project_id.to_string()
            ],
        ))?;
        if let Some(watermark) = &snapshot.watermark {
            block_on(transaction.execute(
                "INSERT INTO entity_meta (key, value) VALUES (?1, ?2)",
                params![WATERMARK, watermark.to_string()],
            ))?;
        }
        block_on(transaction.commit())?;
        Ok(())
    }

    /// The entity view, or `None` when this store has never held one.
    ///
    /// `None` and an empty view are different answers: the first says recovery
    /// should start from the log, the second that a view was taken and held
    /// nothing.
    pub fn entities(&self) -> Result<Option<DomainSnapshot>, StoreError> {
        let mut rows = block_on(
            self.connection()
                .query("SELECT kind, id, json FROM entity ORDER BY kind, id", ()),
        )?;
        let mut projects = Vec::new();
        let mut threads = Vec::new();
        let mut hosts = Vec::new();
        let mut environments = Vec::new();
        let mut queued_messages = Vec::new();
        let mut interactions = Vec::new();
        let mut thread_sections = Vec::new();
        let mut runs = Vec::new();
        let mut settings = None;
        let mut automations = None;
        let mut rows_seen = 0usize;
        while let Some(row) = block_on(rows.next())? {
            rows_seen += 1;
            let kind = column_text(&row, 0)?;
            let id = column_text(&row, 1)?;
            let json = column_text(&row, 2)?;
            match kind.as_str() {
                "project" => projects.push(decode::<Project>(&kind, &id, &json)?),
                "thread" => threads.push(decode::<Thread>(&kind, &id, &json)?),
                "host" => hosts.push(decode::<Host>(&kind, &id, &json)?),
                "environment" => environments.push(decode::<Environment>(&kind, &id, &json)?),
                "queued_message" => {
                    queued_messages.push(decode::<QueuedMessage>(&kind, &id, &json)?)
                }
                "interaction" => interactions.push(decode::<Interaction>(&kind, &id, &json)?),
                "thread_section" => {
                    thread_sections.push(decode::<ThreadSection>(&kind, &id, &json)?)
                }
                "run" => runs.push(decode::<RunRecord>(&kind, &id, &json)?),
                "settings" => settings = Some(decode::<SettingsSnapshot>(&kind, &id, &json)?),
                "automations" => {
                    automations = Some(decode::<crate::automations::AutomationState>(
                        &kind, &id, &json,
                    )?)
                }
                other => {
                    return Err(StoreError::new(format!(
                        "the store holds an entity of an unknown kind {other:?}"
                    )))
                }
            }
        }
        if rows_seen == 0 {
            return Ok(None);
        }

        let mut meta = block_on(
            self.connection()
                .query("SELECT key, value FROM entity_meta", ()),
        )?;
        let mut personal_project_id = None;
        let mut watermark = None;
        while let Some(row) = block_on(meta.next())? {
            let key = column_text(&row, 0)?;
            let value = column_text(&row, 1)?;
            match key.as_str() {
                PERSONAL_PROJECT => personal_project_id = Some(value),
                WATERMARK => watermark = Some(value),
                _ => {}
            }
        }
        let personal_project_id = personal_project_id
            .ok_or_else(|| StoreError::new("the stored entity view has no personal project id"))?;
        let personal_project_id = personal_project_id.parse().map_err(|error| {
            StoreError::new(format!(
                "the stored personal project id {personal_project_id:?}: {error}"
            ))
        })?;
        let watermark = match watermark {
            Some(watermark) => Some(watermark.parse().map_err(|error| {
                StoreError::new(format!("the stored watermark {watermark:?}: {error}"))
            })?),
            None => None,
        };

        Ok(Some(DomainSnapshot {
            version: SNAPSHOT_VERSION,
            watermark,
            registry: crate::domain_state::RegistrySnapshot {
                personal_project_id,
                projects,
                threads,
                hosts,
                environments,
                queued_messages,
                interactions,
                thread_sections,
            },
            runs,
            settings,
            automations,
        }))
    }
}

/// The rows a snapshot becomes, identities and parents as columns.
fn rows_of(snapshot: &DomainSnapshot) -> Result<Vec<Row>, StoreError> {
    let registry = &snapshot.registry;
    let mut rows = Vec::new();
    let mut push = |kind: &'static str, id: String, parent: Option<String>, json: String| {
        rows.push(Row {
            kind,
            id,
            parent,
            json,
        });
    };

    for project in &registry.projects {
        push("project", project.id.to_string(), None, encode(project)?);
    }
    for thread in &registry.threads {
        push(
            "thread",
            thread.id.to_string(),
            Some(thread.project_id.to_string()),
            encode(thread)?,
        );
    }
    for host in &registry.hosts {
        push("host", host.id.to_string(), None, encode(host)?);
    }
    for environment in &registry.environments {
        push(
            "environment",
            environment.id.to_string(),
            Some(environment.project_id.to_string()),
            encode(environment)?,
        );
    }
    for message in &registry.queued_messages {
        push(
            "queued_message",
            message.id.to_string(),
            Some(message.thread_id.to_string()),
            encode(message)?,
        );
    }
    for interaction in &registry.interactions {
        push(
            "interaction",
            interaction.id.to_string(),
            Some(interaction.thread_id.to_string()),
            encode(interaction)?,
        );
    }
    for section in &registry.thread_sections {
        push(
            "thread_section",
            section.id.to_string(),
            None,
            encode(section)?,
        );
    }
    for run in &snapshot.runs {
        push(
            "run",
            run.run_id.to_string(),
            Some(run.thread_id.to_string()),
            encode(run)?,
        );
    }
    if let Some(settings) = &snapshot.settings {
        push("settings", SINGLETON.to_owned(), None, encode(settings)?);
    }
    if let Some(automations) = &snapshot.automations {
        push(
            "automations",
            SINGLETON.to_owned(),
            None,
            encode(automations)?,
        );
    }
    Ok(rows)
}

fn encode<T: Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value)
        .map_err(|error| StoreError::new(format!("an entity did not serialize: {error}")))
}

fn decode<T: DeserializeOwned>(kind: &str, id: &str, json: &str) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|error| {
        StoreError::new(format!(
            "the stored {kind} {id} is not one any more: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real view from the registry, so the fixture is the shape the server
    /// actually produces rather than one this test invented.
    fn fixture() -> DomainSnapshot {
        let registry = crate::domain_state::DomainRegistry::new(1);
        registry
            .create_thread(
                Some(registry.personal_project_id()),
                Some("a thread".to_owned()),
                None,
                2,
            )
            .expect("the thread is created");
        DomainSnapshot {
            version: SNAPSHOT_VERSION,
            watermark: Some(loom_relay::EventId::new()),
            registry: registry.export(),
            runs: vec![],
            settings: None,
            automations: None,
        }
    }

    /// A store nothing has written to holds no view, which is a different answer
    /// from a view that held nothing.
    #[test]
    fn an_empty_store_has_no_view() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.entities().unwrap(), None);
    }

    /// What goes in comes back, field for field: the DB is now the view's home,
    /// and recovery reads it as the snapshot did.
    #[test]
    fn the_entity_view_round_trips() {
        let store = Store::open_in_memory().unwrap();
        let written = fixture();
        store.replace_entities(&written).unwrap();
        assert_eq!(store.entities().unwrap(), Some(written));
    }

    /// Replacing is whole: an entity the new view does not have is gone, not
    /// left behind for a reader to find.
    #[test]
    fn replacing_the_view_leaves_nothing_of_the_old_one() {
        let store = Store::open_in_memory().unwrap();
        let first = fixture();
        store.replace_entities(&first).unwrap();

        let mut second = fixture();
        second.registry.hosts.clear();
        second.watermark = None;
        store.replace_entities(&second).unwrap();

        let read = store.entities().unwrap().unwrap();
        assert!(read.registry.hosts.is_empty());
        assert_eq!(read.watermark, None);
        assert_eq!(
            read.registry.personal_project_id.to_string(),
            "proj_personal"
        );
    }

    /// An entity that no longer parses is an error: a view with a hole in it
    /// would silently drop a project.
    #[test]
    fn an_unreadable_entity_fails_the_read() {
        let store = Store::open_in_memory().unwrap();
        store.replace_entities(&fixture()).unwrap();
        crate::store::block_on(store.connection().execute(
            "UPDATE entity SET json = 'not an entity' WHERE kind = 'thread'",
            (),
        ))
        .unwrap();
        let error = store
            .entities()
            .expect_err("an unreadable entity is refused");
        assert!(
            error.to_string().contains("is not one any more"),
            "the failure names the entity: {error}"
        );
    }

    /// A thread's project is a column, so a lookup by parent does not read every
    /// entity's JSON.
    #[test]
    fn parents_are_columns() {
        let store = Store::open_in_memory().unwrap();
        let written = fixture();
        let thread = written
            .registry
            .threads
            .first()
            .expect("the fixture has a thread")
            .clone();
        store.replace_entities(&written).unwrap();

        let mut rows = crate::store::block_on(store.connection().query(
            "SELECT id FROM entity WHERE kind = 'thread' AND parent_id = ?1",
            (thread.project_id.to_string(),),
        ))
        .unwrap();
        let row = crate::store::block_on(rows.next()).unwrap().expect("a row");
        assert_eq!(column_text(&row, 0).unwrap(), thread.id.to_string());
    }
}
