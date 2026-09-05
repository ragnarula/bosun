//! systemd readiness and watchdog notifications.
//!
//! Written by hand rather than through libsystemd: the released binary is a
//! static musl build that cannot link it, and the protocol is one datagram of
//! newline-separated `key=value` pairs sent to `$NOTIFY_SOCKET`. Every
//! function here is a no-op when that variable is unset, so a node started
//! from a plain shell behaves exactly as it did before.
//!
//! Why the watchdog is gated on the poll loop rather than sent from a bare
//! timer: a timer only proves the process exists, which `Restart=` already
//! covers. The failure worth catching is a node that is alive but no longer
//! talking to the control plane, so the keepalive is withheld unless the poll
//! loop has advanced recently.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use tracing::debug;
use tracing::warn;

/// How far behind the poll loop may fall before the keepalives stop. The
/// loop's own request timeout is the longest it can legitimately be quiet, so
/// the bound sits above it: anything longer is a wedge, not a slow control
/// plane.
pub const STALE_AFTER: Duration = Duration::from_secs(crate::poll::POLL_TIMEOUT.as_secs() + 60);

/// The poll loop's liveness signal. The loop marks it on every turn; the
/// watchdog task feeds systemd only while the mark is recent.
pub struct Progress {
    base: Instant,
    millis: AtomicU64,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            millis: AtomicU64::new(0),
        }
    }

    /// Records that the loop has turned.
    pub fn mark(&self) {
        self.millis
            .store(self.base.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// How long since the last mark.
    pub fn since_mark(&self) -> Duration {
        self.base
            .elapsed()
            .saturating_sub(Duration::from_millis(self.millis.load(Ordering::Relaxed)))
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

/// Tells systemd the node is up. Only meaningful under `Type=notify`, where
/// the unit is not considered started until this lands.
pub fn ready() {
    if send("READY=1") {
        debug!("notified systemd that the node is ready");
    }
}

/// Starts feeding the systemd watchdog, if systemd asked for one. Returns
/// without spawning anything when `WatchdogSec=` is not configured, which is
/// every case except a unit that opts in.
pub fn spawn_watchdog(progress: Arc<Progress>) {
    let Some(interval) = watchdog_interval() else {
        return;
    };
    debug!(
        interval_secs = interval.as_secs(),
        "feeding the systemd watchdog"
    );
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let behind = progress.since_mark();
            if behind > STALE_AFTER {
                // Deliberately no keepalive: systemd restarts the node once
                // its own deadline passes. Logged every interval, because the
                // log is the only evidence left if the restart then fails.
                warn!(
                    behind_secs = behind.as_secs(),
                    "the poll loop has not advanced; withholding the systemd watchdog keepalive"
                );
                continue;
            }
            send("WATCHDOG=1");
        }
    });
}

/// Half the deadline systemd set, which is the feeding interval it documents.
/// `None` when no watchdog is configured for this process.
fn watchdog_interval() -> Option<Duration> {
    interval_from(
        std::env::var("WATCHDOG_USEC").ok().as_deref(),
        std::env::var("WATCHDOG_PID").ok().as_deref(),
        std::process::id(),
    )
}

/// The environment-free half, so the rules can be tested without touching the
/// process environment.
fn interval_from(
    usec: Option<&str>,
    watchdog_pid: Option<&str>,
    this_pid: u32,
) -> Option<Duration> {
    let usec: u64 = usec?.trim().parse().ok()?;
    // systemd sets WATCHDOG_PID when a specific process must answer. The node
    // execve's itself on update and keeps its pid, so this still matches after
    // an update; a forked child would not, and must stay silent.
    if let Some(pid) = watchdog_pid {
        let pid: u32 = pid.trim().parse().ok()?;
        if pid != this_pid {
            return None;
        }
    }
    Duration::from_micros(usec).checked_div(2)
}

/// Sends one datagram to the notify socket. False when there is no socket to
/// send to, or the send failed — both are non-fatal, so the caller carries on.
#[cfg(target_os = "linux")]
fn send(message: &str) -> bool {
    use std::os::unix::net::UnixDatagram;

    let Ok(socket_path) = std::env::var("NOTIFY_SOCKET") else {
        return false;
    };
    if socket_path.starts_with('@') {
        // The abstract-namespace form. systemd uses a filesystem path for both
        // system and user services, so this is not reachable in practice, and
        // sending to it needs APIs that are still unstable.
        warn!("NOTIFY_SOCKET is an abstract socket, which is not supported; no notifications sent");
        return false;
    }
    let Ok(socket) = UnixDatagram::unbound() else {
        return false;
    };
    socket.send_to(message.as_bytes(), &socket_path).is_ok()
}

#[cfg(not(target_os = "linux"))]
fn send(_message: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_watchdog_env_means_no_keepalives() {
        assert_eq!(interval_from(None, None, 42), None);
    }

    #[test]
    fn the_interval_is_half_the_deadline() {
        assert_eq!(
            interval_from(Some("30000000"), None, 42),
            Some(Duration::from_secs(15))
        );
    }

    #[test]
    fn a_watchdog_pid_naming_another_process_is_refused() {
        assert_eq!(interval_from(Some("30000000"), Some("41"), 42), None);
    }

    #[test]
    fn a_watchdog_pid_naming_this_process_is_accepted() {
        // The pid still matches after the node execve's itself on update.
        assert_eq!(
            interval_from(Some("30000000"), Some("42"), 42),
            Some(Duration::from_secs(15))
        );
    }

    #[test]
    fn unparsable_values_are_refused_rather_than_defaulted() {
        assert_eq!(interval_from(Some("later"), None, 42), None);
        assert_eq!(interval_from(Some("30000000"), Some("x"), 42), None);
    }

    #[test]
    fn a_fresh_progress_is_not_stale() {
        let progress = Progress::new();
        assert!(progress.since_mark() < STALE_AFTER);
    }

    #[test]
    fn marking_resets_the_measured_gap() {
        let progress = Progress::new();
        std::thread::sleep(Duration::from_millis(20));
        let before = progress.since_mark();
        progress.mark();
        assert!(before >= Duration::from_millis(20));
        assert!(progress.since_mark() < before);
    }

    #[test]
    fn the_stale_bound_sits_above_the_polls_own_timeout() {
        // Otherwise a legitimately long poll would look like a wedge and the
        // node would be restarted mid-request.
        assert!(STALE_AFTER > crate::poll::POLL_TIMEOUT);
    }
}
