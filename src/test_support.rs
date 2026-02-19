use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process,
    sync::{
        Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crate::config::{
    Agent, Config, DEFAULT_DECISIONS_MAX_LINES, DEFAULT_DECOMPOSITION_MAX_ROUNDS,
    DEFAULT_MAX_PARALLEL, DEFAULT_MAX_ROUNDS, DEFAULT_PLANNING_MAX_ROUNDS, DEFAULT_TIMEOUT_SECONDS,
    QualityCommand, RunMode,
};

#[derive(Debug, Clone)]
pub struct TestConfigOptions {
    pub max_rounds: u32,
    pub planning_max_rounds: u32,
    pub decomposition_max_rounds: u32,
    pub timeout_seconds: u64,
    pub implementer: Agent,
    pub single_agent: bool,
    pub auto_commit: bool,
    pub auto_test: bool,
    pub auto_test_cmd: Option<String>,
    pub quality_commands: Vec<QualityCommand>,
    pub compound: bool,
    pub decisions_max_lines: u32,
    pub diff_max_lines: Option<u32>,
    pub context_line_cap: Option<u32>,
    pub planning_context_excerpt_lines: Option<u32>,
    pub max_parallel: u32,
    pub batch_implement: bool,
    pub verbose: bool,
}

impl Default for TestConfigOptions {
    fn default() -> Self {
        Self {
            max_rounds: DEFAULT_MAX_ROUNDS,
            planning_max_rounds: DEFAULT_PLANNING_MAX_ROUNDS,
            decomposition_max_rounds: DEFAULT_DECOMPOSITION_MAX_ROUNDS,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            implementer: Agent::Claude,
            single_agent: false,
            auto_commit: true,
            auto_test: false,
            auto_test_cmd: None,
            quality_commands: Vec::new(),
            compound: true,
            decisions_max_lines: DEFAULT_DECISIONS_MAX_LINES,
            diff_max_lines: None,
            context_line_cap: None,
            planning_context_excerpt_lines: None,
            max_parallel: DEFAULT_MAX_PARALLEL,
            batch_implement: true,
            verbose: false,
        }
    }
}

pub fn make_test_config(root: &Path, options: TestConfigOptions) -> Config {
    let reviewer = if options.single_agent {
        options.implementer
    } else {
        match options.implementer {
            Agent::Claude => Agent::Codex,
            Agent::Codex => Agent::Claude,
        }
    };

    Config {
        project_dir: root.to_path_buf(),
        state_dir: root.join(".agent-loop").join("state"),
        max_rounds: options.max_rounds,
        planning_max_rounds: options.planning_max_rounds,
        decomposition_max_rounds: options.decomposition_max_rounds,
        timeout_seconds: options.timeout_seconds,
        implementer: options.implementer,
        reviewer,
        single_agent: options.single_agent,
        run_mode: if options.single_agent {
            RunMode::SingleAgent
        } else {
            RunMode::DualAgent
        },
        auto_commit: options.auto_commit,
        auto_test: options.auto_test,
        auto_test_cmd: options.auto_test_cmd.clone(),
        quality_commands: options.quality_commands.clone(),
        compound: options.compound,
        decisions_max_lines: options.decisions_max_lines,
        diff_max_lines: options.diff_max_lines,
        context_line_cap: options.context_line_cap,
        planning_context_excerpt_lines: options.planning_context_excerpt_lines,
        max_parallel: options.max_parallel,
        batch_implement: options.batch_implement,
        verbose: options.verbose,
    }
}

pub fn unique_temp_path(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    env::temp_dir().join(format!("{prefix}_{}_{}_{}", process::id(), nanos, seq))
}

pub fn create_temp_project_root(prefix: &str) -> PathBuf {
    let root = unique_temp_path(prefix);
    fs::create_dir_all(&root).expect("test project directory should be created");
    root
}

pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock should not be poisoned")
}

pub struct ScopedEnvVar {
    key: String,
    previous: Option<OsString>,
}

impl ScopedEnvVar {
    pub fn set(key: &str, value: impl AsRef<OsStr>) -> Self {
        let previous = env::var_os(key);
        // SAFETY: tests serialize env mutation with env_lock().
        unsafe {
            env::set_var(key, value);
        }
        Self {
            key: key.to_string(),
            previous,
        }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => {
                // SAFETY: tests serialize env mutation with env_lock().
                unsafe {
                    env::set_var(&self.key, value);
                }
            }
            None => {
                // SAFETY: tests serialize env mutation with env_lock().
                unsafe {
                    env::remove_var(&self.key);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unified TestProject with builder
// ---------------------------------------------------------------------------

pub struct TestProject {
    pub root: PathBuf,
    pub config: Config,
}

pub struct TestProjectBuilder {
    prefix: String,
    options: TestConfigOptions,
    init_git: bool,
}

impl TestProject {
    pub fn builder(prefix: &str) -> TestProjectBuilder {
        TestProjectBuilder {
            prefix: prefix.to_string(),
            options: TestConfigOptions::default(),
            init_git: false,
        }
    }

    #[allow(dead_code)]
    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn read_log(&self) -> String {
        fs::read_to_string(self.config.state_dir.join("log.txt")).unwrap_or_default()
    }

    pub fn write_file(&self, relative_path: &str, content: &str) {
        let full_path = self.root.join(relative_path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).expect("parent directories should be created");
        }
        fs::write(full_path, content).expect("file should be written");
    }

    #[cfg(unix)]
    pub fn create_executable(&self, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let bin_dir = self.bin_dir();
        fs::create_dir_all(&bin_dir).expect("bin directory should be created");

        let script_path = bin_dir.join(name);
        fs::write(&script_path, body).expect("script should be written");
        let permissions = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        script_path
    }

    pub fn with_path_override(&self) -> ScopedEnvVar {
        ScopedEnvVar::set("PATH", self.bin_dir())
    }

    // -- Git helpers ----------------------------------------------------------

    pub fn setup_git_repo(&self) {
        run_git_ok(&self.root, &["init"]);
        run_git_ok(&self.root, &["config", "user.name", "agent-loop-tests"]);
        run_git_ok(
            &self.root,
            &["config", "user.email", "agent-loop-tests@example.com"],
        );
    }

    pub fn commit_all(&self, message: &str) {
        run_git_ok(&self.root, &["add", "-A", "--"]);
        run_git_ok(&self.root, &["commit", "-m", message]);
    }

    pub fn commit_count(&self) -> u32 {
        let output = run_git_raw(&self.root, &["rev-list", "--count", "HEAD"]);
        if !output.status.success() {
            return 0;
        }

        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .expect("commit count should be numeric")
    }

    pub fn head_subject(&self) -> String {
        run_git_ok(&self.root, &["log", "-1", "--pretty=%s"])
            .trim()
            .to_string()
    }

    pub fn head_files(&self) -> Vec<String> {
        let output = run_git_raw(
            &self.root,
            &["show", "--name-only", "--pretty=format:", "-z", "HEAD"],
        );
        assert!(
            output.status.success(),
            "git show failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout)
            .split('\0')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    pub fn tracked_files(&self) -> Vec<String> {
        run_git_ok(&self.root, &["ls-tree", "--name-only", "-r", "HEAD"])
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl TestProjectBuilder {
    pub fn timeout_seconds(mut self, value: u64) -> Self {
        self.options.timeout_seconds = value;
        self
    }

    #[allow(dead_code)]
    pub fn single_agent(mut self, value: bool) -> Self {
        self.options.single_agent = value;
        self
    }

    pub fn auto_commit(mut self, value: bool) -> Self {
        self.options.auto_commit = value;
        self
    }

    #[allow(dead_code)]
    pub fn auto_test(mut self, value: bool) -> Self {
        self.options.auto_test = value;
        self
    }

    #[allow(dead_code)]
    pub fn auto_test_cmd(mut self, value: Option<String>) -> Self {
        self.options.auto_test_cmd = value;
        self
    }

    #[allow(dead_code)]
    pub fn quality_commands(mut self, value: Vec<QualityCommand>) -> Self {
        self.options.quality_commands = value;
        self
    }

    #[allow(dead_code)]
    pub fn compound(mut self, value: bool) -> Self {
        self.options.compound = value;
        self
    }

    #[allow(dead_code)]
    pub fn decisions_max_lines(mut self, value: u32) -> Self {
        self.options.decisions_max_lines = value;
        self
    }

    #[allow(dead_code)]
    pub fn batch_implement(mut self, value: bool) -> Self {
        self.options.batch_implement = value;
        self
    }

    pub fn with_git(mut self) -> Self {
        self.init_git = true;
        self
    }

    pub fn build(self) -> TestProject {
        let root = create_temp_project_root(&self.prefix);
        let config = make_test_config(&root, self.options);
        let project = TestProject { root, config };

        if self.init_git {
            project.setup_git_repo();
        }

        project
    }
}

// ---------------------------------------------------------------------------
// Shared git command helpers (used by TestProject and git.rs tests)
// ---------------------------------------------------------------------------

pub fn run_git_raw(root: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("git command should be invoked")
}

pub fn run_git_ok(root: &Path, args: &[&str]) -> String {
    let output = run_git_raw(root, args);
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).to_string()
}
