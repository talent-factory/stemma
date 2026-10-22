//! Fuzzes the reader of an actor line (see `stemma_core::fuzz::actor`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::actor(data));
