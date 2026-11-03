pub mod dap;
pub mod debug;
pub mod python;
pub mod types;
pub mod worker;

mod util;

use wasm_bindgen::prelude::*;

/// Runtime asset URLs (binary, then sysroot) for a language, so the host can warm
/// them into cache before running. Single source of truth for these URLs.
#[wasm_bindgen]
pub fn prefetch_urls(lang: &str) -> Vec<String> {
    match lang {
        "python" => vec![
            python::worker::WASM_URL.to_string(),
            python::worker::STDLIB_URL.to_string(),
        ],
        "c" => vec![
            worker::CPP_WASM_URL.to_string(),
            worker::CPP_STDLIB_URL.to_string(),
        ],
        _ => Vec::new(),
    }
}

/// Identity of the embedded runtime, verified by the parent against its build manifest.
#[wasm_bindgen]
pub fn cpp_exception_hash() -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(include_bytes!("../assets/cpp-exceptions/runtime.tar.gz")))
}
