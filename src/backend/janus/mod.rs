use std::ops::Bound;

use anyhow::Result;
use async_std::stream;
use chrono::{DateTime, NaiveDateTime, Utc};
use diesel::pg::PgConnection;
use svc_agent::mqtt::{
    IncomingEvent as MQTTIncomingEvent, IncomingEventProperties, IncomingRequestProperties,
    IncomingResponse as MQTTIncomingResponse, IntoPublishableMessage, OutgoingResponse,
    ResponseStatus, ShortTermTimingProperties,
};
use svc_agent::Addressable;
use svc_error::Error as SvcError;
use uuid::Uuid;

use crate::app::context::Context;
use crate::app::endpoint;
use crate::app::error::{Error as AppError, ErrorExt, ErrorKind as AppErrorKind};
use crate::app::message_handler::MessageStream;
use crate::app::API_VERSION;
use crate::db::{agent_connection, janus_backend, janus_rtc_stream, recording, room, rtc};
use crate::diesel::Connection;
use crate::util::from_base64;

use self::events::{HandleEvent, IncomingEvent, StatusEvent, WebRtcUpEvent};
use self::responses::{ErrorResponse, IncomingResponse};
use self::transactions::Transaction;

////////////////////////////////////////////////////////////////////////////////

const STREAM_UPLOAD_METHOD: &str = "stream.upload";
pub(crate) const JANUS_API_VERSION: &str = "v1";

const ALREADY_RUNNING_STATE: &str = "already_running";

////////////////////////////////////////////////////////////////////////////////

pub(crate) async fn handle_response<C: Context>(
    context: &mut C,
    resp: &MQTTIncomingResponse<String>,
) -> MessageStream {
    handle_response_impl(context, resp)
        .await
        .unwrap_or_else(|app_error| {
            error!(
                context.logger(),
                "Failed to handle a response from janus: {}", app_error,
            );

            app_error.notify_sentry(context.logger());
            Box::new(stream::empty())
        })
}

async fn handle_response_impl<C: Context>(
    context: &mut C,
    resp: &MQTTIncomingResponse<String>,
) -> Result<MessageStream, AppError> {
    let respp = resp.properties();
    context.janus_client().finish_transaction(respp);

    let payload = MQTTIncomingResponse::convert_payload::<IncomingResponse>(&resp)
        .map_err(|err| anyhow!("Failed to parse response: {}", err))
        .error(AppErrorKind::MessageParsingFailed)?;

    match payload {
        IncomingResponse::Success(ref inresp) => {
            let txn = from_base64::<Transaction>(&inresp.transaction())
                .map_err(|err| err.context("Failed to parse transaction"))
                .error(AppErrorKind::MessageParsingFailed)?;

            match txn {
                // Session has been created.
                Transaction::CreateSession(tn) => {
                    // Initialize handle pool.
                    let backend_id = respp.as_agent_id().to_owned();
                    let session_id = inresp.data().id();
                    let pool_size = tn.capacity() as usize;
                    let janus_handle_pool = context.janus_handle_pool().clone();
                    let logger = context.logger().clone();

                    async_std::task::spawn(async move {
                        let result =
                            janus_handle_pool.create_handles(&backend_id, session_id, pool_size);

                        if let Err(err) = result {
                            error!(logger, "{}", err);

                            AppError::new(AppErrorKind::BackendInitializationFailed, err)
                                .notify_sentry(&logger);
                        }
                    });

                    // Create control handle.
                    let backreq = context
                        .janus_client()
                        .create_control_handle_request(
                            respp,
                            inresp.data().id(),
                            tn.capacity(),
                            tn.balancer_capacity(),
                            context.start_timestamp(),
                        )
                        .error(AppErrorKind::MessageBuildingFailed)?;

                    let boxed_backreq = Box::new(backreq) as Box<dyn IntoPublishableMessage + Send>;
                    Ok(Box::new(stream::once(boxed_backreq)))
                }
                // Control handle has been created.
                Transaction::CreateControlHandle(tn) => {
                    let backend_id = respp.as_agent_id();
                    let handle_id = inresp.data().id();
                    let conn = context.get_conn()?;

                    janus_backend::UpsertQuery::new(backend_id, handle_id, tn.session_id())
                        .capacity(tn.capacity())
                        .balancer_capacity(tn.balancer_capacity())
                        .execute(&conn)?;

                    Ok(Box::new(stream::empty()))
                }
                // Pool handle has been created.
                Transaction::CreatePoolHandle(_tn) => {
                    let result = context.janus_handle_pool().handle_created_callback(
                        respp.as_agent_id(),
                        inresp.data().id(),
                        Some(respp),
                    );

                    if let Err(err) = result {
                        error!(context.logger(), "{}", err);

                        AppError::new(AppErrorKind::BackendInitializationFailed, err)
                            .notify_sentry(context.logger());
                    }

                    Ok(Box::new(stream::empty()))
                }
                // An unsupported incoming Success message has been received.
                _ => Ok(Box::new(stream::empty())),
            }
        }
        IncomingResponse::Ack(ref inresp) => {
            let txn = from_base64::<Transaction>(&inresp.transaction())
                .map_err(|err| err.context("Failed to parse transaction"))
                .error(AppErrorKind::MessageParsingFailed)?;

            match txn {
                // Conference Stream is being created
                Transaction::CreateStream(_tn) => Ok(Box::new(stream::empty())),
                // Trickle message has been received by Janus Gateway
                Transaction::Trickle(tn) => {
                    let resp = endpoint::rtc_signal::CreateResponse::unicast(
                        endpoint::rtc_signal::CreateResponseData::new(None),
                        tn.reqp().to_response(
                            ResponseStatus::OK,
                            ShortTermTimingProperties::until_now(context.start_timestamp()),
                        ),
                        tn.reqp().as_agent_id(),
                        JANUS_API_VERSION,
                    );

                    let boxed_resp = Box::new(resp) as Box<dyn IntoPublishableMessage + Send>;
                    Ok(Box::new(stream::once(boxed_resp)))
                }
                // An unsupported incoming Ack message has been received
                _ => Ok(Box::new(stream::empty())),
            }
        }
        IncomingResponse::Event(ref inresp) => {
            context.add_logger_tags(o!("transaction" => inresp.transaction().to_string()));

            let txn = from_base64::<Transaction>(&inresp.transaction())
                .map_err(|err| err.context("Failed to parse transaction"))
                .error(AppErrorKind::MessageParsingFailed)?;

            match txn {
                // Conference Stream has been created (an answer received)
                Transaction::CreateStream(ref tn) => {
                    context.add_logger_tags(o!("method" => tn.reqp().method().to_string()));

                    inresp
                        .plugin()
                        .data()
                        .ok_or_else(|| anyhow!("Missing 'data' in the response"))
                        .error(AppErrorKind::MessageParsingFailed)?
                        .get("status")
                        .ok_or_else(|| anyhow!("Missing 'status' in the response"))
                        .error(AppErrorKind::MessageParsingFailed)
                        .and_then(|status| {
                            context.add_logger_tags(o!("status" => status.as_u64()));

                            if status == "200" {
                                Ok(())
                            } else {
                                Err(anyhow!("Received error status"))
                                    .error(AppErrorKind::BackendRequestFailed)
                            }
                        })
                        .and_then(|_| {
                            // Getting answer (as JSEP)
                            let jsep = inresp
                                .jsep()
                                .ok_or_else(|| anyhow!("Missing 'jsep' in the response"))
                                .error(AppErrorKind::MessageParsingFailed)?;

                            let timing =
                                ShortTermTimingProperties::until_now(context.start_timestamp());

                            let resp = endpoint::rtc_signal::CreateResponse::unicast(
                                endpoint::rtc_signal::CreateResponseData::new(Some(jsep.clone())),
                                tn.reqp().to_response(ResponseStatus::OK, timing),
                                tn.reqp().as_agent_id(),
                                JANUS_API_VERSION,
                            );

                            let boxed_resp =
                                Box::new(resp) as Box<dyn IntoPublishableMessage + Send>;
                            Ok(Box::new(stream::once(boxed_resp)) as MessageStream)
                        })
                        .or_else(|err| Ok(handle_response_error(context, &tn.reqp(), err)))
                }
                // Conference Stream has been read (an answer received)
                Transaction::ReadStream(ref tn) => {
                    context.add_logger_tags(o!("method" => tn.reqp().method().to_string()));

                    inresp
                        .plugin()
                        .data()
                        .ok_or_else(|| anyhow!("Missing 'data' in the response"))
                        .error(AppErrorKind::MessageParsingFailed)?
                        .get("status")
                        .ok_or_else(|| anyhow!("Missing 'status' in the response"))
                        .error(AppErrorKind::MessageParsingFailed)
                        // We fail if the status isn't equal to 200
                        .and_then(|status| {
                            context.add_logger_tags(o!("status" => status.as_u64()));

                            if status == "200" {
                                Ok(())
                            } else {
                                Err(anyhow!("Received error status"))
                                    .error(AppErrorKind::BackendRequestFailed)
                            }
                        })
                        .and_then(|_| {
                            // Getting answer (as JSEP)
                            let jsep = inresp
                                .jsep()
                                .ok_or_else(|| anyhow!("Missing 'jsep' in the response"))
                                .error(AppErrorKind::MessageParsingFailed)?;

                            let timing =
                                ShortTermTimingProperties::until_now(context.start_timestamp());

                            let resp = endpoint::rtc_signal::CreateResponse::unicast(
                                endpoint::rtc_signal::CreateResponseData::new(Some(jsep.clone())),
                                tn.reqp().to_response(ResponseStatus::OK, timing),
                                tn.reqp().as_agent_id(),
                                JANUS_API_VERSION,
                            );

                            let boxed_resp =
                                Box::new(resp) as Box<dyn IntoPublishableMessage + Send>;
                            Ok(Box::new(stream::once(boxed_resp)) as MessageStream)
                        })
                        .or_else(|err| Ok(handle_response_error(context, &tn.reqp(), err)))
                }
                // Conference Stream has been uploaded to a storage backend (a confirmation)
                Transaction::UploadStream(ref tn) => {
                    context.add_logger_tags(o!(
                        "method" => tn.method().to_string(),
                        "rtc_id" => tn.rtc_id().to_string(),
                    ));

                    // TODO: improve error handling
                    let plugin_data = inresp
                        .plugin()
                        .data()
                        .ok_or_else(|| anyhow!("Missing 'data' in the response"))
                        .error(AppErrorKind::MessageParsingFailed)?;

                    plugin_data
                        .get("status")
                        .ok_or_else(|| anyhow!("Missing 'status' in the response"))
                        .error(AppErrorKind::MessageParsingFailed)
                        // We fail if the status isn't equal to 200
                        .and_then(|status| {
                            context.add_logger_tags(o!("status" => status.as_u64()));

                            match status {
                                val if val == "200" => Ok(()),
                                val if val == "404" => {
                                    let conn = context.get_conn()?;

                                    recording::UpdateQuery::new(tn.rtc_id())
                                        .status(recording::Status::Missing)
                                        .execute(&conn)?;

                                    Err(anyhow!("Janus is missing recording"))
                                        .error(AppErrorKind::BackendRecordingMissing)
                                }
                                _ => Err(anyhow!("Received error status"))
                                    .error(AppErrorKind::BackendRequestFailed),
                            }
                        })
                        .and_then(|_| {
                            let rtc_id = plugin_data
                                .get("id")
                                .ok_or_else(|| anyhow!("Missing 'id' in response"))
                                .error(AppErrorKind::MessageParsingFailed)
                                .and_then(|val| {
                                    serde_json::from_value::<Uuid>(val.clone())
                                        .map_err(|err| anyhow!("Invalid value for 'id': {}", err))
                                        .error(AppErrorKind::MessageParsingFailed)
                                })?;

                            // if vacuuming was already started by previous request - just do nothing
                            let maybe_already_running =
                                plugin_data.get("state").and_then(|v| v.as_str())
                                    == Some(ALREADY_RUNNING_STATE);
                            if maybe_already_running {
                                return Ok(Box::new(stream::empty()) as MessageStream);
                            }

                            let started_at = plugin_data
                                .get("started_at")
                                .ok_or_else(|| anyhow!("Missing 'started_at' in response"))
                                .error(AppErrorKind::MessageParsingFailed)
                                .and_then(|val| {
                                    let unix_ts = serde_json::from_value::<u64>(val.clone())
                                        .map_err(|err| {
                                            anyhow!("Invalid value for 'started_at': {}", err)
                                        })
                                        .error(AppErrorKind::MessageParsingFailed)?;

                                    let naive_datetime = NaiveDateTime::from_timestamp(
                                        unix_ts as i64 / 1000,
                                        ((unix_ts % 1000) * 1_000_000) as u32,
                                    );

                                    Ok(DateTime::<Utc>::from_utc(naive_datetime, Utc))
                                })?;

                            let segments = plugin_data
                                .get("time")
                                .ok_or_else(|| anyhow!("Missing time"))
                                .error(AppErrorKind::MessageParsingFailed)
                                .and_then(|segments| {
                                    Ok(serde_json::from_value::<Vec<(i64, i64)>>(segments.clone())
                                        .map_err(|err| anyhow!("Invalid value for 'time': {}", err))
                                        .error(AppErrorKind::MessageParsingFailed)?
                                        .into_iter()
                                        .map(|(start, end)| {
                                            (Bound::Included(start), Bound::Excluded(end))
                                        })
                                        .collect())
                                })?;

                            let (room, rtcs_with_recs): (
                                room::Object,
                                Vec<(rtc::Object, Option<recording::Object>)>,
                            ) = {
                                let conn = context.get_conn()?;

                                recording::UpdateQuery::new(rtc_id)
                                    .status(recording::Status::Ready)
                                    .started_at(started_at)
                                    .segments(segments)
                                    .execute(&conn)?;

                                let rtc = rtc::FindQuery::new()
                                    .id(rtc_id)
                                    .execute(&conn)?
                                    .ok_or_else(|| anyhow!("RTC not found"))
                                    .error(AppErrorKind::RtcNotFound)?;

                                let room = endpoint::helpers::find_room_by_rtc_id(
                                    context,
                                    rtc.id(),
                                    endpoint::helpers::RoomTimeRequirement::Any,
                                    &conn,
                                )?;

                                let rtcs_with_recs =
                                    rtc::ListWithRecordingQuery::new(room.id()).execute(&conn)?;

                                (room, rtcs_with_recs)
                            };

                            // Ensure that all rtcs have a recording.
                            let rtcs_total = rtcs_with_recs.len();

                            let recs_with_rtcs = rtcs_with_recs
                                .into_iter()
                                .filter_map(|(rtc, maybe_recording)| {
                                    maybe_recording.map(|recording| (recording, rtc))
                                })
                                .collect::<Vec<_>>();

                            if recs_with_rtcs.len() < rtcs_total {
                                return Ok(Box::new(stream::empty()) as MessageStream);
                            }

                            // Send room.upload event.
                            let event = endpoint::system::upload_event(
                                context,
                                &room,
                                recs_with_rtcs.into_iter(),
                                respp.tracking(),
                            )?;

                            let event_box =
                                Box::new(event) as Box<dyn IntoPublishableMessage + Send>;

                            Ok(Box::new(stream::once(event_box)) as MessageStream)
                        })
                }
                // An unsupported incoming Event message has been received
                _ => Ok(Box::new(stream::empty())),
            }
        }
        IncomingResponse::Error(ErrorResponse::Session(ref inresp)) => {
            let err = anyhow!(
                "received an unexpected Error message (session): {}",
                inresp.error()
            );
            Err(err).error(AppErrorKind::MessageParsingFailed)
        }
        IncomingResponse::Error(ErrorResponse::Handle(ref inresp)) => {
            let err = anyhow!(
                "received an unexpected Error message (handle): {}",
                inresp.error()
            );
            Err(err).error(AppErrorKind::MessageParsingFailed)
        }
    }
}

fn handle_response_error<C: Context>(
    context: &mut C,
    reqp: &IncomingRequestProperties,
    app_error: AppError,
) -> MessageStream {
    context.add_logger_tags(o!(
        "status" => app_error.status().as_u16(),
        "kind" => app_error.kind().to_owned(),
    ));

    error!(
        context.logger(),
        "Failed to handle a response from janus: {}",
        app_error.source(),
    );

    app_error.notify_sentry(context.logger());

    let svc_error: SvcError = app_error.to_svc_error();
    let timing = ShortTermTimingProperties::until_now(context.start_timestamp());
    let respp = reqp.to_response(svc_error.status_code(), timing);
    let resp = OutgoingResponse::unicast(svc_error, respp, reqp, API_VERSION);
    let boxed_resp = Box::new(resp) as Box<dyn IntoPublishableMessage + Send>;
    Box::new(stream::once(boxed_resp))
}

pub(crate) async fn handle_event<C: Context>(
    context: &mut C,
    event: &MQTTIncomingEvent<String>,
) -> MessageStream {
    handle_event_impl(context, event)
        .await
        .unwrap_or_else(|app_error| {
            error!(
                context.logger(),
                "Failed to handle an event from janus: {}", app_error
            );

            app_error.notify_sentry(context.logger());
            Box::new(stream::empty())
        })
}

async fn handle_event_impl<C: Context>(
    context: &mut C,
    event: &MQTTIncomingEvent<String>,
) -> Result<MessageStream, AppError> {
    context.add_logger_tags(o!("label" => event.properties().label().unwrap_or("").to_string()));

    let payload = MQTTIncomingEvent::convert_payload::<IncomingEvent>(&event)
        .map_err(|err| anyhow!("Failed to parse event: {}", err))
        .error(AppErrorKind::MessageParsingFailed)?;

    let evp = event.properties();

    match payload {
        IncomingEvent::WebRtcUp(ref inev) => handle_webrtc_up(context, inev, evp),
        IncomingEvent::HangUp(ref inev) => handle_hangup_detach(context, inev, evp),
        IncomingEvent::Detached(ref inev) => handle_hangup_detach(context, inev, evp),
        IncomingEvent::Media(_) | IncomingEvent::Timeout(_) | IncomingEvent::SlowLink(_) => {
            // Ignore these kinds of events.
            Ok(Box::new(stream::empty()))
        }
    }
}

fn handle_webrtc_up<C: Context>(
    context: &mut C,
    inev: &WebRtcUpEvent,
    evp: &IncomingEventProperties,
) -> Result<MessageStream, AppError> {
    let conn = context.get_conn()?;

    let maybe_rtc_stream = conn.transaction::<_, AppError, _>(|| {
        if let Some(rtc_stream) = find_rtc_stream(&conn, inev, evp)? {
            context.add_logger_tags(o!("rtc_stream_id" => rtc_stream.id().to_string()));
            janus_rtc_stream::start(rtc_stream.id(), &conn).map_err(Into::into)
        } else {
            Ok(None)
        }
    })?;

    // If the event relates to a publisher's handle,
    // we will find the corresponding stream and send event w/ updated stream object
    // to the room's topic.
    if let Some(rtc_stream) = maybe_rtc_stream {
        let room = endpoint::helpers::find_room_by_rtc_id(
            context,
            rtc_stream.rtc_id(),
            endpoint::helpers::RoomTimeRequirement::Open,
            &conn,
        )?;

        let event = endpoint::rtc_stream::update_event(
            room.id(),
            rtc_stream,
            context.start_timestamp(),
            evp.tracking(),
        )?;

        Ok(Box::new(stream::once(
            Box::new(event) as Box<dyn IntoPublishableMessage + Send>
        )))
    } else {
        Ok(Box::new(stream::empty()))
    }
}

fn handle_hangup_detach<C: Context, E: HandleEvent>(
    context: &mut C,
    inev: &E,
    evp: &IncomingEventProperties,
) -> Result<MessageStream, AppError> {
    let conn = context.get_conn()?;

    let maybe_rtc_stream = conn.transaction::<_, AppError, _>(|| {
        if let Some(rtc_stream) = find_rtc_stream(&conn, inev, evp)? {
            context.add_logger_tags(o!("rtc_stream_id" => rtc_stream.id().to_string()));
            janus_rtc_stream::stop(rtc_stream.id(), &conn).map_err(Into::into)
        } else {
            Ok(None)
        }
    })?;

    // If the event relates to the publisher's handle,
    // we will find the corresponding stream and send an event w/ updated stream object
    // to the room's topic.
    if let Some(rtc_stream) = maybe_rtc_stream {
        let room = endpoint::helpers::find_room_by_rtc_id(
            context,
            rtc_stream.rtc_id(),
            endpoint::helpers::RoomTimeRequirement::Open,
            &conn,
        )?;

        // Publish the update event only if the stream object has been changed.
        // If there's no actual media stream, the object wouldn't contain its start time.
        if rtc_stream.time().is_some() {
            // Disconnect agents.
            agent_connection::BulkDisconnectByRoomQuery::new(room.id()).execute(&conn)?;

            // Send rtc_stream.update event.
            let event = endpoint::rtc_stream::update_event(
                room.id(),
                rtc_stream,
                context.start_timestamp(),
                evp.tracking(),
            )?;

            let boxed_event = Box::new(event) as Box<dyn IntoPublishableMessage + Send>;
            return Ok(Box::new(stream::once(boxed_event)));
        }
    }

    Ok(Box::new(stream::empty()))
}

fn find_rtc_stream<E: HandleEvent>(
    conn: &PgConnection,
    inev: &E,
    evp: &IncomingEventProperties,
) -> Result<Option<janus_rtc_stream::Object>, AppError> {
    let mut rtc_streams = janus_rtc_stream::ListQuery::new()
        .backend_id(evp.as_agent_id())
        .handle_id(inev.handle_id())
        .active(false)
        .limit(1)
        .execute(&conn)?;

    Ok(rtc_streams.pop())
}

pub(crate) async fn handle_status_event<C: Context>(
    context: &mut C,
    event: &MQTTIncomingEvent<String>,
) -> MessageStream {
    handle_status_event_impl(context, event)
        .await
        .unwrap_or_else(|app_error| {
            error!(
                context.logger(),
                "Failed to handle a status event from janus: {}", app_error
            );

            app_error.notify_sentry(context.logger());
            Box::new(stream::empty())
        })
}

async fn handle_status_event_impl<C: Context>(
    context: &mut C,
    event: &MQTTIncomingEvent<String>,
) -> Result<MessageStream, AppError> {
    let evp = event.properties();
    context.add_logger_tags(o!("label" => evp.label().unwrap_or("").to_string()));

    let payload = MQTTIncomingEvent::convert_payload::<StatusEvent>(&event)
        .map_err(|err| anyhow!("Failed to parse event: {}", err))
        .error(AppErrorKind::MessageParsingFailed)?;

    if payload.online() {
        let event = context
            .janus_client()
            .create_session_request(&payload, evp, context.start_timestamp())
            .error(AppErrorKind::MessageBuildingFailed)?;

        let boxed_event = Box::new(event) as Box<dyn IntoPublishableMessage + Send>;
        Ok(Box::new(stream::once(boxed_event)))
    } else {
        let conn = context.get_conn()?;

        let streams_with_rtc = conn.transaction::<_, AppError, _>(|| {
            let streams_with_rtc = janus_rtc_stream::ListWithRtcQuery::new()
                .active(true)
                .backend_id(evp.as_agent_id())
                .execute(&conn)?;

            agent_connection::BulkDisconnectByBackendQuery::new(evp.as_agent_id())
                .execute(&conn)?;

            janus_backend::DeleteQuery::new(evp.as_agent_id()).execute(&conn)?;
            Ok(streams_with_rtc)
        })?;

        let now = Utc::now();
        let mut events = Vec::with_capacity(streams_with_rtc.len());

        for (mut stream, rtc) in streams_with_rtc {
            stream.set_time(stream.time().map(|t| (t.0, Bound::Excluded(now))));

            let event = endpoint::rtc_stream::update_event(
                rtc.room_id(),
                stream,
                context.start_timestamp(),
                evp.tracking(),
            )?;

            events.push(Box::new(event) as Box<dyn IntoPublishableMessage + Send>);
        }

        Ok(Box::new(stream::from_iter(events)))
    }
}

////////////////////////////////////////////////////////////////////////////////

mod client;
mod events;
mod handle_pool;
pub(crate) mod requests;
mod responses;
mod transactions;

pub(crate) use client::Client;
pub(crate) use handle_pool::HandlePool;
