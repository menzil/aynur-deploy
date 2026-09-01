use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::config::{LoadedConfig, Project};
use crate::db::Database;
use crate::error::AppError;
use crate::model::Deployment;

const GITEE_TOKEN_HEADER: &str = "x-gitee-token";
const GITEE_PING_HEADER: &str = "x-gitee-ping";
const TAG_HOOK_NAME: &str = "tag_push_hooks";

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<LoadedConfig>,
    pub database: Database,
    pub notifier: Arc<Notify>,
}

#[derive(Debug, Deserialize)]
struct GiteeTagPayload {
    hook_name: String,
    created: bool,
    deleted: bool,
    r#ref: String,
    after: String,
    repository: GiteeRepository,
}

#[derive(Debug, Deserialize)]
struct GiteeRepository {
    full_name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    ok: bool,
    status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HookResponse {
    ok: bool,
    accepted: bool,
    duplicate: bool,
    reason: Option<&'static str>,
    deployment: Option<Deployment>,
}

pub fn router(state: AppState) -> Router {
    let max_body_bytes = state.config.global.max_webhook_body_bytes;
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/hooks/gitee/{projectId}", post(gitee_hook))
        .layer(axum::extract::DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}

async fn healthz(State(state): State<AppState>) -> Result<Json<HealthResponse>, AppError> {
    state.database.health().await?;
    Ok(Json(HealthResponse {
        ok: true,
        status: "healthy",
    }))
}

async fn gitee_hook(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<(StatusCode, Json<HookResponse>), AppError> {
    let project = state.config.project(&project_id)?;
    authenticate(project, &headers)?;
    if let Some(ping) = headers.get(GITEE_PING_HEADER) {
        let ping_value = ping
            .to_str()
            .map_err(|source| AppError::RequestValidation {
                reason: format!("X-Gitee-Ping header is not valid ASCII: {source}"),
            })?;
        if ping_value != "true" {
            return Err(AppError::RequestValidation {
                reason: format!("X-Gitee-Ping must be exactly \"true\", got {ping_value:?}"),
            });
        }
        return Ok((
            StatusCode::OK,
            Json(HookResponse {
                ok: true,
                accepted: false,
                duplicate: false,
                reason: Some("ping"),
                deployment: None,
            }),
        ));
    }

    let body = body.map_err(|source| {
        if source.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::RequestTooLarge {
                reason: source.body_text(),
            }
        } else {
            AppError::RequestValidation {
                reason: source.body_text(),
            }
        }
    })?;
    let payload: GiteeTagPayload =
        serde_json::from_slice(&body).map_err(|source| AppError::RequestValidation {
            reason: format!("request body must be a valid Gitee Tag WebHook JSON object: {source}"),
        })?;
    validate_payload(project, &payload)?;
    if payload.deleted {
        return Ok(ignored_response("tagDeleted"));
    }
    if !payload.created {
        return Ok(ignored_response("tagNotCreated"));
    }
    let tag =
        payload
            .r#ref
            .strip_prefix("refs/tags/")
            .ok_or_else(|| AppError::RequestValidation {
                reason: format!("ref {:?} is not a Tag ref", payload.r#ref),
            })?;
    if !project.tag_pattern.is_match(tag) {
        return Ok(ignored_response("tagPatternMismatch"));
    }

    let event_key = format!(
        "gitee:{}:{}:{}",
        project.project_id, payload.r#ref, payload.after
    );
    let created = state
        .database
        .create_webhook_deployment(&project.project_id, &event_key, tag, &payload.after)
        .await?;
    if created.created {
        state.notifier.notify_one();
    }
    let status = if created.created {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(HookResponse {
            ok: true,
            accepted: created.created,
            duplicate: !created.created,
            reason: None,
            deployment: Some(created.deployment),
        }),
    ))
}

fn authenticate(project: &Project, headers: &HeaderMap) -> Result<(), AppError> {
    let provided = headers
        .get(GITEE_TOKEN_HEADER)
        .ok_or_else(|| AppError::Authentication {
            reason: "X-Gitee-Token header is required".to_owned(),
        })?
        .as_bytes();
    if !constant_time_equal(provided, project.webhook_token.expose().as_bytes()) {
        return Err(AppError::Authentication {
            reason: "X-Gitee-Token does not match the configured project token".to_owned(),
        });
    }
    Ok(())
}

fn validate_payload(project: &Project, payload: &GiteeTagPayload) -> Result<(), AppError> {
    if payload.hook_name != TAG_HOOK_NAME {
        return Err(AppError::RequestValidation {
            reason: format!(
                "hook_name must be {TAG_HOOK_NAME:?}, got {:?}",
                payload.hook_name
            ),
        });
    }
    if payload.created && payload.deleted {
        return Err(AppError::RequestValidation {
            reason: "created and deleted cannot both be true".to_owned(),
        });
    }
    if payload.repository.full_name != project.repository_full_name {
        return Err(AppError::RequestValidation {
            reason: format!(
                "repository.full_name must be {:?}, got {:?}",
                project.repository_full_name, payload.repository.full_name
            ),
        });
    }
    let tag =
        payload
            .r#ref
            .strip_prefix("refs/tags/")
            .ok_or_else(|| AppError::RequestValidation {
                reason: format!("ref {:?} is not a Tag ref", payload.r#ref),
            })?;
    if tag.is_empty() {
        return Err(AppError::RequestValidation {
            reason: "ref must contain a non-empty Tag name".to_owned(),
        });
    }
    if payload.after.len() != 40
        || !payload
            .after
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AppError::RequestValidation {
            reason: format!(
                "after must be a lowercase 40-character hexadecimal Git object ID, got {:?}",
                payload.after
            ),
        });
    }
    if payload.created && payload.after == "0000000000000000000000000000000000000000" {
        return Err(AppError::RequestValidation {
            reason: "created Tag after cannot be the all-zero Git object ID".to_owned(),
        });
    }
    Ok(())
}

fn ignored_response(reason: &'static str) -> (StatusCode, Json<HookResponse>) {
    (
        StatusCode::OK,
        Json(HookResponse {
            ok: true,
            accepted: false,
            duplicate: false,
            reason: Some(reason),
            deployment: None,
        }),
    )
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let maximum = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..maximum {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_equal;

    #[test]
    fn token_comparison_requires_exact_bytes() {
        assert!(constant_time_equal(b"secret", b"secret"));
        assert!(!constant_time_equal(b"secret", b"Secret"));
        assert!(!constant_time_equal(b"secret", b"secret-longer"));
    }
}
