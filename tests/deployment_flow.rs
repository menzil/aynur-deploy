use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use aynur_deploy::config::{LoadedConfig, load_config};
use aynur_deploy::db::Database;
use aynur_deploy::deploy::Deployer;
use aynur_deploy::model::DeploymentStatus;
use aynur_deploy::web::{AppState, router};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tower::ServiceExt;

const TOKEN: &str = "integration-test-token-with-high-entropy";
const PROJECT_ID: &str = "test-static";
const REPOSITORY_FULL_NAME: &str = "aynurcn/test-static";

struct TestEnvironment {
    _temporary_directory: TempDir,
    config: Arc<LoadedConfig>,
    database: Database,
    deployer: Deployer,
    repository: Repository,
    current_path: PathBuf,
    fail_all_health_checks: Arc<AtomicBool>,
    health_server: JoinHandle<()>,
}

struct Repository {
    origin: PathBuf,
    work: PathBuf,
}

#[derive(Clone)]
struct HealthState {
    current_path: PathBuf,
    fail_all: Arc<AtomicBool>,
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        self.health_server.abort();
    }
}

#[tokio::test]
async fn static_tag_deploys_exact_commit_without_git_metadata() {
    let environment = setup().await;
    let commit_sha = commit_and_push_tag(&environment.repository, "deploy-20260901-120000", "v1");
    let created = environment
        .database
        .create_webhook_deployment(
            PROJECT_ID,
            "event:static-success",
            "deploy-20260901-120000",
            &commit_sha,
        )
        .await
        .expect("deployment must be queued");

    environment
        .deployer
        .process(&created.deployment)
        .await
        .expect("deployment must succeed");

    let stored = environment
        .database
        .get_deployment(&created.deployment.id)
        .await
        .expect("deployment must be readable");
    assert_eq!(stored.status, DeploymentStatus::Succeeded);
    assert_eq!(stored.commit_sha.as_deref(), Some(commit_sha.as_str()));
    let release_path = stored.release_path.expect("releasePath must be recorded");
    assert_eq!(
        fs::read_to_string(release_path.join("index.html")).unwrap(),
        "v1"
    );
    assert!(!release_path.join(".git").exists());
    assert_eq!(
        fs::read_link(&environment.current_path).unwrap(),
        release_path
    );
}

#[tokio::test]
async fn failed_health_check_rolls_back_to_previous_release() {
    let environment = setup().await;
    let first_sha = commit_and_push_tag(&environment.repository, "deploy-20260901-120001", "v1");
    let first = queue(
        &environment,
        "event:first",
        "deploy-20260901-120001",
        &first_sha,
    )
    .await;
    environment.deployer.process(&first).await.unwrap();
    let first_release = fs::read_link(&environment.current_path).unwrap();

    let second_sha = commit_and_push_tag(
        &environment.repository,
        "deploy-20260901-120002",
        "unhealthy",
    );
    let second = queue(
        &environment,
        "event:second",
        "deploy-20260901-120002",
        &second_sha,
    )
    .await;
    environment.deployer.process(&second).await.unwrap();

    let stored = environment
        .database
        .get_deployment(&second.id)
        .await
        .unwrap();
    assert_eq!(stored.status, DeploymentStatus::Failed);
    assert!(
        stored
            .error_message
            .unwrap()
            .contains("rolled back successfully")
    );
    assert_eq!(
        fs::read_link(&environment.current_path).unwrap(),
        first_release
    );
    assert!(
        !environment
            .database
            .project_state(PROJECT_ID)
            .await
            .unwrap()
            .blocked
    );
}

#[tokio::test]
async fn failed_rollback_blocks_project_until_unblock() {
    let environment = setup().await;
    let first_sha = commit_and_push_tag(&environment.repository, "deploy-20260901-120003", "v1");
    let first = queue(
        &environment,
        "event:block-first",
        "deploy-20260901-120003",
        &first_sha,
    )
    .await;
    environment.deployer.process(&first).await.unwrap();

    environment
        .fail_all_health_checks
        .store(true, Ordering::SeqCst);
    let second_sha = commit_and_push_tag(&environment.repository, "deploy-20260901-120004", "v2");
    let second = queue(
        &environment,
        "event:block-second",
        "deploy-20260901-120004",
        &second_sha,
    )
    .await;
    environment.deployer.process(&second).await.unwrap();

    let stored = environment
        .database
        .get_deployment(&second.id)
        .await
        .unwrap();
    assert_eq!(stored.status, DeploymentStatus::RollbackFailed);
    assert!(
        environment
            .database
            .project_state(PROJECT_ID)
            .await
            .unwrap()
            .blocked
    );
    environment
        .database
        .unblock_project(PROJECT_ID)
        .await
        .unwrap();
    assert!(
        !environment
            .database
            .project_state(PROJECT_ID)
            .await
            .unwrap()
            .blocked
    );
}

#[tokio::test]
async fn activation_stage_is_recovered_after_restart() {
    let environment = setup().await;
    let commit_sha = "1111111111111111111111111111111111111111";
    let release_path = environment
        .config
        .global
        .state_directory
        .join("projects")
        .join(PROJECT_ID)
        .join("releases")
        .join(commit_sha);
    fs::create_dir_all(&release_path).unwrap();
    fs::write(release_path.join("index.html"), "v1").unwrap();
    fs::create_dir_all(environment.current_path.parent().unwrap()).unwrap();
    symlink(&release_path, &environment.current_path).unwrap();
    let created = environment
        .database
        .create_webhook_deployment(
            PROJECT_ID,
            "event:restart",
            "deploy-20260901-120005",
            commit_sha,
        )
        .await
        .unwrap();
    environment
        .database
        .set_resolved_commit(&created.deployment.id, commit_sha)
        .await
        .unwrap();
    environment
        .database
        .set_release(&created.deployment.id, &release_path)
        .await
        .unwrap();
    environment
        .database
        .set_activation(&created.deployment.id, None)
        .await
        .unwrap();
    let interrupted = environment
        .database
        .get_deployment(&created.deployment.id)
        .await
        .unwrap();

    environment.deployer.process(&interrupted).await.unwrap();
    assert_eq!(
        environment
            .database
            .get_deployment(&created.deployment.id)
            .await
            .unwrap()
            .status,
        DeploymentStatus::Succeeded
    );
}

#[tokio::test]
async fn fetch_and_build_failures_are_persisted_and_cleaned() {
    let environment = setup().await;
    let missing_tag = queue(
        &environment,
        "event:missing-tag",
        "deploy-20260901-120007",
        "2222222222222222222222222222222222222222",
    )
    .await;
    let fetch_error = environment
        .deployer
        .process(&missing_tag)
        .await
        .expect_err("missing remote Tag must fail");
    environment
        .deployer
        .handle_processing_error(&missing_tag.id, fetch_error)
        .await
        .unwrap();
    assert_eq!(
        environment
            .database
            .get_deployment(&missing_tag.id)
            .await
            .unwrap()
            .status,
        DeploymentStatus::Failed
    );

    let commit_sha = commit_without_entry(&environment.repository, "deploy-20260901-120008");
    let build_failure = queue(
        &environment,
        "event:missing-entry",
        "deploy-20260901-120008",
        &commit_sha,
    )
    .await;
    let build_error = environment
        .deployer
        .process(&build_failure)
        .await
        .expect_err("missing static entry must fail");
    environment
        .deployer
        .handle_processing_error(&build_failure.id, build_error)
        .await
        .unwrap();
    assert_eq!(
        environment
            .database
            .get_deployment(&build_failure.id)
            .await
            .unwrap()
            .status,
        DeploymentStatus::Failed
    );
    let release_root = environment
        .config
        .global
        .state_directory
        .join("projects")
        .join(PROJECT_ID)
        .join("releases");
    let entries = fs::read_dir(release_root)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        entries.is_empty(),
        "failed release directories must be removed"
    );
    assert!(
        !environment
            .config
            .global
            .state_directory
            .join("worktrees")
            .join(&build_failure.id)
            .exists()
    );
}

#[tokio::test]
async fn fetched_tag_object_must_equal_webhook_after() {
    let environment = setup().await;
    let commit_sha = commit_and_push_tag(&environment.repository, "deploy-20260901-120009", "v1");
    assert_ne!(commit_sha, "3333333333333333333333333333333333333333");
    let deployment = queue(
        &environment,
        "event:wrong-after",
        "deploy-20260901-120009",
        "3333333333333333333333333333333333333333",
    )
    .await;
    let source = environment
        .deployer
        .process(&deployment)
        .await
        .expect_err("mismatched WebHook after must fail");
    assert_eq!(source.code(), "requestInvalid");
}

#[tokio::test]
async fn webhook_authentication_filtering_and_deduplication_are_strict() {
    let environment = setup().await;
    let commit_sha = commit_and_push_tag(&environment.repository, "deploy-20260901-120006", "v1");
    let app = webhook_app(&environment);
    let payload = webhook_payload(
        REPOSITORY_FULL_NAME,
        "refs/tags/deploy-20260901-120006",
        &commit_sha,
        true,
        false,
    );

    let first = send_webhook(app.clone(), TOKEN, payload.clone()).await;
    assert_eq!(first.0, StatusCode::ACCEPTED);
    assert_eq!(first.1["accepted"], true);
    let first_id = first.1["deployment"]["id"].as_str().unwrap().to_owned();

    let duplicate = send_webhook(app.clone(), TOKEN, payload).await;
    assert_eq!(duplicate.0, StatusCode::OK);
    assert_eq!(duplicate.1["duplicate"], true);
    assert_eq!(duplicate.1["deployment"]["id"], first_id);

    let wrong_token = send_webhook(
        app.clone(),
        "wrong-token",
        webhook_payload(
            REPOSITORY_FULL_NAME,
            "refs/tags/deploy-20260901-120006",
            &commit_sha,
            true,
            false,
        ),
    )
    .await;
    assert_eq!(wrong_token.0, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong_token.1["error"]["code"], "authenticationFailed");

    let wrong_repository = send_webhook(
        app.clone(),
        TOKEN,
        webhook_payload(
            "aynurcn/other",
            "refs/tags/deploy-20260901-120006",
            &commit_sha,
            true,
            false,
        ),
    )
    .await;
    assert_eq!(wrong_repository.0, StatusCode::BAD_REQUEST);

    let ignored = send_webhook(
        app.clone(),
        TOKEN,
        webhook_payload(
            REPOSITORY_FULL_NAME,
            "refs/tags/release-1",
            &commit_sha,
            true,
            false,
        ),
    )
    .await;
    assert_eq!(ignored.0, StatusCode::OK);
    assert_eq!(ignored.1["reason"], "tagPatternMismatch");

    let deleted = send_webhook(
        app,
        TOKEN,
        webhook_payload(
            REPOSITORY_FULL_NAME,
            "refs/tags/deploy-20260901-120006",
            "0000000000000000000000000000000000000000",
            false,
            true,
        ),
    )
    .await;
    assert_eq!(deleted.0, StatusCode::OK);
    assert_eq!(deleted.1["reason"], "tagDeleted");
}

async fn setup() -> TestEnvironment {
    let temporary_directory = tempfile::tempdir().unwrap();
    let root = temporary_directory.path();
    let repository = create_repository(root);
    let state_directory = root.join("state");
    let current_path = root.join("published/current");
    let fail_all = Arc::new(AtomicBool::new(false));
    let (health_url, health_server) = start_health_server(HealthState {
        current_path: current_path.clone(),
        fail_all: fail_all.clone(),
    })
    .await;
    let config_path = write_config(
        root,
        &repository.origin,
        &state_directory,
        &current_path,
        &health_url,
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
        config,
        database,
        deployer,
        repository,
        current_path,
        fail_all_health_checks: fail_all,
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

fn commit_and_push_tag(repository: &Repository, tag: &str, content: &str) -> String {
    fs::write(repository.work.join("index.html"), content).unwrap();
    fs::write(repository.work.join("verification.txt"), "verification").unwrap();
    run_git(&repository.work, &["add", "index.html", "verification.txt"]);
    run_git(&repository.work, &["commit", "-m", tag]);
    run_git(&repository.work, &["tag", tag]);
    run_git(
        &repository.work,
        &["push", "origin", &format!("refs/tags/{tag}")],
    );
    git_output(&repository.work, &["rev-parse", "HEAD"])
}

fn commit_without_entry(repository: &Repository, tag: &str) -> String {
    if repository.work.join("index.html").exists() {
        run_git(&repository.work, &["rm", "index.html"]);
    } else {
        fs::write(repository.work.join("metadata.txt"), tag).unwrap();
        run_git(&repository.work, &["add", "metadata.txt"]);
    }
    run_git(&repository.work, &["commit", "-m", tag]);
    run_git(&repository.work, &["tag", tag]);
    run_git(
        &repository.work,
        &["push", "origin", &format!("refs/tags/{tag}")],
    );
    git_output(&repository.work, &["rev-parse", "HEAD"])
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

fn write_config(
    root: &Path,
    repository_url: &Path,
    state_directory: &Path,
    current_path: &Path,
    health_url: &str,
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
cargoCommand = "/usr/bin/git"
gitFetchAttempts = 2
gitFetchRetryDelayMs = 1
commandTimeoutSeconds = 30
maxWebhookBodyBytes = 65536
workerPollIntervalMs = 10
"#,
            root.join("deployments.sqlite3").display(),
            state_directory.display(),
            projects_directory.display(),
        ),
    )
    .unwrap();
    fs::write(
        projects_directory.join("test-static.toml"),
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
attempts = 2
intervalMs = 1
timeoutMs = 1000

[deployment]
type = "static"
entryFile = "index.html"
"#,
            current_path.display(),
            repository_url.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(
        projects_directory.join("test-static.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    config_path
}

async fn start_health_server(state: HealthState) -> (String, JoinHandle<()>) {
    let app = Router::new()
        .route("/health", get(health_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}/health"), server)
}

async fn health_handler(State(state): State<HealthState>) -> StatusCode {
    if state.fail_all.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    let current = match fs::read_link(&state.current_path) {
        Ok(value) => value,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
    };
    match fs::read_to_string(current.join("index.html")) {
        Ok(content) if content != "unhealthy" => StatusCode::OK,
        Ok(_) | Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
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

fn webhook_app(environment: &TestEnvironment) -> Router {
    router(AppState {
        config: environment.config.clone(),
        database: environment.database.clone(),
        notifier: Arc::new(Notify::new()),
    })
}

fn webhook_payload(
    repository: &str,
    tag_ref: &str,
    after: &str,
    created: bool,
    deleted: bool,
) -> Value {
    serde_json::json!({
        "hook_name": "tag_push_hooks",
        "created": created,
        "deleted": deleted,
        "ref": tag_ref,
        "after": after,
        "repository": { "full_name": repository },
        "unrelated": "ignored"
    })
}

async fn send_webhook(app: Router, token: &str, payload: Value) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::post(format!("/v1/hooks/gitee/{PROJECT_ID}"))
                .header("content-type", "application/json")
                .header("x-gitee-token", token)
                .header("x-gitee-ping", "false")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap();
    (status, body)
}
