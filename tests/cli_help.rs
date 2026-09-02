use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
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
    assert_eq!(output.stdout, b"aynur-deploy 0.2.0\n");
}

#[test]
fn init_creates_private_project_config_with_generated_token() {
    let directory = tempdir().expect("temporary home must be created");
    let xdg_config_home = directory.path().join("xdg");
    let output = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["init", "--home"])
        .arg(directory.path())
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("init command must start");
    assert!(output.status.success(), "stderr={:?}", output.stderr);
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("{\n  \"ok\": true,\n"));

    let empty_list = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .arg("list")
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("list command must start");
    assert!(
        empty_list.status.success(),
        "stderr={:?}",
        empty_list.stderr
    );
    let empty_list_json: Value =
        serde_json::from_slice(&empty_list.stdout).expect("list output must be JSON");
    assert_eq!(empty_list_json["projects"], serde_json::json!([]));

    let add = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["add", "orhan-blog"])
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("add command must start");
    assert!(add.status.success(), "stderr={:?}", add.stderr);
    let project_path = directory.path().join("projects/orhan-blog.toml");
    let project_text = fs::read_to_string(&project_path).expect("project config must exist");
    assert!(project_text.contains("webhookToken = \""));
    assert!(project_text.contains(&format!(
        "currentPath = \"{}\"",
        directory
            .path()
            .join("state/projects/orhan-blog/current")
            .display()
    )));
    assert_eq!(
        fs::metadata(project_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let repeated = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["init", "--home"])
        .arg(directory.path())
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("repeated init command must start");
    assert!(repeated.status.success(), "stderr={:?}", repeated.stderr);
    let repeated_json: Value =
        serde_json::from_slice(&repeated.stdout).expect("init output must be JSON");
    assert_eq!(repeated_json["alreadyInitialized"], true);

    let latest_home = directory.path().join("latest");
    let latest_init = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["init", "--home"])
        .arg(&latest_home)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("latest init command must start");
    assert!(
        latest_init.status.success(),
        "stderr={:?}",
        latest_init.stderr
    );
    let latest_config_path = latest_home.join("config.toml");
    let latest_config = fs::read_to_string(&latest_config_path).expect("config must exist");
    fs::write(
        &latest_config_path,
        latest_config.replace("/usr/bin/cargo", env!("CARGO")),
    )
    .expect("cargoCommand must be updated for the test environment");
    let latest_add = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["add", "latest-project"])
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("latest add command must start");
    assert!(
        latest_add.status.success(),
        "stderr={:?}",
        latest_add.stderr
    );
    assert!(latest_home.join("projects/latest-project.toml").is_file());

    let check = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["check"])
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("check command must start");
    assert!(check.status.success(), "stderr={:?}", check.stderr);

    let status = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["status", "latest-project"])
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("status command must start");
    assert!(status.status.success(), "stderr={:?}", status.stderr);
    let status_text = String::from_utf8(status.stdout).expect("status output must be UTF-8");
    assert!(status_text.starts_with("{\n  \"ok\": true,\n"));
    assert!(status_text.contains("\n  \"deployments\": []\n}"));

    let custom_current_path = latest_home.join("published/test-binary");
    for (project_id, deployment_type, current_path, expected) in [
        (
            "test-binary",
            "binary",
            Some(custom_current_path.as_path()),
            "binaryPath = \"bin/my-service\"",
        ),
        ("test-rust", "rust", None, "cargoManifest = \"Cargo.toml\""),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"));
        command
            .args(["add", project_id, "--type", deployment_type])
            .env("XDG_CONFIG_HOME", &xdg_config_home);
        if let Some(path) = current_path {
            command.arg("--current-path").arg(path);
        }
        let add = command.output().expect("add command must start");
        assert!(add.status.success(), "stderr={:?}", add.stderr);
        let text = fs::read_to_string(latest_home.join(format!("projects/{project_id}.toml")))
            .expect("typed project config must exist");
        assert!(text.contains(expected));
        assert!(text.contains("# [reload]"));
        let expected_current_path = current_path.map_or_else(
            || {
                latest_home
                    .join("state/projects")
                    .join(project_id)
                    .join("current")
            },
            PathBuf::from,
        );
        assert!(text.contains(&format!(
            "currentPath = \"{}\"",
            expected_current_path.display()
        )));
    }

    let list = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .arg("list")
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("list command must start");
    assert!(list.status.success(), "stderr={:?}", list.stderr);
    let list_json: Value = serde_json::from_slice(&list.stdout).expect("list output must be JSON");
    assert_eq!(list_json["ok"], true);
    assert_eq!(list_json["projects"][0]["projectId"], "latest-project");
    assert_eq!(list_json["projects"][1]["projectId"], "test-binary");
    assert_eq!(
        list_json["projects"][1]["currentPath"],
        custom_current_path.to_str().unwrap()
    );
    assert_eq!(list_json["projects"][2]["projectId"], "test-rust");

    let relative = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["add", "invalid-path", "--current-path", "relative/current"])
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("add with relative current path must start");
    assert!(!relative.status.success());
    assert!(String::from_utf8_lossy(&relative.stderr).starts_with("{\n  \"ok\": false,\n"));
    assert!(
        String::from_utf8_lossy(&relative.stderr)
            .contains("currentPath must be a normalized absolute path other than /")
    );
}
