use crate::trace_id::TraceId;

use self::{
    agent_leave::AgentLeaveRequest,
    create_handle::{CreateHandleRequest, CreateHandleResponse, OpaqueId},
    create_session::CreateSessionResponse,
    create_stream::{CreateStreamRequest, CreateStreamResponse, CreateStreamTransaction},
    events::{
        DetachedEvent, EventResponse, HangUpEvent, MediaEvent, SlowLinkEvent, TimeoutEvent,
        WebRtcUpEvent,
    },
    read_stream::{ReadStreamRequest, ReadStreamResponse, ReadStreamTransaction},
    service_ping::ServicePingRequest,
    transactions::{Transaction, TransactionKind},
    trickle::TrickleRequest,
    update_agent_reader_config::UpdateReaderConfigRequest,
    update_agent_writer_config::UpdateWriterConfigRequest,
    upload_stream::{UploadStreamRequest, UploadStreamTransaction},
};
use anyhow::Context;
use diesel_derive_newtype::DieselNewType;

use rand::Rng;
use reqwest::{Client, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

use derive_more::{Display, FromStr};

pub mod agent_leave;
pub mod create_handle;
pub mod create_session;
pub mod create_stream;
pub mod events;
pub mod read_stream;
pub mod service_ping;
pub mod transactions;
pub mod trickle;
pub mod update_agent_reader_config;
pub mod update_agent_writer_config;
pub mod upload_stream;

#[derive(Debug, Clone)]
pub struct JanusClient {
    http: Client,
    janus_url: Url,
}

impl JanusClient {
    pub fn new(janus_url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            http: Client::new(),
            janus_url: janus_url.parse()?,
        })
    }

    pub async fn poll(&self, session_id: SessionId) -> anyhow::Result<PollResult> {
        let response = self
            .http
            .get(format!("{}/poll?max_events=5", self.janus_url))
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(PollResult::SessionNotFound);
        }
        let body = response.text().await?;
        let body: Vec<Value> = serde_json::from_str(&body).context(body)?;
        Ok(PollResult::Events(body))
    }

    pub async fn upload_stream(
        &self,
        request: UploadStreamRequest,
        transaction: UploadStreamTransaction,
    ) -> anyhow::Result<()> {
        let _response: AckResponse = self.send_request(request).await?;
        Ok(())
    }

    pub async fn reader_update(&self, request: UpdateReaderConfigRequest) -> anyhow::Result<()> {
        let _response: Value = self.send_request(request).await?;
        Ok(())
    }

    pub async fn writer_update(&self, request: UpdateWriterConfigRequest) -> anyhow::Result<()> {
        self.send_request(request).await?;
        Ok(())
    }

    pub async fn create_stream(
        &self,
        request: CreateStreamRequest,
    ) -> anyhow::Result<CreateStreamResponse> {
        self.send_request(request).await
    }

    pub async fn read_stream(
        &self,
        request: ReadStreamRequest,
    ) -> anyhow::Result<ReadStreamResponse> {
        self.send_request(request).await
    }

    pub async fn trickle_request(&self, request: TrickleRequest) -> anyhow::Result<()> {
        let _response: Value = self.send_request(request).await?;
        Ok(())
    }

    pub async fn create_handle(
        &self,
        request: CreateHandleRequest,
    ) -> anyhow::Result<CreateHandleResponse> {
        // let _timer = METRICS.create_handle_time.start_timer();
        let response: JanusResponse<CreateHandleResponse> =
            self.send_request(create_handle(request)).await?;
        Ok(response.data)
    }

    async fn send_request<R: DeserializeOwned>(&self, body: impl Serialize) -> anyhow::Result<R> {
        let body = serde_json::to_vec(&body)?;
        let response = self
            .http
            .post(self.janus_url.clone())
            .body(body)
            .send()
            .await?
            .text()
            .await?;
        Ok(serde_json::from_str(&response).context(response)?)
    }
}

#[derive(Debug)]
pub enum PollResult {
    SessionNotFound,
    Events(Vec<Value>),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Jsep {
    // '{"type": "offer", "sdp": _}' or '{"type": "answer", "sdp": _}'
    OfferOrAnswer {
        #[serde(rename = "type")]
        kind: JsepType,
        sdp: String,
    },
    IceCandidate(IceCandidateSdp),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JsepType {
    Offer,
    Answer,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct IceCandidate {
    #[serde(rename = "sdpMid")]
    _sdp_mid: String,
    #[serde(rename = "sdpMLineIndex")]
    _sdp_m_line_index: u16,
    #[serde(rename = "candidate")]
    _candidate: String,
    #[serde(rename = "usernameFragment")]
    _username_fragment: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum IceCandidateSdpItem {
    IceCandidate(IceCandidate),
    // {"completed": true}
    Completed {
        #[serde(rename = "completed")]
        _completed: bool,
    },
    // null
    Null(Option<usize>),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum IceCandidateSdp {
    // {"sdpMid": _, "sdpMLineIndex": _, "candidate": _}
    Single(IceCandidateSdpItem),
    // [{"sdpMid": _, "sdpMLineIndex": _, "candidate": _}, …, {"completed": true}]
    List(Vec<IceCandidateSdpItem>),
}

#[derive(
    Debug, Deserialize, Serialize, Display, Copy, Clone, DieselNewType, Hash, PartialEq, Eq, FromStr,
)]
pub struct HandleId(i64);

impl HandleId {
    pub fn stub_id() -> Self {
        Self(123)
    }

    pub fn random() -> Self {
        Self(rand::thread_rng().gen())
    }
}

#[derive(
    Debug, Deserialize, Serialize, Display, Copy, Clone, DieselNewType, Hash, PartialEq, Eq, FromStr,
)]
pub struct SessionId(i64);

impl SessionId {
    pub fn random() -> Self {
        Self(rand::thread_rng().gen())
    }
}

/////////////////////////////////////////////////

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(tag = "janus")]
#[allow(clippy::large_enum_variant)]
pub enum IncomingEvent {
    WebRtcUp(WebRtcUpEvent),
    Media(MediaEvent),
    Timeout(TimeoutEvent),
    HangUp(HangUpEvent),
    SlowLink(SlowLinkEvent),
    Detached(DetachedEvent),
    Event(EventResponse),
}

impl IncomingEvent {
    pub fn event_kind(&self) -> &'static str {
        match self {
            IncomingEvent::WebRtcUp(_) => "WebRtcUp",
            IncomingEvent::Media(_) => "Media",
            IncomingEvent::Timeout(_) => "Media",
            IncomingEvent::HangUp(_) => "HangUp",
            IncomingEvent::SlowLink(_) => "SlowLink",
            IncomingEvent::Detached(_) => "Detached",
            IncomingEvent::Event(e) => match e.transaction.kind.as_ref() {
                Some(TransactionKind::AgentSpeaking) => "AgentSpeaking",
                None => "EmptyTran",
            },
        }
    }

    pub fn trace_id(&self) -> Option<&TraceId> {
        if let IncomingEvent::Event(x) = self {
            x.transaction.trace_id()
        } else {
            None
        }
    }

    pub fn opaque_id(&self) -> Option<&OpaqueId> {
        match self {
            IncomingEvent::WebRtcUp(x) => Some(&x.opaque_id),
            IncomingEvent::Media(x) => Some(&x.opaque_id),
            IncomingEvent::Timeout(_) => None,
            IncomingEvent::HangUp(x) => Some(&x.opaque_id),
            IncomingEvent::SlowLink(x) => Some(&x.opaque_id),
            IncomingEvent::Detached(x) => Some(&x.opaque_id),
            IncomingEvent::Event(_) => None,
        }
    }
}

#[derive(Deserialize, Debug)]
enum Ack {
    #[serde(rename = "ack")]
    Ack,
}

#[derive(Deserialize, Debug)]
struct AckResponse {
    janus: Ack,
}

#[derive(Deserialize, Debug)]
enum Success {
    #[serde(rename = "success")]
    Success,
}

#[derive(Deserialize, Debug)]
struct JanusResponse<T> {
    data: T,
    janus: Success,
}

#[derive(Serialize, Debug)]
struct JanusRequest<T> {
    transaction: Transaction,
    janus: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin: Option<&'static str>,
    #[serde(flatten)]
    data: T,
}

fn create_handle(request: CreateHandleRequest) -> JanusRequest<CreateHandleRequest> {
    JanusRequest {
        transaction: Transaction::only_id(),
        janus: "attach",
        plugin: Some("janus.plugin.conference"),
        data: request,
    }
}

mod serialize_as_base64 {
    use serde::{de, ser};

    use crate::util::{from_base64, to_base64};

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: de::Deserializer<'de>,
        T: serde::de::DeserializeOwned,
    {
        let s: String = de::Deserialize::deserialize(deserializer)?;
        from_base64(&s).map_err(de::Error::custom)
    }

    pub fn serialize<S, T>(obj: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
        T: serde::Serialize,
    {
        let s = to_base64(obj).map_err(ser::Error::custom)?;
        serializer.serialize_str(&s)
    }
}

mod serialize_as_str {
    use serde::{de, ser};

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: de::Deserializer<'de>,
        T: serde::de::DeserializeOwned,
    {
        let s: String = de::Deserialize::deserialize(deserializer)?;
        serde_json::from_str(&s).map_err(de::Error::custom)
    }

    pub fn serialize<S, T>(obj: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
        T: serde::Serialize,
    {
        let s = serde_json::to_string(obj).map_err(ser::Error::custom)?;
        serializer.serialize_str(&s)
    }
}
