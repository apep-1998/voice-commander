//! The sinks that drive an external tool: the clipboard, keystroke injection, notifications.
//!
//! Each is barely more than an argv and an exit code. What they mostly need to get right is
//! the *diagnosis* when the tool is missing — "wl-copy is not installed" is actionable, and
//! "No such file or directory (os error 2)" is not.

use std::sync::Arc;
use std::time::Duration;

use vc_core::config::{ClipboardSink, NotifySink, TypeSink, TypeTool, Urgency};
use vc_exec::{CommandSpec, ExecError, Runner};

use crate::{Sink, SinkContext, SinkError};

/// Run a tool, turning a missing program into an explanation.
async fn invoke(
    runner: &Arc<dyn Runner>,
    sink: &str,
    what: &str,
    argv: Vec<String>,
    timeout: Duration,
    detaches: bool,
) -> Result<(), SinkError> {
    let mut spec = CommandSpec::new(argv).with_timeout(timeout);
    if detaches {
        spec = spec.detaching();
    }
    let program = spec.program().to_owned();

    let output = runner.run(&spec).await.map_err(|error| match error {
        ExecError::NotFound { .. } => SinkError::Config(format!(
            "{sink}: {program} is not installed, and it is what {what}"
        )),
        other => SinkError::Failed(other.to_string()),
    })?;

    if output.timed_out {
        return Err(SinkError::Transient(output.failure_reason()));
    }
    if !output.succeeded() {
        return Err(SinkError::Failed(format!(
            "{program}: {}",
            output.failure_reason()
        )));
    }
    Ok(())
}

/// Copies the transcript to the Wayland clipboard.
#[derive(Debug)]
pub struct Clipboard {
    name: String,
    config: ClipboardSink,
    timeout: Duration,
    runner: Arc<dyn Runner>,
}

impl Clipboard {
    pub fn new(
        name: String,
        config: ClipboardSink,
        timeout_ms: u64,
        runner: Arc<dyn Runner>,
    ) -> Self {
        Self {
            name,
            config,
            timeout: Duration::from_millis(timeout_ms),
            runner,
        }
    }
}

#[async_trait::async_trait]
impl Sink for Clipboard {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "clipboard"
    }
    fn requires_text(&self) -> bool {
        true
    }

    async fn deliver(&self, context: &SinkContext) -> Result<(), SinkError> {
        let text = context.text.clone().unwrap_or_default();

        // `--` and then the text as one argument: a transcript beginning with a dash would
        // otherwise be read as an option by wl-copy.
        let mut argv = vec!["wl-copy".to_owned()];
        if self.config.primary {
            argv.push("--primary".to_owned());
        }
        argv.push("--".to_owned());
        argv.push(text);

        invoke(
            &self.runner,
            &self.name,
            "puts text on the Wayland clipboard",
            argv,
            self.timeout,
            // wl-copy forks to keep owning the selection until something replaces it.
            true,
        )
        .await
    }
}

/// Types the transcript into whatever window has focus.
#[derive(Debug)]
pub struct Typist {
    name: String,
    config: TypeSink,
    timeout: Duration,
    runner: Arc<dyn Runner>,
}

impl Typist {
    pub fn new(name: String, config: TypeSink, timeout_ms: u64, runner: Arc<dyn Runner>) -> Self {
        Self {
            name,
            config,
            timeout: Duration::from_millis(timeout_ms),
            runner,
        }
    }

    /// Pick the tool to type with.
    ///
    /// `wtype` is preferred because it speaks the Wayland virtual-keyboard protocol directly;
    /// `ydotool` needs a running daemon and access to `/dev/uinput`, so it is the fallback
    /// rather than the default.
    fn argv(&self, text: &str) -> Result<Vec<String>, SinkError> {
        let tool = match self.config.tool {
            TypeTool::Wtype => "wtype",
            TypeTool::Ydotool => "ydotool",
            TypeTool::Auto => {
                if self.runner.has("wtype") {
                    "wtype"
                } else if self.runner.has("ydotool") {
                    "ydotool"
                } else {
                    return Err(SinkError::Config(format!(
                        "{}: neither wtype nor ydotool is installed, and one of them is what \
                         types the text into the focused window",
                        self.name
                    )));
                }
            }
        };

        let mut argv = vec![tool.to_owned()];
        match tool {
            "wtype" => {
                if self.config.key_delay_ms > 0 {
                    argv.push("-d".to_owned());
                    argv.push(self.config.key_delay_ms.to_string());
                }
                // Everything after `--` is literal text, so a transcript starting with a
                // dash is typed rather than parsed as a flag.
                argv.push("--".to_owned());
                argv.push(text.to_owned());
            }
            _ => {
                argv.push("type".to_owned());
                if self.config.key_delay_ms > 0 {
                    argv.push("--key-delay".to_owned());
                    argv.push(self.config.key_delay_ms.to_string());
                }
                argv.push("--".to_owned());
                argv.push(text.to_owned());
            }
        }
        Ok(argv)
    }
}

#[async_trait::async_trait]
impl Sink for Typist {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "type"
    }
    fn requires_text(&self) -> bool {
        true
    }

    async fn deliver(&self, context: &SinkContext) -> Result<(), SinkError> {
        let text = context.text.clone().unwrap_or_default();
        if text.is_empty() {
            // Nothing to type is not a failure, and invoking the tool with an empty string
            // can still steal focus events.
            return Ok(());
        }
        let argv = self.argv(&text)?;
        invoke(
            &self.runner,
            &self.name,
            "types into the focused window",
            argv,
            self.timeout,
            false,
        )
        .await
    }
}

/// Raises a desktop notification.
#[derive(Debug)]
pub struct Notifier {
    name: String,
    config: NotifySink,
    timeout: Duration,
    runner: Arc<dyn Runner>,
}

impl Notifier {
    pub fn new(name: String, config: NotifySink, timeout_ms: u64, runner: Arc<dyn Runner>) -> Self {
        Self {
            name,
            config,
            timeout: Duration::from_millis(timeout_ms),
            runner,
        }
    }
}

#[async_trait::async_trait]
impl Sink for Notifier {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "notify"
    }
    fn requires_text(&self) -> bool {
        // A notification saying a recording finished is useful with no transcript at all.
        false
    }

    async fn deliver(&self, context: &SinkContext) -> Result<(), SinkError> {
        let urgency = match self.config.urgency {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        };

        let argv = vec![
            "notify-send".to_owned(),
            "-u".to_owned(),
            urgency.to_owned(),
            "-t".to_owned(),
            self.config.expire_ms.to_string(),
            "--".to_owned(),
            context.tokens.expand(&self.config.summary),
            context.tokens.expand(&self.config.body),
        ];

        invoke(
            &self.runner,
            &self.name,
            "raises desktop notifications",
            argv,
            self.timeout,
            false,
        )
        .await
    }
}
