use flate2::read::GzDecoder;
use js_sys::SharedArrayBuffer;
use std::cell::RefCell;
use std::rc::Rc;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tar::Archive;
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;
use wasmer::{Module, RuntimeError, Store};
use wasmer_wasix::types::wasi::ExitCode;
use wasmer_wasix::virtual_fs::AsyncReadExt;
use wasmer_wasix::{
    WasiEnv, WasiEnvBuilder, WasiError, WasiFunctionEnv,
    virtual_fs::{AsyncWriteExt, FileSystem, create_dir_all, mem_fs},
};

use web_sys::DedicatedWorkerGlobalScope;

use super::debuggee::Debuggee;
use crate::debug::instrument::{InstrumenterResult, instrument_wasm};
use crate::types::{StdoutMode, WorkerOut};

use super::io::{Stdin, Stdout};
use super::runtime::JsRuntime;
use super::cpp_stock_trace_host::{InvocationTrace, TraceImport};

use std::fmt::Debug;

thread_local! {
    // Explicitly installed by our worker entrypoint. Only immutable compiler code is retained.
    static COMPILER_MODULE_CONSTRUCTOR: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    static TOOLCHAIN: RefCell<Option<Rc<CompilerToolchain>>> = const { RefCell::new(None) };
    static OBJECT_BUILD_OBSERVER: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    static COMPILER_INSTANCE_OBSERVER: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
}

pub struct CompilerToolchain {
    pub compiler: Vec<u8>,
    pub sysroot: Vec<u8>,
    pub identities: (String, String, String),
    pub stock_profile: bool,
}

#[wasm_bindgen]
pub fn configure_compiler_toolchain(compiler: Vec<u8>, sysroot: Vec<u8>, compiler_hash: String, sysroot_hash: String, exceptions_hash: String, stock_profile: bool) {
    // Only the private worker entrypoint accepts this parent-owned seed. Its hashes
    // were computed from owned bytes before transport; guest filesystem bytes never enter it.
    TOOLCHAIN.with(|slot| *slot.borrow_mut() = Some(Rc::new(CompilerToolchain { compiler, sysroot, identities: (compiler_hash, sysroot_hash, exceptions_hash), stock_profile })));
}

pub fn compiler_toolchain() -> Option<Rc<CompilerToolchain>> {
    TOOLCHAIN.with(|slot| slot.borrow().clone())
}

#[wasm_bindgen]
pub fn set_object_build_observer(observer: js_sys::Function) {
    OBJECT_BUILD_OBSERVER.with(|slot| *slot.borrow_mut() = Some(observer));
}

pub async fn object_build_complete() -> Result<(), JsValue> {
    let observer = OBJECT_BUILD_OBSERVER.with(|slot| slot.borrow().clone());
    if let Some(observer) = observer {
        let promise = observer.call0(&JsValue::UNDEFINED)?;
        JsFuture::from(js_sys::Promise::resolve(&promise)).await?;
    }
    Ok(())
}

pub async fn prepare_compiler() -> Result<(), JsValue> {
    if compiler_toolchain().is_none() { return Err("Missing verified compiler toolchain".into()); }
    let constructor = COMPILER_MODULE_CONSTRUCTOR.with(|slot| slot.borrow().clone());
    if let Some(constructor) = constructor {
        let toolchain = compiler_toolchain().expect("verified toolchain");
        let bytes = unsafe { js_sys::Uint8Array::view(&toolchain.compiler) };
        JsFuture::from(js_sys::Promise::resolve(&constructor.call1(&JsValue::UNDEFINED, bytes.as_ref())?)).await?;
    }
    Ok(())
}


#[wasm_bindgen]
pub fn set_compiler_module_constructor(constructor: js_sys::Function) {
    COMPILER_MODULE_CONSTRUCTOR.with(|slot| *slot.borrow_mut() = Some(constructor));
}

#[wasm_bindgen]
pub fn set_compiler_instance_observer(observer: js_sys::Function) {
    COMPILER_INSTANCE_OBSERVER.with(|slot| *slot.borrow_mut() = Some(observer));
}

pub struct Execution {
    pub fs: mem_fs::FileSystem,
    pub stdin_buffer: js_sys::SharedArrayBuffer,
    output_bytes: Arc<AtomicU64>,
}

pub struct Step<'a> {
    // exec cannot outlive Step
    exec: &'a Execution,
    builder: WasiEnvBuilder,
    binary: Option<String>,
    sysroot: Option<String>,
    union_fs: Option<Box<dyn FileSystem>>,
    debug: bool,
    compiler: bool,
    stock_profile: bool,
    stock_trace: Option<InvocationTrace>,
    envs: Vec<(String, String)>,
    device_files: Vec<(PathBuf, Box<dyn wasmer_wasix::virtual_fs::VirtualFile + Send + Sync>)>,
}

impl Execution {
    pub fn new(stdin_buffer: SharedArrayBuffer) -> Self {
        Self {
            fs: mem_fs::FileSystem::default(),
            stdin_buffer,
            output_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Count both streams while preserving their original delivery and contents.
    pub fn output_bytes(&self) -> u64 {
        self.output_bytes.load(Ordering::Relaxed)
    }

    pub fn step<'a>(&'a self, name: &str) -> Step<'a> {
        Step {
            exec: self,
            builder: WasiEnv::builder(name),
            binary: None,
            sysroot: None,
            union_fs: None,
            debug: false,
            compiler: false,
            stock_profile: false,
            stock_trace: None,
            envs: Vec::new(),
            device_files: Vec::new(),
        }
    }

    pub async fn read_bytes(&self, path: &str) -> Result<Vec<u8>, std::io::Error> {
        let mut file = self.fs.new_open_options().read(true).open(path)?;
        let mut wasm_bytes = Vec::new();
        file.read_to_end(&mut wasm_bytes).await?;
        Ok(wasm_bytes)
    }

    /// Optional cache reads are bounded before allocating and use fallible
    /// reservation. The owned job filesystem is immutable between tool steps.
    pub async fn read_cache_bytes(&self, path: &str, limit: usize) -> Result<Vec<u8>, std::io::Error> {
        let size = usize::try_from(self.fs.metadata(std::path::Path::new(path))?.len)
            .ok().filter(|size| *size <= limit)
            .ok_or_else(|| std::io::Error::other("cache input budget"))?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).map_err(|_| std::io::Error::other("cache input allocation"))?;
        bytes.resize(size, 0);
        let mut file = self.fs.new_open_options().read(true).open(path)?;
        file.read_exact(&mut bytes).await?;
        Ok(bytes)
    }

    #[allow(dead_code)]
    pub async fn write_bytes(&self, path: &str, bytes: &[u8]) -> Result<(), std::io::Error> {
        let mut file = self
            .fs
            .new_open_options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        file.write_all(&bytes).await?;
        Ok(())
    }
}

impl<'a> Step<'a> {
    /// Sets the binary to be executed for this step.
    ///
    /// If it starts with a "/", it is treated as an absolute path in the current filesystem.
    pub fn binary(mut self, url_or_path: &str) -> Self {
        self.binary = Some(String::from(url_or_path));
        self
    }

    /// Only LLVM compiler/linker steps may use immutable compiler Module retention.
    pub fn compiler(mut self) -> Self {
        self.compiler = true;
        self
    }

    /// Observe only the fixed stock compiler invocation; never a student program.
    pub fn compiler_stock(mut self, trace: Option<InvocationTrace>) -> Self {
        self.compiler = true;
        self.stock_profile = true;
        self.stock_trace = trace;
        self
    }

    /// Sets the sysroot to be used for this step.
    ///
    /// This should be a URL to a tarball which will be injected into the root of the filesystem.
    pub fn sysroot(mut self, url: &str) -> Self {
        self.sysroot = Some(String::from(url));
        self
    }

    /// Allows unioning a custom filesystem into this step's filesystem.
    ///
    /// The unioned filesystem will be layered on top of the sysroot, if any,
    /// potentially overwriting files in the sysroot if there are conflicts.
    pub fn fs(mut self, fs: Box<dyn FileSystem>) -> Self {
        self.union_fs = Some(fs);
        self
    }

    /// Adds arguments to be passed to argv.
    /// The program name is not included here.
    pub fn args<I, Arg>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = Arg>,
        Arg: AsRef<[u8]>,
    {
        self.builder.add_args(args);
        self
    }

    /// Enable/disable debug mode for this step
    pub fn debug(mut self, enable_debugging: bool) -> Self {
        self.debug = enable_debugging;
        self
    }

    pub fn envs<I, K, V>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.envs.extend(
            envs.into_iter()
                .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string())),
        );
        self
    }

    pub fn device_file(mut self, path: &str, file: Box<dyn wasmer_wasix::virtual_fs::VirtualFile + Send + Sync>) -> Self {
        self.device_files
            .push((PathBuf::from(path), file));
        self
    }

    /// Runs this step to completion
    pub async fn run(mut self) -> Result<ExitCode, RuntimeError> {
        /* Download the binary from the URL / filesystem */
        let Some(binary_loc) = &self.binary else {
            return Err(RuntimeError::new("No binary specified"));
        };

        /* In debug mode, we need to instrument the binary */
        let mut debugger = None;

        let binary_bytes = if self.compiler {
            compiler_toolchain().ok_or_else(|| RuntimeError::new("Missing verified compiler toolchain"))?.compiler.clone()
        } else if binary_loc.starts_with("/") {
            let mut wasm = self
                .exec
                .read_bytes(binary_loc)
                .await
                .ensure("Read binary from filesystem")?;

            if self.debug {
                WorkerOut::Artifact {
                    data: &wasm,
                    name: "pre.wasm".into(),
                }
                .send();

                let InstrumenterResult {
                    info,
                    wasm: instrumented_wasm,
                } = instrument_wasm(&wasm).ensure("Instrumented WASM")?;
                debugger = Some(Debuggee::new(info));
                wasm = instrumented_wasm;

                WorkerOut::Artifact {
                    data: &wasm,
                    name: "post.wasm".into(),
                }
                .send();
            }

            wasm
        } else {
            fetch_bytes(binary_loc)
                .await
                .ensure("Fetch binary from network")?
        };

        let mut store = Store::default();
        let constructor = if self.compiler {
            COMPILER_MODULE_CONSTRUCTOR.with(|slot| slot.borrow().clone())
        } else {
            None
        };
        let module = if let Some(constructor) = constructor {
            // The JS constructor synchronously takes its own snapshot before its first await.
            let bytes = unsafe { js_sys::Uint8Array::view(&binary_bytes) };
            let promise = constructor
                .call1(&JsValue::UNDEFINED, bytes.as_ref())
                .ensure("Constructed compiler Module")?;
            let module: js_sys::WebAssembly::Module =
                JsFuture::from(js_sys::Promise::resolve(&promise))
                    .await
                    .ensure("Compiled compiler Module")?
                    .dyn_into()
                    .ensure("Received compiler Module")?;
            // Wasmer's supported JS conversion reparses type hints from these verified bytes.
            Module::from((module, binary_bytes))
        } else {
            Module::from_binary(&store, &binary_bytes).ensure("Created WASM module")?
        };

        /* Fetch the sysroot and union into the vfs if needed */
        if let Some(sysroot_loc) = &self.sysroot {
            let sysroot_bytes = fetch_bytes(sysroot_loc)
                .await
                .ensure("Fetched sysroot from network")?;
            let sysroot_fs = extract_tar_gz(sysroot_bytes)
                .await
                .ensure("Created sysroot fs from tarball")?;
            let sysroot_fs: Arc<dyn FileSystem + Send + Sync> = Arc::new(sysroot_fs);
            self.exec.fs.union(&sysroot_fs);
        }

        /* Union user files into the vfs if needed */
        if let Some(union_fs) = self.union_fs {
            let union_fs: Arc<dyn FileSystem + Send + Sync> = Arc::new(union_fs);
            self.exec.fs.union(&union_fs);
        }

        for (path, file) in self.device_files {
            if let Some(parent) = path.parent() {
                create_dir_all(&self.exec.fs, parent).ensure("Created device parent directory")?;
            }
            self.exec
                .fs
                .new_open_options_ext()
                .insert_device_file(path, file)
                .ensure("Registered device file")?;
        }

        /* Configure a fresh Wasmer WASI environment and Instance on every invocation. */
        let instance_start = instant::Instant::now();
        for (k, v) in &self.envs {
            self.builder.add_env(k, v);
        }
        if self.stock_profile {
            // The stock environment is enforced here, never inferred from
            // WASI defaults or the student's interactive input queue.
            self.builder.get_env_mut().clear();
            self.builder.set_current_dir("/");
        }
        let mut builder = self
            .builder
            .runtime(JsRuntime::instance())
            .fs(Box::new(self.exec.fs.clone()))
            .stdout(Box::new(Stdout::new_counted(StdoutMode::Out, self.exec.output_bytes.clone())))
            .stderr(Box::new(Stdout::new_counted(StdoutMode::Err, self.exec.output_bytes.clone())))
            .stdin(Box::new(Stdin::new(&self.exec.stdin_buffer)));
        if self.stock_profile {
            builder = builder.stdin(Box::new(wasmer_wasix::virtual_fs::NullFile::default()));
        }

        builder
            .add_preopen_dir("/")
            .ensure("Preopened root directory")?;

        /* Instantiate and run the binary */
        let instance = if let Some(debugger) = debugger {
            let wasi_env = builder.build().ensure("Built WASI environment")?;
            let mut wasi_func_env = WasiFunctionEnv::new(&mut store, wasi_env);

            let mut imports = wasi_func_env
                .import_object(&mut store, &module)
                .ensure("Created WASI import object")?;

            debugger.attach(&mut store, &mut imports);

            let instance = wasmer::Instance::new(&mut store, &module, &imports)
                .ensure("Created instance with debug imports")?;

            wasi_func_env
                .initialize(&mut store, instance.clone())
                .ensure("Initialized WASI")?;

            instance
        } else if self.stock_profile && self.stock_trace.is_some() {
            let wasi_env = builder.build().ensure("Built stock compiler WASI environment")?;
            let mut wasi_func_env = WasiFunctionEnv::new(&mut store, wasi_env);
            let mut imports = wasi_func_env.import_object(&mut store, &module).ensure("Created stock compiler WASI imports")?;
            let trace = TraceImport::attach(&mut store, &mut imports, &module, self.stock_trace.take().expect("stock trace"));
            let instance = wasmer::Instance::new(&mut store, &module, &imports).ensure("Created stock compiler instance with observation")?;
            TraceImport::bind(&trace, &mut store, &instance);
            wasi_func_env.initialize(&mut store, instance.clone()).ensure("Initialized stock compiler WASI")?;
            instance
        } else {
            let (instance, _env) = builder
                .instantiate(module, &mut store)
                .ensure("Instantiated Wasmer instance")?;
            instance
        };

        if self.compiler {
            COMPILER_INSTANCE_OBSERVER.with(|slot| {
                if let Some(observer) = slot.borrow().as_ref() {
                    let milliseconds = instance_start.elapsed().as_secs_f64() * 1000.0;
                    let _ = observer.call1(&JsValue::UNDEFINED, &milliseconds.into());
                }
            });
        }

        let start = instance
            .exports
            .get_function("_start")
            .ensure("Found _start export")?;

        /* Prevent `exit(0)` from being treated as an error */
        if let Err(err) = start.call(&mut store, &[]) {
            let wasi_err = err.downcast_ref::<WasiError>();
            let Some(wasi_err) = wasi_err else {
                return Err(err);
            };

            let WasiError::Exit(code) = wasi_err else {
                return Err(err);
            };

            return Ok(*code);
        }

        Ok(0.into())
    }
}

pub async fn fetch_bytes(url: &str) -> Result<Vec<u8>, JsValue> {
    let scope = DedicatedWorkerGlobalScope::from(JsValue::from(js_sys::global()));
    let response: web_sys::Response = JsFuture::from(scope.fetch_with_str(url))
        .await?
        .dyn_into()?;
    let buffer = JsFuture::from(response.array_buffer()?).await?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

pub async fn extract_tar_gz(data: Vec<u8>) -> Result<mem_fs::FileSystem, std::io::Error> {
    let decoder = GzDecoder::new(Cursor::new(data));
    let mut archive = Archive::new(decoder);

    let fs = mem_fs::FileSystem::default();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let is_dir = entry.header().entry_type().is_dir();
        let abs_path = PathBuf::from(format!("/{}", entry.path()?.to_string_lossy()));

        if is_dir {
            // in the case that a file is listed before the parent directory
            create_dir_all(&fs, &abs_path).expect("Created directory");
        } else {
            if let Some(parent) = abs_path.parent() {
                create_dir_all(&fs, &parent).expect("Created parent directories");
            }
            let mut file = fs
                .new_open_options()
                .create(true)
                .write(true)
                .open(&abs_path)
                .expect("Created file");

            let mut contents = Vec::new();
            entry.read_to_end(&mut contents)?;
            file.write(&contents).await.expect("Wrote file");
        }
    }
    Ok(fs)
}

trait Ensure<T> {
    fn ensure(self, message: &str) -> Result<T, RuntimeError>;
}

impl<T, E: Debug> Ensure<T> for Result<T, E> {
    fn ensure(self, message: &str) -> Result<T, RuntimeError> {
        self.map_err(|e| RuntimeError::new(format!("{message}: {:?}", e)))
    }
}
