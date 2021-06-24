use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_derive::{Deserialize, Serialize};
use svc_agent::{
    mqtt::{
        IncomingResponseProperties, OutgoingMessage, OutgoingRequest, OutgoingRequestProperties,
        ShortTermTimingProperties,
    },
    Addressable,
};

use crate::util::{generate_correlation_data, to_base64};

use super::super::requests::CreateHandleRequest;
use super::super::{Client, JANUS_API_VERSION};
use super::Transaction;

const METHOD: &str = "janus_handle.create";

////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TransactionData {
    session_id: i64,
    capacity: Option<i32>,
    balancer_capacity: Option<i32>,
    group: Option<String>,
    janus_url: Option<String>,
}

impl TransactionData {
    pub(crate) fn new(session_id: i64) -> Self {
        Self {
            session_id,
            capacity: None,
            balancer_capacity: None,
            group: None,
            janus_url: None,
        }
    }

    pub(crate) fn session_id(&self) -> i64 {
        self.session_id
    }

    pub(crate) fn capacity(&self) -> Option<i32> {
        self.capacity
    }

    pub(crate) fn set_capacity(&mut self, capacity: i32) -> &mut Self {
        self.capacity = Some(capacity);
        self
    }

    pub(crate) fn balancer_capacity(&self) -> Option<i32> {
        self.balancer_capacity
    }

    pub(crate) fn set_balancer_capacity(&mut self, balancer_capacity: i32) -> &mut Self {
        self.balancer_capacity = Some(balancer_capacity);
        self
    }

    pub(crate) fn group(&self) -> Option<&str> {
        self.group.as_ref().map(AsRef::as_ref)
    }

    pub(crate) fn set_group(&mut self, group: &str) -> &mut Self {
        self.group = Some(group.to_owned());
        self
    }

    /// Get a reference to the transaction data's janus url.
    pub(crate) fn janus_url(&self) -> Option<&str> {
        self.janus_url.as_deref()
    }

    /// Set the transaction data's janus url.
    pub(crate) fn set_janus_url(&mut self, janus_url: String) {
        self.janus_url = Some(janus_url);
    }
}

////////////////////////////////////////////////////////////////////////////////

impl Client {
    pub(crate) fn create_handle_request(
        &self,
        respp: &IncomingResponseProperties,
        session_id: i64,
        capacity: Option<i32>,
        balancer_capacity: Option<i32>,
        group: Option<&str>,
        janus_url: Option<&str>,
        start_timestamp: DateTime<Utc>,
    ) -> Result<OutgoingMessage<CreateHandleRequest>> {
        let to = respp.as_agent_id();
        let mut tn_data = TransactionData::new(session_id);

        if let Some(capacity) = capacity {
            tn_data.set_capacity(capacity);
        }

        if let Some(balancer_capacity) = balancer_capacity {
            tn_data.set_balancer_capacity(balancer_capacity);
        }

        if let Some(group) = group {
            tn_data.set_group(group);
        }

        if let Some(janus_url) = janus_url {
            tn_data.set_janus_url(janus_url.to_owned())
        }

        let transaction = Transaction::CreateHandle(tn_data);

        let payload = CreateHandleRequest::new(
            &to_base64(&transaction)?,
            session_id,
            "janus.plugin.conference",
            None,
        );

        let mut props = OutgoingRequestProperties::new(
            METHOD,
            &self.response_topic(to)?,
            &generate_correlation_data(),
            ShortTermTimingProperties::until_now(start_timestamp),
        );

        props.set_tracking(respp.tracking().to_owned());
        self.register_transaction(to, start_timestamp, &props, &payload, self.timeout(METHOD));

        Ok(OutgoingRequest::unicast(
            payload,
            props,
            to,
            JANUS_API_VERSION,
        ))
    }
}
