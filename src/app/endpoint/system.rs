use crate::{
    app::{
        context::{AppContext, Context},
        endpoint::{self, prelude::*},
        error::Error as AppError,
        service_utils::{RequestParams, Response},
    },
    authz::AuthzObject,
    backend::janus::client::upload_stream::{UploadResponse, UploadStreamRequest},
    config::UploadConfig,
    db,
    db::{
        recording::{self, Object as Recording, Status as RecordingStatus},
        room::{self, Object as Room},
        rtc::{self, SharingPolicy},
    },
};
use anyhow::anyhow;
use async_trait::async_trait;
use axum::extract::Extension;
use chrono::Utc;
use futures::stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{ops::Bound, result::Result as StdResult, sync::Arc};
use svc_agent::{
    mqtt::{
        IncomingEventProperties, IntoPublishableMessage, OutgoingEvent, OutgoingEventProperties,
        OutgoingMessage, ResponseStatus, ShortTermTimingProperties,
    },
    AgentId,
};
use svc_authn::Authenticable;
use svc_utils::extractors::AuthnExtractor;

use tracing::{error, info};
use tracing_attributes::instrument;

use super::MqttResult;

////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Serialize)]
pub struct RoomUploadEventData {
    id: db::room::Id,
    rtcs: Vec<RtcUploadEventData>,
}

#[derive(Debug, Serialize)]
struct RtcUploadEventData {
    id: db::rtc::Id,
    status: RecordingStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    uri: Option<String>,
    created_by: AgentId,
    mjr_dumps_uris: Option<Vec<String>>,
}

pub type RoomUploadEvent = OutgoingMessage<RoomUploadEventData>;

////////////////////////////////////////////////////////////////////////////////

#[derive(Serialize)]
struct ClosedRoomNotification {
    room_id: db::room::Id,
}

#[derive(Debug, Deserialize)]
pub struct VacuumRequest {}
pub async fn vacuum(
    Extension(ctx): Extension<Arc<AppContext>>,
    AuthnExtractor(agent_id): AuthnExtractor,
) -> RequestResult {
    let request = VacuumRequest {};
    VacuumHandler::handle(
        &mut ctx.start_message(),
        request,
        RequestParams::Http {
            agent_id: &agent_id,
        },
    )
    .await
}

pub struct VacuumHandler;

#[async_trait]
impl RequestHandler for VacuumHandler {
    type Payload = VacuumRequest;
    const ERROR_TITLE: &'static str = "Failed to vacuum system";

    #[instrument(skip(context, _payload, reqp))]
    async fn handle<C: Context>(
        context: &mut C,
        _payload: Self::Payload,
        reqp: RequestParams<'_>,
    ) -> RequestResult {
        let _t_imer = context
            .metrics()
            .request_duration
            .upload_stream
            .start_timer();
        // Authorization: only trusted subjects are allowed to perform operations with the system
        let audience = context.agent_id().as_account_id().audience();

        context
            .authz()
            .authorize(
                audience.into(),
                reqp,
                AuthzObject::new(&["system"]).into(),
                "update".into(),
            )
            .await?;

        let mut response = Response::new(
            ResponseStatus::NO_CONTENT,
            json!({}),
            context.start_timestamp(),
            None,
        );
        let conn = context.get_conn().await?;
        let group = context.config().janus_group.clone();
        let rooms = crate::util::spawn_blocking(move || {
            db::room::finished_with_in_progress_recordings(&conn, group.as_deref())
        })
        .await?;

        for (room, recording, backend) in rooms.into_iter() {
            let conn = context.get_conn().await?;
            let room_id = room.id();
            crate::util::spawn_blocking(move || {
                db::agent::DeleteQuery::new()
                    .room_id(room_id)
                    .execute(&conn)
            })
            .await?;

            let config = upload_config(context, &room)?;
            let request = UploadStreamRequest {
                id: recording.rtc_id(),
                backend: config.backend.clone(),
                bucket: config.bucket.clone(),
            };
            // TODO: Send the error as an event to "app/${APP}/audiences/${AUD}" topic
            let janus_response = context
                .janus_clients()
                .get_or_insert(&backend)
                .error(AppErrorKind::BackendClientCreationFailed)?
                .upload_stream(request)
                .await
                .error(AppErrorKind::BackendRequestFailed)?;

            // Publish room closed notification
            response.add_notification(
                "room.close",
                &format!("rooms/{}/events", room.id()),
                room,
                context.start_timestamp(),
            );
            match janus_response {
                UploadResponse::Missing { id } => {
                    let conn = context.get_conn().await?;
                    crate::util::spawn_blocking(move || {
                        recording::UpdateQuery::new(id)
                            .status(recording::Status::Missing)
                            .execute(&conn)
                    })
                    .await?;
                    error!(%id, "Janus is missing recording")
                }
                UploadResponse::AlreadyRunning { id } => {
                    info!(%id, "Vacuum already started")
                }
                UploadResponse::Done { id, mjr_dumps_uris } => {
                    let (room, rtcs_with_recs): (
                        room::Object,
                        Vec<(rtc::Object, Option<recording::Object>)>,
                    ) = {
                        let conn = context.get_conn().await?;
                        crate::util::spawn_blocking(move || {
                            recording::UpdateQuery::new(id)
                                .status(recording::Status::Ready)
                                .mjr_dumps_uris(mjr_dumps_uris)
                                .execute(&conn)?;

                            let rtc = rtc::FindQuery::new()
                                .id(id)
                                .execute(&conn)?
                                .ok_or_else(|| anyhow!("RTC not found"))
                                .error(AppErrorKind::RtcNotFound)?;

                            let room = endpoint::helpers::find_room_by_rtc_id(
                                rtc.id(),
                                endpoint::helpers::RoomTimeRequirement::Any,
                                &conn,
                            )?;

                            let rtcs_with_recs =
                                rtc::ListWithRecordingQuery::new(room.id()).execute(&conn)?;

                            Ok::<_, AppError>((room, rtcs_with_recs))
                        })
                        .await?
                    };
                    let room_done =
                        rtcs_with_recs.iter().all(
                            |(_rtc, maybe_recording)| match maybe_recording {
                                None => true,
                                Some(recording) => {
                                    recording.status() == db::recording::Status::Ready
                                }
                            },
                        );

                    if room_done {
                        let recs_with_rtcs =
                            rtcs_with_recs
                                .into_iter()
                                .filter_map(|(rtc, maybe_recording)| {
                                    let recording = maybe_recording?;
                                    matches!(recording.status(), db::recording::Status::Ready)
                                        .then(|| (recording, rtc))
                                });

                        let event = upload_event(context, &room, recs_with_rtcs.into_iter())?;

                        let event_box = Box::new(event)
                            as Box<dyn IntoPublishableMessage + Send + Sync + 'static>;
                        response.add_message(event_box);
                    }
                }
            }
        }

        Ok(response)
    }
}

#[derive(Debug, Deserialize)]
pub struct OrphanedRoomCloseEvent {}

pub struct OrphanedRoomCloseHandler;

#[async_trait]
impl EventHandler for OrphanedRoomCloseHandler {
    type Payload = OrphanedRoomCloseEvent;

    #[instrument(skip(context, _payload))]
    async fn handle<C: Context>(
        context: &mut C,
        _payload: Self::Payload,
        evp: &IncomingEventProperties,
    ) -> MqttResult {
        let audience = context.agent_id().as_account_id().audience();
        // Authorization: only trusted subjects are allowed to perform operations with the system
        context
            .authz()
            .authorize(
                audience.into(),
                evp,
                AuthzObject::new(&["system"]).into(),
                "update".into(),
            )
            .await?;

        let load_till = Utc::now()
            - chrono::Duration::from_std(context.config().orphaned_room_timeout)
                .expect("Orphaned room timeout misconfigured");
        let connection = context.get_conn().await?;
        let timed_out = crate::util::spawn_blocking(move || {
            db::orphaned_room::get_timed_out(load_till, &connection)
        })
        .await?;

        let mut close_tasks = vec![];
        let mut closed_rooms = vec![];
        for (orphan, room) in timed_out {
            match room {
                Some(room) if !room.is_closed() => {
                    let connection = context.get_conn().await?;
                    let close_task = crate::util::spawn_blocking(move || {
                        let room = db::room::UpdateQuery::new(room.id())
                            .time(Some((room.time().0, Bound::Excluded(Utc::now()))))
                            .timed_out()
                            .execute(&connection)?;
                        Ok::<_, diesel::result::Error>(room)
                    });

                    close_tasks.push(close_task)
                }

                _ => {
                    closed_rooms.push(orphan.id);
                }
            }
        }
        let mut notifications = vec![];
        for close_task in close_tasks {
            match close_task.await {
                Ok(room) => {
                    closed_rooms.push(room.id());
                    notifications.push(helpers::build_notification(
                        "room.close",
                        &format!("rooms/{}/events", room.id()),
                        room.clone(),
                        evp.tracking(),
                        context.start_timestamp(),
                    ));
                    notifications.push(helpers::build_notification(
                        "room.close",
                        &format!("audiences/{}/events", room.audience()),
                        room,
                        evp.tracking(),
                        context.start_timestamp(),
                    ));
                }
                Err(err) => {
                    error!(?err, "Closing room failed");
                }
            }
        }
        let connection = context.get_conn().await?;
        if let Err(err) = db::orphaned_room::remove_rooms(&closed_rooms, &connection) {
            error!(?err, "Error removing rooms fron orphan table");
        }

        Ok(Box::new(stream::iter(notifications)))
    }
}

////////////////////////////////////////////////////////////////////////////////

pub fn upload_event<C: Context, I>(
    context: &C,
    room: &db::room::Object,
    recordings: I,
) -> StdResult<RoomUploadEvent, AppError>
where
    I: Iterator<Item = (db::recording::Object, db::rtc::Object)>,
{
    let mut event_entries = Vec::new();

    for (recording, rtc) in recordings {
        let uri = match recording.status() {
            RecordingStatus::InProgress => {
                let err = anyhow!(
                    "Unexpected recording in in_progress status, rtc_id = '{}'",
                    recording.rtc_id(),
                );

                return Err(err).error(AppErrorKind::MessageBuildingFailed)?;
            }
            RecordingStatus::Missing => None,
            RecordingStatus::Ready => Some(format!(
                "s3://{}/{}",
                &upload_config(context, room)?.bucket,
                record_name(&recording, room)
            )),
        };

        let entry = RtcUploadEventData {
            id: recording.rtc_id(),
            status: recording.status().to_owned(),
            uri,
            created_by: rtc.created_by().to_owned(),
            mjr_dumps_uris: recording.mjr_dumps_uris().cloned(),
        };

        event_entries.push(entry);
    }

    let uri = format!("audiences/{}/events", room.audience());
    let timing = ShortTermTimingProperties::until_now(context.start_timestamp());
    let props = OutgoingEventProperties::new("room.upload", timing);

    let event = RoomUploadEventData {
        id: room.id(),
        rtcs: event_entries,
    };

    Ok(OutgoingEvent::broadcast(event, props, &uri))
}

fn upload_config<'a, C: Context>(
    context: &'a C,
    room: &Room,
) -> StdResult<&'a UploadConfig, AppError> {
    let configs = &context.config().upload;

    let config = match room.rtc_sharing_policy() {
        SharingPolicy::Shared => &configs.shared,
        SharingPolicy::Owned => &configs.owned,
        SharingPolicy::None => {
            let err = anyhow!("Uploading not available for rooms with 'none' RTC sharing policy");
            return Err(err).error(AppErrorKind::NotImplemented);
        }
    };

    config
        .get(room.audience())
        .ok_or_else(|| anyhow!("Missing upload configuration for the room's audience"))
        .error(AppErrorKind::ConfigKeyMissing)
}

fn record_name(recording: &Recording, room: &Room) -> String {
    let prefix = match room.rtc_sharing_policy() {
        SharingPolicy::Owned => {
            if let Some(classroom_id) = room.classroom_id() {
                format!("{}/", classroom_id)
            } else {
                String::from("")
            }
        }
        _ => String::from(""),
    };

    format!("{}{}.source.webm", prefix, recording.rtc_id())
}

///////////////////////////////////////////////////////////////////////////////

// #[cfg(test)]
// mod test {
//     mod orphaned {
//         use chrono::Utc;

//         use crate::{
//             app::endpoint::system::{OrphanedRoomCloseEvent, OrphanedRoomCloseHandler},
//             db,
//             test_helpers::{
//                 authz::TestAuthz,
//                 context::TestContext,
//                 db::TestDb,
//                 handle_event,
//                 prelude::{GlobalContext, TestAgent},
//                 shared_helpers,
//                 test_deps::LocalDeps,
//                 SVC_AUDIENCE,
//             },
//         };

//         #[tokio::test]
//         async fn close_orphaned_rooms() -> anyhow::Result<()> {
//             let local_deps = LocalDeps::new();
//             let postgres = local_deps.run_postgres();
//             let db = TestDb::with_local_postgres(&postgres);
//             let mut authz = TestAuthz::new();
//             authz.set_audience(SVC_AUDIENCE);
//             let agent = TestAgent::new("alpha", "cron", SVC_AUDIENCE);
//             authz.allow(agent.account_id(), vec!["system"], "update");
//             let mut context = TestContext::new(db, authz);
//             let connection = context.get_conn().await?;
//             let opened_room = shared_helpers::insert_room(&connection);
//             let opened_room2 = shared_helpers::insert_room(&connection);
//             let closed_room = shared_helpers::insert_closed_room(&connection);
//             db::orphaned_room::upsert_room(
//                 opened_room.id(),
//                 Utc::now() - chrono::Duration::seconds(10),
//                 &connection,
//             )?;
//             db::orphaned_room::upsert_room(
//                 closed_room.id(),
//                 Utc::now() - chrono::Duration::seconds(10),
//                 &connection,
//             )?;
//             db::orphaned_room::upsert_room(
//                 opened_room2.id(),
//                 Utc::now() + chrono::Duration::seconds(10),
//                 &connection,
//             )?;

//             let messages = handle_event::<OrphanedRoomCloseHandler>(
//                 &mut context,
//                 &agent,
//                 OrphanedRoomCloseEvent {},
//             )
//             .await
//             .expect("System vacuum failed");

//             let rooms: Vec<db::room::Object> =
//                 messages.into_iter().map(|ev| ev.payload()).collect();
//             assert_eq!(rooms.len(), 2);
//             assert!(rooms[0].timed_out());
//             assert_eq!(rooms[0].id(), opened_room.id());
//             let orphaned = db::orphaned_room::get_timed_out(
//                 Utc::now() + chrono::Duration::seconds(20),
//                 &connection,
//             )?;
//             assert_eq!(orphaned.len(), 1);
//             assert_eq!(orphaned[0].0.id, opened_room2.id());
//             Ok(())
//         }
//     }

//     mod vacuum {
//         use svc_agent::mqtt::ResponseStatus;

//         use crate::{
//             backend::janus::client::{
//                 events::EventResponse,
//                 transactions::{Transaction, TransactionKind},
//                 IncomingEvent,
//             },
//             test_helpers::{prelude::*, test_deps::LocalDeps},
//         };

//         use super::super::*;

//         #[tokio::test]
//         async fn vacuum_system() {
//             let local_deps = LocalDeps::new();
//             let postgres = local_deps.run_postgres();
//             let janus = local_deps.run_janus();
//             let db = TestDb::with_local_postgres(&postgres);
//
//             let mut authz = TestAuthz::new();
//             authz.set_audience(SVC_AUDIENCE);

//             let (rtcs, backend) = db
//                 .connection_pool()
//                 .get()
//                 .map(|conn| {
//                     // Insert janus backend and rooms.
//                     let backend = shared_helpers::insert_janus_backend(
//                         &conn, &janus.url,
//                     );

//                     let room1 =
//                         shared_helpers::insert_closed_room_with_backend_id(&conn, &backend.id());

//                     let room2 =
//                         shared_helpers::insert_closed_room_with_backend_id(&conn, &backend.id());

//                     // Insert rtcs.
//                     let rtcs = vec![
//                         shared_helpers::insert_rtc_with_room(&conn, &room1),
//                         shared_helpers::insert_rtc_with_room(&conn, &room2),
//                     ];

//                     let _other_rtc = shared_helpers::insert_rtc(&conn);

//                     // Insert active agents.
//                     let agent = TestAgent::new("web", "user123", USR_AUDIENCE);

//                     for rtc in rtcs.iter() {
//                         shared_helpers::insert_agent(&conn, agent.agent_id(), rtc.room_id());
//                         shared_helpers::insert_recording(&conn, rtc);
//                     }

//                     (
//                         rtcs.into_iter().map(|x| x.id()).collect::<Vec<_>>(),
//                         backend,
//                     )
//                 })
//                 .unwrap();

//             // Allow cron to perform vacuum.
//             let agent = TestAgent::new("alpha", "cron", SVC_AUDIENCE);
//             authz.allow(agent.account_id(), vec!["system"], "update");

//             // Make system.vacuum request.
//             let mut context = TestContext::new(db, authz);
//             let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
//             context.with_janus(tx.clone());
//             let payload = VacuumRequest {};

//             let messages = handle_request::<VacuumHandler>(&mut context, &agent, payload)
//                 .await
//                 .expect("System vacuum failed");
//             rx.recv().await.unwrap();
//             let recv_rtcs: Vec<db::rtc::Id> = [rx.recv().await.unwrap(), rx.recv().await.unwrap()]
//                 .iter()
//                 .map(|resp| match resp {
//                     IncomingEvent::Event(EventResponse {
//                         transaction:
//                             Transaction {
//                                 kind:
//                                     Some(TransactionKind::UploadStream(UploadStreamTransaction {
//                                         rtc_id,
//                                         start_timestamp: _start_timestamp,
//                                     })),
//                                 ..
//                             },
//                         ..
//                     }) => *rtc_id,
//                     _ => panic!("Got wrong event"),
//                 })
//                 .collect();
//             context.janus_clients().remove_client(&backend);
//             assert!(messages.len() > 0);
//             assert_eq!(recv_rtcs, rtcs);
//         }

//         #[tokio::test]
//         async fn vacuum_system_unauthorized() {
//             let local_deps = LocalDeps::new();
//             let postgres = local_deps.run_postgres();
//             let db = TestDb::with_local_postgres(&postgres);
//             let mut authz = TestAuthz::new();
//             authz.set_audience(SVC_AUDIENCE);

//             // Make system.vacuum request.
//             let agent = TestAgent::new("web", "user123", USR_AUDIENCE);
//             let mut context = TestContext::new(db, authz);
//             let payload = VacuumRequest {};

//             let err = handle_request::<VacuumHandler>(&mut context, &agent, payload)
//                 .await
//                 .expect_err("Unexpected success on system vacuum");

//             assert_eq!(err.status(), ResponseStatus::FORBIDDEN);
//             assert_eq!(err.kind(), "access_denied");
//         }
//     }
// }
