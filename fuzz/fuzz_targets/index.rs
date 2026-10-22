//! Fuzzes the reader of an index (see `stemma_core::fuzz::index`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::index(data));
