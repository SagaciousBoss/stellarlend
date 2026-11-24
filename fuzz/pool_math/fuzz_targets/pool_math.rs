#![no_main]

use libfuzzer_sys::fuzz_target;
use stellarlend_pool_math_fuzz::run;

fuzz_target!(|data: &[u8]| {
    // `run` asserts invariants with normal Rust panics, so a violation is
    // reported to libFuzzer as a crash with the invariant message.
    if let Err(violation) = run(data) {
        panic!("pool math invariant violated: {violation}");
    }
});
