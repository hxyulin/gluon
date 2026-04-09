#![no_std]
#![no_main]

use minimal_derive::Hello;

#[derive(Hello)]
struct Marker;

// PVH ELF note for QEMU >= 7.2 direct boot is generated entirely in the
// linker script (kernel.ld) to avoid R_X86_64_32 relocation issues in PIE
// mode. See the .note.Xen section there.

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let _ = HELLO;
    loop {}
}
