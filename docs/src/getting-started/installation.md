# Installation

## Requirements

- **Rust nightly toolchain (1.85+)** with the `rust-src` component. Gluon
  compiles custom sysroots (`core`, `alloc`, `compiler_builtins`) from source,
  so `rust-src` is required.
- **Optional Rust components:** `clippy` (for `gluon clippy`), `rustfmt` (for
  `gluon fmt`), and `rust-analyzer` (for IDE integration via
  `gluon configure`).
- **QEMU** -- only needed if you want to use `gluon run`. Not required for
  building alone.
- **OVMF firmware** -- only needed for UEFI boot. If you are targeting BIOS or
  a non-UEFI boot protocol, you can skip this.

## Install Gluon

Clone the repository and install the CLI:

```sh
git clone https://github.com/hxyulin/gluon.git
cd gluon
cargo install --path crates/gluon-cli
```

The repository includes a `rust-toolchain.toml` that pins the required nightly
toolchain. Cargo will automatically download and use it when you run
`cargo install` inside the repo.

Verify the installation:

```sh
gluon --version
```

## Adding rust-src

If your toolchain does not already include `rust-src`, add it manually:

```sh
rustup component add rust-src
```

You can confirm it is present with:

```sh
rustup component list --installed | grep rust-src
```

## Optional: QEMU and OVMF

Install QEMU through your system package manager:

```sh
# macOS
brew install qemu

# Ubuntu / Debian
sudo apt install qemu-system-x86 qemu-system-aarch64

# Arch Linux
sudo pacman -S qemu-full
```

For UEFI boot, you will also need OVMF firmware. On most Linux distributions
it is available as the `ovmf` or `edk2-ovmf` package. On macOS, OVMF can be
obtained via Homebrew or built from source.
