use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tracing::{info, warn};

use crate::config::{AliyunCdnCacheRefresh, CacheRefresh};
use crate::error::AppError;
use crate::process::{CommandSpec, run_command};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RefreshResponse {
    refresh_task_id: String,
    request_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TasksResponse {
    request_id: String,
    tasks: Tasks,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Tasks {
    #[serde(rename = "CDNTask")]
    cdn_task: Vec<CdnTask>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CdnTask {
    status: String,
    description: String,
    object_path: String,
    object_type: String,
    task_id: String,
}

pub async fn submit_cache_refresh(
    aliyun_command: &Path,
    command_timeout_seconds: u64,
    cache_refresh: &CacheRefresh,
) -> Result<Vec<String>, AppError> {
    match cache_refresh {
        CacheRefresh::AliyunCdn(config) => {
            submit_refresh(aliyun_command, command_timeout_seconds, config).await
        }
    }
}

pub async fn wait_for_cache_refresh(
    aliyun_command: &Path,
    command_timeout_seconds: u64,
    cache_refresh: &CacheRefresh,
    task_ids: &[String],
) -> Result<(), AppError> {
    match cache_refresh {
        CacheRefresh::AliyunCdn(config) => {
            wait_for_refresh(aliyun_command, command_timeout_seconds, config, task_ids).await
        }
    }
}

async fn submit_refresh(
    aliyun_command: &Path,
    command_timeout_seconds: u64,
    config: &AliyunCdnCacheRefresh,
) -> Result<Vec<String>, AppError> {
    let args = vec![
        os("cdn"),
        os("RefreshObjectCaches"),
        os("--ObjectPath"),
        OsString::from(config.object_path.as_str()),
        os("--ObjectType"),
        OsString::from(config.object_type.as_api_value()),
        os("--Force"),
        OsString::from(config.force.to_string()),
        os("--profile"),
        OsString::from(&config.profile),
    ];
    let output = run_with_retries(
        aliyun_command,
        command_timeout_seconds,
        args,
        config,
        "submit",
    )
    .await?;
    let response: RefreshResponse =
        serde_json::from_str(&output).map_err(|source| AppError::CacheRefresh {
            operation: "parse RefreshObjectCaches response",
            reason: format!("responseBody={output:?}; {source}"),
        })?;
    let task_ids = parse_task_ids(&response.refresh_task_id)?;
    info!(
        provider = "aliyunCdn",
        requestId = response.request_id,
        taskIds = ?task_ids,
        objectPath = %config.object_path,
        "CDN cache refresh submitted"
    );
    Ok(task_ids)
}

async fn wait_for_refresh(
    aliyun_command: &Path,
    command_timeout_seconds: u64,
    config: &AliyunCdnCacheRefresh,
    task_ids: &[String],
) -> Result<(), AppError> {
    let started = Instant::now();
    loop {
        let mut all_complete = true;
        for task_id in task_ids {
            match query_task(aliyun_command, command_timeout_seconds, config, task_id).await? {
                TaskState::Refreshing => all_complete = false,
                TaskState::Complete => {}
            }
        }
        if all_complete {
            return Ok(());
        }
        if started.elapsed() >= Duration::from_millis(config.timeout_ms) {
            return Err(AppError::CacheRefresh {
                operation: "wait for CDN refresh",
                reason: format!(
                    "taskIds={task_ids:?}; objectPath={:?}; timed out after {}ms",
                    config.object_path, config.timeout_ms
                ),
            });
        }
        tokio::time::sleep(Duration::from_millis(config.poll_interval_ms)).await;
    }
}

#[derive(Debug, Eq, PartialEq)]
enum TaskState {
    Refreshing,
    Complete,
}

async fn query_task(
    aliyun_command: &Path,
    command_timeout_seconds: u64,
    config: &AliyunCdnCacheRefresh,
    task_id: &str,
) -> Result<TaskState, AppError> {
    let args = vec![
        os("cdn"),
        os("DescribeRefreshTasks"),
        os("--TaskId"),
        OsString::from(task_id),
        os("--ObjectPath"),
        OsString::from(config.object_path.as_str()),
        os("--ObjectType"),
        OsString::from(config.object_type.as_response_value()),
        os("--profile"),
        OsString::from(&config.profile),
    ];
    let output = run_with_retries(
        aliyun_command,
        command_timeout_seconds,
        args,
        config,
        "query",
    )
    .await?;
    let response: TasksResponse =
        serde_json::from_str(&output).map_err(|source| AppError::CacheRefresh {
            operation: "parse DescribeRefreshTasks response",
            reason: format!("taskId={task_id:?}; responseBody={output:?}; {source}"),
        })?;
    validate_task_response(&response, config, task_id, &output)
}

fn validate_task_response(
    response: &TasksResponse,
    config: &AliyunCdnCacheRefresh,
    task_id: &str,
    response_body: &str,
) -> Result<TaskState, AppError> {
    let matching = response
        .tasks
        .cdn_task
        .iter()
        .filter(|task| task.task_id == task_id)
        .collect::<Vec<&CdnTask>>();
    if matching.is_empty() {
        return Ok(TaskState::Refreshing);
    }

    let mut all_complete = true;
    for task in matching {
        if task.object_path != config.object_path
            || task.object_type != config.object_type.as_response_value()
        {
            return Err(AppError::CacheRefresh {
                operation: "validate DescribeRefreshTasks response",
                reason: format!(
                    "requestId={:?}; taskId={task_id:?}; objectPath={:?}; objectType={:?}; expected objectPath={:?}, objectType={:?}",
                    response.request_id,
                    task.object_path,
                    task.object_type,
                    config.object_path,
                    config.object_type.as_response_value()
                ),
            });
        }
        match task.status.as_str() {
            "Refreshing" => all_complete = false,
            "Complete" => {}
            "Failed" => {
                return Err(AppError::CacheRefresh {
                    operation: "wait for CDN refresh",
                    reason: format!(
                        "requestId={:?}; taskId={task_id:?}; status={:?}; description={:?}",
                        response.request_id, task.status, task.description
                    ),
                });
            }
            status => {
                return Err(AppError::CacheRefresh {
                    operation: "validate DescribeRefreshTasks response",
                    reason: format!(
                        "requestId={:?}; taskId={task_id:?}; unsupported status={status:?}; responseBody={response_body:?}",
                        response.request_id
                    ),
                });
            }
        }
    }

    if all_complete {
        Ok(TaskState::Complete)
    } else {
        Ok(TaskState::Refreshing)
    }
}

async fn run_with_retries(
    aliyun_command: &Path,
    command_timeout_seconds: u64,
    args: Vec<OsString>,
    config: &AliyunCdnCacheRefresh,
    operation: &'static str,
) -> Result<String, AppError> {
    let mut last_error: Option<AppError> = None;
    for attempt in 1..=config.attempts {
        let spec = CommandSpec {
            program: aliyun_command.to_path_buf(),
            args: args.clone(),
            current_directory: None,
            environment: Vec::new(),
        };
        match run_command(&spec, command_timeout_seconds).await {
            Ok(output) => return Ok(output.stdout),
            Err(source) => {
                warn!(
                    provider = "aliyunCdn",
                    operation,
                    attempt,
                    attempts = config.attempts,
                    errorCode = source.code(),
                    error = %source,
                    "CDN cache refresh API call failed"
                );
                last_error = Some(source);
                if attempt < config.attempts {
                    tokio::time::sleep(Duration::from_millis(config.retry_delay_ms)).await;
                }
            }
        }
    }
    Err(last_error.expect("cache refresh attempts is greater than zero"))
}

fn parse_task_ids(value: &str) -> Result<Vec<String>, AppError> {
    let task_ids = value
        .split(',')
        .map(str::trim)
        .map(str::to_owned)
        .collect::<Vec<String>>();
    if task_ids.is_empty()
        || task_ids.iter().any(|task_id| {
            task_id.is_empty() || !task_id.chars().all(|value| value.is_ascii_digit())
        })
    {
        return Err(AppError::CacheRefresh {
            operation: "parse RefreshObjectCaches task IDs",
            reason: format!(
                "RefreshTaskId must contain comma-separated decimal IDs, got {value:?}"
            ),
        });
    }
    Ok(task_ids)
}

fn os(value: &'static str) -> OsString {
    OsString::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::config::AliyunCdnObjectType;

    #[test]
    fn task_ids_are_strict_decimal_values() {
        assert_eq!(parse_task_ids("123,456").unwrap(), ["123", "456"]);
        assert!(parse_task_ids("123,task").is_err());
        assert!(parse_task_ids("").is_err());
    }

    #[test]
    fn task_is_refreshing_until_it_is_visible() {
        let response: TasksResponse =
            serde_json::from_str(r#"{"RequestId":"query-request","Tasks":{"CDNTask":[]}}"#)
                .unwrap();

        assert_eq!(
            validate_task_response(&response, &test_config(), "123", "response").unwrap(),
            TaskState::Refreshing
        );
    }

    #[test]
    fn every_task_object_with_the_same_id_must_complete() {
        let response: TasksResponse = serde_json::from_str(
            r#"{"RequestId":"query-request","Tasks":{"CDNTask":[{"Status":"Complete","Description":"","ObjectPath":"https://static.orhan.cn/","ObjectType":"directory","TaskId":"123"},{"Status":"Refreshing","Description":"","ObjectPath":"https://static.orhan.cn/","ObjectType":"directory","TaskId":"123"}]}}"#,
        )
        .unwrap();

        assert_eq!(
            validate_task_response(&response, &test_config(), "123", "response").unwrap(),
            TaskState::Refreshing
        );
    }

    #[tokio::test]
    async fn connector_submits_and_waits_for_directory_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let command = directory.path().join("aliyun");
        fs::write(
            &command,
            r#"#!/bin/sh
case "$2" in
  RefreshObjectCaches)
    printf '%s\n' '{"RefreshTaskId":"123,456","RequestId":"submit-request"}'
    ;;
  DescribeRefreshTasks)
    task_id="$4"
    [ "$8" = "directory" ] || exit 65
    printf '%s\n' "{\"RequestId\":\"query-request\",\"Tasks\":{\"CDNTask\":[{\"Status\":\"Complete\",\"Description\":\"\",\"ObjectPath\":\"https://static.orhan.cn/\",\"ObjectType\":\"directory\",\"TaskId\":\"$task_id\"}]}}"
    ;;
  *)
    exit 64
    ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();
        let cache_refresh = CacheRefresh::AliyunCdn(test_config());

        let task_ids = submit_cache_refresh(&command, 5, &cache_refresh)
            .await
            .unwrap();
        wait_for_cache_refresh(&command, 5, &cache_refresh, &task_ids)
            .await
            .unwrap();

        assert_eq!(task_ids, ["123", "456"]);
    }

    fn test_config() -> AliyunCdnCacheRefresh {
        AliyunCdnCacheRefresh {
            profile: "orhan-blog-cdn".to_owned(),
            object_path: "https://static.orhan.cn/".to_owned(),
            object_type: AliyunCdnObjectType::Directory,
            force: true,
            attempts: 3,
            retry_delay_ms: 1,
            poll_interval_ms: 1,
            timeout_ms: 1000,
        }
    }
}
