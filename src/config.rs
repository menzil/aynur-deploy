use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
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
    current_path: PathBuf,
    repository_full_name: String,
    repository_url: String,
    webhook_token: String,
    tag_pattern: String,
    retain_releases: usize,
    health_check: HealthCheckConfig,
    deployment: DeploymentConfig,
    migration: Option<MigrationConfig>,
    reload: Option<ReloadConfig>,
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
    Binary {
        binary_path: PathBuf,
    },
    Rust {
        cargo_manifest: PathBuf,
        binaries: Vec<RustBinaryConfig>,
        include_paths: Vec<PathBuf>,
        environment_file: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RustBinaryConfig {
    package: String,
    binary: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MigrationConfig {
    command: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReloadConfig {
    commands: Vec<Vec<String>>,
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
    pub config_path: PathBuf,
    pub project_id: String,
    pub current_path: PathBuf,
    pub repository_full_name: String,
    pub repository_url: String,
    pub webhook_token: SecretString,
    pub tag_pattern: Regex,
    pub retain_releases: usize,
    pub health_check: HealthCheck,
    pub deployment: DeploymentTarget,
    pub migration: Option<ConfiguredCommand>,
    pub reload: Vec<ConfiguredCommand>,
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
    Binary {
        binary_path: PathBuf,
    },
    Rust {
        cargo_manifest: PathBuf,
        binaries: Vec<RustBinary>,
        include_paths: Vec<PathBuf>,
        environment_file: Option<PathBuf>,
    },
}

#[derive(Debug, Clone)]
pub struct RustBinary {
    pub package: String,
    pub binary: String,
}

#[derive(Debug, Clone)]
pub struct ConfiguredCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
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
    let mut projects: HashMap<String, Project> = HashMap::with_capacity(project_paths.len());
    for project_path in project_paths {
        let project = load_project(&project_path)?;
        let project_id = project.project_id.clone();
        if let Some(existing) = projects
            .values()
            .find(|existing| existing.current_path == project.current_path)
        {
            return Err(AppError::Config {
                path: project_path,
                reason: format!(
                    "currentPath {:?} is already used by project {:?}",
                    project.current_path, existing.project_id
                ),
            });
        }
        if projects.insert(project_id.clone(), project).is_some() {
            return Err(AppError::Config {
                path: project_path,
                reason: format!("duplicate projectId {project_id:?}"),
            });
        }
    }

    if projects
        .values()
        .any(|project| matches!(project.deployment, DeploymentTarget::Rust { .. }))
    {
        validate_executable(path, "cargoCommand", &global.cargo_command)?;
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
    validate_current_path(path, &config.current_path)?;
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
    let migration = config
        .migration
        .map(|migration| load_command(path, "migration.command", migration.command))
        .transpose()?;
    let reload = load_reload(path, config.reload)?;

    Ok(Project {
        config_path: path.to_path_buf(),
        project_id: config.project_id,
        current_path: config.current_path,
        repository_full_name: config.repository_full_name,
        repository_url: config.repository_url,
        webhook_token: SecretString(config.webhook_token),
        tag_pattern,
        retain_releases: config.retain_releases,
        health_check,
        deployment,
        migration,
        reload,
    })
}

fn validate_current_path(path: &Path, value: &Path) -> Result<(), AppError> {
    validate_absolute_path(path, "currentPath", value)?;
    if value == Path::new("/")
        || value
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!(
                "currentPath must be a normalized absolute path other than /, got {value:?}"
            ),
        });
    }
    match fs::symlink_metadata(value) {
        Ok(metadata) if !metadata.file_type().is_symlink() => Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!("currentPath {value:?} must be absent or a symbolic link"),
        }),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(file_error(
            "read symlink metadata",
            value.to_path_buf(),
            source,
        )),
    }
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
        DeploymentConfig::Binary { binary_path } => {
            validate_relative_path(path, "deployment.binaryPath", &binary_path)?;
            Ok(DeploymentTarget::Binary { binary_path })
        }
        DeploymentConfig::Rust {
            cargo_manifest,
            binaries,
            include_paths,
            environment_file,
        } => {
            validate_relative_path(path, "deployment.cargoManifest", &cargo_manifest)?;
            if binaries.is_empty() {
                return Err(AppError::Config {
                    path: path.to_path_buf(),
                    reason: "deployment.binaries must contain at least one binary".to_owned(),
                });
            }
            let mut binary_names: HashSet<String> = HashSet::with_capacity(binaries.len());
            let binaries = binaries
                .into_iter()
                .enumerate()
                .map(|(index, binary)| {
                    validate_name(
                        path,
                        &format!("deployment.binaries[{index}].package"),
                        &binary.package,
                    )?;
                    validate_name(
                        path,
                        &format!("deployment.binaries[{index}].binary"),
                        &binary.binary,
                    )?;
                    if !binary_names.insert(binary.binary.clone()) {
                        return Err(AppError::Config {
                            path: path.to_path_buf(),
                            reason: format!(
                                "deployment.binaries contains duplicate binary {:?}",
                                binary.binary
                            ),
                        });
                    }
                    Ok(RustBinary {
                        package: binary.package,
                        binary: binary.binary,
                    })
                })
                .collect::<Result<Vec<RustBinary>, AppError>>()?;
            validate_include_paths(path, &include_paths, &binary_names)?;
            if let Some(environment_file) = &environment_file {
                validate_absolute_path(path, "deployment.environmentFile", environment_file)?;
                load_environment_file(environment_file)?;
            }
            Ok(DeploymentTarget::Rust {
                cargo_manifest,
                binaries,
                include_paths,
                environment_file,
            })
        }
    }
}

fn validate_include_paths(
    path: &Path,
    include_paths: &[PathBuf],
    binary_names: &HashSet<String>,
) -> Result<(), AppError> {
    for (index, include_path) in include_paths.iter().enumerate() {
        validate_relative_path(
            path,
            &format!("deployment.includePaths[{index}]"),
            include_path,
        )?;
        if binary_names
            .iter()
            .any(|binary| include_path == Path::new(binary))
        {
            return Err(AppError::Config {
                path: path.to_path_buf(),
                reason: format!(
                    "deployment.includePaths entry {include_path:?} conflicts with a binary destination"
                ),
            });
        }
        if include_paths[..index].iter().any(|existing| {
            include_path.starts_with(existing) || existing.starts_with(include_path)
        }) {
            return Err(AppError::Config {
                path: path.to_path_buf(),
                reason: format!(
                    "deployment.includePaths entry {include_path:?} overlaps another included path"
                ),
            });
        }
    }
    Ok(())
}

fn load_reload(
    path: &Path,
    config: Option<ReloadConfig>,
) -> Result<Vec<ConfiguredCommand>, AppError> {
    let Some(config) = config else {
        return Ok(Vec::new());
    };
    if config.commands.is_empty() {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: "reload.commands must contain at least one command".to_owned(),
        });
    }
    config
        .commands
        .into_iter()
        .enumerate()
        .map(|(index, command)| load_command(path, &format!("reload.commands[{index}]"), command))
        .collect()
}

fn load_command(
    path: &Path,
    field: &str,
    command: Vec<String>,
) -> Result<ConfiguredCommand, AppError> {
    if command.is_empty() {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!("{field} must contain at least one argument"),
        });
    }
    if command
        .iter()
        .any(|value| value.is_empty() || value.chars().any(char::is_control))
    {
        return Err(AppError::Config {
            path: path.to_path_buf(),
            reason: format!(
                "{field} arguments must be non-empty and contain no control characters"
            ),
        });
    }
    let mut command = command.into_iter();
    let program = PathBuf::from(command.next().expect("command is non-empty"));
    let args = command.map(OsString::from).collect();
    Ok(ConfiguredCommand { program, args })
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

pub fn load_environment_file(path: &Path) -> Result<Vec<(OsString, OsString)>, AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| AppError::EnvironmentFile {
        path: path.to_path_buf(),
        reason: format!("could not read metadata: {source}"),
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if !metadata.is_file() || mode != 0o600 {
        return Err(AppError::EnvironmentFile {
            path: path.to_path_buf(),
            reason: format!("must be a regular file with mode 0600, got mode {mode:04o}"),
        });
    }
    let entries =
        dotenvy::from_path_iter(path).map_err(|source| environment_file_error(path, source))?;
    let mut names: HashSet<String> = HashSet::new();
    let mut environment: Vec<(OsString, OsString)> = Vec::new();
    for entry in entries {
        let (name, value) = entry.map_err(|source| environment_file_error(path, source))?;
        if !names.insert(name.clone()) {
            return Err(AppError::EnvironmentFile {
                path: path.to_path_buf(),
                reason: format!("variable {name:?} is defined more than once"),
            });
        }
        environment.push((OsString::from(name), OsString::from(value)));
    }
    Ok(environment)
}

fn environment_file_error(path: &Path, source: dotenvy::Error) -> AppError {
    let reason = match source {
        dotenvy::Error::LineParse(_, index) => {
            format!("syntax error at byte index {index}; the line value was redacted")
        }
        dotenvy::Error::Io(source) => format!("could not be read: {source}"),
        dotenvy::Error::EnvVar(source) => {
            format!("contains an invalid variable reference: {source}")
        }
        _ => "could not be parsed; the source value was redacted".to_owned(),
    };
    AppError::EnvironmentFile {
        path: path.to_path_buf(),
        reason,
    }
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::{
        DeploymentConfig, DeploymentTarget, ReloadConfig, load_deployment, load_environment_file,
    };

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

    #[test]
    fn deployment_types_keep_binary_and_rust_fields_distinct() {
        let binary = toml::from_str::<DeploymentConfig>(
            r#"
type = "binary"
binaryPath = "bin/service"
"#,
        )
        .expect("binary deployment must parse");
        assert!(matches!(
            load_deployment(Path::new("project.toml"), binary),
            Ok(DeploymentTarget::Binary { .. })
        ));

        let rust = toml::from_str::<DeploymentConfig>(
            r#"
type = "rust"
cargoManifest = "Cargo.toml"
includePaths = ["config"]
binaries = [
    { package = "service", binary = "service" },
    { package = "migrator", binary = "migrator" },
]
"#,
        )
        .expect("rust deployment must parse");
        assert!(matches!(
            load_deployment(Path::new("project.toml"), rust),
            Ok(DeploymentTarget::Rust { .. })
        ));
    }

    #[test]
    fn rust_deployment_rejects_v03_fields() {
        let result = toml::from_str::<DeploymentConfig>(
            r#"
type = "rust"
cargoManifest = "Cargo.toml"
package = "service"
binary = "service"
"#,
        );
        assert!(result.is_err(), "v0.3 Rust fields must be rejected");
    }

    #[test]
    fn rust_deployment_rejects_duplicate_binary_names() {
        let config = toml::from_str::<DeploymentConfig>(
            r#"
type = "rust"
cargoManifest = "Cargo.toml"
includePaths = []
binaries = [
    { package = "service", binary = "server" },
    { package = "admin", binary = "server" },
]
"#,
        )
        .expect("Rust deployment config must parse before semantic validation");
        let error = load_deployment(Path::new("project.toml"), config)
            .expect_err("duplicate binary destinations must fail");
        assert!(error.to_string().contains("duplicate binary \"server\""));
    }

    #[test]
    fn rust_deployment_rejects_empty_binaries_and_overlapping_include_paths() {
        let empty = toml::from_str::<DeploymentConfig>(
            r#"
type = "rust"
cargoManifest = "Cargo.toml"
includePaths = []
binaries = []
"#,
        )
        .unwrap();
        assert!(
            load_deployment(Path::new("project.toml"), empty)
                .unwrap_err()
                .to_string()
                .contains("must contain at least one binary")
        );

        let overlapping = toml::from_str::<DeploymentConfig>(
            r#"
type = "rust"
cargoManifest = "Cargo.toml"
includePaths = ["profiles", "profiles/production.toml"]
binaries = [{ package = "service", binary = "service" }]
"#,
        )
        .unwrap();
        assert!(
            load_deployment(Path::new("project.toml"), overlapping)
                .unwrap_err()
                .to_string()
                .contains("overlaps another included path")
        );
    }

    #[test]
    fn reload_rejects_v03_command_field() {
        let result = toml::from_str::<ReloadConfig>(r#"command = ["aynur", "reload", "service"]"#);
        assert!(result.is_err(), "v0.3 reload.command must be rejected");
    }

    #[test]
    fn environment_file_rejects_duplicate_variables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.env");
        fs::write(&path, "APP_PROFILE=one\nAPP_PROFILE=two\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let error = load_environment_file(&path).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("variable \"APP_PROFILE\" is defined more than once")
        );
    }

    #[test]
    fn environment_file_parse_errors_redact_source_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.env");
        fs::write(&path, "SECRET='super-secret\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let error = load_environment_file(&path).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("value was redacted"));
        assert!(!message.contains("super-secret"));
    }
}
