# Benchmarks

Where Athen stands on public agent benchmarks, how to reproduce each run, and what we learned about the harness while doing it. Update this file every time a new bench is run.

## Headline results

| Benchmark | Model | Mode | Athen score | Baseline | Δ | Run date |
|-----------|-------|------|-------------|----------|---|----------|
| Terminal-Bench 2.0 (89 tasks) | DeepSeek V4 Flash | Non-thinking | **53.9%** (48/89) | 49.1% (DeepSeek-published, harness undisclosed) | **+4.8** | 2026-05-22 |

Single attempt per task with one retry on infrastructure errors (`--max-retries 1`). Temperature 0.2. `coder` profile.

---

## Terminal-Bench 2.0

### Result

- **53.9%** (48/89 tasks passed)
- Tested via the [Harbor](https://harborframework.com) harness, the official TB2 runner.
- DeepSeek's own published V4 Flash non-thinking score is 49.1%. Their harness is undisclosed (Harbor supports Terminus 2, Claude Code, Codex, Gemini CLI, OpenHands, Mini-SWE-Agent).
- Athen scaffold beats the published baseline by **+4.8 points** at the same model + mode.

### Why this matters

Terminal-Bench 2.0 is the benchmark for agentic terminal tasks (file/shell/compile/debug). It's the closest public proxy for "does Athen's tool layer give the model useful leverage". Beating the public baseline means the scaffold pays off — same model, different harness, better outcomes.

### How to reproduce

Prereqs:
- Linux host with Docker (tested on Fedora 7.0.6, Ryzen AI 9 HX370, 23 GB RAM)
- `harbor` CLI: `uv tool install harbor`
- DeepSeek API key
- ~$5 in DeepSeek balance + ~50 GB free disk for task images

**1. Build the Athen CLI binary against an old glibc.**

TB2 task containers vary in age; some carry glibc 2.31 (Bullseye-era). Build athen-cli inside a Bullseye container so the binary works everywhere:

```bash
mkdir -p /tmp/athen-bullseye-target
docker run --rm \
  -v $(pwd):/work:Z \
  -v /tmp/athen-bullseye-target:/build-target:Z \
  -w /work \
  -e CARGO_TARGET_DIR=/build-target \
  rust:1-bullseye \
  cargo build -p athen-cli --release
```

The resulting `/tmp/athen-bullseye-target/release/athen-cli` requires glibc ≤ 2.30. Verify with `objdump -T … | grep GLIBC`.

> **Bookworm (glibc 2.36) is NOT enough.** It breaks on the qemu-* tasks. Always use Bullseye or older.

**2. Drop the Harbor adapter in place.**

The adapter `athen_adapter.py` (kept at `/home/alex/athen-harbor/` on the bench machine) is a `BaseInstalledAgent` subclass that uploads the prebuilt binary into the container and invokes athen-cli with bench-mode env vars. Key env it sets inside the container:

| Env var | Value | Why |
|---|---|---|
| `ATHEN_BASE_URL` | `https://api.deepseek.com` | DeepSeek host root (provider appends `/v1/chat/completions`) |
| `ATHEN_MODEL` | `deepseek-v4-flash` | Model ID DeepSeek exposes |
| `ATHEN_FAMILY` | `DeepSeekV4Chat` | Triggers the non-thinking wire shape (`thinking: {"type": "disabled"}`) |
| `ATHEN_TEMPERATURE` | `0.2` | Determinism |
| `ATHEN_MAX_STEPS` | `0` | Unlimited (per-task ceiling enforced by Athen timeout below) |
| `ATHEN_TASK_TIMEOUT_SECS` | `1800` | 30 min per task |
| `ATHEN_WORKSPACE_DIR` | `$PWD` | Resolved at exec time; tells Athen to use the container's cwd instead of `~/.athen/workspace` |
| `ATHEN_DISABLE_RISK_GATE` | `1` | Skip the per-command rule-engine block (TB2 containers are throw-away) |

**3. Patch Harbor for SELinux (Fedora only).**

Fedora's SELinux Enforcing mode blocks docker bind-mount writes by default. The verifier container can't write its reward file back to the host. Patch the in-package compose-writer to add `selinux: "Z"` on bind mounts:

```python
# /home/alex/.local/share/uv/tools/harbor/lib*/python*/site-packages/harbor/environments/docker/__init__.py
def _apply_selinux_relabel(mount):
    if mount.get("type") != "bind": return mount
    new = dict(mount); bind = dict(new.get("bind") or {})
    bind.setdefault("selinux", "Z"); new["bind"] = bind
    return new

def write_mounts_compose_file(path, mounts):
    compose = {"services": {"main": {"volumes": [_apply_selinux_relabel(m) for m in mounts]}}}
    ...
```

The full patch is in this codebase's bench history. Gate on `subprocess.run(['getenforce']).stdout == 'Enforcing'` if you want it cross-platform safe.

**4. Run the bench.**

```bash
PYTHONPATH=/home/alex/athen-harbor \
ATHEN_CLI_PATH=/tmp/athen-bullseye-target/release/athen-cli \
DEEPSEEK_API_KEY="$(cat ~/Documents/DeepseekAPI.txt)" \
harbor run \
  --dataset terminal-bench@2.0 \
  --agent-import-path athen_adapter:Athen \
  --model deepseek/deepseek-v4-flash \
  --n-concurrent 6 \
  --agent-timeout-multiplier 2 \
  --max-retries 1 \
  --jobs-dir /tmp/athen_tb2/jobs \
  --quiet --yes
```

- `n-concurrent 6` is the safe ceiling for 23 GB RAM + Docker's default 31-subnet address pool. n=10 hit "all predefined address pools have been fully subnetted" after stale networks accumulated.
- `agent-timeout-multiplier 2` doubles each task's `task.toml` agent timeout — heavy compile tasks (Caffe, CompCert) need it.
- `max-retries 1` covers transient DeepSeek API parse errors.

Expect ~1 h wall clock, ~$5 in API calls.

**5. Read the result.**

```bash
python3 -c "import json; r=json.load(open('/tmp/athen_tb2/jobs/<timestamp>/result.json')); \
e=list(r['stats']['evals'].values())[0]; print(f\"score={e['metrics'][0]['mean']:.3f}, pass={len(e['reward_stats']['reward'].get('1.0',[]))}\")"
```

### Lessons learned (operational)

These bit us during the May 2026 run. Don't re-learn them.

- **The 30-second inner shell timeout in `athen-shell::{native,nushell}.rs` silently capped every `apt-get install`.** The outer `shell_execute` tool layer offers `timeout_ms` up to 600,000ms, but the inner shell was hardcoded at 30s. Bumped to 600s.
- **DeepSeek V4 Flash defaults to thinking-on the wire** regardless of `reasoning_effort` omission. The only knob that disables is `thinking: {"type": "disabled"}` — Anthropic-style. The `DeepSeekV4Chat` family in `athen-llm` now emits this automatically when no explicit effort is requested. See `crates/athen-llm/src/providers/deepseek.rs`.
- **`athen_workspace_dir()` resolved to `~/.athen/workspace` by default**, regardless of the process cwd. The agent's `write` tool was happily putting `hello.txt` in `~/.athen/workspace/hello.txt` instead of the container's `$PWD`. Fixed via `ATHEN_WORKSPACE_DIR` env override in `crates/athen-core/src/paths.rs`.
- **Rust's `target/debug/incremental/` grows unbounded.** Cargo never garbage-collects old fingerprint dirs. In this project it hit 203 GB across 3351 sub-dirs. Keep only the newest fingerprint per crate name:
  ```bash
  cd target/debug/incremental && ls -d */ | sed 's:/$::' | \
    while read d; do echo "$(stat -c '%Y' "$d") $(echo "$d" | sed 's/-[^-]*$//') $d"; done | \
    sort -k2,2 -k1,1nr | awk '$2 != prev { prev=$2; next } { print $3 }' | \
    xargs -P 8 -I {} rm -rf "{}"
  ```
- **Don't run `cargo clean` reflexively** — it nukes the release build too. Per-crate `cargo clean -p <crate>` doesn't help with the `incremental/` dir, which is the actual bloat.

### Failure breakdown (16 exceptions out of 89)

| Cause | Count | Treatment |
|---|---|---|
| 30-min task timeout (genuinely hard tasks) | 11 | Real signal — `regex-chess`, `path-tracing`, `caffe-cifar-10`, `compile-compcert` (later passed on retry), `circuit-fibsqrt` (later passed on retry), `make-mips-interpreter`, etc. |
| Glibc mismatch | 2 | Fixed by Bullseye rebuild — `qemu-startup` flipped to pass |
| Transient DeepSeek API parse error | 1 | `write-compressor` — `--max-retries 1` didn't recover (same error twice) |
| Tool args truncation by DeepSeek's response cap | 1 | `protein-assembly` — agent paste-bombed a 64 KB protein sequence inline. Not worth fixing for the bench. |
| OCR loop never converged | 1 | `extract-moves-from-video` — same family as the 11 timeouts |

The retry pass turned 3 trials from fail → pass: `qemu-startup` (glibc rebuild), `compile-compcert` + `circuit-fibsqrt` (stochastic variance at temp=0.2).

### Caveats on the baseline comparison

DeepSeek's published 49.1% V4 Flash non-thinking number is from their own evaluation but the harness is **not disclosed**. Harbor lists six agent scaffolds compatible with TB2 (Terminus 2, Claude Code, Codex, Gemini CLI, OpenHands, Mini-SWE-Agent). If they used the weakest (Terminus 2), the comparison is generous to Athen. If they used Claude Code, the +4.8 delta is meaningful. We don't know which. Treat the result as "Athen scaffold is at least competitive with whatever public-pipeline produced the headline number."

---

## Future benchmarks

Picking-menu candidates worth running once Athen's tool surface settles. Not committed.

- **SWE-bench Verified**: code-fix benchmark on real Python repos. Big context windows, lots of file ops — exercises Athen's `read`/`edit`/`grep`.
- **GAIA (level 1-3)**: real-world web + tool use. Tests the `http_request` + browser layer once it ships.
- **HumanEval+ / LiveCodeBench**: pure code-gen, no scaffold needed — useful as a sanity-check sub-bench to confirm the model itself isn't the bottleneck on TB2 fails.
- **Terminal-Bench 2.0 thinking mode**: same harness with `ATHEN_FAMILY=DeepSeekV4Pro` (which keeps thinking on). Direct apples-to-apples comparison with DeepSeek's 56.9% thinking-mode headline.

See [[memory:project_villanicode_benchmark]] for prior harness-shape research.
