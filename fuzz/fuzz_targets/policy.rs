//! Fuzzes the reader of a policy file (see `stemma_core::fuzz::policy`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::policy(data));
