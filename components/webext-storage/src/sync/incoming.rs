/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// The "incoming" part of syncing - handling the incoming rows, staging them,
// working out a plan for them, updating the local data and mirror, etc.

use interrupt::Interruptee;
use rusqlite::{types::ToSql, Connection, Row};
use serde_json;
use sql_support::ConnExt;
use sync_guid::Guid as SyncGuid;

use crate::error::*;

use super::{merge, JsonMap, ServerPayload};

// This module deals exclusively with the Map inside a JsonValue::Object().
// This helper reads such a Map from a SQL row, ignoring anything which is
// either invalid JSON or a different JSON type.
fn json_map_from_row(row: &Row<'_>, col: &str) -> Result<Option<JsonMap>> {
    let s = row.get::<_, Option<String>>(col)?;
    Ok(match s {
        None => None,
        Some(s) => match serde_json::from_str(&s) {
            Ok(serde_json::Value::Object(m)) => Some(m),
            _ => {
                // We don't want invalid json or wrong types to kill syncing -
                // but it should be impossible as we never write anything which
                // could cause it, so logging shouldn't hurt.
                log::warn!("skipping invalid json in {}", col);
                None
            }
        },
    })
}

/// The first thing we do with incoming items is to "stage" them in a temp table.
/// The actual processing is done via this table.
pub fn stage_incoming<S: ?Sized + Interruptee>(
    conn: &Connection,
    incoming_bsos: Vec<ServerPayload>,
    signal: &S,
) -> Result<()> {
    // markh always struggles with the sql_support chunking :( So take the
    // low road...
    let cext = conn.conn();
    let tx = cext.unchecked_transaction()?;
    let sql = "
        INSERT OR REPLACE INTO temp.moz_extension_data_staging
        (guid, ext_id, data, server_modified)
        VALUES (:guid, :ext_id, :data, :ts)";
    for bso in incoming_bsos {
        signal.err_if_interrupted()?;
        cext.execute_named_cached(
            &sql,
            &[
                (":guid", &bso.guid as &dyn ToSql),
                (":ext_id", &bso.ext_id),
                (":data", &bso.data),
                (":ts", &bso.last_modified.as_millis()),
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Details about an incoming item.
#[derive(Debug, PartialEq)]
pub struct IncomingItem {
    guid: SyncGuid,
    ext_id: String,
}

/// The "state" we find ourselves in when considering an incoming/staging
/// record. This "state" is the input to calculating the IncomingAction.
#[derive(Debug, PartialEq)]
pub enum IncomingState {
    IncomingOnly {
        incoming: Option<JsonMap>,
    },
    HasLocal {
        incoming: Option<JsonMap>,
        local: Option<JsonMap>,
    },
    NotLocal {
        incoming: Option<JsonMap>,
        mirror: Option<JsonMap>,
    },
    Everywhere {
        incoming: Option<JsonMap>,
        mirror: Option<JsonMap>,
        local: Option<JsonMap>,
    },
}

/// Get the items we need to process from the staging table. Return details about
/// the item and the state of that item, ready for processing.
pub fn get_incoming(conn: &Connection) -> Result<Vec<(IncomingItem, IncomingState)>> {
    let sql = "
        SELECT
            s.guid as guid,
            m.guid IS NOT NULL as m_exists,
            l.ext_id IS NOT NULL as l_exists,
            s.ext_id as ext_id,
            s.data as s_data, m.data as m_data, l.data as l_data,
            l.sync_change_counter
        FROM temp.moz_extension_data_staging s
        LEFT JOIN moz_extension_data_mirror m ON m.guid = s.guid
        LEFT JOIN moz_extension_data l on l.ext_id = s.ext_id;";

    fn from_row(row: &Row<'_>) -> Result<(IncomingItem, IncomingState)> {
        let guid = row.get("guid")?;
        let ext_id = row.get("ext_id")?;
        let incoming = json_map_from_row(row, "s_data")?;

        let mirror_exists = row.get("m_exists")?;
        let local_exists = row.get("l_exists")?;

        let state = match (local_exists, mirror_exists) {
            (false, false) => IncomingState::IncomingOnly { incoming },
            (true, false) => IncomingState::HasLocal {
                incoming,
                local: json_map_from_row(row, "l_data")?,
            },
            (false, true) => IncomingState::NotLocal {
                incoming,
                mirror: json_map_from_row(row, "m_data")?,
            },
            (true, true) => IncomingState::Everywhere {
                incoming,
                mirror: json_map_from_row(row, "m_data")?,
                local: json_map_from_row(row, "l_data")?,
            },
        };
        Ok((IncomingItem { guid, ext_id }, state))
    }

    Ok(conn.conn().query_rows_and_then_named(sql, &[], from_row)?)
}

/// This is the set of actions we know how to take *locally* for incoming
/// records. Which one depends on the IncomingState.
#[derive(Debug, PartialEq)]
pub enum IncomingAction {
    // Something's wrong with this entry - probably JSON?
    // (but we seem to be getting away without this for now)
    //Invalid { reason: String },
    /// We should locally delete the data for this record
    DeleteLocally,
    /// We will take the remote.
    TakeRemote { data: JsonMap },
    /// We merged this data - this is what we came up with.
    Merge { data: JsonMap },
    /// Entry exists locally and it's the same as the incoming record.
    Same,
}

/// Takes the state of an item and returns the action we should take for it.
pub fn plan_incoming(s: IncomingState) -> IncomingAction {
    match s {
        IncomingState::Everywhere {
            incoming,
            local,
            mirror,
        } => {
            // All records exist - but do they all have data?
            match (incoming, local, mirror) {
                (Some(id), Some(ld), Some(md)) => {
                    // all records have data - 3-way merge.
                    merge(id, ld, Some(md))
                }
                (Some(id), Some(ld), None) => {
                    // No parent, so first time seeing this remotely - 2-way merge
                    merge(id, ld, None)
                }
                (Some(id), None, _) => {
                    // Local Incoming data, removed locally. Server wins.
                    IncomingAction::TakeRemote { data: id }
                }
                (None, _, _) => {
                    // Deleted remotely. Server wins.
                    // XXX - WRONG - we want to 3 way merge here still!
                    // Eg, final key removed remotely, different key added
                    // locally, the new key should still be added.
                    IncomingAction::DeleteLocally
                }
            }
        }
        IncomingState::HasLocal { incoming, local } => {
            // So we have a local record and an incoming/staging record, but *not* a
            // mirror record. This means some other device has synced this for
            // the first time and we are yet to do the same.
            match (incoming, local) {
                (Some(id), Some(ld)) => {
                    // This means the extension exists locally and remotely
                    // but this is the first time we've synced it. That's no problem, it's
                    // just a 2-way merge...
                    merge(id, ld, None)
                }
                (Some(_), None) => {
                    // We've data locally, but there's an incoming deletion.
                    // Remote wins.
                    IncomingAction::DeleteLocally
                }
                (None, Some(data)) => {
                    // No data locally, but some is incoming - take it.
                    IncomingAction::TakeRemote { data }
                }
                (None, None) => {
                    // Nothing anywhere - odd, but OK.
                    IncomingAction::Same
                }
            }
        }
        IncomingState::NotLocal { incoming, .. } => {
            // No local data but there's mirror and an incoming record.
            // This means a local deletion is being replaced by, or just re-doing
            // the incoming record.
            match incoming {
                Some(data) => IncomingAction::TakeRemote { data },
                None => IncomingAction::Same,
            }
        }
        IncomingState::IncomingOnly { incoming } => {
            // Only the staging record exists - this means it's the first time
            // we've ever seen it. No conflict possible, just take the remote.
            match incoming {
                Some(data) => IncomingAction::TakeRemote { data },
                None => IncomingAction::DeleteLocally,
            }
        }
    }
}

// Apply the actions necessary to fully process the incoming items.
pub fn apply_actions<S: ?Sized + Interruptee>(
    conn: &Connection,
    actions: Vec<(IncomingItem, IncomingAction)>,
    signal: &S,
) -> Result<()> {
    let cext = conn.conn();
    let tx = cext.unchecked_transaction()?;
    for (item, action) in actions {
        signal.err_if_interrupted()?;

        log::trace!("action for '{}': {:?}", item.ext_id, action);
        // XXX - change counter should be updated consistently here!
        match action {
            IncomingAction::DeleteLocally => {
                // Can just nuke it entirely.
                cext.execute_named_cached(
                    "DELETE FROM moz_extension_data WHERE ext_id = :ext_id",
                    &[(":ext_id", &item.ext_id)],
                )?;
            }
            // We want to update the local record with 'data' and after this update the item no longer is considered dirty.
            IncomingAction::TakeRemote { data } => {
                cext.execute_named_cached(
                    "UPDATE moz_extension_data SET data = :data, sync_change_counter = 0 WHERE ext_id = :ext_id",
                    &[
                        (":ext_id", &item.ext_id),
                        (":data", &serde_json::Value::Object(data).as_str()),
                    ]
                )?;
            }

            // We merged this data, so need to update locally but still consider
            // it dirty because the merged data must be uploaded.
            IncomingAction::Merge { data } => {
                cext.execute_named_cached(
                    "UPDATE moz_extension_data SET data = :data, sync_change_counter = sync_change_counter + 1 WHERE ext_id = :ext_id",
                    &[
                        (":ext_id", &item.ext_id),
                        (":data", &serde_json::Value::Object(data).to_string()),
                    ]
                )?;
            }

            // Both local and remote ended up the same - only need to nuke the
            // change counter.
            IncomingAction::Same => {
                cext.execute_named_cached(
                    "UPDATE moz_extension_data SET sync_change_counter = 0 WHERE ext_id = :ext_id",
                    &[(":ext_id", &item.ext_id)],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;
    use crate::db::test::new_mem_db;
    use interrupt::NeverInterrupts;
    use rusqlite::NO_PARAMS;
    use serde_json::{json, Value};

    // select simple int
    fn ssi(conn: &Connection, stmt: &str) -> u32 {
        conn.query_row_and_then(stmt, rusqlite::NO_PARAMS, |row| row.get::<_, u32>(0))
            .expect("must work")
    }

    fn array_to_incoming(mut array: Value) -> Vec<ServerPayload> {
        let jv = array.as_array_mut().expect("you must pass a json array");
        let mut result = Vec::with_capacity(jv.len());
        for elt in jv {
            result.push(serde_json::from_value(elt.take()).expect("must be valid"));
        }
        result
    }

    // Can't find a way to import this from crate::sync::tests...
    macro_rules! map {
        ($($map:tt)+) => {
            json!($($map)+).as_object().unwrap().clone()
        };
    }

    #[test]
    fn test_incoming_populates_staging() -> Result<()> {
        let db = new_mem_db();
        let conn = db.writer.lock().unwrap();

        let incoming = json! {[
            {
                "guid": "guidAAAAAAAA",
                "last_modified": 0.0,
                "ext_id": "ext1@example.com",
                "data": json!({"foo": "bar"}).to_string(),
            }
        ]};

        stage_incoming(&conn, array_to_incoming(incoming), &NeverInterrupts)?;
        // check staging table
        assert_eq!(
            ssi(
                &conn,
                "SELECT count(*) FROM temp.moz_extension_data_staging"
            ),
            1
        );
        Ok(())
    }

    #[test]
    fn test_fetch_incoming_state() -> Result<()> {
        let db = new_mem_db();
        let conn = db.writer.lock().unwrap();

        // Start with an item just in staging.
        conn.execute(
            r#"
            INSERT INTO temp.moz_extension_data_staging (guid, ext_id, data, server_modified)
            VALUES ('guid', 'ext_id', '{"foo":"bar"}', 1)
        "#,
            NO_PARAMS,
        )?;

        let incoming = get_incoming(&conn)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].0,
            IncomingItem {
                guid: SyncGuid::new("guid"),
                ext_id: "ext_id".into()
            }
        );
        assert_eq!(
            incoming[0].1,
            IncomingState::IncomingOnly {
                incoming: Some(map!({"foo": "bar"})),
            }
        );

        // Add the same item to the mirror.
        conn.execute(
            r#"
            INSERT INTO moz_extension_data_mirror (guid, ext_id, data, server_modified)
            VALUES ('guid', 'ext_id', '{"foo":"new"}', 2)
        "#,
            NO_PARAMS,
        )?;
        let incoming = get_incoming(&conn)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].1,
            IncomingState::NotLocal {
                incoming: Some(map!({"foo": "bar"})),
                mirror: Some(map!({"foo": "new"})),
            }
        );

        // and finally the data itself - might as use the API here!
        api::set(&conn, "ext_id", json!({"foo": "local"}))?;
        let incoming = get_incoming(&conn)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].1,
            IncomingState::Everywhere {
                incoming: Some(map!({"foo": "bar"})),
                local: Some(map!({"foo": "local"})),
                mirror: Some(map!({"foo": "new"})),
            }
        );
        Ok(())
    }

    // Like test_fetch_incoming_state, but check NULLs are handled correctly.
    #[test]
    fn test_fetch_incoming_state_nulls() -> Result<()> {
        let db = new_mem_db();
        let conn = db.writer.lock().unwrap();

        // Start with an item just in staging.
        conn.execute(
            r#"
            INSERT INTO temp.moz_extension_data_staging (guid, ext_id, data, server_modified)
            VALUES ('guid', 'ext_id', NULL, 1)
        "#,
            NO_PARAMS,
        )?;

        let incoming = get_incoming(&conn)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].1,
            IncomingState::IncomingOnly { incoming: None }
        );

        // Add the same item to the mirror.
        conn.execute(
            r#"
            INSERT INTO moz_extension_data_mirror (guid, ext_id, data, server_modified)
            VALUES ('guid', 'ext_id', NULL, 2)
        "#,
            NO_PARAMS,
        )?;
        let incoming = get_incoming(&conn)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].1,
            IncomingState::NotLocal {
                incoming: None,
                mirror: None,
            }
        );

        conn.execute(
            r#"
            INSERT INTO moz_extension_data (ext_id, data)
            VALUES ('ext_id', NULL)
        "#,
            NO_PARAMS,
        )?;
        let incoming = get_incoming(&conn)?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(
            incoming[0].1,
            IncomingState::Everywhere {
                incoming: None,
                local: None,
                mirror: None,
            }
        );
        Ok(())
    }

    // XXX - test apply_actions!
}
