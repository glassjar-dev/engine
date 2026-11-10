use console_error_panic_hook;
use instant::Instant;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasmer_wasix::virtual_fs::{AsyncWriteExt, FileSystem, create_dir_all, mem_fs};
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

use crate::types::{CppArtifacts, CppCacheBypass, CppCacheEntry, CppCacheRequest, CppCacheStats, FsNode, Lang, WorkerOut, WorkerStart};

mod debuggee;
mod cpp_cache;
mod cpp_cache_inputs;
mod cpp_stock_policy;
mod cpp_stock_trace;
mod cpp_stock_trace_host;
mod cpp_stock_job;
pub(crate) mod execution;
pub(crate) mod host_device;
mod io;
mod read_only_fs;
mod runtime;

use execution::{Execution, extract_tar_gz};
use host_device::{HOST_DEVICE_PATH, HostDeviceFile};

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ Helpers                                                                  │
// ╰──────────────────────────────────────────────────────────────────────────╯

pub(crate) async fn create_user_fs(node: FsNode) -> Result<mem_fs::FileSystem, std::io::Error> {
    let fs = mem_fs::FileSystem::default();
    create_user_fs_rec(&fs, &PathBuf::from("/"), &node).await?;
    Ok(fs)
}

async fn create_user_fs_rec(
    fs: &mem_fs::FileSystem,
    base_path: &PathBuf,
    node: &FsNode,
) -> Result<(), std::io::Error> {
    match node {
        FsNode::File(contents) => {
            let mut file = fs
                .new_open_options()
                .create(true)
                .write(true)
                .open(base_path)?;
            file.write_all(contents.as_bytes())
                .await
                .expect("Failed to write injected file");
            file.flush().await.expect("Flushed file")
        }
        FsNode::Dir(children) => {
            create_dir_all(fs, base_path)?;
            for (name, child_node) in children {
                let mut child_path = base_path.clone();
                child_path.push(name);
                Box::pin(create_user_fs_rec(fs, &child_path, child_node)).await?;
            }
        }
    }
    Ok(())
}

fn is_source_ext(ext: &str, lang: Lang) -> bool {
    match lang {
        Lang::C => matches!(
            ext.to_ascii_lowercase().as_str(),
            "c" | "cc" | "cp" | "cpp" | "cxx" | "c++"
        ),
        Lang::Rust => ext.eq_ignore_ascii_case("rs"),
        Lang::Python => false,
    }
}

fn collect_sources(node: &FsNode, base_path: &PathBuf, lang: Lang, sources: &mut Vec<String>) {
    match node {
        FsNode::File(_) => {
            if let Some(ext) = base_path.extension().and_then(|ext| ext.to_str()) {
                if is_source_ext(ext, lang) {
                    sources.push(base_path.to_string_lossy().to_string());
                }
            }
        }
        FsNode::Dir(children) => {
            collect_dir_sources(children, base_path, lang, sources);
        }
    }
}

fn collect_dir_sources(
    children: &std::collections::HashMap<String, FsNode>,
    base_path: &PathBuf,
    lang: Lang,
    sources: &mut Vec<String>,
) {
    for (name, child_node) in children {
        let mut child_path = base_path.clone();
        child_path.push(name);
        collect_sources(child_node, &child_path, lang, sources);
    }
}

fn rust_obj_path(source: &str) -> String {
    format!("{}.o", source.trim_end_matches(".rs"))
}

fn collect_rust_objs(fs: &mem_fs::FileSystem, source: &str) -> Vec<String> {
    let obj_path = rust_obj_path(source);
    let mut objs = vec![obj_path.clone()];

    let path = Path::new(source);
    let dir = path.parent().unwrap_or(Path::new("/"));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    let Ok(mut entries) = fs.read_dir(dir) else {
        return objs;
    };
    for entry in entries.by_ref().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(stem) && name.ends_with(".rcgu.o") {
            objs.push(format!("{}/{}", dir.to_string_lossy(), name));
        }
    }
    objs
}

fn find_rust_rlibs(fs: &mem_fs::FileSystem) -> Vec<String> {
    let dir = Path::new("/lib/rustlib/wasm32-wasip1/lib");
    let mut entries = fs.read_dir(dir).expect("Rust sysroot rlib directory");
    let names: Vec<String> = entries
        .by_ref()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    [
        "libpanic_abort-",
        "libstd-",
        "libwasi-",
        "libcfg_if-",
        "librustc_demangle-",
        "libstd_detect-",
        "libhashbrown-",
        "librustc_std_workspace_alloc-",
        "libminiz_oxide-",
        "libadler2-",
        "libunwind-",
        "liblibc-",
        "librustc_std_workspace_core-",
        "liballoc-",
        "libcore-",
        "libcompiler_builtins-",
    ]
    .iter()
    .map(|prefix| {
        names
            .iter()
            .find(|name| name.starts_with(prefix) && name.ends_with(".rlib"))
            .map(|name| format!("/lib/rustlib/wasm32-wasip1/lib/{name}"))
            .unwrap_or_else(|| panic!("missing rlib for prefix {prefix}"))
    })
    .collect()
}

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ Worker                                                                   │
// ╰──────────────────────────────────────────────────────────────────────────╯

// Single source of truth for these is `prefetch_urls` in lib.rs (warmed by the host).
pub(crate) const CPP_WASM_URL: &str = "https://cdn.yousefabuhashem.com/clang/2026-11-10-slim/llvm.core.wasm";
pub(crate) const CPP_STDLIB_URL: &str = "https://cdn.yousefabuhashem.com/clang/2026-05-22/llvm-resources.tar.gz";
pub(crate) const RUST_WASM_URL: &str = "https://cdn.yousefabuhashem.com/rust/2026-05-31/rustc.wasm";
pub(crate) const RUST_SYSROOT_URL: &str = "https://cdn.yousefabuhashem.com/rust/2026-05-31/rust-sysroot.tar.gz";

/// Builds a `Stop` with build/run timing. `run_start` is `None` when execution never
/// began (e.g. a compile error), in which case the elapsed time is all build.
pub(crate) fn stop(exit_code: i32, build_start: Instant, run_start: Option<Instant>) -> WorkerOut<'static> {
    let ms = |d: instant::Duration| d.as_secs_f64() * 1000.0;
    let (build_ms, run_ms) = match run_start {
        Some(run) => (ms(run - build_start), ms(run.elapsed())),
        None => (ms(build_start.elapsed()), 0.0),
    };
    WorkerOut::Stop { exit_code, build_ms, run_ms }
}

async fn start(msg: WorkerStart) {
    crate::types::set_operation_identity(msg.scope_epoch, msg.operation_id, msg.cache_epoch);
    if msg.prepare_compiler {
        let started = Instant::now();
        if msg.lang != Lang::C { return WorkerOut::Error { message: "Compiler preparation requires C/C++".into() }.send(); }
        if let Err(error) = execution::prepare_compiler().await {
            return WorkerOut::Error { message: format!("Compiler preparation failed: {error:?}") }.send();
        }
        cpp_phase("toolchain_ready", 0, 0);
        if !msg.validate_cache_inputs { return stop(0, started, None).send(); }
        if !execution::compiler_toolchain().is_some_and(|t| t.stock_profile) {
            WorkerOut::CppCacheEligibility { eligible: false, certificate_sha256: None, snapshot: None, reason: Some("stock-profile-required".into()) }.send();
            return stop(0, started, None).send();
        }
        return start_native(msg).await;
    }
    if msg.build_only && msg.lang != Lang::C {
        return WorkerOut::Error { message: "Compile-only preparation requires C/C++".into() }.send();
    }
    if msg.lang != Lang::C && (msg.cpp_artifacts.is_some() || !msg.binary_files.is_empty()) {
        return WorkerOut::Error { message: "Binary build inputs require C/C++".into() }.send();
    }
    if msg.host_device.is_some() && !matches!(msg.lang, Lang::C | Lang::Python) {
        return WorkerOut::Error {
            message: "Host devices are supported only for C/C++ and Python execution".into(),
        }
        .send();
    }
    match msg.lang {
        Lang::Python => crate::python::worker::start(msg).await,
        Lang::C | Lang::Rust => start_native(msg).await,
    }
}

async fn start_native(msg: WorkerStart) {
    let build_start = Instant::now();
    let mut sources = Vec::new();
    collect_dir_sources(&msg.fs, &PathBuf::from("/"), msg.lang, &mut sources);
    let mut artifacts = msg.cpp_artifacts.unwrap_or_default();
    if let Some(selected) = artifacts.sources.take() {
        if selected.is_empty() || selected.iter().any(|path| !sources.contains(path)) {
            return WorkerOut::Error { message: "C++ sources must select existing source files".into() }.send();
        }
        sources = selected;
    } else { sources.sort(); }
    // A Rust crate builds from its root and rustc loads `mod` files itself. Compiling each
    // .rs file on its own would make every module a separate crate with no `main`.
    if msg.lang == Lang::Rust {
        let root = "/main.rs".to_string();
        if !sources.contains(&root) {
            return WorkerOut::Error { message: "Rust needs a main.rs crate root".into() }.send();
        }
        sources = vec![root];
    }
    for path in artifacts.archives.iter().flatten().chain(artifacts.precompiled_header.iter()) {
        if !msg.binary_files.contains_key(path) {
            return WorkerOut::Error { message: format!("Missing binary build input: {path}") }.send();
        }
    }
    let hash_start = Instant::now();
    let stock_profile = execution::compiler_toolchain().is_some_and(|toolchain| toolchain.stock_profile);
    let cache_inputs = msg.cpp_cache.as_ref().filter(|_| !stock_profile).map(|_| legacy_user_inputs(&msg.fs, &msg.binary_files, &sources));
    let stock_inputs = (stock_profile && (msg.cpp_cache.is_some() || msg.validate_cache_inputs)).then(|| stock_user_inputs(&msg.fs, &msg.binary_files));
    let input_hash_ms = hash_start.elapsed().as_secs_f64() * 1000.0;

    let lang_name = match msg.lang {
        Lang::C => "C/C++",
        Lang::Rust => "Rust",
        Lang::Python => unreachable!(),
    };
    assert!(
        !sources.is_empty(),
        "No {lang_name} source files found in provided filesystem"
    );

    let fs = create_user_fs(FsNode::Dir(msg.fs))
        .await
        .expect("created user files filesystem");

    for (path, bytes) in msg.binary_files {
        if !path.starts_with('/') || path.split('/').skip(1).any(|part| matches!(part, "" | "." | "..")) {
            return WorkerOut::Error { message: format!("Invalid binary input path: {path}") }.send();
        }
        let written = async {
            create_dir_all(&fs, Path::new(&path).parent().unwrap())?;
            let mut file = fs.new_open_options().write(true).create_new(true).open(&path)?;
            file.write_all(&bytes).await
        }.await;
        if let Err(error) = written {
            return WorkerOut::Error { message: format!("Cannot mount binary input {path}: {error}") }.send();
        }
    }

    let exec = Execution::new(msg.stdin_buffer);
    let is_debug = msg.is_debug;

    match msg.lang {
        Lang::C => {
            let host_device = match msg.host_device.filter(|_| !msg.build_only).map(HostDeviceFile::new).transpose() {
                Ok(device) => device,
                Err(error) => {
                    return WorkerOut::Error {
                        message: format!("Invalid host device: {error}"),
                    }
                    .send();
                }
            };
            let toolchain = execution::compiler_toolchain().expect("verified toolchain");
            start_cpp(is_debug, msg.build_only, msg.prepare_compiler, build_start, sources, exec, fs, host_device, artifacts, msg.cpp_cache, cache_inputs, stock_inputs, toolchain, input_hash_ms).await;
        }
        Lang::Rust => start_rust(is_debug, build_start, sources, exec, fs).await,
        Lang::Python => unreachable!(),
    }
}

fn legacy_user_inputs(
    fs: &std::collections::HashMap<String, FsNode>,
    binaries: &std::collections::HashMap<String, serde_bytes::ByteBuf>,
    sources: &[String],
) -> cpp_cache::Inputs {
    fn walk<'a>(node: &'a FsNode, path: String, files: &mut Vec<(String, &'a [u8], bool)>, paths: &mut Vec<String>) {
        paths.push(path.clone());
        match node {
            FsNode::File(text) => files.push((path, text.as_bytes(), true)),
            FsNode::Dir(children) => for (name, child) in children { walk(child, format!("{path}/{name}"), files, paths); },
        }
    }
    let mut files = Vec::new();
    let mut paths = Vec::new();
    for (name, node) in fs { walk(node, format!("/{name}"), &mut files, &mut paths); }
    for (path, bytes) in binaries {
        paths.push(path.clone());
        paths.extend(Path::new(path).ancestors().skip(1).filter(|path| *path != Path::new("/")).map(|path| path.to_string_lossy().into_owned()));
        files.push((path.clone(), bytes.as_ref(), false));
    }
    cpp_cache::Inputs::new(files, paths, sources)
}

fn stock_user_inputs(
    fs: &std::collections::HashMap<String, FsNode>,
    binaries: &std::collections::HashMap<String, serde_bytes::ByteBuf>,
) -> Result<cpp_cache_inputs::UserInputs, cpp_cache_inputs::Bypass> {
    fn walk<'a>(node: &'a FsNode, path: String, files: &mut Vec<(String, &'a [u8])>) {
        match node {
            FsNode::File(text) => files.push((path, text.as_bytes())),
            FsNode::Dir(children) => for (name, child) in children { walk(child, format!("{path}/{name}"), files); },
        }
    }
    let mut files = Vec::new();
    for (name, node) in fs { walk(node, format!("/{name}"), &mut files); }
    for (path, bytes) in binaries { files.push((path.clone(), bytes.as_ref())); }
    cpp_cache_inputs::UserInputs::new(files)
}

/// Enumerate the exact mounted namespace before generating any output. Every
/// toolchain member is bound by the verified archives; user overlays have their
/// own precomputed byte digests. Unknown filesystem kinds make caching optional.
fn stock_namespace(fs: &mem_fs::FileSystem) -> Result<(Vec<String>, Vec<String>), cpp_cache_inputs::Bypass> {
    let mut directories = vec!["/".to_string()];
    let mut files = Vec::new();
    let mut pending = vec!["/".to_string()];
    while let Some(directory) = pending.pop() {
        for entry in fs.read_dir(Path::new(&directory)).map_err(|_| cpp_cache_inputs::Bypass::DependencyClosure)? {
            let entry = entry.map_err(|_| cpp_cache_inputs::Bypass::DependencyClosure)?;
            let path = entry.path().to_string_lossy().into_owned();
            if path.len() > cpp_cache::MAX_SOURCE_PATH { return Err(cpp_cache_inputs::Bypass::UnsupportedPath); }
            let kind = entry.file_type().map_err(|_| cpp_cache_inputs::Bypass::DependencyClosure)?;
            if kind.is_dir() { directories.push(path.clone()); pending.push(path); }
            else if kind.is_file() { files.push(path); }
            else { return Err(cpp_cache_inputs::Bypass::UnsupportedPath); }
        }
    }
    files.sort(); files.dedup(); directories.sort(); directories.dedup();
    Ok((files, directories))
}

struct CppCacheJob {
    generation: u32,
    entries: std::collections::HashMap<String, CppCacheEntry>,
    pending_count: usize,
    pending_bytes: usize,
    metadata_limit: usize,
    stats: CppCacheStats,
}

impl CppCacheJob {
    fn bypass(&mut self, source: &str, reason: &str) {
        if self.stats.bypasses.len() < cpp_cache::MAX_REPORTED_SOURCES {
            self.stats.bypasses.push(CppCacheBypass { source: source.chars().take(cpp_cache::MAX_SOURCE_PATH).collect(), reason: reason.into() });
        }
    }

    fn send(self) {
        WorkerOut::CppCacheStats { generation: self.generation, stats: self.stats }.send();
    }

}

fn cpp_clang_args(source: &str, is_debug: bool, artifacts: &CppArtifacts, depfile: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "-cc1", "-triple", "wasm32-wasip1", "-Werror", "-emit-obj", "-disable-free", "-isysroot", "/",
        "-internal-isystem", "/include/c++/v1", "-internal-isystem", "/include", "-internal-isystem", "/include/wasm32-wasip1",
        "-ferror-limit", "4", "-fcolor-diagnostics", "-x", "c++", "-std=c++23", "-fexceptions", "-fcxx-exceptions", "-exception-model=wasm",
        "-target-feature", "+exception-handling", "-mllvm", "-wasm-enable-eh", "-mllvm", "-wasm-use-legacy-eh=true",
        // Quoted includes also resolve from the workspace root, so `lib/list.cpp` can include
        // "vector.h" or "lib/list.h" the same way `main.cpp` does.
        "-iquote", "/", "-o",
    ].into_iter().map(String::from).collect();
    args.push(format!("{source}.o"));
    if is_debug { args.extend(["-O0", "-debug-info-kind=standalone", "-dwarf-version=5"].into_iter().map(String::from)); }
    if let Some(pch) = &artifacts.precompiled_header { args.extend(["-include-pch".into(), pch.clone(), "-fvalidate-ast-input-files-content".into()]); }
    if depfile { args.extend(["-dependency-file".into(), format!("{source}.o.d"), "-MT".into(), format!("{source}.o"), "-sys-header-deps".into()]); }
    args.push(source.into());
    args
}

fn cpp_link_args(sources: &[String], artifacts: &CppArtifacts) -> Vec<String> {
    let mut args: Vec<String> = ["--export-dynamic", "-z", "stack-size=1048576", "-L/lib/wasm32-wasip1", "/lib/wasm32-wasip1/crt1.o"].into_iter().map(String::from).collect();
    args.extend(sources.iter().map(|source| format!("{source}.o")));
    args.extend(artifacts.archives.iter().flatten().cloned());
    args.extend([
        "/lib/wasm32-wasip1/cxa_exception.cpp.o", "/lib/wasm32-wasip1/cxa_personality.cpp.o", "-lc++", "-lc++abi", "-lunwind",
        "/lib/wasm32-unknown-wasip1/libclang_rt.builtins.a", "-lc", "-o", "/main.wasm",
    ].into_iter().map(String::from));
    args
}

async fn compile_stock_source(
    exec: &Execution, cache: &mut Option<CppCacheJob>, mut job: Option<&mut Result<cpp_stock_job::StockJob, String>>,
    source: &str, source_index: usize, source_count: usize, is_debug: bool, artifacts: &CppArtifacts,
) -> Option<i32> {
    let started = Instant::now();
    let arguments = cpp_clang_args(source, is_debug, artifacts, false);
    let metadata_limit = cache.as_ref().map_or(0, |c| c.metadata_limit);
    let eligibility = if is_debug { Err("debug-profile".to_string()) }
        else if metadata_limit == 0 { Err("stock-metadata-budget".to_string()) }
        else { match job.as_deref_mut() {
            Some(Ok(job)) => job.preflight_source(exec, source).await,
            Some(Err(reason)) => Err(reason.clone()),
            None => Err("stock-cache-disabled".into()),
        }};
    if let Some(cache) = cache { cache.stats.hash_ms += started.elapsed().as_secs_f64() * 1000.0; }
    let object_path = format!("{source}.o");
    let lookup_started = Instant::now();
    let mut hit = false;
    if let Some(cache) = cache {
        match &eligibility {
            Err(reason) => cache.bypass(source, reason),
            Ok(()) => {
                if let Some(entry) = cache.entries.remove(source) {
                    let key = entry.receipt.as_ref().ok_or("stock-metadata-missing".to_string()).and_then(|record| {
                        job.as_deref().and_then(|j| j.as_ref().ok()).ok_or("stock-cache-disabled".to_string())?
                            .lookup_key(record, source, &arguments, metadata_limit)
                    });
                    if key.as_ref().is_ok_and(|key| *key == entry.key) {
                        if entry.object.len() <= 128 * 1024 * 1024 && cpp_cache::valid_object(&entry.object, &entry.object_hash) {
                            hit = exec.write_bytes(&object_path, &entry.object).await.is_ok();
                        }
                        if !hit && cache.stats.invalid_sources.len() < cpp_cache::MAX_REPORTED_SOURCES { cache.stats.invalid_sources.push(source.into()); }
                    } else if key.is_err() && cache.stats.invalid_sources.len() < cpp_cache::MAX_REPORTED_SOURCES { cache.stats.invalid_sources.push(source.into()); }
                }
                if hit { cache.stats.hits += 1; if cache.stats.hit_sources.len() < cpp_cache::MAX_REPORTED_SOURCES { cache.stats.hit_sources.push(source.into()); } }
                else { cache.stats.misses += 1; }
            }
        }
        cache.stats.lookup_ms += lookup_started.elapsed().as_secs_f64() * 1000.0;
    }
    if hit { cpp_phase("cache_hit", source_index, source_count); return None; }
    // Observe even a lexically ineligible miss when the registered job exists:
    // an unexpected mutating syscall must disable later lookups in this build.
    let trace = if is_debug { None } else { job.as_deref().and_then(|j| j.as_ref().ok()).filter(|j| j.active().is_ok()).map(|j| j.trace(source)) };
    cpp_phase("translation_unit_start", source_index, source_count);
    let output_before = exec.output_bytes();
    let compile_started = Instant::now();
    if let Some(cache) = cache { cache.stats.clang_invocations += 1; }
    let exit = exec.step("clang").binary("/llvm.core.wasm").compiler_stock(trace.clone()).args(&arguments).run().await.expect("Compilation succeeded");
    cpp_phase("translation_unit_complete", source_index, source_count);
    if let Some(cache) = cache { cache.stats.compile_ms += compile_started.elapsed().as_secs_f64() * 1000.0; }
    if !exit.is_success() { return Some(exit.raw()); }
    let expected_capture = trace.is_some();
    let observation = trace.and_then(|t| t.lock().ok().map(|t| t.finish()));
    let capture_bypass = observation.as_ref().and_then(|o| o.bypass)
        .or_else(|| (expected_capture && observation.is_none()).then_some("stock-capture-unavailable"));
    if let Some(reason) = capture_bypass {
        if let Some(Ok(job)) = job.as_deref_mut() { job.poison(reason); }
        WorkerOut::CppCacheAbandon.send();
        if let Some(cache) = cache { cache.bypass(source, reason); }
        return None;
    }
    if eligibility.is_err() { return None; }
    let (Some(cache), Some(Ok(job)), Some(observation)) = (cache, job, observation) else { return None; };
    if exec.output_bytes() != output_before { cache.bypass(source, "compiler-diagnostics"); return None; }
    let Ok(object) = exec.read_cache_bytes(&object_path, (128 * 1024 * 1024usize).saturating_sub(cache.pending_bytes)).await else {
        WorkerOut::CppCacheAbandon.send(); cache.bypass(source, "object-capture-budget-or-missing"); return None;
    };
    let admitted = Instant::now();
    let (key, record) = match job.admit(exec, source, &arguments, observation, metadata_limit).await {
        Ok(value) => value, Err(reason) => { cache.bypass(source, &reason); return None; },
    };
    cache.stats.hash_ms += admitted.elapsed().as_secs_f64() * 1000.0;
    let object_hash = cpp_cache::hash(&object);
    if !cpp_cache::valid_object(&object, &object_hash) { cache.bypass(source, "invalid-object"); return None; }
    if cache.pending_count >= 256 || object.len() > 128 * 1024 * 1024 - cache.pending_bytes {
        WorkerOut::CppCacheAbandon.send(); cache.bypass(source, "staging-budget"); return None;
    }
    cache.pending_bytes += object.len(); cache.pending_count += 1;
    WorkerOut::CppCacheObject { generation: cache.generation, source: source.into(), key, object_hash, receipt: Some(record), object: &object }.send();
    None
}

type SharedFs = Arc<dyn FileSystem + Send + Sync>;

thread_local! {
    // The verified sysroot and exception runtime, unpacked once per worker (about 50 ms
    // each time otherwise) and keyed by the sysroot hash. Runs see them read-only.
    static CPP_SYSROOT: std::cell::RefCell<Option<(String, SharedFs, SharedFs)>> = const { std::cell::RefCell::new(None) };
}

async fn cpp_sysroot(toolchain: &execution::CompilerToolchain) -> (SharedFs, SharedFs) {
    let identity = &toolchain.identities.1;
    let cached = CPP_SYSROOT.with(|slot| slot.borrow().as_ref().filter(|(id, ..)| id == identity).map(|(_, s, e)| (s.clone(), e.clone())));
    if let Some(cached) = cached { return cached; }
    let sysroot = extract_tar_gz(toolchain.sysroot.clone()).await.expect("C++ sysroot filesystem");
    let exceptions = extract_tar_gz(include_bytes!("../../assets/cpp-exceptions/runtime.tar.gz").to_vec()).await.expect("embedded C++ exception runtime");
    let sysroot: SharedFs = Arc::new(read_only_fs::ReadOnlyFs(Arc::new(sysroot)));
    let exceptions: SharedFs = Arc::new(read_only_fs::ReadOnlyFs(Arc::new(exceptions)));
    CPP_SYSROOT.with(|slot| *slot.borrow_mut() = Some((identity.clone(), sysroot.clone(), exceptions.clone())));
    (sysroot, exceptions)
}

const PREAMBLE_HEADER: &str = "/__glassjar/preamble.h";
const PREAMBLE_PCH: &str = "/__glassjar/preamble.pch";

thread_local! {
    // Precompiled leading `#include <...>` blocks (clangd calls these preambles), so a file
    // that starts with <iostream> parses it once per worker instead of on every run.
    // Most recently used last; `None` remembers a block that failed to precompile.
    static PREAMBLES: std::cell::RefCell<Vec<(String, Option<std::rc::Rc<Vec<u8>>>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The leading `#include <...>` lines of a source file, skipping blank lines and `//` comments.
fn system_preamble(source: &str) -> String {
    let mut preamble = String::new();
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") { continue; }
        let Some(header) = line.strip_prefix('#').and_then(|d| d.trim_start().strip_prefix("include")).map(str::trim_start) else { break };
        let Some(end) = header.find('>').filter(|_| header.starts_with('<')) else { break };
        let rest = header[end + 1..].trim();
        if !rest.is_empty() && !rest.starts_with("//") { break; }
        preamble.push_str(&format!("#include {}\n", &header[..=end]));
    }
    preamble
}

/// Mounts a precompiled header for the source's system include block, building it once per
/// worker. False when there is no such block or it doesn't precompile.
async fn use_preamble_pch(exec: &Execution, source: &str) -> bool {
    let Ok(text) = exec.read_bytes(source).await else { return false };
    let preamble = system_preamble(&String::from_utf8_lossy(&text));
    if preamble.is_empty() { return false; }
    let cached = PREAMBLES.with(|cache| {
        let mut cache = cache.borrow_mut();
        let at = cache.iter().position(|(k, _)| *k == preamble)?;
        let entry = cache.remove(at);
        let pch = entry.1.clone();
        cache.push(entry);
        Some(pch)
    });
    if create_dir_all(&exec.fs, Path::new("/__glassjar")).is_err() || exec.write_bytes(PREAMBLE_HEADER, preamble.as_bytes()).await.is_err() { return false; }
    let pch = match cached {
        Some(pch) => pch,
        None => {
            // Quiet: an include that fails here fails again, visibly, in the real compile.
            let mut args = cpp_clang_args(PREAMBLE_HEADER, false, &CppArtifacts::default(), false);
            for arg in &mut args {
                match arg.as_str() { "-emit-obj" => *arg = "-emit-pch".into(), "c++" => *arg = "c++-header".into(), _ => {} }
            }
            if let Some(out) = args.iter().position(|a| a == "-o") { args[out + 1] = PREAMBLE_PCH.into(); }
            // Each run rewrites the header, so validate it by content rather than timestamp.
            let at = args.len() - 1;
            args.insert(at, "-fvalidate-ast-input-files-content".into());
            let built = exec.step("clang").binary("/llvm.core.wasm").compiler().quiet().args(&args).run().await;
            let pch = match built {
                Ok(exit) if exit.is_success() => exec.read_bytes(PREAMBLE_PCH).await.ok().map(std::rc::Rc::new),
                _ => None,
            };
            PREAMBLES.with(|cache| {
                let mut cache = cache.borrow_mut();
                if cache.len() >= 4 { cache.remove(0); }
                cache.push((preamble, pch.clone()));
            });
            pch
        }
    };
    match pch {
        Some(pch) => exec.write_bytes(PREAMBLE_PCH, &pch).await.is_ok(),
        None => false,
    }
}

async fn start_cpp(
    is_debug: bool,
    build_only: bool,
    prepare_only: bool,
    build_start: Instant,
    sources: Vec<String>,
    exec: Execution,
    fs: mem_fs::FileSystem,
    host_device: Option<HostDeviceFile>,
    artifacts: CppArtifacts,
    request: Option<CppCacheRequest>,
    inputs: Option<cpp_cache::Inputs>,
    stock_inputs: Option<Result<cpp_cache_inputs::UserInputs, cpp_cache_inputs::Bypass>>,
    toolchain: std::rc::Rc<execution::CompilerToolchain>,
    input_hash_ms: f64,
) {
    let hash_start = Instant::now();
    let identities = request.as_ref().map(|_| &toolchain.identities);
    let mut cache = request.map(|request| CppCacheJob {
        generation: request.generation,
        entries: request.entries.into_iter().take(cpp_cache::MAX_REPORTED_SOURCES).map(|entry| (entry.source.clone(), entry)).collect(),
        pending_count: 0,
        pending_bytes: 0,
        metadata_limit: request.max_entry_metadata_bytes.unwrap_or(0).min(cpp_cache_inputs::MAX_METADATA_BYTES),
        stats: CppCacheStats { hash_ms: input_hash_ms + hash_start.elapsed().as_secs_f64() * 1000.0, ..Default::default() },
    });
    let (sysroot, exceptions) = cpp_sysroot(&toolchain).await;
    exec.fs.union(&sysroot);
    // These files add exception support without replacing the fetched sysroot.
    exec.fs.union(&exceptions);
    let user_fs: Arc<dyn FileSystem + Send + Sync> = Arc::new(fs);
    exec.fs.union(&user_fs);
    // Compiler code is an internal verified handle, never a mutable guest file.
    let link_args = cpp_link_args(&sources, &artifacts);
    let stock_started = Instant::now();
    let mut stock_job = if let Some(inputs) = stock_inputs {
        Some(cpp_stock_job::StockJob::new(&exec, &artifacts, &sources, inputs, &toolchain).await)
    } else { None };
    if prepare_only {
        let result = match stock_job.as_mut() {
            Some(Ok(job)) => job.preflight(&exec).await.and_then(|_| job.snapshot().map(|snapshot| (job.certificate().to_string(), snapshot))),
            Some(Err(reason)) => Err(reason.clone()),
            None => Err("stock-cache-disabled".into()),
        };
        match result {
            Ok((certificate, snapshot)) => WorkerOut::CppCacheEligibility { eligible: true, certificate_sha256: Some(certificate), snapshot: Some(snapshot), reason: None }.send(),
            Err(reason) => WorkerOut::CppCacheEligibility { eligible: false, certificate_sha256: stock_job.as_ref().and_then(|j| j.as_ref().ok()).map(|j| j.certificate().into()), snapshot: None, reason: Some(reason) }.send(),
        }
        return stop(0, build_start, None).send();
    }
    if let Some(cache) = &mut cache { cache.stats.hash_ms += stock_started.elapsed().as_secs_f64() * 1000.0; }

    cpp_phase("object_build_start", 0, sources.len());
    for (source_index, source) in sources.iter().enumerate() {
        yield_to_messages().await;
        if toolchain.stock_profile {
            if let Some(exit) = compile_stock_source(&exec, &mut cache, stock_job.as_mut(), source, source_index, sources.len(), is_debug, &artifacts).await {
                if let Some(cache) = cache { cache.send(); }
                return stop(exit, build_start, None).send();
            }
            continue;
        }
        let obj_path = format!("{source}.o");
        let hash_start = Instant::now();
        let bypass = if is_debug { Some("debug-profile") }
            else if artifacts.precompiled_header.is_some() { Some("opaque-precompiled-header") }
            else if cache.is_some() && exec.fs.metadata(Path::new(&format!("{source}.o.d"))).is_ok() { Some("dependency-path-collision") }
            else { inputs.as_ref().and_then(|inputs| inputs.bypass(source)) };
        let mut clang_args = cpp_clang_args(source, is_debug, &artifacts, cache.is_some() && bypass.is_none());
        // Debug builds share the plain (no debug info) preamble: debug info is generated from
        // the AST at code generation, so functions, variables, types and line tables come out
        // the same. Only declarations nothing uses (such as `std::` using-declarations from C
        // headers) are left out of the DWARF; the debugger never reads them.
        if cache.is_none() && artifacts.precompiled_header.is_none() && use_preamble_pch(&exec, source).await {
            let at = clang_args.len() - 1;
            clang_args.splice(at..at, ["-include-pch".into(), PREAMBLE_PCH.into(), "-fvalidate-ast-input-files-content".into()]);
        }
        let key = if bypass.is_none() {
            inputs.as_ref().zip(identities.as_ref()).and_then(|(inputs, (compiler, sysroot, exceptions))| inputs.key(source, compiler, sysroot, exceptions, &clang_args, &link_args))
        } else { None };
        if let Some(cache) = &mut cache { cache.stats.hash_ms += hash_start.elapsed().as_secs_f64() * 1000.0; }
        let lookup_start = Instant::now();
        let mut cached = false;
        if let Some(cache) = &mut cache {
            if let Some(reason) = bypass { cache.bypass(source, reason); }
            else if let Some(key) = &key {
                if let Some(entry) = cache.entries.remove(source) {
                    if entry.key == *key {
                        if entry.object.len() <= 128 * 1024 * 1024 && cpp_cache::valid_object(&entry.object, &entry.object_hash) {
                            // write_bytes creates a fresh owned copy in this job's fresh fs.
                            cached = exec.write_bytes(&obj_path, &entry.object).await.is_ok();
                        }
                        if !cached && cache.stats.invalid_sources.len() < cpp_cache::MAX_REPORTED_SOURCES { cache.stats.invalid_sources.push(source.clone()); }
                    }
                }
                if cached {
                    cache.stats.hits += 1;
                    if cache.stats.hit_sources.len() < cpp_cache::MAX_REPORTED_SOURCES { cache.stats.hit_sources.push(source.clone()); }
                } else { cache.stats.misses += 1; }
            } else { cache.bypass(source, "unsupported-input"); }
            cache.stats.lookup_ms += lookup_start.elapsed().as_secs_f64() * 1000.0;
        }
        // A hit is counted only here, when the clang step is actually omitted.
        if cached { cpp_phase("cache_hit", source_index, sources.len()); continue; }
        cpp_phase("translation_unit_start", source_index, sources.len());
        let output_before = exec.output_bytes();
        let compile_start = Instant::now();
        if let Some(cache) = &mut cache { cache.stats.clang_invocations += 1; }
        let exit = exec.step("clang").binary("/llvm.core.wasm").compiler().args(&clang_args).run().await.expect("Compilation succeeded");
        cpp_phase("translation_unit_complete", source_index, sources.len());
        if let Some(cache) = &mut cache { cache.stats.compile_ms += compile_start.elapsed().as_secs_f64() * 1000.0; }
        // Dependency receipts are compiler-private. They must never appear in
        // the student filesystem or differ between cache misses and hits.
        let depfile = if key.is_some() {
            let dep_path = format!("{source}.o.d");
            let bytes = exec.read_bytes(&dep_path).await;
            let _ = exec.fs.remove_file(Path::new(&dep_path));
            if exec.fs.metadata(Path::new(&dep_path)).is_ok() {
                if let Some(cache) = cache { cache.send(); }
                return WorkerOut::Error { message: "Cannot remove compiler dependency receipt".into() }.send();
            }
            Some(bytes)
        } else { None };
        if !exit.is_success() {
            if let Some(cache) = cache { cache.send(); }
            return stop(exit.raw(), build_start, None).send();
        }
        if let Some(cache) = &mut cache {
            if let Some(key) = key {
                if exec.output_bytes() != output_before { cache.bypass(source, "compiler-diagnostics"); continue; }
                let depfile = depfile.expect("eligible compile has dependency receipt result");
                if !depfile.as_ref().ok().zip(inputs.as_ref()).is_some_and(|(depfile, inputs)| cpp_cache::dependencies_valid(depfile, source, &sources, &inputs.user_paths, &inputs.opaque_paths)) {
                    cache.bypass(source, "unsupported-dependencies"); continue;
                }
                if let Some(reason) = inputs.as_ref().and_then(|inputs| inputs.dependency_bypass(depfile.as_ref().unwrap())) {
                    cache.bypass(source, reason); continue;
                }
                let mut volatile_dependency = false;
                let mut system_headers = Vec::new();
                let mut user_headers = Vec::new();
                for dependency in cpp_cache::dependencies(depfile.as_ref().unwrap()).unwrap().into_iter()
                    .map(|path| cpp_cache::normalized_dependency(&path).expect("validated canonical dependency")) {
                    if inputs.as_ref().is_some_and(|inputs| !inputs.user_paths.contains(&dependency)) {
                        match exec.read_bytes(&dependency).await {
                            Ok(bytes) if !cpp_cache::volatile_macro(&bytes) => system_headers.push(bytes),
                            _ => { volatile_dependency = true; break; },
                        }
                    } else {
                        match exec.read_bytes(&dependency).await {
                            Ok(bytes) => user_headers.push(bytes),
                            _ => { volatile_dependency = true; break; },
                        }
                    }
                }
                if volatile_dependency { cache.bypass(source, "volatile-system-header"); continue; }
                let Some(tainted) = cpp_cache::trusted_macro_taint(&system_headers) else { cache.bypass(source, "unsupported-system-macros"); continue; };
                if user_headers.iter().any(|bytes| cpp_cache::references_tainted_macro(bytes, &tainted)) {
                    cache.bypass(source, "dynamic-system-macro"); continue;
                }
                let Ok(object) = exec.read_cache_bytes(&obj_path, (128 * 1024 * 1024usize).saturating_sub(cache.pending_bytes)).await else {
                    WorkerOut::CppCacheAbandon.send(); cache.bypass(source, "object-capture-budget-or-missing"); continue;
                };
                let object_hash = cpp_cache::hash(&object);
                if !cpp_cache::valid_object(&object, &object_hash) { cache.bypass(source, "invalid-object"); continue; }
                if cache.pending_count >= 256 || object.len() > 128 * 1024 * 1024 - cache.pending_bytes {
                    WorkerOut::CppCacheAbandon.send();
                    cache.bypass(source, "staging-budget"); continue;
                }
                cache.pending_bytes += object.len();
                cache.pending_count += 1;
                WorkerOut::CppCacheObject { generation: cache.generation, source: source.clone(), key, object_hash, receipt: None, object: &object }.send();
            }
        }
    }

    if toolchain.stock_profile {
        let verified = stock_job.as_ref().and_then(|j| j.as_ref().ok());
        let eligible = verified.is_some_and(|job| job.active().is_ok()) && cache.as_ref().is_some_and(|c|
            c.stats.bypasses.is_empty() && c.stats.hits as usize + c.pending_count == sources.len());
        let reason = if eligible { None } else {
            cache.as_ref().and_then(|c| c.stats.bypasses.first()).map(|b| b.reason.clone())
                .or_else(|| stock_job.as_ref().and_then(|j| j.as_ref().err()).cloned())
                .or_else(|| Some("stock-incomplete-admission".into()))
        };
        WorkerOut::CppCacheEligibility { eligible, certificate_sha256: verified.map(|j| j.certificate().into()),
            snapshot: if eligible { verified.and_then(|j| j.snapshot().ok()) } else { None }, reason }.send();
    }
    if let Some(cache) = &mut cache { cache.entries.clear(); }
    if let Some(cache) = &cache { WorkerOut::CppCacheStats { generation: cache.generation, stats: cache.stats.clone() }.send(); }
    cpp_phase("object_build_complete", sources.len(), sources.len());
    // A real macrotask boundary makes queued cancellation observable before commit.
    yield_to_messages().await;
    if execution::object_build_complete().await.is_err() {
        return WorkerOut::Error { message: "Object publication acknowledgement failed".into() }.send();
    }
    // Preparation stops before linking, student instantiation or host devices.
    if build_only {
        if let Some(cache) = cache { cache.send(); }
        return stop(0, build_start, None).send();
    }
    cpp_phase("link_start", sources.len(), sources.len());
    let link_start = Instant::now();
    if let Some(cache) = &mut cache { cache.stats.link_invocations += 1; }
    let mut linker = exec.step("wasm-ld").binary("/llvm.core.wasm").compiler().args(&link_args);
    if toolchain.stock_profile { linker = linker.compiler_stock(None); }
    let exit = linker.run().await.expect("Linking succeeded");
    if let Some(cache) = &mut cache {
        cache.stats.link_ms = link_start.elapsed().as_secs_f64() * 1000.0;
        // A noncooperative/GUI guest may never return, and Stop can terminate
        // this worker. Publish completed compiler counters before guest entry.
        WorkerOut::CppCacheStats { generation: cache.generation, stats: cache.stats.clone() }.send();
    }
    cpp_phase("link_complete", sources.len(), sources.len());
    if !exit.is_success() {
        if let Some(cache) = cache { cache.send(); }
        return stop(exit.raw(), build_start, None).send();
    }

    let run_start = Instant::now();
    cpp_phase("guest_start", sources.len(), sources.len());
    let mut run = exec.step("main").binary("/main.wasm").debug(is_debug);
    if let Some(device) = host_device { run = run.device_file(HOST_DEVICE_PATH, Box::new(device)); }
    let exit = run.run().await.expect("Running succeeded");
    if let Some(cache) = cache { cache.send(); }
    stop(exit.raw(), build_start, Some(run_start)).send();
}

async fn start_rust(
    is_debug: bool,
    build_start: Instant,
    sources: Vec<String>,
    exec: Execution,
    fs: mem_fs::FileSystem,
) {
    let mut union_fs: Option<Box<dyn FileSystem>> = Some(Box::new(fs));

    for source in &sources {
        let obj_path = rust_obj_path(source);
        let mut rustc_args = vec![
            source.as_str(),
            // Without this rustc defaults to the 2015 edition; `cargo new` uses 2024.
            "--edition=2024",
            "--sysroot",
            "/",
            "--target",
            "wasm32-wasip1",
            "-Cpanic=abort",
            "-Ccodegen-units=1",
            "-Zthreads=1",
            "--emit=obj",
            "-o",
            obj_path.as_str(),
        ];

        if is_debug {
            rustc_args.extend(["-g", "-Copt-level=0"]);
        }

        let mut step = exec
            .step("rustc")
            .binary(RUST_WASM_URL)
            .args(&rustc_args);
        if let Some(fs) = union_fs.take() {
            step = step
                .sysroot(RUST_SYSROOT_URL)
                .fs(fs);
        }
        let exit = step.run().await.expect("Compilation succeeded");

        if !exit.is_success() {
            return stop(exit.raw(), build_start, None).send();
        }
    }

    let mut link_args: Vec<String> = vec![
        "--export=__main_void".into(),
        "-z".into(),
        "stack-size=1048576".into(),
        "--stack-first".into(),
        "--no-demangle".into(),
        "/lib/rustlib/wasm32-wasip1/lib/self-contained/crt1-command.o".into(),
    ];
    for source in &sources {
        link_args.extend(collect_rust_objs(&exec.fs, source));
    }
    link_args.extend(find_rust_rlibs(&exec.fs));
    link_args.extend([
        "-L/lib/rustlib/wasm32-wasip1/lib/self-contained".into(),
        "-lc".into(),
        "-o".into(),
        "/main.wasm".into(),
        "--gc-sections".into(),
        "-O0".into(),
    ]);

    let exit = exec
        .step("wasm-ld")
        .binary(CPP_WASM_URL)
        .args(link_args.iter().map(String::as_str))
        .run()
        .await
        .expect("Linking succeeded");

    if !exit.is_success() {
        return stop(exit.raw(), build_start, None).send();
    }

    let run_start = Instant::now();
    let exit = exec
        .step("main")
        .binary("/main.wasm")
        .debug(is_debug)
        .run()
        .await
        .expect("Running succeeded");

    stop(exit.raw(), build_start, Some(run_start)).send();
}

fn cpp_phase(phase: &str, source_index: usize, source_count: usize) {
    WorkerOut::CppPhase { phase: phase.into(), source_index, source_count }.send();
}

async fn yield_to_messages() {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let scope = DedicatedWorkerGlobalScope::from(JsValue::from(js_sys::global()));
        let _ = scope.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

#[wasm_bindgen]
pub fn main() {
    console_error_panic_hook::set_once();
    let scope = DedicatedWorkerGlobalScope::from(JsValue::from(js_sys::global()));

    let onmessage = Closure::wrap(Box::new(move |msg: MessageEvent| {
        let message: WorkerStart = serde_wasm_bindgen::from_value(msg.data()).expect("");
        wasm_bindgen_futures::spawn_local(start(message));
    }) as Box<dyn Fn(MessageEvent)>);
    scope.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    WorkerOut::Ready.send();
}
