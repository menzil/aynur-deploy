use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_directory: Option<PathBuf>,
    pub environment: Vec<(OsString, OsString)>,
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

pub async fn run_command(
    spec: &CommandSpec,
    timeout_seconds: u64,
) -> Result<CommandOutput, AppError> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(current_directory) = &spec.current_directory {
        command.current_dir(current_directory);
    }
    for (key, value) in &spec.environment {
        command.env(key, value);
    }
    let output = tokio::time::timeout(Duration::from_secs(timeout_seconds), command.output())
        .await
        .map_err(|_| AppError::CommandTimedOut {
            program: spec.program.clone(),
            args: spec.args.clone(),
            timeout_seconds,
        })?
        .map_err(|source| AppError::CommandFailed {
            program: spec.program.clone(),
            args: spec.args.clone(),
            status: None,
            stdout: String::new(),
            stderr: source.to_string(),
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() {
        return Err(AppError::CommandFailed {
            program: spec.program.clone(),
            args: spec.args.clone(),
            status: output.status.code(),
            stdout,
            stderr,
        });
    }
    Ok(CommandOutput { stdout, stderr })
}
