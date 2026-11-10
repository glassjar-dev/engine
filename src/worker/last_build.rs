//! A worker remembers its last successful C++ build and exactly what it was built from.
//! Pressing Run again with nothing changed reuses that program instead of compiling and
//! linking it again. Inputs are compared in full, byte for byte (sources, binary inputs,
//! the build's flags and the compiler), so any change at all builds as usual.

use crate::types::FsNode;
use serde_bytes::ByteBuf;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(PartialEq)]
pub struct BuildInputs {
    pub fs: HashMap<String, FsNode>,
    pub binary_files: HashMap<String, ByteBuf>,
    pub sources: Vec<String>,
    pub archives: Option<Vec<String>>,
    pub precompiled_header: Option<String>,
    pub is_debug: bool,
    pub toolchain: (String, String, String),
}

thread_local! {
    static LAST: RefCell<Option<(BuildInputs, Rc<Vec<u8>>)>> = const { RefCell::new(None) };
}

/// The linked program of the last build, if it was built from exactly these inputs.
pub fn lookup(inputs: &BuildInputs) -> Option<Rc<Vec<u8>>> {
    LAST.with(|last| last.borrow().as_ref().filter(|(built, _)| built == inputs).map(|(_, wasm)| wasm.clone()))
}

/// Replaces the remembered build. Only successful links are remembered.
pub fn remember(inputs: BuildInputs, wasm: Vec<u8>) {
    LAST.with(|last| *last.borrow_mut() = Some((inputs, Rc::new(wasm))));
}
