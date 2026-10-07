# Daimon

**An operating system where the language model *is* the system.** 0.1.0 "Deucalion".

![boot](docs/boot.gif)

Daimon is a minimal x86_64 Linux: the kernel boots straight into one Rust binary (`aios`) that is init, module
supervisor, agent and console. A local model (llama.cpp) runs the machine through tools; a second, smaller decision
model (the *controller*) judges every action that changes the system and everything the agent wants to remember.
No shell, no package manager, no systemd: the whole OS is a single 24 MB EFI file.

- **The brain proposes, the controller judges, the code decides.** Non-negotiable rules live in code, never in a prompt.
- **Modular.** Every component is a supervised, restartable module; none is a hard dependency.
- **Local.** OpenAI-compatible API on the LAN, framebuffer console on the screen, nothing leaves the machine.

![console](docs/console.png)

## Build and run

Needs a Linux host with Rust (musl target), the kernel and llama.cpp sources under `build/`, python3-pil, ckbcomp
and QEMU + OVMF.

```sh
./build.sh        # -> out/daimon.qcow2 (any UEFI VM, e.g. Proxmox with --cpu host, --bios ovmf, Secure Boot off)
./build.sh run    # build, then boot it in QEMU (API on host :8080)
```

Give the VM at least 8 GB of RAM (brain + controller).

## Releases

Semver; the codename changes only with the major version: 0.x **Deucalion**, 1.x Talos, 2.x Galatea.

## Status

[PLAN.md](PLAN.md) is the living plan: what works, the benchmarks behind the controller, next steps, known limits.

## License

GPL-3.0-or-later.
