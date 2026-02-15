use std::fmt;
use std::io;

/// Unified error type for the agent-loop crate.
///
/// Each variant maps to a distinct failure domain so callers can match on the
/// category without parsing free-form strings.
#[derive(Debug)]
pub enum AgentLoopError {
    /// Filesystem or process I/O failure.
    Io(io::Error),
    /// Git operation failure (command error, unexpected output, etc.).
    Git(String),
    /// Agent execution failure (spawn, timeout, non-zero exit, etc.).
    Agent(String),
    /// Configuration error (CLI validation, TOML parse, env conflict, etc.).
    Config(String),
    /// State management error (status.json parse, missing files, etc.).
    State(String),
}

impl fmt::Display for AgentLoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Git(msg) => write!(f, "Git error: {msg}"),
            Self::Agent(msg) => write!(f, "Agent error: {msg}"),
            Self::Config(msg) => write!(f, "Config error: {msg}"),
            Self::State(msg) => write!(f, "State error: {msg}"),
        }
    }
}

impl std::error::Error for AgentLoopError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Git(_) | Self::Agent(_) | Self::Config(_) | Self::State(_) => None,
        }
    }
}

impl From<io::Error> for AgentLoopError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for AgentLoopError {
    fn from(err: serde_json::Error) -> Self {
        Self::State(err.to_string())
    }
}

impl From<toml::de::Error> for AgentLoopError {
    fn from(err: toml::de::Error) -> Self {
        Self::Config(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_io_error_produces_io_variant() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let err: AgentLoopError = io_err.into();
        assert!(matches!(err, AgentLoopError::Io(_)));
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn from_serde_json_error_produces_state_variant() {
        let json_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let err: AgentLoopError = json_err.into();
        assert!(matches!(err, AgentLoopError::State(_)));
        assert!(err.to_string().contains("State error:"));
    }

    #[test]
    fn from_toml_error_produces_config_variant() {
        let toml_err = toml::from_str::<toml::Value>("{{invalid").unwrap_err();
        let err: AgentLoopError = toml_err.into();
        assert!(matches!(err, AgentLoopError::Config(_)));
        assert!(err.to_string().contains("Config error:"));
    }

    #[test]
    fn display_io_variant_is_informative() {
        let err = AgentLoopError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "access denied",
        ));
        let display = err.to_string();
        assert!(display.starts_with("I/O error:"));
        assert!(display.contains("access denied"));
    }

    #[test]
    fn display_git_variant_is_informative() {
        let err = AgentLoopError::Git("git status failed: exit code 128".to_string());
        let display = err.to_string();
        assert!(display.starts_with("Git error:"));
        assert!(display.contains("git status failed"));
    }

    #[test]
    fn display_agent_variant_is_informative() {
        let err = AgentLoopError::Agent("claude timed out after 600s".to_string());
        let display = err.to_string();
        assert!(display.starts_with("Agent error:"));
        assert!(display.contains("claude timed out"));
    }

    #[test]
    fn display_config_variant_is_informative() {
        let err = AgentLoopError::Config("unknown key 'foo' in .agent-loop.toml".to_string());
        let display = err.to_string();
        assert!(display.starts_with("Config error:"));
        assert!(display.contains("unknown key"));
    }

    #[test]
    fn display_state_variant_is_informative() {
        let err = AgentLoopError::State("status.json missing required field".to_string());
        let display = err.to_string();
        assert!(display.starts_with("State error:"));
        assert!(display.contains("status.json"));
    }

    #[test]
    fn error_source_returns_inner_for_io_variant() {
        let io_err = io::Error::new(io::ErrorKind::Other, "test");
        let err = AgentLoopError::Io(io_err);
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn error_source_returns_none_for_string_variants() {
        let variants: Vec<AgentLoopError> = vec![
            AgentLoopError::Git("msg".to_string()),
            AgentLoopError::Agent("msg".to_string()),
            AgentLoopError::Config("msg".to_string()),
            AgentLoopError::State("msg".to_string()),
        ];
        for err in &variants {
            assert!(
                std::error::Error::source(err).is_none(),
                "source() should be None for {:?}",
                err
            );
        }
    }
}
