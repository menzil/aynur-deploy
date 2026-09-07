use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::error::AppError;

const OUTPUT_CAPTURE_BYTES: usize = 64 * 1024;
const LOG_LINE_BYTES: usize = 8 * 1024;
const OUTPUT_TRUNCATED_MARKER: &str = "[... earlier command output truncated ...]\n";

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
    mut reader: R,
    program: PathBuf,
    stream: &'static str,
) -> std::io::Result<String>
where
    R: AsyncRead + Unpin,
{
    let mut capture = VecDeque::with_capacity(OUTPUT_CAPTURE_BYTES);
    let mut line = Vec::with_capacity(LOG_LINE_BYTES);
    let mut line_truncated = false;
    let mut output_truncated = false;
    let mut chunk = [0_u8; 8 * 1024];

    loop {
        let bytes_read = reader.read(&mut chunk).await?;
        if bytes_read == 0 {
            break;
        }
        let chunk = &chunk[..bytes_read];
        append_tail(&mut capture, chunk, &mut output_truncated);
        log_lines(chunk, &mut line, &mut line_truncated, &program, stream)?;
    }
    log_final_line(&line, line_truncated, &program, stream)?;

    let bytes: Vec<u8> = capture.into();
    let output = decode_capture(&bytes, output_truncated)?;
    if output_truncated {
        Ok(format!("{OUTPUT_TRUNCATED_MARKER}{output}"))
    } else {
        Ok(output)
    }
}

fn decode_capture(bytes: &[u8], truncated: bool) -> io::Result<String> {
    let mut bytes = bytes;
    if truncated {
        while let Err(source) = std::str::from_utf8(bytes) {
            if source.valid_up_to() != 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, source));
            }
            bytes = &bytes[1..];
        }
    }
    let output = std::str::from_utf8(bytes)
        .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
    Ok(output.lines().collect::<Vec<_>>().join("\n"))
}

fn append_tail(capture: &mut VecDeque<u8>, chunk: &[u8], truncated: &mut bool) {
    if chunk.len() >= OUTPUT_CAPTURE_BYTES {
        capture.clear();
        capture.extend(&chunk[chunk.len() - OUTPUT_CAPTURE_BYTES..]);
        *truncated = true;
        return;
    }
    let overflow = capture
        .len()
        .saturating_add(chunk.len())
        .saturating_sub(OUTPUT_CAPTURE_BYTES);
    if overflow > 0 {
        capture.drain(..overflow);
        *truncated = true;
    }
    capture.extend(chunk);
}

fn log_lines(
    chunk: &[u8],
    line: &mut Vec<u8>,
    line_truncated: &mut bool,
    program: &Path,
    stream: &'static str,
) -> io::Result<()> {
    for &byte in chunk {
        if byte == b'\n' {
            log_line(line, *line_truncated, program, stream)?;
            line.clear();
            *line_truncated = false;
        } else if line.len() < LOG_LINE_BYTES {
            line.push(byte);
        } else {
            *line_truncated = true;
        }
    }
    Ok(())
}

fn log_final_line(
    line: &[u8],
    truncated: bool,
    program: &Path,
    stream: &'static str,
) -> io::Result<()> {
    if !line.is_empty() || truncated {
        log_line(line, truncated, program, stream)?;
    }
    Ok(())
}

fn log_line(line: &[u8], truncated: bool, program: &Path, stream: &'static str) -> io::Result<()> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let line = std::str::from_utf8(line)
        .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
    tracing::info!(
        program = %program.display(),
        stream,
        line,
        truncated,
        "command output"
    );
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn command_output_capture_keeps_only_a_bounded_tail() {
        let (mut writer, reader) = tokio::io::duplex(8 * 1024);
        let reader_task =
            tokio::spawn(read_stream(reader, PathBuf::from("test-command"), "stdout"));
        let payload = vec![b'x'; OUTPUT_CAPTURE_BYTES * 4];
        writer.write_all(&payload).await.unwrap();
        writer.write_all(b"\ntail").await.unwrap();
        drop(writer);

        let output = reader_task.await.unwrap().unwrap();
        assert!(output.starts_with(OUTPUT_TRUNCATED_MARKER));
        assert!(output.ends_with("tail"));
        assert!(output.len() <= OUTPUT_CAPTURE_BYTES + OUTPUT_TRUNCATED_MARKER.len());
    }

    #[tokio::test]
    async fn command_output_capture_preserves_small_line_output() {
        let input = b"first\r\nsecond\n";
        let output = read_stream(&input[..], PathBuf::from("test-command"), "stderr")
            .await
            .unwrap();

        assert_eq!(output, "first\nsecond");
    }
}
