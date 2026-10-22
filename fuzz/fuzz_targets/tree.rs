//! Fuzzes the reader of a tree (see `stemma_core::fuzz::tree`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::tree(data));
