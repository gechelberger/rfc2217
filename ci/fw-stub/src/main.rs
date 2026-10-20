//! Drives a `Client` through the core API only, the way firmware would, so
//! that linking it without a `#[global_allocator]` proves the core needs no
//! heap.

#![no_std]
#![no_main]

use core::hint::black_box;
use core::panic::PanicInfo;

use rfc2217::{Client, PortSettings};

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let mut client = Client::new(PortSettings::default());
    let mut payload = [0u8; 16];
    // The bridge accepts COM-PORT-OPTION, then sends a byte of data.
    let input = black_box([0xFF, 0xFD, 0x2C, b'x']);
    let mut at = 0;
    while at < input.len() {
        let p = client.receive_into(&input[at..], &mut payload);
        at += p.consumed;
        black_box(&payload[..p.payload_len]);
        while let Some(event) = client.poll_event() {
            black_box(event);
        }
        // A socket that takes 8 bytes per write.
        while !client.output().is_empty() {
            let n = client.output().len().min(8);
            black_box(&client.output()[..n]);
            client.consume_output(n);
        }
    }
    let _ = black_box(client.try_set_dtr(true));
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
