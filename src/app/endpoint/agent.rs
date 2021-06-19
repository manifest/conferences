use async_std::{stream, task};
use async_trait::async_trait;
use serde::Deserialize;
use svc_agent::mqtt::{IncomingRequestProperties, ResponseStatus};
use uuid::Uuid;

use crate::{
    app::{context::Context, endpoint::prelude::*},
    db,
};

///////////////////////////////////////////////////////////////////////////////

const MAX_LIMIT: i64 = 25;

#[derive(Debug, Deserialize)]
pub struct ListRequest {
    room_id: Uuid,
    offset: Option<i64>,
    limit: Option<i64>,
}

pub struct ListHandler;

#[async_trait]
impl RequestHandler for ListHandler {
    type Payload = ListRequest;
    const ERROR_TITLE: &'static str = "Failed to list agents";

    async fn handle<C: Context>(
        context: &mut C,
        payload: Self::Payload,
        reqp: &IncomingRequestProperties,
    ) -> Result {
        let conn = context.get_conn().await?;
        let room = task::spawn_blocking({
            let room_id = payload.room_id;
            move || helpers::find_room_by_id(room_id, helpers::RoomTimeRequirement::Open, &conn)
        })
        .await?;
        helpers::add_room_logger_tags(context, &room);

        // Authorize agents listing in the room.
        let room_id = room.id().to_string();
        let object = vec!["rooms", &room_id];

        let authz_time = context
            .authz()
            .authorize(room.audience(), reqp, object, "read")
            .await?;

        // Get agents list in the room.
        let conn = context.get_conn().await?;
        let agents = task::spawn_blocking(move || {
            db::agent::ListQuery::new()
                .room_id(payload.room_id)
                .offset(payload.offset.unwrap_or(0))
                .limit(std::cmp::min(payload.limit.unwrap_or(MAX_LIMIT), MAX_LIMIT))
                .execute(&conn)
        })
        .await?;

        // Respond with agents list.
        Ok(Box::new(stream::once(helpers::build_response(
            ResponseStatus::OK,
            agents,
            reqp,
            context.start_timestamp(),
            Some(authz_time),
        ))))
    }
}

///////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    mod list {
        use serde::Deserialize;
        use svc_agent::AgentId;
        use uuid::Uuid;

        use crate::test_helpers::prelude::*;

        use super::super::*;

        ///////////////////////////////////////////////////////////////////////////

        #[derive(Deserialize)]
        struct Agent {
            agent_id: AgentId,
            room_id: Uuid,
        }

        #[test]
        fn list_agents() {
            async_std::task::block_on(async {
                let db = TestDb::new();
                let agent = TestAgent::new("web", "user123", USR_AUDIENCE);

                let room = {
                    let conn = db
                        .connection_pool()
                        .get()
                        .expect("Failed to get DB connection");

                    // Create room and put the agent online.
                    let room = shared_helpers::insert_room(&conn);
                    shared_helpers::insert_agent(&conn, agent.agent_id(), room.id());
                    room
                };

                // Allow agent to list agents in the room.
                let mut authz = TestAuthz::new();
                let room_id = room.id().to_string();

                authz.allow(agent.account_id(), vec!["rooms", &room_id], "read");

                // Make agent.list request.
                let mut context = TestContext::new(db, authz);

                let payload = ListRequest {
                    room_id: room.id(),
                    offset: None,
                    limit: None,
                };

                let messages = handle_request::<ListHandler>(&mut context, &agent, payload)
                    .await
                    .expect("Agents listing failed");

                // Assert response.
                let (agents, respp, _) = find_response::<Vec<Agent>>(messages.as_slice());
                assert_eq!(respp.status(), ResponseStatus::OK);
                assert_eq!(agents.len(), 1);
                assert_eq!(&agents[0].agent_id, agent.agent_id());
                assert_eq!(agents[0].room_id, room.id());
            });
        }

        #[test]
        fn list_agents_not_authorized() {
            async_std::task::block_on(async {
                let agent = TestAgent::new("web", "user123", USR_AUDIENCE);
                let db = TestDb::new();

                let room = {
                    let conn = db
                        .connection_pool()
                        .get()
                        .expect("Failed to get DB connection");

                    shared_helpers::insert_room(&conn)
                };

                let mut context = TestContext::new(db, TestAuthz::new());

                let payload = ListRequest {
                    room_id: room.id(),
                    offset: None,
                    limit: None,
                };

                let err = handle_request::<ListHandler>(&mut context, &agent, payload)
                    .await
                    .expect_err("Unexpected success on agents listing");

                assert_eq!(err.status(), ResponseStatus::FORBIDDEN);
                assert_eq!(err.kind(), "access_denied");
            });
        }

        #[test]
        fn list_agents_closed_room() {
            async_std::task::block_on(async {
                let agent = TestAgent::new("web", "user123", USR_AUDIENCE);
                let db = TestDb::new();

                let room = {
                    let conn = db
                        .connection_pool()
                        .get()
                        .expect("Failed to get DB connection");

                    // Create closed room.
                    shared_helpers::insert_closed_room(&conn)
                };

                // Allow agent to list agents in the room.
                let mut authz = TestAuthz::new();
                let room_id = room.id().to_string();

                authz.allow(agent.account_id(), vec!["rooms", &room_id], "read");

                // Make agent.list request.
                let mut context = TestContext::new(db, authz);

                let payload = ListRequest {
                    room_id: room.id(),
                    offset: None,
                    limit: None,
                };

                let err = handle_request::<ListHandler>(&mut context, &agent, payload)
                    .await
                    .expect_err("Unexpected success on agents listing");

                assert_eq!(err.status(), ResponseStatus::NOT_FOUND);
                assert_eq!(err.kind(), "room_closed");
            });
        }

        #[test]
        fn list_agents_missing_room() {
            async_std::task::block_on(async {
                let agent = TestAgent::new("web", "user123", USR_AUDIENCE);
                let mut context = TestContext::new(TestDb::new(), TestAuthz::new());

                let payload = ListRequest {
                    room_id: Uuid::new_v4(),
                    offset: None,
                    limit: None,
                };

                let err = handle_request::<ListHandler>(&mut context, &agent, payload)
                    .await
                    .expect_err("Unexpected success on agents listing");

                assert_eq!(err.status(), ResponseStatus::NOT_FOUND);
                assert_eq!(err.kind(), "room_not_found");
            });
        }
    }
}
