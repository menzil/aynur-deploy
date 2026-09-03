use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::{Notify, watch};
use tracing::{error, info, warn};
use walkdir::WalkDir;

use crate::config::{
    ConfiguredCommand, DeploymentTarget, LoadedConfig, Project, RustBinary, load_environment_file,
};
use crate::db::Database;
use crate::error::{AppError, file_error};
use crate::model::{Deployment, DeploymentKind, DeploymentStatus};
use crate::process::{CommandSpec, run_command};

#[derive(Clone)]
pub struct Deployer {
    config: Arc<LoadedConfig>,
    database: Database,
    http_client: Client,
}

struct RustReleaseSpec<'a> {
    cargo_manifest: &'a Path,
    binaries: &'a [RustBinary],
    include_paths: &'a [PathBuf],
    environment_file: Option<&'a Path>,
}

impl Deployer {
    pub fn new(config: Arc<LoadedConfig>, database: Database) -> Result<Self, AppError> {
        let http_client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|source| AppError::HealthCheck {
                url: "client initialization".to_owned(),
                reason: source.to_string(),
            })?;
        Ok(Self {
            config,
            database,
            http_client,
        })
    }

    pub async fn process(&self, deployment: &Deployment) -> Result<(), AppError> {
        let project = self.config.project(&deployment.project_id)?;
        info!(
            projectId = %deployment.project_id,
            deploymentId = %deployment.id,
            tag = %deployment.tag,
            commitSha = deployment.commit_sha.as_deref().unwrap_or(""),
            stage = %deployment.status,
            "processing deployment"
        );
        match deployment.status {
            DeploymentStatus::Queued | DeploymentStatus::Fetching | DeploymentStatus::Building => {
                let release_path = match deployment.kind {
                    DeploymentKind::Deploy => {
                        let release_path = self.prepare_release(project, deployment).await?;
                        self.migrate_if_required(project, deployment, &release_path)
                            .await?;
                        release_path
                    }
                    DeploymentKind::Rollback => {
                        deployment
                            .release_path
                            .clone()
                            .ok_or_else(|| AppError::InvalidState {
                                reason: format!(
                                    "rollback deployment {} has no releasePath",
                                    deployment.id
                                ),
                            })?
                    }
                };
                self.activate(project, deployment, &release_path).await
            }
            DeploymentStatus::Migrating => {
                if deployment.kind != DeploymentKind::Deploy {
                    return Err(AppError::InvalidState {
                        reason: format!(
                            "rollback deployment {} cannot be in migrating state",
                            deployment.id
                        ),
                    });
                }
                let release_path =
                    deployment
                        .release_path
                        .as_deref()
                        .ok_or_else(|| AppError::InvalidState {
                            reason: format!(
                                "migrating deployment {} has no releasePath",
                                deployment.id
                            ),
                        })?;
                validate_release(project, release_path)?;
                let migration =
                    project
                        .migration
                        .as_ref()
                        .ok_or_else(|| AppError::InvalidState {
                            reason: format!(
                                "migrating deployment {} requires migration configuration",
                                deployment.id
                            ),
                        })?;
                self.run_migration(project, deployment, release_path, migration)
                    .await?;
                self.activate(project, deployment, release_path).await
            }
            DeploymentStatus::Activating | DeploymentStatus::HealthChecking => {
                self.recover_after_activation(
                    project,
                    deployment,
                    "service restarted during activation",
                )
                .await
            }
            DeploymentStatus::RollingBack => self.recover_rollback(project, deployment).await,
            DeploymentStatus::Succeeded
            | DeploymentStatus::Failed
            | DeploymentStatus::RollbackFailed => Err(AppError::InvalidState {
                reason: format!(
                    "terminal deployment {} with status {} was selected for processing",
                    deployment.id, deployment.status
                ),
            }),
        }
    }

    pub async fn handle_processing_error(
        &self,
        deployment_id: &str,
        source: AppError,
    ) -> Result<(), AppError> {
        let deployment = self.database.get_deployment(deployment_id).await?;
        error!(
            projectId = %deployment.project_id,
            deploymentId = %deployment.id,
            tag = %deployment.tag,
            commitSha = deployment.commit_sha.as_deref().unwrap_or(""),
            stage = %deployment.status,
            errorCode = source.code(),
            error = %source,
            "deployment stage failed"
        );
        match deployment.status {
            DeploymentStatus::Queued
            | DeploymentStatus::Fetching
            | DeploymentStatus::Building
            | DeploymentStatus::Migrating => {
                self.database
                    .mark_failed(&deployment.id, source.code(), &source.to_string())
                    .await
            }
            DeploymentStatus::Activating | DeploymentStatus::HealthChecking => {
                let project = self.config.project(&deployment.project_id)?;
                self.recover_after_activation(project, &deployment, &source.to_string())
                    .await
            }
            DeploymentStatus::RollingBack => {
                let project = self.config.project(&deployment.project_id)?;
                self.recover_rollback(project, &deployment).await
            }
            DeploymentStatus::Succeeded
            | DeploymentStatus::Failed
            | DeploymentStatus::RollbackFailed => Ok(()),
        }
    }

    async fn prepare_release(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<PathBuf, AppError> {
        self.database
            .set_status(&deployment.id, DeploymentStatus::Fetching)
            .await?;
        let mirror_path = self.mirror_path(project);
        let commit_sha = self
            .fetch_exact_tag(project, deployment, &mirror_path)
            .await?;
        self.database
            .set_resolved_commit(&deployment.id, &commit_sha)
            .await?;

        let release_path = self.release_root(project).join(&commit_sha);
        let worktree_path = self.worktree_path(deployment);
        if release_path.exists() {
            if worktree_path.exists() {
                self.remove_worktree(&mirror_path, &worktree_path).await?;
            }
            validate_release(project, &release_path)?;
            self.database
                .set_release(&deployment.id, &release_path)
                .await?;
            return Ok(release_path);
        }

        self.database
            .set_status(&deployment.id, DeploymentStatus::Building)
            .await?;
        self.add_worktree(&mirror_path, &worktree_path, &commit_sha)
            .await?;
        let build_result = self
            .build_release(project, deployment, &worktree_path, &release_path)
            .await;
        let cleanup_result = self.remove_worktree(&mirror_path, &worktree_path).await;
        match (build_result, cleanup_result) {
            (Ok(()), Ok(())) => {}
            (Err(build_error), Ok(())) => return Err(build_error),
            (Ok(()), Err(cleanup_error)) => return Err(cleanup_error),
            (Err(build_error), Err(cleanup_error)) => {
                return Err(AppError::InvalidState {
                    reason: format!(
                        "build failed with {build_error}; worktree cleanup also failed with {cleanup_error}"
                    ),
                });
            }
        }
        self.database
            .set_release(&deployment.id, &release_path)
            .await?;
        Ok(release_path)
    }

    async fn fetch_exact_tag(
        &self,
        project: &Project,
        deployment: &Deployment,
        mirror_path: &Path,
    ) -> Result<String, AppError> {
        let mirror_parent = mirror_path.parent().ok_or_else(|| AppError::InvalidState {
            reason: format!("mirror path {mirror_path:?} has no parent"),
        })?;
        tokio::fs::create_dir_all(mirror_parent)
            .await
            .map_err(|source| {
                file_error("create directory", mirror_parent.to_path_buf(), source)
            })?;
        if mirror_path.exists() {
            let remote = self
                .git(
                    vec![
                        os("--git-dir"),
                        mirror_path.as_os_str().to_owned(),
                        os("remote"),
                        os("get-url"),
                        os("origin"),
                    ],
                    None,
                )
                .await?;
            if remote.stdout != project.repository_url {
                return Err(AppError::InvalidState {
                    reason: format!(
                        "mirror {mirror_path:?} origin is {:?}, expected {:?}",
                        remote.stdout, project.repository_url
                    ),
                });
            }
        } else {
            self.git(
                vec![os("init"), os("--bare"), mirror_path.as_os_str().to_owned()],
                None,
            )
            .await?;
            self.git(
                vec![
                    os("--git-dir"),
                    mirror_path.as_os_str().to_owned(),
                    os("remote"),
                    os("add"),
                    os("origin"),
                    OsString::from(&project.repository_url),
                ],
                None,
            )
            .await?;
        }

        let tag_ref = format!("refs/tags/{}", deployment.tag);
        self.git(vec![os("check-ref-format"), OsString::from(&tag_ref)], None)
            .await?;
        let refspec = format!("{tag_ref}:{tag_ref}");
        let mut last_error: Option<AppError> = None;
        for attempt in 1..=self.config.global.git_fetch_attempts {
            let result = self
                .git(
                    vec![
                        os("--git-dir"),
                        mirror_path.as_os_str().to_owned(),
                        os("fetch"),
                        os("--no-tags"),
                        os("origin"),
                        OsString::from(&refspec),
                    ],
                    Some(vec![(os("GIT_TERMINAL_PROMPT"), os("0"))]),
                )
                .await;
            match result {
                Ok(_) => {
                    last_error = None;
                    break;
                }
                Err(source) => {
                    warn!(
                        projectId = %deployment.project_id,
                        deploymentId = %deployment.id,
                        tag = %deployment.tag,
                        commitSha = %deployment.requested_sha,
                        stage = "fetching",
                        attempt,
                        attempts = self.config.global.git_fetch_attempts,
                        errorCode = source.code(),
                        error = %source,
                        "Git fetch attempt failed"
                    );
                    last_error = Some(source);
                    if attempt < self.config.global.git_fetch_attempts {
                        tokio::time::sleep(Duration::from_millis(
                            self.config.global.git_fetch_retry_delay_ms,
                        ))
                        .await;
                    }
                }
            }
        }
        if let Some(source) = last_error {
            return Err(source);
        }

        let tag_object = self
            .git(
                vec![
                    os("--git-dir"),
                    mirror_path.as_os_str().to_owned(),
                    os("rev-parse"),
                    os("--verify"),
                    OsString::from(&tag_ref),
                ],
                None,
            )
            .await?
            .stdout;
        if tag_object != deployment.requested_sha {
            return Err(AppError::RequestValidation {
                reason: format!(
                    "fetched tag object {tag_object} does not match webhook after {} for tag {}",
                    deployment.requested_sha, deployment.tag
                ),
            });
        }
        let commit_ref = format!("{tag_ref}^{{commit}}");
        let commit_sha = self
            .git(
                vec![
                    os("--git-dir"),
                    mirror_path.as_os_str().to_owned(),
                    os("rev-parse"),
                    os("--verify"),
                    OsString::from(commit_ref),
                ],
                None,
            )
            .await?
            .stdout;
        validate_sha(&commit_sha)?;
        Ok(commit_sha)
    }

    async fn add_worktree(
        &self,
        mirror_path: &Path,
        worktree_path: &Path,
        commit_sha: &str,
    ) -> Result<(), AppError> {
        self.git(
            vec![
                os("--git-dir"),
                mirror_path.as_os_str().to_owned(),
                os("worktree"),
                os("prune"),
            ],
            None,
        )
        .await?;
        if worktree_path.exists() {
            self.remove_worktree(mirror_path, worktree_path).await?;
        }
        let parent = worktree_path
            .parent()
            .ok_or_else(|| AppError::InvalidState {
                reason: format!("worktree path {worktree_path:?} has no parent"),
            })?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| file_error("create directory", parent.to_path_buf(), source))?;
        self.git(
            vec![
                os("--git-dir"),
                mirror_path.as_os_str().to_owned(),
                os("worktree"),
                os("add"),
                os("--detach"),
                worktree_path.as_os_str().to_owned(),
                OsString::from(commit_sha),
            ],
            None,
        )
        .await?;
        Ok(())
    }

    async fn remove_worktree(
        &self,
        mirror_path: &Path,
        worktree_path: &Path,
    ) -> Result<(), AppError> {
        let result = self
            .git(
                vec![
                    os("--git-dir"),
                    mirror_path.as_os_str().to_owned(),
                    os("worktree"),
                    os("remove"),
                    os("--force"),
                    worktree_path.as_os_str().to_owned(),
                ],
                None,
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(source) if !worktree_path.exists() => Err(source),
            Err(source) => Err(AppError::InvalidState {
                reason: format!("Git could not remove worktree {worktree_path:?}: {source}"),
            }),
        }
    }

    async fn build_release(
        &self,
        project: &Project,
        deployment: &Deployment,
        worktree_path: &Path,
        release_path: &Path,
    ) -> Result<(), AppError> {
        let release_parent = release_path
            .parent()
            .ok_or_else(|| AppError::InvalidState {
                reason: format!("release path {release_path:?} has no parent"),
            })?;
        tokio::fs::create_dir_all(release_parent)
            .await
            .map_err(|source| {
                file_error("create directory", release_parent.to_path_buf(), source)
            })?;
        let temporary_release = release_parent.join(format!(
            ".{}.{}.tmp",
            deployment.requested_sha, deployment.id
        ));
        remove_directory_if_exists(&temporary_release)?;
        fs::create_dir(&temporary_release)
            .map_err(|source| file_error("create directory", temporary_release.clone(), source))?;

        let result = match &project.deployment {
            DeploymentTarget::Static { .. } => copy_static_tree(worktree_path, &temporary_release),
            DeploymentTarget::Binary { binary_path } => {
                copy_prebuilt_binary(worktree_path, &temporary_release, binary_path)
            }
            DeploymentTarget::Rust {
                cargo_manifest,
                binaries,
                include_paths,
                environment_file,
            } => {
                self.build_rust_release(
                    deployment,
                    worktree_path,
                    &temporary_release,
                    RustReleaseSpec {
                        cargo_manifest,
                        binaries,
                        include_paths,
                        environment_file: environment_file.as_deref(),
                    },
                )
                .await
            }
        };
        if let Err(source) = result {
            return Err(cleanup_failed_release(source, &temporary_release));
        }
        if let Err(source) = validate_release(project, &temporary_release) {
            return Err(cleanup_failed_release(source, &temporary_release));
        }
        if let Err(source) = fs::rename(&temporary_release, release_path)
            .map_err(|source| file_error("rename", temporary_release.clone(), source))
        {
            return Err(cleanup_failed_release(source, &temporary_release));
        }
        if let Err(source) = sync_directory(release_parent) {
            return Err(cleanup_failed_release(source, release_path));
        }
        Ok(())
    }

    async fn build_rust_release(
        &self,
        deployment: &Deployment,
        worktree_path: &Path,
        temporary_release: &Path,
        release_spec: RustReleaseSpec<'_>,
    ) -> Result<(), AppError> {
        let manifest_path = worktree_path.join(release_spec.cargo_manifest);
        if !manifest_path.is_file() {
            return Err(AppError::InvalidState {
                reason: format!("Cargo manifest {manifest_path:?} does not exist"),
            });
        }
        let target_directory = self
            .config
            .global
            .state_directory
            .join("targets")
            .join(&deployment.id);
        remove_directory_if_exists(&target_directory)?;
        fs::create_dir_all(&target_directory)
            .map_err(|source| file_error("create directory", target_directory.clone(), source))?;
        let mut args = vec![
            os("build"),
            os("--release"),
            os("--locked"),
            os("--manifest-path"),
            manifest_path.as_os_str().to_owned(),
        ];
        for binary in release_spec.binaries {
            args.extend([
                os("--package"),
                OsString::from(&binary.package),
                os("--bin"),
                OsString::from(&binary.binary),
            ]);
        }
        let mut environment = load_optional_environment_file(release_spec.environment_file)?;
        environment.push((
            os("CARGO_TARGET_DIR"),
            target_directory.as_os_str().to_owned(),
        ));
        let spec = CommandSpec {
            program: self.config.global.cargo_command.clone(),
            args,
            current_directory: Some(worktree_path.to_path_buf()),
            environment,
        };
        let build_and_copy_result = async {
            run_command(&spec, self.config.global.command_timeout_seconds).await?;
            for binary in release_spec.binaries {
                copy_rust_binary(&target_directory, temporary_release, binary)?;
            }
            for include_path in release_spec.include_paths {
                copy_release_path(worktree_path, temporary_release, include_path)?;
            }
            Ok(())
        }
        .await;
        let cleanup_result = remove_directory_if_exists(&target_directory);
        match (build_and_copy_result, cleanup_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(source), Ok(())) => Err(source),
            (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
            (Err(source), Err(cleanup_error)) => Err(AppError::InvalidState {
                reason: format!(
                    "Cargo build failed with {source}; target cleanup also failed with {cleanup_error}"
                ),
            }),
        }
    }

    async fn migrate_if_required(
        &self,
        project: &Project,
        deployment: &Deployment,
        release_path: &Path,
    ) -> Result<(), AppError> {
        let Some(migration) = &project.migration else {
            return Ok(());
        };
        self.run_migration(project, deployment, release_path, migration)
            .await
    }

    async fn run_migration(
        &self,
        project: &Project,
        deployment: &Deployment,
        release_path: &Path,
        migration: &ConfiguredCommand,
    ) -> Result<(), AppError> {
        self.database
            .set_status(&deployment.id, DeploymentStatus::Migrating)
            .await?;
        let environment_file = match &project.deployment {
            DeploymentTarget::Rust {
                environment_file, ..
            } => environment_file.as_deref(),
            DeploymentTarget::Static { .. } | DeploymentTarget::Binary { .. } => None,
        };
        let spec = command_spec(
            migration,
            Some(release_path.to_path_buf()),
            load_optional_environment_file(environment_file)?,
        );
        run_command(&spec, self.config.global.command_timeout_seconds).await?;
        info!(
            projectId = %deployment.project_id,
            deploymentId = %deployment.id,
            tag = %deployment.tag,
            commitSha = deployment.commit_sha.as_deref().unwrap_or(&deployment.requested_sha),
            stage = "migrating",
            migrationProgram = %migration.program.display(),
            "migration command completed"
        );
        Ok(())
    }

    async fn activate(
        &self,
        project: &Project,
        deployment: &Deployment,
        release_path: &Path,
    ) -> Result<(), AppError> {
        validate_release(project, release_path)?;
        let current_path = self.current_path(project);
        let previous = read_current_target(&current_path)?;
        self.database
            .set_activation(&deployment.id, previous.as_deref())
            .await?;
        switch_current(&current_path, release_path, &deployment.id)?;
        if let Err(source) = self.reload_if_required(project, deployment).await {
            return self
                .rollback_after_activation(project, deployment, previous.as_deref(), &source)
                .await;
        }
        self.database
            .set_status(&deployment.id, DeploymentStatus::HealthChecking)
            .await?;
        if let Err(source) = self.health_check(project, deployment).await {
            return self
                .rollback_after_activation(project, deployment, previous.as_deref(), &source)
                .await;
        }
        self.database.mark_succeeded(&deployment.id).await?;
        self.cleanup_retention(project).await?;
        info!(
            projectId = %deployment.project_id,
            deploymentId = %deployment.id,
            tag = %deployment.tag,
            commitSha = deployment.commit_sha.as_deref().unwrap_or(&deployment.requested_sha),
            stage = "succeeded",
            "deployment succeeded"
        );
        Ok(())
    }

    async fn recover_after_activation(
        &self,
        project: &Project,
        deployment: &Deployment,
        original_error: &str,
    ) -> Result<(), AppError> {
        let release_path =
            deployment
                .release_path
                .as_deref()
                .ok_or_else(|| AppError::InvalidState {
                    reason: format!(
                        "deployment {} has no releasePath during recovery",
                        deployment.id
                    ),
                })?;
        let current_path = self.current_path(project);
        let current = read_current_target(&current_path)?;
        if current.as_deref() == Some(release_path) {
            if let Err(source) = self.reload_if_required(project, deployment).await {
                return self
                    .rollback_after_activation(
                        project,
                        deployment,
                        deployment.previous_release_path.as_deref(),
                        &source,
                    )
                    .await;
            }
            self.database
                .set_status(&deployment.id, DeploymentStatus::HealthChecking)
                .await?;
            match self.health_check(project, deployment).await {
                Ok(()) => {
                    self.database.mark_succeeded(&deployment.id).await?;
                    self.cleanup_retention(project).await?;
                    return Ok(());
                }
                Err(source) => {
                    return self
                        .rollback_after_activation(
                            project,
                            deployment,
                            deployment.previous_release_path.as_deref(),
                            &source,
                        )
                        .await;
                }
            }
        }
        if current.as_deref() == deployment.previous_release_path.as_deref()
            || (current.is_none() && deployment.previous_release_path.is_none())
        {
            self.database
                .mark_failed(&deployment.id, "activationInterrupted", original_error)
                .await?;
            return Ok(());
        }
        let source = AppError::InvalidState {
            reason: format!(
                "current symlink {current:?} points to neither release {release_path:?} nor previous {:?}",
                deployment.previous_release_path
            ),
        };
        self.rollback_after_activation(
            project,
            deployment,
            deployment.previous_release_path.as_deref(),
            &source,
        )
        .await
    }

    async fn rollback_after_activation(
        &self,
        project: &Project,
        deployment: &Deployment,
        previous: Option<&Path>,
        original_error: &AppError,
    ) -> Result<(), AppError> {
        self.database
            .set_status(&deployment.id, DeploymentStatus::RollingBack)
            .await?;
        let Some(previous_path) = previous else {
            let reason = format!(
                "deployment failed with {original_error}; no previous release exists for rollback"
            );
            self.lock_after_rollback_failure(project, deployment, &reason)
                .await?;
            return Ok(());
        };
        let rollback_result = async {
            validate_release(project, previous_path)?;
            switch_current(&self.current_path(project), previous_path, &deployment.id)?;
            self.reload_if_required(project, deployment).await?;
            self.health_check(project, deployment).await
        }
        .await;
        match rollback_result {
            Ok(()) => {
                let message =
                    format!("deployment failed and was rolled back successfully: {original_error}");
                self.database
                    .mark_failed(&deployment.id, original_error.code(), &message)
                    .await?;
                warn!(
                    projectId = %deployment.project_id,
                    deploymentId = %deployment.id,
                    tag = %deployment.tag,
                    commitSha = deployment.commit_sha.as_deref().unwrap_or(&deployment.requested_sha),
                    stage = "failed",
                    errorCode = original_error.code(),
                    error = %original_error,
                    "deployment failed and rollback succeeded"
                );
            }
            Err(rollback_error) => {
                let reason = format!(
                    "deployment failed with {original_error}; rollback also failed with {rollback_error}"
                );
                self.lock_after_rollback_failure(project, deployment, &reason)
                    .await?;
            }
        }
        Ok(())
    }

    async fn recover_rollback(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<(), AppError> {
        let Some(previous_path) = deployment.previous_release_path.as_deref() else {
            let reason = "service restarted during rollback, but no previous release is recorded";
            self.lock_after_rollback_failure(project, deployment, reason)
                .await?;
            return Ok(());
        };
        let recovery = async {
            if read_current_target(&self.current_path(project))?.as_deref() != Some(previous_path) {
                switch_current(&self.current_path(project), previous_path, &deployment.id)?;
            }
            self.reload_if_required(project, deployment).await?;
            self.health_check(project, deployment).await
        }
        .await;
        match recovery {
            Ok(()) => {
                self.database
                    .mark_failed(
                        &deployment.id,
                        "deploymentInterrupted",
                        "service restarted during deployment; rollback completed successfully",
                    )
                    .await?;
            }
            Err(source) => {
                let reason = format!("rollback recovery failed: {source}");
                self.lock_after_rollback_failure(project, deployment, &reason)
                    .await?;
            }
        }
        Ok(())
    }

    async fn lock_after_rollback_failure(
        &self,
        project: &Project,
        deployment: &Deployment,
        reason: &str,
    ) -> Result<(), AppError> {
        self.database
            .mark_rollback_failed(&deployment.id, "rollbackFailed", reason)
            .await?;
        self.database
            .block_project(&project.project_id, reason)
            .await?;
        error!(
            projectId = %deployment.project_id,
            deploymentId = %deployment.id,
            tag = %deployment.tag,
            commitSha = deployment.commit_sha.as_deref().unwrap_or(&deployment.requested_sha),
            stage = "rollbackFailed",
            errorCode = "rollbackFailed",
            error = reason,
            "rollback failed and project was blocked"
        );
        Ok(())
    }

    async fn reload_if_required(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<(), AppError> {
        for (index, reload) in project.reload.iter().enumerate() {
            let spec = command_spec(reload, None, Vec::new());
            run_command(&spec, self.config.global.command_timeout_seconds).await?;
            info!(
                projectId = %deployment.project_id,
                deploymentId = %deployment.id,
                tag = %deployment.tag,
                commitSha = deployment.commit_sha.as_deref().unwrap_or(&deployment.requested_sha),
                stage = "reload",
                reloadIndex = index,
                reloadProgram = %reload.program.display(),
                "reload command completed"
            );
        }
        Ok(())
    }

    async fn health_check(
        &self,
        project: &Project,
        deployment: &Deployment,
    ) -> Result<(), AppError> {
        let mut last_error: Option<AppError> = None;
        for attempt in 1..=project.health_check.attempts {
            let result = self
                .single_health_check(
                    project.health_check.url.clone(),
                    project.health_check.timeout_ms,
                )
                .await;
            match result {
                Ok(()) => return Ok(()),
                Err(source) => {
                    warn!(
                        projectId = %deployment.project_id,
                        deploymentId = %deployment.id,
                        tag = %deployment.tag,
                        commitSha = deployment.commit_sha.as_deref().unwrap_or(&deployment.requested_sha),
                        stage = "healthChecking",
                        attempt,
                        attempts = project.health_check.attempts,
                        errorCode = source.code(),
                        error = %source,
                        "health check attempt failed"
                    );
                    last_error = Some(source);
                    if attempt < project.health_check.attempts {
                        tokio::time::sleep(Duration::from_millis(project.health_check.interval_ms))
                            .await;
                    }
                }
            }
        }
        match last_error {
            Some(source) => Err(source),
            None => Err(AppError::InvalidState {
                reason: format!(
                    "health check for project {} completed without an attempt",
                    project.project_id
                ),
            }),
        }
    }

    async fn single_health_check(
        &self,
        url: reqwest::Url,
        timeout_ms: u64,
    ) -> Result<(), AppError> {
        let mut response = self
            .http_client
            .get(url.clone())
            .timeout(Duration::from_millis(timeout_ms))
            .send()
            .await
            .map_err(|source| AppError::HealthCheck {
                url: url.to_string(),
                reason: source.to_string(),
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response
            .chunk()
            .await
            .map_err(|source| AppError::HealthCheck {
                url: url.to_string(),
                reason: format!("status {status}; failed to read response body: {source}"),
            })?;
        let body = body.unwrap_or_default();
        let truncated = &body[..body.len().min(1024)];
        Err(AppError::HealthCheck {
            url: url.to_string(),
            reason: format!(
                "status {status}; responseBody={:?}",
                String::from_utf8_lossy(truncated)
            ),
        })
    }

    async fn cleanup_retention(&self, project: &Project) -> Result<(), AppError> {
        let release_root = self.release_root(project);
        let current = read_current_target(&self.current_path(project))?;
        let paths = self
            .database
            .retained_release_paths(&project.project_id)
            .await?;
        for old_path in paths.into_iter().skip(project.retain_releases) {
            let path = PathBuf::from(old_path);
            if current.as_deref() == Some(path.as_path()) {
                continue;
            }
            if path.parent() != Some(release_root.as_path()) {
                return Err(AppError::InvalidState {
                    reason: format!(
                        "refusing to remove retained release {path:?} outside {release_root:?}"
                    ),
                });
            }
            remove_directory_if_exists(&path)?;
        }
        Ok(())
    }

    async fn git(
        &self,
        args: Vec<OsString>,
        environment: Option<Vec<(OsString, OsString)>>,
    ) -> Result<crate::process::CommandOutput, AppError> {
        let spec = CommandSpec {
            program: self.config.global.git_command.clone(),
            args,
            current_directory: None,
            environment: environment.unwrap_or_default(),
        };
        run_command(&spec, self.config.global.command_timeout_seconds).await
    }

    fn mirror_path(&self, project: &Project) -> PathBuf {
        self.config
            .global
            .state_directory
            .join("mirrors")
            .join(format!("{}.git", project.project_id))
    }

    fn worktree_path(&self, deployment: &Deployment) -> PathBuf {
        self.config
            .global
            .state_directory
            .join("worktrees")
            .join(&deployment.id)
    }

    fn release_root(&self, project: &Project) -> PathBuf {
        self.config
            .global
            .state_directory
            .join("projects")
            .join(&project.project_id)
            .join("releases")
    }

    fn current_path(&self, project: &Project) -> PathBuf {
        project.current_path.clone()
    }
}

pub async fn run_worker(
    deployer: Deployer,
    notifier: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), AppError> {
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        if let Some(deployment) = deployer.database.next_pending().await? {
            let deployment_id = deployment.id.clone();
            if let Err(source) = deployer.process(&deployment).await {
                deployer
                    .handle_processing_error(&deployment_id, source)
                    .await?;
            }
            continue;
        }
        tokio::select! {
            _ = notifier.notified() => {}
            changed = shutdown.changed() => {
                changed.map_err(|source| AppError::TaskJoin {
                    reason: format!("worker shutdown channel closed unexpectedly: {source}"),
                })?;
            }
            _ = tokio::time::sleep(Duration::from_millis(
                deployer.config.global.worker_poll_interval_ms,
            )) => {}
        }
    }
}

fn copy_prebuilt_binary(
    source_root: &Path,
    destination_root: &Path,
    binary_path: &Path,
) -> Result<(), AppError> {
    let source_path = source_root.join(binary_path);
    let metadata = fs::symlink_metadata(&source_path)
        .map_err(|source| file_error("read metadata", source_path.clone(), source))?;
    if !metadata.file_type().is_file() {
        return Err(AppError::InvalidState {
            reason: format!("binary source {source_path:?} is not a regular file"),
        });
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(AppError::InvalidState {
            reason: format!("binary source {source_path:?} is not executable"),
        });
    }
    let destination_path = destination_root.join(binary_path);
    if let Some(parent) = destination_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|source| file_error("create directory", parent.to_path_buf(), source))?;
    }
    fs::copy(&source_path, &destination_path)
        .map_err(|source| file_error("copy", source_path.clone(), source))?;
    fs::set_permissions(&destination_path, metadata.permissions())
        .map_err(|source| file_error("set permissions", destination_path, source))?;
    Ok(())
}

fn copy_rust_binary(
    target_directory: &Path,
    destination_root: &Path,
    binary: &RustBinary,
) -> Result<(), AppError> {
    let source_binary = target_directory.join("release").join(&binary.binary);
    let metadata = fs::metadata(&source_binary)
        .map_err(|source| file_error("read metadata", source_binary.clone(), source))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(AppError::InvalidState {
            reason: format!(
                "Cargo build succeeded but expected executable binary {source_binary:?} does not exist"
            ),
        });
    }
    let destination_binary = destination_root.join(&binary.binary);
    fs::copy(&source_binary, &destination_binary)
        .map_err(|source| file_error("copy", source_binary, source))?;
    fs::set_permissions(&destination_binary, metadata.permissions())
        .map_err(|source| file_error("set permissions", destination_binary, source))?;
    Ok(())
}

fn copy_release_path(
    source_root: &Path,
    destination_root: &Path,
    relative_path: &Path,
) -> Result<(), AppError> {
    let source_path = source_root.join(relative_path);
    let destination_path = destination_root.join(relative_path);
    let metadata = fs::symlink_metadata(&source_path)
        .map_err(|source| file_error("read metadata", source_path.clone(), source))?;
    if metadata.file_type().is_file() {
        if let Some(parent) = destination_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| file_error("create directory", parent.to_path_buf(), source))?;
        }
        fs::copy(&source_path, &destination_path)
            .map_err(|source| file_error("copy", source_path.clone(), source))?;
        fs::set_permissions(&destination_path, metadata.permissions())
            .map_err(|source| file_error("set permissions", destination_path, source))?;
        return Ok(());
    }
    if metadata.file_type().is_dir() {
        fs::create_dir_all(&destination_path)
            .map_err(|source| file_error("create directory", destination_path.clone(), source))?;
        return copy_static_tree(&source_path, &destination_path);
    }
    Err(AppError::InvalidState {
        reason: format!(
            "included release path {source_path:?} must be a regular file or directory"
        ),
    })
}

fn copy_static_tree(source: &Path, destination: &Path) -> Result<(), AppError> {
    for entry_result in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
    {
        let entry = entry_result.map_err(|walk_error| AppError::InvalidState {
            reason: format!("walking static source {source:?} failed: {walk_error}"),
        })?;
        let relative =
            entry
                .path()
                .strip_prefix(source)
                .map_err(|strip_error| AppError::InvalidState {
                    reason: format!(
                        "could not make {:?} relative to {source:?}: {strip_error}",
                        entry.path(),
                    ),
                })?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir(&target)
                .map_err(|source| file_error("create directory", target, source))?;
        } else if entry.file_type().is_file() {
            fs::copy(entry.path(), &target)
                .map_err(|source| file_error("copy", entry.path().to_path_buf(), source))?;
            let permissions = entry
                .metadata()
                .map_err(|source| {
                    file_error("read metadata", entry.path().to_path_buf(), source.into())
                })?
                .permissions();
            fs::set_permissions(&target, permissions)
                .map_err(|source| file_error("set permissions", target, source))?;
        } else if entry.file_type().is_symlink() {
            let link_target = fs::read_link(entry.path())
                .map_err(|source| file_error("read symlink", entry.path().to_path_buf(), source))?;
            if link_target.is_absolute()
                || link_target
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
            {
                return Err(AppError::InvalidState {
                    reason: format!(
                        "static release symlink {:?} has unsafe target {link_target:?}",
                        entry.path()
                    ),
                });
            }
            symlink(&link_target, &target)
                .map_err(|source| file_error("create symlink", target, source))?;
        } else {
            return Err(AppError::InvalidState {
                reason: format!(
                    "unsupported file type in static release: {:?}",
                    entry.path()
                ),
            });
        }
    }
    Ok(())
}

fn validate_release(project: &Project, release_path: &Path) -> Result<(), AppError> {
    if !release_path.is_dir() {
        return Err(AppError::InvalidState {
            reason: format!("release path {release_path:?} is not a directory"),
        });
    }
    match &project.deployment {
        DeploymentTarget::Static { entry_file } => {
            validate_release_file(&release_path.join(entry_file), false)?;
        }
        DeploymentTarget::Binary { binary_path } => {
            validate_release_file(&release_path.join(binary_path), true)?;
        }
        DeploymentTarget::Rust {
            binaries,
            include_paths,
            ..
        } => {
            for binary in binaries {
                validate_release_file(&release_path.join(&binary.binary), true)?;
            }
            for include_path in include_paths {
                let path = release_path.join(include_path);
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|source| file_error("read metadata", path.clone(), source))?;
                if !metadata.file_type().is_file() && !metadata.file_type().is_dir() {
                    return Err(AppError::InvalidState {
                        reason: format!(
                            "included release path {path:?} is not a regular file or directory"
                        ),
                    });
                }
            }
        }
    }
    Ok(())
}

fn validate_release_file(path: &Path, executable: bool) -> Result<(), AppError> {
    let metadata = fs::metadata(path)
        .map_err(|source| file_error("read metadata", path.to_path_buf(), source))?;
    if !metadata.is_file() {
        return Err(AppError::InvalidState {
            reason: format!("release entry {path:?} is not a regular file"),
        });
    }
    if executable && metadata.permissions().mode() & 0o111 == 0 {
        return Err(AppError::InvalidState {
            reason: format!("release entry {path:?} is not executable"),
        });
    }
    Ok(())
}

fn command_spec(
    command: &ConfiguredCommand,
    current_directory: Option<PathBuf>,
    environment: Vec<(OsString, OsString)>,
) -> CommandSpec {
    CommandSpec {
        program: command.program.clone(),
        args: command.args.clone(),
        current_directory,
        environment,
    }
}

fn load_optional_environment_file(
    path: Option<&Path>,
) -> Result<Vec<(OsString, OsString)>, AppError> {
    match path {
        Some(path) => load_environment_file(path),
        None => Ok(Vec::new()),
    }
}

fn read_current_target(current_path: &Path) -> Result<Option<PathBuf>, AppError> {
    let metadata = match fs::symlink_metadata(current_path) {
        Ok(value) => value,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(file_error(
                "read symlink metadata",
                current_path.to_path_buf(),
                source,
            ));
        }
    };
    if !metadata.file_type().is_symlink() {
        return Err(AppError::InvalidState {
            reason: format!("current path {current_path:?} must be a symlink"),
        });
    }
    let target = fs::read_link(current_path)
        .map_err(|source| file_error("read symlink", current_path.to_path_buf(), source))?;
    if !target.is_absolute() {
        return Err(AppError::InvalidState {
            reason: format!("current symlink {current_path:?} has non-absolute target {target:?}"),
        });
    }
    Ok(Some(target))
}

fn switch_current(
    current_path: &Path,
    release_path: &Path,
    deployment_id: &str,
) -> Result<(), AppError> {
    if !release_path.is_absolute() {
        return Err(AppError::InvalidState {
            reason: format!("release path {release_path:?} must be absolute"),
        });
    }
    let parent = current_path
        .parent()
        .ok_or_else(|| AppError::InvalidState {
            reason: format!("current path {current_path:?} has no parent"),
        })?;
    fs::create_dir_all(parent)
        .map_err(|source| file_error("create directory", parent.to_path_buf(), source))?;
    let temporary_link = parent.join(format!(".current.{deployment_id}.tmp"));
    if fs::symlink_metadata(&temporary_link).is_ok() {
        fs::remove_file(&temporary_link)
            .map_err(|source| file_error("remove stale symlink", temporary_link.clone(), source))?;
    }
    symlink(release_path, &temporary_link)
        .map_err(|source| file_error("create symlink", temporary_link.clone(), source))?;
    fs::rename(&temporary_link, current_path)
        .map_err(|source| file_error("rename symlink", temporary_link, source))?;
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<(), AppError> {
    let directory = fs::File::open(path)
        .map_err(|source| file_error("open directory", path.to_path_buf(), source))?;
    directory
        .sync_all()
        .map_err(|source| file_error("sync directory", path.to_path_buf(), source))
}

fn remove_directory_if_exists(path: &Path) -> Result<(), AppError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)
            .map_err(|source| file_error("remove directory", path.to_path_buf(), source)),
        Ok(_) => Err(AppError::InvalidState {
            reason: format!("expected directory at {path:?}"),
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(file_error("read metadata", path.to_path_buf(), source)),
    }
}

fn cleanup_failed_release(source: AppError, path: &Path) -> AppError {
    match remove_directory_if_exists(path) {
        Ok(()) => source,
        Err(cleanup_error) => AppError::InvalidState {
            reason: format!(
                "release creation failed with {source}; failed release cleanup also failed with {cleanup_error}"
            ),
        },
    }
}

fn validate_sha(value: &str) -> Result<(), AppError> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::InvalidState {
            reason: format!("Git resolved invalid commit SHA {value:?}"),
        });
    }
    Ok(())
}

fn os(value: &str) -> OsString {
    OsString::from(value)
}
