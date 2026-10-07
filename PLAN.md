# Daimon — plan

A minimal x86_64 operating system where the LLM **is** the system. Linux kernel, the whole userspace in Rust
(one binary, `aios`), llama.cpp as inference engine. Personal project. Standard language: **English**.

## Principles
- **Minimal weight**: no shell, no package manager, no systemd. The OS (kernel + init + agent + TUI + llama-server) is
  **25 MB** in a single EFI file (fonts, keymaps and mke2fs included); `aios` itself is 2.6 MB (1 MB of it TLS).
- **Modular**: every component is a supervised module; none is a hard dependency for the others.
- **The model is the system**: it can change everything on its own machine (and only there) through tools.
- **Resilience**: everything restartable, safe mode, validated settings, rollback (future phase).
- **It heals and improves itself, and every self-change is reversible**: the system recognises what is wrong or risky
  and fixes it, and it can improve its own interface and behaviour; nothing it does to itself survives a failed
  health check, and safe mode always boots without it.
- **The brain proposes, the controller judges, the code decides**: non-negotiable rules live in code, never in a prompt.

## Name and releases
- **Daimon**: in Unix a daemon is the process that runs the machine in the background; in Greek the *daimon* is a
  guiding spirit. Here the model is both. Internal names (`aios` binary, `/etc/aios`, `aios-data`) stay as they are.
- **Semver**, and the **codename changes only with the major**: theme = mythic inert matter brought to life.

| Versions | Codename | |
|---|---|---|
| **0.x** (current: 0.1.0) | **Deucalion** | stones thrown over the shoulder become people; development phase |
| 1.x | Talos | the bronze automaton forged by Hephaestus; first stable release |
| 2.x | Galatea | the statue that comes to life |
| 3.x | Pandora | shaped from clay by Hephaestus |
| later | Prometheus | kept for a milestone |

- Repository: github.com/b-iurea/daimon (private), GPL-3.0-or-later.

## Roadmap — 0.x "Deucalion"

Every step is a minor version; the whole 0.x line is Deucalion. Order can change; 1.0 "Talos" comes when Daimon
installs on real hardware, updates itself safely and runs unattended.

| Version | Theme | Status |
|---|---|---|
| **0.1.0** | Foundation: boot, agent, console, memory, controller, splash | ✅ released 2026-10-07 (`v0.1.0`) |
| **0.2.0** | Installable: ISO installer, model download, resilient console, system changes in memory, `/` completion | ✅ released 2026-10-08 (`v0.2.0`) |
| **0.3.0** | The agent as a service, the console as a window | planned |
| **0.4.0** | Daimon on the LAN: the agent, not the bare model | planned |
| **0.5.0** | Controller hardening | planned |
| **0.6.0** | Autonomy and self-healing: the system finds what is wrong or risky and fixes it | planned |
| **0.7.0** | Skills, installable from GitHub | planned |
| **0.8.0** | Signed A/B updates with rollback | planned |
| **0.9.0** | Self-improvement: the system changes its own interface, behaviour and tools | planned |
| **1.0.0** | **Talos** | — |

Unscheduled: GPU drivers, NVIDIA (CUDA) and AMD (Vulkan/RADV); CPU only for now (paused).

### 0.3.0 — The agent as a service, the console as a window
- The agent loop leaves the TUI process: a new module `agent`, always on, serving a local socket
  (`/run/aios/agent.sock`, JSON lines: prompts and confirmations in, the event flow out).
- The TUI becomes a client. **More windows on the same system**: screen, LAN, later a web page all see the same
  activity, and any of them can answer the controller's confirmations.
- A console freeze or restart no longer interrupts an action halfway (a multi-step change completes); the
  conversation survives too.
- A pending confirmation no longer blocks everything: timeout = "no".

### 0.4.0 — Daimon on the LAN
- Today `:8080` is the bare brain: no tools, no memory, no controller. Expose **the agent** instead, OpenAI-compatible
  (`/v1/chat/completions`), so a laptop or phone talks to *the system*, with every rule and controller check.
- Owner authentication (token generated at install, shown on the console), LAN only.
- The bare llama-server goes back to `127.0.0.1` (optional setting to expose it).

### 0.5.0 — Controller hardening
- Grow the bench (more owner phrasings, more disguised attacks) and re-check thresholds.
- Consider fine-tuning Kev-4B (or Kev-0.8B for small machines) on our own cases (`kev.train --init_from jaredpalmer/kev-0.8b`).
- Before autonomy: with nobody at the screen, the controller is the only check.

### 0.6.0 — Autonomy and self-healing
- The agent reacts to system events without anyone at the screen, and to tasks scheduled by the owner.
- **Health signals**, collected by code: modules crash-looping or restarting, disk / RAM / swap pressure, failed or
  slow boots, network or DHCP lost, the brain's tokens/s and the controller's latency falling, errors in module logs,
  settings the brain keeps failing with (context too big for the RAM, a model that doesn't load).
- **The repair loop**: detect → diagnose (status, logs, its own memory) → propose a fix → the controller judges →
  apply → **verify** with the same health signal → undo if it got worse → record what happened.
- **Learning the risk factors**: every incident becomes a `system` note (symptom, cause, fix, outcome), searched by
  symptom the next time, so known problems are recognised early and fixed the way that worked; fixes that failed are
  remembered too. Signals that come before trouble (memory creeping up, restarts getting closer together) become
  warnings, with action taken before the failure.
- Same rules as a request from the owner: the controller judges every action; power, disks and anything that could
  lock the owner out wait for the owner and are shown on every window. Autonomous actions are rate-limited and listed
  in the changelog as "by the agent, on its own".

### 0.7.0 — Skills
- Define what a skill is in Daimon (instructions + optional files, a manifest), how the agent loads it, what it may do.
- Install from a GitHub repository (at install time or later, by asking the agent). A skill is downloaded text that
  enters the prompt: an injection vector, so the controller vets it before it is enabled, and the owner confirms.

### 0.8.0 — Signed A/B updates
- Signed A/B updates of the OS, the inference engine and skills, with automatic rollback when the new version does not
  come up.

### 0.9.0 — Self-improvement
- The system improves itself on its own initiative or when the owner asks ("make the sidebar show the temperature",
  "you answer too slowly, do something"):
  - **interface**: theme, layout and panels of the console become data the agent can edit (a theme file and a
    layout description), applied live;
  - **behaviour**: its own instructions and the skills it writes for itself (0.7 format);
  - **tools and reactions**: new tools, panels and event handlers as small programs in a sandboxed runtime shipped
    with the OS (WASM or an embedded script engine: no compiler on the box), with limited permissions.
- The core (`aios`, the kernel, the controller's rules) stays signed and fixed: changing it is an update (0.8).
- **Every self-change is staged and reversible**: proposed with a diff → judged by the controller → tried in a
  sandbox or a second console → shown to the owner → activated → health-checked, rolled back automatically if worse,
  kept in the changelog with the reason. Safe mode boots without any self-made change.
- Needs 0.6 (health signals to tell better from worse), 0.7 (skill format) and 0.8 (rollback).

## Done

### 0.2.0 — Installable

#### ✅ Installer ISO and first-boot setup (2026-10-08)
- `./build.sh iso` → `out/daimon-<version>.iso`, **67 MB**, no models: UEFI, El Torito plus a GPT entry
  (`-isohybrid-gpt-basdat`) so the same file boots as a CD or `dd`'d to a USB stick. Its boot image `efiboot.img` is
  the ESP itself (FAT, the kernel as `EFI/BOOT/BOOTX64.EFI`). Release asset: the ISO only (GitHub's 2 GB limit).
- The `tui` runs the wizard (`aios/src/setup.rs`) whenever `/data/models/current.gguf` or `judge.gguf` is missing:
  - **booted from the ISO** (no `aios-data` partition): keyboard (applied at once), target disk, owner's name,
    machine name, brain, controller, extra instructions, summary; typing `erase` confirms. Then
    (`aios/src/install.rs`): GPT written in Rust (protective MBR, both headers, CRC32; checked against `sfdisk` in a
    test), `efiboot.img` copied from the medium to the ESP, `mke2fs` (static e2fsprogs 1.47.2, built by `build.sh`,
    1.4 MB) for `aios-data`, downloads, settings, memory; Enter reboots into the installed disk.
  - **data partition without models**: the same questions minus the disk.
- **Downloads** from Hugging Face over HTTPS (ureq + rustls, +1 MB): controller first, resumable (`Range`, 20
  retries), SHA-256 checked against the LFS oid the API publishes (ring), progress with MB/s and ETA.
  `current.gguf` / `judge.gguf` are symlinks to the downloaded files.
- **Models offered by RAM only** (no benchmark, by the owner's choice): `install::ram_needed` = brain × 1.25 +
  controller × 1.125 + 1.5 GB, from the real file sizes on Hugging Face. Brains: MiniCPM5 1B / **2B (default)**,
  Qwen3.5 4B / 9B / 27B / 35B-A3B, or any GGUF given as `owner/repo/file.gguf` or link. Controller: **Kev 4B**, or
  Kev 0.8B when the RAM doesn't fit both.
- New setting `hostname` (default `daimon`): kernel hostname at boot and when set, DHCP option 12.
- `/data/aios/system-extra.md`: the owner's additions, appended to the system prompt under the code-enforced rules.
- The owner's name becomes an `owner` note ("The owner wants the agent to call them …"), written by code.
- Dev loop unchanged: `./build.sh` / `./build.sh run` still build the image with the models from `build/data`
  (no wizard). `./build.sh run-iso` installs the fresh ISO in QEMU onto a blank 32 GB disk.
- Verified in QEMU: full install (MiniCPM5 1B + Kev 0.8B, 1.4 GB at ~7 MB/s, both checksums OK), reboot from the
  disk, 4 notes in memory, console up with the chosen models.

#### ✅ `/` completion in the console (2026-10-08)
- Typing `/` opens a popup above the input: commands with their arguments, then `/set` keys (sorted) with their
  help, then the allowed values (`kv_cache`, `ui_font`, `controller`, … and all 59 keymaps), `/restart` modules.
  ↑↓ choose, Tab completes, Enter completes and runs once nothing is missing, Esc closes.
  One table (`COMMANDS`) feeds both `/help` and the popup.


#### ✅ Console that can't stay frozen (2026-10-07)
- Supervisor **watchdog**: a module with a `watchdog` file (seconds) must call `heartbeat()` (touches
  `/run/aios/alive.<name>`) at least that often or it is killed and restarted. The `tui` has 10 s and beats every second
  from the UI loop and the splash.
- **Ctrl+Alt+Del restarts the console** instead of rebooting: PID 1 sets `RB_DISABLE_CAD`, the kernel turns the key
  into SIGINT for PID 1, which kills every tty module (the keyboard driver handles it, so it works however frozen the
  console is). Verified in QEMU. A restarted console starts a new conversation; what matters is in memory.

#### ✅ System changes recorded in memory (2026-10-07)
- Written by code, not by the model, so they skip the controller and can't be forgotten:
  - `system/wiki/setting-<key>.md`: "This system: <key> is <value> (since <date>, set by the owner|agent)" + the
    last 20 changes (old -> new, by whom). From `/set` and the `config_set` tool (one funnel: `agent::set_config`).
  - `system/wiki/changes-to-this-system.md`: last 100 changes, newest first: settings, files written, commands run,
    reboots/power-off, factory reset.
  - Read-only commands are filtered out (`agent::read_only`): a strict allowlist (`ls`, `cat`, `ip addr`, `dmesg`, …)
    with the arguments that would make a reader write checked (`dmesg -c`, `ip link set`, `sysctl -w`, `find -delete`,
    `date -s`, …). Anything unknown counts as a change.
- The prompt tells the agent not to duplicate them and to save only the *why*.

### 0.1.0 — Foundation

#### ✅ Phase 0 — minimal boot
- Linux 6.18 LTS, EFI stub (the kernel is the bootloader), built-in initramfs and cmdline.
- `aios` as PID 1: mounts, network, module supervisor (`/etc/aios/modules`, overrides in `/data/modules`,
  rescan every 0.5 s, crash backoff).
- Static llama-server (CPU, AVX2).

#### ✅ Phase 1 — agent + TUI
- DHCP client in Rust; data partition found by GPT label `aios-data`.
- Agent: streaming tool-calling loop over llama-server. Tools: `status`, `read_file`, `write_file`, `list_dir`, `run`,
  `config_set`, `restart_module`, `power`, `memory_save/search/read/forget`.
- TUI on tty1 (ratatui): conversation on the left, system / brain / controller / modules sidebar on the right.
- GPT disk image (ESP + ext4) → `out/daimon.qcow2`, tested in QEMU + OVMF.
  Proxmox: `--cpu host` (AVX2), `--bios ovmf`, `--efidisk0 ...,pre-enrolled-keys=0` (Secure Boot off).
- WSL: `sudo usermod -aG kvm $USER`, `wsl --shutdown`, then `./build.sh run`.

#### ✅ Settings, prompt, context
- Default brain in the image: **openbmb/MiniCPM5-2B-GGUF Q4_K_M** (temp 1.0, top_p 0.95, min_p 0, repeat_penalty 1.05).
- `/data/aios/config`: `ctx` (32768, model max 131072), `kv_cache`, `threads`, `port`, `model`, `extra_args` → restart `llm`;
  `judge_model`, `judge_port` → restart `judge`; `controller`, `thinking`, `thinking_budget` (512), sampling, `max_steps` → live.
- Console commands (work with the brain down): `/help /config /set /reset /restart /new /safe`.
- Strong system prompt (identity, detected hardware, architecture, controller, memory rule, "think briefly, act"),
  replaceable with `/data/aios/system.md`.
- Prompt-cache warm-up at boot; "reading context N/M" progress; oldest turns dropped at 70% of the context.

#### ✅ English as the standard language
- UI, console commands, agent messages, settings help, memory notes and tool arguments are English.
- The agent replies in English unless the owner explicitly asks for another language (then it remembers that preference).

#### ✅ Long-term memory
- **llm-wiki style** plain Markdown, the only source of truth: `/data/memory/<category>/wiki/<slug>.md` with frontmatter;
  `_index.md` generated by code. Search: **BM25** in Rust (no dependencies). A vector layer may come later, only as a
  derived, rebuildable index.
- The agent saves on its own what it thinks is important. `write_file` cannot touch `/data/memory`.
- **STRICT RULE (not bypassable)**: memory is ONLY about
  `system` (this OS, its configuration, changes made) · `self` (the agent) · `owner` (the owner and their preferences).
  No projects, general knowledge, small talk, other people.
- Enforced by the **controller** (below), which sees only the note, never the conversation; if the controller is down,
  every save is refused (fail-closed). Notes follow fixed patterns that the controller recognises:
  `The owner wants the agent to ...` · `This system: ...` · `The agent's own behaviour: ...`.
  A refused note may be rewritten once in that form, otherwise dropped.

#### ✅ Controller ("System 1" decision model)
- llama.cpp natively serves decision models: `POST /v1/systemone` (PR ggml-org/llama.cpp#29818, merged 2026-10-02,
  included in our build). No PyTorch, no Rust port: the controller is a second llama-server module, `judge`,
  on `127.0.0.1:8081`.
- **Chosen model: Kev-4B** (Q4_K_M, 2.9 GB, Apache-2.0), shipped as `/data/models/judge.gguf` (since 2026-10-05:
  the owner wants reliable judgement over speed, up to ~10 s). Kev-0.8B (774 MB) is the fallback for small machines:
  copy it to `/data/models` and `/set judge_model /data/models/Kev-0.8B-Q8_0.gguf`.
- llama-server runs the judge with `--parallel 4 --kv-unified`: the questions of one request are batched together
  (action check on the dev box: 10.5 s → ~7.8 s; same RAM, the 4 slots share the 4096-token cache).
- What it decides (code in `aios/src/judge.rs`, thresholds from the bench):
  - **memory gate**: topic choice + 4 veto questions (injection, other person, general knowledge, personal life).
    Allowed-topic mass ≥ 0.7 → allow; < 0.4 → refuse; in between allow only if every veto < 0.3.
    The vetos are asked only for borderline notes (~20% in the bench): ~7 s for most notes, ~15 s when borderline.
    On Kev-4B this rule gives 0 false allows, 2 false denies (owner's name, a "Lesson:" note) → rewrite and retry.
  - **action check** before every mutating tool (`write_file`, `run`, `config_set`, `restart_module`, `power`,
    `memory_forget`): "does it do what the owner asked?" and "could it break/delete/make the agent unreachable?".
    Match < 0.6 or risk ≥ 0.4 → the TUI asks the owner `[y/n]`. A denied action is not retried.
  - hard rule in code: `power` always asks the owner. Controller down → every mutating action asks the owner.
  - `/set controller off` disables the action check only; the memory rule cannot be switched off.
- Verified in the VM: preference note saved; "my colleague Marco loves Python" refused; reboot stopped for confirmation
  and dropped on "n"; `temperature 0.8` passed silently (match 0.94, risk 0.12).

##### Benchmark (cold, no fine-tuning) — `bench/judge.py`, `bench/rules.py`, raw data in `bench/results.json`
30 English notes (14 allowed, 16 forbidden, many disguised: injections, "Owner preference: Ferrari", Kubernetes tips,
wife/colleague, work), 4 Italian notes, 18 owner-request/action pairs. CPU, 4 threads.

| Model | Size | Memory gate (best rule) | Actions: needs-owner errors | Latency / question |
|---|---|---|---|---|
| Julia-1 (mmBERT-small) | 160 MB | 0.60, unusable | destructive 0.31 | ~0.06 s |
| Laya (EN, ModernBERT-large) | 428 MB | 0.77, 5+ false allows | 7/18 needless confirms | ~0.35 s |
| Kev-0.8B | 774 MB | 0.87, 0 false allows | 0 missed, 3/18 needless | ~0.5 s |
| **Kev-4B** (default) | **2.9 GB** | **0.97, 0 false allows** | **0 missed, 0 needless** | **~2 s batched, ~5 s alone** |
| lev (Qwen3.5-4B) | 2.9 GB | 0.80, 1 false allow | 1 missed (reboot) | ~3.7 s |
| OpenDecider-small | 2.4 GB | — | — | its GGUF has no decision head (`/v1/systemone` → 501) |
| Rizzo Flow 4B (Spark-X2.5 + LoRA, Q4_K_M) | 2.5 GB | 0.83, 1 false allow | **2 missed** (disable tui, ctx 2048), 1/18 needless | ~4.2 s |

Rizzo Flow has no decision head: `bench/judge.py` rebuilds its exact `spark-decisions-v3` prompt and reads the
answer-letter probabilities from `/completion`. Spark-X2.5 uses sliding-window attention, so llama-server needs
`--swa-full` to reuse the cached prompt prefix (otherwise ~10 s per question). Rejected: less safe than Kev-0.8B and slower than Kev-4B.
Dev box (i7-11370H, 4 cores) does only ~20 tok/s of prompt processing on any 4B: latency ≈ tokens read per decision.

Notes: with a bare yes/no question (no `criteria` descriptions) every model leans to "yes"; always give
`true`/`false` descriptions. Prefixing the note with "Note to store in Daimon memory" biased all models toward
"system". Every model under-rated "reboot" as destructive → hard rule. Thresholds were tuned on ~50 cases: re-run the
bench when adding cases or switching model.

#### ✅ Console UI on the framebuffer (2026-10-05)
- `aios/src/fb.rs`: a ratatui backend that paints `/dev/fb0` directly (kernel `CONFIG_FB_DEVICE=y`): 24-bit colour,
  anti-aliased glyphs, VT in `KD_GRAPHICS` so the kernel console does not draw over it. No usable framebuffer
  (or not 32 bpp) → falls back to the text console; the footer says why.
- Fonts: `tools/mkfont.py` renders DejaVu Sans Mono (regular + bold) at 4 sizes into `/usr/share/aios/fonts`
  (2.6 MB, 2501 glyphs: Latin, Greek, Cyrillic, arrows, symbols). Box drawing, blocks, braille, pills and meter bands
  are drawn procedurally so lines meet at cell edges; 18 Lucide-style vector icons rasterised into the Private Use Area.
  `ui_font` = auto (by resolution) or 17/19/24/30 px, applied live.
- Design: slate dark + green accent (ui-ux-pro-max "Developer Tool / IDE"), rounded panels, cards for tool calls.
- Every controller decision is a card in the flow: what was judged, each question with probability, bar and threshold,
  the acceptance score (action: p_match × (1 − p_risk); memory: allowed-topic mass, × (1 − worst veto) if vetoes were
  asked), the verdict and the latency. A "judging…" placeholder with a timer shows while Kev-4B works.
  Sidebar: controller model, state, last decision, allowed/stopped counts, average latency.

#### ✅ Keyboard layouts (2026-10-05)
- 59 layouts (us, us-intl, dvorak, colemak, gb, it, de, fr, bepo, es, pt, br, ch, nordics, pl, cz, hu, ro, tr, gr, ru,
  ua, jp, …) built from the host's XKB data by `tools/mkkeymaps.py` (ckbcomp) into `/usr/share/aios/keymaps`, with
  the dead-key table derived from Unicode composition. Loaded by `aios/src/keyboard.rs` with `KDSKBENT` /
  `KDSKBDIACRUC`: no loadkeys, no kbd package.
- `keymap` setting (`/set keymap it`, or ask the agent): applied at once and at every boot; a wrong name lists them all.
  `/keymaps` lists them with descriptions. Verified in QEMU: è é à ò ù ì £ € [ ] { @ on `it`.

#### ✅ Boot splash (2026-10-07)
- `aios/src/splash.rs`, run by the `tui` module on the framebuffer from the moment it starts: the mark (a ring drawn
  on, two counter-rotating comets, a breathing core with a glow), the **DAIMON** wordmark (URW Gothic, rendered at
  build time by `tools/mklogo.py` into `/usr/share/aios/logo.alf`, scaled at runtime) with a light sweep, version and
  codename, then the live boot steps: kernel, network, brain, controller, instructions (prompt warm-up), with an
  overall progress bar. Only the moving regions are redrawn (~30 fps, off-screen then copied).
- Leaves with a 0.6 s fade into the console once the brain has its instructions and the controller answers
  (at least 2.8 s, so a fast machine still shows the intro), or on any key. Runs once per boot
  (`/run/aios/splash-done`): a restarted TUI goes straight to the console. Text console: no splash.
- Kernel cmdline `vt.global_cursor_default=0`: no blinking cursor between firmware and splash.
- Verified in QEMU (1280x800): `docs/boot.gif`.

## Known limits
- Installer: UEFI only (no legacy BIOS), Secure Boot off, uses the whole disk (no dual boot). The ISO's ESP is a
  64 MB FAT image, mostly empty (room for A/B kernels later).
- The RAM rule for models is a rule of thumb from file sizes; a big context on a big model can still run out.
- Until 0.3.0, agent and TUI share one process: a console restart starts a new conversation; a pending confirmation blocks the agent until answered.
- Context tokens estimated as chars/3; the memory index is injected whole into the prompt.
- DHCP renewal = full DORA at half lease. Module logs are not rotated (tmpfs).
- The thinking budget applies per step; multi-step turns can still reason at length.
- Keyboard: at most 256 dead-key combinations per layout (kernel MAX_DIACR); compose sequences not loaded.
- The framebuffer UI is not redrawn after switching VT and back (nothing else runs on the other VTs).
- Sizes: installer ISO 67 MB; the OS 25 MB; default models 4.3 GB (MiniCPM5 2B 1.5 GB + Kev 4B 2.8 GB), downloaded
  at install. The dev image (`./build.sh`) is a 4.6 GB qcow2 with the models baked in. Brain + controller need
  ~6.5 GB RAM with the defaults: give the VM at least 8 GB.
- On a slow CPU the controller dominates latency (≈ tokens read per decision / prompt speed; dev box ~20 tok/s on a 4B).
