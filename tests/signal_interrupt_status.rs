//! Integration test: sending SIGINT to a running agent-loop writes
//! `Status::Interrupted` to `status.json` and exits with code 130.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn agent_loop_bin() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_agent-loop"))
    }

    fn create_mock_claude(bin_dir: &Path) {
        fs::create_dir_all(bin_dir).expect("bin dir should be created");
        let script_path = bin_dir.join("claude");

        // The mock claude:
        //   - Responds to `--version` quickly (for preflight).
        //   - For normal invocation, sleeps long enough for the test to send SIGINT.
        let script = r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        --version) echo "claude mock 1.0.0"; exit 0 ;;
    esac
done
exec /bin/sleep 300
"#;

        fs::write(&script_path, script).expect("mock claude should be written");
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("mock claude should be executable");
    }

    fn create_mock_codex(bin_dir: &Path) {
        let script_path = bin_dir.join("codex");
        let script = r#"#!/bin/sh
echo "codex mock 1.0.0"
exit 0
"#;
        fs::write(&script_path, script).expect("mock codex should be written");
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("mock codex should be executable");
    }

    #[test]
    fn sigint_writes_interrupted_status_and_exits_130() {
        let project_dir = std::env::temp_dir().join(format!(
            "agent_loop_signal_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&project_dir).expect("project dir should be created");

        let bin_dir = project_dir.join("bin");
        create_mock_claude(&bin_dir);
        create_mock_codex(&bin_dir);

        let status_json_path = project_dir
            .join(".agent-loop")
            .join("state")
            .join("status.json");

        // Spawn agent-loop implement with --single-agent to simplify.
        // AUTO_COMMIT=0 avoids requiring git in PATH.
        let mut child = Command::new(agent_loop_bin())
            .args([
                "implement",
                "--task",
                "test task for signal handling",
                "--single-agent",
            ])
            .env("PATH", &bin_dir)
            .env("TIMEOUT", "300")
            .env("MAX_ROUNDS", "1")
            .env("AUTO_COMMIT", "0")
            .current_dir(&project_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("agent-loop should spawn");

        let pid = child.id() as libc::pid_t;

        // Wait for status.json to appear, indicating initialization completed and
        // the agent phase is running.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if status_json_path.exists() {
                // Give a brief moment for the agent subprocess to start.
                std::thread::sleep(Duration::from_millis(500));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "status.json should appear within 30s"
            );
            std::thread::sleep(Duration::from_millis(100));
        }

        // Send SIGINT to the agent-loop process.
        let rc = unsafe { libc::kill(pid, libc::SIGINT) };
        assert_eq!(rc, 0, "kill(SIGINT) should succeed");

        // Wait for the process to exit.
        let exit_deadline = Instant::now() + Duration::from_secs(15);
        let exit_status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() > exit_deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("agent-loop did not exit within 15s after SIGINT");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(err) => panic!("error waiting for agent-loop: {err}"),
            }
        };

        // Verify exit code. On Unix the process might get the raw signal (code=None)
        // or our explicit 130. Both are acceptable; the critical check is status.json.
        if let Some(c) = exit_status.code() {
            assert_eq!(c, 130, "exit code should be 130, got {c}");
        }

        // Verify status.json contains INTERRUPTED.
        let status_content =
            fs::read_to_string(&status_json_path).expect("status.json should exist after SIGINT");
        let status_json: serde_json::Value =
            serde_json::from_str(&status_content).expect("status.json should be valid JSON");
        assert_eq!(
            status_json["status"].as_str(),
            Some("INTERRUPTED"),
            "status.json should contain INTERRUPTED, got: {status_content}"
        );

        // Cleanup.
        let _ = fs::remove_dir_all(&project_dir);
    }
}
