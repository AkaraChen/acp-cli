//! Agent processes: lazy connection, event pumping, restart after exit.

use std::collections::HashMap;
use std::sync::Arc;

use acp_client::AgentCommand;
use acp_client::AgentConnection;
use acp_client::AgentEvent;
use acp_client::ClientOptions;
use acp_client::schema as acp;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::sync::mpsc;

use crate::server::Bridge;
use crate::server::BridgeOptions;
use crate::server::Input;

/// A configured agent the user can select.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentProfile {
    pub slug: String,
    pub name: String,
    pub command: AgentCommand,
}

/// An agent connected (and possibly authenticated) before the bridge started.
pub struct ConnectedAgent {
    pub slug: String,
    pub connection: Arc<AgentConnection>,
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    /// A session created up front (to detect `auth_required` before the
    /// TUI starts); the first `thread/start` in `cwd` adopts it.
    pub prepared_session: Option<(AbsolutePathBuf, acp::NewSessionResponse)>,
}

pub(crate) type Waiter = Box<dyn FnOnce(&mut Bridge, Result<Arc<AgentConnection>, String>) + Send>;

enum SlotState {
    Idle,
    Connecting(Vec<Waiter>),
    Ready(Arc<AgentConnection>),
}

struct Slot {
    generation: u64,
    state: SlotState,
}

pub(crate) struct Agents {
    profiles: Vec<AgentProfile>,
    slots: HashMap<String, Slot>,
    /// The agent new threads use.
    pub(crate) current: String,
    client: ClientOptions,
    prepared: Option<(String, AbsolutePathBuf, acp::NewSessionResponse)>,
}

impl Agents {
    pub(crate) fn new(options: &BridgeOptions) -> Self {
        Self {
            profiles: options.agents.clone(),
            slots: HashMap::new(),
            current: options.default_agent.clone(),
            client: options.client.clone(),
            prepared: None,
        }
    }

    pub(crate) fn profiles(&self) -> &[AgentProfile] {
        &self.profiles
    }

    pub(crate) fn profile(&self, slug: &str) -> Option<&AgentProfile> {
        self.profiles.iter().find(|profile| profile.slug == slug)
    }

    pub(crate) fn connection(&self, slug: &str) -> Option<Arc<AgentConnection>> {
        match self.slots.get(slug).map(|slot| &slot.state) {
            Some(SlotState::Ready(connection)) => Some(Arc::clone(connection)),
            _ => None,
        }
    }

    pub(crate) fn is_current(&self, slug: &str, generation: u64) -> bool {
        self.slots
            .get(slug)
            .is_some_and(|slot| slot.generation == generation)
    }

    pub(crate) fn adopt(&mut self, agent: ConnectedAgent, tx: mpsc::UnboundedSender<Input>) {
        let slot = self.slots.entry(agent.slug.clone()).or_insert(Slot {
            generation: 0,
            state: SlotState::Idle,
        });
        slot.generation += 1;
        slot.state = SlotState::Ready(agent.connection);
        if let Some((cwd, session)) = agent.prepared_session {
            self.prepared = Some((agent.slug.clone(), cwd, session));
        }
        pump_events(agent.slug, slot.generation, agent.events, tx);
    }

    pub(crate) fn set_prepared(
        &mut self,
        slug: &str,
        cwd: AbsolutePathBuf,
        session: acp::NewSessionResponse,
    ) {
        self.prepared = Some((slug.to_string(), cwd, session));
    }

    /// Forget a running agent so the next use starts a fresh process.
    /// Returns the old connection for the caller to shut down.
    pub(crate) fn restart(&mut self, slug: &str) -> Option<Arc<AgentConnection>> {
        let slot = self.slots.get_mut(slug)?;
        slot.generation += 1;
        match std::mem::replace(&mut slot.state, SlotState::Idle) {
            SlotState::Ready(connection) => Some(connection),
            other => {
                slot.state = other;
                None
            }
        }
    }

    /// The session created before startup, if it matches this request.
    pub(crate) fn take_prepared(
        &mut self,
        slug: &str,
        cwd: &AbsolutePathBuf,
    ) -> Option<acp::NewSessionResponse> {
        match &self.prepared {
            Some((prepared_slug, prepared_cwd, _)) if prepared_slug == slug && prepared_cwd == cwd => {
                self.prepared.take().map(|(_, _, session)| session)
            }
            _ => None,
        }
    }

    pub(crate) fn mark_exited(&mut self, slug: &str) {
        if let Some(slot) = self.slots.get_mut(slug) {
            slot.generation += 1;
            slot.state = SlotState::Idle;
        }
    }

    pub(crate) async fn shutdown_all(&mut self) {
        let connections: Vec<_> = self
            .slots
            .values()
            .filter_map(|slot| match &slot.state {
                SlotState::Ready(connection) => Some(Arc::clone(connection)),
                _ => None,
            })
            .collect();
        for connection in connections {
            connection.shutdown().await;
        }
    }
}

fn pump_events(
    slug: String,
    generation: u64,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    tx: mpsc::UnboundedSender<Input>,
) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if tx
                .send(Input::Agent {
                    slug: slug.clone(),
                    generation,
                    event,
                })
                .is_err()
            {
                break;
            }
        }
    });
}

/// Run `waiter` once `slug` is connected, starting the agent if needed.
pub(crate) fn with_agent(bridge: &mut Bridge, slug: &str, waiter: Waiter) {
    let Some(profile) = bridge.agents.profile(slug).cloned() else {
        waiter(bridge, Err(format!("unknown agent `{slug}`")));
        return;
    };
    let slot = bridge
        .agents
        .slots
        .entry(slug.to_string())
        .or_insert(Slot {
            generation: 0,
            state: SlotState::Idle,
        });
    match &mut slot.state {
        SlotState::Ready(connection) => {
            let connection = Arc::clone(connection);
            waiter(bridge, Ok(connection));
        }
        SlotState::Connecting(waiters) => waiters.push(waiter),
        SlotState::Idle => {
            slot.generation += 1;
            let generation = slot.generation;
            slot.state = SlotState::Connecting(vec![waiter]);
            let client = bridge.agents.client.clone();
            let tx = bridge.sender();
            let slug = slug.to_string();
            bridge.spawn(async move {
                let result = AgentConnection::connect(profile.command.clone(), &client).await;
                Box::new(move |bridge: &mut Bridge| {
                    on_connected(bridge, &slug, generation, result, tx);
                })
            });
        }
    }
}

fn on_connected(
    bridge: &mut Bridge,
    slug: &str,
    generation: u64,
    result: Result<
        (Arc<AgentConnection>, mpsc::UnboundedReceiver<AgentEvent>),
        acp_client::ConnectError,
    >,
    tx: mpsc::UnboundedSender<Input>,
) {
    let Some(slot) = bridge.agents.slots.get_mut(slug) else {
        return;
    };
    if slot.generation != generation {
        return;
    }
    let waiters = match std::mem::replace(&mut slot.state, SlotState::Idle) {
        SlotState::Connecting(waiters) => waiters,
        other => {
            slot.state = other;
            Vec::new()
        }
    };
    let outcome = match result {
        Ok((connection, events)) => {
            slot.state = SlotState::Ready(Arc::clone(&connection));
            pump_events(slug.to_string(), generation, events, tx);
            Ok(connection)
        }
        Err(err) => {
            tracing::warn!(agent = slug, "agent failed to start: {err}");
            Err(err.to_string())
        }
    };
    for waiter in waiters {
        waiter(bridge, outcome.clone());
    }
}
