//! End-to-end: a stored profile → keychain → tunnel → introspection.
//!
//! This is the exact path the desktop app and the CLI both take, driven through
//! the engine's public API. The lower-level tunnel and introspection suites
//! prove the pieces; this proves they are wired together.
//!
//!     docker compose -f docker-compose.test.yml up -d --wait

use db_sync_engine::connect;
use db_sync_engine::profile::{DbConfig, ProfileCreate, ToolOverrides};
use db_sync_engine::secrets::{self, SecretKind};
use db_sync_engine::ssh::{RusshTunnelProvider, SshCredentials, TunnelProvider};
use db_sync_engine::sshconn::{SshAuth, SshConfig, SshConnectionCreate, SshEndpoint};
use db_sync_engine::store::Store;
use db_sync_engine::types::{Engine, EnvironmentTag};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;

const SSH_PORT: u16 = 12222;

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    })
}

macro_rules! db_test {
    (async fn $name:ident() $body:block) => {
        #[test]
        fn $name() {
            rt().block_on(async move $body);
        }
    };
    (#[ignore = $reason:literal] async fn $name:ident() $body:block) => {
        #[test]
        #[ignore = $reason]
        fn $name() {
            rt().block_on(async move $body);
        }
    };
}

fn key_path() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/fixtures/ssh/id_ed25519"
    )
    .to_string()
}

async fn containers_up() -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", SSH_PORT)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

macro_rules! require_containers {
    () => {
        if !containers_up().await {
            if std::env::var("DBSYNC_REQUIRE_CONTAINERS").is_ok() {
                panic!("test containers are required but not reachable");
            }
            eprintln!("skipping: test containers not running");
            return;
        }
    };
}

async fn temp_store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("t.db")).await.unwrap();
    (store, dir)
}

fn ssh_endpoint() -> SshEndpoint {
    SshEndpoint {
        host: "127.0.0.1".into(),
        port: SSH_PORT,
        user: "tunnel".into(),
        auth: SshAuth::KeyFile {
            path: key_path(),
            passphrase_in_keychain: false,
        },
    }
}

/// The same endpoint in the resolved shape, for driving the tunnel provider
/// directly without going through a stored record.
fn ssh_config() -> SshConfig {
    SshConfig {
        endpoint: ssh_endpoint(),
        jump_host: None,
    }
}

/// Save the fixture SSH server as a connection, the way the UI does before a
/// profile can reference it.
async fn saved_ssh(store: &Store, name: &str) -> uuid::Uuid {
    store
        .create_ssh_connection(SshConnectionCreate {
            name: name.into(),
            endpoint: ssh_endpoint(),
            jump_host_id: None,
        })
        .await
        .expect("save the ssh connection")
        .id
}

/// A profile pointing at the fixture MySQL, reached through the SSH container.
///
/// `db.host` is `mysql` because it is resolved *from the SSH server*, via the
/// compose network — which is exactly the semantic the UI has to explain.
fn mysql_profile(name: &str, ssh_connection_id: uuid::Uuid) -> ProfileCreate {
    ProfileCreate {
        name: name.into(),
        engine: Engine::Mysql,
        environment: EnvironmentTag::Dev,
        ssh_connection_id: Some(ssh_connection_id),
        db: DbConfig {
            host: "mysql".into(),
            port: 3306,
            user: "root".into(),
            database: Some("fixture".into()),
        },
        tool_overrides: ToolOverrides::default(),
    }
}

/// Learn the fixture server's host key and pin it, the way the UI does after
/// the user confirms the fingerprint.
async fn pin_host_key(store: &Store) {
    let probe = RusshTunnelProvider::new(Arc::new(db_sync_engine::ssh::AcceptAllHostKeys))
        .probe(&ssh_config(), &SshCredentials::default())
        .await
        .expect("probe");

    store
        .remember_host(
            &format!("127.0.0.1:{SSH_PORT}"),
            &probe.host_key.algorithm,
            &probe.host_key.fingerprint,
        )
        .await
        .unwrap();
}

db_test! {
    async fn test_connection_reports_an_unpinned_host_key() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let profile = store
            .create_profile(mysql_profile("unpinned", saved_ssh(&store, "fixture-ssh").await)).await.unwrap();

        let report = connect::test_connection(&profile, &store).await;

        assert!(!report.succeeded(), "an unpinned host must not connect");
        assert!(report.ssh.is_failed(), "SSH is the step that failed");

        // The whole point: the user gets a fingerprint they can verify, not a
        // generic failure.
        let prompt = report
            .host_key_prompt
            .expect("a first-contact prompt must be offered");
        assert!(!prompt.changed);
        assert!(prompt.fingerprint.starts_with("SHA256:"));
        assert_eq!(prompt.host_port, format!("127.0.0.1:{SSH_PORT}"));
    }
}

db_test! {
    async fn test_connection_reports_a_changed_host_key_distinctly() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        let profile = store
            .create_profile(mysql_profile("changed", saved_ssh(&store, "fixture-ssh").await)).await.unwrap();

        // Pin a key that is not the server's.
        store
            .remember_host(&format!("127.0.0.1:{SSH_PORT}"), "ssh-ed25519", "SHA256:bogus")
            .await
            .unwrap();

        let report = connect::test_connection(&profile, &store).await;
        assert!(!report.succeeded());

        let prompt = report.host_key_prompt.expect("prompt");
        assert!(
            prompt.changed,
            "a mismatched pin must be flagged as CHANGED, not first contact"
        );
        assert_eq!(prompt.previous_fingerprint.as_deref(), Some("SHA256:bogus"));
        assert_ne!(prompt.fingerprint, "SHA256:bogus");
    }
}

db_test! {
    async fn ssh_and_tunnel_succeed_once_the_key_is_pinned() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        pin_host_key(&store).await;
        let profile = store
            .create_profile(mysql_profile("pinned", saved_ssh(&store, "fixture-ssh").await)).await.unwrap();

        let report = connect::test_connection(&profile, &store).await;

        assert!(report.ssh.is_ok(), "ssh step: {:?}", report.ssh);
        assert!(report.tunnel.is_ok(), "tunnel step: {:?}", report.tunnel);
        assert!(
            report.host_key_prompt.is_none(),
            "a pinned key needs no prompt"
        );

        // No password is stored for this profile, so the database step must
        // fail — and it must be reported as *that* step, not as an SSH problem.
        assert!(
            report.db_ping.is_failed(),
            "expected the db step to fail without a stored password"
        );
        assert!(!report.succeeded());
    }
}

db_test! {
    async fn a_direct_profile_skips_the_ssh_steps() {
        let (store, _dir) = temp_store().await;
        let profile = store
            .create_profile(ProfileCreate {
                name: "direct".into(),
                engine: Engine::Mysql,
                environment: EnvironmentTag::Dev,
                ssh_connection_id: None,
                db: DbConfig {
                    // Port 1 is reserved and will refuse instantly.
                    host: "127.0.0.1".into(),
                    port: 1,
                    user: "root".into(),
                    database: None,
                },
                tool_overrides: ToolOverrides::default(),
            })
            .await
            .unwrap();

        let report = connect::test_connection(&profile, &store).await;

        assert!(!report.ssh.is_failed(), "a direct profile has no SSH step to fail");
        assert!(!report.tunnel.is_failed());
        assert!(report.db_ping.is_failed(), "nothing is listening on port 1");
    }
}

// ── Full path, including the OS keychain ────────────────────────────────

/// Removes the keychain entry even if an assertion fails.
struct Cleanup(uuid::Uuid);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = secrets::delete_all_for_profile(self.0);
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn full_path_from_stored_profile_to_table_list() {
        require_containers!();
        let (store, _dir) = temp_store().await;
        pin_host_key(&store).await;

        let profile = store
            .create_profile(mysql_profile("full-path", saved_ssh(&store, "fixture-ssh").await)).await.unwrap();
        let _cleanup = Cleanup(profile.id);
        secrets::set_secret(profile.id, SecretKind::DbPassword, "testroot").unwrap();

        // Every step should now pass.
        let report = connect::test_connection(&profile, &store).await;
        assert!(report.ssh.is_ok(), "ssh: {:?}", report.ssh);
        assert!(report.tunnel.is_ok(), "tunnel: {:?}", report.tunnel);
        assert!(report.db_ping.is_ok(), "db: {:?}", report.db_ping);
        assert!(report.catalog_read.is_ok(), "catalog: {:?}", report.catalog_read);
        assert!(
            report.server_version.as_deref().is_some_and(|v| v.starts_with('8')),
            "server version should be reported, got {:?}",
            report.server_version
        );
        assert!(report.succeeded());

        // And the same profile should drive real introspection.
        let connection = connect::open(&profile, &store, Some("fixture"))
            .await
            .expect("open should succeed with a stored password");

        let tables = connection
            .introspector
            .list_tables("fixture")
            .await
            .expect("list tables");
        assert_eq!(tables.len(), 20);
        assert!(tables.iter().any(|t| t.name == "日本語テーブル"));
        assert!(tables.iter().any(|t| !t.transactional), "MyISAM flagged");

        let count = connection
            .introspector
            .exact_row_count("fixture", "users")
            .await
            .expect("count");
        assert_eq!(count, 3);

        connection.close().await;
    }
}

db_test! {
    #[ignore = "requires an unlocked OS keychain"]
    async fn deleting_a_profile_purges_its_password() {
        let (store, _dir) = temp_store().await;
        let profile = store
            .create_profile(mysql_profile("purge-me", saved_ssh(&store, "fixture-ssh").await)).await.unwrap();
        let _cleanup = Cleanup(profile.id);

        secrets::set_secret(profile.id, SecretKind::DbPassword, "testroot").unwrap();
        assert!(secrets::has_secret(profile.id, SecretKind::DbPassword).unwrap());

        store.delete_profile(profile.id).await.unwrap();
        secrets::delete_all_for_profile(profile.id).unwrap();

        assert!(
            !secrets::has_secret(profile.id, SecretKind::DbPassword).unwrap(),
            "a deleted profile must not leave credentials behind"
        );
    }
}
