/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

mod incoming;
mod outgoing;

use serde_derive::*;
use serde_json;
use sync15::ServerTimestamp;
use sync_guid::Guid as SyncGuid;

use incoming::IncomingAction;

type JsonMap = serde_json::Map<String, serde_json::Value>;

// We use the same values as places. Note that some of these values are
// duplicated in the schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum SyncStatus {
    Unknown = 0,
    New = 1,
    Normal = 2,
}

/// For use with `#[serde(skip_serializing_if = )]`
#[inline]
pub fn is_default<T: PartialEq + Default>(v: &T) -> bool {
    *v == T::default()
}

// XXX - need to work out how to consolidate "payload" vs "bso", particularly
// the timestamp, etc. For now, we represent this in ServerPayload, even though
// that's never actually stored in this way.
// XXX - this isn't going to work without other changes.
#[derive(Debug, Serialize, Deserialize)]
struct ServerPayload {
    guid: SyncGuid,
    ext_id: String,
    #[serde(default, skip_serializing_if = "is_default")]
    data: Option<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    deleted: bool,
    last_modified: ServerTimestamp,
}

// Perform a 2-way or 3-way merge, where the incoming value wins on confict.
// XXX - this needs more thought, and probably needs significant changes.
// Main problem is that it doesn't handle deletions - but to do that, we need
// something other than a simple Option<JsonMap> - we need to differentiate
// "doesn't exist" from "removed".
// TODO!
fn merge(other: JsonMap, mut ours: JsonMap, parent: Option<JsonMap>) -> IncomingAction {
    if other == ours {
        return IncomingAction::Same;
    }
    // Server wins. Iterate over incoming - if incoming and the parent are
    // identical, then we will take our local value.
    for (k, iv) in other.into_iter() {
        let vour = ours.get(&k);
        match vour {
            Some(vour) => {
                if *vour != iv {
                    // So we have a discrepency between 'ours' and 'other' - use parent
                    // to resolve.
                    let can_take_local = match parent {
                        Some(ref pm) => {
                            if let Some(pv) = pm.get(&k) {
                                // parent has a value - we can only take our local
                                // value if the parent and incoming have the same.
                                *pv == iv
                            } else {
                                // Value doesn't exist in the parent - can't take local
                                false
                            }
                        }
                        None => {
                            // 2 way merge because there's no parent. We always
                            // prefer incoming here.
                            false
                        }
                    };
                    if can_take_local {
                        log::trace!("merge: no remote change in key {} - taking local", k);
                    } else {
                        log::trace!("merge: conflict in existing key {} - taking remote", k);
                        ours.insert(k, iv);
                    }
                } else {
                    log::trace!("merge: local and incoming same for key {}", k);
                }
            }
            None => {
                log::trace!("merge: incoming new value for key {}", k);
                ours.insert(k, iv);
            }
        }
    }
    IncomingAction::Merge { data: ours }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::*;
    use serde_json::json;

    // a macro for these tests - constructs a serde_json::Value::Object
    macro_rules! map {
        ($($map:tt)+) => {
            json!($($map)+).as_object().unwrap().clone()
        };
    }

    #[test]
    fn test_3way_merging() -> Result<()> {
        // fn merge(other: JsonMap, mut ours: JsonMap, mut parent: JsonMap) -> IncomingAction {
        // No conflict.
        // Identical local and remote.
        assert_eq!(
            merge(
                map!({"one": "one", "two": "two"}),
                map!({"two": "two", "one": "one"}),
                Some(map!({"parent_only": "parent"})),
            ),
            IncomingAction::Same
        );
        assert_eq!(
            merge(
                map!({"other_only": "other", "common": "common"}),
                map!({"ours_only": "ours", "common": "common"}),
                Some(map!({"parent_only": "parent", "common": "old_common"})),
            ),
            IncomingAction::Merge {
                data: map!({"other_only": "other", "ours_only": "ours", "common": "common"})
            }
        );
        // Simple conflict - parent value is neither local nor incoming. incoming wins.
        assert_eq!(
            merge(
                map!({"other_only": "other", "common": "incoming"}),
                map!({"ours_only": "ours", "common": "local"}),
                Some(map!({"parent_only": "parent", "common": "parent"})),
            ),
            IncomingAction::Merge {
                data: map!({"other_only": "other", "ours_only": "ours", "common": "incoming"})
            }
        );
        // Local change, no conflict.
        assert_eq!(
            merge(
                map!({"other_only": "other", "common": "old_value"}),
                map!({"ours_only": "ours", "common": "new_value"}),
                Some(map!({"parent_only": "parent", "common": "old_value"})),
            ),
            IncomingAction::Merge {
                data: map!({"other_only": "other", "ours_only": "ours", "common": "new_value"})
            }
        );
        Ok(())
    }

    // XXX - add `fn test_2way_merging() -> Result<()> {`!!
}