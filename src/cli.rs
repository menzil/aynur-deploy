use std::fs::{self, OpenOptions};
use std::future::IntoFuture;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use tokio::sync::{Notify, watch};
use uuid::Uuid;

use crate::config::{LoadedConfig, load_config};
use crate::db::Database;
use crate::deploy::{Deployer, run_worker};
use crate::error::{AppError, file_error};
use crate::model::{Deployment, ProjectState};
use crate::web::{AppState, router};

#[derive(Debug, Parser)]
#[command(
    name = "aynur-deploy",
    version,
    about = "Strict Gitee Tag deployment service"
)]
struct Cli {
    #[arg(long, global = true, default_value = "/etc/aynur-deploy")]
    home: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Initialize the service configuration directory")]
    Init,
    #[command(about = "Check global and project configuration")]
    Check,
    #[command(about = "Add a project and generate its WebHook password")]
    Add(AddArgs),
    #[command(about = "Show deployment status for a project")]
    Status(ProjectArgs),
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
struct AddArgs {
    project_id: String,
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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AddOutput {
    ok: bool,
    project_id: String,
    project_config_path: PathBuf,
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
    let config_path = cli.home.join("config.toml");
    match cli.command {
        Command::Init => init_home(&cli.home),
        Command::Check => check_config(&config_path),
        Command::Add(args) => add_project(&cli.home, &args.project_id),
        Command::Status(args) => status(&config_path, &args.project_id).await,
        Command::Retry(args) => retry(&config_path, &args.deployment_id).await,
        Command::Rollback(args) => rollback(&config_path, &args.project_id, &args.commit_sha).await,
        Command::Unblock(args) => unblock(&config_path, &args.project_id).await,
        Command::Serve => serve(&config_path).await,
    }
}

fn init_home(home: &Path) -> Result<(), AppError> {
    let config_path = home.join("config.toml");
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
        let config = format_global_config(home);
        write_new_file(&config_path, &config, 0o600)?;
    }
    emit_json(&InitOutput {
        ok: true,
        config_path,
        projects_directory,
    })
}

fn add_project(home: &Path, project_id: &str) -> Result<(), AppError> {
    validate_project_id(project_id)?;
    let config_path = home.join("config.toml");
    if !config_path.exists() {
        return Err(AppError::Config {
            path: config_path,
            reason: "configuration is not initialized; run `aynur-deploy init` first".to_owned(),
        });
    }
    let text = fs::read_to_string(&config_path)
        .map_err(|source| file_error("read", config_path.clone(), source))?;
    let config: crate::config::GlobalConfig =
        toml::from_str(&text).map_err(|source| AppError::Config {
            path: config_path,
            reason: source.to_string(),
        })?;
    let projects_directory = config.projects_directory;
    fs::create_dir_all(&projects_directory)
        .map_err(|source| file_error("create directory", projects_directory.clone(), source))?;
    let project_config_path = projects_directory.join(format!("{project_id}.toml"));
    let token = format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple());
    let project_config = format_project_config(project_id, &token);
    write_new_file(&project_config_path, &project_config, 0o600)?;
    emit_json(&AddOutput {
        ok: true,
        project_id: project_id.to_owned(),
        project_config_path,
    })
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

fn format_global_config(home: &Path) -> String {
    let state_directory = if home == Path::new("/etc/aynur-deploy") {
        Path::new("/var/lib/aynur-deploy").to_path_buf()
    } else {
        home.join("state")
    };
    format!(
        "listenAddress = \"127.0.0.1:9091\"\ndatabasePath = \"{}\"\nstateDirectory = \"{}\"\nprojectsDirectory = \"{}\"\ngitCommand = \"/usr/bin/git\"\ncargoCommand = \"/usr/bin/cargo\"\naynurCommand = \"/usr/local/bin/aynur\"\ngitFetchAttempts = 3\ngitFetchRetryDelayMs = 2000\ncommandTimeoutSeconds = 1800\nmaxWebhookBodyBytes = 65536\nworkerPollIntervalMs = 1000\n",
        state_directory.join("deployments.sqlite3").display(),
        state_directory.display(),
        home.join("projects").display(),
    )
}

fn format_project_config(project_id: &str, token: &str) -> String {
    format!(
        "projectId = \"{project_id}\"\nrepositoryFullName = \"owner/repository\"\nrepositoryUrl = \"https://gitee.com/owner/repository.git\"\nwebhookToken = \"{token}\"\ntagPattern = \"^deploy-[0-9]{{8}}-[0-9]{{6}}$\"\nretainReleases = 3\n\n[healthCheck]\nurl = \"http://127.0.0.1:8080/\"\nattempts = 5\nintervalMs = 2000\ntimeoutMs = 5000\n\n[deployment]\ntype = \"static\"\nentryFile = \"index.html\"\n"
    )
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
    serde_json::to_writer(&mut handle, value).map_err(|source| AppError::InvalidState {
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
    if serde_json::to_writer(&mut handle, value).is_ok() {
        let _ = handle.write_all(b"\n");
    }
}
