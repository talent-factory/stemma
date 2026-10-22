//! Fuzzes the reader of the journal of a purge (see `stemma_core::fuzz::journal`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::journal(data));
