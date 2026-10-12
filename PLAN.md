# PLAN — `nvidia-debug`: Reproducible Root-Cause Diagnosis for RTX 5090 Context Loss

Status: **planning only** (code lands in a later turn)
Repo state: fresh skeleton — `Cargo.toml` (deps: `inquire`, `indicatif`, `owo-colors`), `src/main.rs` stub.

---

## 1. Objective

Build a repeatable Rust CLI diagnostic tool (`nvidia-debug`) that:

1. Collects a structured baseline of the system (BIOS, driver, kernel, PCIe, GPU).
2. Captures live GPU telemetry (temp, power, clocks, throttle reasons, Xid events) **while the user's real CUDA workload runs** — the condition under which the failure reproduces.
3. Correlates failure events (black screen + fan-to-max) with telemetry to identify what preceded the context loss.
4. Guides the user through isolation experiments (BIOS/driver rollback, 12VHPWR reseat, PSU swap, cross-system test) and scores each suspect.
5. Emits a shareable JSON + terminal report so others can replicate the exact same troubleshooting steps.

The end state is a ranked verdict: **which suspect (BIOS / driver / card HW / PSU / cooling) is most likely the root cause, with the evidence trail.**

## 2. System & symptom context

| Fact | Value |
|---|---|
| GPU | NVIDIA RTX 5090 (Blackwell, GB202, 32 GB GDDR7, TGP 575 W, 3× 12VHPWR) |
| OS | Linux (this machine), NVIDIA kernel module + `nvidia-smi` |
| Symptom | Intermittent context loss under high CUDA utilization: display goes black, card fans spool to max |
| Timing | Started after a recent **BIOS update** and a recent **GPU driver update** |
| Notable | Card is rated to > 90 °C, so simple "it's hot" is less likely, but must still be ruled out |

**Black screen + fan-to-max is the signature of a driver-level GPU reset** (the kernel module resets the GPU, display output drops, fans return to default control until the driver reinitializes). The most informative signal is therefore the **Xid error** logged just before/at the reset.

### Primary Xid errors to detect (dmesg `NVRM: Xid (PCI:BDF) <id>`)

| Xid | Meaning | Implicates |
|---|---|---|
| 79 | GPU fallen off the bus | PCIe path: connector/12VHPWR, slot, BIOS PCIe settings, PSU |
| 43 / 45 / 68 | Graphics / video engine exception | Driver, card HW |
| 48 / 95 | Double-bit / uncorrectable ECC | Card HW (memory) |
| 63 / 64 | ECC row remapping (ok / failed) | Card HW (memory) |
| 66 / 92 | Thermal trip / high-temperature shutdown | Cooling |
| 94 | Atomic intra-GPU error | Card HW / driver |
| 109 / 119 | Internal micro-op / P2P error | Card HW / driver |
| 137 | Unsupported GPC clock | Driver / BIOS clock config |
| 31 | Mem page fault | Card HW |

## 3. Suspect hypotheses (ranked, to be confirmed/refuted by evidence)

| # | Suspect | Why suspect | What confirms it |
|---|---|---|---|
| H1 | **GPU driver** (recent update) | Onset correlates with update; Xid 43/45/68/109 class errors are driver-context failures | Errors of that class recur; **fixed on driver rollback** |
| H2 | **Motherboard BIOS** (recent update) | Onset correlates with update; BIOS owns PCIe training, power state, Above-4G/ReBAR | Xid 79 + PCIe link degradation (LnkSta < LnkCap, AER errors); **fixed on BIOS rollback** |
| H3 | **12VHPWR / PCIe connector** (a form of "card HW", classic 4090/5090 failure) | Load-dependent, intermittent, Xid 79-class | **Fixed on reseat**; Xid 79 with clean power/thermal |
| H4 | **PSU** | 575 W TGP with large transients; sag → bus fall-off | Power-draw spike/sag before reset; **fixed on PSU swap** |
| H5 | **Card hardware** (VRAM, mem controller, GPC) | Latent suspect | Xid 48/63/64/95; reproduces in a known-good second system |
| H6 | **Cooling** | Latent suspect, card rated > 90 °C | Throttle reasons active, temp ≥ trip point at failure; fixed on fan curve / airflow change |

The tool's job is to accumulate evidence for/against each and emit a ranked verdict.

## 4. Tool design

### 4.1 CLI UX (`inquire`-driven)

Entry: `nvidia-debug` (no args → interactive menu) plus subcommands for scripted use.

```
? What do you want to do?
  ▸ Full diagnostic battery            (recommended)
  ▸ Monitor only                       (run your workload, we watch)
  ▸ Rerun an isolation experiment       (after a BIOS/driver/PSU change)
  ▸ View / export previous report
```

**Full battery flow** (the canonical, replicable procedure):

1. **Preflight** — detect `nvidia-smi`, driver version, kernel, root availability (probes needing root degrade to warnings). Ask the user (inquire): *when did the failures start? what changed around then (BIOS? driver? PSU?)* — this builds the timeline used for correlation.
2. **Baseline snapshot** — run all static probes (§5, Stage 1) → `Baseline` record.
3. **Live monitor** — sampler starts (10 Hz telemetry, 1 s Xid scan); user launches their real CUDA workload (or supplies a command to the tool, e.g. the inference server); user presses **`m`** to mark each black-screen moment. Auto-detects driver resets.
4. **Stress window (optional)** — user-supplied stress command (e.g. a CUDA burn) run under the monitor for a fixed duration, with an `indicatif` progress bar.
5. **Analysis** — parse Xid stream, correlate each failure marker/reset with the last ~10 s of telemetry (power, temp, clocks, throttle, PCIe link state).
6. **Scoring & verdict** — score each hypothesis (§6), print ranked result with `owo-colors`, suggest the next isolation experiment.
7. **Guided experiments** — see §5 Stage 5; after each, user re-runs monitor + stress and the tool folds the result into the scores.
8. **Report** — write `report-<timestamp>.json` + human summary; `nvidia-debug report` re-opens it.

`monitor` subcommand = step 3 only (for use under an already-running session). `experiment <name>` re-runs baseline + monitor + scoring after the user performed the change.

### 4.2 Architecture / module map

```
src/
├── main.rs            # entry: subcommand dispatch + interactive menu
├── cli.rs             # inquire menus, confirmations, event-marker key handling
├── util/
│   └── cmd.rs         # CommandRunner: spawn/parse shell commands, tolerant of missing tools
├── probe/
│   ├── mod.rs         # trait Probe { name, requires_root, run(&BaselineCtx) -> ProbeResult }
│   ├── system.rs      # uname, modinfo nvidia, lsmod, kernel
│   ├── bios.rs        # dmidecode -s bios-version/-s bios-date/-s system-sku (root)
│   ├── gpu.rs         # nvidia-smi: name, serial, vbios, driver, temp, power, clocks, mem, ECC, perf
│   ├── pcie.rs        # lspci -vvv: LnkCap vs LnkSta, AER counters, slot
│   └── logs.rs        # dmesg/journalctl Xid + NVRM history, last reset times
├── telemetry/
│   └── sampler.rs     # 10 Hz JSONL writer: temp, power, clocks, throttle, fan, util
│                      # 1 s incremental Xid scan; 2 s lspci link-state poll
├── events/
│   └── xid.rs         # regex parser for "NVRM: Xid (PCI:xx:00.0) <id>" + NVRM reset lines
│                      # (BDF: short `01:00.0` or domain-prefixed `0000:01:00`)
├── workload.rs        # spawn user workload / stress command, capture exit, live PIDs
├── analysis.rs        # M3: session load, 10 s pre-window correlation, §6 evidence
├── experiments.rs     # definitions + guided steps for X1..X8 (§5.5)
├── score.rs           # M3: §6 weight table, Evidence -> ranked verdict, printout
├── report.rs          # serde JSON report + owo-colors terminal summary
└── model.rs           # Baseline, TelemetrySample, XidEvent, FailureMarker,
                       # FailureCorrelation, Evidence, Verdict, Analysis
```

New dependencies (add to `Cargo.toml`): `serde` + `serde_json` (report), `chrono` (timestamps), `anyhow` (errors), `regex` (parsing). Keep `inquire`, `indicatif`, `owo-colors` as-is. No CUDA dependency in the main crate — the workload is a user-supplied external command (keeps the build light and the tool portable).

### 4.3 Key data model

```rust
struct Baseline { ts, system: SystemInfo, bios: BiosInfo, driver: DriverInfo,
                  gpu: GpuInfo, pcie: PcieInfo, xid_history: Vec<XidEvent>,
                  user_timeline: TimelineAnswers }

struct TelemetrySample { ts, temp_gpu, temp_mem, power_draw, power_limit,
                         clock_graphics, clock_sm, fan_pct, throttle_reasons,
                         util_gpu, util_mem, pcie_link: Option<(gen, width)> }

struct XidEvent { ts, bdf, id, raw }
struct FailureMarker { ts, kind: UserMark | DetectedReset, note }

enum Suspect { Driver, Bios, PcieConnector, Psu, CardHardware, Cooling }
struct Evidence { id, description, weight_per_suspect: BTreeMap<Suspect, i8>, source }
struct Verdict { ranked: Vec<(Suspect, score, Vec<Evidence>)>, next_experiment: Option<String> }
```

### 4.4 Report format

`report-<ts>.json`: `{ baseline, timeline, telemetry_summary (min/max/avg + window around each failure), xid_events, failure_markers, correlations, experiments_run, scores, verdict }`. The JSON is the unit of replication: anyone can diff two reports (before/after a change, or two machines).

## 5. Diagnostic stages (the procedure the tool automates)

### Stage 0 — Preflight & baseline snapshot

| Probe | Command | Notes |
|---|---|---|
| Driver/kernel | `nvidia-smi --query-gpu=driver_version,vbios_version,serial --format=csv`; `modinfo nvidia \| grep ^version`; `uname -r` | |
| BIOS | `dmidecode -s bios-version -s bios-date -s bios-vendor` | root; warn if unavailable |
| GPU state | `nvidia-smi -q -d TEMPERATURE,POWER,CLOCK,MEMORY,ECC,PERFORMANCE` | includes max-op temp (90 °C), power limits, throttle reasons, ECC counters |
| PCIe | `lspci -vvv -s <bdf>` → `LnkCap` vs `LnkSta`, AER counters | link running below capability → PCIe/BIOS signal |
| Xid history | `dmesg -T \| grep -E 'NVRM|Xid'` | full history before the test, to see past events |

**Pass criteria:** `nvidia-smi` healthy, no *new* Xids at rest, link at LnkCap speed. Any new Xid at rest is itself evidence (record which).

### Stage 1 — Live telemetry monitor (the core)

- Sampler at **10 Hz** (`nvidia-smi` CSV batch query) writing JSONL; **1 s** incremental Xid scan (`dmesg` tail); **2 s** PCIe link-state poll.
- User runs their real CUDA workload (the exact condition that fails). Key **`m`** marks each black screen; the tool also auto-detects resets (driver log lines / Xid 79 / `nvidia-smi` going stale).
- For each marker: extract the **10 s pre-window** (power, temp, clocks, throttle, link) + the Xid(s) within ±2 s. This is the correlation that localizes the root cause.

**Interpretation rules (first pass):**
- Xid 79 at marker → bus fall-off: chase PCIe path (connector, slot, BIOS, PSU).
- Xid 43/45/68 at marker → engine/driver context fault.
- Xid 48/63/64/95 → memory hardware.
- Xid 66/92 or `HW Thermal` throttle active in pre-window → cooling.
- Power draw at/over limit in pre-window with clean temp/link → PSU transient.
- No Xid, clean pre-window, but display lost → display/EDID path (lower priority; note it).

### Stage 2 — Optional stress window

Run a user-supplied stress command (CUDA burn, the inference server at peak batch) for a fixed duration under the monitor, with an `indicatif` bar. Goal: **reproduce on demand** so experiments (§5.5) can be A/B tested quickly. Record: did it reproduce? which Xid?

### Stage 3 — Static evidence triage

Fold Stage 0/1/2 results into the scoring model (§6) and print the ranked suspects + the single most informative next experiment.

### Stage 4 — Guided isolation experiments (user executes, tool retests)

Each experiment = { description, preconditions, tool steps (re-run baseline + monitor + stress), post-check }. After each, fold results into scores.

| ID | Experiment | Targets |
|---|---|---|
| X1 | **Driver rollback** to the last known-good version (apt/dkms), reboot, retest | H1 |
| X2 | **BIOS rollback** to previous version (record both versions in report), retest | H2 |
| X3 | **Power off, reseat 3× 12VHPWR + PCIe card** (known 5090 failure mode), retest | H3 |
| X4 | **Swap PSU** to a known-good unit, retest | H4 |
| X5 | **PCIe slot change / clamp Gen5→Gen4** in BIOS, retest | H2/H3 |
| X6 | **Toggle Resizable BAR / Above-4G Decoding** in BIOS, retest | H2 |
| X7 | **Card into a known-good second system**, run stress | H5 (reproduces → card; not → system-side) |
| X8 | **Fan curve / airflow check** (`nvidia-smi --fan-control` best-effort; 5090 may not allow manual fan) | H6 |

Order of execution is driven by the current score ranking (cheapest discriminating experiment first — e.g. X1/X2 are free if the failure onset clearly postdates one of the updates; X3 is a 2-minute power-off task with high discriminating power for Xid-79 cases).

### Stage 5 — Verdict

Final ranked verdict with per-suspect evidence list, confidence, and "what we would do next if it were my machine". Export report.

## 6. Scoring model

Evidence items carry signed integer weights per suspect; total = sum. Weights (tunable in `score.rs`):

| Evidence | Driver | BIOS | Conn/PCIe | PSU | Card HW | Cooling |
|---|---|---|---|---|---|---|
| Xid 79 (bus fall-off) at failure | +1 | +2 | +3 | +2 | 0 | 0 |
| Xid 43/45/68 at failure | +3 | 0 | 0 | 0 | +1 | 0 |
| Xid 48/63/64/95 (ECC) | 0 | 0 | 0 | 0 | +4 | 0 |
| Xid 66/92 (thermal) | 0 | 0 | 0 | 0 | 0 | +4 |
| Throttle reason active in pre-window | 0 | 0 | 0 | 0 | 0 | +3 |
| Power at/over limit in pre-window | 0 | 0 | +1 | +3 | 0 | 0 |
| LnkSta < LnkCap | 0 | +2 | +2 | 0 | 0 | 0 |
| AER uncorrectable errors | 0 | +1 | +2 | 0 | 0 | 0 |
| Onset postdates driver update (user timeline) | +2 | 0 | 0 | 0 | 0 | 0 |
| Onset postdates BIOS update (user timeline) | 0 | +2 | 0 | 0 | 0 | 0 |
| **Fixed after X1 driver rollback** | +4 | 0 | 0 | 0 | 0 | 0 |
| **Fixed after X2 BIOS rollback** | 0 | +4 | 0 | 0 | 0 | 0 |
| Fixed after X3 reseat | 0 | 0 | +4 | 0 | 0 | 0 |
| Fixed after X4 PSU swap | 0 | 0 | 0 | +4 | 0 | 0 |
| Reproduced in known-good 2nd system | 0 | 0 | 0 | 0 | +3 | 0 |
| NOT reproduced in 2nd system | 0 | 0 | 0 | 0 | −2 | 0 |
| Failure with temp well below trip, fans not pinned pre-event | 0 | 0 | 0 | 0 | 0 | −2 |

Confidence = top score vs. runner-up gap; verdict text states the discriminating experiment if the gap is < 3.

## 7. Implementation plan (for the next turn)

1. **M1 — skeleton & baseline**: add deps; `util/cmd.rs`; `probe/*` for Stage 0; `model.rs`; print colored baseline table.
2. **M2 — monitor**: `telemetry/sampler.rs` JSONL writer, `events/xid.rs` parser, marker key handling, reset detection.
3. **M3 — analysis & scoring**: pre-window correlation, `score.rs` with the §6 table, verdict printout.
4. **M4 — experiments & report**: `experiments.rs` guided flow, `report.rs` JSON export, `report` subcommand.
5. **M5 — polish**: `indicatif` bars on stress windows, README (usage + how to run the battery on a fresh machine), edge cases (missing tools, no root, multi-GPU guardrail: tool targets the single 5090, detected by name).

Error-handling policy: probes never abort the run — a failed/missing command records `ProbeResult::Degraded { reason }` and the report flags the gap. Root-only probes warn once.

## 8. Testing & validation

- **Unit:** Xid/NVRM regex parser, `lspci` LnkCap/LnkSta parser, `nvidia-smi` CSV parsing (fixture strings from this machine).
- **Integration on this machine:** full battery at rest (no Xids expected); monitor under a real inference session; induce a benign reset (`nvidia-smi --gpu-reset -i 0`, only when no workload is attached) to validate reset detection + Xid capture.
- **End-to-end:** run the battery *during* a reproducing session; confirm the pre-window correlation localizes the failure; then run the top suggested experiment and confirm the verdict flips.
- **Replication check:** run the battery on a second, healthy system → all scores near zero (no false positives).

## 9. Risks, assumptions, limitations

- **Assumes Linux + nvidia driver + `nvidia-smi` + `lspci` (+ optional `dmidecode` as root).**
- 10 Hz sampling may miss sub-100 ms power transients; if PSU stays a top suspect after a clean report, recommend an external power probe (limitation, documented in README).
- 12VHPWR reseat / PSU swap / BIOS changes are **user-executed** — the tool guides and retests, it does not do the physical work.
- `nvidia-smi --gpu-reset` and fan control may be unsupported on the 5090; both are best-effort.
- Running the battery while the inference session is live is fine for monitoring; a GPU reset during the test will interrupt the session (warn in UI).
- Out of scope: Windows, multi-GPU systems, CUDA-level profiling (nsight), firmware updates beyond recording VBIOS version.

## 10. Deliverables

1. This `PLAN.md`.
2. The `nvidia-debug` Rust CLI implementing §4–§6 (next turn).
3. `README.md` with the replication procedure for others.
4. Sample `report-*.json` from a run on this machine.

## 11. Status

Each chunk below is self-contained and can be started at any point; recommended order is M1→M5, but M2+ only need `model.rs`/`util/cmd.rs` from M1. Check off as work lands.

- [x] **M1** — skeleton & baseline: deps, `util/cmd.rs`, `probe/*` (Stage 0), `model.rs`, colored baseline table
- [x] **M2** — monitor: `telemetry/sampler.rs` (JSONL), `events/xid.rs` parser, `m`-key marker, reset detection
- [x] **M3** — analysis & scoring: `analysis.rs` (session load, 10 s pre-window correlation, ±2 s Xid attribution, §6 evidence), `score.rs` (§6 weight table, ranked verdict, confidence + discriminating experiment), `analyze` subcommand, full-battery flow (baseline → monitor → analyze)
- [x] **M4** — experiments & report: `experiments.rs` (X1–X8 definitions, outcome→§6-evidence mapping, guided flow), `report.rs` (build/write/load/print the shareable `report-<ts>.json`), `experiment` + `report` subcommands, `reconstruct` subcommand (rebuild a `monitor-*.json` sidecar from a bare JSONL after a force shutdown), `analyze`/battery now write a report
- [x] **M5** — polish: `indicatif` progress bar on the monitor (determinate over `--duration`, elapsed clock otherwise, rendered on stderr) + a `stress <SECS>` subcommand (fixed-duration stress window, §5 Stage 2); `probe/target.rs` multi-GPU guardrail (select the RTX 5090 by name, used by the gpu/pcie probes and the sampler); `baseline` preflight tool-availability summary (missing tools + no-root degrade notes); `README.md` (usage + fresh-machine battery procedure)

Current pointer: **complete through M5** (all five chunks landed; 51 unit tests). M5 polish: the `indicatif` progress bar + `stress` subcommand, the `probe/target.rs` name-based multi-GPU guardrail, the `baseline` preflight tool-availability summary, and `README.md`. Earlier analysis fixes stand: (1) Xid 79 + its follow-up Xid 154 (same event, within 2 s) collapse to a single marker; (2) the standing 0x400 "Reliability" throttle bit is masked out of `throttle_active`; (3) `reconstruct` scans the current + last 15 previous boots. The 2026-10-12 live crashes (Xid 79 + 154) rank **12VHPWR / PCIe connector** first and suggest X3 (reseat). The 12VHPWR connector has now been checked for seating issues and none were found, so X3 (reseat) and X4 (PSU swap) remain the discriminating next steps; a power limit is applied to the card via `nvidia-smi` for testing.
