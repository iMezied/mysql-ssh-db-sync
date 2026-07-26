//! SSH tunnels.
//!
//! The bash predecessor captured tunnel PIDs with `pgrep -f` pattern matching
//! and leaked orphaned tunnels whenever the pattern missed. Here a tunnel is
//! owned by a [`TunnelHandle`] whose `Drop` shuts it down, so a tunnel cannot
//! outlive the job that opened it even on an early return or a panic.
//!
//! The russh implementation lands in M1'; the ownership semantics are real now
//! so the rest of the engine can be written against them.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::profile::SshConfig;

/// Decision returned by the host-key verification callback.
///
/// We never do the equivalent of `StrictHostKeyChecking=no`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// Key matches what we pinned previously.
    Known,
    /// First time seeing this host; the user accepted the fingerprint.
    AcceptedNew,
    /// The key changed. Refuse unless the user explicitly re-pinned it.
    Rejected,
}

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("ssh connection failed: {0}")]
    Connect(String),
    #[error("host key verification failed for {host_port}: {detail}")]
    HostKey { host_port: String, detail: String },
    #[error("authentication failed for {user}@{host}")]
    Auth { user: String, host: String },
    #[error("could not allocate a local port: {0}")]
    PortAllocation(String),
    #[error("tunnel closed before it became ready")]
    Closed,
    #[error("not implemented until M1': {0}")]
    NotImplemented(&'static str),
}

/// An open local-forward tunnel.
///
/// Cloning shares ownership; the tunnel closes when the last clone drops or
/// when [`TunnelHandle::close`] is called.
#[derive(Clone)]
pub struct TunnelHandle {
    id: Uuid,
    local_port: u16,
    inner: Arc<TunnelInner>,
}

struct TunnelInner {
    cancel: CancellationToken,
}

impl Drop for TunnelInner {
    fn drop(&mut self) {
        // Last owner released: signal the forwarder task to shut down. This is
        // what makes leaked tunnels structurally impossible.
        self.cancel.cancel();
    }
}

impl TunnelHandle {
    pub fn new(local_port: u16, cancel: CancellationToken) -> Self {
        Self {
            id: Uuid::new_v4(),
            local_port,
            inner: Arc::new(TunnelInner { cancel }),
        }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Port on 127.0.0.1 that forwards to the remote database.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    pub fn is_closed(&self) -> bool {
        self.inner.cancel.is_cancelled()
    }

    /// Close the tunnel explicitly. Idempotent.
    pub fn close(&self) {
        self.inner.cancel.cancel();
    }

    /// Resolves once the tunnel has been asked to shut down.
    pub async fn closed(&self) {
        self.inner.cancel.cancelled().await;
    }
}

impl std::fmt::Debug for TunnelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelHandle")
            .field("id", &self.id)
            .field("local_port", &self.local_port)
            .field("closed", &self.is_closed())
            .finish()
    }
}

#[async_trait]
pub trait TunnelProvider: Send + Sync {
    /// Open a local forward to `remote_host:remote_port` reached via `config`.
    async fn open(
        &self,
        config: &SshConfig,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<TunnelHandle, TunnelError>;

    /// Connect and authenticate without forwarding, returning the server
    /// version banner. Used by the "test connection" flow.
    async fn probe(&self, config: &SshConfig) -> Result<String, TunnelError>;
}

/// Reserve an ephemeral local port by binding it and immediately releasing it.
///
/// Hardcoded ports (the bash tool used 13306/13307) collide the moment two jobs
/// run at once.
pub fn allocate_local_port() -> Result<u16, TunnelError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| TunnelError::PortAllocation(e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| TunnelError::PortAllocation(e.to_string()))?
        .port();
    drop(listener);
    Ok(port)
}

/// Tracks every tunnel opened by a job so they can all be closed at once.
#[derive(Clone, Default)]
pub struct TunnelSet {
    handles: Arc<Mutex<Vec<TunnelHandle>>>,
}

impl TunnelSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn track(&self, handle: TunnelHandle) -> TunnelHandle {
        self.handles.lock().await.push(handle.clone());
        handle
    }

    pub async fn close_all(&self) {
        let mut handles = self.handles.lock().await;
        for h in handles.iter() {
            h.close();
        }
        handles.clear();
    }

    pub async fn len(&self) -> usize {
        self.handles.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// Placeholder provider until russh lands in M1'.
pub struct RusshTunnelProvider;

#[async_trait]
impl TunnelProvider for RusshTunnelProvider {
    async fn open(
        &self,
        _config: &SshConfig,
        _remote_host: &str,
        _remote_port: u16,
    ) -> Result<TunnelHandle, TunnelError> {
        Err(TunnelError::NotImplemented("ssh tunnel"))
    }

    async fn probe(&self, _config: &SshConfig) -> Result<String, TunnelError> {
        Err(TunnelError::NotImplemented("ssh probe"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocated_ports_are_usable_and_distinct() {
        let a = allocate_local_port().unwrap();
        let b = allocate_local_port().unwrap();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        // Not strictly guaranteed by the OS, but a collision here would mean
        // concurrent jobs fight over ports.
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn dropping_the_last_handle_closes_the_tunnel() {
        let cancel = CancellationToken::new();
        let observer = cancel.clone();

        {
            let _h = TunnelHandle::new(15000, cancel);
            assert!(!observer.is_cancelled());
        }

        assert!(
            observer.is_cancelled(),
            "tunnel must close when its last handle drops, or we leak tunnels"
        );
    }

    #[tokio::test]
    async fn clones_keep_the_tunnel_open() {
        let cancel = CancellationToken::new();
        let observer = cancel.clone();

        let h = TunnelHandle::new(15001, cancel);
        let clone = h.clone();
        drop(h);
        assert!(
            !observer.is_cancelled(),
            "a live clone still owns the tunnel"
        );

        drop(clone);
        assert!(observer.is_cancelled());
    }

    #[tokio::test]
    async fn close_is_idempotent() {
        let h = TunnelHandle::new(15002, CancellationToken::new());
        h.close();
        h.close();
        assert!(h.is_closed());
    }

    #[tokio::test]
    async fn tunnel_set_closes_everything_it_tracks() {
        let set = TunnelSet::new();
        let a = set
            .track(TunnelHandle::new(1, CancellationToken::new()))
            .await;
        let b = set
            .track(TunnelHandle::new(2, CancellationToken::new()))
            .await;

        assert_eq!(set.len().await, 2);
        set.close_all().await;

        assert!(a.is_closed());
        assert!(b.is_closed());
        assert!(set.is_empty().await);
    }
}
