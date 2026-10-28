//! Fuzzes the reader of a `commit2` (see `stemma_core::fuzz::commit2`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::commit2(data));
