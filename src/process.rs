use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
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
    let mut child = command.spawn().map_err(|source| AppError::CommandFailed {
        program: spec.program.clone(),
        args: spec.args.clone(),
        status: None,
        stdout: String::new(),
        stderr: source.to_string(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| AppError::InvalidState {
        reason: format!("command {:?} stdout pipe was not available", spec.program),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| AppError::InvalidState {
        reason: format!("command {:?} stderr pipe was not available", spec.program),
    })?;
    let stdout_task = tokio::spawn(read_stream(stdout, spec.program.clone(), "stdout"));
    let stderr_task = tokio::spawn(read_stream(stderr, spec.program.clone(), "stderr"));
    let status =
        match tokio::time::timeout(Duration::from_secs(timeout_seconds), child.wait()).await {
            Ok(result) => result.map_err(|source| AppError::CommandFailed {
                program: spec.program.clone(),
                args: spec.args.clone(),
                status: None,
                stdout: String::new(),
                stderr: source.to_string(),
            })?,
            Err(_) => {
                child
                    .kill()
                    .await
                    .map_err(|source| AppError::CommandFailed {
                        program: spec.program.clone(),
                        args: spec.args.clone(),
                        status: None,
                        stdout: String::new(),
                        stderr: format!("timed out and failed to terminate command: {source}"),
                    })?;
                return Err(AppError::CommandTimedOut {
                    program: spec.program.clone(),
                    args: spec.args.clone(),
                    timeout_seconds,
                });
            }
        };
    let stdout = join_stream(stdout_task, spec, "stdout").await?;
    let stderr = join_stream(stderr_task, spec, "stderr").await?;
    if !status.success() {
        return Err(AppError::CommandFailed {
            program: spec.program.clone(),
            args: spec.args.clone(),
            status: status.code(),
            stdout,
            stderr,
        });
    }
    Ok(CommandOutput { stdout, stderr })
}

async fn read_stream<R>(
    reader: R,
    program: PathBuf,
    stream: &'static str,
) -> std::io::Result<String>
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut output = Vec::new();
    while let Some(line) = lines.next_line().await? {
        tracing::info!(program = %program.display(), stream, line = %line, "command output");
        output.push(line);
    }
    Ok(output.join("\n"))
}

async fn join_stream(
    task: tokio::task::JoinHandle<std::io::Result<String>>,
    spec: &CommandSpec,
    stream: &'static str,
) -> Result<String, AppError> {
    task.await
        .map_err(|source| AppError::TaskJoin {
            reason: format!(
                "command {:?} {stream} reader failed: {source}",
                spec.program
            ),
        })?
        .map_err(|source| AppError::CommandFailed {
            program: spec.program.clone(),
            args: spec.args.clone(),
            status: None,
            stdout: String::new(),
            stderr: format!("failed to read {stream}: {source}"),
        })
}
