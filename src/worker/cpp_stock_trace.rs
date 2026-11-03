//! Bounded observation of a fixed stock compiler invocation. Syscall results
//! are unchanged. This trace is one admission condition, never authority.
use std::collections::BTreeSet;
use std::sync::Arc;

pub const MAX_PATH: usize = 4096;
// The unchanged MarbleGUI TU performs 69,776 calls, mostly object writes/seeks.
// No event history is retained; this bound limits observation work, not storage.
const MAX_CALLS: usize = 262144;
const MAX_PATHS: usize = 16384;
const MAX_PATH_BYTES: usize = 1024 * 1024;
const MAX_FDS: usize = 256;

fn copy_string(value: &str) -> Option<String> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len()).ok()?;
    copy.push_str(value);
    Some(copy)
}
fn append_path(paths: &mut Vec<String>, path: &str, bytes: &mut usize) -> Option<()> {
    if paths.len() >= MAX_PATHS {
        return None;
    }
    *bytes = bytes
        .checked_add(path.len())
        .filter(|n| *n <= MAX_PATH_BYTES)?;
    let copy = copy_string(path)?;
    paths.try_reserve(1).ok()?;
    paths.push(copy);
    Some(())
}
#[derive(Debug)]
pub struct Namespace {
    pub files: Vec<String>,
    pub directories: Vec<String>,
    selected_sources: Vec<usize>,
    valid: bool,
}
impl Namespace {
    /// Fallibly copy the exact effective namespace already used by the key.
    /// No content bytes, mutable outputs, or caller-owned buffers are retained.
    pub fn try_new<'a>(
        files: impl IntoIterator<Item = &'a str>,
        selected_sources: impl IntoIterator<Item = &'a str>,
    ) -> Option<Self> {
        let mut paths = Vec::new();
        let mut bytes = 0;
        for path in files {
            if path.len() > MAX_PATH
                || !path.starts_with('/')
                || path == "/"
                || path.chars().any(|c| c.is_control() || c == '\\')
                || path[1..]
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == "..")
            {
                return None;
            }
            append_path(&mut paths, path, &mut bytes)?;
        }
        paths.sort_unstable();
        paths.dedup();
        let mut directories = Vec::new();
        let mut directory_bytes = 0;
        append_path(&mut directories, "/", &mut directory_bytes)?;
        for path in &paths {
            for (at, _) in path.match_indices('/').skip(1) {
                let parent = &path[..at];
                if let Err(index) = directories.binary_search_by(|p| p.as_str().cmp(parent)) {
                    append_path(&mut directories, parent, &mut directory_bytes)?;
                    // Rotate the newly allocated entry into its sorted position.
                    directories[index..].rotate_right(1);
                }
            }
        }
        let mut sources = Vec::new();
        for source in selected_sources {
            if sources.len() >= 1024 {
                return None;
            }
            let index = paths.binary_search_by(|p| p.as_str().cmp(source)).ok()?;
            sources.try_reserve(1).ok()?;
            sources.push(index);
        }
        sources.sort_unstable();
        sources.dedup();
        Some(Self {
            files: paths,
            directories,
            selected_sources: sources,
            valid: true,
        })
    }
    // Test convenience; production uses try_new and treats None as a bypass.
    pub fn new(files: BTreeSet<String>, selected_sources: BTreeSet<String>) -> Self {
        Self::try_new(
            files.iter().map(String::as_str),
            selected_sources.iter().map(String::as_str),
        )
        .unwrap_or(Self {
            files: Vec::new(),
            directories: Vec::new(),
            selected_sources: Vec::new(),
            valid: false,
        })
    }
    fn file(&self, path: &str) -> Option<usize> {
        self.files.binary_search_by(|p| p.as_str().cmp(path)).ok()
    }
    fn directory(&self, path: &str) -> Option<usize> {
        self.directories
            .binary_search_by(|p| p.as_str().cmp(path))
            .ok()
    }
    fn selected(&self, index: usize) -> bool {
        self.selected_sources.binary_search(&index).is_ok()
    }
    fn output_for(source: &str, path: &str) -> bool {
        path.strip_prefix(source) == Some(".o")
    }
    fn temporary_for(source: &str, path: &str) -> bool {
        path.strip_prefix(source)
            .and_then(|s| s.strip_prefix('-'))
            .and_then(|s| s.strip_suffix(".o.tmp"))
            .is_some_and(|s| s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    }
    pub fn generated(&self, path: &str) -> bool {
        self.selected_sources.iter().any(|&i| {
            Self::output_for(&self.files[i], path) || Self::temporary_for(&self.files[i], path)
        })
    }
}
#[derive(Debug, Clone, Copy)]
enum Owner {
    Stdin,
    Stdout,
    Stderr,
    Directory(usize),
    Input(usize),
    Output,
}
#[derive(Debug, Default, Clone)]
pub struct Observation {
    pub input_paths: Vec<String>,
    pub bypass: Option<&'static str>,
    pub clock_calls: u32,
    pub random_calls: u32,
}
#[derive(Debug)]
pub struct Trace {
    namespace: Arc<Namespace>,
    source: Option<usize>,
    fds: Vec<(u32, Owner)>,
    inputs: Vec<bool>,
    calls: usize,
    bypass: Option<&'static str>,
    clock_calls: u32,
    random_calls: u32,
    guard_seen: bool,
    output_checked: bool,
    output_random: u32,
    temporary: Option<String>,
    output_renamed: bool,
}
impl Trace {
    pub fn new(namespace: Arc<Namespace>, source: String) -> Self {
        Self::from_source(namespace, &source)
    }
    pub fn from_source(namespace: Arc<Namespace>, source: &str) -> Self {
        let source = namespace.file(source).filter(|&i| namespace.selected(i));
        let collision = namespace.files.iter().any(|p| namespace.generated(p));
        let mut bypass = if !namespace.valid || source.is_none() {
            Some("stock-namespace-budget-or-path")
        } else {
            collision.then_some("stock-output-collision")
        };
        let mut fds = Vec::new();
        let mut inputs = Vec::new();
        if fds.try_reserve_exact(MAX_FDS).is_err()
            || inputs.try_reserve_exact(namespace.files.len()).is_err()
        {
            bypass = Some("stock-trace-allocation");
        } else {
            fds.extend([(0, Owner::Stdin), (1, Owner::Stdout), (2, Owner::Stderr)]);
            inputs.resize(namespace.files.len(), false);
        }
        Self {
            namespace,
            source,
            fds,
            inputs,
            calls: 0,
            bypass,
            clock_calls: 0,
            random_calls: 0,
            guard_seen: false,
            output_checked: false,
            output_random: 0,
            temporary: None,
            output_renamed: false,
        }
    }
    pub fn decline(&mut self, reason: &'static str) {
        self.bypass.get_or_insert(reason);
    }
    pub fn begin(&mut self) -> bool {
        self.calls = self.calls.saturating_add(1);
        if self.calls > MAX_CALLS {
            self.decline("stock-trace-budget");
        }
        self.active()
    }
    pub fn active(&self) -> bool {
        self.bypass.is_none()
    }
    fn source_path(&self) -> Option<&str> {
        self.source.map(|i| self.namespace.files[i].as_str())
    }
    pub fn resolve(&self, fd: u32, spelling: &str) -> Option<String> {
        if spelling.len() > MAX_PATH || spelling.chars().any(|c| c.is_control() || c == '\\') {
            return None;
        }
        let base = if spelling.starts_with('/') {
            "/"
        } else {
            match self.fds.iter().find(|(id, _)| *id == fd)?.1 {
                Owner::Directory(index) => &self.namespace.directories[index],
                _ => return None,
            }
        };
        // Fixed stack scratch and component offsets avoid per-component heap work.
        let mut buffer = [0u8; MAX_PATH];
        let mut ends = [0usize; MAX_PATH / 2 + 1];
        let mut count = 0;
        let mut length = 1;
        buffer[0] = b'/';
        for part in base.split('/').chain(spelling.split('/')) {
            match part {
                "" | "." => (),
                ".." => {
                    if count == 0 {
                        return None;
                    }
                    count -= 1;
                    length = ends[count];
                }
                _ => {
                    let old = length;
                    let separator = usize::from(length > 1);
                    let end = length.checked_add(separator)?.checked_add(part.len())?;
                    if end > MAX_PATH || count >= ends.len() {
                        return None;
                    }
                    if separator == 1 {
                        buffer[length] = b'/';
                        length += 1;
                    }
                    buffer[length..end].copy_from_slice(part.as_bytes());
                    length = end;
                    ends[count] = old;
                    count += 1;
                }
            }
        }
        copy_string(std::str::from_utf8(&buffer[..length]).ok()?)
    }
    fn input(&mut self, path: &str) {
        if self.namespace.generated(path) {
            self.decline("stock-mutable-input");
            return;
        }
        let Some(index) = self.namespace.file(path) else {
            self.decline("stock-unrepresented-input");
            return;
        };
        if self.namespace.selected(index) && Some(index) != self.source {
            self.decline("stock-mutable-input");
            return;
        }
        if let Some(input) = self.inputs.get_mut(index) {
            *input = true;
        } else {
            self.decline("stock-trace-allocation");
        }
    }
    fn owner(&mut self, fd: u32) -> Option<Owner> {
        let owner = self.fds.iter().find(|(id, _)| *id == fd).map(|(_, o)| *o);
        if owner.is_none() {
            self.decline("stock-unknown-descriptor");
        }
        owner
    }
    fn insert_fd(&mut self, fd: u32, owner: Owner) {
        if self.fds.len() >= MAX_FDS
            || self.fds.iter().any(|(id, _)| *id == fd)
            || self.fds.len() == self.fds.capacity()
        {
            self.decline("stock-descriptor-budget");
        } else {
            self.fds.push((fd, owner));
        }
    }
    pub fn preopen(&mut self, fd: u32, path: &str) {
        // Pinned Wasmer advertises name.len()+1 and writes a terminal NUL.
        // Accept only its exact root encoding, never trim arbitrary names.
        if path != "/" && path != "/\0" {
            self.decline("stock-preopen");
        } else if let Some(index) = self.namespace.directory("/") {
            self.insert_fd(fd, Owner::Directory(index));
        } else {
            self.decline("stock-preopen");
        }
    }
    pub fn clock(&mut self) {
        self.clock_calls = self.clock_calls.saturating_add(1);
        self.decline("stock-clock-input");
    }
    pub fn random(&mut self, length: u32, errno: i32) {
        self.random_calls = self.random_calls.saturating_add(1);
        // These phases supplement the source/binary consumer audit. Counts alone
        // never grant admission. Random values are never modified or replayed.
        if errno != 0 || length != 4 {
            self.decline("stock-entropy-protocol");
        } else if !self.guard_seen && !self.output_checked && !self.inputs.iter().any(|b| *b) {
            self.guard_seen = true;
        } else if self.guard_seen
            && self.output_checked
            && self.temporary.is_none()
            && self.output_random < 8
        {
            self.output_random += 1;
        } else {
            self.decline("stock-entropy-protocol");
        }
    }
    pub fn path_stat(&mut self, path: &str, errno: i32, filetype: Option<u8>) {
        if self
            .source_path()
            .is_some_and(|s| Namespace::output_for(s, path))
            && !self.output_checked
            && self.temporary.is_none()
            && errno == 44
        {
            // Fixed output creator only. Lexical policy separately excludes all
            // user observations of generated output names, including this one.
            self.output_checked = true;
            return;
        }
        if self.namespace.generated(path) {
            self.decline("stock-generated-probe");
            return;
        }
        match errno {
            0 if filetype == Some(3) && self.namespace.directory(path).is_some() => (),
            0 if filetype == Some(4) => self.input(path),
            44 | 54
                if self.namespace.file(path).is_none()
                    && self.namespace.directory(path).is_none() =>
            {
                ()
            }
            _ => self.decline("stock-stat-closure"),
        }
    }
    pub fn open(
        &mut self,
        path: &str,
        oflags: u32,
        rights: u64,
        fdflags: u32,
        errno: i32,
        opened: Option<u32>,
    ) {
        if self.namespace.generated(path) {
            let expected = self
                .source_path()
                .is_some_and(|s| Namespace::temporary_for(s, path));
            if !expected
                || !self.output_checked
                || self.output_random != 8
                || self.temporary.is_some()
                || oflags != 5
                || fdflags != 0
                || rights & 64 == 0
                || errno != 0
            {
                self.decline("stock-output-protocol");
                return;
            }
            let Some(fd) = opened else {
                self.decline("stock-output-protocol");
                return;
            };
            let Some(copy) = copy_string(path) else {
                self.decline("stock-trace-allocation");
                return;
            };
            self.temporary = Some(copy);
            self.insert_fd(fd, Owner::Output);
            return;
        }
        if oflags != 0 || fdflags != 0 || rights & 64 != 0 {
            self.decline("stock-input-write");
            return;
        }
        if errno == 44 || errno == 54 {
            if self.namespace.file(path).is_some() || self.namespace.directory(path).is_some() {
                self.decline("stock-open-closure");
            }
            return;
        }
        if errno != 0 {
            self.decline("stock-open-closure");
            return;
        }
        self.input(path);
        let (Some(fd), Some(index)) = (opened, self.namespace.file(path)) else {
            self.decline("stock-open-closure");
            return;
        };
        self.insert_fd(fd, Owner::Input(index));
    }
    pub fn fd_stat(&mut self, fd: u32, errno: i32, filetype: Option<u8>) {
        if errno != 0 {
            self.decline("stock-stat-closure");
            return;
        }
        match self.owner(fd) {
            Some(Owner::Input(index)) if filetype == Some(4) => {
                if let Some(input) = self.inputs.get_mut(index) {
                    *input = true;
                } else {
                    self.decline("stock-trace-allocation");
                }
            }
            Some(Owner::Directory(_)) if filetype == Some(3) => (),
            Some(Owner::Output) if filetype == Some(4) && self.temporary.is_some() => (),
            Some(Owner::Stdin | Owner::Stdout | Owner::Stderr) => (),
            _ => self.decline("stock-stat-closure"),
        }
    }
    pub fn read(&mut self, fd: u32) {
        match self.owner(fd) {
            Some(Owner::Input(index)) => {
                if let Some(input) = self.inputs.get_mut(index) {
                    *input = true;
                } else {
                    self.decline("stock-trace-allocation");
                }
            }
            _ => self.decline("stock-unowned-read"),
        }
    }
    pub fn write(&mut self, fd: u32) {
        match self.owner(fd) {
            Some(Owner::Output | Owner::Stdout | Owner::Stderr) => (),
            _ => self.decline("stock-unowned-write"),
        }
    }
    pub fn descriptor(&mut self, fd: u32) {
        self.owner(fd);
    }
    pub fn seek(&mut self, fd: u32, offset: i64, whence: u32, errno: i32) {
        let owner = self.owner(fd);
        if errno == 0 {
            return;
        }
        // LLVM raw_fd_ostream probes outs()/errs() with lseek(fd,0,SEEK_CUR).
        // This engine's original stdout/stderr lack FD_SEEK; pinned WASI
        // returns Access before mutation. Only these exact original stream
        // queries are fixed bookkeeping, never an allowance for reopened FDs.
        let original_output = fd == 1 && matches!(owner, Some(Owner::Stdout))
            || fd == 2 && matches!(owner, Some(Owner::Stderr));
        if original_output && offset == 0 && whence == 1 && errno == 2 {
            return;
        }
        self.decline("stock-descriptor-protocol");
    }
    pub fn close(&mut self, fd: u32, errno: i32) {
        if errno != 0 {
            self.decline("stock-close-protocol");
            return;
        }
        if let Some(index) = self.fds.iter().position(|(id, _)| *id == fd) {
            self.fds.swap_remove(index);
        } else {
            self.decline("stock-close-protocol");
        }
    }
    pub fn rename(&mut self, from: &str, to: &str, errno: i32) {
        if errno != 0
            || self.output_renamed
            || self.temporary.as_deref() != Some(from)
            || !self
                .source_path()
                .is_some_and(|s| Namespace::output_for(s, to))
        {
            self.decline("stock-output-protocol");
            return;
        }
        self.output_renamed = true;
    }
    pub fn readlink(&mut self, path: &str, errno: i32) {
        if self.namespace.generated(path) {
            self.decline("stock-generated-probe");
            return;
        }
        match errno {
            28 | 52 if self.namespace.file(path).is_some() => self.input(path),
            // realpath probes every represented parent. Known non-symlink
            // directories are existence inputs, not content dependencies.
            28 | 52 if self.namespace.directory(path).is_some() => (),
            44 | 54
                if self.namespace.file(path).is_none()
                    && self.namespace.directory(path).is_none() =>
            {
                ()
            }
            _ => self.decline("stock-symlink-input"),
        }
    }
    pub fn finish(&self) -> Observation {
        let incomplete = !self.guard_seen
            || !self.output_renamed
            || !self
                .source
                .and_then(|i| self.inputs.get(i))
                .copied()
                .unwrap_or(false);
        let mut result = Observation {
            input_paths: Vec::new(),
            bypass: self
                .bypass
                .or(incomplete.then_some("stock-incomplete-trace")),
            clock_calls: self.clock_calls,
            random_calls: self.random_calls,
        };
        if result.bypass.is_some() {
            return result;
        }
        let count = self.inputs.iter().filter(|b| **b).count();
        if result.input_paths.try_reserve_exact(count).is_err() {
            result.bypass = Some("stock-trace-allocation");
            return result;
        }
        for (index, present) in self.inputs.iter().enumerate() {
            if *present {
                let Some(copy) = copy_string(&self.namespace.files[index]) else {
                    result.input_paths.clear();
                    result.bypass = Some("stock-trace-allocation");
                    return result;
                };
                result.input_paths.push(copy);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> Trace {
        let files = ["/main.cpp", "/other.cpp", "/include/header.h"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let sources = ["/main.cpp", "/other.cpp"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        Trace::new(Arc::new(Namespace::new(files, sources)), "/main.cpp".into())
    }
    fn begin_input(trace: &mut Trace) {
        trace.random(4, 0);
        trace.preopen(3, "/");
        trace.path_stat("/include", 0, Some(3));
        trace.path_stat("/missing.h", 44, None);
        trace.open("/main.cpp", 0, 2, 0, 0, Some(4));
        trace.readlink("/main.cpp", 28);
        trace.fd_stat(4, 0, Some(4));
        trace.read(4);
        trace.close(4, 0);
    }
    fn output(trace: &mut Trace) {
        trace.path_stat("/main.cpp.o", 44, None);
        for _ in 0..8 {
            trace.random(4, 0);
        }
        trace.open("/main.cpp-a12bc789.o.tmp", 5, 66, 0, 0, Some(4));
        trace.fd_stat(4, 0, Some(4));
        trace.write(4);
        trace.close(4, 0);
        trace.rename("/main.cpp-a12bc789.o.tmp", "/main.cpp.o", 0);
    }
    #[test]
    fn normal_fixed_output_protocol_records_only_immutable_inputs() {
        let mut t = setup();
        begin_input(&mut t);
        t.open("/include/header.h", 0, 2, 0, 0, Some(5));
        t.read(5);
        t.close(5, 0);
        output(&mut t);
        let result = t.finish();
        assert_eq!(result.bypass, None);
        assert_eq!(result.random_calls, 9);
        assert_eq!(result.input_paths, ["/include/header.h", "/main.cpp"]);
    }
    #[test]
    fn only_exact_root_preopen_transport_encodings_are_accepted() {
        for spelling in ["/", "/\0"] {
            let mut t = setup();
            t.preopen(3, spelling);
            assert!(t.active());
            assert_eq!(t.resolve(3, "main.cpp"), Some("/main.cpp".into()));
        }
        for spelling in [
            "",
            ".",
            "./",
            "//",
            "/other",
            "/\0suffix",
            "/\0\0",
            "//\0",
            "/other\0",
            "\0/",
        ] {
            let mut t = setup();
            t.preopen(3, spelling);
            assert_eq!(t.finish().bypass, Some("stock-preopen"), "{spelling:?}");
        }
    }
    #[test]
    fn original_output_stream_seek_queries_preserve_fixed_access_denial_only() {
        for fd in [1, 2] {
            let mut t = setup();
            begin_input(&mut t);
            t.seek(fd, 0, 1, 2);
            output(&mut t);
            assert_eq!(t.finish().bypass, None);
        }
        for (fd, offset, whence, errno) in [
            (2, 1, 1, 2),
            (2, -1, 1, 2),
            (2, 0, 0, 2),
            (2, 0, 2, 2),
            (1, 1, 1, 2),
            (1, -1, 1, 2),
            (1, 0, 0, 2),
            (1, 0, 2, 2),
            (1, 0, 1, 8),
            (1, 0, 1, 28),
            (0, 0, 1, 2),
            (2, 0, 1, 8),
            (2, 0, 1, 28),
            (99, 0, 1, 0),
        ] {
            let mut t = setup();
            begin_input(&mut t);
            t.seek(fd, offset, whence, errno);
            output(&mut t);
            assert!(
                t.finish().bypass.is_some(),
                "{fd}/{offset}/{whence}/{errno}"
            );
        }
        let mut t = setup();
        begin_input(&mut t);
        t.open("/include/header.h", 0, 2, 0, 0, Some(4));
        t.seek(4, 0, 1, 2);
        output(&mut t);
        assert_eq!(t.finish().bypass, Some("stock-descriptor-protocol"));
        for fd in [1, 2] {
            let mut t = setup();
            begin_input(&mut t);
            t.close(fd, 0);
            t.open("/include/header.h", 0, 2, 0, 0, Some(fd));
            t.seek(fd, 0, 1, 2);
            output(&mut t);
            assert_eq!(t.finish().bypass, Some("stock-descriptor-protocol"));
        }
        let mut t = setup();
        begin_input(&mut t);
        t.open("/include/header.h", 0, 2, 0, 0, Some(4));
        t.seek(4, 0, 1, 0);
        t.close(4, 0);
        output(&mut t);
        assert_eq!(t.finish().bypass, None);
    }
    #[test]
    fn normalized_missing_paths_are_bounded_and_cannot_escape_root() {
        let mut t = setup();
        t.preopen(3, "/");
        assert_eq!(
            t.resolve(3, "include/../main.cpp"),
            Some("/main.cpp".into())
        );
        assert_eq!(t.resolve(3, "../../main.cpp"), None);
        assert_eq!(t.resolve(9, "main.cpp"), None);
        assert_eq!(t.resolve(3, "foo\0bar"), None);
        assert_eq!(t.resolve(3, &"a".repeat(MAX_PATH + 1)), None);
    }
    #[test]
    fn all_selected_output_families_and_observations_are_excluded() {
        for path in [
            "/other.cpp.o",
            "/other.cpp-abcdef12.o.tmp",
            "/main.cpp-12345678.o.tmp",
        ] {
            let mut t = setup();
            begin_input(&mut t);
            t.path_stat(path, 44, None);
            output(&mut t);
            assert_eq!(t.finish().bypass, Some("stock-generated-probe"));
        }
        let mut t = setup();
        begin_input(&mut t);
        output(&mut t);
        t.path_stat("/main.cpp.o", 0, Some(4));
        assert_eq!(t.finish().bypass, Some("stock-generated-probe"));
    }
    #[test]
    fn entropy_count_without_proven_protocol_never_admits() {
        let mut t = setup();
        begin_input(&mut t);
        for _ in 0..8 {
            t.random(4, 0);
        }
        output(&mut t);
        assert_eq!(t.finish().bypass, Some("stock-entropy-protocol"));
        let mut t = setup();
        begin_input(&mut t);
        t.path_stat("/main.cpp.o", 44, None);
        t.random(8, 0);
        assert_eq!(t.finish().bypass, Some("stock-entropy-protocol"));
    }
    #[test]
    fn other_selected_source_is_not_hidden_by_broad_namespace_key() {
        let mut t = setup();
        begin_input(&mut t);
        t.open("/other.cpp", 0, 2, 0, 0, Some(5));
        output(&mut t);
        assert_eq!(t.finish().bypass, Some("stock-mutable-input"));
    }
    #[test]
    fn unknown_reads_writes_symlinks_and_clocks_decline() {
        let mut t = setup();
        begin_input(&mut t);
        t.open("/outside.h", 0, 2, 0, 0, Some(5));
        assert_eq!(t.finish().bypass, Some("stock-unrepresented-input"));
        let mut t = setup();
        begin_input(&mut t);
        t.read(0);
        assert_eq!(t.finish().bypass, Some("stock-unowned-read"));
        let mut t = setup();
        begin_input(&mut t);
        t.readlink("/include/header.h", 0);
        assert_eq!(t.finish().bypass, Some("stock-symlink-input"));
        let mut t = setup();
        begin_input(&mut t);
        t.clock();
        output(&mut t);
        assert_eq!(t.finish().bypass, Some("stock-clock-input"));
        let mut t = setup();
        begin_input(&mut t);
        t.open("/include/header.h", 0, 66, 0, 0, Some(5));
        assert_eq!(t.finish().bypass, Some("stock-input-write"));
    }
    #[test]
    fn mutable_output_reads_and_wrong_publication_decline() {
        let mut t = setup();
        begin_input(&mut t);
        t.path_stat("/main.cpp.o", 44, None);
        for _ in 0..8 {
            t.random(4, 0);
        }
        t.open("/main.cpp-abcdef12.o.tmp", 5, 66, 0, 0, Some(5));
        t.read(5);
        assert_eq!(t.finish().bypass, Some("stock-unowned-read"));
        let mut t = setup();
        begin_input(&mut t);
        t.rename("/foreign.o.tmp", "/main.cpp.o", 0);
        assert_eq!(t.finish().bypass, Some("stock-output-protocol"));
    }
    #[test]
    fn compiler_output_protocol_requires_complete_success_and_no_preloaded_collision() {
        let mut t = setup();
        begin_input(&mut t);
        assert_eq!(t.finish().bypass, Some("stock-incomplete-trace"));
        let n = Namespace::new(
            ["/main.cpp".into(), "/main.cpp-0123abcd.o.tmp".into()].into(),
            ["/main.cpp".into()].into(),
        );
        assert_eq!(
            Trace::new(Arc::new(n), "/main.cpp".into()).finish().bypass,
            Some("stock-output-collision")
        );
    }
    #[test]
    fn trace_call_budget_stays_bounded_and_failure_is_sticky() {
        let mut t = setup();
        for _ in 0..=MAX_CALLS {
            t.begin();
        }
        begin_input(&mut t);
        output(&mut t);
        assert_eq!(t.finish().bypass, Some("stock-trace-budget"));
        assert!(!t.active());
    }
    #[test]
    fn realpath_accepts_only_known_non_symlink_directories() {
        let mut t = setup();
        begin_input(&mut t);
        t.readlink("/", 28);
        t.readlink("/include", 28);
        t.readlink("/include", 52);
        output(&mut t);
        assert_eq!(t.finish().bypass, None);
        assert_eq!(t.finish().input_paths, ["/main.cpp"]);
        for (path, errno) in [("/include", 0), ("/unrepresented", 28), ("/include", 44)] {
            let mut t = setup();
            begin_input(&mut t);
            t.readlink(path, errno);
            assert_eq!(t.finish().bypass, Some("stock-symlink-input"));
        }
    }
    #[test]
    fn namespace_paths_count_and_total_bytes_are_bounded() {
        for invalid in [
            "relative", "/", "/a//b", "/a/../b", "/a/./b", "/a\\b", "/a\0b",
        ] {
            let n = Namespace::new([invalid.to_owned()].into(), [invalid.to_owned()].into());
            assert!(!n.valid);
            assert!(n.files.is_empty());
            assert!(n.directories.is_empty());
        }
        let files: BTreeSet<_> = (0..=MAX_PATHS).map(|i| format!("/file{i}")).collect();
        assert!(!Namespace::new(files, ["/file0".into()].into()).valid);
        let files: BTreeSet<_> = (0..=1024).map(|i| format!("/file{i}")).collect();
        assert!(!Namespace::new(files.clone(), files).valid);
        let files: BTreeSet<_> = (0..1024)
            .map(|i| format!("/{}-{i}", "x".repeat(1024)))
            .collect();
        assert!(!Namespace::new(files, BTreeSet::new()).valid);
        // The retained ancestor set has its own bound even when file paths fit.
        let path = format!("/{}", (0..1800).map(|_| "a").collect::<Vec<_>>().join("/"));
        let n = Namespace::new([path.clone()].into(), [path].into());
        assert!(!n.valid);
        assert!(n.directories.is_empty());
        let n = Namespace::new(["/main.cpp".into()].into(), ["/unknown.cpp".into()].into());
        assert!(!n.valid);
    }
}
