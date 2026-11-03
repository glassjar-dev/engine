//! Evidence-only CLI: reports prototype inspection, never grants cache capability.
use engine_cpp_stock_policy_tests::{Policy, Toolchain};
use serde::Deserialize;
use std::{collections::BTreeMap, fs};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    certificate_path: String,
    files: Vec<Input>,
    dependencies: Vec<String>,
    source: String,
    selected_sources: Vec<String>,
    toolchain: Toolchain,
    pch: Option<Pch>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input { path: String, disk_path: String }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pch { path: String, flags: Vec<String> }
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args().nth(1).ok_or("request file required")?;
    let request: Request = serde_json::from_slice(&fs::read(arg)?)?;
    let raw = fs::read(&request.certificate_path)?;
    let policy = match Policy::parse(&raw) {
        Ok(p) => p,
        Err(reason) => { println!("{}", serde_json::json!({"eligible":false,"registered":false,"phase":"certificate","reason":reason.reason()})); return Ok(()); }
    };
    let files: BTreeMap<_, _> = request.files.into_iter().map(|f| Ok((f.path, fs::read(f.disk_path)?))).collect::<Result<_,std::io::Error>>()?;
    let pch = request.pch.as_ref().map(|p| (p.path.as_str(),p.flags.as_slice()));
    let started = std::time::Instant::now();
    let result = policy.inspect_closure(&files, &request.dependencies, &request.source, &request.selected_sources, &request.toolchain, pch);
    let report = match result {
        Ok(inspection) => serde_json::json!({"eligible":false,"prototypeLexicalPass":true,"inspection":inspection}),
        Err(reason) => serde_json::json!({"eligible":false,"prototypeLexicalPass":false,"registered":policy.registered(),"certificateSha256":policy.sha256(),"phase":"inputs","reason":reason.reason()}),
    };
    println!("{}", serde_json::json!({"result":report,"analysisMs":started.elapsed().as_secs_f64()*1000.0,"performanceGate":false}));
    Ok(())
}
