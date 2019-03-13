use failure::{format_err, Error};
use serde_derive::Deserialize;
use svc_agent::mqtt::compat::IntoEnvelope;
use svc_agent::mqtt::{
    IncomingRequest, OutgoingEvent, OutgoingEventProperties, OutgoingResponse,
    OutgoingResponseStatus, Publishable,
};
use uuid::Uuid;

use crate::db::{janus_rtc_stream, janus_rtc_stream::Time, room, ConnectionPool};

////////////////////////////////////////////////////////////////////////////////

const MAX_LIMIT: i64 = 25;

////////////////////////////////////////////////////////////////////////////////

pub(crate) type ListRequest = IncomingRequest<ListRequestData>;

#[derive(Debug, Deserialize)]
pub(crate) struct ListRequestData {
    room_id: Uuid,
    rtc_id: Option<Uuid>,
    #[serde(with = "crate::serde::ts_seconds_option_bound_tuple")]
    time: Option<Time>,
    offset: Option<i64>,
    limit: Option<i64>,
}

pub(crate) type ObjectListResponse = OutgoingResponse<Vec<janus_rtc_stream::Object>>;
pub(crate) type ObjectUpdateEvent = OutgoingEvent<janus_rtc_stream::Object>;

////////////////////////////////////////////////////////////////////////////////

pub(crate) struct State {
    authz: svc_authz::ClientMap,
    db: ConnectionPool,
}

impl State {
    pub(crate) fn new(authz: svc_authz::ClientMap, db: ConnectionPool) -> Self {
        Self { authz, db }
    }
}

impl State {
    pub(crate) fn list(&self, inreq: &ListRequest) -> Result<impl Publishable, Error> {
        let room_id = inreq.payload().room_id;

        // Authorization: room's owner has to allow the action
        {
            let conn = self.db.get()?;
            let room = room::FindQuery::new()
                .id(room_id)
                .execute(&conn)?
                .ok_or_else(|| format_err!("the room = '{}' is not found", &room_id))?;

            let room_id = room.id().to_string();
            self.authz.authorize(
                room.audience(),
                inreq.properties(),
                vec!["rooms", &room_id, "rtcs"],
                "list",
            )?;
        };

        let objects = {
            let conn = self.db.get()?;
            janus_rtc_stream::ListQuery::from((
                Some(room_id),
                inreq.payload().rtc_id,
                inreq.payload().time,
                inreq.payload().offset,
                Some(std::cmp::min(
                    inreq.payload().limit.unwrap_or_else(|| MAX_LIMIT),
                    MAX_LIMIT,
                )),
            ))
            .execute(&conn)?
        };

        let resp = inreq.to_response(objects, OutgoingResponseStatus::OK);
        resp.into_envelope()
    }
}

////////////////////////////////////////////////////////////////////////////////

pub(crate) fn update_event(room_id: Uuid, object: janus_rtc_stream::Object) -> ObjectUpdateEvent {
    let uri = format!("rooms/{}/events", room_id);
    OutgoingEvent::broadcast(
        object,
        OutgoingEventProperties::new("rtc_stream.update"),
        &uri,
    )
}
