#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    const EXECUTABLE_MODE: u32 = 0o755;
    const FORWARDED_EXIT_CODE: i32 = 37;

    struct Fixture {
        directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            #[cfg(target_os = "linux")]
            // Reap the orphaned worker explicitly when the test kills its parent.
            // SAFETY: this prctl operation takes integer arguments only.
            assert_eq!(
                unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
                0
            );
            let fixture = Self {
                directory: tempfile::tempdir().unwrap(),
            };
            let root = fixture.root();
            for name in ["bin", "home", "artifacts"] {
                fs::create_dir(root.join(name)).unwrap();
            }
            symlink(root.join("artifacts"), root.join("target")).unwrap();
            fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
            fs::write(
                root.join("home/.cargo-target-cleaner.toml"),
                "[free_space]\nmin_free_space_gib = 1000000000\nacross_projects = true\n",
            )
            .unwrap();
            fs::write(
                root.join("bin/cargo"),
                r#"#!/bin/sh
case "$1" in
metadata) printf '%s' "$METADATA";;
clean)
  sleep 30 &
  echo $! > "$ROOT/descendant"
  echo $$ > "$ROOT/leader"
  echo "$PPID" > "$ROOT/worker"
  rm -rf "$ROOT/artifacts"
  touch "$ROOT/ready"
  wait;;
*) touch "$ROOT/forwarded"; exit 37;;
esac
"#,
            )
            .unwrap();
            fs::set_permissions(
                root.join("bin/cargo"),
                fs::Permissions::from_mode(EXECUTABLE_MODE),
            )
            .unwrap();
            fixture
        }

        fn root(&self) -> &Path {
            self.directory.path()
        }

        fn command(&self) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-target-cleaner"));
            let mut paths = vec![self.root().join("bin")];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            command.args(["exec", "--", "build"])
                .current_dir(self.root())
                .env("HOME", self.root().join("home"))
                .env("ROOT", self.root())
                .env("PATH", std::env::join_paths(paths).unwrap())
                .env("METADATA", serde_json::json!({
                    "workspace_root": self.root(), "target_directory": self.root().join("target")
                }).to_string())
                .stdout(Stdio::null()).stderr(Stdio::null());
            command
        }

        fn ready(&self, wrapper: &mut Child) {
            let started = Instant::now();
            while !self.root().join("ready").exists() {
                assert!(
                    wrapper.try_wait().unwrap().is_none(),
                    "wrapper exited before cleaning"
                );
                assert!(started.elapsed() < WAIT_TIMEOUT, "cleanup did not start");
                std::thread::sleep(POLL_INTERVAL);
            }
        }

        fn pid(&self, name: &str) -> libc::pid_t {
            fs::read_to_string(self.root().join(name))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        }

        fn wait_for_release(&self) {
            let file = fs::File::open(self.root().join("home/.cargo-target-cleaner/cleanup.lock"))
                .unwrap();
            let started = Instant::now();
            loop {
                match fs2::FileExt::try_lock_exclusive(&file) {
                    Ok(()) => return,
                    Err(error) => assert_eq!(error.kind(), fs2::lock_contended_error().kind()),
                }
                assert!(
                    started.elapsed() < WAIT_TIMEOUT,
                    "worker did not release lock"
                );
                std::thread::sleep(POLL_INTERVAL);
            }
        }

        fn assert_gone(&self, name: &str) {
            // SAFETY: signal zero only checks whether the recorded process exists.
            assert_eq!(
                unsafe { libc::kill(self.pid(name), 0) },
                -1,
                "{name} survived cancellation"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }

        fn reap_worker(&self) {
            #[cfg(target_os = "linux")]
            {
                let started = Instant::now();
                loop {
                    // SAFETY: wait only for the worker adopted by this test process.
                    let result = unsafe {
                        libc::waitpid(self.pid("worker"), std::ptr::null_mut(), libc::WNOHANG)
                    };
                    if result > 0 {
                        break;
                    }
                    assert_eq!(result, 0);
                    assert!(started.elapsed() < WAIT_TIMEOUT);
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
    }

    #[test]
    fn wrapper_termination_stops_descendants_and_restores_target_before_unlocking() {
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            let fixture = Fixture::new();
            let mut wrapper = fixture.command().spawn().unwrap();
            fixture.ready(&mut wrapper);
            // This contender must return promptly without launching another clean.
            let mut contender = fixture.command().spawn().unwrap();
            let started = Instant::now();
            let status = loop {
                if let Some(status) = contender.try_wait().unwrap() {
                    break status;
                }
                assert!(
                    started.elapsed() < WAIT_TIMEOUT,
                    "contender waited for cleanup"
                );
                std::thread::sleep(POLL_INTERVAL);
            };
            assert_eq!(status.code(), Some(FORWARDED_EXIT_CODE));
            assert!(!fixture.root().join("artifacts").exists());
            fs::remove_file(fixture.root().join("forwarded")).unwrap();
            // SAFETY: signal only the wrapper this test spawned, not its group.
            assert_eq!(
                unsafe { libc::kill(libc::pid_t::try_from(wrapper.id()).unwrap(), signal) },
                0
            );
            wrapper.wait().unwrap();
            fixture.wait_for_release();
            fixture.assert_gone("leader");
            fixture.assert_gone("descendant");
            assert!(
                fixture.root().join("target").is_dir(),
                "symlink destination was not restored"
            );
            assert!(
                !fixture.root().join("forwarded").exists(),
                "cancelled wrapper forwarded Cargo"
            );
            fixture.reap_worker();
        }
    }
}
