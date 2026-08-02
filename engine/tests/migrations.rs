//! Migrations applied to a database that already has data in it.
//!
//! `Store::open` runs every migration on a fresh file, which every other suite
//! exercises. What that never covers is the case that can actually lose
//! something: a migration arriving at a database with rows already in it.
//!
//! 0006 rebuilds `schedules` — SQLite cannot drop a NOT NULL constraint any
//! other way — and a rebuild that dropped the old table before copying, or
//! mismatched a column list, would silently delete every schedule a user had.
//! The tests here run the migration files by hand, in order, against a
//! populated database, so the SQL is checked rather than assumed.

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Row, SqliteConnection};

/// Apply migration files in filename order, from just after `after` up to and
/// including `through`.
///
/// `after` exists so a test can stop, insert data as an older version would
/// have left it, and then apply exactly the migration under test — rather than
/// replaying everything and hitting "duplicate column".
async fn apply_range(conn: &mut SqliteConnection, after: Option<&str>, through: &str) {
    let mut files: Vec<_> = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))
        .expect("migrations directory")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "sql"))
        .collect();
    files.sort();

    let mut started = after.is_none();
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();

        if !started {
            if after.is_some_and(|a| name.starts_with(a)) {
                started = true;
            }
            continue;
        }

        let sql = std::fs::read_to_string(&path).expect("read migration");
        // Each file is several statements; sqlx executes a multi-statement
        // string when it is passed raw.
        sqlx::raw_sql(&sql)
            .execute(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("{name} failed: {e}"));

        if name.starts_with(through) {
            return;
        }
    }
    panic!("no migration starting with {through:?}");
}

/// One connection, not a pool.
///
/// A pool hands out a different connection per query, and each caches prepared
/// statements describing a table this test deliberately rebuilds underneath
/// them. That is a property of the harness, not of the migration — the
/// application opens its store once and never rebuilds a table at runtime.
async fn conn() -> (SqliteConnection, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let conn = SqliteConnectOptions::new()
        .filename(dir.path().join("m.db"))
        .create_if_missing(true)
        .foreign_keys(true)
        .connect()
        .await
        .expect("open");
    (conn, dir)
}

#[tokio::test]
async fn the_drill_migration_keeps_every_existing_schedule() {
    let (mut conn, _dir) = conn().await;
    apply_range(&mut conn, None, "0005").await;

    // A profile and a plan for the schedule's foreign key to point at.
    sqlx::query(
        "INSERT INTO profiles (id, name, engine, environment, db_config, tool_overrides, \
         created_at, updated_at) VALUES ('p1','prod','mysql','prod','{}','{}','t','t')",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sync_plans (id, profile_id, name, database_name, table_selections, \
         revision, created_at, updated_at) \
         VALUES ('pl1','p1','nightly','app','[]',1,'t','t')",
    )
    .execute(&mut conn)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO schedules (id, sync_plan_id, name, dest_profile_id, cron_expression, \
         timezone, enabled, action_json, webhook_url, notify, catch_up, last_run_at, \
         last_outcome, last_job_id, created_at, updated_at) \
         VALUES ('s1','pl1','nightly sync','p2','0 3 * * *','utc',1,'{\"a\":1}', \
         'https://example.com/hook','always',1,'2026-07-01T03:00:00Z','success','j1','c','u')",
    )
    .execute(&mut conn)
    .await
    .unwrap();

    apply_range(&mut conn, Some("0005"), "0006").await;

    let row = sqlx::query(
        "SELECT kind, sync_plan_id, name, dest_profile_id, cron_expression, timezone, enabled, \
         action_json, webhook_url, notify, catch_up, last_run_at, last_outcome, last_job_id, \
         created_at, updated_at FROM schedules WHERE id = 's1'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("the schedule must survive the rebuild");

    // Every column, not just a count: a mismatched column list in the INSERT
    // ... SELECT would shift values sideways and still leave one row.
    assert_eq!(row.get::<String, _>("kind"), "sync");
    assert_eq!(row.get::<String, _>("sync_plan_id"), "pl1");
    assert_eq!(row.get::<String, _>("name"), "nightly sync");
    assert_eq!(row.get::<String, _>("dest_profile_id"), "p2");
    assert_eq!(row.get::<String, _>("cron_expression"), "0 3 * * *");
    assert_eq!(row.get::<String, _>("timezone"), "utc");
    assert_eq!(row.get::<i64, _>("enabled"), 1);
    assert_eq!(row.get::<String, _>("action_json"), "{\"a\":1}");
    assert_eq!(
        row.get::<String, _>("webhook_url"),
        "https://example.com/hook"
    );
    assert_eq!(row.get::<String, _>("notify"), "always");
    assert_eq!(row.get::<i64, _>("catch_up"), 1);
    assert_eq!(
        row.get::<String, _>("last_run_at"),
        "2026-07-01T03:00:00Z",
        "the high-water mark must survive, or the schedule re-runs an \
         occurrence it already did"
    );
    assert_eq!(row.get::<String, _>("last_outcome"), "success");
    assert_eq!(row.get::<String, _>("last_job_id"), "j1");
    assert_eq!(row.get::<String, _>("created_at"), "c");
    assert_eq!(row.get::<String, _>("updated_at"), "u");
}

#[tokio::test]
async fn a_drill_row_is_accepted_with_no_plan_and_a_sync_row_is_not() {
    let (mut conn, _dir) = conn().await;
    apply_range(&mut conn, None, "0006").await;

    // The whole point of the rebuild: NULL is now legal here.
    sqlx::query(
        "INSERT INTO schedules (id, kind, sync_plan_id, name, dest_profile_id, cron_expression, \
         created_at, updated_at) \
         VALUES ('d1','drill',NULL,'nightly drill','p1','0 4 * * *','c','u')",
    )
    .execute(&mut conn)
    .await
    .expect("a drill has no plan");

    let plan: Option<String> = sqlx::query("SELECT sync_plan_id FROM schedules WHERE id = 'd1'")
        .fetch_one(&mut conn)
        .await
        .unwrap()
        .get("sync_plan_id");
    assert_eq!(plan, None);
}

#[tokio::test]
async fn deleting_a_plan_still_removes_the_sync_schedules_that_ran_it() {
    // The foreign key survives the rebuild. Losing it would leave schedules
    // pointing at a plan that no longer exists, which fails at 3am instead of
    // at the moment the plan was deleted.
    let (mut conn, _dir) = conn().await;
    apply_range(&mut conn, None, "0006").await;

    sqlx::query(
        "INSERT INTO profiles (id, name, engine, environment, db_config, tool_overrides, \
         created_at, updated_at) VALUES ('p1','prod','mysql','prod','{}','{}','t','t')",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sync_plans (id, profile_id, name, database_name, table_selections, \
         revision, created_at, updated_at) \
         VALUES ('pl1','p1','nightly','app','[]',1,'t','t')",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO schedules (id, kind, sync_plan_id, name, cron_expression, created_at, \
         updated_at) VALUES ('s1','sync','pl1','nightly','0 3 * * *','c','u')",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO schedules (id, kind, sync_plan_id, name, dest_profile_id, cron_expression, \
         created_at, updated_at) VALUES ('d1','drill',NULL,'drill','p1','0 4 * * *','c','u')",
    )
    .execute(&mut conn)
    .await
    .unwrap();

    sqlx::query("DELETE FROM sync_plans WHERE id = 'pl1'")
        .execute(&mut conn)
        .await
        .unwrap();

    let remaining: Vec<String> = sqlx::query("SELECT id FROM schedules ORDER BY id")
        .fetch_all(&mut conn)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.get::<String, _>("id"))
        .collect();

    assert_eq!(
        remaining,
        vec!["d1"],
        "the sync schedule cascades away with its plan; the drill, which never \
         had one, is untouched"
    );
}

#[tokio::test]
async fn the_unique_name_migration_renames_collisions_instead_of_deleting_them() {
    let (mut conn, _dir) = conn().await;
    apply_range(&mut conn, None, "0008").await;

    sqlx::query(
        "INSERT INTO profiles (id, name, engine, environment, db_config, tool_overrides, \
         created_at, updated_at) VALUES ('p1','prod','mysql','prod','{}','{}','t','t')",
    )
    .execute(&mut conn)
    .await
    .unwrap();

    // Two sets sharing a name on one connection — legal until now — plus one on
    // another connection that must be left alone.
    for (id, profile, name) in [
        ("pl1", "p1", "nightly"),
        ("pl2", "p1", "nightly"),
        ("pl3", "p1", "weekly"),
    ] {
        sqlx::query(
            "INSERT INTO sync_plans (id, profile_id, name, database_name, table_selections, \
             revision, created_at, updated_at) VALUES (?1,?2,?3,'app','[]',1,'t','t')",
        )
        .bind(id)
        .bind(profile)
        .bind(name)
        .execute(&mut conn)
        .await
        .unwrap();
    }

    // A schedule pointing at the second one, to prove the rename does not
    // rebuild the table the foreign key depends on.
    sqlx::query(
        "INSERT INTO schedules (id, sync_plan_id, name, dest_profile_id, cron_expression, \
         timezone, enabled, action_json, webhook_url, notify, catch_up, last_run_at, \
         last_outcome, last_job_id, created_at, updated_at) \
         VALUES ('s1','pl2','nightly sync',NULL,'0 3 * * *','utc',1,'{}',NULL,'always',1, \
         NULL,NULL,NULL,'c','u')",
    )
    .execute(&mut conn)
    .await
    .unwrap();

    apply_range(&mut conn, Some("0008"), "0009").await;

    // Nothing was deleted. A set is a table selection somebody built by hand;
    // resolving a name collision by dropping one would destroy real work.
    let names: Vec<(String, String)> = sqlx::query("SELECT id, name FROM sync_plans ORDER BY id")
        .fetch_all(&mut conn)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.get("id"), r.get("name")))
        .collect();

    assert_eq!(names.len(), 3, "no set may be dropped: {names:?}");
    assert_eq!(names[0], ("pl1".into(), "nightly".into()), "oldest keeps it");
    assert_eq!(names[1], ("pl2".into(), "nightly (2)".into()));
    assert_eq!(names[2], ("pl3".into(), "weekly".into()), "untouched");

    // The schedule still points where it did.
    let plan_id: String = sqlx::query("SELECT sync_plan_id FROM schedules WHERE id = 's1'")
        .fetch_one(&mut conn)
        .await
        .unwrap()
        .get("sync_plan_id");
    assert_eq!(plan_id, "pl2");

    // And the constraint is now live.
    let err = sqlx::query(
        "INSERT INTO sync_plans (id, profile_id, name, database_name, table_selections, \
         revision, created_at, updated_at) \
         VALUES ('pl4','p1','weekly','app','[]',1,'t','t')",
    )
    .execute(&mut conn)
    .await;
    assert!(err.is_err(), "a duplicate name must now be refused");
}

#[tokio::test]
async fn the_unique_name_migration_leaves_a_clean_database_alone() {
    let (mut conn, _dir) = conn().await;
    apply_range(&mut conn, None, "0008").await;

    sqlx::query(
        "INSERT INTO profiles (id, name, engine, environment, db_config, tool_overrides, \
         created_at, updated_at) VALUES ('p1','prod','mysql','prod','{}','{}','t','t')",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sync_plans (id, profile_id, name, database_name, table_selections, \
         revision, created_at, updated_at) \
         VALUES ('pl1','p1','nightly','app','[]',1,'t','t')",
    )
    .execute(&mut conn)
    .await
    .unwrap();

    apply_range(&mut conn, Some("0008"), "0009").await;

    let name: String = sqlx::query("SELECT name FROM sync_plans WHERE id = 'pl1'")
        .fetch_one(&mut conn)
        .await
        .unwrap()
        .get("name");
    assert_eq!(name, "nightly", "a name with no collision must not gain a suffix");
}
