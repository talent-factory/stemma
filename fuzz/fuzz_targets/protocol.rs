//! Fuzzes the readers of the protocol messages: the list of references, the fetch request, the
//! error, and the object stream (see `stemma_core::fuzz::protocol`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::protocol(data));
