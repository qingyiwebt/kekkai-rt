use anyhow::Context;
use async_trait::async_trait;
use tokio::process::Command;
use tracing::debug;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CommandSpec {
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) context: &'static str,
}

impl CommandSpec {
    pub(super) fn new<I, S>(program: impl Into<String>, args: I, context: &'static str) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            context,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CommandOutput {
    success: bool,
    stderr: String,
}

impl CommandOutput {
    pub(super) fn new(success: bool, stderr: impl Into<String>) -> Self {
        Self {
            success,
            stderr: stderr.into(),
        }
    }

    pub(super) fn success(&self) -> bool {
        self.success
    }

    pub(super) fn stderr(&self) -> &str {
        &self.stderr
    }
}

#[async_trait]
pub(super) trait CommandExecutor: Send + Sync {
    async fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput>;
}

#[derive(Default)]
pub(super) struct TokioCommandExecutor;

#[async_trait]
impl CommandExecutor for TokioCommandExecutor {
    async fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput> {
        debug!(
            program = %command.program,
            args = ?command.args,
            context = command.context,
            "executing host command"
        );
        let output = Command::new(&command.program)
            .args(&command.args)
            .output()
            .await
            .with_context(|| format!("{}: execute {}", command.context, command.program))?;
        Ok(CommandOutput::new(
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ))
    }
}
