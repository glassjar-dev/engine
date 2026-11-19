mod debuggee;

use instant::Instant;
use wasmer_wasix::virtual_fs::{AsyncWriteExt, FileSystem, create_dir_all, mem_fs};

use crate::types::{FsNode, WorkerOut, WorkerStart};
use crate::worker::execution::Execution;
use crate::worker::host_device::{HOST_DEVICE_PATH, HostDeviceFile};
use crate::worker::{Program, stop};

use debuggee::PythonDebuggee;

// Single source of truth for these is `prefetch_urls` in lib.rs (warmed by the host).
// glassjar-dev/python: CPython 3.15 for WASI with zlib, bz2, lzma, sqlite3 and C decimal.
pub(crate) const WASM_URL: &str = "https://cdn.yousefabuhashem.com/python/3.15.2-glassjar1/python.wasm";
pub(crate) const STDLIB_URL: &str = "https://cdn.yousefabuhashem.com/python/3.15.2-glassjar1/python-stdlib.tar.gz";

const SITE_PACKAGES: &str = "/usr/local/lib/python3.15/site-packages";
const SITECUSTOMIZE: &str = "import os\nif os.environ.get('GLASSJAR_CWD'):\n    os.chdir(os.environ['GLASSJAR_CWD'])\n";

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

async fn run(mut msg: WorkerStart, build_start: Instant) -> Result<WorkerOut<'static>, String> {
    let program = Program::new(std::mem::take(&mut msg.program), &msg.fs);
    let entry = program.start.entry.clone().unwrap_or_else(|| "/main.py".into());
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

    if fs.metadata(std::path::Path::new(&entry)).is_err() {
        return Err(format!("python: can't open file '{entry}': No such file or directory"));
    }
    // wasi-libc starts every program at `/`; Python imports sitecustomize at start-up,
    // before the script runs, so it can change to the directory the host asked for.
    if program.start.cwd.is_some() {
        create_dir_all(&fs, std::path::Path::new(SITE_PACKAGES))
            .map_err(|e| format!("Failed to prepare site-packages: {e}"))?;
        write_file(&fs, &format!("{SITE_PACKAGES}/sitecustomize.py"), SITECUSTOMIZE)
            .await
            .map_err(|e| format!("Failed to prepare the working directory: {e}"))?;
    }

    let exec = Execution::new(msg.stdin_buffer);

    let debuggee = msg.is_debug.then(PythonDebuggee::new);
    if let Some(debuggee) = &debuggee {
        write_file(&fs, "/_bridge.py", include_str!("bridge.py"))
            .await
            .map_err(|e| format!("Failed to write the debug bridge: {e}"))?;
        debuggee.send_and_wait();
    }

    // The student's files as the program will see them, before it runs.
    let snapshot = program.snapshot(&fs).await;
    let mut step = exec
        .step("python")
        .binary(WASM_URL)
        .sysroot(STDLIB_URL)
        .fs(Box::new(fs))
        .args(if msg.is_debug { vec!["/_bridge.py", entry.as_str()] } else { vec![entry.as_str()] })
        .program(&program.start)
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
    if let Some(snapshot) = snapshot {
        snapshot.report(&exec.fs).await;
    }
    Ok(stop(exit.raw(), build_start, Some(run_start)))
}
