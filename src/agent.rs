use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crate::{
    config::{Agent, Config},
    error::AgentLoopError,
    state::{log, timestamp},
};

fn resolve_command(agent: Agent, prompt: &str) -> (&'static str, Vec<String>) {
    match agent {
        Agent::Claude => (
            "claude",
            vec![
                "-p".to_string(),
                prompt.to_string(),
                "--verbose".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ],
        ),
        Agent::Codex => (
            "codex",
            vec![
                "exec".to_string(),
                "--skip-git-repo-check".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                prompt.to_string(),
            ],
        ),
    }
}

const SUPERVISOR_TICK: Duration = Duration::from_secs(1);
const FORCE_KILL_GRACE_MS: u64 = 2_000;

fn append_response_block(agent: Agent, output: &str, config: &Config) -> std::io::Result<()> {
    if output.trim().is_empty() {
        return Ok(());
    }

    let log_path = config.state_dir.join("log.txt");
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let separator = "─".repeat(60);
    let block = format!(
        "\n{separator}\n[{}] {} response:\n{separator}\n{output}\n{separator}\n\n",
        timestamp(),
        agent
    );

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    file.write_all(block.as_bytes())
}

fn assistant_text_from_stream_event(value: &serde_json::Value) -> Option<String> {
    let content = value.get("message")?.get("content")?.as_array()?;
    let mut text = String::new();

    for block in content {
        if block.get("type").and_then(|value| value.as_str()) == Some("text")
            && let Some(segment) = block.get("text").and_then(|value| value.as_str())
        {
            text.push_str(segment);
        }
    }

    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn extract_claude_stream_json_text(output: &str) -> Option<String> {
    let mut result_text = None;
    let mut assistant_text = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        match value.get("type").and_then(|value| value.as_str()) {
            Some("result") => {
                if let Some(text) = value.get("result").and_then(|value| value.as_str()) {
                    result_text = Some(text.to_string());
                }
            }
            Some("assistant") => {
                if let Some(text) = assistant_text_from_stream_event(&value) {
                    assistant_text = Some(text);
                }
            }
            _ => {}
        }
    }

    result_text.or(assistant_text)
}

fn normalize_agent_output(agent: Agent, output: String) -> String {
    match agent {
        Agent::Claude => extract_claude_stream_json_text(&output).unwrap_or(output),
        Agent::Codex => output,
    }
}

fn terminate_for_timeout(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child was spawned in its own process group (setpgid(0,0)),
        // so child.id() == pgid. Send SIGTERM to the entire group via -pgid.
        let pgid = child.id() as libc::pid_t;
        let rc = unsafe { libc::kill(-pgid, libc::SIGTERM) };
        if rc == -1 {
            let _ = child.kill();
        }
    }

    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

fn spawn_reader_thread<R>(
    mut reader: R,
    output: Arc<Mutex<Vec<u8>>>,
    last_output_ms: Arc<AtomicU64>,
    start: Instant,
    to_stderr: bool,
) -> JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];

        loop {
            let read_count = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(value) => value,
                Err(_) => break,
            };

            let bytes = &buffer[..read_count];
            if to_stderr {
                let mut stderr = std::io::stderr().lock();
                let _ = stderr.write_all(bytes);
                let _ = stderr.flush();
            } else {
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(bytes);
                let _ = stdout.flush();
            }

            if let Ok(mut combined) = output.lock() {
                combined.extend_from_slice(bytes);
            }

            last_output_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
    })
}

fn join_reader_thread(handle: Option<JoinHandle<()>>) {
    if let Some(join_handle) = handle {
        let _ = join_handle.join();
    }
}

pub fn run_agent(agent: Agent, prompt: &str, config: &Config) -> Result<String, AgentLoopError> {
    let _ = log(&format!("▶ Running {agent}..."), config);

    let (command, args) = resolve_command(agent, prompt);
    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(&config.project_dir)
        // Agent invocations are non-interactive; do not let children read caller TTY input.
        // This keeps Ctrl+C handling at the loop level predictable.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CLAUDECODE");

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let reason = format!("{agent} failed to start: {err}");
            let _ = log(&format!("⚠ {reason}"), config);
            return Err(AgentLoopError::Agent(reason));
        }
    };

    let start = Instant::now();
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let last_output_ms = Arc::new(AtomicU64::new(start.elapsed().as_millis() as u64));

    let stdout_handle = child.stdout.take().map(|stdout| {
        spawn_reader_thread(
            stdout,
            Arc::clone(&output),
            Arc::clone(&last_output_ms),
            start,
            false,
        )
    });
    let stderr_handle = child.stderr.take().map(|stderr| {
        spawn_reader_thread(
            stderr,
            Arc::clone(&output),
            Arc::clone(&last_output_ms),
            start,
            true,
        )
    });

    let mut status = None;
    let mut timed_out = false;
    let mut timed_out_at_ms = None;
    let mut force_kill_attempted = false;
    let mut run_error = None::<String>;
    let mut interrupted = false;

    loop {
        match child.try_wait() {
            Ok(Some(exit_status)) => {
                status = Some(exit_status);
                break;
            }
            Ok(None) => {
                // Check for interrupt signal before anything else.
                if !interrupted && crate::interrupt::is_interrupted() {
                    let _ = log("Interrupted by signal — terminating child process", config);
                    terminate_for_timeout(&mut child);
                    interrupted = true;
                    timed_out_at_ms = Some(start.elapsed().as_millis() as u64);
                }

                let now_ms = start.elapsed().as_millis() as u64;
                let idle_ms = now_ms.saturating_sub(last_output_ms.load(Ordering::Relaxed));
                if !timed_out && !interrupted && idle_ms > config.timeout_seconds.saturating_mul(1000) {
                    let _ = log(
                        &format!("⏱️ Idle timeout: no output for {}s", config.timeout_seconds),
                        config,
                    );
                    terminate_for_timeout(&mut child);
                    timed_out = true;
                    timed_out_at_ms = Some(now_ms);
                }

                if (timed_out || interrupted)
                    && !force_kill_attempted
                    && let Some(timeout_started_ms) = timed_out_at_ms
                    && now_ms.saturating_sub(timeout_started_ms) >= FORCE_KILL_GRACE_MS
                {
                    #[cfg(unix)]
                    {
                        let pgid = child.id() as libc::pid_t;
                        let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    }
                    let _ = child.kill();
                    force_kill_attempted = true;
                }
                thread::sleep(SUPERVISOR_TICK);
            }
            Err(err) => {
                let reason = format!("{agent} process error: {err}");
                let _ = log(&format!("⚠ {reason}"), config);
                run_error = Some(reason);
                break;
            }
        }
    }

    if status.is_none() && run_error.is_none() {
        match child.wait() {
            Ok(exit_status) => status = Some(exit_status),
            Err(err) => {
                let reason = format!("{agent} wait error: {err}");
                let _ = log(&format!("⚠ {reason}"), config);
                run_error = Some(reason);
            }
        }
    }

    if run_error.is_some() {
        // Ensure pipes close so reader threads can finish even after try_wait/wait failures.
        let _ = child.kill();
        let _ = child.wait();
    }

    join_reader_thread(stdout_handle);
    join_reader_thread(stderr_handle);

    let raw_bytes = output
        .lock()
        .map(|mut guard| std::mem::take(&mut *guard))
        .unwrap_or_default();
    let combined_output = String::from_utf8_lossy(&raw_bytes).into_owned();
    let normalized_output = normalize_agent_output(agent, combined_output);
    if let Err(err) = append_response_block(agent, &normalized_output, config) {
        let _ = log(&format!("⚠ {agent} error: {err}"), config);
    }

    if interrupted {
        let reason = "Interrupted by signal".to_string();
        let _ = log(&format!("⚠ {reason}"), config);
        return Err(AgentLoopError::Interrupted(reason));
    }

    if timed_out {
        let reason = format!(
            "{agent} timed out after {}s of inactivity",
            config.timeout_seconds
        );
        let _ = log(&format!("⚠ {reason}"), config);
        return Err(AgentLoopError::Agent(reason));
    }

    if let Some(reason) = run_error {
        return Err(AgentLoopError::Agent(reason));
    }

    let Some(exit_status) = status else {
        let reason = format!("{agent} did not report an exit status");
        let _ = log(&format!("⚠ {reason}"), config);
        return Err(AgentLoopError::Agent(reason));
    };

    if !exit_status.success() {
        let code = exit_status
            .code()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_string());
        let reason = format!("{agent} exited with code {code}");
        let _ = log(&format!("⚠ {reason}"), config);
        return Err(AgentLoopError::Agent(reason));
    }

    Ok(normalized_output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ScopedEnvVar, TestProject, env_lock};

    fn new_project(timeout_seconds: u64) -> TestProject {
        TestProject::builder("agent_loop_agent_test")
            .timeout_seconds(timeout_seconds)
            .build()
    }

    #[test]
    fn resolve_command_builds_expected_claude_invocation() {
        let (command, args) = resolve_command(Agent::Claude, "hello");
        assert_eq!(command, "claude");
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "hello".to_string(),
                "--verbose".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--dangerously-skip-permissions".to_string()
            ]
        );
    }

    #[test]
    fn resolve_command_builds_expected_codex_invocation() {
        let (command, args) = resolve_command(Agent::Codex, "hello");
        assert_eq!(command, "codex");
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--skip-git-repo-check".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "hello".to_string()
            ]
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_removes_claudecode_from_child_environment() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable(
            "claude",
            "#!/bin/sh\nif [ \"${CLAUDECODE+x}\" = \"x\" ]; then\n  printf 'CLAUDECODE=%s\\n' \"$CLAUDECODE\"\nelse\n  echo 'CLAUDECODE=unset'\nfi\n",
        );
        let _path_guard = project.with_path_override();
        let _claudecode_guard = ScopedEnvVar::set("CLAUDECODE", "nested-agent-marker");

        let output =
            run_agent(Agent::Claude, "ignored", &project.config).expect("agent should succeed");
        assert!(output.contains("CLAUDECODE=unset"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_collects_output_from_stdout_and_stderr() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable(
            "claude",
            "#!/bin/sh\nprintf 'out-1\\n'\n/bin/sleep 0.1\nprintf 'err-1\\n' >&2\n/bin/sleep 0.1\nprintf 'out-2\\n'\n",
        );
        let _path_guard = project.with_path_override();

        let output =
            run_agent(Agent::Claude, "ignored", &project.config).expect("agent should succeed");

        assert!(output.contains("out-1"));
        assert!(output.contains("err-1"));
        assert!(output.contains("out-2"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_logs_idle_timeout_once_and_terminates_process() {
        let _env_guard = env_lock();
        let project = new_project(1);
        project.create_executable("claude", "#!/bin/sh\nexec /bin/sleep 10\n");
        let _path_guard = project.with_path_override();

        let started = Instant::now();
        let result = run_agent(Agent::Claude, "ignored", &project.config);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(8),
            "timed out process should terminate quickly"
        );
        assert!(result.is_err());
        let logs = project.read_log();
        assert_eq!(logs.matches("⏱️ Idle timeout: no output for 1s").count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_force_kills_process_that_ignores_sigterm() {
        let _env_guard = env_lock();
        let project = new_project(1);
        project.create_executable(
            "claude",
            "#!/bin/sh\ntrap '' TERM\nwhile true; do\n  /bin/sleep 1\ndone\n",
        );
        let _path_guard = project.with_path_override();

        let started = Instant::now();
        let result = run_agent(Agent::Claude, "ignored", &project.config);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(8),
            "TERM-ignoring process should be force-killed after grace period"
        );
        assert!(result.is_err());
        let logs = project.read_log();
        assert_eq!(logs.matches("⏱️ Idle timeout: no output for 1s").count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_writes_response_block_for_non_empty_output() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable("claude", "#!/bin/sh\nprintf 'hello'\n");
        let _path_guard = project.with_path_override();

        let output =
            run_agent(Agent::Claude, "ignored", &project.config).expect("agent should succeed");
        assert_eq!(output, "hello");

        let separator = "─".repeat(60);
        let logs = project.read_log();
        assert!(logs.contains("claude response:"));
        assert!(logs.contains(&separator));
        assert!(logs.contains(&format!("hello\n{separator}\n\n")));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_skips_response_block_for_whitespace_only_output() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable("claude", "#!/bin/sh\nprintf '  \\n\\t  '\n");
        let _path_guard = project.with_path_override();

        let output =
            run_agent(Agent::Claude, "ignored", &project.config).expect("agent should succeed");
        assert_eq!(output, "  \n\t  ");

        let logs = project.read_log();
        assert!(!logs.contains("claude response:"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_logs_warning_and_returns_error_on_non_zero_exit() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable("claude", "#!/bin/sh\nprintf 'partial-output'\nexit 7\n");
        let _path_guard = project.with_path_override();

        let result = run_agent(Agent::Claude, "ignored", &project.config);
        assert!(result.is_err());

        let logs = project.read_log();
        assert!(logs.contains("⚠ claude exited with code 7"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_logs_warning_with_null_for_signal_exit() {
        let _env_guard = env_lock();
        let project = new_project(5);
        project.create_executable(
            "claude",
            "#!/bin/sh\nprintf 'signal-output\\n'\nkill -TERM $$\n",
        );
        let _path_guard = project.with_path_override();

        let result = run_agent(Agent::Claude, "ignored", &project.config);
        assert!(result.is_err());

        let logs = project.read_log();
        assert!(logs.contains("⚠ claude exited with code null"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_logs_spawn_error_and_returns_error() {
        let _env_guard = env_lock();
        let project = new_project(5);
        fs::create_dir_all(project.bin_dir()).expect("bin dir should exist");
        let _path_guard = project.with_path_override();

        let result = run_agent(Agent::Claude, "ignored", &project.config);
        assert!(result.is_err());

        let logs = project.read_log();
        assert!(logs.contains("⚠ claude failed to start:"));
    }

    #[test]
    #[cfg(unix)]
    fn run_agent_timeout_kills_spawned_subprocesses() {
        let _env_guard = env_lock();
        let project = new_project(1);

        let pid_file = project.root.join("child.pid");
        let pid_file_str = pid_file.display().to_string();

        // Script spawns a background sleep, records its PID via $!, then
        // replaces itself with another sleep. On timeout the entire process
        // group should be killed, including the background child.
        let script =
            format!("#!/bin/sh\n/bin/sleep 60 &\necho $! > {pid_file_str}\nexec /bin/sleep 60\n");
        project.create_executable("claude", &script);
        let _path_guard = project.with_path_override();

        let result = run_agent(Agent::Claude, "ignored", &project.config);
        assert!(result.is_err(), "agent should time out");

        // The PID file must exist and contain a valid PID.
        let pid_contents = fs::read_to_string(&pid_file)
            .expect("child.pid must exist — script should have written it");
        let child_pid: libc::pid_t = pid_contents
            .trim()
            .parse()
            .expect("child.pid must contain a numeric PID");
        assert!(child_pid > 0, "child PID must be positive");

        // Poll for up to 2 seconds to confirm the subprocess is dead.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let alive = unsafe { libc::kill(child_pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "spawned subprocess (pid {child_pid}) should have been killed by group signal"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// A `Read` implementation that yields predefined byte slices, one per call.
    struct ChunkedReader {
        chunks: Vec<Vec<u8>>,
        index: usize,
    }

    impl ChunkedReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self { chunks, index: 0 }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.index >= self.chunks.len() {
                return Ok(0);
            }
            let chunk = &self.chunks[self.index];
            self.index += 1;
            let len = chunk.len().min(buf.len());
            buf[..len].copy_from_slice(&chunk[..len]);
            Ok(len)
        }
    }

    #[test]
    fn reader_thread_preserves_multibyte_utf8_split_across_chunks() {
        // 🦀 is U+1F980, encoded as 4 bytes: F0 9F A6 80
        // Split across two chunks to reproduce the old per-chunk lossy decode bug.
        let chunks = vec![
            b"hello \xF0".to_vec(),           // first byte of 🦀
            b"\x9F\xA6\x80 world\n".to_vec(), // remaining 3 bytes + text
        ];
        let reader = ChunkedReader::new(chunks);

        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let last_output_ms = Arc::new(AtomicU64::new(0));
        let start = Instant::now();

        let handle = spawn_reader_thread(reader, Arc::clone(&output), last_output_ms, start, false);
        handle.join().expect("reader thread should not panic");

        let raw = output.lock().map(|mut g| std::mem::take(&mut *g)).unwrap();
        let result = String::from_utf8_lossy(&raw).into_owned();

        assert_eq!(result, "hello 🦀 world\n");
        assert!(
            !result.contains('\u{FFFD}'),
            "output must not contain replacement characters"
        );
    }

    #[test]
    fn extract_claude_stream_json_text_prefers_result_payload() {
        let output = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"assistant text\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"final result text\"}\n"
        );

        let extracted = extract_claude_stream_json_text(output);
        assert_eq!(extracted.as_deref(), Some("final result text"));
    }

    #[test]
    fn normalize_agent_output_leaves_plain_text_unchanged_for_claude() {
        let output = "plain text output".to_string();
        let normalized = normalize_agent_output(Agent::Claude, output.clone());
        assert_eq!(normalized, output);
    }
}
