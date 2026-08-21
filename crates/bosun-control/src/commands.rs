use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bosun_common::types::CommandResult;
use bosun_common::types::NodeCommand;
use tokio::sync::Notify;
use tokio::sync::oneshot;

/// Per-node queue of commands waiting for a poll, plus the reply channels the
/// API handlers await. The node's poll handler both takes the next command and
/// delivers the previous one's result, so one request serves both directions.
pub struct CommandQueue {
    nodes: RwLock<HashMap<String, NodeCommands>>,
    next_id: AtomicU64,
    hold: Duration,
}

struct NodeCommands {
    queue: VecDeque<NodeCommand>,
    results: HashMap<u64, oneshot::Sender<CommandResult>>,
    notify: Arc<Notify>,
}

impl Default for NodeCommands {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            results: HashMap::new(),
            notify: Arc::new(Notify::new()),
        }
    }
}

impl CommandQueue {
    /// `node_timeout` bounds how long a poll is held: a node that polls every
    /// `node_timeout / 2` seconds is never judged down while healthy.
    pub fn new(node_timeout: Duration) -> Self {
        Self {
            nodes: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            hold: node_timeout / 2,
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Queues a command for the node and records where to send its result.
    pub fn enqueue(&self, node: &str, command: NodeCommand, reply: oneshot::Sender<CommandResult>) {
        let mut nodes = self.nodes.write().unwrap();
        let entry = nodes.entry(node.to_string()).or_default();
        entry.results.insert(command.id(), reply);
        entry.queue.push_back(command);
        entry.notify.notify_waiters();
    }

    /// Waits for the node's next command, up to the hold time. Returns `None`
    /// when the hold elapses with nothing queued.
    pub async fn next(&self, node: &str) -> Option<NodeCommand> {
        let deadline = tokio::time::Instant::now() + self.hold;
        let notify = self
            .nodes
            .write()
            .unwrap()
            .entry(node.to_string())
            .or_default()
            .notify
            .clone();
        loop {
            if let Some(command) = self.pop(node) {
                return Some(command);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::timeout_at(deadline, notify.notified())
                .await
                .ok()?;
        }
    }

    /// Routes a command result to the API handler that is awaiting it.
    pub fn report(&self, node: &str, result: CommandResult) {
        let id = result.id();
        let sender = self
            .nodes
            .write()
            .unwrap()
            .get_mut(node)
            .and_then(|entry| entry.results.remove(&id));
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
    }

    /// Drops the reply channel for a command that timed out on the API side.
    pub fn forget(&self, node: &str, id: u64) {
        if let Some(entry) = self.nodes.write().unwrap().get_mut(node) {
            entry.results.remove(&id);
        }
    }

    fn pop(&self, node: &str) -> Option<NodeCommand> {
        self.nodes
            .write()
            .unwrap()
            .get_mut(node)
            .and_then(|entry| entry.queue.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use bosun_common::types::NodeCommand;
    use tokio::sync::oneshot;

    use super::*;

    fn command(id: u64) -> NodeCommand {
        NodeCommand::Stop {
            id,
            session_id: format!("s{id}"),
        }
    }

    fn result(id: u64) -> CommandResult {
        CommandResult::Stop { id }
    }

    #[tokio::test]
    async fn next_returns_an_enqueued_command_immediately() {
        let queue = CommandQueue::new(Duration::from_secs(30));
        let (tx, _rx) = oneshot::channel();
        queue.enqueue("n1", command(7), tx);

        let next = queue.next("n1").await.expect("a command should be queued");
        assert_eq!(next.id(), 7);
    }

    #[tokio::test]
    async fn next_returns_none_after_the_hold_elapses() {
        let queue = CommandQueue::new(Duration::from_millis(50));
        let next = queue.next("n1").await;
        assert!(next.is_none());
    }

    #[tokio::test]
    async fn next_wakes_when_a_command_is_enqueued_during_the_hold() {
        let queue = Arc::new(CommandQueue::new(Duration::from_secs(30)));
        let queue_clone = queue.clone();
        let (tx, _rx) = oneshot::channel();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            queue_clone.enqueue("n1", command(1), tx);
        });

        let next = queue
            .next("n1")
            .await
            .expect("the command should wake the poll");
        assert_eq!(next.id(), 1);
    }

    #[tokio::test]
    async fn report_delivers_the_result_to_the_awaiting_handler() {
        let queue = CommandQueue::new(Duration::from_secs(30));
        let (tx, rx) = oneshot::channel();
        queue.enqueue("n1", command(3), tx);

        queue.report("n1", result(3));

        let delivered = rx.await.expect("the handler should receive the result");
        assert_eq!(delivered.id(), 3);
    }

    #[tokio::test]
    async fn report_for_an_unknown_or_timeout_command_is_dropped() {
        let queue = CommandQueue::new(Duration::from_secs(30));
        queue.report("n1", result(1));

        let (tx, rx) = oneshot::channel();
        queue.enqueue("n1", command(2), tx);
        queue.forget("n1", 2);
        queue.report("n1", result(2));
        drop(rx);

        queue.report("n1", result(1));
    }

    #[tokio::test]
    async fn commands_follow_fifo_order() {
        let queue = CommandQueue::new(Duration::from_secs(30));
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        queue.enqueue("n1", command(1), tx1);
        queue.enqueue("n1", command(2), tx2);

        assert_eq!(queue.next("n1").await.unwrap().id(), 1);
        assert_eq!(queue.next("n1").await.unwrap().id(), 2);
    }
}
