use std::str::FromStr;

use aynur_deploy::db::Database;
use aynur_deploy::model::DeploymentStatus;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

#[tokio::test]
async fn existing_v03_database_is_upgraded_without_losing_deployments() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("deployments.sqlite3");
    let options = SqliteConnectOptions::from_str(database_path.to_str().unwrap())
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE project_state (
            project_id TEXT PRIMARY KEY NOT NULL,
            blocked INTEGER NOT NULL CHECK (blocked IN (0, 1)),
            blocked_reason TEXT,
            updated_at TEXT NOT NULL
        ) STRICT
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE deployments (
            id TEXT PRIMARY KEY NOT NULL,
            project_id TEXT NOT NULL,
            event_key TEXT NOT NULL UNIQUE,
            kind TEXT NOT NULL CHECK (kind IN ('deploy', 'rollback')),
            tag TEXT NOT NULL,
            requested_sha TEXT NOT NULL,
            commit_sha TEXT,
            release_path TEXT,
            previous_release_path TEXT,
            status TEXT NOT NULL CHECK (status IN (
                'queued', 'fetching', 'building', 'activating', 'healthChecking',
                'rollingBack', 'succeeded', 'failed', 'rollbackFailed'
            )),
            error_code TEXT,
            error_message TEXT,
            retry_of TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            completed_at TEXT,
            FOREIGN KEY (project_id) REFERENCES project_state(project_id),
            FOREIGN KEY (retry_of) REFERENCES deployments(id)
        ) STRICT
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO project_state(project_id, blocked, updated_at) VALUES ('service', 0, '2026-09-03T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        INSERT INTO deployments(
            id, project_id, event_key, kind, tag, requested_sha, commit_sha,
            release_path, status, created_at, updated_at, completed_at
        ) VALUES (
            'deployment-1', 'service', 'event-1', 'deploy', 'deploy-20260903-120000',
            '1111111111111111111111111111111111111111',
            '1111111111111111111111111111111111111111', '/release/one', 'succeeded',
            '2026-09-03T00:00:00Z', '2026-09-03T00:00:01Z', '2026-09-03T00:00:01Z'
        )
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let database = Database::connect(&database_path).await.unwrap();
    let deployment = database.get_deployment("deployment-1").await.unwrap();
    assert_eq!(deployment.status, DeploymentStatus::Succeeded);
    assert_eq!(deployment.project_id, "service");
    let project = database.project_state("service").await.unwrap();
    assert!(!project.stopped);
    database
        .set_status("deployment-1", DeploymentStatus::Migrating)
        .await
        .unwrap();

    let options = SqliteConnectOptions::from_str(database_path.to_str().unwrap())
        .unwrap()
        .foreign_keys(true);
    let verification_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&verification_pool)
        .await
        .unwrap();
    assert_eq!(version, 2);
    let table_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'deployments'",
    )
    .fetch_one(&verification_pool)
    .await
    .unwrap();
    assert!(table_sql.contains("'migrating'"));
    let project_table_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'project_state'",
    )
    .fetch_one(&verification_pool)
    .await
    .unwrap();
    assert!(project_table_sql.contains("stopped"));
}

#[tokio::test]
async fn existing_v1_database_is_upgraded_without_clearing_blocked_state() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("deployments.sqlite3");
    let options = SqliteConnectOptions::from_str(database_path.to_str().unwrap())
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE project_state (
            project_id TEXT PRIMARY KEY NOT NULL,
            blocked INTEGER NOT NULL CHECK (blocked IN (0, 1)),
            blocked_reason TEXT,
            updated_at TEXT NOT NULL
        ) STRICT
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE deployments (
            id TEXT PRIMARY KEY NOT NULL,
            project_id TEXT NOT NULL,
            event_key TEXT NOT NULL UNIQUE,
            kind TEXT NOT NULL CHECK (kind IN ('deploy', 'rollback')),
            tag TEXT NOT NULL,
            requested_sha TEXT NOT NULL,
            commit_sha TEXT,
            release_path TEXT,
            previous_release_path TEXT,
            status TEXT NOT NULL CHECK (status IN (
                'queued', 'fetching', 'building', 'migrating', 'activating',
                'healthChecking', 'rollingBack', 'succeeded', 'failed', 'rollbackFailed'
            )),
            error_code TEXT,
            error_message TEXT,
            retry_of TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            completed_at TEXT,
            FOREIGN KEY (project_id) REFERENCES project_state(project_id),
            FOREIGN KEY (retry_of) REFERENCES deployments(id)
        ) STRICT
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO project_state(project_id, blocked, blocked_reason, updated_at) VALUES ('service', 1, 'rollback failed', '2026-09-03T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("PRAGMA user_version = 1")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let database = Database::connect(&database_path).await.unwrap();
    let project = database.project_state("service").await.unwrap();

    assert!(project.blocked);
    assert_eq!(project.blocked_reason.as_deref(), Some("rollback failed"));
    assert!(!project.stopped);
    let verification_options = SqliteConnectOptions::from_str(database_path.to_str().unwrap())
        .unwrap()
        .foreign_keys(true);
    let verification_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(verification_options)
        .await
        .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&verification_pool)
        .await
        .unwrap();
    assert_eq!(version, 2);
}
