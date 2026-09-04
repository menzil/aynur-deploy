use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{AssertSqlSafe, Sqlite, SqlitePool, Transaction};
use uuid::Uuid;

use crate::error::{AppError, file_error};
use crate::model::{
    CleanDeploymentType, Deployment, DeploymentRow, DeploymentStatus, ProjectState, ProjectStateRow,
};

#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

#[derive(Debug)]
pub struct CreatedDeployment {
    pub deployment: Deployment,
    pub created: bool,
}

impl Database {
    pub async fn connect(path: &Path) -> Result<Self, AppError> {
        let parent = path.parent().ok_or_else(|| AppError::Config {
            path: path.to_path_buf(),
            reason: "databasePath must have a parent directory".to_owned(),
        })?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| file_error("create directory", parent.to_path_buf(), source))?;
        let options =
            SqliteConnectOptions::from_str(path.to_str().ok_or_else(|| AppError::Config {
                path: path.to_path_buf(),
                reason: "databasePath must be valid UTF-8 for SQLite".to_owned(),
            })?)?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        let database = Self { pool };
        database.initialize().await?;
        Ok(database)
    }

    async fn initialize(&self) -> Result<(), AppError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS project_state (
                project_id TEXT PRIMARY KEY NOT NULL,
                stopped INTEGER NOT NULL DEFAULT 0 CHECK (stopped IN (0, 1)),
                blocked INTEGER NOT NULL CHECK (blocked IN (0, 1)),
                blocked_reason TEXT,
                updated_at TEXT NOT NULL
            ) STRICT
            "#,
        )
        .execute(&self.pool)
        .await?;
        let schema_version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&self.pool)
            .await?;
        let deployments_table_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'deployments'",
        )
        .fetch_one(&self.pool)
        .await?;
        match (schema_version, deployments_table_exists) {
            (0, 0) => self.create_schema_v2().await?,
            (0, 1) => {
                self.migrate_schema_v0_to_v1().await?;
                self.migrate_schema_v1_to_v2().await?;
            }
            (1, 1) => self.migrate_schema_v1_to_v2().await?,
            (2, 1) => {}
            (version, table_count) => {
                return Err(AppError::InvalidState {
                    reason: format!(
                        "unsupported deployment database schema version {version} with deployments table count {table_count}"
                    ),
                });
            }
        }
        self.create_deployment_indexes().await?;
        Ok(())
    }

    async fn create_schema_v2(&self) -> Result<(), AppError> {
        let statement = create_deployments_table_sql("deployments")?;
        sqlx::query(AssertSqlSafe(statement))
            .execute(&self.pool)
            .await?;
        sqlx::query("PRAGMA user_version = 2")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn migrate_schema_v0_to_v1(&self) -> Result<(), AppError> {
        let mut transaction = self.pool.begin().await?;
        let statement = create_deployments_table_sql("deployments_v1")?;
        sqlx::query(AssertSqlSafe(statement))
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            r#"
            INSERT INTO deployments_v1(
                id, project_id, event_key, kind, tag, requested_sha, commit_sha,
                release_path, previous_release_path, status, error_code, error_message,
                retry_of, created_at, updated_at, completed_at
            )
            SELECT
                id, project_id, event_key, kind, tag, requested_sha, commit_sha,
                release_path, previous_release_path, status, error_code, error_message,
                retry_of, created_at, updated_at, completed_at
            FROM deployments
            "#,
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query("DROP TABLE deployments")
            .execute(&mut *transaction)
            .await?;
        sqlx::query("ALTER TABLE deployments_v1 RENAME TO deployments")
            .execute(&mut *transaction)
            .await?;
        sqlx::query("PRAGMA user_version = 1")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn migrate_schema_v1_to_v2(&self) -> Result<(), AppError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "ALTER TABLE project_state ADD COLUMN stopped INTEGER NOT NULL DEFAULT 0 CHECK (stopped IN (0, 1))",
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn create_deployment_indexes(&self) -> Result<(), AppError> {
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS deployments_queue_idx
            ON deployments(status, created_at, id)
            "#,
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS deployments_project_idx
            ON deployments(project_id, created_at DESC)
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn register_projects<'a>(
        &self,
        project_ids: impl Iterator<Item = &'a String>,
    ) -> Result<(), AppError> {
        let now = now();
        let mut transaction = self.pool.begin().await?;
        for project_id in project_ids {
            sqlx::query(
                r#"
                INSERT INTO project_state(
                    project_id, stopped, blocked, blocked_reason, updated_at
                ) VALUES (?, 0, 0, NULL, ?)
                ON CONFLICT(project_id) DO NOTHING
                "#,
            )
            .bind(project_id)
            .bind(&now)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn health(&self) -> Result<(), AppError> {
        let value: i64 = sqlx::query_scalar("SELECT 1").fetch_one(&self.pool).await?;
        if value != 1 {
            return Err(AppError::InvalidState {
                reason: format!("SQLite health query returned {value}, expected 1"),
            });
        }
        Ok(())
    }

    pub async fn create_webhook_deployment(
        &self,
        project_id: &str,
        event_key: &str,
        tag: &str,
        requested_sha: &str,
    ) -> Result<CreatedDeployment, AppError> {
        let id = Uuid::now_v7().to_string();
        let now = now();
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query(
            r#"
            INSERT INTO deployments(
                id, project_id, event_key, kind, tag, requested_sha, status, created_at, updated_at
            )
            SELECT ?, ?, ?, 'deploy', ?, ?, 'queued', ?, ?
            FROM project_state
            WHERE project_id = ? AND stopped = 0
            ON CONFLICT(event_key) DO NOTHING
            "#,
        )
        .bind(&id)
        .bind(project_id)
        .bind(event_key)
        .bind(tag)
        .bind(requested_sha)
        .bind(&now)
        .bind(&now)
        .bind(project_id)
        .execute(&mut *transaction)
        .await?;
        let created = result.rows_affected() == 1;
        if !created {
            let project = sqlx::query_as::<_, ProjectStateRow>(
                "SELECT * FROM project_state WHERE project_id = ?",
            )
            .bind(project_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or_else(|| AppError::ProjectNotFound {
                project_id: project_id.to_owned(),
            })?;
            if project.stopped != 0 {
                return Err(AppError::ProjectStopped {
                    project_id: project_id.to_owned(),
                });
            }
        }
        let row = fetch_by_event_key(&mut transaction, event_key).await?;
        transaction.commit().await?;
        Ok(CreatedDeployment {
            deployment: row.try_into()?,
            created,
        })
    }

    pub async fn create_retry(&self, deployment_id: &str) -> Result<Deployment, AppError> {
        let original = self.get_deployment(deployment_id).await?;
        if !matches!(
            original.status,
            DeploymentStatus::Failed | DeploymentStatus::RollbackFailed
        ) {
            return Err(AppError::DeploymentNotRetryable {
                deployment_id: original.id,
                status: original.status.to_string(),
            });
        }
        let id = Uuid::now_v7().to_string();
        let event_key = format!("retry:{deployment_id}:{id}");
        let now = now();
        sqlx::query(
            r#"
            INSERT INTO deployments(
                id, project_id, event_key, kind, tag, requested_sha, commit_sha,
                release_path, status, retry_of, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(&original.project_id)
        .bind(&event_key)
        .bind(original.kind.to_string())
        .bind(&original.tag)
        .bind(&original.requested_sha)
        .bind(&original.commit_sha)
        .bind(path_string(original.release_path.as_deref())?)
        .bind(&original.id)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        self.get_deployment(&id).await
    }

    pub async fn create_rollback(
        &self,
        project_id: &str,
        commit_sha: &str,
    ) -> Result<Deployment, AppError> {
        let release = self
            .successful_release(project_id, commit_sha)
            .await?
            .ok_or_else(|| AppError::ReleaseNotFound {
                project_id: project_id.to_owned(),
                commit_sha: commit_sha.to_owned(),
            })?;
        let release_path = release.release_path.ok_or_else(|| AppError::InvalidState {
            reason: format!("successful deployment {} has no releasePath", release.id),
        })?;
        if !release_path.is_dir() {
            return Err(AppError::ReleaseNotFound {
                project_id: project_id.to_owned(),
                commit_sha: commit_sha.to_owned(),
            });
        }
        let id = Uuid::now_v7().to_string();
        let event_key = format!("rollback:{project_id}:{commit_sha}:{id}");
        let now = now();
        sqlx::query(
            r#"
            INSERT INTO deployments(
                id, project_id, event_key, kind, tag, requested_sha, commit_sha,
                release_path, status, created_at, updated_at
            ) VALUES (?, ?, ?, 'rollback', ?, ?, ?, ?, 'queued', ?, ?)
            "#,
        )
        .bind(&id)
        .bind(project_id)
        .bind(&event_key)
        .bind(&release.tag)
        .bind(commit_sha)
        .bind(commit_sha)
        .bind(path_string(Some(&release_path))?)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        self.get_deployment(&id).await
    }

    pub async fn get_deployment(&self, deployment_id: &str) -> Result<Deployment, AppError> {
        let row = sqlx::query_as::<_, DeploymentRow>("SELECT * FROM deployments WHERE id = ?")
            .bind(deployment_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| AppError::DeploymentNotFound {
                deployment_id: deployment_id.to_owned(),
            })?;
        row.try_into()
    }

    pub async fn next_pending(&self) -> Result<Option<Deployment>, AppError> {
        let row = sqlx::query_as::<_, DeploymentRow>(
            r#"
            UPDATE deployments
            SET
                status = CASE WHEN status = 'queued' THEN 'fetching' ELSE status END,
                updated_at = CASE WHEN status = 'queued' THEN ? ELSE updated_at END
            WHERE id = (
                SELECT d.id
                FROM deployments d
                JOIN project_state p ON p.project_id = d.project_id
                WHERE d.status IN (
                    'queued', 'fetching', 'building', 'migrating', 'activating',
                    'healthChecking', 'rollingBack'
                ) AND p.stopped = 0 AND p.blocked = 0
                ORDER BY d.created_at ASC, d.id ASC
                LIMIT 1
            )
            RETURNING *
            "#,
        )
        .bind(now())
        .fetch_optional(&self.pool)
        .await?;
        row.map(Deployment::try_from).transpose()
    }

    pub async fn deployments_for_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<Deployment>, AppError> {
        let rows = sqlx::query_as::<_, DeploymentRow>(
            "SELECT * FROM deployments WHERE project_id = ? ORDER BY created_at DESC, id DESC",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Deployment::try_from).collect()
    }

    pub async fn project_state(&self, project_id: &str) -> Result<ProjectState, AppError> {
        let row = sqlx::query_as::<_, ProjectStateRow>(
            "SELECT * FROM project_state WHERE project_id = ?",
        )
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::ProjectNotFound {
            project_id: project_id.to_owned(),
        })?;
        Ok(row.into())
    }

    pub async fn stop_project(&self, project_id: &str) -> Result<ProjectState, AppError> {
        let now = now();
        update_project_exactly_one(
            sqlx::query(
                "UPDATE project_state SET stopped = 1, updated_at = ? WHERE project_id = ?",
            )
            .bind(now)
            .bind(project_id)
            .execute(&self.pool)
            .await?
            .rows_affected(),
            project_id,
        )?;
        self.project_state(project_id).await
    }

    pub async fn start_project(&self, project_id: &str) -> Result<ProjectState, AppError> {
        let now = now();
        let result = sqlx::query(
            "UPDATE project_state SET stopped = 0, updated_at = ? WHERE project_id = ? AND blocked = 0",
        )
        .bind(now)
        .bind(project_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            let project = self.project_state(project_id).await?;
            return Err(AppError::ProjectBlocked {
                project_id: project_id.to_owned(),
                reason: project
                    .blocked_reason
                    .unwrap_or_else(|| "no blocked reason was recorded".to_owned()),
            });
        }
        self.project_state(project_id).await
    }

    pub async fn delete_project(&self, project_id: &str) -> Result<(), AppError> {
        let mut transaction = self.pool.begin().await?;
        let project = sqlx::query_as::<_, ProjectStateRow>(
            "SELECT * FROM project_state WHERE project_id = ?",
        )
        .bind(project_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| AppError::ProjectNotFound {
            project_id: project_id.to_owned(),
        })?;
        if project.stopped == 0 {
            return Err(AppError::ProjectMustBeStopped {
                project_id: project_id.to_owned(),
            });
        }
        let active = sqlx::query_as::<_, DeploymentRow>(
            r#"
            SELECT * FROM deployments
            WHERE project_id = ? AND status IN (
                'fetching', 'building', 'migrating', 'activating',
                'healthChecking', 'rollingBack'
            )
            ORDER BY created_at ASC, id ASC
            LIMIT 1
            "#,
        )
        .bind(project_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(row) = active {
            let deployment = Deployment::try_from(row)?;
            return Err(AppError::ProjectDeploymentActive {
                project_id: project_id.to_owned(),
                deployment_id: deployment.id,
                status: deployment.status.to_string(),
            });
        }
        sqlx::query("DELETE FROM deployments WHERE project_id = ?")
            .bind(project_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM project_state WHERE project_id = ?")
            .bind(project_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn clean_deployments(
        &self,
        project_id: &str,
        keep: u64,
        deployment_type: CleanDeploymentType,
    ) -> Result<Vec<Deployment>, AppError> {
        let keep = i64::try_from(keep).map_err(|_| AppError::InvalidState {
            reason: format!("clean keep value {keep} exceeds the supported SQLite integer range"),
        })?;
        let mut transaction = self.pool.begin().await?;
        let project_exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM project_state WHERE project_id = ?")
                .bind(project_id)
                .fetch_one(&mut *transaction)
                .await?;
        if project_exists == 0 {
            return Err(AppError::ProjectNotFound {
                project_id: project_id.to_owned(),
            });
        }
        let active = sqlx::query_as::<_, DeploymentRow>(
            r#"
            SELECT * FROM deployments
            WHERE project_id = ? AND status IN (
                'queued', 'fetching', 'building', 'migrating', 'activating',
                'healthChecking', 'rollingBack'
            )
            ORDER BY created_at ASC, id ASC
            LIMIT 1
            "#,
        )
        .bind(project_id)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(row) = active {
            let deployment = Deployment::try_from(row)?;
            return Err(AppError::ProjectDeploymentActive {
                project_id: project_id.to_owned(),
                deployment_id: deployment.id,
                status: deployment.status.to_string(),
            });
        }
        let query = match deployment_type {
            CleanDeploymentType::Failed => {
                "SELECT * FROM deployments WHERE project_id = ? AND status IN ('failed', 'rollbackFailed') ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?"
            }
            CleanDeploymentType::Succeeded => {
                "SELECT * FROM deployments WHERE project_id = ? AND status = 'succeeded' ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?"
            }
            CleanDeploymentType::All => {
                "SELECT * FROM deployments WHERE project_id = ? AND status IN ('succeeded', 'failed', 'rollbackFailed') ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?"
            }
        };
        let rows = sqlx::query_as::<_, DeploymentRow>(query)
            .bind(project_id)
            .bind(keep)
            .fetch_all(&mut *transaction)
            .await?;
        let deployments = rows
            .into_iter()
            .map(Deployment::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        for deployment in &deployments {
            sqlx::query("UPDATE deployments SET retry_of = NULL WHERE retry_of = ?")
                .bind(&deployment.id)
                .execute(&mut *transaction)
                .await?;
            sqlx::query("DELETE FROM deployments WHERE id = ?")
                .bind(&deployment.id)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(deployments)
    }

    pub async fn set_status(
        &self,
        deployment_id: &str,
        status: DeploymentStatus,
    ) -> Result<(), AppError> {
        let now = now();
        update_exactly_one(
            sqlx::query("UPDATE deployments SET status = ?, updated_at = ? WHERE id = ?")
                .bind(status.to_string())
                .bind(now)
                .bind(deployment_id)
                .execute(&self.pool)
                .await?
                .rows_affected(),
            deployment_id,
        )
    }

    pub async fn set_resolved_commit(
        &self,
        deployment_id: &str,
        commit_sha: &str,
    ) -> Result<(), AppError> {
        let now = now();
        update_exactly_one(
            sqlx::query("UPDATE deployments SET commit_sha = ?, updated_at = ? WHERE id = ?")
                .bind(commit_sha)
                .bind(now)
                .bind(deployment_id)
                .execute(&self.pool)
                .await?
                .rows_affected(),
            deployment_id,
        )
    }

    pub async fn set_release(
        &self,
        deployment_id: &str,
        release_path: &Path,
    ) -> Result<(), AppError> {
        let now = now();
        update_exactly_one(
            sqlx::query("UPDATE deployments SET release_path = ?, updated_at = ? WHERE id = ?")
                .bind(path_string(Some(release_path))?)
                .bind(now)
                .bind(deployment_id)
                .execute(&self.pool)
                .await?
                .rows_affected(),
            deployment_id,
        )
    }

    pub async fn set_activation(
        &self,
        deployment_id: &str,
        previous_release_path: Option<&Path>,
    ) -> Result<(), AppError> {
        let now = now();
        update_exactly_one(
            sqlx::query(
                r#"
                UPDATE deployments
                SET status = 'activating', previous_release_path = ?, updated_at = ?
                WHERE id = ?
                "#,
            )
            .bind(path_string(previous_release_path)?)
            .bind(now)
            .bind(deployment_id)
            .execute(&self.pool)
            .await?
            .rows_affected(),
            deployment_id,
        )
    }

    pub async fn mark_succeeded(&self, deployment_id: &str) -> Result<(), AppError> {
        let now = now();
        update_exactly_one(
            sqlx::query(
                r#"
                UPDATE deployments
                SET status = 'succeeded', error_code = NULL, error_message = NULL,
                    updated_at = ?, completed_at = ?
                WHERE id = ?
                "#,
            )
            .bind(&now)
            .bind(&now)
            .bind(deployment_id)
            .execute(&self.pool)
            .await?
            .rows_affected(),
            deployment_id,
        )
    }

    pub async fn mark_failed(
        &self,
        deployment_id: &str,
        error_code: &str,
        error_message: &str,
    ) -> Result<(), AppError> {
        self.mark_terminal(
            deployment_id,
            DeploymentStatus::Failed,
            error_code,
            error_message,
        )
        .await
    }

    pub async fn mark_rollback_failed(
        &self,
        deployment_id: &str,
        error_code: &str,
        error_message: &str,
    ) -> Result<(), AppError> {
        self.mark_terminal(
            deployment_id,
            DeploymentStatus::RollbackFailed,
            error_code,
            error_message,
        )
        .await
    }

    async fn mark_terminal(
        &self,
        deployment_id: &str,
        status: DeploymentStatus,
        error_code: &str,
        error_message: &str,
    ) -> Result<(), AppError> {
        let now = now();
        update_exactly_one(
            sqlx::query(
                r#"
                UPDATE deployments
                SET status = ?, error_code = ?, error_message = ?, updated_at = ?, completed_at = ?
                WHERE id = ?
                "#,
            )
            .bind(status.to_string())
            .bind(error_code)
            .bind(error_message)
            .bind(&now)
            .bind(&now)
            .bind(deployment_id)
            .execute(&self.pool)
            .await?
            .rows_affected(),
            deployment_id,
        )
    }

    pub async fn block_project(&self, project_id: &str, reason: &str) -> Result<(), AppError> {
        let now = now();
        update_project_exactly_one(
            sqlx::query(
                "UPDATE project_state SET blocked = 1, blocked_reason = ?, updated_at = ? WHERE project_id = ?",
            )
            .bind(reason)
            .bind(now)
            .bind(project_id)
            .execute(&self.pool)
            .await?
            .rows_affected(),
            project_id,
        )
    }

    pub async fn unblock_project(&self, project_id: &str) -> Result<(), AppError> {
        let now = now();
        update_project_exactly_one(
            sqlx::query(
                "UPDATE project_state SET blocked = 0, blocked_reason = NULL, updated_at = ? WHERE project_id = ?",
            )
            .bind(now)
            .bind(project_id)
            .execute(&self.pool)
            .await?
            .rows_affected(),
            project_id,
        )
    }

    pub async fn successful_release(
        &self,
        project_id: &str,
        commit_sha: &str,
    ) -> Result<Option<Deployment>, AppError> {
        let row = sqlx::query_as::<_, DeploymentRow>(
            r#"
            SELECT * FROM deployments
            WHERE project_id = ? AND commit_sha = ? AND status = 'succeeded'
            ORDER BY completed_at DESC, id DESC
            LIMIT 1
            "#,
        )
        .bind(project_id)
        .bind(commit_sha)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Deployment::try_from).transpose()
    }

    pub async fn retained_release_paths(&self, project_id: &str) -> Result<Vec<String>, AppError> {
        let paths = sqlx::query_scalar::<_, String>(
            r#"
            SELECT release_path
            FROM deployments
            WHERE project_id = ? AND status = 'succeeded' AND release_path IS NOT NULL
            GROUP BY release_path
            ORDER BY MAX(completed_at) DESC
            "#,
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(paths)
    }
}

fn create_deployments_table_sql(table_name: &str) -> Result<String, AppError> {
    if !matches!(table_name, "deployments" | "deployments_v1") {
        return Err(AppError::InvalidState {
            reason: format!("unsupported internal deployments table name {table_name:?}"),
        });
    }
    Ok(format!(
        r#"
        CREATE TABLE {table_name} (
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
            FOREIGN KEY (retry_of) REFERENCES {table_name}(id)
        ) STRICT
        "#
    ))
}

async fn fetch_by_event_key(
    transaction: &mut Transaction<'_, Sqlite>,
    event_key: &str,
) -> Result<DeploymentRow, AppError> {
    let row = sqlx::query_as::<_, DeploymentRow>("SELECT * FROM deployments WHERE event_key = ?")
        .bind(event_key)
        .fetch_one(&mut **transaction)
        .await?;
    Ok(row)
}

fn update_exactly_one(rows_affected: u64, deployment_id: &str) -> Result<(), AppError> {
    if rows_affected != 1 {
        return Err(AppError::DeploymentNotFound {
            deployment_id: deployment_id.to_owned(),
        });
    }
    Ok(())
}

fn update_project_exactly_one(rows_affected: u64, project_id: &str) -> Result<(), AppError> {
    if rows_affected != 1 {
        return Err(AppError::ProjectNotFound {
            project_id: project_id.to_owned(),
        });
    }
    Ok(())
}

fn path_string(path: Option<&Path>) -> Result<Option<String>, AppError> {
    path.map(|value| {
        value
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| AppError::InvalidState {
                reason: format!("path {value:?} is not valid UTF-8"),
            })
    })
    .transpose()
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
