-- This Source Code Form is subject to the terms of the Mozilla Public
-- License, v. 2.0. If a copy of the MPL was not distributed with this
-- file, You can obtain one at http://mozilla.org/MPL/2.0/.

-- Temp tables which only need to be created on the write connection.
CREATE TEMP TABLE moz_extension_data_staging (
    guid TEXT PRIMARY KEY,
    /* The extension_id is explicitly not the GUID used on the server.
       We may end up making this a regular foreign-key relationship back to
       moz_extension_data, although maybe not - the ext_id may not exist in
       moz_extension_data at the time we populate this table.
       We can iterate here as we site up sync support.
    */
    ext_id TEXT NOT NULL UNIQUE,

    /* Timestamp as recorded by the server - XXX - we don't currently use this
       for merging, so should consider killing it?
    */
    server_modified INTEGER NOT NULL,
    /* The JSON payload. We *do* allow NULL here - it means "deleted" */
    data TEXT
) WITHOUT ROWID;
