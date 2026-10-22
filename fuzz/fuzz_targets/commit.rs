//! Fuzzes the reader of a commit (see `stemma_core::fuzz::commit`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::commit(data));
