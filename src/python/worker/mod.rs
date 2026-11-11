mod debuggee;

use instant::Instant;
use wasmer_wasix::virtual_fs::{AsyncWriteExt, FileSystem, mem_fs};

use crate::types::{FsNode, WorkerOut, WorkerStart};
use crate::worker::execution::Execution;
use crate::worker::host_device::{HOST_DEVICE_PATH, HostDeviceFile};
use crate::worker::stop;

use debuggee::PythonDebuggee;

// Single source of truth for these is `prefetch_urls` in lib.rs (warmed by the host).
// glassjar-dev/python: CPython 3.15 for WASI with zlib, bz2, lzma, sqlite3 and C decimal.
pub(crate) const WASM_URL: &str = "https://cdn.yousefabuhashem.com/python/3.15.2-glassjar1/python.wasm";
pub(crate) const STDLIB_URL: &str = "https://cdn.yousefabuhashem.com/python/3.15.2-glassjar1/python-stdlib.tar.gz";

async fn write_file(fs: &mem_fs::FileSystem, path: &str, contents: &str) -> std::io::Result<()> {
    let mut file = fs
        .new_open_options()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(contents.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

pub async fn start(msg: WorkerStart) {
    // Report setup/runtime failures as a clean error instead of panicking, so the
    // host shows a message rather than a Rust backtrace. A non-zero exit code is
    // the program's own result, not an error.
    let build_start = Instant::now();
    match run(msg, build_start).await {
        Ok(stop) => stop.send(),
        Err(message) => WorkerOut::Error { message }.send(),
    }
}

async fn run(msg: WorkerStart, build_start: Instant) -> Result<WorkerOut<'static>, String> {
    let host_device = msg
        .host_device
        .map(HostDeviceFile::new)
        .transpose()
        .map_err(|e| format!("Invalid host device: {e}"))?;
    let fs = crate::worker::create_user_fs(FsNode::Dir(msg.fs))
        .await
        .map_err(|e| format!("Failed to prepare the filesystem: {e}"))?;
    // Installed packages and data files, under the paths the host chose.
    crate::worker::mount_binary_files(&fs, &msg.binary_files).await?;

    let exec = Execution::new(msg.stdin_buffer);

    let debuggee = msg.is_debug.then(PythonDebuggee::new);
    if let Some(debuggee) = &debuggee {
        write_file(&fs, "/_bridge.py", include_str!("bridge.py"))
            .await
            .map_err(|e| format!("Failed to write the debug bridge: {e}"))?;
        debuggee.send_and_wait();
    }

    let mut step = exec
        .step("python")
        .binary(WASM_URL)
        .sysroot(STDLIB_URL)
        .fs(Box::new(fs))
        .args(if msg.is_debug { &["/_bridge.py"][..] } else { &["/main.py"][..] })
        .envs([
            ("PYTHONUNBUFFERED", "1"),
            ("PYTHONDONTWRITEBYTECODE", "1"),
            // WASI has no dup() (pytest's fd capture, faulthandler) or umask() (its cache,
            // which a fresh filesystem per run would lose anyway).
            ("PYTEST_ADDOPTS", "--capture=sys -p no:faulthandler -p no:cacheprovider"),
        ]);

    if let Some(debuggee) = debuggee {
        step = step.device_file("/__debug__", Box::new(debuggee.debug_file()));
    }
    if let Some(device) = host_device {
        step = step.device_file(HOST_DEVICE_PATH, Box::new(device));
    }

    let run_start = Instant::now();
    let exit = step
        .run()
        .await
        .map_err(|e| format!("Failed to run Python (could not load the runtime?): {e}"))?;
    Ok(stop(exit.raw(), build_start, Some(run_start)))
}
