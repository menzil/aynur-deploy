use std::env;
use std::fs::{self, OpenOptions};
use std::future::IntoFuture;
use std::io::{self, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use tokio::sync::{Notify, watch};
use uuid::Uuid;

use crate::config::{LoadedConfig, load_config};
use crate::db::Database;
use crate::deploy::{Deployer, run_worker};
use crate::error::{AppError, file_error};
use crate::model::{CleanDeploymentType, Deployment, ProjectState, ProjectStatus};
use crate::web::{AppState, router};

#[derive(Debug, Parser)]
#[command(
    name = "aynur-deploy",
    version,
    about = "Strict Gitee Tag deployment service"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Initialize the service configuration directory")]
    Init(InitArgs),
    #[command(about = "Check global and project configuration")]
    Check,
    #[command(about = "Add a project and generate its WebHook password")]
    Add(AddArgs),
    #[command(about = "List configured deployment projects")]
    List,
    #[command(about = "Show deployment status for a project")]
    Status(ProjectArgs),
    #[command(about = "Stop accepting and processing deployments for a project")]
    Stop(ProjectArgs),
    #[command(about = "Resume accepting and processing deployments for a project")]
    Start(ProjectArgs),
    #[command(about = "Delete a stopped deployment project while preserving its releases")]
    Delete(ProjectArgs),
    #[command(about = "Clean completed deployment history for a project")]
    Clean(CleanArgs),
    #[command(about = "Retry a failed deployment")]
    Retry(RetryArgs),
    #[command(about = "Roll back to a successful release")]
    Rollback(RollbackArgs),
    #[command(about = "Unlock a blocked project")]
    Unblock(ProjectArgs),
    #[command(about = "Start the HTTP deployment service")]
    Serve,
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(long)]
    home: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct AddArgs {
    project_id: String,
    #[arg(long = "type", value_enum, default_value_t = DeploymentType::Static)]
    deployment_type: DeploymentType,
    #[arg(long)]
    current_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum DeploymentType {
    Static,
    Binary,
    Rust,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum CleanType {
    Failed,
    Succeeded,
    All,
}

impl From<CleanType> for CleanDeploymentType {
    fn from(value: CleanType) -> Self {
        match value {
            CleanType::Failed => Self::Failed,
            CleanType::Succeeded => Self::Succeeded,
            CleanType::All => Self::All,
        }
    }
}

#[derive(Debug, Args)]
struct CleanArgs {
    project_id: String,
    #[arg(long)]
    keep: u64,
    #[arg(long = "type", value_enum)]
    deployment_type: CleanType,
}

#[derive(Debug, Args)]
struct ProjectArgs {
    project_id: String,
}

#[derive(Debug, Args)]
struct RetryArgs {
    deployment_id: String,
}

#[derive(Debug, Args)]
struct RollbackArgs {
    project_id: String,
    commit_sha: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitOutput {
    ok: bool,
    config_path: PathBuf,
    projects_directory: PathBuf,
    already_initialized: bool,
    message: Option<&'static str>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AddOutput {
    ok: bool,
    project_id: String,
    project_config_path: PathBuf,
    current_path: PathBuf,
    bootstrap_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListProjectsOutput {
    ok: bool,
    projects: Vec<ListedProject>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListedProject {
    project_id: String,
    repository_full_name: String,
    current_path: PathBuf,
    status: ProjectStatus,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckConfigOutput {
    ok: bool,
    config_path: PathBuf,
    project_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusOutput {
    ok: bool,
    project: ProjectState,
    deployments: Vec<Deployment>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeploymentOutput {
    ok: bool,
    deployment: Deployment,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectOutput {
    ok: bool,
    project: ProjectState,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteProjectOutput {
    ok: bool,
    project_id: String,
    project_config_path: PathBuf,
    current_path: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CleanOutput {
    ok: bool,
    project_id: String,
    deployment_type: String,
    keep: u64,
    removed_deployments: usize,
    removed_worktrees: usize,
    removed_targets: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServeOutput {
    ok: bool,
    status: &'static str,
    listen_address: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CliErrorOutput {
    ok: bool,
    error: CliErrorBody,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CliErrorBody {
    code: String,
    message: String,
}

pub async fn main_entry() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(value) => value,
        Err(source)
            if matches!(
                source.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            return match emit_text_stdout(&source.to_string()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(source) => {
                    let output = CliErrorOutput {
                        ok: false,
                        error: CliErrorBody {
                            code: source.code().to_owned(),
                            message: source.to_string(),
                        },
                    };
                    emit_stderr(&output);
                    ExitCode::FAILURE
                }
            };
        }
        Err(source) => {
            let output = CliErrorOutput {
                ok: false,
                error: CliErrorBody {
                    code: "cliArgumentInvalid".to_owned(),
                    message: source.to_string(),
                },
            };
            emit_stderr(&output);
            return ExitCode::FAILURE;
        }
    };
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(source) => {
            let output = CliErrorOutput {
                ok: false,
                error: CliErrorBody {
                    code: source.code().to_owned(),
                    message: source.to_string(),
                },
            };
            emit_stderr(&output);
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), AppError> {
    match cli.command {
        Command::Init(args) => init_home(args.home.as_deref()),
        Command::Check => check_config(&resolve_config_path()?),
        Command::Add(args) => add_project(
            &args.project_id,
            args.deployment_type,
            args.current_path.as_deref(),
        ),
        Command::List => list_projects(&resolve_config_path()?).await,
        Command::Status(args) => status(&resolve_config_path()?, &args.project_id).await,
        Command::Stop(args) => stop(&resolve_config_path()?, &args.project_id).await,
        Command::Start(args) => start(&resolve_config_path()?, &args.project_id).await,
        Command::Delete(args) => delete(&resolve_config_path()?, &args.project_id).await,
        Command::Clean(args) => clean(&resolve_config_path()?, &args).await,
        Command::Retry(args) => retry(&resolve_config_path()?, &args.deployment_id).await,
        Command::Rollback(args) => {
            rollback(&resolve_config_path()?, &args.project_id, &args.commit_sha).await
        }
        Command::Unblock(args) => unblock(&resolve_config_path()?, &args.project_id).await,
        Command::Serve => serve(&resolve_config_path()?).await,
    }
}

fn init_home(requested_home: Option<&Path>) -> Result<(), AppError> {
    let home = match requested_home {
        Some(value) => value.to_path_buf(),
        None => match find_config_path()? {
            Some(config_path) => config_path
                .parent()
                .ok_or_else(|| AppError::InvalidState {
                    reason: format!("configuration path {config_path:?} has no parent"),
                })?
                .to_path_buf(),
            None => PathBuf::from("/etc/aynur-deploy"),
        },
    };
    let config_path = home.join("config.toml");
    let already_initialized = config_path.exists();
    let projects_directory = if config_path.exists() {
        let text = fs::read_to_string(&config_path)
            .map_err(|source| file_error("read", config_path.clone(), source))?;
        let config: crate::config::GlobalConfig =
            toml::from_str(&text).map_err(|source| AppError::Config {
                path: config_path.clone(),
                reason: source.to_string(),
            })?;
        config.projects_directory
    } else {
        home.join("projects")
    };
    fs::create_dir_all(&projects_directory)
        .map_err(|source| file_error("create directory", projects_directory.clone(), source))?;
    if !config_path.exists() {
        let config = format_global_config(&home);
        write_new_file(&config_path, &config, 0o600)?;
    }
    if requested_home.is_some() {
        remember_home(&home)?;
    }
    emit_json(&InitOutput {
        ok: true,
        config_path,
        projects_directory,
        already_initialized,
        message: already_initialized
            .then_some("configuration already initialized; existing configuration was kept"),
    })
}

fn add_project(
    project_id: &str,
    deployment_type: DeploymentType,
    requested_current_path: Option<&Path>,
) -> Result<(), AppError> {
    validate_project_id(project_id)?;
    let config_path = resolve_config_path()?;
    let text = fs::read_to_string(&config_path)
        .map_err(|source| file_error("read", config_path.clone(), source))?;
    let config: crate::config::GlobalConfig =
        toml::from_str(&text).map_err(|source| AppError::Config {
            path: config_path,
            reason: source.to_string(),
        })?;
    let current_path = requested_current_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            config
                .state_directory
                .join("projects")
                .join(project_id)
                .join("current")
        });
    validate_cli_current_path(&current_path)?;
    let projects_directory = config.projects_directory;
    fs::create_dir_all(&projects_directory)
        .map_err(|source| file_error("create directory", projects_directory.clone(), source))?;
    let project_config_path = projects_directory.join(format!("{project_id}.toml"));
    let token = format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple());
    let project_config = format_project_config(project_id, &current_path, &token, deployment_type)?;
    write_new_file(&project_config_path, &project_config, 0o600)?;
    let bootstrap_path = match requested_current_path {
        Some(_) => match adopt_existing_current_path(&current_path) {
            Ok(value) => value,
            Err(source) => {
                return Err(remove_failed_project_config(&project_config_path, source));
            }
        },
        None => None,
    };
    emit_json(&AddOutput {
        ok: true,
        project_id: project_id.to_owned(),
        project_config_path,
        current_path,
        bootstrap_path,
    })
}

fn adopt_existing_current_path(current_path: &Path) -> Result<Option<PathBuf>, AppError> {
    let metadata = match fs::symlink_metadata(current_path) {
        Ok(value) => value,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(file_error(
                "read symlink metadata",
                current_path.to_path_buf(),
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(None);
    }
    if !metadata.is_dir() {
        return Err(AppError::RequestValidation {
            reason: format!(
                "currentPath {current_path:?} must be absent, a symbolic link, or an existing directory"
            ),
        });
    }

    let bootstrap_path = bootstrap_path_for(current_path)?;
    match fs::symlink_metadata(&bootstrap_path) {
        Ok(_) => {
            return Err(AppError::RequestValidation {
                reason: format!(
                    "cannot adopt currentPath {current_path:?}: bootstrap path {bootstrap_path:?} already exists"
                ),
            });
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(file_error("read symlink metadata", bootstrap_path, source));
        }
    }

    fs::rename(current_path, &bootstrap_path)
        .map_err(|source| file_error("rename", current_path.to_path_buf(), source))?;
    if let Err(source) = symlink(&bootstrap_path, current_path) {
        let adoption_error = file_error("create symlink", current_path.to_path_buf(), source);
        return match fs::rename(&bootstrap_path, current_path) {
            Ok(()) => Err(adoption_error),
            Err(rollback_error) => Err(AppError::InvalidState {
                reason: format!(
                    "failed to adopt currentPath with {adoption_error}; restoring {bootstrap_path:?} to {current_path:?} also failed: {rollback_error}"
                ),
            }),
        };
    }
    Ok(Some(bootstrap_path))
}

fn bootstrap_path_for(current_path: &Path) -> Result<PathBuf, AppError> {
    let file_name = current_path
        .file_name()
        .ok_or_else(|| AppError::RequestValidation {
            reason: format!("currentPath {current_path:?} has no final path component"),
        })?;
    let mut bootstrap_name = file_name.to_os_string();
    bootstrap_name.push(".before-aynur-deploy");
    Ok(current_path.with_file_name(bootstrap_name))
}

fn remove_failed_project_config(project_config_path: &Path, source: AppError) -> AppError {
    match fs::remove_file(project_config_path) {
        Ok(()) => source,
        Err(cleanup_error) => AppError::InvalidState {
            reason: format!(
                "project setup failed with {source}; removing generated configuration {project_config_path:?} also failed: {cleanup_error}"
            ),
        },
    }
}

async fn list_projects(config_path: &Path) -> Result<(), AppError> {
    let (config, database) = load_database(config_path).await?;
    let mut projects: Vec<ListedProject> = Vec::with_capacity(config.projects.len());
    for project in config.projects.values() {
        let state = database.project_state(&project.project_id).await?;
        projects.push(ListedProject {
            project_id: project.project_id.clone(),
            repository_full_name: project.repository_full_name.clone(),
            current_path: project.current_path.clone(),
            status: state.status,
        });
    }
    projects.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    emit_json(&ListProjectsOutput { ok: true, projects })
}

fn resolve_config_path() -> Result<PathBuf, AppError> {
    find_config_path()?.ok_or_else(|| AppError::Config {
        path: PathBuf::from("/etc/aynur-deploy/config.toml"),
        reason: "configuration is not initialized; run `aynur-deploy init` first".to_owned(),
    })
}

fn find_config_path() -> Result<Option<PathBuf>, AppError> {
    let default_config = Path::new("/etc/aynur-deploy/config.toml");
    let config_home = user_config_home()?;
    let pointer = config_home.join("active-home");
    if pointer.exists() {
        let home = fs::read_to_string(&pointer)
            .map_err(|source| file_error("read", pointer.clone(), source))?;
        let home = PathBuf::from(home.trim());
        let config = home.join("config.toml");
        if config.exists() {
            return Ok(Some(config));
        }
        return Err(AppError::Config {
            path: pointer,
            reason: format!("active home {home:?} does not contain config.toml"),
        });
    }
    let user_config = config_home.join("config.toml");
    if user_config.exists() {
        return Ok(Some(user_config));
    }
    if default_config.exists() {
        return Ok(Some(default_config.to_path_buf()));
    }
    Ok(None)
}

fn user_config_home() -> Result<PathBuf, AppError> {
    if let Some(value) = env::var_os("XDG_CONFIG_HOME")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value).join("aynur-deploy"));
    }
    let home = env::var_os("HOME").ok_or_else(|| AppError::Config {
        path: PathBuf::from("$HOME/.config/aynur-deploy"),
        reason: "HOME is not set; set XDG_CONFIG_HOME or run `aynur-deploy init --home <path>`"
            .to_owned(),
    })?;
    Ok(PathBuf::from(home).join(".config/aynur-deploy"))
}

fn remember_home(home: &Path) -> Result<(), AppError> {
    let config_home = user_config_home()?;
    fs::create_dir_all(&config_home)
        .map_err(|source| file_error("create directory", config_home.clone(), source))?;
    let pointer = config_home.join("active-home");
    fs::write(&pointer, format!("{}\n", home.display()))
        .map_err(|source| file_error("write", pointer.clone(), source))?;
    fs::set_permissions(&pointer, fs::Permissions::from_mode(0o600))
        .map_err(|source| file_error("set permissions", pointer, source))
}

fn validate_project_id(project_id: &str) -> Result<(), AppError> {
    let pattern = regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,63}$").map_err(|source| {
        AppError::InvalidState {
            reason: format!("internal project ID regular expression is invalid: {source}"),
        }
    })?;
    if !pattern.is_match(project_id) {
        return Err(AppError::RequestValidation {
            reason: format!("projectId {project_id:?} is invalid"),
        });
    }
    Ok(())
}

fn validate_cli_current_path(current_path: &Path) -> Result<(), AppError> {
    if !current_path.is_absolute()
        || current_path == Path::new("/")
        || current_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(AppError::RequestValidation {
            reason: format!(
                "currentPath must be a normalized absolute path other than /, got {current_path:?}"
            ),
        });
    }
    Ok(())
}

fn format_global_config(home: &Path) -> String {
    let state_directory = if home == Path::new("/etc/aynur-deploy") {
        Path::new("/var/lib/aynur-deploy").to_path_buf()
    } else {
        home.join("state")
    };
    format!(
        "listenAddress = \"127.0.0.1:9091\"\ndatabasePath = \"{}\"\nstateDirectory = \"{}\"\nprojectsDirectory = \"{}\"\ngitCommand = \"/usr/bin/git\"\ncargoCommand = \"/usr/bin/cargo\"\ngitFetchAttempts = 3\ngitFetchRetryDelayMs = 2000\ncommandTimeoutSeconds = 1800\nmaxWebhookBodyBytes = 65536\nworkerPollIntervalMs = 1000\n",
        state_directory.join("deployments.sqlite3").display(),
        state_directory.display(),
        home.join("projects").display(),
    )
}

fn format_project_config(
    project_id: &str,
    current_path: &Path,
    token: &str,
    deployment_type: DeploymentType,
) -> Result<String, AppError> {
    let deployment = match deployment_type {
        DeploymentType::Static => {
            "type = \"static\"\nentryFile = \"index.html\"\n".to_owned()
        }
        DeploymentType::Binary => {
            "type = \"binary\"\nbinaryPath = \"bin/my-service\"\n\n# Optional fixed-argv commands after activation and rollback:\n# [reload]\n# commands = [[\"aynur\", \"reload\", \"my-service\", \"--update-env\"]]\n".to_owned()
        }
        DeploymentType::Rust => {
            "type = \"rust\"\ncargoManifest = \"Cargo.toml\"\nincludePaths = []\nbinaries = [{ package = \"my-service\", binary = \"my-service\" }]\n# environmentFile = \"/absolute/path/to/my-service.env\"\n\n# Optional fixed-argv migration command before activation:\n# [migration]\n# command = [\"./migrator\", \"migration\", \"apply\"]\n\n# Optional fixed-argv commands after activation and rollback:\n# [reload]\n# commands = [[\"aynur\", \"reload\", \"my-service\", \"--update-env\"]]\n".to_owned()
        }
    };
    let current_path = current_path
        .to_str()
        .ok_or_else(|| AppError::RequestValidation {
            reason: format!("currentPath {current_path:?} must be valid UTF-8"),
        })?;
    if current_path
        .chars()
        .any(|character| character == '"' || character == '\\' || character.is_control())
    {
        return Err(AppError::RequestValidation {
            reason: format!(
                "currentPath {current_path:?} must contain no quotes, backslashes, or control characters"
            ),
        });
    }
    Ok(format!(
        "projectId = \"{project_id}\"\ncurrentPath = \"{current_path}\"\nrepositoryFullName = \"owner/repository\"\nrepositoryUrl = \"git@gitee.com:owner/repository.git\"\nwebhookToken = \"{token}\"\ntagPattern = \"^deploy-[0-9]{{8}}-[0-9]{{6}}$\"\nretainReleases = 3\n\n[healthCheck]\n# Replace this with the deployed project's public URL.\nurl = \"https://example.invalid/\"\nattempts = 5\nintervalMs = 2000\ntimeoutMs = 5000\n\n[deployment]\n{deployment}"
    ))
}

fn write_new_file(path: &Path, contents: &str, mode: u32) -> Result<(), AppError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| file_error("create", path.to_path_buf(), source))?;
    file.write_all(contents.as_bytes())
        .map_err(|source| file_error("write", path.to_path_buf(), source))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|source| file_error("set permissions", path.to_path_buf(), source))
}

fn check_config(config_path: &Path) -> Result<(), AppError> {
    let config = load_config(config_path)?;
    let mut project_ids: Vec<String> = config.projects.keys().cloned().collect();
    project_ids.sort();
    emit_json(&CheckConfigOutput {
        ok: true,
        config_path: config_path.to_path_buf(),
        project_ids,
    })
}

async fn status(config_path: &Path, project_id: &str) -> Result<(), AppError> {
    let (config, database) = load_database(config_path).await?;
    config.project(project_id)?;
    let project = database.project_state(project_id).await?;
    let deployments = database.deployments_for_project(project_id).await?;
    emit_json(&StatusOutput {
        ok: true,
        project,
        deployments,
    })
}

async fn stop(config_path: &Path, project_id: &str) -> Result<(), AppError> {
    let (config, database) = load_database(config_path).await?;
    config.project(project_id)?;
    let project = database.stop_project(project_id).await?;
    emit_json(&ProjectOutput { ok: true, project })
}

async fn start(config_path: &Path, project_id: &str) -> Result<(), AppError> {
    let (config, database) = load_database(config_path).await?;
    config.project(project_id)?;
    let project = database.start_project(project_id).await?;
    emit_json(&ProjectOutput { ok: true, project })
}

async fn delete(config_path: &Path, project_id: &str) -> Result<(), AppError> {
    let (config, database) = load_database(config_path).await?;
    let project = config.project(project_id)?;
    let project_config_path = project.config_path.clone();
    let current_path = project.current_path.clone();
    let staged_config_path = staged_deletion_path(&project_config_path)?;
    fs::rename(&project_config_path, &staged_config_path).map_err(|source| {
        file_error(
            "stage project configuration deletion",
            project_config_path.clone(),
            source,
        )
    })?;
    if let Err(source) = database.delete_project(project_id).await {
        return match fs::rename(&staged_config_path, &project_config_path) {
            Ok(()) => Err(source),
            Err(restore_error) => Err(AppError::InvalidState {
                reason: format!(
                    "project deletion failed with {source}; restoring configuration {staged_config_path:?} to {project_config_path:?} also failed: {restore_error}"
                ),
            }),
        };
    }
    fs::remove_file(&staged_config_path).map_err(|source| {
        file_error(
            "remove deleted project configuration",
            staged_config_path,
            source,
        )
    })?;
    emit_json(&DeleteProjectOutput {
        ok: true,
        project_id: project_id.to_owned(),
        project_config_path,
        current_path,
    })
}

async fn clean(config_path: &Path, args: &CleanArgs) -> Result<(), AppError> {
    let (config, database) = load_database(config_path).await?;
    config.project(&args.project_id)?;
    let deployments = database
        .clean_deployments(&args.project_id, args.keep, args.deployment_type.into())
        .await?;
    let mut removed_worktrees = 0;
    let mut removed_targets = 0;
    for deployment in &deployments {
        removed_worktrees += usize::from(remove_directory_if_present(
            &config
                .global
                .state_directory
                .join("worktrees")
                .join(&deployment.id),
        )?);
        removed_targets += usize::from(remove_directory_if_present(
            &config
                .global
                .state_directory
                .join("targets")
                .join(&deployment.id),
        )?);
    }
    emit_json(&CleanOutput {
        ok: true,
        project_id: args.project_id.clone(),
        deployment_type: match args.deployment_type {
            CleanType::Failed => "failed",
            CleanType::Succeeded => "succeeded",
            CleanType::All => "all",
        }
        .to_owned(),
        keep: args.keep,
        removed_deployments: deployments.len(),
        removed_worktrees,
        removed_targets,
    })
}

fn remove_directory_if_present(path: &Path) -> Result<bool, AppError> {
    if !path.exists() {
        return Ok(false);
    }
    if !path.is_dir() {
        return Err(AppError::InvalidState {
            reason: format!("cleanup path {path:?} exists and is not a directory"),
        });
    }
    fs::remove_dir_all(path)
        .map_err(|source| file_error("remove directory", path.to_path_buf(), source))?;
    Ok(true)
}

fn staged_deletion_path(project_config_path: &Path) -> Result<PathBuf, AppError> {
    let file_name = project_config_path
        .file_name()
        .ok_or_else(|| AppError::InvalidState {
            reason: format!("project configuration {project_config_path:?} has no file name"),
        })?;
    let mut staged_name = file_name.to_os_string();
    staged_name.push(format!(".deleting-{}", Uuid::now_v7().simple()));
    Ok(project_config_path.with_file_name(staged_name))
}

async fn retry(config_path: &Path, deployment_id: &str) -> Result<(), AppError> {
    let (_config, database) = load_database(config_path).await?;
    let deployment = database.create_retry(deployment_id).await?;
    emit_json(&DeploymentOutput {
        ok: true,
        deployment,
    })
}

async fn rollback(config_path: &Path, project_id: &str, commit_sha: &str) -> Result<(), AppError> {
    validate_cli_sha(commit_sha)?;
    let (config, database) = load_database(config_path).await?;
    config.project(project_id)?;
    let deployment = database.create_rollback(project_id, commit_sha).await?;
    emit_json(&DeploymentOutput {
        ok: true,
        deployment,
    })
}

async fn unblock(config_path: &Path, project_id: &str) -> Result<(), AppError> {
    let (config, database) = load_database(config_path).await?;
    config.project(project_id)?;
    database.unblock_project(project_id).await?;
    let project = database.project_state(project_id).await?;
    emit_json(&ProjectOutput { ok: true, project })
}

async fn serve(config_path: &Path) -> Result<(), AppError> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("aynur_deploy=info"))
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .try_init()
        .map_err(|source| AppError::InvalidState {
            reason: format!("structured logging initialization failed: {source}"),
        })?;
    let (config, database) = load_database(config_path).await?;
    tokio::fs::create_dir_all(&config.global.state_directory)
        .await
        .map_err(|source| {
            file_error(
                "create directory",
                config.global.state_directory.clone(),
                source,
            )
        })?;
    let config = Arc::new(config);
    let notifier = Arc::new(Notify::new());
    let deployer = Deployer::new(config.clone(), database.clone())?;
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let mut worker = tokio::spawn(run_worker(deployer, notifier.clone(), shutdown_receiver));
    let state = AppState {
        config: config.clone(),
        database,
        notifier,
    };
    let listener = tokio::net::TcpListener::bind(config.global.listen_address)
        .await
        .map_err(|source| {
            file_error(
                "bind TCP listener",
                PathBuf::from(config.global.listen_address.to_string()),
                source,
            )
        })?;
    emit_json(&ServeOutput {
        ok: true,
        status: "serving",
        listen_address: config.global.listen_address.to_string(),
    })?;
    let server = axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => {
            result.map_err(|source| AppError::InvalidState {
                reason: format!("HTTP server failed: {source}"),
            })?;
            shutdown_sender.send(true).map_err(|source| AppError::TaskJoin {
                reason: format!("could not notify worker to stop: {source}"),
            })?;
            worker.await.map_err(|source| AppError::TaskJoin {
                reason: format!("worker join failed: {source}"),
            })??;
        }
        result = &mut worker => {
            result.map_err(|source| AppError::TaskJoin {
                reason: format!("worker join failed: {source}"),
            })??;
            return Err(AppError::TaskJoin {
                reason: "worker stopped before HTTP server shutdown".to_owned(),
            });
        }
    }
    Ok(())
}

async fn load_database(config_path: &Path) -> Result<(LoadedConfig, Database), AppError> {
    let config = load_config(config_path)?;
    let database = Database::connect(&config.global.database_path).await?;
    database.register_projects(config.projects.keys()).await?;
    Ok((config, database))
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let terminate_result =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        let mut terminate = match terminate_result {
            Ok(value) => value,
            Err(source) => {
                tracing::error!(error = %source, "SIGTERM handler installation failed");
                if let Err(ctrl_c_error) = tokio::signal::ctrl_c().await {
                    tracing::error!(error = %ctrl_c_error, "SIGINT handler failed");
                }
                return;
            }
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(source) = result {
                    tracing::error!(error = %source, "SIGINT handler failed");
                }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(source) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %source, "shutdown signal handler failed");
        }
    }
}

fn validate_cli_sha(value: &str) -> Result<(), AppError> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AppError::RequestValidation {
            reason: format!(
                "commitSha must be a lowercase 40-character hexadecimal Git commit ID, got {value:?}"
            ),
        });
    }
    Ok(())
}

fn emit_json<T: Serialize>(value: &T) -> Result<(), AppError> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(&mut handle, value).map_err(|source| AppError::InvalidState {
        reason: format!("could not serialize CLI JSON output: {source}"),
    })?;
    handle
        .write_all(b"\n")
        .map_err(|source| file_error("write stdout", PathBuf::from("/dev/stdout"), source))
}

fn emit_text_stdout(value: &str) -> Result<(), AppError> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle
        .write_all(value.as_bytes())
        .map_err(|source| file_error("write stdout", PathBuf::from("/dev/stdout"), source))?;
    if !value.ends_with('\n') {
        handle
            .write_all(b"\n")
            .map_err(|source| file_error("write stdout", PathBuf::from("/dev/stdout"), source))?;
    }
    Ok(())
}

fn emit_stderr<T: Serialize>(value: &T) {
    let stderr = io::stderr();
    let mut handle = stderr.lock();
    if serde_json::to_writer_pretty(&mut handle, value).is_ok() {
        let _ = handle.write_all(b"\n");
    }
}
