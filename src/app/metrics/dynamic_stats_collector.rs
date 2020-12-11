use std::collections::BTreeMap;
use std::thread;

use anyhow::{Context, Result};
use svc_agent::AgentId;

enum Message {
    Register {
        key: String,
        value: usize,
    },
    Flush {
        tx: crossbeam_channel::Sender<Vec<(String, usize)>>,
    },
    JanusTimeout(AgentId),
    Stop,
    GetJanusTimeouts {
        tx: crossbeam_channel::Sender<Vec<(String, u64)>>,
    },
}

pub(crate) struct DynamicStatsCollector {
    tx: crossbeam_channel::Sender<Message>,
}

struct State {
    data: BTreeMap<String, usize>,
    janus_timeouts: BTreeMap<String, u64>,
}

impl DynamicStatsCollector {
    pub(crate) fn start() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();

        thread::spawn(move || {
            let mut state = State {
                data: BTreeMap::new(),
                janus_timeouts: BTreeMap::new(),
            };

            for message in rx {
                match message {
                    Message::Register { key, value } => {
                        let current_value = state.data.get_mut(&key).map(|v| *v);

                        match current_value {
                            Some(current_value) => state.data.insert(key, current_value + value),
                            None => state.data.insert(key, value),
                        };
                    }
                    Message::Flush { tx } => {
                        let report = state.data.into_iter().collect();

                        if let Err(err) = tx.send(report) {
                            warn!(
                                crate::LOG,
                                "Failed to send dynamic stats collector report: {}", err,
                            );
                        }

                        state.data = BTreeMap::new();
                    }
                    Message::Stop => break,
                    Message::JanusTimeout(agent_id) => {
                        let entry = state
                            .janus_timeouts
                            .entry(agent_id.to_string())
                            .or_insert(0);
                        *entry += 1;
                    }
                    Message::GetJanusTimeouts { tx } => {
                        let report = state
                            .janus_timeouts
                            .iter()
                            .map(|(aid, c)| (aid.clone(), *c))
                            .collect();

                        if let Err(err) = tx.send(report) {
                            warn!(
                                crate::LOG,
                                "Failed to send dynamic stats collector report: {}", err,
                            );
                        }
                    }
                }
            }
        });

        Self { tx }
    }

    pub(crate) fn collect(&self, key: impl Into<String>, value: usize) {
        let message = Message::Register {
            key: key.into(),
            value,
        };

        if let Err(err) = self.tx.send(message) {
            warn!(
                crate::LOG,
                "Failed to register dynamic stats collector value: {}", err
            );
        }
    }

    pub(crate) fn flush(&self) -> Result<Vec<(String, usize)>> {
        let (tx, rx) = crossbeam_channel::bounded(1);

        self.tx
            .send(Message::Flush { tx })
            .context("Failed to send flush message to the dynamic stats collector")?;

        rx.recv()
            .context("Failed to receive dynamic stats collector report")
    }

    pub(crate) fn record_janus_timeout(&self, janus: AgentId) {
        if let Err(err) = self.tx.send(Message::JanusTimeout(janus)) {
            warn!(
                crate::LOG,
                "Failed to register dynamic stats collector value: {}", err
            );
        }
    }

    pub(crate) fn get_janus_timeouts(&self) -> Result<Vec<(String, u64)>> {
        let (tx, rx) = crossbeam_channel::bounded(1);

        self.tx
            .send(Message::GetJanusTimeouts { tx })
            .context("Failed to send GetJanusTimeouts message to the dynamic stats collector")?;

        rx.recv()
            .context("Failed to receive dynamic stats collector report")
    }
}

impl Drop for DynamicStatsCollector {
    fn drop(&mut self) {
        if let Err(err) = self.tx.send(Message::Stop) {
            warn!(
                crate::LOG,
                "Failed to stop dynamic stats collector: {}", err
            );
        }
    }
}
