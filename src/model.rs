use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, FixedOffset};
use serde::Serializer;
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
    Migrating,
    Activating,
    HealthChecking,
    RollingBack,
    Succeeded,
    Failed,
    RollbackFailed,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CleanDeploymentType {
    Failed,
    Succeeded,
    All,
}

impl Display for DeploymentStatus {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Queued => "queued",
            Self::Fetching => "fetching",
            Self::Building => "building",
            Self::Migrating => "migrating",
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
            "migrating" => Ok(Self::Migrating),
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
    #[serde(serialize_with = "serialize_china_datetime")]
    pub created_at: String,
    #[serde(serialize_with = "serialize_china_datetime")]
    pub updated_at: String,
    #[serde(serialize_with = "serialize_optional_china_datetime")]
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
    pub stopped: i64,
    pub blocked: i64,
    pub blocked_reason: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectStatus {
    Running,
    Stopped,
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectState {
    pub project_id: String,
    pub status: ProjectStatus,
    pub stopped: bool,
    pub blocked: bool,
    pub blocked_reason: Option<String>,
    #[serde(serialize_with = "serialize_china_datetime")]
    pub updated_at: String,
}

fn serialize_china_datetime<S>(value: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::ser::Error;

    let parsed = DateTime::parse_from_rfc3339(value).map_err(S::Error::custom)?;
    let offset = FixedOffset::east_opt(8 * 60 * 60)
        .ok_or_else(|| S::Error::custom("Asia/Shanghai UTC offset is invalid"))?;
    serializer.serialize_str(
        &parsed
            .with_timezone(&offset)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
    )
}

fn serialize_optional_china_datetime<S>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(value) => serialize_china_datetime(value, serializer),
        None => serializer.serialize_none(),
    }
}

impl From<ProjectStateRow> for ProjectState {
    fn from(row: ProjectStateRow) -> Self {
        let stopped = row.stopped != 0;
        let blocked = row.blocked != 0;
        let status = if blocked {
            ProjectStatus::Blocked
        } else if stopped {
            ProjectStatus::Stopped
        } else {
            ProjectStatus::Running
        };
        Self {
            project_id: row.project_id,
            status,
            stopped,
            blocked,
            blocked_reason: row.blocked_reason,
            updated_at: row.updated_at,
        }
    }
}
