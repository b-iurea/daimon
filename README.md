# Daimon

**An operating system where the language model *is* the system.** 0.2.0 "Deucalion".

![boot](docs/boot.gif)

Daimon is a minimal x86_64 Linux: the kernel boots straight into one Rust binary (`aios`) that is init, module
supervisor, agent and console. A local model (llama.cpp) runs the machine through tools; a second, smaller decision
model (the *controller*) judges every action that changes the system and everything the agent wants to remember.
No shell, no package manager, no systemd: the whole OS is a single 25 MB EFI file.

- **The brain proposes, the controller judges, the code decides.** Non-negotiable rules live in code, never in a prompt.
- **Modular.** Every component is a supervised, restartable module; none is a hard dependency.
- **Local.** OpenAI-compatible API on the LAN, framebuffer console on the screen, nothing leaves the machine.

![console](docs/console.png)

## Install

Download `daimon-<version>.iso` from the releases (67 MB) and boot it on a UEFI machine or VM (Secure Boot off;
on Proxmox: `--cpu host`, `--bios ovmf`), as a CD or written to a USB stick with `dd`. The installer asks for the
keyboard, the disk (erased entirely), your name, the machine name, the brain and the controller, then downloads the
models from Hugging Face (models are offered by the machine's RAM; MiniCPM5 2B + Kev 4B by default, ~4.6 GB).

## Build and run

Needs a Linux host with Rust (musl target), the kernel and llama.cpp sources under `build/`, python3-pil, ckbcomp,
mtools and QEMU + OVMF; `build.sh` fetches e2fsprogs and xorriso itself.

```sh
./build.sh          # dev image out/daimon.qcow2, models from build/data already installed
./build.sh run      # build, then boot it in QEMU (API on host :8080)
./build.sh iso      # out/daimon-<version>.iso, the installer
./build.sh run-iso  # build the ISO and install it in QEMU onto a blank disk
```

Give the VM at least 8 GB of RAM (brain + controller).

## Releases

Semver; the codename changes only with the major version: 0.x **Deucalion**, 1.x Talos, 2.x Galatea.

## Status

[PLAN.md](PLAN.md) is the living plan: what works, the benchmarks behind the controller, next steps, known limits.

## License

GPL-3.0-or-later.
