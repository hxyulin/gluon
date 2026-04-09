// Minimal UEFI bootloader for the uefi-bootloader-kernel fixture.
//
// This is a raw UEFI entry (no `uefi` crate) targeting
// `x86_64-unknown-uefi`. rustc's builtin spec for that target already
// sets the entry symbol to `efi_main`, so `#[no_mangle] pub extern
// "efiapi" fn efi_main(...)` is the canonical entry point — no custom
// linker entry required.
//
// Runtime behaviour:
//   1. Verify that the embedded kernel slice is non-empty.
//   2. Write an observability byte (`'K'` on success, `'!'` on failure)
//      to QEMU's debugcon port (0xe9), which the integration test
//      captures via `-debugcon file:<path>`.
//   3. Halt in an `hlt` loop. No kernel handoff — the fixture's
//      purpose is to prove the build chain works, not to be an OS.
//
// The embedded kernel path comes from `env!("KERNEL_PATH")`, which is
// injected by the Gluon build system's `artifact_env` mechanism: the
// bootloader crate declares `.artifact_env("KERNEL_PATH", "kernel")`
// in gluon.rhai, and gluon-core sets `KERNEL_PATH=<abs path>` in the
// rustc process environment after compiling the kernel.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// Bytes of the compiled kernel ELF. Resolved at compile time by
/// `env!`, baked into the .efi's .rodata by `include_bytes!`.
static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    halt()
}

/// UEFI entry point. The `efiapi` calling convention is stable since
/// Rust 1.68 and is what OVMF calls into.
///
/// The two parameters are `EFI_HANDLE` and `EFI_SYSTEM_TABLE*`, which
/// we treat as opaque — we don't actually use the UEFI boot services
/// here. If we did, we'd want the real type definitions.
#[no_mangle]
pub extern "efiapi" fn efi_main(
    _image_handle: *mut core::ffi::c_void,
    _system_table: *mut core::ffi::c_void,
) -> usize {
    // SAFETY: port 0xe9 is QEMU's debugcon, which is always safe to
    // write to on x86_64. The value is a single byte and we touch no
    // shared state.
    unsafe {
        if KERNEL.is_empty() {
            debugcon(b'!');
        } else {
            debugcon(b'K');
        }
        // Second byte so tests can assert on a 2-byte sequence — adds
        // a little protection against partial-write false positives.
        debugcon(b'\n');
    }
    halt()
}

/// Write one byte to QEMU's debug console I/O port (0xe9). Visible in
/// the integration test via `-debugcon file:<path>`.
#[inline(always)]
unsafe fn debugcon(b: u8) {
    core::arch::asm!(
        "out dx, al",
        in("dx") 0xe9u16,
        in("al") b,
        options(nomem, nostack, preserves_flags),
    );
}

/// Spin in an `hlt` loop. Under QEMU this wedges the guest CPU
/// indefinitely, which is exactly what we want — the test has a
/// wall-clock timeout that will tear it down.
fn halt() -> ! {
    loop {
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
