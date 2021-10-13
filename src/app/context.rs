use std::sync::Arc;

use chrono::{DateTime, Utc};
use diesel::{
    pg::PgConnection,
    r2d2::{ConnectionManager, PooledConnection},
};
use futures::{future::BoxFuture, FutureExt};

use svc_agent::{request::Dispatcher, AgentId};

use svc_authz::{cache::ConnectionPool as RedisConnectionPool, ClientMap as Authz};

use crate::{
    app::error::{Error as AppError, ErrorExt, ErrorKind as AppErrorKind},
    backend::janus::client_pool::Clients,
    config::Config,
    db::ConnectionPool as Db,
};

use super::metrics::Metrics;

///////////////////////////////////////////////////////////////////////////////

pub trait Context: GlobalContext + MessageContext {}

pub trait GlobalContext: Sync {
    fn authz(&self) -> &Authz;
    fn config(&self) -> &Config;
    fn db(&self) -> &Db;
    fn agent_id(&self) -> &AgentId;
    fn janus_clients(&self) -> Clients;
    fn redis_pool(&self) -> &Option<RedisConnectionPool>;
    fn dispatcher(&self) -> &Arc<Dispatcher>;
    fn metrics(&self) -> Arc<Metrics>;
    fn get_conn(
        &self,
    ) -> BoxFuture<Result<PooledConnection<ConnectionManager<PgConnection>>, AppError>> {
        let db = self.db().clone();
        async move {
            crate::util::spawn_blocking(move || {
                db.get()
                    .map_err(|err| {
                        anyhow::Error::from(err).context("Failed to acquire DB connection")
                    })
                    .error(AppErrorKind::DbConnAcquisitionFailed)
            })
            .await
        }
        .boxed()
    }
}

pub trait MessageContext: Send {
    fn start_timestamp(&self) -> DateTime<Utc>;
}

///////////////////////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct AppContext {
    config: Arc<Config>,
    authz: Authz,
    db: Db,
    agent_id: AgentId,
    redis_pool: Option<RedisConnectionPool>,
    clients: Clients,
    metrics: Arc<Metrics>,
    dispatcher: Arc<Dispatcher>,
}

impl AppContext {
    pub fn new(
        config: Config,
        authz: Authz,
        db: Db,
        clients: Clients,
        metrics: Arc<Metrics>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        let agent_id = AgentId::new(&config.agent_label, config.id.to_owned());

        Self {
            config: Arc::new(config),
            authz,
            db,
            agent_id,
            redis_pool: None,
            clients,
            metrics,
            dispatcher,
        }
    }

    pub fn add_redis_pool(self, pool: RedisConnectionPool) -> Self {
        Self {
            redis_pool: Some(pool),
            ..self
        }
    }

    pub fn start_message(&self) -> AppMessageContext<'_, Self> {
        AppMessageContext::new(self, Utc::now())
    }
}

impl GlobalContext for AppContext {
    fn authz(&self) -> &Authz {
        &self.authz
    }

    fn config(&self) -> &Config {
        &self.config
    }

    fn db(&self) -> &Db {
        &self.db
    }

    fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    fn redis_pool(&self) -> &Option<RedisConnectionPool> {
        &self.redis_pool
    }

    fn janus_clients(&self) -> Clients {
        self.clients.clone()
    }

    fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    fn dispatcher(&self) -> &Arc<Dispatcher> {
        &self.dispatcher
    }
}

///////////////////////////////////////////////////////////////////////////////

pub struct AppMessageContext<'a, C: GlobalContext> {
    global_context: &'a C,
    start_timestamp: DateTime<Utc>,
}

impl<'a, C: GlobalContext> AppMessageContext<'a, C> {
    pub fn new(global_context: &'a C, start_timestamp: DateTime<Utc>) -> Self {
        Self {
            global_context,
            start_timestamp,
        }
    }
}

impl<'a, C: GlobalContext> GlobalContext for AppMessageContext<'a, C> {
    fn authz(&self) -> &Authz {
        self.global_context.authz()
    }

    fn config(&self) -> &Config {
        self.global_context.config()
    }

    fn db(&self) -> &Db {
        self.global_context.db()
    }

    fn agent_id(&self) -> &AgentId {
        self.global_context.agent_id()
    }

    fn redis_pool(&self) -> &Option<RedisConnectionPool> {
        self.global_context.redis_pool()
    }

    fn janus_clients(&self) -> Clients {
        self.global_context.janus_clients()
    }

    fn metrics(&self) -> Arc<Metrics> {
        self.global_context.metrics()
    }

    fn dispatcher(&self) -> &Arc<Dispatcher> {
        self.global_context.dispatcher()
    }
}

impl<'a, C: GlobalContext> MessageContext for AppMessageContext<'a, C> {
    fn start_timestamp(&self) -> DateTime<Utc> {
        self.start_timestamp
    }
}

impl<'a, C: GlobalContext> Context for AppMessageContext<'a, C> {}
