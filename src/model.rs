use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::error::AppError;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeploymentKind {
    Deploy,
    Rollback,
}

impl Display for DeploymentKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Deploy => "deploy",
            Self::Rollback => "rollback",
        })
    }
}

impl FromStr for DeploymentKind {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "deploy" => Ok(Self::Deploy),
            "rollback" => Ok(Self::Rollback),
            _ => Err(AppError::InvalidState {
                reason: format!("unknown deployment kind {value:?}"),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeploymentStatus {
    Queued,
    Fetching,
    Building,
    Activating,
    HealthChecking,
    RollingBack,
    Succeeded,
    Failed,
    RollbackFailed,
}

impl Display for DeploymentStatus {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Queued => "queued",
            Self::Fetching => "fetching",
            Self::Building => "building",
            Self::Activating => "activating",
            Self::HealthChecking => "healthChecking",
            Self::RollingBack => "rollingBack",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::RollbackFailed => "rollbackFailed",
        })
    }
}

impl FromStr for DeploymentStatus {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "fetching" => Ok(Self::Fetching),
            "building" => Ok(Self::Building),
            "activating" => Ok(Self::Activating),
            "healthChecking" => Ok(Self::HealthChecking),
            "rollingBack" => Ok(Self::RollingBack),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "rollbackFailed" => Ok(Self::RollbackFailed),
            _ => Err(AppError::InvalidState {
                reason: format!("unknown deployment status {value:?}"),
            }),
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct DeploymentRow {
    pub id: String,
    pub project_id: String,
    pub event_key: String,
    pub kind: String,
    pub tag: String,
    pub requested_sha: String,
    pub commit_sha: Option<String>,
    pub release_path: Option<String>,
    pub previous_release_path: Option<String>,
    pub status: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub retry_of: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Deployment {
    pub id: String,
    pub project_id: String,
    pub event_key: String,
    pub kind: DeploymentKind,
    pub tag: String,
    pub requested_sha: String,
    pub commit_sha: Option<String>,
    pub release_path: Option<PathBuf>,
    pub previous_release_path: Option<PathBuf>,
    pub status: DeploymentStatus,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub retry_of: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

impl TryFrom<DeploymentRow> for Deployment {
    type Error = AppError;

    fn try_from(row: DeploymentRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            project_id: row.project_id,
            event_key: row.event_key,
            kind: row.kind.parse()?,
            tag: row.tag,
            requested_sha: row.requested_sha,
            commit_sha: row.commit_sha,
            release_path: row.release_path.map(PathBuf::from),
            previous_release_path: row.previous_release_path.map(PathBuf::from),
            status: row.status.parse()?,
            error_code: row.error_code,
            error_message: row.error_message,
            retry_of: row.retry_of,
            created_at: row.created_at,
            updated_at: row.updated_at,
            completed_at: row.completed_at,
        })
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct ProjectStateRow {
    pub project_id: String,
    pub blocked: i64,
    pub blocked_reason: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectState {
    pub project_id: String,
    pub blocked: bool,
    pub blocked_reason: Option<String>,
    pub updated_at: String,
}

impl From<ProjectStateRow> for ProjectState {
    fn from(row: ProjectStateRow) -> Self {
        Self {
            project_id: row.project_id,
            blocked: row.blocked != 0,
            blocked_reason: row.blocked_reason,
            updated_at: row.updated_at,
        }
    }
}
