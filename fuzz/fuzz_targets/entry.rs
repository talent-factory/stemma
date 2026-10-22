//! Fuzzes the reader of an entry of the operation log, in either encoding (see
//! `stemma_core::fuzz::entry`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::entry(data));
