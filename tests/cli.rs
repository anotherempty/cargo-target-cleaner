use std::process::Command;

const USAGE_ERROR_EXIT_CODE: i32 = 2;

#[test]
fn rejects_unsupported_shell_without_printing_shell_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
        .args(["init", "unsupported"])
        .output()
        .expect("run cleaner");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid value 'unsupported'"));
}

#[test]
fn clap_handles_help_version_and_invalid_commands() {
    for args in [
        vec![],
        vec!["--help"],
        vec!["init", "--help"],
        vec!["uninstall", "--help"],
        vec!["exec", "--help"],
        vec!["--version"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
            .args(args)
            .env_remove("HOME")
            .output()
            .expect("run CLI");
        assert!(output.status.success(), "{output:?}");
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty(), "{output:?}");
    }
    for args in [
        vec!["unknown"],
        vec!["init"],
        vec!["uninstall", "unsupported"],
        vec!["exec", "build"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
            .args(args)
            .env_remove("HOME")
            .output()
            .expect("run CLI");
        assert_eq!(output.status.code(), Some(USAGE_ERROR_EXIT_CODE));
        assert!(!output.stderr.is_empty());
    }
}

#[cfg(all(unix, test))]
mod zsh {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;

    const EXECUTABLE_MODE: u32 = 0o755;
    const FORWARDED_EXIT_CODE: i32 = 37;
    const REPEAT_COUNT: usize = 2;
    const DEFAULT_CONFIG: &str = "[free_space]\nmin_free_space_gib = 100\nacross_projects = true\n";
    const EXISTING_CONFIG: &str = "max_target_size_gib = 7\n";

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                assert!(std::thread::panicking(), "remove fixture: {error}");
            }
        }
    }

    impl Fixture {
        fn new() -> std::io::Result<Self> {
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "cargo-target-cleaner-shell-test-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }
    }

    #[test]
    fn init_preserves_config_and_is_repeatable() {
        let fixture = Fixture::new().expect("create fixture");
        let rc = fixture.0.join(".zshrc");
        fs::write(&rc, "# keep me\nexport KEEP=yes").expect("write config");
        for _ in 0..REPEAT_COUNT {
            let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
                .args(["init", "zsh"])
                .env("ZDOTDIR", &fixture.0)
                .env("HOME", &fixture.0)
                .output()
                .expect("run init");
            assert!(output.status.success(), "{:?}", output);
        }
        assert_eq!(
            fs::read_to_string(rc).expect("read config"),
            "# keep me\nexport KEEP=yes\ncargo() {\n    command cargo-target-cleaner exec -- \"$@\"\n}\n"
        );
    }

    #[test]
    fn init_creates_global_default_in_home_and_preserves_existing_config() {
        let fixture = Fixture::new().unwrap();
        let zsh = fixture.0.join("zsh");
        fs::create_dir(&zsh).unwrap();
        let config = fixture.0.join(".cargo-target-cleaner.toml");
        for existing in [None, Some(EXISTING_CONFIG), Some("")] {
            if let Some(contents) = existing {
                fs::write(&config, contents).unwrap();
            }
            for _ in 0..REPEAT_COUNT {
                let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
                    .args(["init", "zsh"])
                    .env("HOME", &fixture.0)
                    .env("ZDOTDIR", &zsh)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
                assert_eq!(
                    fs::read_to_string(&config).unwrap(),
                    existing.unwrap_or(DEFAULT_CONFIG)
                );
            }
        }
        assert!(!zsh.join(".cargo-target-cleaner.toml").exists());
    }

    #[test]
    fn init_rejects_global_config_directory_before_installing_shell_function() {
        let fixture = Fixture::new().unwrap();
        fs::create_dir(fixture.0.join(".cargo-target-cleaner.toml")).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
            .args(["init", "zsh"])
            .env("HOME", &fixture.0)
            .env_remove("ZDOTDIR")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("read"));
        assert!(!fixture.0.join(".zshrc").exists());
    }

    #[test]
    fn init_reports_missing_home_and_unwritable_config() {
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
            .args(["init", "zsh"])
            .env_remove("HOME")
            .env_remove("ZDOTDIR")
            .output()
            .expect("run init");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("HOME is not set"));
        let fixture = Fixture::new().expect("create fixture");
        fs::create_dir(fixture.0.join(".zshrc")).expect("create directory");
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
            .args(["init", "zsh"])
            .env("HOME", &fixture.0)
            .env_remove("ZDOTDIR")
            .output()
            .expect("run init");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("failed to read"));
    }

    #[test]
    fn uninstall_removes_integration_preserves_config_and_is_repeatable() {
        let fixture = Fixture::new().expect("create fixture");
        let rc = fixture.0.join(".zshrc");
        let original = "# keep me\ncargo_custom() { echo custom; }\n";
        fs::write(&rc, original).expect("write config");
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
            .args(["init", "zsh"])
            .env("ZDOTDIR", &fixture.0)
            .env("HOME", &fixture.0)
            .output()
            .expect("run init");
        assert!(output.status.success());
        let mut contents = fs::read_to_string(&rc).expect("read config");
        contents.push_str("# trailing config\n");
        fs::write(&rc, contents).expect("write trailing config");
        for _ in 0..REPEAT_COUNT {
            let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
                .args(["uninstall", "zsh"])
                .env("ZDOTDIR", &fixture.0)
                .env("HOME", &fixture.0)
                .output()
                .expect("run uninstall");
            assert!(output.status.success(), "{output:?}");
            assert_eq!(
                fs::read_to_string(&rc).expect("read config"),
                format!("{original}# trailing config\n")
            );
        }
    }

    #[test]
    fn uninstall_missing_config_does_not_create_file() {
        let fixture = Fixture::new().expect("create fixture");
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"))
            .args(["uninstall", "zsh"])
            .env("HOME", &fixture.0)
            .env_remove("ZDOTDIR")
            .output()
            .expect("run uninstall");
        assert!(output.status.success(), "{output:?}");
        assert!(!fixture.0.join(".zshrc").exists());
    }

    #[test]
    fn shell_forwards_arguments_streams_environment_directory_and_status() {
        let fixture = Fixture::new().expect("create fixture");
        fs::write(fixture.0.join(".cargo-target-cleaner.toml"), "").expect("disable cleanup");
        let cargo = fixture.0.join("cargo");
        fs::write(
            &cargo,
            "#!/bin/sh\nif [ \"$2\" = metadata ]; then printf '%s' \"$CLEANER_METADATA\"; exit 0; fi\nprintf '%s\\0' \"$@\"\nprintf '%s\\0' \"$PWD\" \"$CLEANER_TEST_VALUE\"\nprintf 'cargo diagnostic' >&2\nexit \"$CLEANER_FORWARD_EXIT\"\n",
        )
        .expect("write fake cargo");
        fs::set_permissions(&cargo, fs::Permissions::from_mode(EXECUTABLE_MODE))
            .expect("make executable");
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_cargo-target-cleaner"),
            fixture.0.join("cargo-target-cleaner"),
        )
        .expect("link wrapper");

        let mut paths = vec![fixture.0.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let output = Command::new("zsh")
            .args([
                "-f",
                "-c",
                "cargo-target-cleaner init zsh >/dev/null || exit; source \"$HOME/.zshrc\" || exit; cargo \"$@\"",
                "shell-test",
                "+stable",
                "build",
                "--manifest-path",
                "directory with spaces/Cargo.toml",
                "",
                "literal * $HOME ;",
            ])
            .current_dir(&fixture.0)
            .env("PATH", std::env::join_paths(paths).expect("join PATH"))
            .env("HOME", &fixture.0)
            .env_remove("ZDOTDIR")
            .env("CLEANER_METADATA", serde_json::json!({
                "workspace_root": fixture.0,
                "target_directory": fixture.0.join("target"),
            }).to_string())
            .env("CLEANER_FORWARD_EXIT", FORWARDED_EXIT_CODE.to_string())
            .env("CLEANER_TEST_VALUE", "inherited value")
            .output()
            .expect("run zsh; install zsh to run shell integration tests");

        assert_eq!(output.status.code(), Some(FORWARDED_EXIT_CODE));
        assert_eq!(output.stderr, b"cargo diagnostic");
        let expected = format!(
            "+stable\0build\0--manifest-path\0directory with spaces/Cargo.toml\0\0literal * $HOME ;\0{}\0inherited value\0",
            fixture
                .0
                .canonicalize()
                .expect("resolve directory")
                .display()
        );
        assert_eq!(output.stdout, expected.as_bytes());
    }
}
