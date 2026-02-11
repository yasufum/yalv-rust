# yalv-rust

Yet Another LibVirt  client — a terminal UI for browsing and connecting to libvirt/KVM virtual machines.

## Prerequisites

- [libvirt](https://libvirt.org/) with `virsh` available on your `PATH`
- Rust 2024 edition (1.85+)

## Build

```sh
cargo build --release
```

## Install

Install from this repository with Cargo:

```sh
cargo install --path .
```

This installs `yalv-rust` to `~/.cargo/bin`.
Make sure that directory is included in your `PATH`.

## Usage

```
yalv-rust [OPTIONS]
```

### Options

| Option       | Description                          |
|--------------|--------------------------------------|
| `--all`      | Show all VMs (including inactive)    |
| `-h, --help` | Show help message and exit           |

By default, only running VMs are listed (same as `virsh list`).
Use `--all` to include inactive VMs (same as `virsh list --all`).

### Keybindings

| Key          | Action                            |
|--------------|-----------------------------------|
| `j` / `Down` | Move selection down               |
| `k` / `Up`   | Move selection up                 |
| `Enter`      | Open console (running VMs only)   |
| `s`          | SSH into VM (running VMs only)    |
| `u`          | Start VM (shut off VMs only)      |
| `d`          | Shut down VM (running VMs only)   |
| `A`          | Toggle between all / running VMs  |
| `q` / `Esc`  | Quit                              |


## Note

This tool is generated with Claude Code.

## License

TBD
