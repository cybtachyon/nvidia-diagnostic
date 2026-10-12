//! System probe: kernel, arch, OS, nvidia kernel module (PLAN §5 Stage 0).

use crate::probe::{BaselineCtx, Probe, ProbeResult};
use crate::util::cmd::{CommandRunner, read_file_lossy};
use serde_json::json;

pub struct SystemProbe;

impl Probe for SystemProbe {
    fn name(&self) -> &str {
        "system"
    }

    fn run(&self, ctx: &BaselineCtx) -> ProbeResult {
        let r: &CommandRunner = &ctx.runner;

        let kernel = r.stdout("uname", &["-r"]);
        let arch = r.stdout("uname", &["-m"]);

        let os = read_file_lossy("/etc/os-release").and_then(|content| {
            content
                .lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_string())
        });

        // `modinfo nvidia` — version of the loaded kernel module.
        let module_version = r
            .run("modinfo", &["nvidia"])
            .stdout_if_ok()
            .and_then(|out| {
                out.lines()
                    .find_map(|l| l.strip_prefix("version:"))
                    .map(|v| v.trim().to_string())
            });

        // `lsmod` — which nvidia modules are loaded.
        let modules: Vec<String> = r
            .run("lsmod", &[])
            .stdout_if_ok()
            .map(|out| {
                out.lines()
                    .filter_map(|l| l.split_whitespace().next())
                    .filter(|name| name.starts_with("nvidia"))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        if kernel.is_none() {
            return ProbeResult::Degraded {
                reason: "uname unavailable".into(),
            };
        }

        ProbeResult::Ok {
            summary: format!(
                "kernel {} / {} / module {}",
                kernel.as_deref().unwrap_or("?"),
                arch.as_deref().unwrap_or("?"),
                module_version
                    .as_deref()
                    .unwrap_or("unknown (module not found)")
            ),
            data: json!({
                "kernel": kernel,
                "arch": arch,
                "os": os,
                "nvidia_module_version": module_version,
                "nvidia_modules_loaded": modules,
            }),
        }
    }
}
