//! Suspend/resume diagnostic: the resume-from-sleep test.
//!
//! Checks the pieces that make NVIDIA resume from sleep work (Arch Wiki,
//! "Preserve video memory after suspend"):
//!
//! 1. The nvidia systemd services (suspend/resume/hibernate/persistenced).
//! 2. DRM/KMS: /sys/module/nvidia_drm/parameters/modeset, and the nvidia
//!    module list in mkinitcpio (early KMS) when modeset is not Y.
//! 3. Video memory preservation: pre-595 drivers use
//!    NVreg_PreserveVideoMemoryAllocations=1, 595+ drivers use
//!    NVreg_UseKernelSuspendNotifiers=1. /proc/driver/nvidia/params is the
//!    primary source; the sysfs module parameters are the fallback.
//! 4. The active boot loader (GRUB vs Limine) via efibootmgr.
//!
//! The parsers are pure functions over captured text (unit-tested). The
//! interactive flow adds inquire prompts and remediation commands
//! (systemctl, mkinitcpio -P, modprobe.d). Privileged commands run under
//! sudo only when not already root; the user handles the password prompt.

use crate::util::cmd::{CommandRunner, is_root, read_file_lossy};
use anyhow::Result;
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use std::io::{IsTerminal, Write};
use std::path::Path;

/// The nvidia systemd services on the suspend/resume path, in lifecycle
/// order (suspend, resume, hibernate, then the persistenced daemon).
const SUSPEND_SERVICES: &[&str] = &[
    "nvidia-suspend.service",
    "nvidia-resume.service",
    "nvidia-hibernate.service",
    "nvidia-persistenced.service",
];

/// The modules early KMS must load in the initramfs.
const EARLY_KMS_MODULES: &[&str] = &["nvidia", "nvidia_modeset", "nvidia_uvm", "nvidia_drm"];

/// The mkinitcpio drop-in that carries the early-KMS module list.
const CHWD_CONF: &str = "/etc/mkinitcpio.conf.d/10-chwd.conf";

/// The modprobe.d file for nvidia kernel module options.
const MODPROBE_NV: &str = "/etc/modprobe.d/nvidia.conf";

/// Sysfs fallback for the legacy preserve parameter.
const SYSFS_PRESERVE: &str = "/sys/module/nvidia/parameters/NVreg_PreserveVideoMemoryAllocations";

/// The module option that moves the temp file off tmpfs (Arch default).
const TEMP_PATH_OPTION: &str = "options nvidia NVreg_TemporaryFilePath=/var/tmp";

// ---------------------------------------------------------------------------
// result types
// ---------------------------------------------------------------------------

/// State of one nvidia systemd service on the suspend/resume path.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ServiceState {
    pub name: String,
    pub active: bool,
    pub active_state: String,
    pub enabled: bool,
    pub enabled_state: String,
    /// oneshot units (nvidia-suspend/resume/hibernate) are inactive at rest;
    /// only the persistenced daemon is expected to be active.
    pub oneshot: bool,
}

/// Driver series for the preserve-parameter switch (Arch Wiki).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DriverSeries {
    #[default]
    Unknown,
    Pre595,
    Post595,
}

/// The /proc/driver/nvidia/params keys relevant to suspend.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NvidiaParams {
    /// 0 off, 1 preserve (430-590), 2 preserve via kernel suspend notifiers
    /// (the 595+ driver default when the notifiers are enabled).
    pub preserve_video_memory_allocations: Option<i64>,
    pub use_kernel_suspend_notifiers: Option<bool>,
    /// Raw value; an empty string means the driver default (/tmp, tmpfs).
    pub temporary_file_path: Option<String>,
}

/// Result of scanning the mkinitcpio config for the nvidia module list.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MkinitcpioScan {
    /// True when all four early-KMS modules appear in some MODULES line.
    pub early_kms: bool,
    /// Files that define a MODULES line.
    pub module_files: Vec<String>,
    /// Union of the nvidia* module names found.
    pub nvidia_modules: Vec<String>,
}

/// Boot loader family detected from `efibootmgr` output.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BootloaderFamily {
    #[default]
    Unknown,
    Grub,
    Limine,
    Other,
}

/// Boot loader detection from `efibootmgr` output.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Bootloader {
    /// The `BootCurrent` id, e.g. "0002".
    pub boot_current: Option<String>,
    /// Label of the active entry, e.g. "Limine".
    pub active_label: Option<String>,
    pub family: BootloaderFamily,
}

/// Video memory preservation status (the pre-/post-595 paths).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MemPersist {
    pub driver_version: Option<String>,
    pub series: DriverSeries,
    pub enabled: bool,
    /// Where the enabled decision came from.
    pub source: String,
    pub params: NvidiaParams,
    /// Fallback sysfs value (NVreg_PreserveVideoMemoryAllocations).
    pub sysfs_preserve: Option<String>,
    /// Edge-case notes for the user (temp file, early KMS, capacity).
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// pure parsers (unit-tested)
// ---------------------------------------------------------------------------

/// Classify the driver series from a version string like "615.78.08".
pub fn driver_series_from_version(v: &str) -> DriverSeries {
    match v.split('.').next().and_then(|m| m.parse::<u32>().ok()) {
        Some(major) if major >= 595 => DriverSeries::Post595,
        Some(_) => DriverSeries::Pre595,
        None => DriverSeries::Unknown,
    }
}

/// Parse the /proc/driver/nvidia/params lines relevant to suspend.
pub fn parse_nvidia_params(text: &str) -> NvidiaParams {
    let mut p = NvidiaParams::default();
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        match name {
            "PreserveVideoMemoryAllocations" => {
                p.preserve_video_memory_allocations = value.parse().ok();
            }
            "UseKernelSuspendNotifiers" => {
                p.use_kernel_suspend_notifiers = Some(value == "1");
            }
            // The value is quoted: TemporaryFilePath: "/var/tmp".
            "TemporaryFilePath" => {
                p.temporary_file_path = Some(value.trim_matches('"').to_string());
            }
            _ => {}
        }
    }
    p
}

/// Scan mkinitcpio files for MODULES lines. Accepts both `MODULES=(...)`
/// and `MODULES+=(...)` forms, which is what the chwd drop-in uses.
pub fn scan_mkinitcpio(files: &[(String, String)]) -> MkinitcpioScan {
    let mut scan = MkinitcpioScan::default();
    let mut modules: Vec<String> = Vec::new();
    for (path, content) in files {
        let mut file_defines_modules = false;
        for line in content.lines() {
            let t = line.trim();
            if t.starts_with('#') {
                continue;
            }
            let Some(inner) = t
                .strip_prefix("MODULES+=")
                .or_else(|| t.strip_prefix("MODULES="))
            else {
                continue;
            };
            file_defines_modules = true;
            let Some(list) = inner
                .trim()
                .strip_prefix('(')
                .and_then(|s| s.split_once(')').map(|(l, _)| l))
            else {
                continue;
            };
            for m in list.split_whitespace() {
                if m.starts_with("nvidia") && !modules.iter().any(|x| x == m) {
                    modules.push(m.to_string());
                }
            }
        }
        if file_defines_modules {
            scan.module_files.push(path.clone());
        }
    }
    scan.nvidia_modules = modules;
    scan.early_kms = EARLY_KMS_MODULES
        .iter()
        .all(|m| scan.nvidia_modules.iter().any(|x| x == m));
    scan
}

/// Classify the active boot loader from `efibootmgr` output.
/// Returns (boot_current, active_label, family).
pub fn classify_bootloader(out: &str) -> (Option<String>, Option<String>, BootloaderFamily) {
    let boot_current = out
        .lines()
        .find_map(|l| l.strip_prefix("BootCurrent:"))
        .map(|v| v.trim().to_string());
    // Entry lines look like "Boot0002* Limine\tHD(6,...)". BootOrder and
    // BootCurrent start with "Boot" but their 4..8 slice is not hex.
    let entries: Vec<(String, String)> = out
        .lines()
        .filter(|l| {
            l.starts_with("Boot") && l.len() > 8 && l[4..8].chars().all(|c| c.is_ascii_hexdigit())
        })
        .map(|l| {
            let id = l[4..8].to_string();
            // The label ends at the tab that separates it from the device
            // path; an optional '*' marks the active entry.
            let rest = &l[8..];
            let (label_part, _) = rest.split_once('\t').unwrap_or((rest, ""));
            let label = label_part
                .trim_start()
                .trim_start_matches('*')
                .trim()
                .to_string();
            (id, label)
        })
        .collect();
    let active_label = boot_current.as_ref().and_then(|cur| {
        entries
            .iter()
            .find(|(id, _)| id == cur)
            .map(|(_, l)| l.clone())
    });
    let family = match active_label.as_deref() {
        Some(l) if l.to_ascii_lowercase().contains("grub") => BootloaderFamily::Grub,
        Some(l) if l.to_ascii_lowercase().contains("limine") => BootloaderFamily::Limine,
        Some(_) => BootloaderFamily::Other,
        None => BootloaderFamily::Unknown,
    };
    (boot_current, active_label, family)
}

/// The pre-/post-595 preserve decision (Arch Wiki). Returns
/// (enabled, source). 595+ drivers are decided by the kernel suspend
/// notifiers; the sysfs fallback accepts 1 (preserve) and 2 (preserve via
/// the notifiers, the 595+ driver default).
pub fn mem_persist_decision(
    series: DriverSeries,
    params: &NvidiaParams,
    sysfs_preserve: Option<&str>,
) -> (bool, &'static str) {
    match series {
        DriverSeries::Post595 => match params.use_kernel_suspend_notifiers {
            Some(on) => (on, "/proc/driver/nvidia/params (UseKernelSuspendNotifiers)"),
            None => match sysfs_preserve {
                Some("1") | Some("2") => (true, SYSFS_PRESERVE),
                _ => (false, SYSFS_PRESERVE),
            },
        },
        DriverSeries::Pre595 => match params.preserve_video_memory_allocations {
            Some(v) => (
                v == 1,
                "/proc/driver/nvidia/params (PreserveVideoMemoryAllocations)",
            ),
            None => match sysfs_preserve {
                Some("1") => (true, SYSFS_PRESERVE),
                _ => (false, SYSFS_PRESERVE),
            },
        },
        DriverSeries::Unknown => {
            let proc = params.use_kernel_suspend_notifiers == Some(true)
                || matches!(params.preserve_video_memory_allocations, Some(1) | Some(2));
            if proc {
                (true, "/proc/driver/nvidia/params")
            } else {
                (
                    matches!(sysfs_preserve, Some("1") | Some("2")),
                    SYSFS_PRESERVE,
                )
            }
        }
    }
}

/// The modeset sysfs file: "Y" enables DRM/KMS.
pub fn modeset_enabled(value: Option<&str>) -> bool {
    matches!(value, Some("Y"))
}

// ---------------------------------------------------------------------------
// I/O collection
// ---------------------------------------------------------------------------

/// One service needs attention when it is not enabled, or when a
/// long-running daemon is not active (oneshot units are inactive at rest).
fn needs_attention(s: &ServiceState) -> bool {
    !s.enabled || (!s.active && !s.oneshot)
}

fn collect_services(runner: &CommandRunner) -> Vec<ServiceState> {
    SUSPEND_SERVICES
        .iter()
        .map(|name| {
            let active_out = runner.run("systemctl", &["is-active", name]);
            let enabled_out = runner.run("systemctl", &["is-enabled", name]);
            let active_state = active_out.stdout.trim().to_string();
            let enabled_state = enabled_out.stdout.trim().to_string();
            let oneshot = runner
                .run("systemctl", &["cat", name])
                .stdout
                .lines()
                .any(|l| l.trim() == "Type=oneshot");
            ServiceState {
                name: (*name).to_string(),
                active: active_state == "active",
                active_state,
                enabled: enabled_state == "enabled",
                enabled_state,
                oneshot,
            }
        })
        .collect()
}

fn read_modeset() -> Option<String> {
    read_file_lossy("/sys/module/nvidia_drm/parameters/modeset").map(|s| s.trim().to_string())
}

/// The mkinitcpio files to scan: the main config plus every .conf drop-in.
fn mkinitcpio_files() -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = Vec::new();
    if let Some(c) = read_file_lossy("/etc/mkinitcpio.conf") {
        files.push(("/etc/mkinitcpio.conf".to_string(), c));
    }
    if let Ok(entries) = std::fs::read_dir("/etc/mkinitcpio.conf.d") {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "conf")
                && let Some(c) = read_file_lossy(p.to_string_lossy().as_ref())
            {
                files.push((p.to_string_lossy().into_owned(), c));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

/// Driver version from nvidia-smi, falling back to modinfo.
fn driver_version(runner: &CommandRunner) -> Option<String> {
    if let Some(v) = runner.stdout(
        "nvidia-smi",
        &["--query-gpu", "driver_version", "--format=csv,noheader"],
    ) {
        return v.lines().next().map(|l| l.trim().to_string());
    }
    runner
        .run("modinfo", &["nvidia"])
        .stdout_if_ok()
        .and_then(|out| {
            out.lines()
                .find_map(|l| l.strip_prefix("version:"))
                .map(|v| v.trim().to_string())
        })
}

/// Total VRAM across all GPUs in MiB (for the 5% capacity check).
fn total_vram_mib(runner: &CommandRunner) -> Option<u64> {
    let out = runner.stdout(
        "nvidia-smi",
        &[
            "--query-gpu",
            "memory.total",
            "--format=csv,noheader,nounits",
        ],
    )?;
    let total: u64 = out
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .sum();
    (total > 0).then_some(total)
}

/// Free space in MiB on the filesystem holding `path`.
fn free_mib(runner: &CommandRunner, path: &str) -> Option<u64> {
    let out = runner.run("df", &["-k", path]);
    let line = out.stdout.lines().nth(1)?;
    let avail_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb / 1024)
}

/// The filesystem type of `path` (ext4, xfs, tmpfs, ...).
fn fs_type(runner: &CommandRunner, path: &str) -> Option<String> {
    let out = runner.run("df", &["-T", path]);
    out.stdout
        .lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().nth(1))
        .map(String::from)
}

fn collect_mem_persist(runner: &CommandRunner, scan: &MkinitcpioScan) -> MemPersist {
    let driver_version = driver_version(runner);
    let series = driver_version
        .as_deref()
        .map(driver_series_from_version)
        .unwrap_or_default();
    let params = read_file_lossy("/proc/driver/nvidia/params")
        .as_deref()
        .map(parse_nvidia_params)
        .unwrap_or_default();
    let sysfs_preserve = read_file_lossy(SYSFS_PRESERVE).map(|s| s.trim().to_string());
    let (enabled, source) = mem_persist_decision(series, &params, sysfs_preserve.as_deref());

    let mut notes = Vec::new();

    // An empty TemporaryFilePath means the driver default, /tmp (tmpfs).
    // NVIDIA recommends a non-tmpfs filesystem for the temp file.
    let temp_path = params.temporary_file_path.clone().unwrap_or_default();
    if temp_path.is_empty() || temp_path == "/tmp" {
        let where_text = if temp_path.is_empty() {
            "/tmp (the driver default), which is a tmpfs".to_string()
        } else {
            format!("{temp_path}, which is a tmpfs")
        };
        notes.push(format!(
            "video memory is saved to {where_text}; NVIDIA recommends a non-tmpfs \
             filesystem, and Arch defaults to /var/tmp (set {TEMP_PATH_OPTION})"
        ));
    }

    // The temp filesystem must hold at least 5% more than the total VRAM.
    if let Some(total) = total_vram_mib(runner) {
        let need_mib = total * 105 / 100;
        let fs = if temp_path.is_empty() {
            "/tmp"
        } else {
            &temp_path
        };
        if let Some(free) = free_mib(runner, fs)
            && free < need_mib
        {
            notes.push(format!(
                "not enough free space on {fs}: need ~{need_mib} MiB (VRAM + 5%), have {free} MiB"
            ));
        }
    }

    // Early KMS: the module loads in the initramfs, which has no access to
    // the temp file path, so hibernation is unsupported (suspend is fine).
    if scan.early_kms {
        notes.push(
            "early KMS is enabled (nvidia modules in the initramfs): hibernation is \
             not supported, because the initramfs has no access to NVreg_TemporaryFilePath; \
             suspend is unaffected"
                .to_string(),
        );
    }

    MemPersist {
        driver_version,
        series,
        enabled,
        source: source.to_string(),
        params,
        sysfs_preserve,
        notes,
    }
}

fn detect_bootloader(runner: &CommandRunner) -> Bootloader {
    let out = runner.run("efibootmgr", &[]);
    if out.missing || !out.ok {
        return Bootloader::default();
    }
    let (boot_current, active_label, family) = classify_bootloader(&out.stdout);
    Bootloader {
        boot_current,
        active_label,
        family,
    }
}

// ---------------------------------------------------------------------------
// remediation (privilege elevation is left to the user)
// ---------------------------------------------------------------------------

/// Run a command with inherited stdio, so sudo can prompt the user for a
/// password. Returns true on success.
fn run_elevated(root: bool, cmd: &str, args: &[&str]) -> bool {
    let mut argv: Vec<&str> = Vec::new();
    if !root {
        argv.push("sudo");
    }
    argv.push(cmd);
    argv.extend(args);
    match std::process::Command::new(argv[0])
        .args(&argv[1..])
        .status()
    {
        Ok(s) => s.success(),
        Err(e) => {
            eprintln!("{} {e}", "failed to run".red());
            false
        }
    }
}

/// Write a file, using `sudo tee` when not root (the user handles the
/// password prompt).
fn write_file_elevated(root: bool, path: &Path, content: &str) -> Result<()> {
    if root {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, content)?;
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        run_elevated(root, "mkdir", &["-p", dir.to_string_lossy().as_ref()]);
    }
    let mut child = std::process::Command::new("sudo")
        .arg("tee")
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content.as_bytes())?;
    }
    let ok = child.wait().is_ok_and(|s| s.success());
    if !ok {
        anyhow::bail!("failed to write {}", path.display());
    }
    Ok(())
}

/// Append an `options nvidia ...` line to /etc/modprobe.d/nvidia.conf when
/// it is not already present.
fn ensure_modprobe_option(root: bool, option: &str) -> Result<()> {
    let existing = read_file_lossy(MODPROBE_NV).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == option) {
        println!(
            "  {} {} already in {MODPROBE_NV}",
            "note:".dimmed(),
            option.dimmed()
        );
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(option);
    content.push('\n');
    write_file_elevated(root, Path::new(MODPROBE_NV), &content)?;
    println!(
        "  {} {option} -> {MODPROBE_NV} (reboot required)",
        "wrote:".green()
    );
    Ok(())
}

/// Enable and start one service (sudo when not root).
fn enable_service(root: bool, service: &str) {
    let ok = run_elevated(root, "systemctl", &["enable", "--now", service]);
    if ok {
        println!("  {} {service}", "enabled:".green());
    } else {
        println!(
            "  {} {service} (run `sudo systemctl enable --now {service}` manually)",
            "enable failed:".red()
        );
    }
}

/// Write the early-KMS module list to the chwd drop-in and rebuild the
/// initramfs.
fn apply_early_kms(root: bool) -> Result<bool> {
    let content = "# This file is automatically generated by nvidia-diagnostic. \
PLEASE DO NOT EDIT IT.\nMODULES=(nvidia nvidia_modeset nvidia_uvm nvidia_drm)\n";
    write_file_elevated(root, Path::new(CHWD_CONF), content)?;
    println!(
        "  {} MODULES=(nvidia nvidia_modeset nvidia_uvm nvidia_drm) -> {CHWD_CONF}",
        "wrote:".green()
    );
    println!(
        "{} running `mkinitcpio -P` (sudo will prompt if not elevated)...",
        "now:".bold()
    );
    let ok = run_elevated(root, "mkinitcpio", &["-P"]);
    if ok {
        println!(
            "  {} initramfs rebuilt; reboot for the change to take effect",
            "done:".green()
        );
    } else {
        println!(
            "  {} `mkinitcpio -P` failed (run `sudo mkinitcpio -P` manually)",
            "rebuild failed:".red()
        );
    }
    Ok(ok)
}

// ---------------------------------------------------------------------------
// the interactive flow
// ---------------------------------------------------------------------------

/// Run the resume-from-sleep diagnostic (the `suspend` subcommand).
pub fn run(runner: &CommandRunner) -> Result<()> {
    let root = is_root();
    let tty = std::io::stdin().is_terminal();

    println!();
    println!(
        "{}",
        "nvidia-diagnostic — resume-from-sleep diagnostic".bold()
    );

    // --- 1. systemd services ---
    let services = collect_services(runner);
    println!("\n{}", "services:".bold());
    for s in &services {
        let rest_note = if s.oneshot && !s.active {
            " (oneshot; inactive at rest is normal)"
        } else if !s.active {
            " (not running)"
        } else {
            ""
        };
        println!(
            "  {:<28} {:<10} {}{}",
            s.name.dimmed(),
            if s.enabled {
                "enabled".green().to_string()
            } else {
                format!("{} {}", s.enabled_state.yellow(), "not enabled".red())
            },
            if s.active {
                "active".green().to_string()
            } else {
                s.active_state.yellow().to_string()
            },
            rest_note.dimmed()
        );
    }

    let to_enable: Vec<String> = services
        .iter()
        .filter(|s| needs_attention(s))
        .map(|s| s.name.clone())
        .collect();
    if !to_enable.is_empty() {
        if tty {
            let all: Vec<usize> = (0..to_enable.len()).collect();
            let picked = inquire::MultiSelect::new(
                "These services are not active (or not enabled). Enable and start which?",
                to_enable.clone(),
            )
            .with_default(&all)
            .prompt()?;
            for name in &picked {
                enable_service(root, name);
            }
            if picked.is_empty() {
                println!("  {} no services changed", "skipped:".dimmed());
            }
        } else {
            for name in &to_enable {
                println!(
                    "  {} run: sudo systemctl enable --now {name}",
                    "todo:".yellow()
                );
            }
        }
    } else {
        println!("  {} all services enabled and healthy", "ok:".green());
    }

    // --- 2. DRM/KMS ---
    let modeset = read_modeset();
    let mut kms_ok = modeset_enabled(modeset.as_deref());
    let mut scan = MkinitcpioScan::default();
    println!("\n{}", "drm/kms:".bold());
    if kms_ok {
        println!(
            "  {} Direct Rendering Manager and Kernel Mode Setting were detected as \
             enabled (nvidia_drm modeset=Y)",
            "ok:".green()
        );
    } else {
        scan = scan_mkinitcpio(&mkinitcpio_files());
        if scan.early_kms {
            kms_ok = true;
            println!(
                "  {} modeset={} but the nvidia modules are already in the initramfs \
                 config: {}",
                "note:".yellow(),
                modeset.as_deref().unwrap_or("unreadable"),
                scan.module_files.join(", ").dimmed()
            );
            println!(
                "  add the boot parameter nvidia_drm.modeset=1 (GRUB: GRUB_CMDLINE_LINUX, \
                 Limine: the Limine config) and reboot"
            );
        } else {
            println!(
                "  {} modeset={}, and the nvidia modules are missing from the \
                 mkinitcpio config",
                "gap:".red(),
                modeset.as_deref().unwrap_or("unreadable")
            );
            if tty {
                let ask = inquire::Confirm::new(
                    "Try enabling NVIDIA Direct Rendering Manager and Kernel Mode Setting \
                     to resolve black screen issues?",
                )
                .with_default(true)
                .prompt()?;
                if ask {
                    let applied = apply_early_kms(root)?;
                    kms_ok = applied;
                } else {
                    println!("  {} skipped (no change made)", "skipped:".dimmed());
                }
            } else {
                println!(
                    "  {} write MODULES=(nvidia nvidia_modeset nvidia_uvm nvidia_drm) to \
                     {CHWD_CONF}, then run: sudo mkinitcpio -P (reboot required)",
                    "todo:".yellow()
                );
            }
        }
    }

    // --- 3. video memory preservation ---
    let mem = collect_mem_persist(runner, &scan);
    println!("\n{}", "memory preservation:".bold());
    println!(
        "  {} {} ({})",
        "driver:".dimmed(),
        mem.driver_version.clone().unwrap_or_else(|| "?".into()),
        match mem.series {
            DriverSeries::Pre595 => "pre-595 series".to_string(),
            DriverSeries::Post595 => "595+ series".to_string(),
            DriverSeries::Unknown => "series unknown".to_string(),
        }
    );
    println!(
        "  {} via {}",
        if mem.enabled {
            "enabled".green().to_string()
        } else {
            "not enabled".red().to_string()
        },
        mem.source.dimmed()
    );
    for n in &mem.notes {
        println!("  {} {}", "note:".yellow(), n);
    }

    if !mem.enabled {
        let option = match mem.series {
            // The legacy parameter exists on every series, so it is the safe
            // default when the version is unknown.
            DriverSeries::Post595 => "options nvidia NVreg_UseKernelSuspendNotifiers=1",
            DriverSeries::Pre595 | DriverSeries::Unknown => {
                "options nvidia NVreg_PreserveVideoMemoryAllocations=1"
            }
        };
        if tty {
            let ask = inquire::Confirm::new(&format!(
                "Write {option} to {MODPROBE_NV} to enable video memory preservation? \
                 (reboot required)",
            ))
            .with_default(true)
            .prompt()?;
            if ask {
                ensure_modprobe_option(root, option)?;
            } else {
                println!("  {} skipped (no change made)", "skipped:".dimmed());
            }
        } else {
            println!(
                "  {} write {option} to {MODPROBE_NV} (reboot required)",
                "todo:".yellow()
            );
        }
    }

    // The temp-file fix is offered only when its requirements are met: a
    // non-tmpfs filesystem with enough free space for the VRAM + 5%.
    let temp_needs_fix = mem
        .params
        .temporary_file_path
        .clone()
        .unwrap_or_default()
        .is_empty()
        || mem.params.temporary_file_path.as_deref() == Some("/tmp");
    if temp_needs_fix {
        let target_type = fs_type(runner, "/var/tmp");
        let vram = total_vram_mib(runner);
        let free = free_mib(runner, "/var/tmp");
        let requirements_met = target_type.as_deref().is_some_and(|t| t != "tmpfs")
            && vram.is_some_and(|v| free.is_some_and(|f| f >= v * 105 / 100));
        if requirements_met {
            if tty {
                let ask = inquire::Confirm::new(&format!(
                    "Also set {TEMP_PATH_OPTION} so the temp file stays off tmpfs? \
                     (reboot required)",
                ))
                .with_default(true)
                .prompt()?;
                if ask {
                    ensure_modprobe_option(root, TEMP_PATH_OPTION)?;
                } else {
                    println!("  {} skipped (no change made)", "skipped:".dimmed());
                }
            } else {
                println!(
                    "  {} write {TEMP_PATH_OPTION} to {MODPROBE_NV} (reboot required)",
                    "todo:".yellow()
                );
            }
        } else {
            println!(
                "  {} the /var/tmp fix is not offered: /var/tmp is {} (a non-tmpfs \
                 filesystem with enough free space for the VRAM + 5% is required)",
                "note:".yellow(),
                target_type.clone().unwrap_or_else(|| "unavailable".into())
            );
        }
    }

    // --- 4. boot loader ---
    let boot = detect_bootloader(runner);
    println!("\n{}", "boot loader:".bold());
    match (boot.boot_current.as_deref(), boot.active_label.as_deref()) {
        (Some(cur), Some(label)) => println!(
            "  {} {label} (efibootmgr BootCurrent {cur})",
            "active:".green()
        ),
        (None, Some(label)) => println!("  {} {label}", "active:".green()),
        _ => println!(
            "  {} could not detect the active boot loader (efibootmgr missing or failed)",
            "note:".yellow()
        ),
    }

    // --- 5. summary ---
    let services_ok = services.iter().all(|s| !needs_attention(s));
    println!("\n{}", "summary:".bold());
    if services_ok && kms_ok && mem.enabled {
        println!(
            "  the driver-side resume path is healthy (services, DRM/KMS, and video \
             memory preservation all check out)"
        );
        println!(
            "  if the screen stays black after resume, the suspect is the display \
             manager layer (plasma-login-manager, part of the CachyOS stack)"
        );
        println!(
            "  workaround: switch to TTY3 (Ctrl+Alt+F3), then back to TTY2 (Ctrl+Alt+F2); \
             this forces the plasma login screen to re-evaluate the framebuffer"
        );
        println!(
            "  if the issue is confirmed there, pause development of this tool and \
             contribute the fix to plasma-login-manager (planned for a future turn)"
        );
    } else {
        if !services_ok {
            println!("  services still need attention (see above)");
        }
        if !kms_ok {
            println!("  DRM/KMS is not fully in place (see above)");
        }
        if !mem.enabled {
            println!("  video memory preservation is not enabled (see above)");
        }
        println!("  re-run `nvidia-diagnostic suspend` after the changes and a reboot");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_series_splits_at_595() {
        assert_eq!(
            driver_series_from_version("615.78.08"),
            DriverSeries::Post595
        );
        assert_eq!(
            driver_series_from_version("595.58.03"),
            DriverSeries::Post595
        );
        assert_eq!(
            driver_series_from_version("580.95.05"),
            DriverSeries::Pre595
        );
        assert_eq!(
            driver_series_from_version("470.256.02"),
            DriverSeries::Pre595
        );
        assert_eq!(driver_series_from_version("garbage"), DriverSeries::Unknown);
        assert_eq!(driver_series_from_version(""), DriverSeries::Unknown);
    }

    #[test]
    fn parse_nvidia_params_reads_the_relevant_keys() {
        // Fixture from this machine (driver 615.78.08).
        let text = "ResmanDebugLevel: 4294967295\n\
                    PreserveVideoMemoryAllocations: 2\n\
                    UseKernelSuspendNotifiers: 1\n\
                    TemporaryFilePath: \"\"\n";
        let p = parse_nvidia_params(text);
        assert_eq!(p.preserve_video_memory_allocations, Some(2));
        assert_eq!(p.use_kernel_suspend_notifiers, Some(true));
        assert_eq!(p.temporary_file_path.as_deref(), Some(""));
    }

    #[test]
    fn parse_nvidia_params_strips_the_temp_path_quotes() {
        let p = parse_nvidia_params("TemporaryFilePath: \"/var/tmp\"\n");
        assert_eq!(p.temporary_file_path.as_deref(), Some("/var/tmp"));
    }

    #[test]
    fn scan_mkinitcpio_finds_the_chwd_drop_in() {
        let files = vec![
            (
                "/etc/mkinitcpio.conf".to_string(),
                "#     MODULES=(usbhid xhci_hcd)\nMODULES=()\n".to_string(),
            ),
            (
                "/etc/mkinitcpio.conf.d/10-chwd.conf".to_string(),
                "# This file is automatically generated by chwd. PLEASE DO NOT EDIT IT.\n\
                 MODULES+=(nvidia nvidia_modeset nvidia_uvm nvidia_drm)\n"
                    .to_string(),
            ),
        ];
        let scan = scan_mkinitcpio(&files);
        assert!(scan.early_kms);
        assert_eq!(
            scan.nvidia_modules,
            vec![
                "nvidia".to_string(),
                "nvidia_modeset".to_string(),
                "nvidia_uvm".to_string(),
                "nvidia_drm".to_string()
            ]
        );
        assert!(
            scan.module_files
                .iter()
                .any(|f| f.ends_with("10-chwd.conf"))
        );
    }

    #[test]
    fn scan_mkinitcpio_reports_a_missing_module() {
        let files = vec![(
            "/etc/mkinitcpio.conf".to_string(),
            "MODULES=(nvidia nvidia_modeset)\n".to_string(),
        )];
        let scan = scan_mkinitcpio(&files);
        assert!(!scan.early_kms);
        assert_eq!(scan.nvidia_modules.len(), 2);
    }

    #[test]
    fn classify_bootloader_detects_limine() {
        // Fixture shaped like the real efibootmgr output on this machine.
        let out = "BootCurrent: 0002\n\
                   BootOrder: 0002,0000,0005,0001\n\
                   Boot0000* Windows Boot Manager\tHD(1,GPT,...)/\\EFI\\Microsoft\\Boot\\bootmgfw.efi\n\
                   Boot0002* Limine\tHD(6,GPT,...)/\\EFI\\limine\\limine_x64.efi\n";
        let (current, label, family) = classify_bootloader(out);
        assert_eq!(current.as_deref(), Some("0002"));
        assert_eq!(label.as_deref(), Some("Limine"));
        assert_eq!(family, BootloaderFamily::Limine);
    }

    #[test]
    fn classify_bootloader_detects_grub() {
        let out = "BootCurrent: 0001\nBoot0001* GRUB\tHD(0,GPT,...)/\\EFI\\grub\\x64.efi\n";
        let (_, label, family) = classify_bootloader(out);
        assert_eq!(label.as_deref(), Some("GRUB"));
        assert_eq!(family, BootloaderFamily::Grub);
    }

    #[test]
    fn classify_bootloader_handles_missing_current() {
        let (current, label, family) = classify_bootloader("Boot0000* UEFI OS\tHD(0)\n");
        assert_eq!(current, None);
        assert_eq!(label, None);
        assert_eq!(family, BootloaderFamily::Unknown);
    }

    #[test]
    fn mem_persist_decision_post595_uses_the_notifiers() {
        let p = NvidiaParams {
            use_kernel_suspend_notifiers: Some(true),
            ..Default::default()
        };
        assert_eq!(
            mem_persist_decision(DriverSeries::Post595, &p, None),
            (
                true,
                "/proc/driver/nvidia/params (UseKernelSuspendNotifiers)"
            )
        );
        let p = NvidiaParams {
            use_kernel_suspend_notifiers: Some(false),
            ..Default::default()
        };
        assert!(!mem_persist_decision(DriverSeries::Post595, &p, None).0);
    }

    #[test]
    fn mem_persist_decision_pre595_uses_the_legacy_param() {
        let p = NvidiaParams {
            preserve_video_memory_allocations: Some(1),
            ..Default::default()
        };
        assert!(mem_persist_decision(DriverSeries::Pre595, &p, None).0);
        let p = NvidiaParams {
            preserve_video_memory_allocations: Some(0),
            ..Default::default()
        };
        assert!(!mem_persist_decision(DriverSeries::Pre595, &p, None).0);
    }

    #[test]
    fn mem_persist_decision_falls_back_to_sysfs() {
        // No /proc file: the sysfs legacy parameter decides. On 595+ drivers
        // the value 2 (preserve via the notifiers) counts as enabled.
        let p = NvidiaParams::default();
        assert!(mem_persist_decision(DriverSeries::Post595, &p, Some("2")).0);
        assert!(!mem_persist_decision(DriverSeries::Post595, &p, Some("0")).0);
        assert!(mem_persist_decision(DriverSeries::Pre595, &p, Some("1")).0);
        assert!(!mem_persist_decision(DriverSeries::Pre595, &p, Some("0")).0);
        assert!(!mem_persist_decision(DriverSeries::Pre595, &p, None).0);
    }

    #[test]
    fn modeset_only_accepts_y() {
        assert!(modeset_enabled(Some("Y")));
        assert!(!modeset_enabled(Some("N")));
        assert!(!modeset_enabled(None));
        assert!(!modeset_enabled(Some("y")));
    }

    #[test]
    fn needs_attention_flags_only_the_real_gaps() {
        let enabled_active = ServiceState {
            name: "a".into(),
            active: true,
            active_state: "active".into(),
            enabled: true,
            enabled_state: "enabled".into(),
            oneshot: false,
        };
        assert!(!needs_attention(&enabled_active));
        let oneshot_at_rest = ServiceState {
            name: "b".into(),
            active: false,
            active_state: "inactive".into(),
            enabled: true,
            enabled_state: "enabled".into(),
            oneshot: true,
        };
        assert!(!needs_attention(&oneshot_at_rest));
        let daemon_down = ServiceState {
            name: "c".into(),
            active: false,
            active_state: "inactive".into(),
            enabled: true,
            enabled_state: "enabled".into(),
            oneshot: false,
        };
        assert!(needs_attention(&daemon_down));
        let not_enabled = ServiceState {
            name: "d".into(),
            active: false,
            active_state: "inactive".into(),
            enabled: false,
            enabled_state: "disabled".into(),
            oneshot: false,
        };
        assert!(needs_attention(&not_enabled));
    }
}
