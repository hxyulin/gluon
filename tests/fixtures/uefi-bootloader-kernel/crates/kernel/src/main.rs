// Minimal bare-metal x86_64 kernel. This is the artifact the
// bootloader `include_bytes!`es; it never actually runs in this
// fixture (the bootloader halts before handing off).
//
// The fixture's job is end-to-end build plumbing, not OS development.
// Keeping the kernel to a stable non-empty ELF is all we need.

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

// A small static buffer so the output has *some* rodata for the
// include_bytes consumer to see. Marked `used` so the linker doesn't
// drop it even under aggressive dead-code removal.
#[used]
#[no_mangle]
pub static KERNEL_MAGIC: [u8; 8] = *b"GLUONKER";

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Volatile read of KERNEL_MAGIC so the compiler can't elide the
    // reference even with LTO. Keeps the bytes observable inside the
    // linked ELF.
    unsafe {
        core::ptr::read_volatile(KERNEL_MAGIC.as_ptr());
    }
    loop {}
}
