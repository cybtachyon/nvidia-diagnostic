//! User-workload handling (PLAN §4.2, §4.1 step 3): the monitor can spawn
//! the user's real CUDA workload (or a stress command) so the failure
//! reproduces on demand. The workload's stdout/stderr are inherited so the
//! user sees its output while the monitor owns the TTY for keys.

use std::process::{Child, Command, Stdio};

pub struct Workload {
    cmd: Vec<String>,
    child: Child,
}

impl Workload {
    /// Spawn `argv[0] argv[1..]` with stdin detached.
    pub fn spawn(argv: &[String]) -> std::io::Result<Self> {
        let (cmd, args) = argv.split_first().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty workload argv")
        })?;
        let child = Command::new(cmd).args(args).stdin(Stdio::null()).spawn()?;
        Ok(Self {
            cmd: argv.to_vec(),
            child,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn cmd(&self) -> &[String] {
        &self.cmd
    }

    /// Non-blocking exit check; returns the exit code once the process
    /// has ended.
    pub fn poll(&mut self) -> Option<i32> {
        self.child
            .try_wait()
            .ok()
            .flatten()
            .map(|s| s.code().unwrap_or(-1))
    }

    /// Kill (if still running) and reap.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_reap_captures_exit_code() {
        let mut w = Workload::spawn(&["sh".into(), "-c".into(), "exit 3".into()]).unwrap();
        // Give the child a moment to exit, then poll.
        for _ in 0..50 {
            if w.poll().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(w.poll(), Some(3));
        // `pid()` is the (stable) child process id; it does not reset to 0
        // once the child is reaped.
        assert!(w.pid() > 0);
    }
}
