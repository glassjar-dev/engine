//! A read-only view of a filesystem. A worker unpacks the C++ sysroot once and every run
//! links it through this view, so nothing a run does (compiler or student program) can
//! change the files the next run sees: creating, truncating or appending is refused, and
//! other writes land in a private copy. Runs still get their own directories, so creating
//! or removing entries stays private to that run.

use futures::future::BoxFuture;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wasmer_wasix::virtual_fs::{
    CopyOnWriteFile, FileOpener, FileSystem, FsError, Metadata, OpenOptions, OpenOptionsConfig,
    ReadDir, Result, VirtualFile,
};

#[derive(Debug)]
pub struct ReadOnlyFs(pub Arc<dyn FileSystem + Send + Sync>);

impl FileSystem for ReadOnlyFs {
    fn readlink(&self, path: &Path) -> Result<PathBuf> {
        self.0.readlink(path)
    }

    fn read_dir(&self, path: &Path) -> Result<ReadDir> {
        self.0.read_dir(path)
    }

    fn create_dir(&self, _: &Path) -> Result<()> {
        Err(FsError::PermissionDenied)
    }

    fn remove_dir(&self, _: &Path) -> Result<()> {
        Err(FsError::PermissionDenied)
    }

    fn rename<'a>(&'a self, _: &'a Path, _: &'a Path) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(FsError::PermissionDenied) })
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        self.0.metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> Result<Metadata> {
        self.0.symlink_metadata(path)
    }

    fn remove_file(&self, _: &Path) -> Result<()> {
        Err(FsError::PermissionDenied)
    }

    fn new_open_options(&self) -> OpenOptions<'_> {
        OpenOptions::new(self)
    }

    fn mount(&self, _: String, _: &Path, _: Box<dyn FileSystem + Send + Sync>) -> Result<()> {
        Err(FsError::PermissionDenied)
    }
}

impl FileOpener for ReadOnlyFs {
    fn open(
        &self,
        path: &Path,
        conf: &OpenOptionsConfig,
    ) -> Result<Box<dyn VirtualFile + Send + Sync + 'static>> {
        if conf.append || conf.truncate || conf.create || conf.create_new {
            return Err(FsError::PermissionDenied);
        }
        // WASI asks for write rights even on plain reads (clang does), so a writable open
        // gets a copy-on-write handle: anything written stays in that handle.
        let read = OpenOptionsConfig { write: false, ..conf.clone() };
        let file = self.0.new_open_options().options(read).open(path)?;
        Ok(if conf.write { Box::new(CopyOnWriteFile::new(file)) } else { file })
    }
}
