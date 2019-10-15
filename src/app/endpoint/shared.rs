use std::time::Duration;

use chrono::{DateTime, Utc};
use diesel::pg::PgConnection;
use serde::Serialize;
use svc_agent::mqtt::{
    IncomingRequest, LongTermTimingProperties, Measurable, OutgoingEvent, OutgoingEventProperties,
    Publishable, ResponseStatus, ShortTermTimingProperties,
};
use svc_agent::{Addressable, AgentId};
use svc_error::Error as SvcError;

use crate::app::endpoint;
use crate::db::{agent, room};

pub(crate) fn respond<R, O: 'static + Clone + Serialize>(
    inreq: &IncomingRequest<R>,
    object: O,
    notification: Option<(&'static str, &str)>,
    start_timestamp: DateTime<Utc>,
    authz_time: Option<Duration>,
) -> endpoint::Result {
    let short_term_timing = build_short_term_timing(start_timestamp, authz_time);

    let resp = inreq.to_response(
        object.clone(),
        ResponseStatus::OK,
        short_term_timing.clone(),
    );

    let mut messages: Vec<Box<dyn Publishable>> = vec![Box::new(resp)];

    if let Some((label, topic)) = notification {
        let props = OutgoingEventProperties::new(label, short_term_timing);
        messages.push(Box::new(OutgoingEvent::broadcast(object, props, topic)));
    }

    messages.into()
}

pub(crate) fn build_short_term_timing(
    start_timestamp: DateTime<Utc>,
    authz_time: Option<Duration>,
) -> ShortTermTimingProperties {
    let now = Utc::now();
    let mut timing = ShortTermTimingProperties::new(now);

    if let Ok(value) = (now - start_timestamp).to_std() {
        timing.set_processing_time(value);
    }

    if let Some(value) = authz_time {
        timing.set_authorization_time(value);
    }

    timing
}

pub(crate) fn build_timings<P: Addressable + Measurable>(
    incoming_properties: &P,
    start_timestamp: DateTime<Utc>,
    authz_time: Option<Duration>,
) -> (LongTermTimingProperties, ShortTermTimingProperties) {
    let short_term_timing = build_short_term_timing(start_timestamp, authz_time);

    let long_term_timing = incoming_properties
        .long_term_timing()
        .clone()
        .update_cumulative_timings(&short_term_timing);

    (long_term_timing, short_term_timing)
}

pub(crate) fn check_room_presence(
    room: &room::Object,
    agent_id: &AgentId,
    conn: &PgConnection,
) -> Result<(), SvcError> {
    let results = agent::ListQuery::new()
        .room_id(room.id())
        .agent_id(agent_id)
        .execute(conn)?;

    if results.len() == 0 {
        let err = SvcError::builder()
            .status(ResponseStatus::NOT_FOUND)
            .detail(&format!(
                "agent = '{}' is not online in the room = '{}'",
                agent_id,
                room.id()
            ))
            .build();

        Err(err)
    } else {
        Ok(())
    }
}
