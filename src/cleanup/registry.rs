use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::filesystem::FilesystemId;

use crate::error::{Error, FileOperation};

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Registry {
    filesystems: BTreeMap<FilesystemId, Vec<Project>>,
}

impl Registry {
    pub(super) fn register(workspace: &Path, target: &Path) -> Result<Self, Error> {
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, workspace = %workspace.display(), "cargo-target-cleaner: updating project registry");
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or(Error::MissingRegistryHome)?;
        let directory = PathBuf::from(home).join(".cargo-target-cleaner");
        let path = directory.join("projects.toml");
        let mut registry = Self::read(&path)?.refresh()?;
        if let Some(current) = Project::resolve(workspace, target)? {
            registry.insert(current)?;
        }
        Ok(registry)
    }

    fn read(path: &Path) -> Result<Self, Error> {
        match fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents).map_err(|source| Error::RegistryRead {
                path: path.to_owned(),
                source,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::file(FileOperation::Read, path, source)),
        }
    }

    fn refresh(self) -> Result<Self, Error> {
        let mut refreshed = Self::default();
        for project in self.filesystems.into_values().flatten() {
            if let Some(project) = Project::resolve(&project.workspace, &project.target)? {
                refreshed.insert(project)?;
            }
        }
        Ok(refreshed)
    }

    pub(super) fn persist(self) -> Result<(), Error> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or(Error::MissingRegistryHome)?;
        let directory = PathBuf::from(home).join(".cargo-target-cleaner");
        fs::create_dir_all(&directory)
            .map_err(|source| Error::file(FileOperation::Create, &directory, source))?;
        let path = directory.join("projects.toml");
        // A separate lock stays valid across atomic replacement of the registry.
        let lock_path = directory.join("projects.lock");
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| Error::file(FileOperation::Create, &lock_path, source))?;
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, "cargo-target-cleaner: acquiring project registry lock");
        fs2::FileExt::lock_exclusive(&lock)
            .map_err(|source| Error::file(FileOperation::Lock, &lock_path, source))?;
        // Merge registrations committed by other wrappers while cleanup ran.
        let mut registry = Self::read(&path)?.refresh()?;
        for project in self.refresh()?.filesystems.into_values().flatten() {
            registry.insert(project)?;
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)
            .map_err(|source| Error::file(FileOperation::Create, &path, source))?;
        let contents =
            toml::to_string_pretty(&registry).map_err(|source| Error::RegistryWrite {
                path: path.clone(),
                source,
            })?;
        temporary
            .write_all(contents.as_bytes())
            .map_err(|source| Error::file(FileOperation::Write, &path, source))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|source| Error::file(FileOperation::Write, &path, source))?;
        temporary
            .persist(&path)
            .map_err(|error| Error::file(FileOperation::Write, &path, error.error))?;
        // Dropping the file releases the lock, including on any error above.
        drop(lock);
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET,
            projects = registry.filesystems.values().map(Vec::len).sum::<usize>(),
            "cargo-target-cleaner: project registry updated"
        );
        Ok(())
    }

    fn insert(&mut self, project: Project) -> Result<(), Error> {
        let filesystem = FilesystemId::resolve(&project.target)?;
        let projects = self.filesystems.entry(filesystem).or_default();
        projects.retain(|existing| existing.target != project.target);
        projects.push(project);
        Ok(())
    }

    pub(super) fn heaviest(&self, filesystem: FilesystemId) -> Result<Option<&Project>, Error> {
        let projects = self
            .filesystems
            .get(&filesystem)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let started = std::time::Instant::now();
        tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET,
            projects = projects.len(),
            "cargo-target-cleaner: scanning registered targets for the largest"
        );
        let mut largest = None;
        for project in projects {
            if Project::resolve(&project.workspace, &project.target)?.is_none()
                || FilesystemId::resolve(&project.target)? != filesystem
            {
                continue;
            }
            tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, target = %project.target.display(), "cargo-target-cleaner: measuring registered target");
            let size = Project::target_size(&project.target, None)?;
            tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, target = %project.target.display(), size_bytes = size, "cargo-target-cleaner: measured registered target");
            if largest.is_none_or(|(_, largest_size)| size > largest_size) {
                largest = Some((project, size));
            }
        }
        if let Some((project, size)) = largest {
            tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET, target = %project.target.display(), size_bytes = size, elapsed_seconds = started.elapsed().as_secs_f64(), "cargo-target-cleaner: selected largest target");
        } else {
            tracing::info!(target: crate::diagnostics::Diagnostics::PROGRESS_TARGET,
                "cargo-target-cleaner: no eligible registered targets; skipping free-space cleanup"
            );
        }
        Ok(largest.map(|(project, _)| project))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(try_from = "ProjectFields")]
pub(super) struct Project {
    workspace: PathBuf,
    target: PathBuf,
}

impl TryFrom<ProjectFields> for Project {
    type Error = ProjectPathError;

    fn try_from(fields: ProjectFields) -> Result<Self, Self::Error> {
        if !fields.workspace.is_absolute() {
            return Err(ProjectPathError::Workspace);
        }
        if !fields.target.is_absolute() {
            return Err(ProjectPathError::Target);
        }
        Ok(Self {
            workspace: fields.workspace,
            target: fields.target,
        })
    }
}

impl Project {
    fn resolve(workspace: &Path, target: &Path) -> Result<Option<Self>, Error> {
        // Stored paths must be absolute; relative entries must never select a
        // target according to a later caller's working directory.
        if !workspace.is_absolute() || !target.is_absolute() {
            return Ok(None);
        }
        for (path, directory) in [
            (workspace.join("Cargo.toml"), false),
            (target.to_owned(), true),
        ] {
            match fs::metadata(&path) {
                Ok(metadata)
                    if metadata.is_dir() == directory && (directory || metadata.is_file()) => {}
                Ok(_) => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(Error::file(FileOperation::Inspect, &path, error)),
            }
        }
        let workspace = workspace
            .canonicalize()
            .map_err(|source| Error::file(FileOperation::Resolve, workspace, source))?;
        let target = target
            .canonicalize()
            .map_err(|source| Error::file(FileOperation::Resolve, target, source))?;
        Ok(Some(Self { workspace, target }))
    }

    pub(super) fn target(&self) -> &Path {
        &self.target
    }

    pub(super) fn is_current(&self, workspace: &Path, target: &Path) -> Result<bool, Error> {
        Ok(Self::resolve(workspace, target)?.is_some_and(|current| {
            self.workspace == current.workspace && self.target == current.target
        }))
    }

    pub(super) fn clean_command(&self) -> Command {
        let mut command = Command::new("cargo");
        command
            .current_dir(&self.workspace)
            .arg("clean")
            .arg("--manifest-path")
            .arg(self.workspace.join("Cargo.toml"));
        command
    }

    pub(super) fn target_size(root: &Path, stop_after: Option<u64>) -> Result<u64, Error> {
        let root = match root.canonicalize() {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(Error::file(FileOperation::Resolve, root, error)),
        };
        let mut pending = vec![root];
        let mut size = 0_u64;
        while let Some(path) = pending.pop() {
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(Error::file(FileOperation::Inspect, &path, error)),
            };
            if metadata.is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                for entry in fs::read_dir(&path)
                    .map_err(|error| Error::file(FileOperation::Read, &path, error))?
                {
                    pending.push(
                        entry
                            .map_err(|error| Error::file(FileOperation::Read, &path, error))?
                            .path(),
                    );
                }
            } else if metadata.is_file() {
                size = size.saturating_add(metadata.len());
                if stop_after.is_some_and(|limit| size > limit) {
                    return Ok(size);
                }
            }
        }
        Ok(size)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFields {
    workspace: PathBuf,
    target: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ProjectPathError {
    #[error("project workspace path must be absolute")]
    Workspace,
    #[error("project target path must be absolute")]
    Target,
}
