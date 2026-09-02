use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

use crate::error::{AppError, file_error};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GlobalConfig {
    pub listen_address: SocketAddr,
    pub database_path: PathBuf,
    pub state_directory: PathBuf,
    pub projects_directory: PathBuf,
    pub git_command: PathBuf,
    pub cargo_command: PathBuf,
    pub aynur_command: PathBuf,
    pub git_fetch_attempts: u32,
    pub git_fetch_retry_delay_ms: u64,
    pub command_timeout_seconds: u64,
    pub max_webhook_body_bytes: usize,
    pub worker_poll_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectConfig {
    project_id: String,
    repository_full_name: String,
    repository_url: String,
    webhook_token: String,
    tag_pattern: String,
    retain_releases: usize,
    health_check: HealthCheckConfig,
    deployment: DeploymentConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HealthCheckConfig {
    url: String,
    attempts: u32,
    interval_ms: u64,
    timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum DeploymentConfig {
    Static {
        entry_file: PathBuf,
    },
    RustAynur {
        cargo_manifest: PathBuf,
        package: String,
        binary: String,
        aynur_app: String,
        aynur_home: PathBuf,
        environment_file: PathBuf,
    },
}

#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Clone)]
pub struct Project {
    pub project_id: String,
    pub repository_full_name: String,
    pub repository_url: String,
    pub webhook_token: SecretString,
    pub tag_pattern: Regex,
    pub retain_releases: usize,
    pub health_check: HealthCheck,
    pub deployment: DeploymentTarget,
}

#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub url: reqwest::Url,
    pub attempts: u32,
    pub interval_ms: u64,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub enum DeploymentTarget {
    Static {
        entry_file: PathBuf,
    },
    RustAynur {
        cargo_manifest: PathBuf,
        package: String,
        binary: String,
        aynur_app: String,
        aynur_home: PathBuf,
        environment_file: PathBuf,
    },
}

#[derive(Clone)]
pub struct LoadedConfig {
    pub global: GlobalConfig,
    pub projects: HashMap<String, Project>,
}

impl LoadedConfig {
    pub fn project(&self, project_id: &str) -> Result<&Project, AppError> {
        self.projects
            .get(project_id)
            .ok_or_else(|| AppError::ProjectNotFound {
                project_id: project_id.to_owned(),
            })
    }
}

pub fn load_config(path: &Path) -> Result<LoadedConfig, AppError> {
    let global_text = fs::read_to_string(path)
        .map_err(|source| file_error("read", path.to_path_buf(), source))?;
    let global: GlobalConfig = toml::from_str(&global_text).map_err(|source| AppError::Config {
        path: path.to_path_buf(),
        reason: source.to_string(),
    })?;
    validate_global(path, &global)?;

    let mut project_paths = fs::read_dir(&global.projects_directory)
        .map_err(|source| file_error("read directory", global.projects_directory.clone(), source))?
        .map(|entry| {
            entry.map(|value| value.path()).map_err(|source| {
                file_error(
                    "read directory entry",
                    global.projects_directory.clone(),
                    source,
                )
            })
        })
        .collect::<Result<Vec<PathBuf>, AppError>>()?;
    project_paths.retain(|project_path| {
        project_path.extension().and_then(|value| value.to_str()) == Some("toml")
    });
    project_paths.sort();
    if project_paths.is_empty() {
        return Err(AppError::Config {
            path: global.projects_directory.clone(),
            reason: "projectsDirectory must contain at least one .toml project file".to_owned(),
        });
    }

    let mut projects = HashMap::with_capacity(project_paths.len());
    for project_path in project_paths {
        let project = load_project(&project_path)?;
        let project_id = project.project_id.clone();
        if projects.insert(project_id.clone(), project).is_some() {
            return Err(AppError::Config {
                path: project_path,
                reason: format!("duplicate projectId {project_id:?}"),
            });
        }
    }

    if projects
        .values()
        .any(|project| matches!(project.deployment, DeploymentTarget::RustAynur { .. }))
    {
        validate_executable(path, "cargoCommand", &global.cargo_command)?;
        validate_executable(path, "aynurCommand", &global.aynur_command)?;
    }

    Ok(LoadedConfig { global, projects })
}

fn validate_global(path: &Path, config: &GlobalConfig) -> Result<(), AppError> {
    if !config.listen_address.ip().is_loopback() {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!(
                "listenAddress must use a loopback IP, got {}",
                config.listen_address
            ),
        });
    }
    validate_absolute_path(path, "databasePath", &config.database_path)?;
    validate_absolute_path(path, "stateDirectory", &config.state_directory)?;
    validate_absolute_path(path, "projectsDirectory", &config.projects_directory)?;
    validate_executable(path, "gitCommand", &config.git_command)?;
    validate_absolute_path(path, "cargoCommand", &config.cargo_command)?;
    validate_absolute_path(path, "aynurCommand", &config.aynur_command)?;
    validate_positive(
        path,
        "gitFetchAttempts",
        u64::from(config.git_fetch_attempts),
    )?;
    validate_positive(
        path,
        "gitFetchRetryDelayMs",
        config.git_fetch_retry_delay_ms,
    )?;
    validate_positive(
        path,
        "commandTimeoutSeconds",
        config.command_timeout_seconds,
    )?;
    validate_positive(
        path,
        "maxWebhookBodyBytes",
        u64::try_from(config.max_webhook_body_bytes).map_err(|source| AppError::Config {
            path: path.to_path_buf(),
            reason: format!("maxWebhookBodyBytes is too large: {source}"),
        })?,
    )?;
    validate_positive(path, "workerPollIntervalMs", config.worker_poll_interval_ms)?;
    Ok(())
}

fn load_project(path: &Path) -> Result<Project, AppError> {
    validate_private_file(path, "projectConfig", path)?;
    let text = fs::read_to_string(path)
        .map_err(|source| file_error("read", path.to_path_buf(), source))?;
    let config: ProjectConfig = toml::from_str(&text).map_err(|source| AppError::Config {
        path: path.to_path_buf(),
        reason: source.to_string(),
    })?;
    let project_id_pattern =
        Regex::new(r"^[a-z0-9][a-z0-9-]{0,63}$").map_err(|source| AppError::Config {
            path: path.to_path_buf(),
            reason: format!("internal project ID regular expression is invalid: {source}"),
        })?;
    if !project_id_pattern.is_match(&config.project_id) {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!("projectId {:?} is invalid", config.project_id),
        });
    }
    validate_repository_full_name(path, &config.repository_full_name)?;
    if config.repository_url.is_empty() || config.repository_url.chars().any(char::is_control) {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: "repositoryUrl must be non-empty and contain no control characters".to_owned(),
        });
    }
    if config.webhook_token.is_empty() || config.webhook_token.chars().any(char::is_control) {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: "webhookToken must be non-empty and contain no control characters".to_owned(),
        });
    }
    if !config.tag_pattern.starts_with('^') || !config.tag_pattern.ends_with('$') {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: "tagPattern must be anchored with ^ and $".to_owned(),
        });
    }
    let tag_pattern = Regex::new(&config.tag_pattern).map_err(|source| AppError::Config {
        path: path.to_path_buf(),
        reason: format!("tagPattern {:?} is invalid: {source}", config.tag_pattern),
    })?;
    if config.retain_releases == 0 {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: "retainReleases must be greater than zero".to_owned(),
        });
    }
    let health_check = load_health_check(path, config.health_check)?;
    let deployment = load_deployment(path, config.deployment)?;

    Ok(Project {
        project_id: config.project_id,
        repository_full_name: config.repository_full_name,
        repository_url: config.repository_url,
        webhook_token: SecretString(config.webhook_token),
        tag_pattern,
        retain_releases: config.retain_releases,
        health_check,
        deployment,
    })
}

fn load_health_check(path: &Path, config: HealthCheckConfig) -> Result<HealthCheck, AppError> {
    let url = reqwest::Url::parse(&config.url).map_err(|source| AppError::Config {
        path: path.to_path_buf(),
        reason: format!("healthCheck.url {:?} is invalid: {source}", config.url),
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!(
                "healthCheck.url must use http or https, got {:?}",
                config.url
            ),
        });
    }
    validate_positive(path, "healthCheck.attempts", u64::from(config.attempts))?;
    validate_positive(path, "healthCheck.intervalMs", config.interval_ms)?;
    validate_positive(path, "healthCheck.timeoutMs", config.timeout_ms)?;
    Ok(HealthCheck {
        url,
        attempts: config.attempts,
        interval_ms: config.interval_ms,
        timeout_ms: config.timeout_ms,
    })
}

fn load_deployment(path: &Path, config: DeploymentConfig) -> Result<DeploymentTarget, AppError> {
    match config {
        DeploymentConfig::Static { entry_file } => {
            validate_relative_path(path, "deployment.entryFile", &entry_file)?;
            Ok(DeploymentTarget::Static { entry_file })
        }
        DeploymentConfig::RustAynur {
            cargo_manifest,
            package,
            binary,
            aynur_app,
            aynur_home,
            environment_file,
        } => {
            validate_relative_path(path, "deployment.cargoManifest", &cargo_manifest)?;
            validate_name(path, "deployment.package", &package)?;
            validate_name(path, "deployment.binary", &binary)?;
            validate_name(path, "deployment.aynurApp", &aynur_app)?;
            validate_absolute_path(path, "deployment.aynurHome", &aynur_home)?;
            validate_absolute_path(path, "deployment.environmentFile", &environment_file)?;
            validate_private_file(path, "deployment.environmentFile", &environment_file)?;
            Ok(DeploymentTarget::RustAynur {
                cargo_manifest,
                package,
                binary,
                aynur_app,
                aynur_home,
                environment_file,
            })
        }
    }
}

fn validate_repository_full_name(path: &Path, value: &str) -> Result<(), AppError> {
    let pattern =
        Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").map_err(|source| AppError::Config {
            path: path.to_path_buf(),
            reason: format!("internal repository regular expression is invalid: {source}"),
        })?;
    if !pattern.is_match(value) {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!("repositoryFullName {value:?} is invalid"),
        });
    }
    Ok(())
}

fn validate_name(path: &Path, field: &str, value: &str) -> Result<(), AppError> {
    let pattern =
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$").map_err(|source| AppError::Config {
            path: path.to_path_buf(),
            reason: format!("internal name regular expression is invalid: {source}"),
        })?;
    if !pattern.is_match(value) {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!("{field} {value:?} is invalid"),
        });
    }
    Ok(())
}

fn validate_absolute_path(path: &Path, field: &str, value: &Path) -> Result<(), AppError> {
    if !value.is_absolute() {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!("{field} must be an absolute path, got {value:?}"),
        });
    }
    Ok(())
}

fn validate_relative_path(path: &Path, field: &str, value: &Path) -> Result<(), AppError> {
    if value.as_os_str().is_empty()
        || value.is_absolute()
        || value
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!(
                "{field} must be a non-empty relative path without . or .., got {value:?}"
            ),
        });
    }
    Ok(())
}

fn validate_executable(path: &Path, field: &str, value: &Path) -> Result<(), AppError> {
    validate_absolute_path(path, field, value)?;
    let metadata = fs::metadata(value)
        .map_err(|source| file_error("read metadata", value.to_path_buf(), source))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!("{field} {value:?} must be an executable file"),
        });
    }
    Ok(())
}

fn validate_private_file(config_path: &Path, field: &str, path: &Path) -> Result<(), AppError> {
    let metadata = fs::metadata(path)
        .map_err(|source| file_error("read metadata", path.to_path_buf(), source))?;
    if !metadata.is_file() {
        return Err(AppError::Config {
            path: config_path.to_path_buf(),
            reason: format!("{field} {path:?} must be a regular file"),
        });
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(AppError::Config {
            path: config_path.to_path_buf(),
            reason: format!("{field} {path:?} must have mode 0600, got {mode:04o}"),
        });
    }
    Ok(())
}

fn validate_positive(path: &Path, field: &str, value: u64) -> Result<(), AppError> {
    if value == 0 {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!("{field} must be greater than zero"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::DeploymentConfig;

    #[test]
    fn deployment_rejects_unknown_fields() {
        let result = toml::from_str::<DeploymentConfig>(
            r#"
type = "static"
entryFile = "index.html"
binary = "unexpected"
"#,
        );
        assert!(
            result.is_err(),
            "unknown deployment fields must be rejected"
        );
    }
}
