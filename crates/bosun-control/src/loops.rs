//! Per-session agent loops and their live delta broadcasts. The registry is
//! the control-plane handle on every running loop: it starts and stops loops
//! and forwards events from the session API.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

use bosun_agent::agent_loop::ChildSpawner;
use bosun_agent::agent_loop::DeltaSink;
use bosun_agent::agent_loop::LoopDeps;
use bosun_agent::agent_loop::LoopEvent;
use bosun_agent::agent_loop::LoopHandle;
use bosun_agent::agent_loop::LoopMailbox;
use bosun_agent::agent_loop::spawn_loop;
use bosun_agent::provider::Provider;
use bosun_common::config::PersonaConfig;
use bosun_store::store::Store;
use tokio::sync::broadcast;
use tracing::debug;

use crate::commands::CommandQueue;
use crate::registry::NodeRegistry;
use crate::spawn::ChildSessionSpawner;
use crate::tools::TunnelToolExecutor;
use crate::tunnel::TunnelRegistry;

/// Subscribers fall this far behind a live delta stream before broadcast
/// drops the oldest deltas instead of blocking the loop.
const LIVE_CHANNEL_CAPACITY: usize = 512;

/// Non-archived messages allowed in the provider window before the loop
/// compacts the retired tail into a summary.
const MAX_WINDOW_MESSAGES: usize = 80;

/// The running loop handle and live-delta broadcast channel per session.
pub struct AgentRegistry {
    loops: RwLock<HashMap<String, LoopHandle>>,
    live: RwLock<HashMap<String, broadcast::Sender<String>>>,
    /// Skills injected by the control plane, passed to every started loop.
    pub skills_dir: Option<PathBuf>,
    /// Providers for persona models, keyed by model name.
    pub providers: HashMap<String, Arc<dyn Provider>>,
    /// Configured personas, keyed by persona name. The loop resolves the
    /// `spawn` tool's persona from here.
    pub personas: HashMap<String, PersonaConfig>,
    /// Per-million-token prices keyed by model name: (input, output).
    pub prices: HashMap<String, (f64, f64)>,
    /// The machinery that spawns child sessions for the loops, attached once
    /// the registry exists (it holds a weak reference back to it). None
    /// disables the `spawn` tool.
    spawner: RwLock<Option<Arc<dyn ChildSpawner>>>,
}

impl AgentRegistry {
    pub fn new(
        skills_dir: Option<PathBuf>,
        providers: HashMap<String, Arc<dyn Provider>>,
        personas: HashMap<String, PersonaConfig>,
        prices: HashMap<String, (f64, f64)>,
    ) -> Self {
        Self {
            loops: RwLock::new(HashMap::new()),
            live: RwLock::new(HashMap::new()),
            skills_dir,
            providers,
            personas,
            prices,
            spawner: RwLock::new(None),
        }
    }

    /// Attaches the child-session spawner: the registry is the only component
    /// that starts loops, so the spawner must reach it, and the registry owns
    /// the spawner, so the link is a weak reference. Called once at startup
    /// before any loop runs.
    pub fn attach_child_spawner(
        self: &Arc<Self>,
        nodes: Arc<NodeRegistry>,
        commands: Arc<CommandQueue>,
        tunnels: Arc<TunnelRegistry>,
    ) {
        *self.spawner.write().unwrap() = Some(Arc::new(ChildSessionSpawner {
            registry: Arc::downgrade(self),
            nodes,
            commands,
            tunnels,
        }));
    }

    /// Starts one loop task for the session and publishes a live-delta
    /// channel for it. Replacing an existing loop drops the old handle. The
    /// loop receives this registry as its mailbox, so a child session can
    /// wake its parent and a parent can wake a child it messages.
    pub fn start(
        self: &Arc<Self>,
        session_id: &str,
        store: Store,
        provider: Arc<dyn Provider>,
        tunnels: Arc<TunnelRegistry>,
        model_name: &str,
    ) {
        let (sender, _receiver) = broadcast::channel::<String>(LIVE_CHANNEL_CAPACITY);
        self.live
            .write()
            .unwrap()
            .insert(session_id.to_string(), sender.clone());
        let (price_input_per_mtok, price_output_per_mtok) =
            self.prices.get(model_name).copied().unwrap_or((0.0, 0.0));
        let tool_store = store.clone();
        let deps = LoopDeps {
            store,
            provider,
            tools: Arc::new(TunnelToolExecutor {
                tunnels,
                store: tool_store,
            }),
            delta_sink: Arc::new(LiveSink { tx: sender }),
            max_window_messages: MAX_WINDOW_MESSAGES,
            injected_skills_dir: self.skills_dir.clone(),
            personas: self.personas.clone(),
            providers: self.providers.clone(),
            prices: self.prices.clone(),
            price_input_per_mtok,
            price_output_per_mtok,
            spawner: self.spawner.read().unwrap().clone(),
            mailbox: Some(self.clone()),
        };
        let handle = spawn_loop(session_id.to_string(), Arc::new(deps));
        self.loops
            .write()
            .unwrap()
            .insert(session_id.to_string(), handle);
        debug!(
            msg = "agent loop started",
            session_id = %session_id,
            model = %model_name
        );
    }

    pub fn interrupt(&self, session_id: &str) {
        if let Some(handle) = self.loops.read().unwrap().get(session_id) {
            handle.send(LoopEvent::Interrupt);
        }
    }

    pub fn wake(&self, session_id: &str) {
        if let Some(handle) = self.loops.read().unwrap().get(session_id) {
            handle.send(LoopEvent::Wake);
        }
    }

    pub fn subscribe(&self, session_id: &str) -> Option<broadcast::Receiver<String>> {
        self.live
            .read()
            .unwrap()
            .get(session_id)
            .map(broadcast::Sender::subscribe)
    }

    pub fn stop(&self, session_id: &str) {
        if let Some(handle) = self.loops.write().unwrap().remove(session_id) {
            handle.stop();
        }
        self.live.write().unwrap().remove(session_id);
    }
}

impl LoopMailbox for AgentRegistry {
    fn send(&self, session_id: &str, event: LoopEvent) {
        if let Some(handle) = self.loops.read().unwrap().get(session_id) {
            handle.send(event);
        }
    }
}

/// Forwards the loop's streaming text to a live broadcast channel.
pub struct LiveSink {
    pub tx: broadcast::Sender<String>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new(None, HashMap::new(), HashMap::new(), HashMap::new())
    }
}

impl DeltaSink for LiveSink {
    fn send(&self, text: String) {
        let _ = self.tx.send(text);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bosun_agent::provider::ProviderCall;
    use bosun_agent::provider::ProviderError;
    use bosun_agent::provider::StreamEvent;
    use bosun_common::session::Permission;
    use bosun_common::session::Session;
    use bosun_common::session::SessionState;
    use bosun_store::store::Store;
    use bosun_test_support::wait_for;
    use futures_util::StreamExt;
    use futures_util::stream;
    use futures_util::stream::BoxStream;
    use tempfile::tempdir;

    use super::*;
    use crate::tunnel::TunnelRegistry;

    /// Answers every turn with one text delta and a stop, so a wake completes
    /// a full turn into `WaitingForInput`.
    struct TrivialProvider;

    impl Provider for TrivialProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn model(&self) -> &str {
            "mock-model"
        }

        fn chat_stream<'a>(
            &'a self,
            _call: ProviderCall<'a>,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            let items: Vec<Result<StreamEvent, ProviderError>> = vec![
                Ok(StreamEvent::TextDelta("hi".into())),
                Ok(StreamEvent::Stop {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
            ];
            Ok(stream::iter(items).boxed())
        }
    }

    #[tokio::test]
    async fn start_then_interrupt_and_stop() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&Session {
                id: "s1".into(),
                node: "node-1".into(),
                repo_url: None,
                git_ref: None,
                dir: "/work".into(),
                model: "mock-model".into(),
                persona: None,
                parent_id: None,
                owner_id: "s1".into(),
                permission: Permission::ReadWrite,
                allowed_tools: "*".into(),
                state: SessionState::Creating,
                interrupt_cause: None,
                created_at_secs: 1_700_000_000,
                prompt: None,
            })
            .await
            .unwrap();

        let registry = Arc::new(AgentRegistry::new(
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::from([("mock-model".to_string(), (3.0, 15.0))]),
        ));
        registry.start(
            "s1",
            store.clone(),
            Arc::new(TrivialProvider),
            Arc::new(TunnelRegistry::new()),
            "mock-model",
        );

        let receiver = registry.subscribe("s1").expect("a subscribe channel");
        drop(receiver);

        registry.interrupt("s1");
        // Nothing is in flight, so the interrupt is ignored: the session
        // stays creating until a wake starts a turn.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stored = store.get_session("s1").await.unwrap().unwrap();
        assert_eq!(stored.state, SessionState::Creating);

        registry.wake("s1");
        wait_for("the wake to run a turn", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s1").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        registry.stop("s1");

        assert!(registry.subscribe("s1").is_none());
        assert!(
            registry.loops.read().unwrap().is_empty(),
            "stop leaves no loop handle"
        );
        assert!(
            registry.live.read().unwrap().is_empty(),
            "stop leaves no live channel"
        );
    }

    #[tokio::test]
    async fn start_without_a_configured_price_defaults_to_zero() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("sessions.db")).unwrap();
        store
            .create_session(&Session {
                id: "s1".into(),
                node: "node-1".into(),
                repo_url: None,
                git_ref: None,
                dir: "/work".into(),
                model: "mock-model".into(),
                persona: None,
                parent_id: None,
                owner_id: "s1".into(),
                permission: Permission::ReadWrite,
                allowed_tools: "*".into(),
                state: SessionState::Creating,
                interrupt_cause: None,
                created_at_secs: 1_700_000_000,
                prompt: None,
            })
            .await
            .unwrap();

        let registry = Arc::new(AgentRegistry::new(
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        ));
        registry.start(
            "s1",
            store.clone(),
            Arc::new(TrivialProvider),
            Arc::new(TunnelRegistry::new()),
            "mock-model",
        );

        registry.wake("s1");
        wait_for("the wake to run a turn", || {
            let store = store.clone();
            async move {
                let stored = store.get_session("s1").await.unwrap().unwrap();
                stored.state == SessionState::WaitingForInput
            }
        })
        .await;

        let calls = store.model_calls("s1").await.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].cost, Some(0.0), "an absent price costs nothing");

        registry.stop("s1");
    }
}
