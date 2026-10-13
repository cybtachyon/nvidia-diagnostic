//! `CommandRunner` (PLAN §4.2): spawn/parse external commands, tolerant of
//! missing tools. A missing binary or non-zero exit never aborts a run; the
//! caller records a `ProbeResult::Degraded` instead.

use std::process::Command;

/// Result of one external command.
#[derive(Debug, Clone)]
pub struct CmdOutput {
    /// True when the process ran and exited 0.
    pub ok: bool,
    /// True when the binary could not be found at all (as opposed to a
    /// non-zero exit of a present tool).
    pub missing: bool,
    /// True when the process was terminated by SIGINT (Ctrl+C). That kill is
    /// user-initiated, not a real tool failure, so callers must not treat it
    /// as one (e.g. nvidia-smi "stopped responding").
    pub killed_by_sigint: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    /// Trimmed stdout when the command succeeded, else `None`.
    pub fn stdout_if_ok(&self) -> Option<String> {
        if self.ok {
            Some(self.stdout.trim().to_string())
        } else {
            None
        }
    }

    /// Best-effort one-line explanation for why this command failed.
    pub fn failure_reason(&self) -> String {
        if self.missing {
            "command not found".to_string()
        } else {
            let stderr = self.stderr.trim();
            if stderr.is_empty() {
                format!("exited with {:?}", self.exit_code)
            } else {
                // Keep it short for report text.
                let first = stderr.lines().next().unwrap_or("");
                format!("exited with {:?}: {first}", self.exit_code)
            }
        }
    }
}

pub struct CommandRunner;

impl CommandRunner {
    pub fn new() -> Self {
        Self
    }

    /// Run `cmd args...` directly (no shell).
    pub fn run(&self, cmd: &str, args: &[&str]) -> CmdOutput {
        let output = Command::new(cmd).args(args).output();
        match output {
            Ok(out) => CmdOutput {
                ok: out.status.success(),
                missing: false,
                killed_by_sigint: killed_by_sigint(&out.status),
                exit_code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => CmdOutput {
                ok: false,
                missing: e.kind() == std::io::ErrorKind::NotFound,
                killed_by_sigint: false,
                exit_code: None,
                stdout: String::new(),
                stderr: e.to_string(),
            },
        }
    }

    /// Convenience: trimmed stdout if the command succeeded, else `None`.
    pub fn stdout(&self, cmd: &str, args: &[&str]) -> Option<String> {
        self.run(cmd, args).stdout_if_ok()
    }
}

/// True when the process was terminated by SIGINT (Ctrl+C). A normal exit
/// (even a non-zero one) returns false; only a signal kill sets it.
#[cfg(unix)]
fn killed_by_sigint(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(libc::SIGINT)
}

#[cfg(not(unix))]
fn killed_by_sigint(_status: &std::process::ExitStatus) -> bool {
    false
}

/// Parse a `nvidia-smi --format=csv,noheader` field list into a map.
/// `[N/A]` and empty values are stored as `None`.
pub fn csv_fields(row: &str) -> Vec<Option<String>> {
    row.split(',')
        .map(|f| {
            let t = f.trim();
            if t.is_empty() || t == "[N/A]" {
                None
            } else {
                Some(t.to_string())
            }
        })
        .collect()
}

/// Parse an optional unsigned integer from a CSV field.
pub fn parse_u32(s: Option<&str>) -> Option<u32> {
    s.and_then(|v| v.trim().parse().ok())
}

/// Parse an optional float from a CSV field.
pub fn parse_f64(s: Option<&str>) -> Option<f64> {
    s.and_then(|v| v.trim().parse().ok())
}

/// Read a UTF-8 file if present (used for /etc/os-release).
pub fn read_file_lossy(path: &str) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// True when the effective uid is 0.
pub fn is_root() -> bool {
    read_file_lossy("/proc/self/status")
        .as_deref()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
        })
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|uid| uid == 0)
}

impl Default for CommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_fields_handles_na_and_missing() {
        let f = csv_fields("RTX 5090,1795125060111,[N/A],615.78.08,00000000:01:00.0,");
        assert_eq!(f[0].as_deref(), Some("RTX 5090"));
        assert_eq!(f[1].as_deref(), Some("1795125060111"));
        assert_eq!(f[2], None);
        assert_eq!(f[3].as_deref(), Some("615.78.08"));
        assert_eq!(f[4].as_deref(), Some("00000000:01:00.0"));
        assert_eq!(f[5], None);
    }

    #[test]
    fn missing_binary_is_flagged() {
        let r = CommandRunner::new();
        let out = r.run("definitely-not-a-real-binary-12345", &[]);
        assert!(out.missing);
        assert!(!out.ok);
    }

    #[cfg(unix)]
    #[test]
    fn sigint_kill_is_flagged() {
        // `sh` sends itself SIGINT; the default action terminates it, so the
        // exit status is a signal kill, not a non-zero exit code.
        let r = CommandRunner::new();
        let out = r.run("sh", &["-c", "kill -INT $$"]);
        assert!(out.killed_by_sigint);
        assert!(!out.ok);
        assert_eq!(out.exit_code, None);
    }

    #[test]
    fn non_zero_exit_is_not_a_sigint_kill() {
        let r = CommandRunner::new();
        let out = r.run("sh", &["-c", "exit 3"]);
        assert!(!out.killed_by_sigint);
        assert!(!out.ok);
        assert_eq!(out.exit_code, Some(3));
    }
}
