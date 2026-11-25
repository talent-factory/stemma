//! Fuzzes the readers of the protocol messages: the list of references, the fetch request, the
//! error, the object stream, the ref update, the record entry, and the three messages of the
//! sign-in (see `stemma_core::fuzz::protocol`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| stemma_core::fuzz::protocol(data));
