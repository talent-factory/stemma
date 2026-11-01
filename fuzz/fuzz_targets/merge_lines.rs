//! Fuzzes the merge of the lines of a file (see `stemma_core::fuzz::merge_lines`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::merge_lines(data));
