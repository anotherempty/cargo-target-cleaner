#[cfg(all(unix, test))]
mod tests {

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Output};
    use std::sync::atomic::{AtomicU64, Ordering};

    const METADATA_CALL_INDEX: usize = 0;
    const CLEAN_CALL_INDEX: usize = 1;
    const FORWARD_AFTER_CLEAN_INDEX: usize = 2;
    const CLEAN_AND_FORWARD_CALLS: usize = 3;
    const METADATA_AND_FORWARD_CALLS: usize = 2;
    const FORWARD_ONLY_CALLS: usize = 1;
    const CONFLICTING_THRESHOLDS_CONFIG: &str =
        "max_target_size_gib = 20\n[free_space]\nmin_free_space_gib = 10\nacross_projects = true";
    const ZERO_LIMIT_CONFIG: &str = "max_target_size_gib = 0";
    const NEGATIVE_LIMIT_CONFIG: &str = "min_free_space_gib = -1";
    const OVERFLOW_LIMIT_CONFIG: &str = "max_target_size_gib = 999999999999";
    const UNKNOWN_FIELD_CONFIG: &str = "max_target_szie_gib = 1";
    const BYTES_PER_GIB: u64 = 1 << 30;
    const OVERSIZED_ARTIFACT_BYTES: u64 = BYTES_PER_GIB + 1;
    const EXECUTABLE_MODE: u32 = 0o755;
    const FORWARDED_EXIT_CODE: i32 = 37;
    const CLEAN_FAILURE_EXIT_CODE: i32 = 9;
    const SUCCESS_EXIT_CODE: i32 = 0;
    const TARGET_LIMIT_CONFIG: &str = "max_target_size_gib = 1";
    // An intentionally unattainable free-space requirement forces cleanup.
    const REGISTRATION_CONFIG: &str =
        "[free_space]\nmin_free_space_gib = 1\nacross_projects = true";
    const LOW_DISK_CONFIG: &str =
        "[free_space]\nmin_free_space_gib = 1000000000\nacross_projects = true";

    #[derive(Debug, thiserror::Error)]
    enum FixtureError {
        #[error(transparent)]
        Io(#[from] std::io::Error),
        #[error(transparent)]
        Path(#[from] std::env::JoinPathsError),
    }

    struct Fixture {
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.root) {
                assert!(std::thread::panicking(), "remove fixture: {error}");
            }
        }
    }

    impl Fixture {
        fn new(config: &str) -> std::io::Result<Self> {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "cargo-target-cleaner-policy-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("bin"))?;
            fs::create_dir_all(root.join("home"))?;
            fs::create_dir_all(root.join("target"))?;
            fs::create_dir_all(root.join("member/src"))?;
            fs::write(root.join(".cargo-target-cleaner.toml"), config)?;
            fs::write(root.join("Cargo.toml"), "[workspace]\n")?;
            let script = root.join("bin/cargo");
            fs::write(
                &script,
                r#"#!/bin/sh
printf '%s\n' "$*" >> "$CLEANER_LOG"
case "$1" in +*) shift;; esac
case "$1" in
  metadata) printf '%s' "$CLEANER_METADATA";;
  clean)
    if [ -n "${CLEANER_REMOVE_TARGET:-}" ]; then
      rm -rf -- "$CLEANER_REMOVE_TARGET" || exit "$CLEANER_CLEAN_EXIT"
      if [ -n "${CLEANER_BLOCK_RESTORE:-}" ]; then
        printf 'blocked' > "$CLEANER_REMOVE_TARGET"
      fi
    fi
    if [ -n "${CLEANER_REQUIRE_NO_REGISTRY:-}" ] && [ -e "$HOME/.cargo-target-cleaner" ]; then
      exit "$CLEANER_CLEAN_EXIT"
    fi
    if [ -n "${CLEANER_REQUIRE_NO_REGISTRY:-}" ]; then
      exit 0
    fi
    exit "${CLEANER_CLEAN_EXIT:-0}";;
  *) printf 'forwarded'; exit "$CLEANER_FORWARD_EXIT";;
esac
"#,
            )?;
            fs::set_permissions(script, fs::Permissions::from_mode(EXECUTABLE_MODE))?;
            Ok(Self {
                root: root.canonicalize()?,
            })
        }

        fn with_global(config: &str) -> std::io::Result<Self> {
            let fixture = Self::new("")?;
            fixture.global(config)?;
            Ok(fixture)
        }

        fn registered_projects(registry: &toml::Value) -> Option<Vec<&toml::Value>> {
            let groups = registry
                .get("filesystems")?
                .as_table()?
                .values()
                .map(toml::Value::as_array)
                .collect::<Option<Vec<_>>>()?;
            Some(groups.into_iter().flatten().collect())
        }

        fn global(&self, config: &str) -> std::io::Result<()> {
            fs::write(self.root.join("home/.cargo-target-cleaner.toml"), config)
        }

        fn command(&self, args: &[&str]) -> Result<Command, FixtureError> {
            let mut paths = vec![self.root.join("bin")];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"));
            command
                .args(["exec", "--"])
                .args(args)
                .current_dir(self.root.join("member/src"))
                .env("HOME", self.root.join("home"))
                .env("PATH", std::env::join_paths(paths)?)
                .env("CLEANER_FORWARD_EXIT", FORWARDED_EXIT_CODE.to_string())
                .env("CLEANER_LOG", self.root.join("calls"))
                .env(
                    "CLEANER_METADATA",
                    serde_json::json!({
                        "workspace_root": self.root,
                        "target_directory": self.root.join("target"),
                    })
                    .to_string(),
                );
            Ok(command)
        }

        fn run(&self, args: &[&str]) -> Result<Output, FixtureError> {
            Ok(self.command(args)?.output()?)
        }

        fn calls(&self) -> std::io::Result<String> {
            match fs::read_to_string(self.root.join("calls")) {
                Ok(calls) => Ok(calls),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
                Err(error) => Err(error),
            }
        }

        fn artifact(&self, relative: &str, size: u64) -> std::io::Result<()> {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().ok_or(std::io::ErrorKind::InvalidInput)?)?;
            fs::File::create(path)?.set_len(size)?;
            Ok(())
        }
    }

    #[test]
    fn verbose_setting_inherits_and_allows_project_overrides() {
        for (global, project, verbose) in [
            ("", "", false),
            ("verbose = true\n", "", true),
            ("verbose = true\n", "verbose = false", false),
            ("verbose = false\n", "verbose = true", true),
        ] {
            let fixture = Fixture::new(project).unwrap();
            fixture
                .global(&format!("{global}{LOW_DISK_CONFIG}"))
                .unwrap();
            let output = fixture.run(&["build"]).unwrap();
            assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
            assert_eq!(output.stdout, b"forwarded");
            let diagnostics = String::from_utf8_lossy(&output.stderr);
            assert!(diagnostics.contains("running cargo clean"));
            assert!(
                diagnostics
                    .lines()
                    .all(|line| line.trim_start().starts_with("INFO "))
            );
            for message in [
                "resolving Cargo workspace",
                "updating project registry",
                "checked free disk space",
                "scanning registered targets",
                "measuring registered target",
                "selected largest target",
                "cargo clean completed",
                "cleanup checks complete",
            ] {
                assert_eq!(
                    diagnostics.contains(message),
                    verbose,
                    "{message}: {diagnostics}"
                );
            }
        }
    }

    #[test]
    fn verbose_requires_a_boolean() {
        for global in [false, true] {
            let fixture = Fixture::new(if global { "" } else { "verbose = 1" }).unwrap();
            if global {
                fixture.global("verbose = 1").unwrap();
            }
            let output = fixture.run(&["build"]).unwrap();
            assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
            assert_eq!(fixture.calls().unwrap(), "build\n");
        }
    }

    #[test]
    fn oversize_target_cleans_before_forwarding_with_toolchain_and_config() {
        let fixture = Fixture::new(TARGET_LIMIT_CONFIG).unwrap();
        fixture
            .artifact("target/debug/artifact", OVERSIZED_ARTIFACT_BYTES)
            .unwrap();
        let output = fixture
            .run(&["+stable", "build", "--config", "net.offline=true"])
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert_eq!(output.stdout, b"forwarded");
        let calls = fixture.calls().unwrap();
        let lines: Vec<_> = calls.lines().collect();
        assert_eq!(lines.len(), CLEAN_AND_FORWARD_CALLS);
        assert!(
            lines[METADATA_CALL_INDEX].starts_with("+stable metadata --config net.offline=true")
        );
        assert!(
            lines[CLEAN_CALL_INDEX]
                .starts_with("+stable clean --config net.offline=true --target-dir ")
        );
        assert_eq!(
            lines[FORWARD_AFTER_CLEAN_INDEX],
            "+stable build --config net.offline=true"
        );
    }

    #[test]
    fn threshold_equality_does_not_clean() {
        let fixture = Fixture::new(TARGET_LIMIT_CONFIG).unwrap();
        fixture.artifact("target/artifact", BYTES_PER_GIB).unwrap();
        assert_eq!(
            fixture.run(&["test"]).unwrap().status.code(),
            Some(FORWARDED_EXIT_CODE)
        );
        assert_eq!(
            fixture.calls().unwrap().lines().count(),
            METADATA_AND_FORWARD_CALLS
        );
        assert!(!fixture.calls().unwrap().contains("clean"));
    }

    #[test]
    fn low_disk_alone_triggers_cleanup() {
        let fixture = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        assert_eq!(
            fixture.run(&["check"]).unwrap().status.code(),
            Some(FORWARDED_EXIT_CODE)
        );
        assert!(
            fixture
                .calls()
                .unwrap()
                .lines()
                .nth(CLEAN_CALL_INDEX)
                .unwrap()
                .starts_with("clean ")
        );
    }

    #[test]
    fn target_override_is_measured_and_cleaned() {
        let fixture = Fixture::new(TARGET_LIMIT_CONFIG).unwrap();
        fixture
            .artifact("custom target/artifact", OVERSIZED_ARTIFACT_BYTES)
            .unwrap();
        let output = fixture
            .run(&["build", "--target-dir=../../custom target"])
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert!(
            fixture
                .calls()
                .unwrap()
                .lines()
                .nth(CLEAN_CALL_INDEX)
                .unwrap()
                .ends_with("/member/src/../../custom target")
        );
    }

    #[test]
    fn manifest_selects_project_config_from_outside_project() {
        let fixture = Fixture::new(TARGET_LIMIT_CONFIG).unwrap();
        fixture
            .artifact("target/artifact", OVERSIZED_ARTIFACT_BYTES)
            .unwrap();
        let manifest = fixture.root.join("Cargo.toml");
        let output = fixture
            .command(&["build", "--manifest-path", manifest.to_str().unwrap()])
            .unwrap()
            .current_dir(std::env::temp_dir())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert!(
            fixture
                .calls()
                .unwrap()
                .lines()
                .nth(CLEAN_CALL_INDEX)
                .unwrap()
                .starts_with("clean --manifest-path ")
        );
    }

    #[test]
    fn invalid_configuration_warns_and_forwards_build() {
        for config in [
            ZERO_LIMIT_CONFIG,
            NEGATIVE_LIMIT_CONFIG,
            OVERFLOW_LIMIT_CONFIG,
            UNKNOWN_FIELD_CONFIG,
            "not toml",
        ] {
            let fixture = Fixture::new(config).unwrap();
            assert_eq!(
                fixture.run(&["build"]).unwrap().status.code(),
                Some(FORWARDED_EXIT_CODE)
            );
            assert_eq!(fixture.calls().unwrap(), "build\n");
        }
    }

    #[test]
    fn project_free_space_setting_warns_and_forwards_build() {
        let fixture = Fixture::new(CONFLICTING_THRESHOLDS_CONFIG).unwrap();
        let output = fixture.run(&["build"]).unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert!(String::from_utf8_lossy(&output.stderr).contains("unknown field `free_space`"));
        assert_eq!(fixture.calls().unwrap(), "build\n");
    }

    #[test]
    fn failed_clean_warns_and_forwards_build() {
        let fixture = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        let output = fixture
            .command(&["build"])
            .unwrap()
            .env("CLEANER_CLEAN_EXIT", CLEAN_FAILURE_EXIT_CODE.to_string())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("continuing with requested Cargo command")
        );
        assert_eq!(
            fixture.calls().unwrap().lines().count(),
            CLEAN_AND_FORWARD_CALLS
        );
    }

    #[test]
    fn invalid_metadata_warns_and_preserves_forwarded_arguments_and_status() {
        for exit_code in [SUCCESS_EXIT_CODE, FORWARDED_EXIT_CODE] {
            let fixture = Fixture::new(TARGET_LIMIT_CONFIG).unwrap();
            let output = fixture
                .command(&["build", "--release"])
                .unwrap()
                .env("CLEANER_METADATA", "invalid json")
                .env("CLEANER_FORWARD_EXIT", exit_code.to_string())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(exit_code));
            assert_eq!(output.stdout, b"forwarded");
            assert!(String::from_utf8_lossy(&output.stderr).contains("parse cargo metadata"));
            let calls = fixture.calls().unwrap();
            assert_eq!(calls.lines().count(), METADATA_AND_FORWARD_CALLS);
            assert_eq!(calls.lines().last(), Some("build --release"));
        }
    }

    #[test]
    fn registry_save_failure_after_cleanup_still_forwards() {
        const REGISTRY_LOCK_PATH: &str = "home/.cargo-target-cleaner/projects.lock";
        let fixture = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        fs::create_dir_all(fixture.root.join(REGISTRY_LOCK_PATH)).unwrap();
        let output = fixture.run(&["build"]).unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert_eq!(output.stdout, b"forwarded");
        assert!(String::from_utf8_lossy(&output.stderr).contains("projects.lock"));
        assert_eq!(
            fixture.calls().unwrap().lines().count(),
            CLEAN_AND_FORWARD_CALLS
        );
        assert!(
            !fixture
                .root
                .join("home/.cargo-target-cleaner/projects.toml")
                .exists()
        );
    }

    #[test]
    fn failed_clean_restores_symlink_destination_and_reports_restoration_errors() {
        const DESTINATION_NAME: &str = "artifacts";
        const TARGET_NAME: &str = "target";
        for block_restoration in [false, true] {
            let fixture = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
            let target = fixture.root.join(TARGET_NAME);
            let destination = fixture.root.join(DESTINATION_NAME);
            fs::rename(&target, &destination).unwrap();
            std::os::unix::fs::symlink(&destination, &target).unwrap();
            let output = fixture
                .command(&["build"])
                .unwrap()
                .env("CLEANER_REMOVE_TARGET", &destination)
                .env(
                    "CLEANER_BLOCK_RESTORE",
                    if block_restoration { "true" } else { "" },
                )
                .env("CLEANER_CLEAN_EXIT", CLEAN_FAILURE_EXIT_CODE.to_string())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
            assert!(target.is_symlink());
            assert_eq!(fs::read_link(&target).unwrap(), destination);
            assert_eq!(target.is_dir(), !block_restoration);
            let diagnostics = String::from_utf8_lossy(&output.stderr);
            assert!(diagnostics.contains("cargo clean failed"));
            assert_eq!(diagnostics.contains("failed to create"), block_restoration);
            assert_eq!(fixture.calls().unwrap().lines().last(), Some("build"));
        }
    }

    #[test]
    fn registry_is_created_only_after_all_cleanup_policies() {
        for config in [LOW_DISK_CONFIG, REGISTRATION_CONFIG] {
            let fixture = Fixture::with_global(config).unwrap();
            fs::write(
                fixture.root.join(".cargo-target-cleaner.toml"),
                TARGET_LIMIT_CONFIG,
            )
            .unwrap();
            fixture
                .artifact("target/artifact", OVERSIZED_ARTIFACT_BYTES)
                .unwrap();
            let output = fixture
                .command(&["build"])
                .unwrap()
                .env("CLEANER_REQUIRE_NO_REGISTRY", "true")
                .env("CLEANER_CLEAN_EXIT", CLEAN_FAILURE_EXIT_CODE.to_string())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
            assert!(fixture.calls().unwrap().contains("clean --"));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("cargo clean failed"));
            assert!(
                fixture
                    .root
                    .join("home/.cargo-target-cleaner/projects.toml")
                    .is_file()
            );
        }
    }

    #[test]
    fn failed_cleanup_preserves_registry_and_does_not_create_one() {
        let fixture = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        let registry_directory = fixture.root.join("home/.cargo-target-cleaner");
        let registry_path = registry_directory.join("projects.toml");
        for existing in [false, true] {
            if existing {
                fs::create_dir_all(&registry_directory).unwrap();
                fs::write(&registry_path, "[filesystems]\n").unwrap();
            }
            let output = fixture
                .command(&["build"])
                .unwrap()
                .env("CLEANER_CLEAN_EXIT", CLEAN_FAILURE_EXIT_CODE.to_string())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
            if existing {
                assert_eq!(
                    fs::read_to_string(&registry_path).unwrap(),
                    "[filesystems]\n"
                );
            } else {
                assert!(!registry_directory.exists());
            }
        }
    }

    #[test]
    fn non_build_commands_and_help_bypass_cleanup() {
        for args in [
            vec!["clean"],
            vec!["fmt"],
            vec!["metadata"],
            vec!["build", "--help"],
            vec!["install", "some-package"],
        ] {
            let fixture = Fixture::new("invalid config").unwrap();
            let expected = if matches!(args[0], "clean" | "metadata") {
                SUCCESS_EXIT_CODE
            } else {
                FORWARDED_EXIT_CODE
            };
            assert_eq!(fixture.run(&args).unwrap().status.code(), Some(expected));
            assert_eq!(fixture.calls().unwrap().lines().count(), FORWARD_ONLY_CALLS);
        }
    }

    #[test]
    fn absent_or_empty_config_preserves_forwarding() {
        for absent in [false, true] {
            let fixture = Fixture::new("").unwrap();
            if absent {
                fs::remove_file(fixture.root.join(".cargo-target-cleaner.toml")).unwrap();
            }
            assert_eq!(
                fixture.run(&["build"]).unwrap().status.code(),
                Some(FORWARDED_EXIT_CODE)
            );
            assert_eq!(fixture.calls().unwrap(), "build\n");
        }
    }

    #[test]
    fn global_free_space_cannot_be_disabled_by_project_config() {
        for project in [None, Some(""), Some(TARGET_LIMIT_CONFIG)] {
            let fixture = Fixture::new(project.unwrap_or("")).unwrap();
            fs::write(
                fixture.root.join("home/.cargo-target-cleaner.toml"),
                LOW_DISK_CONFIG,
            )
            .unwrap();
            if project.is_none() {
                fs::remove_file(fixture.root.join(".cargo-target-cleaner.toml")).unwrap();
            }
            assert_eq!(
                fixture.run(&["build"]).unwrap().status.code(),
                Some(FORWARDED_EXIT_CODE)
            );
            assert!(fixture.calls().unwrap().contains("clean --target-dir"));
        }
    }

    #[test]
    fn invalid_global_config_warns_and_forwards_even_with_project_override() {
        for project in ["", TARGET_LIMIT_CONFIG] {
            let fixture = Fixture::new(project).unwrap();
            fixture.global("invalid toml").unwrap();
            let output = fixture.run(&["build"]).unwrap();
            assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
            assert!(String::from_utf8_lossy(&output.stderr).contains("invalid"));
            assert_eq!(fixture.calls().unwrap(), "build\n");
        }
    }

    #[test]
    fn nearest_config_overrides_parent_and_symlinks_are_not_traversed() {
        let fixture = Fixture::new("invalid parent config").unwrap();
        fs::write(
            fixture.root.join("member/.cargo-target-cleaner.toml"),
            TARGET_LIMIT_CONFIG,
        )
        .unwrap();
        fixture
            .artifact("outside/artifact", OVERSIZED_ARTIFACT_BYTES)
            .unwrap();
        std::os::unix::fs::symlink(
            fixture.root.join("outside"),
            fixture.root.join("target/link"),
        )
        .unwrap();
        assert_eq!(
            fixture.run(&["build"]).unwrap().status.code(),
            Some(FORWARDED_EXIT_CODE)
        );
        assert_eq!(
            fixture.calls().unwrap().lines().count(),
            METADATA_AND_FORWARD_CALLS
        );
    }

    #[test]
    fn real_cargo_cleans_workspace_configured_target_then_rebuilds_member() {
        let fixture = Fixture::new("").unwrap();
        fs::write(
            fixture.root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"member\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::write(
            fixture.root.join("member/Cargo.toml"),
            "[package]\nname = \"cleanup-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(fixture.root.join("member/src/main.rs"), "fn main() {}\n").unwrap();
        fs::create_dir(fixture.root.join(".cargo")).unwrap();
        fs::write(
            fixture.root.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"custom-target\"\n",
        )
        .unwrap();
        let build = Command::new(env!("CARGO"))
            .args(["build", "--offline"])
            .current_dir(&fixture.root)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let marker = fixture.root.join("custom-target/cleanup-marker");
        fs::write(&marker, "old artifact").unwrap();
        fixture.global(LOW_DISK_CONFIG).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
            .args(["exec", "--", "build", "--offline"])
            .current_dir(fixture.root.join("member/src"))
            .env("HOME", fixture.root.join("home"))
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!marker.exists());
        assert!(
            fixture
                .root
                .join("custom-target/debug/cleanup-fixture")
                .is_file()
        );
        assert!(fixture.root.join("member/src/main.rs").is_file());
        assert!(String::from_utf8_lossy(&output.stderr).contains("running cargo clean"));
    }
    #[test]
    fn attached_manifest_selects_configuration_and_program_arguments_are_ignored() {
        let fixture = Fixture::new(TARGET_LIMIT_CONFIG).unwrap();
        fixture
            .artifact("target/artifact", OVERSIZED_ARTIFACT_BYTES)
            .unwrap();
        let manifest = format!("-m{}", fixture.root.join("Cargo.toml").display());
        let output = fixture
            .command(&[
                "run",
                &manifest,
                "--",
                "--help",
                "--target-dir",
                "unrelated",
            ])
            .unwrap()
            .current_dir(std::env::temp_dir())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert!(
            fixture
                .calls()
                .unwrap()
                .lines()
                .nth(CLEAN_CALL_INDEX)
                .unwrap()
                .ends_with("/target")
        );
        assert!(
            fixture
                .calls()
                .unwrap()
                .lines()
                .nth(FORWARD_AFTER_CLEAN_INDEX)
                .unwrap()
                .ends_with("-- --help --target-dir unrelated")
        );
    }
    #[test]
    fn low_disk_selects_largest_registered_project_and_preserves_forwarding() {
        const SMALL_BYTES: u64 = 8;
        const LARGE_BYTES: u64 = 32;
        let large = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
        large.artifact("target/artifact", LARGE_BYTES).unwrap();
        assert_eq!(
            large.run(&["build"]).unwrap().status.code(),
            Some(FORWARDED_EXIT_CODE)
        );
        let small = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        small.artifact("target/artifact", SMALL_BYTES).unwrap();
        large.global(LOW_DISK_CONFIG).unwrap();
        let output = small
            .command(&["+stable", "build", "--config", "net.offline=true"])
            .unwrap()
            .env("HOME", large.root.join("home"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        let calls = small.calls().unwrap();
        let clean = calls.lines().nth(CLEAN_CALL_INDEX).unwrap();
        assert_eq!(
            clean,
            format!(
                "clean --manifest-path {} --target-dir {}",
                large.root.join("Cargo.toml").display(),
                large.root.join("target").display()
            )
        );
        assert_eq!(
            calls.lines().last(),
            Some("+stable build --config net.offline=true")
        );
        assert_eq!(calls.lines().count(), CLEAN_AND_FORWARD_CALLS);
    }

    #[test]
    fn inaccessible_registered_projects_are_retained_while_accessible_targets_are_cleaned() {
        const INACCESSIBLE_MODE: u32 = 0o000;
        const PROJECT_COUNT: usize = 2;
        const MANIFEST_NAME: &str = "Cargo.toml";
        const REGISTRY_PATH: &str = "home/.cargo-target-cleaner/projects.toml";
        const TARGET_NAME: &str = "target";
        for measurement_failure in [false, true] {
            let old = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
            old.run(&["build"]).unwrap();
            old.global(LOW_DISK_CONFIG).unwrap();
            let old_target = old.root.join(TARGET_NAME);
            let permissions = fs::metadata(&old_target).unwrap().permissions();
            if measurement_failure {
                fs::set_permissions(&old_target, fs::Permissions::from_mode(INACCESSIBLE_MODE))
                    .unwrap();
                if fs::read_dir(&old_target).is_ok() {
                    // Privileged test runners bypass directory permissions.
                    fs::set_permissions(&old_target, permissions).unwrap();
                    continue;
                }
            } else {
                let manifest = old.root.join(MANIFEST_NAME);
                fs::remove_file(&manifest).unwrap();
                std::os::unix::fs::symlink(MANIFEST_NAME, &manifest).unwrap();
            }
            let current = Fixture::new(TARGET_LIMIT_CONFIG).unwrap();
            current
                .artifact("target/artifact", OVERSIZED_ARTIFACT_BYTES)
                .unwrap();
            let output = current
                .command(&["build"])
                .unwrap()
                .env("HOME", old.root.join("home"))
                .output()
                .unwrap();
            fs::set_permissions(&old_target, permissions).unwrap();
            assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("skipping inaccessible registered project")
            );
            let calls = current.calls().unwrap();
            const BOTH_POLICIES_CLEAN_COUNT: usize = 2;
            let expected_clean = format!(
                "clean --target-dir {}",
                current.root.join(TARGET_NAME).display()
            );
            assert_eq!(
                calls.lines().filter(|line| *line == expected_clean).count(),
                BOTH_POLICIES_CLEAN_COUNT
            );
            let registry: toml::Value =
                toml::from_str(&fs::read_to_string(old.root.join(REGISTRY_PATH)).unwrap()).unwrap();
            let projects = Fixture::registered_projects(&registry).unwrap();
            assert_eq!(projects.len(), PROJECT_COUNT);
            assert!(projects.iter().any(|project| {
                project.get("target").and_then(toml::Value::as_str) == old_target.to_str()
            }));
        }
    }

    #[test]
    fn current_project_is_registered_before_largest_selection() {
        const SMALL_BYTES: u64 = 8;
        const LARGE_BYTES: u64 = 32;
        let small = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
        small.artifact("target/artifact", SMALL_BYTES).unwrap();
        assert_eq!(
            small.run(&["build"]).unwrap().status.code(),
            Some(FORWARDED_EXIT_CODE)
        );
        let large = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        large.artifact("target/artifact", LARGE_BYTES).unwrap();
        small.global(LOW_DISK_CONFIG).unwrap();
        let output = large
            .command(&["build"])
            .unwrap()
            .env("HOME", small.root.join("home"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert_eq!(
            large
                .calls()
                .unwrap()
                .lines()
                .nth(CLEAN_CALL_INDEX)
                .unwrap(),
            format!("clean --target-dir {}", large.root.join("target").display())
        );
    }

    #[test]
    fn size_policy_still_cleans_current_project_with_a_larger_registered_project() {
        const LARGER_BYTES: u64 = OVERSIZED_ARTIFACT_BYTES * 2;
        let large = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
        large.artifact("target/artifact", LARGER_BYTES).unwrap();
        assert_eq!(
            large.run(&["build"]).unwrap().status.code(),
            Some(FORWARDED_EXIT_CODE)
        );
        let current = Fixture::new(TARGET_LIMIT_CONFIG).unwrap();
        current
            .artifact("target/artifact", OVERSIZED_ARTIFACT_BYTES)
            .unwrap();
        let output = current
            .command(&["build"])
            .unwrap()
            .env("HOME", large.root.join("home"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert_eq!(
            current
                .calls()
                .unwrap()
                .lines()
                .nth(CLEAN_CALL_INDEX)
                .unwrap(),
            format!(
                "clean --target-dir {}",
                current.root.join("target").display()
            )
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cross_project_cleanup_selects_only_the_targets_filesystem() {
        use std::os::unix::fs::MetadataExt;
        const LOCAL_ARTIFACT_BYTES: u64 = 64;
        const REMOTE_ARTIFACT_BYTES: u64 = 8;
        const FILESYSTEM_COUNT: usize = 2;
        let fixture = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        let other = tempfile::tempdir_in("/dev/shm").unwrap();
        assert_ne!(
            fs::metadata(&fixture.root).unwrap().dev(),
            fs::metadata(other.path()).unwrap().dev()
        );
        fixture
            .artifact("target/artifact", LOCAL_ARTIFACT_BYTES)
            .unwrap();
        assert_eq!(
            fixture.run(&["build"]).unwrap().status.code(),
            Some(FORWARDED_EXIT_CODE)
        );
        fs::write(
            other.path().join("artifact"),
            vec![0; REMOTE_ARTIFACT_BYTES as usize],
        )
        .unwrap();
        fs::write(fixture.root.join("calls"), "").unwrap();
        // The workspace stays on the original filesystem; only the target changes.
        let output = fixture
            .run(&["build", "--target-dir", other.path().to_str().unwrap()])
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert_eq!(
            fixture
                .calls()
                .unwrap()
                .lines()
                .nth(CLEAN_CALL_INDEX)
                .unwrap(),
            format!("clean --target-dir {}", other.path().display())
        );
        let registry: toml::Value = toml::from_str(
            &fs::read_to_string(
                fixture
                    .root
                    .join("home/.cargo-target-cleaner/projects.toml"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            registry["filesystems"].as_table().unwrap().len(),
            FILESYSTEM_COUNT
        );
        // Once this target disappears, the remaining target on the workspace's
        // filesystem must not become eligible for cleanup.
        let missing = other.path().join("missing-target");
        other.close().unwrap();
        fs::write(fixture.root.join("calls"), "").unwrap();
        let output = fixture
            .run(&["build", "--target-dir", missing.to_str().unwrap()])
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert_eq!(
            fixture.calls().unwrap().lines().count(),
            METADATA_AND_FORWARD_CALLS
        );
    }

    #[test]
    fn registry_refreshes_stale_filesystem_groups() {
        const STALE_FILESYSTEM: &str = "18446744073709551615";
        let fixture = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        let directory = fixture.root.join("home/.cargo-target-cleaner");
        fs::create_dir_all(&directory).unwrap();
        let registry = toml::toml! {
            [filesystems]
        };
        let mut registry = toml::Value::Table(registry);
        registry["filesystems"].as_table_mut().unwrap().insert(
            STALE_FILESYSTEM.into(),
            vec![
                toml::Value::try_from(serde_json::json!({
                    "workspace": fixture.root,
                    "target": fixture.root.join("target"),
                }))
                .unwrap(),
            ]
            .into(),
        );
        fs::write(
            directory.join("projects.toml"),
            toml::to_string(&registry).unwrap(),
        )
        .unwrap();
        // A missing current target cannot replace the stored entry, so selection
        // demonstrates that registration refreshed the stored filesystem identity.
        let missing = fixture.root.join("missing-target");
        let output = fixture
            .run(&["build", "--target-dir", missing.to_str().unwrap()])
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert!(fixture.calls().unwrap().contains(&format!(
            "--target-dir {}",
            fixture.root.join("target").display()
        )));
        let updated: toml::Value =
            toml::from_str(&fs::read_to_string(directory.join("projects.toml")).unwrap()).unwrap();
        assert!(
            !updated["filesystems"]
                .as_table()
                .unwrap()
                .contains_key(STALE_FILESYSTEM)
        );
    }

    #[test]
    fn registry_deduplicates_targets_and_prunes_removed_projects() {
        const PROJECT_COUNT: usize = 1;
        const REGISTRATION_ATTEMPTS: usize = 2;
        let old = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
        for _ in 0..REGISTRATION_ATTEMPTS {
            assert_eq!(
                old.run(&["build"]).unwrap().status.code(),
                Some(FORWARDED_EXIT_CODE)
            );
        }
        let path = old.root.join("home/.cargo-target-cleaner/projects.toml");
        let registry: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            Fixture::registered_projects(&registry).unwrap().len(),
            PROJECT_COUNT
        );
        fs::remove_dir_all(old.root.join("target")).unwrap();
        let current = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        old.global(LOW_DISK_CONFIG).unwrap();
        let output = current
            .command(&["build"])
            .unwrap()
            .env("HOME", old.root.join("home"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        let registry: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            Fixture::registered_projects(&registry).unwrap().len(),
            PROJECT_COUNT
        );
        assert_eq!(
            Fixture::registered_projects(&registry).unwrap()[0]["workspace"]
                .as_str()
                .unwrap(),
            current.root.to_str().unwrap()
        );
    }

    #[test]
    fn missing_targets_are_not_registered_or_cleaned() {
        let fixture = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        fs::remove_dir_all(fixture.root.join("target")).unwrap();
        assert_eq!(
            fixture.run(&["build"]).unwrap().status.code(),
            Some(FORWARDED_EXIT_CODE)
        );
        assert_eq!(
            fixture.calls().unwrap().lines().count(),
            METADATA_AND_FORWARD_CALLS
        );
        let registry: toml::Value = toml::from_str(
            &fs::read_to_string(
                fixture
                    .root
                    .join("home/.cargo-target-cleaner/projects.toml"),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(Fixture::registered_projects(&registry).unwrap().is_empty());
    }

    #[test]
    fn malformed_registry_is_not_overwritten_and_blocks_cleanup() {
        let fixture = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        let directory = fixture.root.join("home/.cargo-target-cleaner");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("projects.toml");
        fs::write(&path, "invalid registry").unwrap();
        let output = fixture.run(&["build"]).unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert!(!fixture.calls().unwrap().contains("clean --"));
        assert_eq!(fs::read_to_string(path).unwrap(), "invalid registry");
    }

    #[test]
    fn real_cargo_cleans_largest_other_project_before_building_current_project() {
        let old = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
        let current = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
        for fixture in [&old, &current] {
            fs::create_dir(fixture.root.join("src")).unwrap();
            fs::write(fixture.root.join("src/main.rs"), "fn main() {}\n").unwrap();
            fs::write(
                fixture.root.join("Cargo.toml"),
                "[package]\nname = \"registry-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .unwrap();
        }
        for fixture in [&old, &current] {
            fs::remove_dir(fixture.root.join("target")).unwrap();
            let output = Command::new(env!("CARGO"))
                .args(["build", "--offline"])
                .current_dir(&fixture.root)
                .env_remove("CARGO_TARGET_DIR")
                .env_remove("CARGO_BUILD_TARGET_DIR")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        old.artifact("target/large-marker", OVERSIZED_ARTIFACT_BYTES)
            .unwrap();
        fs::write(current.root.join("target/keep-marker"), "keep").unwrap();
        for fixture in [&old, &current] {
            old.global(if fixture.root == current.root {
                LOW_DISK_CONFIG
            } else {
                REGISTRATION_CONFIG
            })
            .unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
                .args(["exec", "--", "build", "--offline"])
                .current_dir(&fixture.root)
                .env("HOME", old.root.join("home"))
                .env_remove("CARGO_TARGET_DIR")
                .env_remove("CARGO_BUILD_TARGET_DIR")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(!old.root.join("target/large-marker").exists());
        assert!(old.root.join("src/main.rs").is_file());
        assert!(current.root.join("target/keep-marker").is_file());
        assert!(current.root.join("target/debug/registry-fixture").is_file());
    }
    #[test]
    fn real_cargo_preserves_symlinked_targets_after_registry_cleanup() {
        for clean_other_project in [false, true] {
            let selected = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
            let current = Fixture::with_global(LOW_DISK_CONFIG).unwrap();
            for fixture in [&selected, &current] {
                fs::create_dir(fixture.root.join("src")).unwrap();
                fs::write(fixture.root.join("src/main.rs"), "fn main() {}\n").unwrap();
                fs::write(
                    fixture.root.join("Cargo.toml"),
                    "[package]\nname = \"symlink-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                )
                .unwrap();
                fs::remove_dir(fixture.root.join("target")).unwrap();
                let output = Command::new(env!("CARGO"))
                    .args(["build", "--offline"])
                    .current_dir(&fixture.root)
                    .env_remove("CARGO_TARGET_DIR")
                    .env_remove("CARGO_BUILD_TARGET_DIR")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let destination = selected.root.join("artifacts");
            let target = selected.root.join("target");
            fs::rename(&target, &destination).unwrap();
            std::os::unix::fs::symlink(&destination, &target).unwrap();
            selected
                .artifact("target/large-marker", OVERSIZED_ARTIFACT_BYTES)
                .unwrap();
            assert_eq!(
                selected.run(&["build"]).unwrap().status.code(),
                Some(FORWARDED_EXIT_CODE)
            );
            selected.global(LOW_DISK_CONFIG).unwrap();
            let building = if clean_other_project {
                &current
            } else {
                &selected
            };
            let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
                .args(["exec", "--", "build", "--offline"])
                .current_dir(&building.root)
                .env("HOME", selected.root.join("home"))
                .env_remove("CARGO_TARGET_DIR")
                .env_remove("CARGO_BUILD_TARGET_DIR")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(target.is_symlink());
            assert!(destination.is_dir());
            assert!(!target.join("large-marker").exists());
            let output = Command::new(env!("CARGO"))
                .args(["build", "--offline"])
                .current_dir(&selected.root)
                .env_remove("CARGO_TARGET_DIR")
                .env_remove("CARGO_BUILD_TARGET_DIR")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(target.join("debug/symlink-fixture").is_file());
        }
    }

    #[test]
    fn concurrent_registration_keeps_both_projects() {
        const PROJECT_COUNT: usize = 2;
        let first = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
        let second = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
        let mut first_process = first
            .command(&["build"])
            .unwrap()
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut second_process = second
            .command(&["build"])
            .unwrap()
            .env("HOME", first.root.join("home"))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        assert_eq!(
            first_process.wait().unwrap().code(),
            Some(FORWARDED_EXIT_CODE)
        );
        assert_eq!(
            second_process.wait().unwrap().code(),
            Some(FORWARDED_EXIT_CODE)
        );
        let registry: toml::Value = toml::from_str(
            &fs::read_to_string(first.root.join("home/.cargo-target-cleaner/projects.toml"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            Fixture::registered_projects(&registry).unwrap().len(),
            PROJECT_COUNT
        );
    }

    #[test]
    fn shared_target_is_registered_once_across_projects() {
        const PROJECT_COUNT: usize = 1;
        let first = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
        let second = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
        assert_eq!(
            first.run(&["build"]).unwrap().status.code(),
            Some(FORWARDED_EXIT_CODE)
        );
        let target = first.root.join("target");
        let output = second
            .command(&["build", "--target-dir", target.to_str().unwrap()])
            .unwrap()
            .env("HOME", first.root.join("home"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        let registry: toml::Value = toml::from_str(
            &fs::read_to_string(first.root.join("home/.cargo-target-cleaner/projects.toml"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            Fixture::registered_projects(&registry).unwrap().len(),
            PROJECT_COUNT
        );
    }
    #[test]
    fn project_free_space_override_is_rejected_even_with_global_policy() {
        let fixture = Fixture::new(REGISTRATION_CONFIG).unwrap();
        fixture.global(LOW_DISK_CONFIG).unwrap();
        let output = fixture.run(&["build"]).unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert!(String::from_utf8_lossy(&output.stderr).contains("unknown field `free_space`"));
        assert_eq!(fixture.calls().unwrap(), "build\n");
    }

    #[test]
    fn registry_is_unused_without_global_free_space_policy() {
        for project in ["", TARGET_LIMIT_CONFIG] {
            let fixture = Fixture::new(project).unwrap();
            assert_eq!(
                fixture.run(&["build"]).unwrap().status.code(),
                Some(FORWARDED_EXIT_CODE)
            );
            let directory = fixture.root.join("home/.cargo-target-cleaner");
            assert!(!directory.exists());
            fs::create_dir(&directory).unwrap();
            let registry = directory.join("projects.toml");
            fs::write(&registry, "invalid registry").unwrap();
            assert_eq!(
                fixture.run(&["build"]).unwrap().status.code(),
                Some(FORWARDED_EXIT_CODE)
            );
            assert_eq!(fs::read_to_string(registry).unwrap(), "invalid registry");
        }
    }

    #[test]
    fn project_size_limit_overrides_global_size_default() {
        const LARGER_TARGET_LIMIT: &str = "max_target_size_gib = 2";
        for project in [None, Some(""), Some(LARGER_TARGET_LIMIT)] {
            let fixture = Fixture::new(project.unwrap_or("")).unwrap();
            fixture.global(TARGET_LIMIT_CONFIG).unwrap();
            fixture
                .artifact("target/artifact", OVERSIZED_ARTIFACT_BYTES)
                .unwrap();
            if project.is_none() {
                fs::remove_file(fixture.root.join(".cargo-target-cleaner.toml")).unwrap();
            }
            assert_eq!(
                fixture.run(&["build"]).unwrap().status.code(),
                Some(FORWARDED_EXIT_CODE)
            );
            assert_eq!(
                fixture.calls().unwrap().contains("clean --target-dir"),
                project.is_none()
            );
        }
    }

    #[test]
    fn global_free_space_runs_before_local_size_cleanup() {
        const LARGER_BYTES: u64 = OVERSIZED_ARTIFACT_BYTES * 2;
        let old = Fixture::with_global(REGISTRATION_CONFIG).unwrap();
        old.artifact("target/artifact", LARGER_BYTES).unwrap();
        assert_eq!(
            old.run(&["build"]).unwrap().status.code(),
            Some(FORWARDED_EXIT_CODE)
        );
        old.global(LOW_DISK_CONFIG).unwrap();
        let current = Fixture::new(TARGET_LIMIT_CONFIG).unwrap();
        current
            .artifact("target/artifact", OVERSIZED_ARTIFACT_BYTES)
            .unwrap();
        let output = current
            .command(&["build"])
            .unwrap()
            .env("HOME", old.root.join("home"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        let calls = current.calls().unwrap();
        let cleans: Vec<_> = calls
            .lines()
            .filter(|line| line.starts_with("clean "))
            .collect();
        assert_eq!(
            cleans,
            [
                format!(
                    "clean --manifest-path {} --target-dir {}",
                    old.root.join("Cargo.toml").display(),
                    old.root.join("target").display()
                ),
                format!(
                    "clean --target-dir {}",
                    current.root.join("target").display()
                ),
            ]
        );
        assert_eq!(calls.lines().last(), Some("build"));
    }
    #[test]
    fn current_project_free_space_cleans_current_without_reading_registry() {
        const LOCAL_FREE_SPACE: &str =
            "[free_space]\nmin_free_space_gib = 1000000000\nacross_projects = false";
        let fixture = Fixture::with_global(LOCAL_FREE_SPACE).unwrap();
        let directory = fixture.root.join("home/.cargo-target-cleaner");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("projects.toml");
        fs::write(&path, "invalid registry").unwrap();
        let output = fixture.run(&["build"]).unwrap();
        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert_eq!(
            fixture
                .calls()
                .unwrap()
                .lines()
                .nth(CLEAN_CALL_INDEX)
                .unwrap(),
            format!(
                "clean --target-dir {}",
                fixture.root.join("target").display()
            )
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "invalid registry");
    }

    #[test]
    fn invalid_free_space_objects_warn_and_forward_cargo() {
        for config in [
            "free_space = 100",
            "min_free_space_gib = 100",
            "[free_space]\nmin_free_space_gib = 1",
            "[free_space]\nacross_projects = true",
            "[free_space]\nmin_free_space_gib = 0\nacross_projects = true",
            "[free_space]\nmin_free_space_gib = -1\nacross_projects = true",
            "[free_space]\nmin_free_space_gib = 999999999999\nacross_projects = true",
            "[free_space]\nmin_free_space_gib = 1\nacross_projects = \"true\"",
            "[free_space]\nmin_free_space_gib = 1\nacross_projects = true\nunknown = true",
        ] {
            let fixture = Fixture::with_global(config).unwrap();
            assert_eq!(
                fixture.run(&["build"]).unwrap().status.code(),
                Some(FORWARDED_EXIT_CODE)
            );
            assert_eq!(fixture.calls().unwrap(), "build\n");
        }
    }
}
