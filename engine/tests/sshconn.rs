//! Saved SSH connections: the rules that make one record safe to share
//! between profiles, and the upgrade that turns old inline configs into them.
//!
//! Deliberately keychain-free. Every endpoint here either uses ssh-agent or a
//! key file with no stored passphrase, so the suite never touches — or leaves
//! anything in — the developer's login keychain.

use db_sync_engine::profile::{DbConfig, ProfileCreate, ToolOverrides};
use db_sync_engine::sshconn::{
    self, SshAuth, SshConnectionCreate, SshConnectionUpdate, SshEndpoint,
};
use db_sync_engine::store::{Store, StoreError};
use db_sync_engine::types::{Engine, EnvironmentTag};
use uuid::Uuid;

async fn store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("test.db"))
        .await
        .expect("open store");
    (store, dir)
}

fn endpoint(host: &str) -> SshEndpoint {
    SshEndpoint {
        host: host.into(),
        port: 22,
        user: "ops".into(),
        auth: SshAuth::Agent,
    }
}

async fn save(store: &Store, name: &str, host: &str) -> Uuid {
    store
        .create_ssh_connection(SshConnectionCreate {
            name: name.into(),
            endpoint: endpoint(host),
            jump_host_id: None,
        })
        .await
        .expect("save")
        .id
}

async fn profile_through(store: &Store, name: &str, ssh_connection_id: Option<Uuid>) -> Uuid {
    store
        .create_profile(ProfileCreate {
            name: name.into(),
            engine: Engine::Mysql,
            environment: EnvironmentTag::Dev,
            ssh_connection_id,
            db: DbConfig {
                host: "127.0.0.1".into(),
                port: 3306,
                user: "app".into(),
                database: None,
            },
            tool_overrides: ToolOverrides::default(),
        })
        .await
        .expect("create profile")
        .id
}

// ── The point of the record ─────────────────────────────────────────────

#[tokio::test]
async fn one_edit_moves_every_profile_behind_it() {
    // The whole reason this is a record instead of a field. Three databases
    // behind one bastion; the bastion moves; nothing about the three changes.
    let (store, _dir) = store().await;
    let ssh_id = save(&store, "bastion", "old.example.com").await;
    for name in ["db-a", "db-b", "db-c"] {
        profile_through(&store, name, Some(ssh_id)).await;
    }

    store
        .update_ssh_connection(
            ssh_id,
            SshConnectionUpdate {
                endpoint: Some(SshEndpoint {
                    host: "new.example.com".into(),
                    port: 2222,
                    ..endpoint("unused")
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    for profile in store.list_profiles().await.unwrap() {
        let resolved = store
            .resolve_ssh(profile.ssh_connection_id.expect("still tunnelled"))
            .await
            .unwrap();
        assert_eq!(
            resolved.config.endpoint.host, "new.example.com",
            "{} did not follow the edit",
            profile.name
        );
        assert_eq!(resolved.config.endpoint.port, 2222);
    }
}

#[tokio::test]
async fn names_are_unique() {
    let (store, _dir) = store().await;
    save(&store, "bastion", "a.example.com").await;

    let err = store
        .create_ssh_connection(SshConnectionCreate {
            name: "bastion".into(),
            endpoint: endpoint("b.example.com"),
            jump_host_id: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::DuplicateName(_)), "{err}");
}

#[tokio::test]
async fn an_unusable_connection_is_refused_rather_than_stored() {
    // Stored, it would look configured and fail only when something depended
    // on it — which for a schedule means at 3am.
    let (store, _dir) = store().await;

    let err = store
        .create_ssh_connection(SshConnectionCreate {
            name: "no-host".into(),
            endpoint: SshEndpoint {
                host: "  ".into(),
                ..endpoint("ignored")
            },
            jump_host_id: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidSshConnection(_)), "{err}");
    assert!(store.list_ssh_connections().await.unwrap().is_empty());
}

// ── Jump hosts ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_jump_host_is_shared_rather_than_copied() {
    let (store, _dir) = store().await;
    let edge = save(&store, "edge", "edge.example.com").await;

    for name in ["db-a-host", "db-b-host"] {
        store
            .create_ssh_connection(SshConnectionCreate {
                name: name.into(),
                endpoint: endpoint(&format!("{name}.internal")),
                jump_host_id: Some(edge),
            })
            .await
            .unwrap();
    }

    // Move the bastion once.
    store
        .update_ssh_connection(
            edge,
            SshConnectionUpdate {
                endpoint: Some(SshEndpoint {
                    host: "edge2.example.com".into(),
                    ..endpoint("unused")
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    for c in store.list_ssh_connections().await.unwrap() {
        if c.id == edge {
            continue;
        }
        let resolved = store.resolve_ssh(c.id).await.unwrap();
        assert_eq!(
            resolved.config.jump_host.expect("jump host").host,
            "edge2.example.com",
            "{} kept a stale copy of the bastion",
            c.name
        );
    }
}

#[tokio::test]
async fn a_connection_cannot_be_its_own_jump_host() {
    let (store, _dir) = store().await;
    let id = save(&store, "self", "a.example.com").await;

    let err = store
        .update_ssh_connection(
            id,
            SshConnectionUpdate {
                jump_host_id: Some(Some(id)),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidSshConnection(_)), "{err}");
}

#[tokio::test]
async fn chained_jumps_are_refused_from_both_directions() {
    // Single-hop is enforced when the row is written, not when the tunnel is
    // opened: a route that cannot work must not be storable.
    let (store, _dir) = store().await;
    let edge = save(&store, "edge", "edge.example.com").await;
    let middle = store
        .create_ssh_connection(SshConnectionCreate {
            name: "middle".into(),
            endpoint: endpoint("middle.example.com"),
            jump_host_id: Some(edge),
        })
        .await
        .unwrap()
        .id;

    // Forwards: jumping through something that itself jumps.
    let err = store
        .create_ssh_connection(SshConnectionCreate {
            name: "inner".into(),
            endpoint: endpoint("inner.example.com"),
            jump_host_id: Some(middle),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidSshConnection(_)), "{err}");

    // Backwards: giving a bastion a bastion of its own.
    let err = store
        .update_ssh_connection(
            edge,
            SshConnectionUpdate {
                jump_host_id: Some(Some(save(&store, "outer", "outer.example.com").await)),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidSshConnection(_)), "{err}");
}

// ── Deletion ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_connection_in_use_is_not_deletable() {
    // Deleting it would leave the profile connecting *directly* to a host and
    // port that were only ever meaningful from the SSH server.
    let (store, _dir) = store().await;
    let ssh_id = save(&store, "bastion", "a.example.com").await;
    profile_through(&store, "prod", Some(ssh_id)).await;

    let err = store.delete_ssh_connection(ssh_id).await.unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("prod"),
        "the error must name what is still using it, got {message}"
    );
    assert_eq!(store.list_ssh_connections().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_connection_used_as_a_jump_host_is_not_deletable() {
    let (store, _dir) = store().await;
    let edge = save(&store, "edge", "edge.example.com").await;
    store
        .create_ssh_connection(SshConnectionCreate {
            name: "inner".into(),
            endpoint: endpoint("inner.example.com"),
            jump_host_id: Some(edge),
        })
        .await
        .unwrap();

    let err = store.delete_ssh_connection(edge).await.unwrap_err();
    assert!(err.to_string().contains("inner"), "{err}");
}

#[tokio::test]
async fn a_connection_nothing_uses_is_deletable() {
    let (store, _dir) = store().await;
    let ssh_id = save(&store, "spare", "a.example.com").await;
    let profile = profile_through(&store, "prod", Some(ssh_id)).await;

    store
        .update_profile(
            profile,
            db_sync_engine::profile::ProfileUpdate {
                ssh_connection_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(store.delete_ssh_connection(ssh_id).await.unwrap());
    assert!(store.list_ssh_connections().await.unwrap().is_empty());
}

// ── Upgrading from inline configurations ────────────────────────────────

/// Write a profile the way a version without saved SSH connections did.
async fn legacy_profile(store: &Store, name: &str, ssh_config: serde_json::Value) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO profiles (id, name, engine, environment, ssh_config, db_config, \
         tool_overrides, created_at, updated_at) \
         VALUES (?1, ?2, 'mysql', 'dev', ?3, ?4, '{}', ?5, ?5)",
    )
    .bind(id.to_string())
    .bind(name)
    .bind(ssh_config.to_string())
    .bind(r#"{"host":"db.internal","port":3306,"user":"app","database":null}"#)
    .bind(&now)
    .execute(store.pool())
    .await
    .expect("insert legacy profile");
    id
}

fn legacy_config(host: &str) -> serde_json::Value {
    serde_json::json!({
        "host": host,
        "port": 22,
        "user": "ops",
        "auth": { "kind": "agent" },
        "jump_host": null
    })
}

#[tokio::test]
async fn an_inline_config_becomes_a_saved_connection() {
    let (store, _dir) = store().await;
    let profile_id = legacy_profile(&store, "prod", legacy_config("bastion.example.com")).await;

    let adopted = sshconn::adopt_legacy_configs(&store).await.unwrap();
    assert_eq!(adopted.len(), 1);

    let profile = store.get_profile(profile_id).await.unwrap().unwrap();
    let ssh_id = profile
        .ssh_connection_id
        .expect("the profile must still be tunnelled");

    let resolved = store.resolve_ssh(ssh_id).await.unwrap();
    assert_eq!(resolved.config.endpoint.host, "bastion.example.com");
    assert_eq!(resolved.config.endpoint.user, "ops");
    assert_eq!(
        resolved.connection.name, "ops@bastion.example.com",
        "the generated name should read like something a person would type"
    );
}

#[tokio::test]
async fn profiles_behind_one_bastion_end_up_sharing_one_record() {
    // The upgrade has to *undo* the duplication, not preserve it. Three
    // identical inline configs becoming three records would leave the user
    // exactly where they started.
    let (store, _dir) = store().await;
    for name in ["db-a", "db-b", "db-c"] {
        legacy_profile(&store, name, legacy_config("bastion.example.com")).await;
    }

    sshconn::adopt_legacy_configs(&store).await.unwrap();

    let connections = store.list_ssh_connections().await.unwrap();
    assert_eq!(
        connections.len(),
        1,
        "one bastion must produce one record, got {:?}",
        connections.iter().map(|c| &c.name).collect::<Vec<_>>()
    );

    let ids: Vec<_> = store
        .list_profiles()
        .await
        .unwrap()
        .iter()
        .map(|p| p.ssh_connection_id)
        .collect();
    assert!(
        ids.iter().all(|id| *id == Some(connections[0].id)),
        "every profile must point at the shared record"
    );
}

#[tokio::test]
async fn different_servers_stay_different_records() {
    let (store, _dir) = store().await;
    legacy_profile(&store, "eu", legacy_config("eu.example.com")).await;
    legacy_profile(&store, "us", legacy_config("us.example.com")).await;

    sshconn::adopt_legacy_configs(&store).await.unwrap();
    assert_eq!(store.list_ssh_connections().await.unwrap().len(), 2);
}

#[tokio::test]
async fn an_inline_jump_host_becomes_its_own_record() {
    let (store, _dir) = store().await;
    let mut config = legacy_config("inner.example.com");
    config["jump_host"] = legacy_config("edge.example.com");
    config["jump_host"]["user"] = serde_json::json!("jump");
    let profile_id = legacy_profile(&store, "prod", config).await;

    sshconn::adopt_legacy_configs(&store).await.unwrap();

    let profile = store.get_profile(profile_id).await.unwrap().unwrap();
    let resolved = store
        .resolve_ssh(profile.ssh_connection_id.unwrap())
        .await
        .unwrap();

    let jump = resolved.jump_host.expect("the jump host must be adopted too");
    assert_eq!(jump.endpoint.host, "edge.example.com");
    assert_eq!(jump.name, "jump@edge.example.com");
    assert_eq!(
        store.list_ssh_connections().await.unwrap().len(),
        2,
        "the server and its bastion are two records"
    );
}

#[tokio::test]
async fn adoption_runs_once_and_is_safe_to_repeat() {
    // It runs at every startup, so a second pass must find nothing left to do
    // rather than creating a second copy of everything.
    let (store, _dir) = store().await;
    legacy_profile(&store, "prod", legacy_config("bastion.example.com")).await;

    assert_eq!(sshconn::adopt_legacy_configs(&store).await.unwrap().len(), 1);
    assert!(
        sshconn::adopt_legacy_configs(&store)
            .await
            .unwrap()
            .is_empty(),
        "a second pass must find nothing"
    );
    assert_eq!(store.list_ssh_connections().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_direct_profile_is_left_alone() {
    let (store, _dir) = store().await;
    profile_through(&store, "direct", None).await;

    assert!(
        sshconn::adopt_legacy_configs(&store)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.list_ssh_connections().await.unwrap().is_empty());
}
