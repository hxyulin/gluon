#![no_std]
#![no_main]

use minimal_derive::Hello;

#[derive(Hello)]
struct Marker;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let _ = HELLO;
    loop {}
}
