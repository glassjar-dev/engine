//! What a program wrote, so a host shell can show the files it left behind.
//!
//! The snapshot is taken after the build and just before the program starts, so build
//! outputs never count. Only files in `/`, the folders the run was given, and top-level
//! folders the program itself created are walked: the language's own files (sysroots,
//! the standard library) are never read.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use wasmer_wasix::virtual_fs::{AsyncReadExt, FileSystem, mem_fs};

use crate::types::WorkerOut;

const MAX_FILES: usize = 64;
const MAX_FILE_BYTES: u64 = 1 << 20;
const MAX_TOTAL_BYTES: usize = 4 << 20;
/// Directory entries visited per walk, so a program that writes thousands of files
/// can't stall the report.
const MAX_ENTRIES: usize = 10_000;
/// Where runtimes keep their own files. Walked only if the run was given a folder by
/// that name.
const SYSTEM_ROOTS: &[&str] = &["bin", "dev", "etc", "include", "lib", "share", "tmp", "usr"];

pub(crate) struct Snapshot {
    /// Top-level folders that are not the student's: never walked.
    skip: HashSet<String>,
    files: HashMap<String, Vec<u8>>,
}

impl Snapshot {
    /// `fs` holds the student's files as the program will see them; `given` are the
    /// top-level names the run was given.
    pub async fn take(fs: &mem_fs::FileSystem, given: &HashSet<String>) -> Self {
        let mut skip: HashSet<String> = top_level(fs).into_iter().chain(SYSTEM_ROOTS.iter().map(|name| name.to_string())).collect();
        skip.retain(|name| !given.contains(name));
        let (paths, _) = walk(fs, &skip);
        let mut files = HashMap::new();
        for path in paths {
            if let Some(bytes) = read(fs, &path).await {
                files.insert(path, bytes);
            }
        }
        Self { skip, files }
    }

    /// Send `files`: what is new or changed under the same folders, and what disappeared.
    pub async fn report(mut self, fs: &mem_fs::FileSystem) {
        let (paths, mut truncated) = walk(fs, &self.skip);
        let mut written = Vec::new();
        let mut total = 0;
        for path in &paths {
            let before = self.files.remove(path);
            let Some(bytes) = read(fs, path).await else {
                truncated = true;
                continue;
            };
            if before.as_ref() == Some(&bytes) {
                continue;
            }
            if written.len() == MAX_FILES || total + bytes.len() > MAX_TOTAL_BYTES {
                truncated = true;
                continue;
            }
            total += bytes.len();
            written.push((path.clone(), serde_bytes::ByteBuf::from(bytes)));
        }
        let mut removed: Vec<String> = self.files.into_keys().collect();
        removed.sort();
        WorkerOut::Files { written, removed, truncated }.send();
    }
}

fn top_level(fs: &mem_fs::FileSystem) -> Vec<String> {
    let Ok(entries) = fs.read_dir(Path::new("/")) else { return Vec::new() };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.path().file_name().map(|name| name.to_string_lossy().into_owned()))
        .collect()
}

/// Regular files in `/` and under every top-level folder not in `skip`. Returns whether
/// the walk stopped early.
fn walk(fs: &mem_fs::FileSystem, skip: &HashSet<String>) -> (Vec<String>, bool) {
    let mut files = Vec::new();
    let mut pending = vec!["/".to_string()];
    let mut visited = 0;
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs.read_dir(Path::new(&directory)) else { continue };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_ENTRIES {
                return (files, true);
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_file() {
                files.push(path.to_string_lossy().into_owned());
            } else if kind.is_dir() {
                let skipped = directory == "/"
                    && path.file_name().is_some_and(|name| skip.contains(name.to_string_lossy().as_ref()));
                if !skipped {
                    pending.push(path.to_string_lossy().into_owned());
                }
            }
        }
    }
    (files, false)
}

async fn read(fs: &mem_fs::FileSystem, path: &str) -> Option<Vec<u8>> {
    if fs.metadata(Path::new(path)).ok()?.len > MAX_FILE_BYTES {
        return None;
    }
    let mut file = fs.new_open_options().read(true).open(path).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.ok()?;
    Some(bytes)
}
