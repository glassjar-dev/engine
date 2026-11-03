//! Exact stock-compiler invocation and immutable input keys.
//! Admission authority remains in the registered policy and observed input closure.
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_PATH: usize = 4096;
const LINK_PREFIX: &[&str] = &["--export-dynamic", "-z", "stack-size=1048576", "-L/lib/wasm32-wasip1", "/lib/wasm32-wasip1/crt1.o"];
const LINK_SUFFIX: &[&str] = &["/lib/wasm32-wasip1/cxa_exception.cpp.o", "/lib/wasm32-wasip1/cxa_personality.cpp.o", "-lc++", "-lc++abi", "-lunwind", "/lib/wasm32-unknown-wasip1/libclang_rt.builtins.a", "-lc", "-o", "/main.wasm"];


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bypass {
    UnsupportedInvocation,
    UnsupportedPath,
    OutputCollision,
    DependencyClosure,
}
impl Bypass {
    pub fn reason(self) -> &'static str {
        // Preserve existing observable bypass reasons while removing the dormant observer.
        match self {
            Self::UnsupportedInvocation => "receipt-unsupported-invocation",
            Self::UnsupportedPath => "receipt-unsupported-path",
            Self::OutputCollision => "receipt-output-collision",
            Self::DependencyClosure => "receipt-dependency-closure",
        }
    }
}

type Checked<T> = Result<T, Bypass>;

fn bounded_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(|c| c.is_control())
}
fn canonical(path: &str) -> bool {
    bounded_text(path, MAX_PATH) && path.starts_with('/') && !path.contains('\\') && !path[1..].split('/').any(|p| p.is_empty() || p == "." || p == "..")
}
fn unique<T: Ord>(values: &[T]) -> bool {
    values.iter().collect::<BTreeSet<_>>().len() == values.len()
}
fn digest(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn add(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}


/// This descriptor must be applied at the compiler WASI boundary before admission
/// can be enabled. It is internal policy, never a caller-supplied certification.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Environment {
    argv0: String,
    cwd: String,
    variables: BTreeMap<String, String>,
    preopens: Vec<String>,
    mount_order: Vec<String>,
    path_policy: String,
    resource_directory: String,
    builtin_module_map_directory: String,
    absent_global_module_index: String,
    header_search: Vec<String>,
    stdin_empty: bool,
}
impl Environment {
    pub fn fixed() -> Self {
        Self {
            argv0: "clang".into(), cwd: "/".into(), variables: BTreeMap::new(), preopens: vec!["/".into()],
            mount_order: vec!["verified-sysroot".into(), "verified-exceptions".into(), "owned-user-snapshot".into()],
            // Pinned build.sh uses -DCLANG_RESOURCE_DIR=/usr. Shipped cc1 -v
            // confirms these three search roots only; /usr/include is separately
            // probed for builtin module maps, not added as a header-search root.
            path_policy: "case-sensitive/canonical/no-symlinks/v1".into(), resource_directory: "/usr".into(),
            builtin_module_map_directory: "/usr/include".into(),
            // Normal PCH loading probes this even without module flags. Do not
            // change the compiler's global-index policy to manufacture closure.
            absent_global_module_index: "/modules.idx".into(),
            header_search: vec!["/include/c++/v1".into(), "/include".into(), "/include/wasm32-wasip1".into()], stdin_empty: true,
        }
    }
    fn supported(&self) -> bool { Self::fixed() == *self }
    /// Exact existing stock cc1 action; no depfile, observer or warning flags.
    pub fn stock_arguments(&self, source: &str, pch: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = [
            "-cc1", "-triple", "wasm32-wasip1", "-Werror", "-emit-obj", "-disable-free", "-isysroot", "/",
            "-internal-isystem", "/include/c++/v1", "-internal-isystem", "/include", "-internal-isystem", "/include/wasm32-wasip1",
            "-ferror-limit", "4", "-fcolor-diagnostics", "-x", "c++", "-std=c++23", "-fexceptions", "-fcxx-exceptions", "-exception-model=wasm",
            "-target-feature", "+exception-handling", "-mllvm", "-wasm-enable-eh", "-mllvm", "-wasm-use-legacy-eh=true", "-o",
        ].into_iter().map(String::from).collect();
        args.push(format!("{source}.o"));
        if let Some(pch) = pch { args.extend(["-include-pch".into(), pch.into(), "-fvalidate-ast-input-files-content".into()]); }
        args.push(source.into());
        args
    }

    pub fn link_arguments(sources: &[String], archives: &[String]) -> Vec<String> {
        LINK_PREFIX.iter().map(|s| s.to_string())
            .chain(sources.iter().map(|source| format!("{source}.o")))
            .chain(archives.iter().cloned())
            .chain(LINK_SUFFIX.iter().map(|s| s.to_string())).collect()
    }
}

#[derive(Debug, Clone)]
pub struct ToolchainIdentity {
    pub compiler: String,
    pub sysroot: String,
    pub exceptions: String,
    pub abi: String,
}
impl ToolchainIdentity {
    fn valid(&self) -> bool {
        [&self.compiler, &self.sysroot, &self.exceptions].iter().all(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) && bounded_text(&self.abi, 128)
    }
    fn key(&self) -> String {
        let mut hash = Sha256::new();
        for field in [&self.compiler, &self.sysroot, &self.exceptions, &self.abi] { add(&mut hash, field.as_bytes()); }
        format!("{:x}", hash.finalize())
    }
}

/// Exact effective immutable namespace: toolchain first, then owned user overlays.
/// The toolchain member inventory must come from the verified archive, including
/// all accessible files. Fresh filesystem construction may not introduce extra inputs.
pub struct InputIndex {
    environment: Environment,
    toolchain: ToolchainIdentity,
    files: BTreeMap<String, String>,
    user_files: BTreeMap<String, String>,
    namespace: BTreeSet<String>,
    sources: Vec<String>,
    outputs: Vec<String>,
    pch: Option<String>,
    link_arguments: Vec<String>,
}

/// Hash the owned user snapshot once, before asynchronous filesystem construction.
pub struct UserInputs {
    files: BTreeMap<String, String>,
}
impl UserInputs {
    pub fn new<'a>(files: impl IntoIterator<Item = (String, &'a [u8])>) -> Checked<Self> {
        let mut result = BTreeMap::new();
        for (path, bytes) in files {
            if !canonical(&path) || result.contains_key(&path) { return Err(Bypass::UnsupportedPath); }
            result.insert(path, digest(bytes));
        }
        Ok(Self { files: result })
    }
}

impl InputIndex {
    #[cfg(test)]
    pub fn new<'a>(environment: Environment, toolchain: ToolchainIdentity,
        toolchain_files: impl IntoIterator<Item = String>, user_files: impl IntoIterator<Item = (String, &'a [u8])>,
        mounted_directories: impl IntoIterator<Item = String>, sources: Vec<String>, pch: Option<String>, link_arguments: Vec<String>) -> Checked<Self> {
        Self::from_user_inputs(environment, toolchain, toolchain_files, UserInputs::new(user_files)?, mounted_directories, sources, pch, link_arguments)
    }

    pub fn from_user_inputs(environment: Environment, toolchain: ToolchainIdentity,
        toolchain_files: impl IntoIterator<Item = String>, user_files: UserInputs,
        mounted_directories: impl IntoIterator<Item = String>, sources: Vec<String>, pch: Option<String>, link_arguments: Vec<String>) -> Checked<Self> {
        if !environment.supported() || !toolchain.valid() || sources.is_empty() || !unique(&sources) || sources.len() > 256 ||
            sources.iter().any(|s| !canonical(s)) || pch.as_ref().is_some_and(|s| !canonical(s)) ||
            link_arguments.iter().any(|arg| arg.starts_with('@') || !bounded_text(arg, MAX_PATH)) {
            return Err(Bypass::UnsupportedInvocation);
        }
        let mut files = BTreeMap::new();
        let identity = toolchain.key();
        for path in toolchain_files {
            if !canonical(&path) || files.contains_key(&path) { return Err(Bypass::UnsupportedPath); }
            // All bytes of this member are bound by the actual verified archive identity.
            files.insert(path.clone(), digest(format!("{identity}:{path}").as_bytes()));
        }
        let user = user_files.files;
        files.extend(user.iter().map(|(path, content)| (path.clone(), content.clone())));
        if sources.iter().any(|s| !user.contains_key(s)) || pch.as_ref().is_some_and(|s| !user.contains_key(s)) { return Err(Bypass::DependencyClosure); }
        let archive_start = LINK_PREFIX.len() + sources.len();
        let archive_end = link_arguments.len().checked_sub(LINK_SUFFIX.len()).ok_or(Bypass::UnsupportedInvocation)?;
        if archive_end < archive_start { return Err(Bypass::UnsupportedInvocation); }
        let archives = &link_arguments[archive_start..archive_end];
        if archives.iter().any(|path| !canonical(path) || !files.contains_key(path)) ||
            link_arguments != Environment::link_arguments(&sources, archives) {
            return Err(Bypass::UnsupportedInvocation);
        }
        let mut namespace = BTreeSet::from(["/".to_string()]);
        let mut directories = BTreeSet::new();
        for path in mounted_directories {
            if path == "/" { continue; }
            if !canonical(&path) || files.contains_key(&path) { return Err(Bypass::UnsupportedPath); }
            directories.insert(path);
        }
        for path in files.keys().chain(directories.iter()) {
            namespace.insert(path.clone());
            let mut parent = path.as_str();
            while let Some((prefix, _)) = parent.rsplit_once('/') {
                if prefix.is_empty() { break; }
                if files.contains_key(prefix) { return Err(Bypass::UnsupportedPath); }
                namespace.insert(prefix.to_string()); parent = prefix;
            }
        }
        let mut outputs: Vec<_> = sources.iter().map(|source| format!("{source}.o")).collect();
        outputs.push("/main.wasm".into());
        let result = Self { environment, toolchain, files, user_files: user, namespace, sources, outputs, pch, link_arguments };
        if result.namespace.contains(&result.environment.absent_global_module_index) { return Err(Bypass::UnsupportedInvocation); }
        if result.namespace.iter().any(|path| result.generated(path)) { return Err(Bypass::OutputCollision); }
        Ok(result)
    }

    fn generated(&self, path: &str) -> bool {
        if ["/tmp", "/dev", "/proc"].iter().any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/"))) { return true; }
        self.outputs.iter().any(|output| {
            if path == output { return true; }
            let Some((stem, extension)) = output.rsplit_once('.') else { return true; };
            // Clang creates entropy-derived sibling temporaries before final rename.
            path.starts_with(&format!("{stem}-")) && path.ends_with(&format!(".{extension}.tmp"))
        })
    }

    fn base_key(&self, source: &str, arguments: &[String]) -> Checked<String> {
        let expected = self.environment.stock_arguments(source, self.pch.as_deref());
        if !self.sources.iter().any(|s| s == source) || !self.environment.supported() || arguments.iter().any(|arg| arg.starts_with('@')) || arguments != expected {
            return Err(Bypass::UnsupportedInvocation);
        }
        let mut hash = Sha256::new();
        let schema = "debugger-sh/cpp-tu/stock-certified-v1/policy1";
        let capability = "stock-certified-v1";
        for value in [schema, capability, self.toolchain.key().as_str(), source] { add(&mut hash, value.as_bytes()); }
        add(&mut hash, &serde_json::to_vec(&self.environment).map_err(|_| Bypass::UnsupportedInvocation)?);
        add(&mut hash, b"compile");
        for arg in arguments { add(&mut hash, arg.as_bytes()); }
        add(&mut hash, b"link");
        for arg in &self.link_arguments { add(&mut hash, arg.as_bytes()); }
        add(&mut hash, b"ordered-source-output");
        for (source, output) in self.sources.iter().zip(&self.outputs) { add(&mut hash, source.as_bytes()); add(&mut hash, output.as_bytes()); }
        add(&mut hash, b"namespace");
        for path in &self.namespace { add(&mut hash, path.as_bytes()); }
        add(&mut hash, b"broad-user-content");
        for (path, content) in &self.user_files {
            if path == source || !self.sources.contains(path) { add(&mut hash, path.as_bytes()); add(&mut hash, content.as_bytes()); }
        }
        Ok(format!("{:x}", hash.finalize()))
    }

    /// Stored stock metadata records an already admitted actual input closure.
    /// This key cannot itself authorize an unregistered certificate. The caller
    /// checks that registry first and performs full policy/trace admission on a
    /// miss. Equal keys bind all bytes the prior admission depended upon.
    pub fn stock_key(&self, source: &str, arguments: &[String], certificate: &str, dependencies: &[String]) -> Checked<String> {
        if certificate.len() != 64 || !certificate.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) ||
            dependencies.len() > 16384 || !unique(dependencies) || !dependencies.iter().any(|p| p == source) ||
            self.pch.as_ref().is_some_and(|p| !dependencies.contains(p)) {
            return Err(Bypass::DependencyClosure);
        }
        let base = self.base_key(source, arguments)?;
        let mut sorted = BTreeMap::new();
        for path in dependencies {
            if self.generated(path) || path != source && self.sources.contains(path) { return Err(Bypass::DependencyClosure); }
            sorted.insert(path, self.files.get(path).ok_or(Bypass::DependencyClosure)?);
        }
        let mut hash = Sha256::new(); add(&mut hash, base.as_bytes()); add(&mut hash, certificate.as_bytes());
        for (path, content) in sorted { add(&mut hash, path.as_bytes()); add(&mut hash, content.as_bytes()); }
        Ok(format!("{:x}", hash.finalize()))
    }

    pub fn stock_snapshot(&self, certificate: &str) -> Checked<String> {
        let mut hash = Sha256::new(); add(&mut hash, certificate.as_bytes());
        for source in &self.sources { add(&mut hash, self.base_key(source, &self.environment.stock_arguments(source, self.pch.as_deref()))?.as_bytes()); }
        Ok(format!("{:x}", hash.finalize()))
    }

}



#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ToolchainIdentity {
        ToolchainIdentity { compiler: "a".repeat(64), sysroot: "b".repeat(64), exceptions: "c".repeat(64), abi: "wasm32-wasip1/c++23/legacy-eh/stock-certified-v1/policy1".into() }
    }
    fn files() -> BTreeMap<String, Vec<u8>> {
        [("/a.cpp", "int a;"), ("/b.cpp", "int b;"), ("/common.h", "#define N 1"), ("/image.bin", "binary")]
            .into_iter().map(|(p, b)| (p.into(), b.as_bytes().to_vec())).collect()
    }
    fn index(files: &BTreeMap<String, Vec<u8>>, sources: &[&str], pch: Option<&str>) -> Checked<InputIndex> {
        let sources: Vec<_> = sources.iter().map(|s| s.to_string()).collect();
        InputIndex::new(Environment::fixed(), identity(),
            vec!["/include/stdio.h".into(), "/include/c++/v1/vector".into()],
            files.iter().map(|(p,b)| (p.clone(), b.as_slice())), vec![], sources.clone(), pch.map(String::from),
            Environment::link_arguments(&sources, &[]))
    }
    fn key(index: &InputIndex) -> Checked<String> {
        index.stock_key("/a.cpp", &index.environment.stock_arguments("/a.cpp", index.pch.as_deref()),
            &"d".repeat(64), &["/a.cpp".into(), "/include/stdio.h".into()])
    }

    #[test]
    fn preflight_rejects_alternate_action_configuration_environment_and_response_arguments() {
        let index = index(&files(), &["/a.cpp"], None).unwrap();
        let normal=index.environment.stock_arguments("/a.cpp",None);
        for arg in ["@/args.rsp","-load","-ivfsoverlay","-fmodules","-fsyntax-only","--config=/config","-I/tmp"] {
            let mut args=normal.clone();args.push(arg.into());
            assert_eq!(index.base_key("/a.cpp",&args),Err(Bypass::UnsupportedInvocation));
        }
        let mut args=normal.clone();args[0]="-cc1as".into();
        assert_eq!(index.base_key("/a.cpp",&args),Err(Bypass::UnsupportedInvocation));
        let mut index=index;index.environment.variables.insert("CPATH".into(),"/tmp".into());
        assert_eq!(index.base_key("/a.cpp",&normal),Err(Bypass::UnsupportedInvocation));
        let sources=vec!["/a.cpp".into()];
        let mut args=Environment::link_arguments(&sources,&[]);args.push("-o/alternative".into());
        assert!(matches!(InputIndex::new(Environment::fixed(),identity(),vec![],files().iter().map(|(p,b)|(p.clone(),b.as_slice())),vec![],sources,None,args),Err(Bypass::UnsupportedInvocation)));
    }

    #[test]
    fn stock_keys_bind_exact_invocation_policy_and_owned_snapshot() {
        let original=files(); let sources=["/a.cpp","/b.cpp"];
        let before=index(&original,&sources,None).unwrap();
        let args=before.environment.stock_arguments("/a.cpp",None);let certificate="d".repeat(64);
        let deps=vec!["/a.cpp".into(),"/include/stdio.h".into()];
        let baseline=before.stock_key("/a.cpp",&args,&certificate,&deps).unwrap();
        assert_ne!(baseline,before.stock_key("/a.cpp",&args,&"e".repeat(64),&deps).unwrap());
        for extra in ["-DTEST=1","-fwebide-cache-receipt","@args","-ftime-trace"] {
            let mut changed=args.clone();changed.push(extra.into());
            assert_eq!(before.stock_key("/a.cpp",&changed,&certificate,&deps),Err(Bypass::UnsupportedInvocation));
        }
        for path in ["/a.cpp.o","/a.cpp-12345678.o.tmp","/b.cpp","/missing.h"] {
            let mut changed=deps.clone(); changed.push(path.into());
            assert_eq!(before.stock_key("/a.cpp",&args,&certificate,&changed),Err(Bypass::DependencyClosure));
        }
        let mut sibling=original.clone();sibling.insert("/b.cpp".into(),b"int z;".to_vec());
        let after=index(&sibling,&sources,None).unwrap();
        assert_eq!(baseline,after.stock_key("/a.cpp",&args,&certificate,&deps).unwrap());
        assert_ne!(before.stock_snapshot(&certificate),after.stock_snapshot(&certificate));
        for (path,bytes) in [("/a.cpp",b"int z;".as_slice()),("/image.bin",b"change".as_slice()),("/unused.h",b"new".as_slice()),("/include/stdio.h",b"shadow".as_slice())] {
            let mut changed=original.clone();changed.insert(path.into(),bytes.to_vec());
            assert_ne!(baseline,index(&changed,&sources,None).unwrap().stock_key("/a.cpp",&args,&certificate,&deps).unwrap());
        }
        assert_ne!(baseline,index(&original,&["/b.cpp","/a.cpp"],None).unwrap().stock_key("/a.cpp",&args,&certificate,&deps).unwrap());
        let mut env_changed=before;env_changed.environment.variables.insert("CPATH".into(),"/new".into());
        assert_eq!(env_changed.stock_key("/a.cpp",&args,&certificate,&deps),Err(Bypass::UnsupportedInvocation));
    }

    #[test]
    fn namespace_order_resources_and_shadowing_invalidate_broad_identity() {
        let original=files();
        let before=index(&original,&["/a.cpp","/b.cpp"],None).unwrap();let baseline=key(&before).unwrap();
        for (path,bytes) in [("/common.h",b"#define N 2".as_slice()),("/image.bin",b"change".as_slice()),("/unused.txt",b"new".as_slice()),("/include/stdio.h",b"shadow".as_slice())] {
            let mut files=original.clone();files.insert(path.into(),bytes.to_vec());
            assert_ne!(baseline,key(&index(&files,&["/a.cpp","/b.cpp"],None).unwrap()).unwrap());
        }
        let mut removed=original.clone();removed.remove("/image.bin");
        assert_ne!(baseline,key(&index(&removed,&["/a.cpp","/b.cpp"],None).unwrap()).unwrap());
        assert_ne!(baseline,key(&index(&original,&["/b.cpp","/a.cpp"],None).unwrap()).unwrap());
        let mut changed=index(&original,&["/a.cpp","/b.cpp"],None).unwrap();changed.toolchain.compiler="d".repeat(64);
        assert_ne!(baseline,key(&changed).unwrap());
        let sources=vec!["/a.cpp".into(),"/b.cpp".into()];
        let directory_added=InputIndex::new(Environment::fixed(),identity(),
            vec!["/include/stdio.h".into(),"/include/c++/v1/vector".into()], original.iter().map(|(p,b)|(p.clone(),b.as_slice())),
            vec!["/empty".into()], sources.clone(), None, Environment::link_arguments(&sources,&[])).unwrap();
        assert_ne!(baseline,key(&directory_added).unwrap(),"empty directories are part of the accessible namespace");
    }

    #[test]
    fn output_or_directory_collisions_are_ineligible_before_lookup() {
        for path in ["/a.cpp.o","/a.cpp-deadbeef.o.tmp","/b.cpp.o","/main.wasm","/main-deadbeef.wasm.tmp","/tmp/input.h","/a.cpp/child","/modules.idx"] {
            let mut files=files();files.insert(path.into(),b"owned".to_vec());
            assert!(index(&files,&["/a.cpp","/b.cpp"],None).is_err(),"{path}");
        }
    }

    #[test]
    fn selected_pch_must_be_an_owned_dependency_and_its_exact_bytes_bind_the_key() {
        let mut files = files();
        files.insert("/std.pch".into(), vec![1, 2, 3]);
        let before = index(&files, &["/a.cpp", "/b.cpp"], Some("/std.pch")).unwrap();
        let args = before.environment.stock_arguments("/a.cpp", Some("/std.pch"));
        let certificate = "d".repeat(64);
        let deps = vec!["/a.cpp".into(), "/std.pch".into()];
        let baseline = before.stock_key("/a.cpp", &args, &certificate, &deps).unwrap();
        assert_eq!(before.stock_key("/a.cpp", &args, &certificate, &["/a.cpp".into()]), Err(Bypass::DependencyClosure));
        files.insert("/std.pch".into(), vec![1, 2, 4]);
        let changed = index(&files, &["/a.cpp", "/b.cpp"], Some("/std.pch")).unwrap();
        assert_ne!(baseline, changed.stock_key("/a.cpp", &args, &certificate, &deps).unwrap());
        files.remove("/std.pch");
        assert!(matches!(index(&files, &["/a.cpp"], Some("/std.pch")), Err(Bypass::DependencyClosure)));
    }
}
