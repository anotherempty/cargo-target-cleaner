use std::path::Path;

use super::filesystem::FilesystemId;
use super::registry::Registry;
use super::{ByteLimit, Cleanup, Invocation, PolicyError, Threshold, Workspace};
use crate::error::{CargoOperation, Error, FileOperation};
use serde::Deserialize;

pub(super) struct FreeSpacePolicy {
    limit: ByteLimit,
    selection: Selection,
}

impl FreeSpacePolicy {
    fn new(limit: ByteLimit, across_projects: bool) -> Self {
        Self {
            limit,
            selection: if across_projects {
                Selection::AcrossProjects
            } else {
                Selection::Current
            },
        }
    }

    pub(super) fn apply(
        &self,
        invocation: &Invocation<'_>,
        workspace: &Workspace,
        target: &Path,
    ) -> Result<Option<Registry>, Error> {
        let registry = match self.selection {
            Selection::AcrossProjects => {
                Some(Registry::register(&workspace.workspace_root, target)?)
            }
            Selection::Current => None,
        };
        let filesystem_path = FilesystemId::existing_path(target)?;
        let filesystem = FilesystemId::resolve(&filesystem_path)?;
        let available = fs2::available_space(&filesystem_path)
            .map_err(|source| Error::file(FileOperation::FreeSpace, &filesystem_path, source))?;
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET,
            target = %target.display(),
            available_bytes = available,
            minimum_bytes = self.limit.get(),
            "cargo-target-cleaner: checked free disk space"
        );
        if available >= self.limit.get() {
            tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, "cargo-target-cleaner: free space is sufficient; skipping free-space cleanup");
            return Ok(registry);
        }
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, "cargo-target-cleaner: free space is below minimum; selecting a target to clean");
        match &registry {
            None => Cleanup::clean(invocation.command(CargoOperation::Clean), target)?,
            Some(registry) => {
                if let Some(project) = registry.heaviest(filesystem)? {
                    let command = if project.is_current(&workspace.workspace_root, target)? {
                        invocation.command(CargoOperation::Clean)
                    } else {
                        project.clean_command()
                    };
                    let cleanup = Cleanup::clean(command, project.target());
                    // Restore symlink destinations even after partial or timed-out cleanup.
                    let restoration = std::fs::create_dir_all(project.target()).map_err(|source| {
                        Error::file(FileOperation::Create, project.target(), source)
                    });
                    match (cleanup, restoration) {
                        (Err(cleanup), Err(restoration)) => {
                            tracing::warn!("cargo-target-cleaner: {restoration}");
                            return Err(cleanup);
                        }
                        (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
                        (Ok(()), Ok(())) => {}
                    }
                }
            }
        }
        Ok(registry)
    }
}

#[derive(Clone, Copy)]
enum Selection {
    Current,
    AcrossProjects,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FreeSpaceConfig {
    min_free_space_gib: u64,
    across_projects: bool,
}

impl FreeSpaceConfig {
    pub(super) fn into_policy(self) -> Result<FreeSpacePolicy, PolicyError> {
        let limit = ByteLimit::from_gib(self.min_free_space_gib, Threshold::MinFree)?;
        Ok(FreeSpacePolicy::new(limit, self.across_projects))
    }
}
