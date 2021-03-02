use std::fmt;

use chrono::serde::ts_seconds;
use chrono::{DateTime, Utc};
use diesel::{pg::PgConnection, result::Error};
use serde_derive::{Deserialize, Serialize};
use svc_agent::AgentId;
use uuid::Uuid;

use super::room::Object as Room;
use crate::schema::rtc;

////////////////////////////////////////////////////////////////////////////////

pub(crate) type AllColumns = (rtc::id, rtc::room_id, rtc::created_at, rtc::created_by);

pub(crate) const ALL_COLUMNS: AllColumns =
    (rtc::id, rtc::room_id, rtc::created_at, rtc::created_by);

////////////////////////////////////////////////////////////////////////////////

#[derive(Clone, Copy, Debug, DbEnum, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
#[DieselType = "Rtc_sharing_policy"]
pub(crate) enum SharingPolicy {
    None,
    Shared,
    Owned,
}

impl fmt::Display for SharingPolicy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let serialized = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        write!(f, "{}", serialized)
    }
}

////////////////////////////////////////////////////////////////////////////////

#[derive(
    Clone, Debug, Serialize, Deserialize, Identifiable, Queryable, QueryableByName, Associations,
)]
#[belongs_to(Room, foreign_key = "room_id")]
#[table_name = "rtc"]
pub(crate) struct Object {
    id: Uuid,
    room_id: Uuid,
    #[serde(with = "ts_seconds")]
    created_at: DateTime<Utc>,
    created_by: AgentId,
}

impl Object {
    pub(crate) fn id(&self) -> Uuid {
        self.id
    }

    pub(crate) fn room_id(&self) -> Uuid {
        self.room_id
    }

    pub(crate) fn created_by(&self) -> &AgentId {
        &self.created_by
    }
}

////////////////////////////////////////////////////////////////////////////////

pub(crate) struct FindQuery {
    id: Option<Uuid>,
}

impl FindQuery {
    pub(crate) fn new() -> Self {
        Self { id: None }
    }

    pub(crate) fn id(mut self, id: Uuid) -> Self {
        self.id = Some(id);
        self
    }

    pub(crate) fn execute(&self, conn: &PgConnection) -> Result<Option<Object>, Error> {
        use diesel::prelude::*;

        match self.id {
            Some(id) => rtc::table.find(id).get_result(conn).optional(),
            _ => Err(Error::QueryBuilderError(
                "id is required parameters of the query".into(),
            )),
        }
    }
}

////////////////////////////////////////////////////////////////////////////////

#[derive(Default)]
pub(crate) struct ListQuery {
    room_id: Option<Uuid>,
    offset: Option<i64>,
    limit: Option<i64>,
}

impl ListQuery {
    pub(crate) fn new() -> Self {
        Default::default()
    }

    pub(crate) fn room_id(self, room_id: Uuid) -> Self {
        Self {
            room_id: Some(room_id),
            ..self
        }
    }

    pub(crate) fn offset(self, offset: i64) -> Self {
        Self {
            offset: Some(offset),
            ..self
        }
    }

    pub(crate) fn limit(self, limit: i64) -> Self {
        Self {
            limit: Some(limit),
            ..self
        }
    }

    pub(crate) fn execute(&self, conn: &PgConnection) -> Result<Vec<Object>, Error> {
        use diesel::prelude::*;

        let mut q = rtc::table.into_boxed();

        if let Some(room_id) = self.room_id {
            q = q.filter(rtc::room_id.eq(room_id));
        }

        if let Some(offset) = self.offset {
            q = q.offset(offset);
        }

        if let Some(limit) = self.limit {
            q = q.limit(limit);
        }

        q.order_by(rtc::created_at.asc()).get_results(conn)
    }
}

////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Insertable)]
#[table_name = "rtc"]
pub(crate) struct InsertQuery<'a> {
    id: Option<Uuid>,
    room_id: Uuid,
    created_by: &'a AgentId,
}

impl<'a> InsertQuery<'a> {
    pub(crate) fn new(room_id: Uuid, created_by: &'a AgentId) -> Self {
        Self {
            id: None,
            room_id,
            created_by,
        }
    }

    pub(crate) fn execute(&self, conn: &PgConnection) -> Result<Object, Error> {
        use crate::schema::rtc::dsl::rtc;
        use diesel::RunQueryDsl;

        diesel::insert_into(rtc).values(self).get_result(conn)
    }
}
