//! SSH tunnels.
//!
//! The bash predecessor captured tunnel PIDs with `pgrep -f` pattern matching
//! and leaked orphaned tunnels whenever the pattern missed. Here a tunnel is
//! owned by a [`TunnelHandle`] whose `Drop` shuts it down, so a tunnel cannot
//! outlive the job that opened it even on an early return or a panic.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::Disconnect;
use russh::client::{self, AuthResult, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, load_secret_key, ssh_key};
use secrecy::{ExposeSecret, SecretString};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::profile::{SshAuth, SshConfig, SshEndpoint};

/// Identity of a server's host key, as shown to the user for confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyInfo {
    /// e.g. `ssh-ed25519`
    pub algorithm: String,
    /// e.g. `SHA256:abc...`
    pub fingerprint: String,
}

/// Outcome of host-key verification.
///
/// There is no equivalent of `StrictHostKeyChecking=no`: an unrecognised key
/// must be either explicitly accepted or refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// Matches the key pinned previously.
    Known,
    /// First contact; the user accepted this fingerprint.
    AcceptedNew,
    /// Refuse the connection.
    Rejected,
}

/// Decides whether to trust a server's host key.
///
/// Implementations back onto the pinned `known_hosts` table and, when a key is
/// unknown or changed, onto a user prompt.
#[async_trait]
pub trait HostKeyVerifier: Send + Sync {
    async fn verify(
        &self,
        host_port: &str,
        key: &HostKeyInfo,
    ) -> Result<HostKeyDecision, TunnelError>;
}

/// Accepts any host key without pinning. Test-only.
///
/// Never wire this into the application: it is exactly the
/// `StrictHostKeyChecking=no` behaviour this project refuses to ship.
#[derive(Debug, Default, Clone, Copy)]
pub struct AcceptAllHostKeys;

#[async_trait]
impl HostKeyVerifier for AcceptAllHostKeys {
    async fn verify(&self, _: &str, _: &HostKeyInfo) -> Result<HostKeyDecision, TunnelError> {
        Ok(HostKeyDecision::AcceptedNew)
    }
}

/// Verifier backed by the pinned `known_hosts` table.
///
/// First contact is *not* silently trusted. An unknown key fails with
/// [`TunnelError::HostKeyUnknown`] carrying the fingerprint, so the caller can
/// show it to the user and, once they confirm, pin it via
/// [`crate::store::Store::remember_host`] and retry.
///
/// This deliberately avoids holding a half-open SSH connection while waiting
/// for a human: the prompt happens between two independent attempts.
pub struct StoreHostKeyVerifier {
    store: crate::store::Store,
}

impl StoreHostKeyVerifier {
    pub fn new(store: crate::store::Store) -> Self {
        Self { store }
    }
}

#[async_trait]
impl HostKeyVerifier for StoreHostKeyVerifier {
    async fn verify(
        &self,
        host_port: &str,
        key: &HostKeyInfo,
    ) -> Result<HostKeyDecision, TunnelError> {
        let pinned = self
            .store
            .get_known_host(host_port)
            .await
            .map_err(|e| TunnelError::Protocol(format!("could not read known hosts: {e}")))?;

        match pinned {
            None => Err(TunnelError::HostKeyUnknown {
                host_port: host_port.to_string(),
                algorithm: key.algorithm.clone(),
                fingerprint: key.fingerprint.clone(),
            }),
            Some((_, fingerprint)) if fingerprint == key.fingerprint => Ok(HostKeyDecision::Known),
            Some((_, fingerprint)) => Err(TunnelError::HostKeyChanged {
                host_port: host_port.to_string(),
                expected: fingerprint,
                actual: key.fingerprint.clone(),
            }),
        }
    }
}

/// Credentials for one hop, resolved from the keychain before connecting.
#[derive(Clone, Default)]
pub struct HopCredentials {
    /// Passphrase protecting the private key file, if it has one.
    pub key_passphrase: Option<SecretString>,
}

impl std::fmt::Debug for HopCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the passphrase, even in a debug log.
        f.debug_struct("HopCredentials")
            .field("key_passphrase", &self.key_passphrase.is_some())
            .finish()
    }
}

/// Credentials for a whole tunnel, including its optional jump host.
#[derive(Clone, Default, Debug)]
pub struct SshCredentials {
    pub endpoint: HopCredentials,
    pub jump_host: HopCredentials,
}

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("ssh connection to {target} failed: {detail}")]
    Connect { target: String, detail: String },
    #[error("host key for {host_port} was refused")]
    HostKeyRejected { host_port: String },
    #[error(
        "{host_port} is not a known host; its {algorithm} key fingerprint is {fingerprint}. \
         Verify it out of band, then trust it to continue."
    )]
    HostKeyUnknown {
        host_port: String,
        algorithm: String,
        fingerprint: String,
    },
    #[error(
        "host key for {host_port} has CHANGED (pinned {expected}, server offered {actual}); \
         refusing to connect"
    )]
    HostKeyChanged {
        host_port: String,
        expected: String,
        actual: String,
    },
    #[error("authentication failed for {user}@{host}: {detail}")]
    Auth {
        user: String,
        host: String,
        detail: String,
    },
    #[error("could not read private key {path}: {detail}")]
    KeyFile { path: String, detail: String },
    #[error("ssh-agent is unavailable: {0}")]
    Agent(String),
    #[error("could not allocate a local port: {0}")]
    PortAllocation(String),
    #[error("ssh protocol error: {0}")]
    Protocol(String),
}

impl From<russh::Error> for TunnelError {
    fn from(e: russh::Error) -> Self {
        TunnelError::Protocol(e.to_string())
    }
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
        // Last owner released: signal the forwarder to shut down. This is what
        // makes leaked tunnels structurally impossible.
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

/// Result of a connectivity probe against an SSH endpoint.
#[derive(Debug, Clone)]
pub struct SshProbe {
    pub host_key: HostKeyInfo,
    pub authenticated_as: String,
}

#[async_trait]
pub trait TunnelProvider: Send + Sync {
    /// Open a local forward to `remote_host:remote_port` reached via `config`.
    async fn open(
        &self,
        config: &SshConfig,
        credentials: &SshCredentials,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<TunnelHandle, TunnelError>;

    /// Connect and authenticate without forwarding. Used by "test connection".
    async fn probe(
        &self,
        config: &SshConfig,
        credentials: &SshCredentials,
    ) -> Result<SshProbe, TunnelError>;
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

// ── russh implementation ────────────────────────────────────────────────

/// Bridges russh's host-key callback to a [`HostKeyVerifier`].
struct ClientHandler {
    verifier: Arc<dyn HostKeyVerifier>,
    host_port: String,
    observed: Arc<Mutex<Option<HostKeyInfo>>>,
}

impl client::Handler for ClientHandler {
    type Error = TunnelError;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let info = HostKeyInfo {
            algorithm: server_public_key.algorithm().as_str().to_string(),
            fingerprint: server_public_key.fingerprint(HashAlg::Sha256).to_string(),
        };
        *self.observed.lock().await = Some(info.clone());

        match self.verifier.verify(&self.host_port, &info).await? {
            HostKeyDecision::Known | HostKeyDecision::AcceptedNew => Ok(true),
            HostKeyDecision::Rejected => Ok(false),
        }
    }
}

fn client_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        // Without keepalives a tunnel held open across a long dump dies
        // silently behind a NAT or firewall idle timeout.
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    })
}

fn host_port(endpoint: &SshEndpoint) -> String {
    format!("{}:{}", endpoint.host, endpoint.port)
}

/// Close an SSH session politely.
///
/// Dropping the handle would close the TCP socket without sending
/// SSH_MSG_DISCONNECT. OpenSSH 9.8+ enables `PerSourcePenalties` by default and
/// treats an authenticated session that vanishes as a penalty-worthy event; a
/// tool that opens a tunnel per job would steadily accrue penalties until the
/// server starts refusing the user's address outright with "Not allowed at this
/// time". Saying goodbye costs one message and avoids that entirely.
async fn disconnect_politely(session: &Handle<ClientHandler>) {
    if let Err(e) = session
        .disconnect(Disconnect::ByApplication, "tunnel closed", "")
        .await
    {
        tracing::debug!("could not send disconnect: {e}");
    }
}

/// Wrap a connection failure without flattening host-key verdicts.
///
/// `client::connect` returns the *handler's* error type, so a host-key
/// rejection arrives here as a fully-formed `HostKeyUnknown`/`HostKeyChanged`.
/// Collapsing everything into `Connect` would throw away the fingerprint the
/// user needs in order to decide whether to trust the server.
fn connect_error(target: String, e: TunnelError) -> TunnelError {
    match e {
        TunnelError::HostKeyUnknown { .. }
        | TunnelError::HostKeyChanged { .. }
        | TunnelError::HostKeyRejected { .. } => e,
        other => TunnelError::Connect {
            target,
            detail: other.to_string(),
        },
    }
}

/// Load and decrypt a private key file.
fn load_key(
    path: &str,
    passphrase: Option<&SecretString>,
) -> Result<russh::keys::PrivateKey, TunnelError> {
    let expanded = expand_tilde(path);
    load_secret_key(&expanded, passphrase.map(|p| p.expose_secret())).map_err(|e| {
        TunnelError::KeyFile {
            path: expanded.display().to_string(),
            detail: e.to_string(),
        }
    })
}

/// Expand a leading `~` the way a shell would.
///
/// Key paths are typed by hand and almost always start with `~/.ssh/`.
fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match directories::BaseDirs::new() {
            Some(base) => base.home_dir().join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

/// Authenticate an already-connected session.
async fn authenticate(
    session: &mut Handle<ClientHandler>,
    endpoint: &SshEndpoint,
    credentials: &HopCredentials,
) -> Result<(), TunnelError> {
    let result = match &endpoint.auth {
        SshAuth::Agent => {
            let mut agent = russh::keys::agent::client::AgentClient::connect_env()
                .await
                .map_err(|e| TunnelError::Agent(e.to_string()))?;

            let identities = agent
                .request_identities()
                .await
                .map_err(|e| TunnelError::Agent(e.to_string()))?;

            if identities.is_empty() {
                return Err(TunnelError::Agent(
                    "ssh-agent holds no identities; add one with `ssh-add`".into(),
                ));
            }

            // Try each identity; the agent cannot tell us which the server
            // will accept, and offering the wrong one is not an error.
            let mut last = None;
            for identity in identities {
                let key = identity.public_key().into_owned();
                match session
                    .authenticate_publickey_with(endpoint.user.clone(), key, None, &mut agent)
                    .await
                {
                    Ok(AuthResult::Success) => {
                        last = Some(AuthResult::Success);
                        break;
                    }
                    Ok(other) => last = Some(other),
                    Err(e) => {
                        return Err(TunnelError::Auth {
                            user: endpoint.user.clone(),
                            host: endpoint.host.clone(),
                            detail: e.to_string(),
                        });
                    }
                }
            }

            last.unwrap_or(AuthResult::Failure {
                remaining_methods: russh::MethodSet::empty(),
                partial_success: false,
            })
        }
        SshAuth::KeyFile { path, .. } => {
            let key = load_key(path, credentials.key_passphrase.as_ref())?;
            session
                .authenticate_publickey(
                    endpoint.user.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), Some(HashAlg::Sha256)),
                )
                .await
                .map_err(|e| TunnelError::Auth {
                    user: endpoint.user.clone(),
                    host: endpoint.host.clone(),
                    detail: e.to_string(),
                })?
        }
    };

    match result {
        AuthResult::Success => Ok(()),
        AuthResult::Failure {
            remaining_methods, ..
        } => Err(TunnelError::Auth {
            user: endpoint.user.clone(),
            host: endpoint.host.clone(),
            detail: format!("server rejected our credentials (it offers: {remaining_methods:?})"),
        }),
    }
}

/// Real tunnels over russh.
pub struct RusshTunnelProvider {
    verifier: Arc<dyn HostKeyVerifier>,
}

impl RusshTunnelProvider {
    pub fn new(verifier: Arc<dyn HostKeyVerifier>) -> Self {
        Self { verifier }
    }

    /// Connect and authenticate, following the jump host when configured.
    ///
    /// Returns the target session plus the jump session, which must be kept
    /// alive for as long as the target session is used — dropping it tears
    /// down the channel the target session rides on.
    async fn establish(
        &self,
        config: &SshConfig,
        credentials: &SshCredentials,
    ) -> Result<
        (
            Arc<Handle<ClientHandler>>,
            Option<Arc<Handle<ClientHandler>>>,
        ),
        TunnelError,
    > {
        let cfg = client_config();

        let (mut target, jump) = match &config.jump_host {
            None => {
                let handler = self.handler_for(&config.endpoint);
                let session = client::connect(
                    cfg.clone(),
                    (config.endpoint.host.as_str(), config.endpoint.port),
                    handler,
                )
                .await
                .map_err(|e| connect_error(host_port(&config.endpoint), e))?;
                (session, None)
            }
            Some(jump_endpoint) => {
                // Hop one: reach the bastion.
                let jump_handler = self.handler_for(jump_endpoint);
                let mut jump_session = client::connect(
                    cfg.clone(),
                    (jump_endpoint.host.as_str(), jump_endpoint.port),
                    jump_handler,
                )
                .await
                .map_err(|e| connect_error(host_port(jump_endpoint), e))?;

                authenticate(&mut jump_session, jump_endpoint, &credentials.jump_host).await?;

                // Hop two: run a second SSH session over a forwarded channel,
                // which is exactly what ProxyJump does.
                let channel = jump_session
                    .channel_open_direct_tcpip(
                        config.endpoint.host.clone(),
                        u32::from(config.endpoint.port),
                        "127.0.0.1",
                        0,
                    )
                    .await
                    .map_err(|e| TunnelError::Connect {
                        target: host_port(&config.endpoint),
                        detail: format!("could not reach it through the jump host: {e}"),
                    })?;

                let handler = self.handler_for(&config.endpoint);
                let session = client::connect_stream(cfg, channel.into_stream(), handler)
                    .await
                    .map_err(|e| connect_error(host_port(&config.endpoint), e))?;

                (session, Some(Arc::new(jump_session)))
            }
        };

        authenticate(&mut target, &config.endpoint, &credentials.endpoint).await?;
        Ok((Arc::new(target), jump))
    }

    fn handler_for(&self, endpoint: &SshEndpoint) -> ClientHandler {
        ClientHandler {
            verifier: self.verifier.clone(),
            host_port: host_port(endpoint),
            observed: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl TunnelProvider for RusshTunnelProvider {
    async fn open(
        &self,
        config: &SshConfig,
        credentials: &SshCredentials,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<TunnelHandle, TunnelError> {
        let (session, jump) = self.establish(config, credentials).await?;

        // Bind before returning so the caller can connect immediately; a
        // listener created inside the spawned task would race with the first
        // connection attempt.
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| TunnelError::PortAllocation(e.to_string()))?;
        let local_port = listener
            .local_addr()
            .map_err(|e| TunnelError::PortAllocation(e.to_string()))?
            .port();

        let cancel = CancellationToken::new();
        let handle = TunnelHandle::new(local_port, cancel.clone());

        let remote_host = remote_host.to_string();
        tokio::spawn(async move {
            // `jump` is moved in deliberately: the bastion session must outlive
            // every forwarded connection.
            let jump = jump;

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        tracing::debug!("tunnel on port {local_port} closing");
                        break;
                    }
                    accepted = listener.accept() => {
                        let (socket, peer) = match accepted {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!("tunnel accept failed: {e}");
                                break;
                            }
                        };

                        let session = session.clone();
                        let remote_host = remote_host.clone();
                        let cancel = cancel.clone();

                        tokio::spawn(async move {
                            if let Err(e) =
                                forward(session, socket, peer, &remote_host, remote_port, cancel).await
                            {
                                tracing::warn!("forwarded connection failed: {e}");
                            }
                        });
                    }
                }
            }

            // Say goodbye on the way out, innermost hop first.
            disconnect_politely(&session).await;
            if let Some(jump) = jump {
                disconnect_politely(&jump).await;
            }
        });

        Ok(handle)
    }

    async fn probe(
        &self,
        config: &SshConfig,
        credentials: &SshCredentials,
    ) -> Result<SshProbe, TunnelError> {
        // Capture the host key as it is verified, so the caller can display it.
        let observed = Arc::new(Mutex::new(None));
        let capturing = Arc::new(CapturingVerifier {
            inner: self.verifier.clone(),
            seen: observed.clone(),
        });

        let provider = RusshTunnelProvider::new(capturing);
        let (session, _jump) = provider.establish(config, credentials).await?;

        let host_key = observed.lock().await.clone().unwrap_or(HostKeyInfo {
            algorithm: "unknown".into(),
            fingerprint: "unknown".into(),
        });

        // We only needed to prove we could authenticate, but still disconnect
        // cleanly rather than vanishing.
        disconnect_politely(&session).await;
        if let Some(jump) = &_jump {
            disconnect_politely(jump).await;
        }
        drop(session);

        Ok(SshProbe {
            host_key,
            authenticated_as: config.endpoint.user.clone(),
        })
    }
}

/// Wraps another verifier and records whichever key it was asked about.
struct CapturingVerifier {
    inner: Arc<dyn HostKeyVerifier>,
    seen: Arc<Mutex<Option<HostKeyInfo>>>,
}

#[async_trait]
impl HostKeyVerifier for CapturingVerifier {
    async fn verify(
        &self,
        host_port: &str,
        key: &HostKeyInfo,
    ) -> Result<HostKeyDecision, TunnelError> {
        *self.seen.lock().await = Some(key.clone());
        self.inner.verify(host_port, key).await
    }
}

/// Pump bytes between an accepted local socket and a forwarded SSH channel.
async fn forward(
    session: Arc<Handle<ClientHandler>>,
    mut socket: tokio::net::TcpStream,
    peer: SocketAddr,
    remote_host: &str,
    remote_port: u16,
    cancel: CancellationToken,
) -> Result<(), TunnelError> {
    let channel = session
        .channel_open_direct_tcpip(
            remote_host.to_string(),
            u32::from(remote_port),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await?;

    let mut stream = channel.into_stream();

    tokio::select! {
        // Closing the tunnel must drop in-flight connections too, otherwise a
        // cancelled job keeps streaming until the server gives up.
        _ = cancel.cancelled() => Ok(()),
        result = tokio::io::copy_bidirectional(&mut socket, &mut stream) => {
            match result {
                Ok(_) => Ok(()),
                // A client hanging up mid-copy is normal, not an error worth
                // surfacing to the user.
                Err(e) if matches!(
                    e.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::NotConnected
                ) => Ok(()),
                Err(e) => Err(TunnelError::Protocol(e.to_string())),
            }
        }
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

    #[test]
    fn tilde_paths_expand_to_the_home_directory() {
        let expanded = expand_tilde("~/.ssh/id_ed25519");
        assert!(!expanded.starts_with("~"), "tilde must be expanded");
        assert!(expanded.ends_with(".ssh/id_ed25519"));
    }

    #[test]
    fn absolute_paths_are_left_alone() {
        assert_eq!(
            expand_tilde("/etc/ssh/key"),
            PathBuf::from("/etc/ssh/key"),
            "an absolute path must not be rewritten"
        );
    }

    #[test]
    fn credentials_debug_never_reveals_the_passphrase() {
        let creds = HopCredentials {
            key_passphrase: Some(SecretString::from("hunter2")),
        };
        let rendered = format!("{creds:?}");
        assert!(
            !rendered.contains("hunter2"),
            "a passphrase must not reach a debug log, got {rendered}"
        );
        assert!(rendered.contains("true"));
    }

    #[test]
    fn host_port_key_is_stable() {
        let endpoint = SshEndpoint {
            host: "db.example.com".into(),
            port: 2222,
            user: "ops".into(),
            auth: SshAuth::Agent,
        };
        assert_eq!(host_port(&endpoint), "db.example.com:2222");
    }

    #[test]
    fn keepalives_are_configured() {
        // A tunnel held open across a multi-hour dump dies behind NAT without
        // these.
        let cfg = client_config();
        assert!(cfg.keepalive_interval.is_some());
        assert!(cfg.keepalive_max > 0);
    }
}
