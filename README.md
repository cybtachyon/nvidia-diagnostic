# nvidia-diagnostic

`nvidia-diagnostic` is a Rust CLI for reproducible root-cause diagnosis of GPU
context loss (black screen + fan-to-max) on an NVIDIA card under a real CUDA
workload. It was built around an RTX 5090 (Blackwell, GB202) that started
dropping off the bus after a BIOS update and a driver update.

The tool's job is not to fix the card. It accumulates structured evidence
(baseline, live telemetry, kernel Xid events, isolation-experiment outcomes)
and emits a **ranked verdict** over the suspect set (driver / BIOS / 12VHPWR
PCIe connector / PSU / card hardware / cooling), with the evidence trail and
the cheapest discriminating next step. The shareable `report-<ts>.json` is the
unit of replication: anyone can diff two reports (before/after a change, or
two machines).

See [PLAN.md](PLAN.md) for the full design, the suspect model, and the
scoring table.

## Requirements

- Linux with the NVIDIA kernel module loaded and `nvidia-smi` on `PATH`.
- `lspci` (PCIe link state; a world-readable `sysfs` fallback covers
  speed/width without root).
- `dmidecode` (BIOS version, root only).
- `dmesg` **or** `journalctl` (kernel-log Xid scan).
- `modinfo` / `lsmod` (module version, best-effort).

A missing tool never aborts a run. The probes record a `Degraded` result and
the report flags the gap; the `baseline` preflight prints exactly which tools
are present and what degrades when one is missing. Root-only probes
(`dmidecode`, unrestricted `dmesg`, `lspci` AER detail) degrade to a warning
when not running as root.

The tool targets a **single** GPU, selected by name (`RTX 5090`). On a
multi-GPU box it picks the matching card and records a guardrail note; if no
card matches the name it falls back to the first GPU and says so.

## Build

```sh
CARGO_HOME=.cargo cargo build
# binary: target/debug/nvidia-diagnostic
```

## Subcommands

```
nvidia-diagnostic                  interactive menu
nvidia-diagnostic baseline         preflight + Stage-0 baseline snapshot + colored table
                              (--json [PATH] also writes the baseline as JSON)
nvidia-diagnostic monitor          live telemetry monitor: 10 Hz JSONL, 1 s Xid scan,
                              `m` marks black screens, auto reset detect
                              [--duration N] [--jsonl PATH] [--interval MS]
                              [--workload CMD [ARGS...]]
nvidia-diagnostic stress <SECS>    fixed-duration stress window under the monitor with
                              an `indicatif` progress bar; typically a
                              `--workload` stress command
nvidia-diagnostic analyze [SESSION]  pre-window correlation + scoring + verdict for a
                              monitor session (defaults to the newest
                              monitor-*.json); [--baseline PATH] adds the static
                              evidence. Writes a report-<ts>.json
nvidia-diagnostic experiment <X1..X8>
                              guided isolation experiment: show the steps, retest
                              under the monitor, ask the outcome, fold it into the
                              scores and write a report
nvidia-diagnostic report [PATH]    view / export a previous report (defaults to the
                              newest report-*.json)
nvidia-diagnostic reconstruct <JSONL>
                              rebuild a monitor-*.json sidecar from a raw telemetry
                              JSONL (after a force shutdown skipped the sidecar)
```

## The full battery (fresh machine)

The canonical, replicable procedure. Run it *while the failure reproduces*.

1. **Baseline.** `nvidia-diagnostic baseline` (add `--json baseline.json` to keep
   the static evidence for the analysis). This runs the preflight tool check,
   the Stage-0 probes (system, BIOS, driver, GPU, PCIe, Xid history) and the
   timeline interview (when did it start, what changed).

2. **Monitor under the workload.** `nvidia-diagnostic monitor` and launch your real
   CUDA workload in the same terminal (or pass it with `--workload`). Press
   `m` on each black screen; the tool also auto-detects driver resets and Xid
   79/154. Stop with `q` or Ctrl-C. It writes `telemetry-<ts>.jsonl` plus a
   `monitor-<ts>.json` sidecar.

   For an on-demand reproduction, run a bounded stress window instead:
   `nvidia-diagnostic stress 60 --workload <your-burn-command>`.

3. **Analyze.** `nvidia-diagnostic analyze --baseline baseline.json` correlates each
   failure marker with the 10 s pre-window (power, temp, clocks, throttle,
   PCIe link) and the Xids within ±2 s, builds the §6 evidence set, and prints
   the ranked verdict plus the suggested next experiment. It writes
   `report-<ts>.json`.

4. **Isolation experiments.** Run the top suggested experiment
   (`nvidia-diagnostic experiment X3`, for example), perform the physical change,
   retest under the monitor, and record the outcome. The tool folds the
   outcome into the scores (a "fixed after X3 reseat" is the strongest
   connector evidence) and writes an updated report.

5. **Verdict + report.** `nvidia-diagnostic report` re-opens the newest report.
   The JSON is what you share: it carries the baseline, timeline, telemetry
   summary, per-failure windows, Xids, markers, experiment runs, the evidence
   set, and the final ranked verdict.

## How the verdict works

Each evidence item carries signed integer weights per suspect
(`score.rs` holds the tunable table). The scores are summed per suspect and
ranked. The gap between the top score and the runner-up sets the confidence:
a gap under 3 prints the discriminating experiment to run. The strongest
signals are a Xid 79 at the failure (bus fall-off → chase the PCIe path) and a
"fixed after" experiment outcome.

## Edge cases

- **Missing tools.** Each probe degrades independently and records the gap;
  the preflight lists present/missing tools up front.
- **No root.** `dmidecode`, unrestricted `dmesg`, and `lspci` AER detail
  degrade to warnings; the PCIe probe falls back to the world-readable `sysfs`
  link state, and the Xid scan falls back to `journalctl -k`.
- **Multi-GPU.** The target card is selected by name; a guardrail note is
  recorded when more than one GPU is visible.
- **Force shutdown mid-monitor.** If the monitor is killed before it writes
  the sidecar, `nvidia-diagnostic reconstruct telemetry-<ts>.jsonl` rebuilds it
  from the raw JSONL and re-scans the kernel log (current boot plus the last
  15 previous boots) for Xids in the session window.

## Limitations

- 10 Hz sampling can miss sub-100 ms power transients; if the PSU stays the
  top suspect after a clean report, use an external power probe.
- 12VHPWR reseat, PSU swap, and BIOS changes are user-executed; the tool
  guides and retests, it does not do the physical work.
- `nvidia-smi --gpu-reset` and manual fan control may be unsupported on the
  5090; both are best-effort.
- Out of scope: Windows, CUDA-level profiling (nsight), firmware updates
  beyond recording the VBIOS version.
