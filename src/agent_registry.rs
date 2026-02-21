use std::collections::BTreeMap;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    ClaudeStreamJson,
    PlainText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Stable,
    Experimental,
}

// ---------------------------------------------------------------------------
// AgentSpec
// ---------------------------------------------------------------------------

pub struct AgentSpec {
    pub name: &'static str,
    pub binary: &'static str,
    pub install_hint: &'static str,
    pub default_reviewer: &'static str,
    pub command_builder: fn(&str, Option<&str>) -> Vec<String>,
    pub output_format: OutputFormat,
    pub tier: Tier,
    pub probe_args: &'static [&'static str],
    pub supports_model_flag: bool,
    pub supports_session_resume: bool,
}

// ---------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------

fn claude_command(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        prompt.to_string(),
        "--verbose".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    args
}

fn codex_command(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        "--skip-git-repo-check".to_string(),
        "--json".to_string(),
        "--color".to_string(),
        "never".to_string(),
    ];
    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }
    args.push(prompt.to_string());
    args
}

fn gemini_command(prompt: &str, _model: Option<&str>) -> Vec<String> {
    vec![prompt.to_string()]
}

fn aider_command(prompt: &str, _model: Option<&str>) -> Vec<String> {
    vec!["--message".to_string(), prompt.to_string()]
}

fn qwen_command(prompt: &str, _model: Option<&str>) -> Vec<String> {
    vec![prompt.to_string()]
}

fn vibe_command(prompt: &str, _model: Option<&str>) -> Vec<String> {
    vec![prompt.to_string()]
}

fn deepseek_command(prompt: &str, _model: Option<&str>) -> Vec<String> {
    vec![prompt.to_string()]
}

// ---------------------------------------------------------------------------
// Static registry
// ---------------------------------------------------------------------------

static REGISTRY: LazyLock<BTreeMap<&'static str, AgentSpec>> = LazyLock::new(|| {
    let mut m = BTreeMap::new();

    m.insert(
        "claude",
        AgentSpec {
            name: "claude",
            binary: "claude",
            install_hint: "npm install -g @anthropic-ai/claude-code",
            default_reviewer: "codex",
            command_builder: claude_command,
            output_format: OutputFormat::ClaudeStreamJson,
            tier: Tier::Stable,
            probe_args: &["--version"],
            supports_model_flag: true,
            supports_session_resume: true,
        },
    );

    m.insert(
        "codex",
        AgentSpec {
            name: "codex",
            binary: "codex",
            install_hint: "npm install -g @openai/codex",
            default_reviewer: "claude",
            command_builder: codex_command,
            output_format: OutputFormat::PlainText,
            tier: Tier::Stable,
            probe_args: &["--version"],
            supports_model_flag: true,
            supports_session_resume: true,
        },
    );

    m.insert(
        "gemini",
        AgentSpec {
            name: "gemini",
            binary: "gemini",
            install_hint: "Install Gemini CLI",
            default_reviewer: "claude",
            command_builder: gemini_command,
            output_format: OutputFormat::PlainText,
            tier: Tier::Experimental,
            probe_args: &["--version"],
            supports_model_flag: false,
            supports_session_resume: false,
        },
    );

    m.insert(
        "aider",
        AgentSpec {
            name: "aider",
            binary: "aider",
            install_hint: "pip install aider-chat",
            default_reviewer: "claude",
            command_builder: aider_command,
            output_format: OutputFormat::PlainText,
            tier: Tier::Experimental,
            probe_args: &["--version"],
            supports_model_flag: false,
            supports_session_resume: false,
        },
    );

    m.insert(
        "qwen",
        AgentSpec {
            name: "qwen",
            binary: "qwen",
            install_hint: "Install Qwen CLI",
            default_reviewer: "claude",
            command_builder: qwen_command,
            output_format: OutputFormat::PlainText,
            tier: Tier::Experimental,
            probe_args: &["--version"],
            supports_model_flag: false,
            supports_session_resume: false,
        },
    );

    m.insert(
        "vibe",
        AgentSpec {
            name: "vibe",
            binary: "vibe",
            install_hint: "Install Vibe CLI",
            default_reviewer: "claude",
            command_builder: vibe_command,
            output_format: OutputFormat::PlainText,
            tier: Tier::Experimental,
            probe_args: &["--version"],
            supports_model_flag: false,
            supports_session_resume: false,
        },
    );

    m.insert(
        "deepseek",
        AgentSpec {
            name: "deepseek",
            binary: "deepseek",
            install_hint: "Install DeepSeek CLI",
            default_reviewer: "claude",
            command_builder: deepseek_command,
            output_format: OutputFormat::PlainText,
            tier: Tier::Experimental,
            probe_args: &["--version"],
            supports_model_flag: false,
            supports_session_resume: false,
        },
    );

    m
});

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Look up an agent specification by name.
pub fn get_agent_spec(name: &str) -> Option<&'static AgentSpec> {
    REGISTRY.get(name)
}

/// Validate that every agent's `default_reviewer` references another valid
/// agent in the registry. Panics on the first violation found.
pub fn validate_registry() {
    for (name, spec) in REGISTRY.iter() {
        assert!(
            REGISTRY.contains_key(spec.default_reviewer),
            "agent '{name}': default_reviewer '{}' is not a known agent",
            spec.default_reviewer,
        );
    }
}

/// Returns `true` if `name` is a registered agent.
pub fn is_known_agent(name: &str) -> bool {
    REGISTRY.contains_key(name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_AGENTS: [&str; 7] = [
        "claude", "codex", "gemini", "aider", "qwen", "vibe", "deepseek",
    ];

    #[test]
    fn registry_lookup_succeeds_for_all_agents() {
        for name in &ALL_AGENTS {
            assert!(
                get_agent_spec(name).is_some(),
                "expected agent '{name}' to be in the registry",
            );
        }
    }

    #[test]
    fn unknown_agent_returns_none() {
        assert!(get_agent_spec("nonexistent").is_none());
    }

    #[test]
    fn validate_registry_passes() {
        validate_registry(); // should not panic
    }

    #[test]
    fn every_default_reviewer_is_valid() {
        for name in &ALL_AGENTS {
            let spec = get_agent_spec(name).unwrap();
            assert!(
                is_known_agent(spec.default_reviewer),
                "agent '{name}': default_reviewer '{}' is not in the registry",
                spec.default_reviewer,
            );
        }
    }

    #[test]
    fn claude_command_builder_without_model() {
        let args = claude_command("do stuff", None);
        assert_eq!(
            args,
            vec!["-p", "do stuff", "--verbose", "--output-format", "stream-json"],
        );
    }

    #[test]
    fn claude_command_builder_with_model() {
        let args = claude_command("do stuff", Some("opus"));
        assert_eq!(
            args,
            vec![
                "-p",
                "do stuff",
                "--verbose",
                "--output-format",
                "stream-json",
                "--model",
                "opus",
            ],
        );
    }

    #[test]
    fn codex_command_builder_without_model() {
        let args = codex_command("do stuff", None);
        assert_eq!(
            args,
            vec!["exec", "--skip-git-repo-check", "--json", "--color", "never", "do stuff"],
        );
    }

    #[test]
    fn codex_command_builder_with_model() {
        let args = codex_command("do stuff", Some("gpt-4"));
        assert_eq!(
            args,
            vec![
                "exec",
                "--skip-git-repo-check",
                "--json",
                "--color",
                "never",
                "-m",
                "gpt-4",
                "do stuff",
            ],
        );
    }
}
