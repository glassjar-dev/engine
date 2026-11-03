//! Observation-only WASI adapter for the immutable stock compiler profile.
//! Unknown imports/accesses disable cache admission; the original syscall is
//! still called exactly once with unchanged arguments and return values. The
//! original proc_exit import preserves typed exits; other traps fail closed.
use super::cpp_stock_trace::{MAX_PATH, Trace};
use std::sync::{Arc, Mutex};
use wasmer::{
    Extern, Function, FunctionEnv, FunctionEnvMut, Imports, Instance, Memory, Module, Store, Value,
};

pub type InvocationTrace = Arc<Mutex<Trace>>;
pub struct TraceImport {
    memory: Option<Memory>,
    trace: InvocationTrace,
}
struct Wrapper {
    shared: FunctionEnv<TraceImport>,
    original: Function,
    name: &'static str,
}
struct Pending {
    path: Option<String>,
    second_path: Option<String>,
}

fn syscall_name(name: &str) -> &'static str {
    const NAMES: &[&str] = &[
        "args_get",
        "args_sizes_get",
        "environ_get",
        "environ_sizes_get",
        "clock_time_get",
        "random_get",
        "fd_prestat_get",
        "fd_prestat_dir_name",
        "path_open",
        "path_filestat_get",
        "fd_filestat_get",
        "path_readlink",
        "path_rename",
        "fd_close",
        "fd_read",
        "fd_pread",
        "fd_write",
        "fd_seek",
        "fd_fdstat_get",
    ];
    NAMES
        .iter()
        .copied()
        .find(|value| *value == name)
        .unwrap_or("unsupported")
}

fn arg(args: &[Value], index: usize) -> Option<u32> {
    args.get(index)?.i32().map(|v| v as u32)
}
fn wide(args: &[Value], index: usize) -> Option<u64> {
    args.get(index)?.i64().map(|v| v as u64)
}
fn read(
    memory: &Memory,
    env: &FunctionEnvMut<Wrapper>,
    pointer: u32,
    length: usize,
) -> Option<Vec<u8>> {
    if length > MAX_PATH {
        return None;
    }
    let view = memory.view(env);
    let end = u64::from(pointer).checked_add(length as u64)?;
    if end > view.data_size() {
        return None;
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).ok()?;
    bytes.resize(length, 0);
    view.read(u64::from(pointer), &mut bytes).ok()?;
    Some(bytes)
}
fn string(
    memory: &Memory,
    env: &FunctionEnvMut<Wrapper>,
    args: &[Value],
    pointer: usize,
    length: usize,
) -> Option<String> {
    String::from_utf8(read(
        memory,
        env,
        arg(args, pointer)?,
        arg(args, length)? as usize,
    )?)
    .ok()
}
fn scalar(memory: &Memory, env: &FunctionEnvMut<Wrapper>, pointer: u32) -> Option<u32> {
    let mut bytes = [0u8; 4];
    memory.view(env).read(u64::from(pointer), &mut bytes).ok()?;
    Some(u32::from_le_bytes(bytes))
}
fn filetype(memory: &Memory, env: &FunctionEnvMut<Wrapper>, pointer: u32) -> Option<u8> {
    let mut bytes = [0u8; 1];
    memory
        .view(env)
        .read(u64::from(pointer.checked_add(16)?), &mut bytes)
        .ok()?;
    Some(bytes[0])
}
fn before(
    trace: &mut Trace,
    memory: Option<&Memory>,
    env: &FunctionEnvMut<Wrapper>,
    name: &str,
    args: &[Value],
) -> Pending {
    let mut pending = Pending {
        path: None,
        second_path: None,
    };
    if !trace.begin() {
        return pending;
    }
    let descriptor = match name {
        "path_open" | "path_filestat_get" | "path_link" => Some((0, 2, 3)),
        "path_create_directory"
        | "path_readlink"
        | "path_remove_directory"
        | "path_rename"
        | "path_unlink_file" => Some((0, 1, 2)),
        "path_symlink" => Some((2, 3, 4)),
        _ => None,
    };
    if let Some((fd, pointer, length)) = descriptor {
        pending.path = memory
            .and_then(|m| string(m, env, args, pointer, length))
            .and_then(|s| arg(args, fd).and_then(|fd| trace.resolve(fd, &s)));
        if pending.path.is_none() {
            trace.decline("stock-trace-memory-or-path");
        }
    }
    if name == "path_rename" {
        pending.second_path = memory
            .and_then(|m| string(m, env, args, 4, 5))
            .and_then(|s| arg(args, 3).and_then(|fd| trace.resolve(fd, &s)));
        if pending.second_path.is_none() {
            trace.decline("stock-trace-memory-or-path");
        }
    }
    pending
}
fn after(
    trace: &mut Trace,
    memory: Option<&Memory>,
    env: &FunctionEnvMut<Wrapper>,
    name: &str,
    args: &[Value],
    result: &[Value],
    pending: Pending,
) {
    if !trace.active() {
        return;
    }
    let Some(errno) = result.first().and_then(Value::i32) else {
        trace.decline("stock-trace-signature");
        return;
    };
    let fd = arg(args, 0).unwrap_or(u32::MAX);
    let path = pending.path.as_deref().unwrap_or("");
    match name {
        "args_get" | "args_sizes_get" | "environ_get" | "environ_sizes_get" => {
            if errno != 0 {
                trace.decline("stock-environment-protocol");
            }
        }
        "clock_time_get" => trace.clock(),
        "random_get" => trace.random(arg(args, 1).unwrap_or(u32::MAX), errno),
        "fd_prestat_get" => {
            if ![0, 8].contains(&errno) {
                trace.decline("stock-preopen");
            }
        }
        "fd_prestat_dir_name" => {
            let text = memory.and_then(|m| string(m, env, args, 1, 2));
            if errno == 0 {
                if let Some(text) = text {
                    trace.preopen(fd, &text);
                } else {
                    trace.decline("stock-trace-memory-or-path");
                }
            } else {
                trace.decline("stock-preopen");
            }
        }
        "path_open" => {
            let opened = if errno == 0 {
                memory.and_then(|m| scalar(m, env, arg(args, 8)?))
            } else {
                None
            };
            trace.open(
                path,
                arg(args, 4).unwrap_or(u32::MAX),
                wide(args, 5).unwrap_or(u64::MAX),
                arg(args, 7).unwrap_or(u32::MAX),
                errno,
                opened,
            );
        }
        "path_filestat_get" => trace.path_stat(
            path,
            errno,
            memory.and_then(|m| filetype(m, env, arg(args, 4)?)),
        ),
        "fd_filestat_get" => trace.fd_stat(
            fd,
            errno,
            memory.and_then(|m| filetype(m, env, arg(args, 1)?)),
        ),
        "path_readlink" => trace.readlink(path, errno),
        "path_rename" => trace.rename(path, pending.second_path.as_deref().unwrap_or(""), errno),
        "fd_close" => trace.close(fd, errno),
        "fd_read" | "fd_pread" => {
            trace.read(fd);
            if errno != 0 {
                trace.decline("stock-read-failure");
            }
        }
        "fd_write" => {
            trace.write(fd);
            if errno != 0 {
                trace.decline("stock-write-failure");
            }
        }
        "fd_seek" => trace.seek(
            fd,
            args.get(1).and_then(Value::i64).unwrap_or(i64::MIN),
            arg(args, 2).unwrap_or(u32::MAX),
            errno,
        ),
        "fd_fdstat_get" => {
            trace.descriptor(fd);
            if errno != 0 {
                trace.decline("stock-descriptor-protocol");
            }
        }
        // This includes directory enumeration, clock-based polling, links,
        // metadata mutation, append locking and unrecognized future imports.
        _ => trace.decline("stock-unsupported-syscall"),
    }
}
impl TraceImport {
    pub fn attach(
        store: &mut Store,
        imports: &mut Imports,
        module: &Module,
        trace: InvocationTrace,
    ) -> FunctionEnv<Self> {
        let shared = FunctionEnv::new(
            store,
            Self {
                memory: None,
                trace: trace.clone(),
            },
        );
        for import in module.imports() {
            if import.module() != "wasi_snapshot_preview1" {
                if let Ok(mut trace) = trace.lock() {
                    trace.decline("stock-import-namespace");
                }
                continue;
            }
            let Some(Extern::Function(original)) =
                imports.get_export(import.module(), import.name())
            else {
                if let Ok(mut trace) = trace.lock() {
                    trace.decline("stock-import-type");
                }
                continue;
            };
            // Keep Wasmer's original typed exit trap intact. proc_exit observes
            // no cache inputs; Step::run owns its exact success/error status.
            // Calling it through another JS Function boundary erases WasiError.
            if import.name() == "proc_exit" {
                continue;
            }
            let ty = original.ty(store);
            let wrapper = FunctionEnv::new(
                store,
                Wrapper {
                    shared: shared.clone(),
                    original,
                    name: syscall_name(import.name()),
                },
            );
            let function = Function::new_with_env(
                store,
                &wrapper,
                ty,
                |mut env: FunctionEnvMut<Wrapper>, args| {
                    // Clone handles only. Never hold environment or capture borrows
                    // while invoking the original function, which enters WASI.
                    let original = env.data().original.clone();
                    let name = env.data().name;
                    let shared = env.data().shared.clone();
                    let capture = shared.as_ref(&env).trace.clone();
                    let memory = shared.as_ref(&env).memory.clone();
                    let pending = capture
                        .lock()
                        .ok()
                        .map(|mut trace| before(&mut trace, memory.as_ref(), &env, name, args));
                    let outcome = original.call(&mut env, args);
                    match outcome {
                        Ok(values) => {
                            if let (Some(pending), Ok(mut trace)) = (pending, capture.lock()) {
                                after(
                                    &mut trace,
                                    memory.as_ref(),
                                    &env,
                                    name,
                                    args,
                                    &values,
                                    pending,
                                );
                            }
                            Ok(values.into_vec())
                        }
                        Err(error) => {
                            if let Ok(mut trace) = capture.lock() {
                                trace.decline("stock-syscall-trap");
                            }
                            Err(error)
                        }
                    }
                },
            );
            imports.define(import.module(), import.name(), function);
        }
        shared
    }
    pub fn bind(environment: &FunctionEnv<Self>, store: &mut Store, instance: &Instance) {
        environment.as_mut(store).memory = instance.exports.get_memory("memory").ok().cloned();
    }
}
