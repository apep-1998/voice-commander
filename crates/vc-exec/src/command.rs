//! Spawning a child process with a deadline.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::debug;

/// Why running a command failed.
///
/// A command that ran and exited non-zero is *not* an error here — that is a
/// [`CommandOutput`] with a status. This is for the cases where the program never got to run
/// its own logic.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("{program} is not on PATH or is not executable")]
    NotFound { program: String },
    #[error("could not start {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the command was empty")]
    Empty,
    #[error("reading the output of {program}: {source}")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// What to run.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Argv, already token-expanded. `argv[0]` is the program.
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    /// Written to the child's standard input, which is then closed.
    pub stdin: Option<String>,
    pub timeout: Duration,
}

impl CommandSpec {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            env: BTreeMap::new(),
            cwd: None,
            stdin: None,
            timeout: Duration::from_secs(60),
        }
    }

    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn with_cwd(mut self, cwd: Option<PathBuf>) -> Self {
        self.cwd = cwd;
        self
    }

    pub fn with_stdin(mut self, stdin: Option<String>) -> Self {
        self.stdin = stdin;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn program(&self) -> &str {
        self.argv.first().map_or("", String::as_str)
    }
}

/// What a command did.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Exit status, or `None` if it was killed by a signal or by the deadline.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub elapsed: Duration,
}

impl CommandOutput {
    pub fn succeeded(&self) -> bool {
        self.status == Some(0) && !self.timed_out
    }

    /// A one-line summary suitable for an error message.
    ///
    /// Prefers the child's own stderr, because a script that explains its own failure is
    /// more useful than "exited with status 1" — and falls back to the status when it said
    /// nothing.
    pub fn failure_reason(&self) -> String {
        if self.timed_out {
            return format!("timed out after {:?}", self.elapsed);
        }
        let complaint = self.stderr.trim();
        if !complaint.is_empty() {
            return complaint.lines().next().unwrap_or(complaint).to_owned();
        }
        match self.status {
            Some(code) => format!("exited with status {code}"),
            None => "killed by a signal".to_owned(),
        }
    }
}

/// Run a command, killing it if it outlives its deadline.
///
/// Killing matters. Abandoning a slow child would leave it holding the microphone's
/// recording open, writing to a file the next session is about to use, and accumulating one
/// stray process per keypress until something notices.
pub async fn run(spec: &CommandSpec) -> Result<CommandOutput, ExecError> {
    let Some((program, args)) = spec.argv.split_first() else {
        return Err(ExecError::Empty);
    };
    let program = expand_tilde(program);

    let mut command = Command::new(&program);
    command
        .args(args)
        .envs(&spec.env)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Without this, a child that outlives us keeps running after the daemon exits.
        .kill_on_drop(true);

    if let Some(cwd) = &spec.cwd {
        command.current_dir(expand_tilde(&cwd.display().to_string()));
    }

    let started = Instant::now();
    let mut child = command.spawn().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ExecError::NotFound {
                program: program.clone(),
            }
        } else {
            ExecError::Spawn {
                program: program.clone(),
                source,
            }
        }
    })?;

    if let Some(input) = &spec.stdin {
        if let Some(mut pipe) = child.stdin.take() {
            // A child that never reads its input is normal — a shell script using only `$1`
            // will not — so a broken pipe here is not a failure.
            let _ = pipe.write_all(input.as_bytes()).await;
            let _ = pipe.shutdown().await;
        }
    }

    match tokio::time::timeout(spec.timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(CommandOutput {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            timed_out: false,
            elapsed: started.elapsed(),
        }),
        Ok(Err(source)) => Err(ExecError::Io { program, source }),
        Err(_) => {
            debug!(program, timeout = ?spec.timeout, "killing a command that ran too long");
            // `kill_on_drop` handles this as the child is dropped, but saying so explicitly
            // means the process is gone before this function returns rather than whenever
            // the future happens to be dropped.
            Ok(CommandOutput {
                status: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: true,
                elapsed: started.elapsed(),
            })
        }
    }
}

/// Expand a leading `~`, which users write in config files and `Command` does not understand.
///
/// Only a leading `~/` or a bare `~`: `~user` is someone else's home directory and resolving
/// it needs a passwd lookup nobody here has asked for.
pub fn expand_tilde(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_owned();
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return path.to_owned();
    }
    match std::env::var_os("HOME") {
        Some(home) => format!("{}{rest}", home.to_string_lossy()),
        None => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(argv: &[&str]) -> CommandSpec {
        CommandSpec::new(argv.iter().map(|arg| (*arg).to_owned()).collect())
            .with_timeout(Duration::from_secs(5))
    }

    #[tokio::test]
    async fn stdout_comes_back() {
        let output = run(&spec(&["/bin/echo", "hello"])).await.expect("runs");
        assert!(output.succeeded());
        assert_eq!(output.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_a_result_not_an_error() {
        // The program ran and made a decision. That is information, not a failure to run.
        let output = run(&spec(&["/bin/sh", "-c", "exit 3"]))
            .await
            .expect("runs");
        assert!(!output.succeeded());
        assert_eq!(output.status, Some(3));
    }

    #[tokio::test]
    async fn a_missing_program_says_so_by_name() {
        let error = run(&spec(&["definitely-not-a-real-program-xyz"]))
            .await
            .expect_err("should fail");
        assert!(matches!(error, ExecError::NotFound { .. }), "got {error:?}");
        assert!(error
            .to_string()
            .contains("definitely-not-a-real-program-xyz"));
    }

    #[tokio::test]
    async fn an_empty_command_is_rejected() {
        assert!(matches!(
            run(&CommandSpec::new(Vec::new())).await,
            Err(ExecError::Empty)
        ));
    }

    #[tokio::test]
    async fn a_slow_command_is_killed_at_its_deadline() {
        // Abandoning it instead would leave one stray process per keypress.
        let started = Instant::now();
        let output = run(&spec(&["/bin/sleep", "30"]).with_timeout(Duration::from_millis(200)))
            .await
            .expect("runs");

        assert!(output.timed_out);
        assert!(!output.succeeded());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waited {:?} for a 200ms deadline",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_killed_command_reports_the_deadline_as_its_reason() {
        let output = run(&spec(&["/bin/sleep", "30"]).with_timeout(Duration::from_millis(100)))
            .await
            .expect("runs");
        assert!(
            output.failure_reason().contains("timed out"),
            "{}",
            output.failure_reason()
        );
    }

    #[tokio::test]
    async fn arguments_are_passed_without_a_shell() {
        // The single most important property: a transcript cannot become a second command.
        let output = run(&spec(&[
            "/bin/echo",
            "hello; touch /tmp/vc-should-not-exist",
        ]))
        .await
        .expect("runs");

        assert_eq!(
            output.stdout.trim(),
            "hello; touch /tmp/vc-should-not-exist",
            "the argument was interpreted instead of passed through"
        );
        assert!(!std::path::Path::new("/tmp/vc-should-not-exist").exists());
    }

    #[tokio::test]
    async fn stdin_is_delivered_and_closed() {
        let output = run(&spec(&["/bin/cat"]).with_stdin(Some("piped input".to_owned())))
            .await
            .expect("runs");
        // Closing matters: `cat` never exits otherwise and this would hit the deadline.
        assert!(output.succeeded());
        assert_eq!(output.stdout, "piped input");
    }

    #[tokio::test]
    async fn a_child_that_ignores_stdin_is_not_a_failure() {
        // A shell script using only `$1` never reads it, and the resulting broken pipe must
        // not be reported as the command failing.
        let output = run(&spec(&["/bin/echo", "ignored"]).with_stdin(Some("x".repeat(100_000))))
            .await
            .expect("runs");
        assert!(output.succeeded());
    }

    #[tokio::test]
    async fn the_environment_reaches_the_child() {
        let mut env = BTreeMap::new();
        env.insert("VC_TEXT".to_owned(), "open my calendar".to_owned());

        let output = run(&spec(&["/bin/sh", "-c", "printf %s \"$VC_TEXT\""]).with_env(env))
            .await
            .expect("runs");
        assert_eq!(output.stdout, "open my calendar");
    }

    #[tokio::test]
    async fn the_working_directory_is_honoured() {
        let dir = tempfile::tempdir().expect("temp dir");
        let output = run(&spec(&["/bin/pwd"]).with_cwd(Some(dir.path().to_owned())))
            .await
            .expect("runs");

        // macOS and some setups symlink temp directories, so compare the resolved paths.
        let reported = std::fs::canonicalize(output.stdout.trim()).expect("canonicalize");
        let expected = std::fs::canonicalize(dir.path()).expect("canonicalize");
        assert_eq!(reported, expected);
    }

    #[tokio::test]
    async fn a_failure_reason_prefers_what_the_program_said() {
        // "model file not found" is worth more to a user than "exited with status 1".
        let output = run(&spec(&[
            "/bin/sh",
            "-c",
            "echo 'model file not found' >&2; exit 1",
        ]))
        .await
        .expect("runs");
        assert_eq!(output.failure_reason(), "model file not found");
    }

    #[tokio::test]
    async fn a_silent_failure_falls_back_to_the_exit_status() {
        let output = run(&spec(&["/bin/sh", "-c", "exit 7"]))
            .await
            .expect("runs");
        assert_eq!(output.failure_reason(), "exited with status 7");
    }

    #[test]
    fn a_leading_tilde_becomes_the_home_directory() {
        // Users write `~/.local/bin/script.sh` in config files, and `Command` does not
        // understand it — the shipped example config uses exactly that form.
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(expand_tilde("~/.local/bin/x"), "/home/tester/.local/bin/x");
        assert_eq!(expand_tilde("~"), "/home/tester");
    }

    #[test]
    fn a_tilde_elsewhere_is_left_alone() {
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(expand_tilde("/opt/a~b"), "/opt/a~b");
        // `~user` needs a passwd lookup nobody has asked for.
        assert_eq!(expand_tilde("~root/x"), "~root/x");
    }
}
