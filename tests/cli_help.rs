use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use tempfile::tempdir;

#[test]
fn help_uses_plain_text_with_real_line_breaks() {
    let output = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .arg("--help")
        .output()
        .expect("help command must start");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help output must be UTF-8");
    assert!(stdout.starts_with("Strict Gitee Tag deployment service\n\nUsage:"));
    assert!(!stdout.starts_with('{'));
}

#[test]
fn version_uses_plain_text() {
    let output = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .arg("--version")
        .output()
        .expect("version command must start");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"aynur-deploy 0.1.0\n");
}

#[test]
fn init_creates_private_project_config_with_generated_token() {
    let directory = tempdir().expect("temporary home must be created");
    let output = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["init", "--home"])
        .arg(directory.path())
        .output()
        .expect("init command must start");
    assert!(output.status.success(), "stderr={:?}", output.stderr);
    let add = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["add", "orhan-blog", "--home"])
        .arg(directory.path())
        .output()
        .expect("add command must start");
    assert!(add.status.success(), "stderr={:?}", add.stderr);
    let project_path = directory.path().join("projects/orhan-blog.toml");
    let project_text = fs::read_to_string(&project_path).expect("project config must exist");
    assert!(project_text.contains("webhookToken = \""));
    assert_eq!(
        fs::metadata(project_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let check = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["check", "--home"])
        .arg(directory.path())
        .output()
        .expect("check command must start");
    assert!(check.status.success(), "stderr={:?}", check.stderr);
}
