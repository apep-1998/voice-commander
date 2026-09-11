//! The `command` sink: run a program.
//!
//! The general case, and the reason this project exists. Everything else here is a
//! convenience that could have been written as a script.

use std::time::Duration;

use tracing::debug;
use vc_core::config::CommandSink as Config;
use vc_exec::{CommandSpec, ExecError};

use crate::{Sink, SinkContext, SinkError};

#[derive(Debug)]
pub struct CommandSink {
    name: String,
    config: Config,
    requires_text: bool,
    timeout: Duration,
}

impl CommandSink {
    pub fn new(name: String, config: Config, requires_text: bool, timeout_ms: u64) -> Self {
        Self {
            name,
            config,
            requires_text,
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

#[async_trait::async_trait]
impl Sink for CommandSink {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "command"
    }

    fn requires_text(&self) -> bool {
        self.requires_text
    }

    async fn deliver(&self, context: &SinkContext) -> Result<(), SinkError> {
        for unknown in context.tokens.unknown_in(&self.config.cmd.join(" ")) {
            tracing::warn!(
                sink = self.name,
                token = unknown,
                "unknown token in cmd; it will be passed through literally"
            );
        }

        // The program is handed the same values three ways, so a one-line shell script can
        // use `$1` and a real program can parse JSON, without the user having to write in
        // whichever style we happened to pick.
        let mut env = context.tokens.env();
        env.extend(self.config.env.clone());

        let cwd = self
            .config
            .cwd
            .clone()
            .map(|cwd| context.tokens.expand(&cwd).into())
            // Defaulting to the session directory lets a script write alongside the
            // recording without being told where that is.
            .or_else(|| context.record.audio.path.parent().map(Path::to_path_buf));

        let spec = CommandSpec::new(context.tokens.expand_args(&self.config.cmd))
            .with_env(env)
            .with_cwd(cwd)
            .with_stdin(self.config.stdin_json.then(|| context.record_json()))
            .with_timeout(self.timeout);

        debug!(sink = self.name, argv = ?spec.argv, "running callback");

        let output = vc_exec::run(&spec).await.map_err(|error| match error {
            // A missing program will still be missing next time.
            ExecError::NotFound { .. } | ExecError::Empty => SinkError::Config(error.to_string()),
            other => SinkError::Failed(other.to_string()),
        })?;

        if output.timed_out {
            return Err(SinkError::Transient(output.failure_reason()));
        }
        if !output.succeeded() {
            // The program ran and decided to fail. Running it again does the same thing.
            return Err(SinkError::Failed(output.failure_reason()));
        }
        Ok(())
    }
}

use std::path::Path;
