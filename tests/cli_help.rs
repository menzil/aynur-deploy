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
    assert_eq!(output.stdout, b"aynur-deploy 0.4.0\n");
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
        let add_json: Value = serde_json::from_slice(&add.stdout).expect("add output must be JSON");
        assert_eq!(add_json["bootstrapPath"], Value::Null);
        let text = fs::read_to_string(latest_home.join(format!("projects/{project_id}.toml")))
            .expect("typed project config must exist");
        assert!(text.contains(expected));
        assert!(text.contains("# [reload]"));
        assert!(text.contains("# commands = [["));
        if deployment_type == "rust" {
            assert!(text.contains("binaries = [{ package = \"my-service\""));
            assert!(text.contains("includePaths = []"));
            assert!(text.contains("# [migration]"));
        }
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
    assert_eq!(list_json["projects"][0]["status"], "running");
    assert_eq!(list_json["projects"][1]["projectId"], "test-binary");
    assert_eq!(
        list_json["projects"][1]["currentPath"],
        custom_current_path.to_str().unwrap()
    );
    assert_eq!(list_json["projects"][1]["status"], "running");
    assert_eq!(list_json["projects"][2]["projectId"], "test-rust");

    let stop = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["stop", "test-binary"])
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("stop command must start");
    assert!(stop.status.success(), "stderr={:?}", stop.stderr);
    let stop_json: Value = serde_json::from_slice(&stop.stdout).unwrap();
    assert_eq!(stop_json["project"]["status"], "stopped");

    let stopped_list = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .arg("list")
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("list command must start");
    assert!(stopped_list.status.success());
    let stopped_list_json: Value = serde_json::from_slice(&stopped_list.stdout).unwrap();
    let stopped_project = stopped_list_json["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["projectId"] == "test-binary")
        .unwrap();
    assert_eq!(stopped_project["status"], "stopped");

    let start = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["start", "test-binary"])
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("start command must start");
    assert!(start.status.success(), "stderr={:?}", start.stderr);
    let start_json: Value = serde_json::from_slice(&start.stdout).unwrap();
    assert_eq!(start_json["project"]["status"], "running");

    let running_delete = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["delete", "test-binary"])
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("delete command must start");
    assert!(!running_delete.status.success());
    let running_delete_json: Value = serde_json::from_slice(&running_delete.stderr).unwrap();
    assert_eq!(running_delete_json["error"]["code"], "projectMustBeStopped");
    assert!(latest_home.join("projects/test-binary.toml").is_file());

    let release_path = latest_home.join("state/projects/test-binary/releases/release-one");
    fs::create_dir_all(&release_path).unwrap();
    fs::write(release_path.join("test-binary"), "preserved").unwrap();
    fs::create_dir_all(custom_current_path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&release_path, &custom_current_path).unwrap();
    let final_stop = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["stop", "test-binary"])
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .unwrap();
    assert!(final_stop.status.success());
    let delete = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["delete", "test-binary"])
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("delete command must start");
    assert!(delete.status.success(), "stderr={:?}", delete.stderr);
    assert!(!latest_home.join("projects/test-binary.toml").exists());
    assert_eq!(fs::read_link(&custom_current_path).unwrap(), release_path);
    assert_eq!(
        fs::read_to_string(custom_current_path.join("test-binary")).unwrap(),
        "preserved"
    );

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

#[test]
fn add_adopts_an_existing_current_path_directory() {
    let directory = tempdir().expect("temporary home must be created");
    let xdg_config_home = directory.path().join("xdg");
    let init = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["init", "--home"])
        .arg(directory.path())
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("init command must start");
    assert!(init.status.success(), "stderr={:?}", init.stderr);

    let current_path = directory.path().join("published-site");
    fs::create_dir(&current_path).expect("existing current directory must be created");
    fs::write(current_path.join("index.html"), "existing site")
        .expect("existing site must be written");

    let add = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["add", "existing-site", "--current-path"])
        .arg(&current_path)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("add command must start");
    assert!(add.status.success(), "stderr={:?}", add.stderr);

    let bootstrap_path = directory.path().join("published-site.before-aynur-deploy");
    let add_json: Value = serde_json::from_slice(&add.stdout).expect("add output must be JSON");
    assert_eq!(add_json["bootstrapPath"], bootstrap_path.to_str().unwrap());
    assert_eq!(
        fs::read_link(&current_path).expect("current path must be a symbolic link"),
        bootstrap_path
    );
    assert_eq!(
        fs::read_to_string(current_path.join("index.html"))
            .expect("existing site must remain readable"),
        "existing site"
    );

    let check = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .arg("check")
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("check command must start");
    assert!(check.status.success(), "stderr={:?}", check.stderr);

    let existing_target = directory.path().join("existing-target");
    let existing_link = directory.path().join("existing-link");
    fs::create_dir(&existing_target).expect("existing link target must be created");
    std::os::unix::fs::symlink(&existing_target, &existing_link)
        .expect("existing current symlink must be created");
    let add_link = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["add", "linked-site", "--current-path"])
        .arg(&existing_link)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("add command must start");
    assert!(add_link.status.success(), "stderr={:?}", add_link.stderr);
    let add_link_json: Value =
        serde_json::from_slice(&add_link.stdout).expect("add output must be JSON");
    assert_eq!(add_link_json["bootstrapPath"], Value::Null);
    assert_eq!(
        fs::read_link(&existing_link).expect("existing current symlink must remain readable"),
        existing_target
    );
}

#[test]
fn add_preserves_an_existing_directory_when_bootstrap_path_exists() {
    let directory = tempdir().expect("temporary home must be created");
    let xdg_config_home = directory.path().join("xdg");
    let init = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["init", "--home"])
        .arg(directory.path())
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("init command must start");
    assert!(init.status.success(), "stderr={:?}", init.stderr);

    let current_path = directory.path().join("published-site");
    let bootstrap_path = directory.path().join("published-site.before-aynur-deploy");
    fs::create_dir(&current_path).expect("existing current directory must be created");
    fs::write(current_path.join("index.html"), "existing site")
        .expect("existing site must be written");
    fs::create_dir(&bootstrap_path).expect("conflicting bootstrap directory must be created");

    let add = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["add", "conflicting-site", "--current-path"])
        .arg(&current_path)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("add command must start");
    assert!(!add.status.success());
    assert!(String::from_utf8_lossy(&add.stderr).contains("bootstrap path"));
    assert!(current_path.is_dir());
    assert_eq!(
        fs::read_to_string(current_path.join("index.html"))
            .expect("existing site must remain unchanged"),
        "existing site"
    );
    assert!(
        !directory
            .path()
            .join("projects/conflicting-site.toml")
            .exists()
    );

    let file_path = directory.path().join("published-file");
    fs::write(&file_path, "not a directory").expect("existing file must be written");
    let add_file = Command::new(env!("CARGO_BIN_EXE_aynur-deploy"))
        .args(["add", "file-site", "--current-path"])
        .arg(&file_path)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .expect("add command must start");
    assert!(!add_file.status.success());
    assert!(
        String::from_utf8_lossy(&add_file.stderr)
            .contains("must be absent, a symbolic link, or an existing directory")
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("existing file must remain unchanged"),
        "not a directory"
    );
    assert!(!directory.path().join("projects/file-site.toml").exists());
}
