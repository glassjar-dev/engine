//! Per-job immutable stock policy/index. No compiler or preprocessor state is retained.
use super::{cpp_cache_inputs, cpp_stock_policy as policy, cpp_stock_trace, execution::{CompilerToolchain, Execution}};
use crate::types::CppArtifacts;
use std::{collections::BTreeMap, sync::{Arc, Mutex}};

const ABI: &str = "wasm32-wasip1/c++23/legacy-eh/stock-certified-v1/policy1";
const MAX_OWNED_INPUT_BYTES: usize = 32 * 1024 * 1024;
type Checked<T> = Result<T, String>;

use policy::StoredAdmission;

pub struct StockJob {
    policy: policy::Policy,
    toolchain: policy::Toolchain,
    index: cpp_cache_inputs::InputIndex,
    namespace: Arc<cpp_stock_trace::Namespace>,
    sources: Vec<String>,
    pch: Option<String>,
    actual: BTreeMap<String, Vec<u8>>,
    actual_bytes: usize,
    poisoned: Option<String>,
}
impl StockJob {
    pub async fn new(exec: &Execution, artifacts: &CppArtifacts, sources: &[String],
        inputs: Result<cpp_cache_inputs::UserInputs, cpp_cache_inputs::Bypass>, compiler: &CompilerToolchain,
    ) -> Checked<Self> {
        let raw = artifacts.cache_certificate.as_ref().ok_or("stock-certificate-missing")?;
        let policy = policy::Policy::parse(raw.as_bytes()).map_err(|r| r.reason())?;
        policy.require_registered().map_err(|r| r.reason())?;
        let (files, directories) = super::stock_namespace(&exec.fs).map_err(|r| r.reason())?;
        let namespace = cpp_stock_trace::Namespace::try_new(files.iter().map(String::as_str), sources.iter().map(String::as_str))
            .ok_or("stock-namespace-budget-or-path")?;
        if files.iter().any(|p| namespace.generated(p)) { return Err("stock-output-collision".into()); }
        let (c, s, e) = &compiler.identities;
        let toolchain = policy::Toolchain { compiler_sha256: c.clone(), sysroot_sha256: s.clone(), exceptions_sha256: e.clone() };
        policy.require_toolchain(&toolchain).map_err(|r| r.reason())?;
        let index = cpp_cache_inputs::InputIndex::from_user_inputs(cpp_cache_inputs::Environment::fixed(), cpp_cache_inputs::ToolchainIdentity {
            compiler: c.clone(), sysroot: s.clone(), exceptions: e.clone(), abi: ABI.into(),
        }, files, inputs.map_err(|r| r.reason())?, directories, sources.to_vec(), artifacts.precompiled_header.clone(), super::cpp_link_args(sources, artifacts))
            .map_err(|r| r.reason())?;
        let result = Self { policy, toolchain, index, namespace: Arc::new(namespace), sources: sources.to_vec(),
            pch: artifacts.precompiled_header.clone(), actual: BTreeMap::new(), actual_bytes: 0, poisoned: None };
        result.index.stock_snapshot(result.policy.sha256()).map_err(|r| r.reason())?;
        Ok(result)
    }
    pub fn certificate(&self) -> &str { self.policy.sha256() }
    pub fn snapshot(&self) -> Checked<String> { self.index.stock_snapshot(self.policy.sha256()).map_err(|r| r.reason().into()) }
    pub fn poison(&mut self, reason: &str) { self.poisoned.get_or_insert_with(|| reason.to_string()); }
    pub fn active(&self) -> Checked<()> { self.poisoned.as_ref().map_or(Ok(()), |r| Err(r.clone())) }
    async fn load(&mut self, exec: &Execution, path: &str) -> Checked<()> {
        if self.actual.contains_key(path) { return Ok(()); }
        let bytes = exec.read_cache_bytes(path, MAX_OWNED_INPUT_BYTES.saturating_sub(self.actual_bytes)).await.map_err(|_| "stock-input-budget-or-closure")?;
        self.actual_bytes = self.actual_bytes.checked_add(bytes.len()).filter(|n| *n <= MAX_OWNED_INPUT_BYTES).ok_or("stock-input-budget")?;
        self.actual.insert(path.to_string(), bytes); Ok(())
    }
    pub async fn preflight_source(&mut self, exec: &Execution, source: &str) -> Checked<()> {
        self.active()?;
        self.load(exec, source).await?;
        self.policy.preflight_source(&self.actual[source]).map_err(|r| r.reason().into())
    }
    /// Explicit prepare request only. Actual additional includes/diagnostics are
    /// still checked after each ordinary miss. No source is compiled here.
    pub async fn preflight(&mut self, exec: &Execution) -> Checked<()> {
        for source in self.sources.clone() { self.preflight_source(exec, &source).await?; }
        if let Some(source) = self.sources.first().cloned() {
            if self.pch.is_some() { self.inspect(exec, &source, vec![source.clone()]).await?; }
        }
        Ok(())
    }
    pub fn trace(&self, source: &str) -> Arc<Mutex<cpp_stock_trace::Trace>> {
        Arc::new(Mutex::new(cpp_stock_trace::Trace::from_source(self.namespace.clone(), source)))
    }
    pub fn lookup_key(&self, record: &str, source: &str, arguments: &[String], limit: usize) -> Checked<String> {
        self.active()?;
        self.policy.require_registered().map_err(|r| r.reason())?;
        let record = StoredAdmission::parse(record, self.policy.sha256(), source, limit)?;
        self.index.stock_key(source, arguments, self.policy.sha256(), &record.dependencies).map_err(|r| r.reason().into())
    }
    async fn inspect(&mut self, exec: &Execution, source: &str, mut dependencies: Vec<String>) -> Checked<Vec<String>> {
        let pch_inputs: Vec<String> = self.policy.pch_inputs(self.pch.as_deref()).map_err(|r| r.reason())?.into_iter().map(String::from).collect();
        dependencies.extend(pch_inputs); dependencies.sort(); dependencies.dedup();
        if dependencies.len() > 16384 { return Err("stock-input-budget".into()); }
        for path in &dependencies { self.load(exec, path).await?; }
        let pch = self.pch.as_ref().map(|path| self.policy.pch_flags(Some(path)).map(|flags| (path.as_str(), flags.expect("selected PCH flags")))).transpose().map_err(|r| r.reason())?;
        self.policy.inspect_closure(&self.actual, &dependencies, source, &self.sources, &self.toolchain, pch).map_err(|r| r.reason())?;
        Ok(dependencies)
    }
    pub async fn admit(&mut self, exec: &Execution, source: &str, arguments: &[String], observation: cpp_stock_trace::Observation, limit: usize) -> Checked<(String, String)> {
        self.active()?;
        if let Some(reason) = observation.bypass { self.poison(reason); return Err(reason.into()); }
        let dependencies = self.inspect(exec, source, observation.input_paths).await?;
        let key = self.index.stock_key(source, arguments, self.policy.sha256(), &dependencies).map_err(|r| r.reason())?;
        let record = serde_json::to_string(&StoredAdmission { schema: 1, certificate_sha256: self.policy.sha256().into(), source: source.into(), dependencies })
            .map_err(|_| "stock-metadata-malformed")?;
        if record.len() > limit.min(cpp_cache_inputs::MAX_METADATA_BYTES) { return Err("stock-metadata-budget".into()); }
        Ok((key, record))
    }
}
