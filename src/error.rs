use std::ffi::OsString;
use std::path::PathBuf;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("configuration {path} is invalid: {reason}")]
    Config { path: PathBuf, reason: String },
    #[error("database operation failed: {source}")]
    Database {
        #[from]
        source: sqlx::Error,
    },
    #[error("filesystem operation {operation} failed for {path}: {source}")]
    FileSystem {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "command {program:?} {args:?} failed with status {status:?}; stdout={stdout:?}; stderr={stderr:?}"
    )]
    CommandFailed {
        program: PathBuf,
        args: Vec<OsString>,
        status: Option<i32>,
        stdout: String,
        stderr: String,
    },
    #[error("command {program:?} {args:?} timed out after {timeout_seconds} seconds")]
    CommandTimedOut {
        program: PathBuf,
        args: Vec<OsString>,
        timeout_seconds: u64,
    },
    #[error("HTTP health check for {url} failed: {reason}")]
    HealthCheck { url: String, reason: String },
    #[error("request validation failed: {reason}")]
    RequestValidation { reason: String },
    #[error("request body is too large: {reason}")]
    RequestTooLarge { reason: String },
    #[error("authentication failed: {reason}")]
    Authentication { reason: String },
    #[error("project {project_id} was not found")]
    ProjectNotFound { project_id: String },
    #[error("deployment {deployment_id} was not found")]
    DeploymentNotFound { deployment_id: String },
    #[error("deployment {deployment_id} is in state {status}, which cannot be retried")]
    DeploymentNotRetryable {
        deployment_id: String,
        status: String,
    },
    #[error("project {project_id} has no successful release for commit {commit_sha}")]
    ReleaseNotFound {
        project_id: String,
        commit_sha: String,
    },
    #[error("deployment state is invalid: {reason}")]
    InvalidState { reason: String },
    #[error("background task failed: {reason}")]
    TaskJoin { reason: String },
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config { .. } => "configInvalid",
            Self::Database { .. } => "databaseError",
            Self::FileSystem { .. } => "fileSystemError",
            Self::CommandFailed { .. } => "commandFailed",
            Self::CommandTimedOut { .. } => "commandTimedOut",
            Self::HealthCheck { .. } => "healthCheckFailed",
            Self::RequestValidation { .. } => "requestInvalid",
            Self::RequestTooLarge { .. } => "requestTooLarge",
            Self::Authentication { .. } => "authenticationFailed",
            Self::ProjectNotFound { .. } => "projectNotFound",
            Self::DeploymentNotFound { .. } => "deploymentNotFound",
            Self::DeploymentNotRetryable { .. } => "deploymentNotRetryable",
            Self::ReleaseNotFound { .. } => "releaseNotFound",
            Self::InvalidState { .. } => "deploymentStateInvalid",
            Self::TaskJoin { .. } => "taskFailed",
        }
    }

    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::Authentication { .. } => StatusCode::UNAUTHORIZED,
            Self::ProjectNotFound { .. }
            | Self::DeploymentNotFound { .. }
            | Self::ReleaseNotFound { .. } => StatusCode::NOT_FOUND,
            Self::RequestValidation { .. }
            | Self::Config { .. }
            | Self::DeploymentNotRetryable { .. } => StatusCode::BAD_REQUEST,
            Self::RequestTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::InvalidState { .. } => StatusCode::CONFLICT,
            Self::Database { .. }
            | Self::FileSystem { .. }
            | Self::CommandFailed { .. }
            | Self::CommandTimedOut { .. }
            | Self::HealthCheck { .. }
            | Self::TaskJoin { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope {
    ok: bool,
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    code: &'static str,
    message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = ErrorEnvelope {
            ok: false,
            error: ErrorBody {
                code: self.code(),
                message: self.to_string(),
            },
        };
        (status, Json(body)).into_response()
    }
}

pub fn file_error(operation: &'static str, path: PathBuf, source: std::io::Error) -> AppError {
    AppError::FileSystem {
        operation,
        path,
        source,
    }
}
