use crate::{
    app::{context::GlobalContext, error::Error},
    backend::janus::client::update_agent_reader_config::{
        UpdateReaderConfigRequest, UpdateReaderConfigRequestBody,
        UpdateReaderConfigRequestBodyConfigItem,
    },
    db::{self, room::FindQueryable},
};
use anyhow::{anyhow, Context};
use std::{convert::TryFrom, str::FromStr, sync::Arc};
use svc_authz::Authenticable;
use svc_events::{
    stage::{SendNotificationStageV1, UpdateJanusConfigStageV1},
    Event, EventV1,
};
use svc_nats_client::{
    consumer::{FailureKind, FailureKindExt, HandleMessageFailure},
    Subject,
};
use uuid::Uuid;

use crate::app::{
    error::{ErrorExt, ErrorKind},
    stage::video_group::{MQTT_NOTIFICATION_LABEL, SUBJECT_PREFIX},
};

pub mod nats_ids;
pub mod video_group;

pub async fn route_message(
    ctx: Arc<dyn GlobalContext + Sync + Send>,
    msg: Arc<svc_nats_client::Message>,
) -> Result<(), HandleMessageFailure<anyhow::Error>> {
    let subject = Subject::from_str(&msg.subject)
        .context("parse nats subject")
        .permanent()?;

    let event = serde_json::from_slice::<Event>(msg.payload.as_ref())
        .context("parse nats payload")
        .permanent()?;

    let classroom_id = subject.classroom_id();
    let room = {
        let mut conn = ctx
            .get_conn()
            .await
            .map_err(anyhow::Error::from)
            .transient()?;

        db::room::FindQuery::by_classroom_id(classroom_id)
            .execute(&mut conn)
            .await
            .context("find room by classroom_id")
            .transient()?
            .ok_or(anyhow!(
                "failed to get room by classroom_id: {}",
                classroom_id
            ))
            .permanent()?
    };

    tracing::info!(?event, class_id = %classroom_id);

    let headers = svc_nats_client::Headers::try_from(msg.headers.clone().unwrap_or_default())
        .context("parse nats headers")
        .permanent()?;
    let _agent_id = headers.sender_id();

    let r: Result<(), HandleMessageFailure<Error>> = match event {
        Event::V1(EventV1::UpdateJanusConfigStage(e)) => {
            handle_update_janus_config_stage(ctx.as_ref(), e, classroom_id, &room).await
        }
        Event::V1(EventV1::SendNotificationStage(_e)) => {
            handle_send_notification_stage(ctx.as_ref(), &room).await
        }
        _ => {
            // ignore
            Ok(())
        }
    };

    FailureKindExt::map_err(r, |e| anyhow!(e))
}

async fn handle_update_janus_config_stage(
    ctx: &(dyn GlobalContext + Sync),
    e: UpdateJanusConfigStageV1,
    classroom_id: Uuid,
    room: &db::room::Object,
) -> Result<(), HandleMessageFailure<Error>> {
    let mut conn = ctx.get_conn().await.transient()?;

    let janus_backend = db::janus_backend::FindQuery::new(&e.backend_id)
        .execute(&mut conn)
        .await
        .error(ErrorKind::DbQueryFailed)
        .transient()?
        .ok_or_else(|| anyhow!("Janus backend not found"))
        .error(ErrorKind::BackendNotFound)
        .transient()?;

    let rtcs = db::rtc::ListQuery::new()
        .room_id(room.id)
        .execute(&mut conn)
        .await
        .error(ErrorKind::DbQueryFailed)
        .transient()?;

    let target_rtc = rtcs
        .iter()
        .find(|r| *r.created_by().as_account_id() == e.target_account)
        .ok_or_else(|| anyhow!("agent is not in the room"))
        .error(ErrorKind::AgentNotConnected)
        .transient()?;

    let mut configs = vec![];

    for rtc in rtcs.iter() {
        let receive_video = room
            .host()
            // always receive video for hosts
            .map(|h| h == rtc.created_by())
            .unwrap_or(true);

        let receive_audio = true;
        let stream_id = target_rtc.id;
        let reader_id = rtc.created_by();

        let cfg = UpdateReaderConfigRequestBodyConfigItem {
            reader_id: reader_id.clone(),
            stream_id,
            receive_video,
            receive_audio,
        };
        configs.push(cfg);
    }

    let request = UpdateReaderConfigRequest {
        session_id: janus_backend.session_id(),
        handle_id: janus_backend.handle_id(),
        body: UpdateReaderConfigRequestBody::new(configs.clone()),
    };

    ctx.janus_clients()
        .get_or_insert(&janus_backend)
        .error(ErrorKind::BackendClientCreationFailed)
        .transient()?
        .reader_update(request)
        .await
        .context("Reader update")
        .error(ErrorKind::BackendRequestFailed)
        .transient()?;

    let event = Event::from(SendNotificationStageV1 {});

    let payload = serde_json::to_vec(&event)
        .error(ErrorKind::InvalidPayload)
        .permanent()?;

    let event_id = crate::app::stage::nats_ids::sqlx::InsertQuery::new("conference_internal_event")
        .execute(&mut conn)
        .await
        .error(ErrorKind::InsertEventIdFailed)
        .transient()?;

    let subject = svc_nats_client::Subject::new(
        SUBJECT_PREFIX.to_string(),
        classroom_id,
        event_id.entity_type().to_string(),
    );

    let event = svc_nats_client::event::Builder::new(
        subject,
        payload,
        event_id.to_owned(),
        ctx.agent_id().to_owned(),
    )
    .build();

    ctx.nats_client()
        .ok_or_else(|| anyhow!("nats client not found"))
        .error(ErrorKind::NatsClientNotFound)
        .transient()?
        .publish(&event)
        .await
        .error(ErrorKind::NatsPublishFailed)
        .transient()?;

    Ok(())
}

async fn handle_send_notification_stage(
    ctx: &(dyn GlobalContext + Sync),
    room: &db::room::Object,
) -> Result<(), HandleMessageFailure<Error>> {
    let topic = format!("rooms/{}/events", room.id);

    ctx.mqtt_client()
        .lock()
        .publish(MQTT_NOTIFICATION_LABEL, &topic)
        .error(ErrorKind::MqttPublishFailed)
        .transient()?;

    Ok(())
}
