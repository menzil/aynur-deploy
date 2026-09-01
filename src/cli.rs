use std::future::IntoFuture;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use tokio::sync::{Notify, watch};

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
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    CheckConfig(ConfigOnlyArgs),
    Status(ProjectArgs),
    Retry(RetryArgs),
    Rollback(RollbackArgs),
    Unblock(ProjectArgs),
    Serve(ConfigOnlyArgs),
}

#[derive(Debug, Args)]
struct ConfigOnlyArgs {
    #[arg(long)]
    config: PathBuf,
}

#[derive(Debug, Args)]
struct ProjectArgs {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    project_id: String,
}

#[derive(Debug, Args)]
struct RetryArgs {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    deployment_id: String,
}

#[derive(Debug, Args)]
struct RollbackArgs {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    project_id: String,
    #[arg(long)]
    commit_sha: String,
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CliHelpOutput {
    ok: bool,
    help: String,
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
            let output = CliHelpOutput {
                ok: true,
                help: source.to_string(),
            };
            return emit_stdout(&output);
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
        Command::CheckConfig(args) => check_config(&args.config),
        Command::Status(args) => status(&args.config, &args.project_id).await,
        Command::Retry(args) => retry(&args.config, &args.deployment_id).await,
        Command::Rollback(args) => rollback(&args.config, &args.project_id, &args.commit_sha).await,
        Command::Unblock(args) => unblock(&args.config, &args.project_id).await,
        Command::Serve(args) => serve(&args.config).await,
    }
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

fn emit_stdout<T: Serialize>(value: &T) -> ExitCode {
    match emit_json(value) {
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

fn emit_stderr<T: Serialize>(value: &T) {
    let stderr = io::stderr();
    let mut handle = stderr.lock();
    if serde_json::to_writer(&mut handle, value).is_ok() {
        let _ = handle.write_all(b"\n");
    }
}
