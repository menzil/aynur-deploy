use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use aynur_deploy::config::load_config;
use aynur_deploy::db::Database;
use aynur_deploy::deploy::Deployer;
use aynur_deploy::model::DeploymentStatus;
use tempfile::TempDir;
use tokio::task::JoinHandle;

const PROJECT_ID: &str = "test-rust";
const REPOSITORY_FULL_NAME: &str = "aynurcn/test-rust";
const TOKEN: &str = "rust-integration-test-token-with-high-entropy";

struct TestEnvironment {
    _temporary_directory: TempDir,
    database: Database,
    deployer: Deployer,
    repository: Repository,
    current_path: PathBuf,
    migration_log: PathBuf,
    reload_log: PathBuf,
    health_server: JoinHandle<()>,
}

struct Repository {
    origin: PathBuf,
    work: PathBuf,
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        self.health_server.abort();
    }
}

#[tokio::test]
async fn rust_release_builds_all_binaries_and_runs_migration_before_activation() {
    let environment = setup().await;
    let commit_sha = commit_and_push_tag(&environment.repository, "deploy-20260903-120000", "v1");
    let deployment = queue(
        &environment,
        "event:rust-success",
        "deploy-20260903-120000",
        &commit_sha,
    )
    .await;

    environment.deployer.process(&deployment).await.unwrap();

    let stored = environment
        .database
        .get_deployment(&deployment.id)
        .await
        .unwrap();
    assert_eq!(stored.status, DeploymentStatus::Succeeded);
    let release_path = stored.release_path.unwrap();
    assert_executable(&release_path.join("gateway"));
    assert_executable(&release_path.join("migrator"));
    assert_eq!(
        fs::read_to_string(release_path.join("config/version.txt")).unwrap(),
        "v1"
    );
    assert!(!release_path.join(".git").exists());
    assert_eq!(
        fs::read_link(&environment.current_path).unwrap(),
        release_path
    );

    let migration_lines = log_lines(&environment.migration_log);
    assert_eq!(migration_lines.len(), 1);
    assert!(migration_lines[0].contains("release=v1"));
    assert!(migration_lines[0].contains("current=missing"));
    assert!(migration_lines[0].contains("build_env=present"));
    assert_eq!(
        log_lines(&environment.reload_log),
        ["reload-one|v1", "reload-two|v1"]
    );

    let recovery = queue(
        &environment,
        "event:migration-recovery",
        "deploy-20260903-120000",
        &commit_sha,
    )
    .await;
    environment
        .database
        .set_resolved_commit(&recovery.id, &commit_sha)
        .await
        .unwrap();
    environment
        .database
        .set_release(&recovery.id, &release_path)
        .await
        .unwrap();
    environment
        .database
        .set_status(&recovery.id, DeploymentStatus::Migrating)
        .await
        .unwrap();
    let interrupted = environment
        .database
        .get_deployment(&recovery.id)
        .await
        .unwrap();
    environment.deployer.process(&interrupted).await.unwrap();
    assert_eq!(
        environment
            .database
            .get_deployment(&recovery.id)
            .await
            .unwrap()
            .status,
        DeploymentStatus::Succeeded
    );
    assert_eq!(log_lines(&environment.migration_log).len(), 2);
}

#[tokio::test]
async fn migration_failure_keeps_the_previous_release_and_skips_reload() {
    let environment = setup().await;
    let first_sha = commit_and_push_tag(&environment.repository, "deploy-20260903-120001", "v1");
    deploy_success(
        &environment,
        "event:migration-first",
        "deploy-20260903-120001",
        &first_sha,
    )
    .await;
    let previous_release = fs::read_link(&environment.current_path).unwrap();

    let failing_sha = commit_and_push_tag(
        &environment.repository,
        "deploy-20260903-120002",
        "migration-failure",
    );
    let failing = queue(
        &environment,
        "event:migration-failure",
        "deploy-20260903-120002",
        &failing_sha,
    )
    .await;
    let error = environment
        .deployer
        .process(&failing)
        .await
        .expect_err("migration must fail");
    assert_eq!(error.code(), "commandFailed");
    environment
        .deployer
        .handle_processing_error(&failing.id, error)
        .await
        .unwrap();

    assert_eq!(
        environment
            .database
            .get_deployment(&failing.id)
            .await
            .unwrap()
            .status,
        DeploymentStatus::Failed
    );
    assert_eq!(
        fs::read_link(&environment.current_path).unwrap(),
        previous_release
    );
    assert_eq!(
        log_lines(&environment.reload_log),
        ["reload-one|v1", "reload-two|v1"]
    );
    assert_eq!(log_lines(&environment.migration_log).len(), 2);
}

#[tokio::test]
async fn failed_health_check_rolls_back_without_rerunning_migration() {
    let environment = setup().await;
    let first_sha = commit_and_push_tag(&environment.repository, "deploy-20260903-120003", "v1");
    deploy_success(
        &environment,
        "event:health-first",
        "deploy-20260903-120003",
        &first_sha,
    )
    .await;
    let previous_release = fs::read_link(&environment.current_path).unwrap();

    let unhealthy_sha = commit_and_push_tag(
        &environment.repository,
        "deploy-20260903-120004",
        "unhealthy",
    );
    let unhealthy = queue(
        &environment,
        "event:health-failure",
        "deploy-20260903-120004",
        &unhealthy_sha,
    )
    .await;
    environment.deployer.process(&unhealthy).await.unwrap();

    assert_eq!(
        environment
            .database
            .get_deployment(&unhealthy.id)
            .await
            .unwrap()
            .status,
        DeploymentStatus::Failed
    );
    assert_eq!(
        fs::read_link(&environment.current_path).unwrap(),
        previous_release
    );
    assert_eq!(log_lines(&environment.migration_log).len(), 2);
    assert_eq!(
        log_lines(&environment.reload_log),
        [
            "reload-one|v1",
            "reload-two|v1",
            "reload-one|unhealthy",
            "reload-two|unhealthy",
            "reload-one|v1",
            "reload-two|v1",
        ]
    );
}

#[tokio::test]
async fn failed_second_reload_command_rolls_back_and_reloads_every_service() {
    let environment = setup().await;
    let first_sha = commit_and_push_tag(&environment.repository, "deploy-20260903-120005", "v1");
    deploy_success(
        &environment,
        "event:reload-first",
        "deploy-20260903-120005",
        &first_sha,
    )
    .await;
    let previous_release = fs::read_link(&environment.current_path).unwrap();

    let failing_sha = commit_and_push_tag(
        &environment.repository,
        "deploy-20260903-120006",
        "reload-failure",
    );
    let failing = queue(
        &environment,
        "event:reload-failure",
        "deploy-20260903-120006",
        &failing_sha,
    )
    .await;
    environment.deployer.process(&failing).await.unwrap();

    assert_eq!(
        environment
            .database
            .get_deployment(&failing.id)
            .await
            .unwrap()
            .status,
        DeploymentStatus::Failed
    );
    assert_eq!(
        fs::read_link(&environment.current_path).unwrap(),
        previous_release
    );
    assert_eq!(log_lines(&environment.migration_log).len(), 2);
    assert_eq!(
        log_lines(&environment.reload_log),
        [
            "reload-one|v1",
            "reload-two|v1",
            "reload-one|reload-failure",
            "reload-two|reload-failure",
            "reload-one|v1",
            "reload-two|v1",
        ]
    );
}

async fn setup() -> TestEnvironment {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let repository = create_repository(root);
    write_rust_workspace(&repository.work);
    let state_directory = root.join("state");
    let current_path = root.join("published/current");
    let migration_log = root.join("migration.log");
    let reload_log = root.join("reload.log");
    let first_reload = write_reload_command(root, "reload-one", false);
    let second_reload = write_reload_command(root, "reload-two", true);
    let (health_url, health_server) = start_health_server(current_path.clone()).await;
    let environment_file = write_environment_file(root, &migration_log, &current_path);
    let config_path = write_config(
        root,
        &repository.origin,
        &state_directory,
        &current_path,
        &health_url,
        &environment_file,
        &reload_log,
        &first_reload,
        &second_reload,
    );
    let config = Arc::new(load_config(&config_path).unwrap());
    let database = Database::connect(&config.global.database_path)
        .await
        .unwrap();
    database
        .register_projects(config.projects.keys())
        .await
        .unwrap();
    let deployer = Deployer::new(config.clone(), database.clone()).unwrap();
    TestEnvironment {
        _temporary_directory: temporary_directory,
        database,
        deployer,
        repository,
        current_path,
        migration_log,
        reload_log,
        health_server,
    }
}

fn create_repository(root: &Path) -> Repository {
    let origin = root.join("origin.git");
    let work = root.join("work");
    run_git(root, &["init", "--bare", origin.to_str().unwrap()]);
    run_git(root, &["init", work.to_str().unwrap()]);
    run_git(&work, &["config", "user.name", "Aynur Deploy Test"]);
    run_git(&work, &["config", "user.email", "deploy-test@example.com"]);
    run_git(
        &work,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    Repository { origin, work }
}

fn write_rust_workspace(root: &Path) {
    fs::create_dir_all(root.join("gateway/src")).unwrap();
    fs::create_dir_all(root.join("migrator/src")).unwrap();
    fs::create_dir_all(root.join("config")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"gateway\", \"migrator\"]\n",
    )
    .unwrap();
    fs::write(
        root.join("gateway/Cargo.toml"),
        "[package]\nname = \"gateway\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n",
    )
    .unwrap();
    fs::write(root.join("gateway/src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(
        root.join("gateway/build.rs"),
        r#"fn main() {
    let value = std::env::var("REQUIRED_BUILD_ENV").expect("REQUIRED_BUILD_ENV is required");
    assert_eq!(value, "present");
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("migrator/Cargo.toml"),
        "[package]\nname = \"migrator\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        root.join("migrator/src/main.rs"),
        r#"use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    assert_eq!(args, ["migration", "apply"]);
    let release = fs::read_to_string("release.txt").unwrap();
    let current_path = PathBuf::from(std::env::var("CURRENT_PATH").unwrap());
    let current = fs::read_link(current_path)
        .ok()
        .and_then(|path| fs::read_to_string(path.join("release.txt")).ok())
        .unwrap_or_else(|| "missing".to_owned());
    let build_env = std::env::var("REQUIRED_BUILD_ENV").unwrap();
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var("MIGRATION_LOG").unwrap())
        .unwrap();
    writeln!(
        log,
        "migration|release={release}|current={current}|build_env={build_env}"
    )
    .unwrap();
    if release == "migration-failure" {
        std::process::exit(20);
    }
}
"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO"))
        .args(["generate-lockfile", "--offline"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Cargo.lock generation failed: stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_and_push_tag(repository: &Repository, tag: &str, content: &str) -> String {
    fs::write(repository.work.join("release.txt"), content).unwrap();
    fs::write(repository.work.join("config/version.txt"), content).unwrap();
    run_git(&repository.work, &["add", "."]);
    run_git(&repository.work, &["commit", "-m", tag]);
    run_git(&repository.work, &["tag", tag]);
    run_git(
        &repository.work,
        &["push", "origin", &format!("refs/tags/{tag}")],
    );
    git_output(&repository.work, &["rev-parse", "HEAD"])
}

fn write_environment_file(root: &Path, migration_log: &Path, current_path: &Path) -> PathBuf {
    let path = root.join("service.env");
    fs::write(
        &path,
        format!(
            "REQUIRED_BUILD_ENV=present\nMIGRATION_LOG={}\nCURRENT_PATH={}\n",
            migration_log.display(),
            current_path.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn write_reload_command(root: &Path, name: &str, fail_on_reload_marker: bool) -> PathBuf {
    let path = root.join(format!("{name}.sh"));
    let failure = if fail_on_reload_marker {
        "if [ \"$value\" = \"reload-failure\" ]; then exit 21; fi\n"
    } else {
        ""
    };
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nset -eu\nvalue=$(cat \"$1/release.txt\")\nprintf '{name}|%s\\n' \"$value\" >> \"$2\"\n{failure}"
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[allow(clippy::too_many_arguments)]
fn write_config(
    root: &Path,
    repository_url: &Path,
    state_directory: &Path,
    current_path: &Path,
    health_url: &str,
    environment_file: &Path,
    reload_log: &Path,
    first_reload: &Path,
    second_reload: &Path,
) -> PathBuf {
    let projects_directory = root.join("projects");
    fs::create_dir(&projects_directory).unwrap();
    let config_path = root.join("config.toml");
    fs::write(
        &config_path,
        format!(
            r#"
listenAddress = "127.0.0.1:9091"
databasePath = "{}"
stateDirectory = "{}"
projectsDirectory = "{}"
gitCommand = "/usr/bin/git"
cargoCommand = "{}"
gitFetchAttempts = 2
gitFetchRetryDelayMs = 1
commandTimeoutSeconds = 30
maxWebhookBodyBytes = 65536
workerPollIntervalMs = 10
"#,
            root.join("deployments.sqlite3").display(),
            state_directory.display(),
            projects_directory.display(),
            env!("CARGO"),
        ),
    )
    .unwrap();
    let project_path = projects_directory.join("test-rust.toml");
    fs::write(
        &project_path,
        format!(
            r#"
projectId = "{PROJECT_ID}"
currentPath = "{}"
repositoryFullName = "{REPOSITORY_FULL_NAME}"
repositoryUrl = "{}"
webhookToken = "{TOKEN}"
tagPattern = "^deploy-[0-9]{{8}}-[0-9]{{6}}$"
retainReleases = 3

[healthCheck]
url = "{health_url}"
attempts = 1
intervalMs = 1
timeoutMs = 1000

[deployment]
type = "rust"
cargoManifest = "Cargo.toml"
environmentFile = "{}"
includePaths = ["release.txt", "config"]
binaries = [
    {{ package = "gateway", binary = "gateway" }},
    {{ package = "migrator", binary = "migrator" }},
]

[migration]
command = ["./migrator", "migration", "apply"]

[reload]
commands = [
    ["{}", "{}", "{}"],
    ["{}", "{}", "{}"],
]
"#,
            current_path.display(),
            repository_url.display(),
            environment_file.display(),
            first_reload.display(),
            current_path.display(),
            reload_log.display(),
            second_reload.display(),
            current_path.display(),
            reload_log.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&project_path, fs::Permissions::from_mode(0o600)).unwrap();
    config_path
}

async fn start_health_server(current_path: PathBuf) -> (String, JoinHandle<()>) {
    let app = Router::new()
        .route("/health", get(health_handler))
        .with_state(current_path);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}/health"), server)
}

async fn health_handler(State(current_path): State<PathBuf>) -> StatusCode {
    let value = fs::read_link(current_path)
        .ok()
        .and_then(|path| fs::read_to_string(path.join("release.txt")).ok());
    match value.as_deref() {
        Some("unhealthy") => StatusCode::SERVICE_UNAVAILABLE,
        Some(_) => StatusCode::OK,
        None => StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn deploy_success(
    environment: &TestEnvironment,
    event_key: &str,
    tag: &str,
    commit_sha: &str,
) {
    let deployment = queue(environment, event_key, tag, commit_sha).await;
    environment.deployer.process(&deployment).await.unwrap();
    assert_eq!(
        environment
            .database
            .get_deployment(&deployment.id)
            .await
            .unwrap()
            .status,
        DeploymentStatus::Succeeded
    );
}

async fn queue(
    environment: &TestEnvironment,
    event_key: &str,
    tag: &str,
    commit_sha: &str,
) -> aynur_deploy::model::Deployment {
    environment
        .database
        .create_webhook_deployment(PROJECT_ID, event_key, tag, commit_sha)
        .await
        .unwrap()
        .deployment
}

fn assert_executable(path: &Path) {
    let metadata = fs::metadata(path).unwrap();
    assert!(metadata.is_file());
    assert_ne!(metadata.permissions().mode() & 0o111, 0);
}

fn log_lines(path: &Path) -> Vec<String> {
    match fs::read_to_string(path) {
        Ok(contents) => contents.lines().map(str::to_owned).collect(),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => panic!("could not read log {path:?}: {source}"),
    }
}

fn run_git(current_directory: &Path, args: &[&str]) {
    let output = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(current_directory)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(current_directory: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(current_directory)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
