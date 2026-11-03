//! Bounded stock-compiler certification. Passing lexical checks is not cache authority:
//! a reviewed, build-fixed certificate, the ordinary invocation descriptor and
//! complete WASI input observation are separate mandatory gates.
//!
//! Certificates belong to the companion package. They contain no student source
//! identities. A registry entry authorizes exact audited trusted bytes and public
//! macro wrappers; it is never supplied by the host application. The initial
//! registry pins the independently audited certificate. Changed trusted inputs
//! require review and a compatible tuple release.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const PROFILE: &str = "stock-certified-v1";
pub const MAX_CERTIFICATE_BYTES: usize = 1024 * 1024;
const MAX_FILES: usize = 16384;
const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOKENS: usize = 2_000_000;
const MAX_MACROS: usize = 32768;
const MAX_TEXT: usize = 4096;
const APPROVED_CERTIFICATES: &str = include_str!("../ts/stock-cache-certificates.json");

type Checked<T> = Result<T, Bypass>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Bypass {
    CertificateBudget, CertificateMalformed, CertificateUnregistered,
    ToolchainMismatch, TrustedInputMismatch, PchMismatch, InputClosure,
    LexicalBudget, UnsupportedToken, Temporal, Assembly, PrivateMacro,
    MacroDefinition, Directive, Include, Probe, CertificateMacroMismatch,
}
impl Bypass {
    pub fn reason(self) -> &'static str {
        match self {
            Self::CertificateBudget => "stock-certificate-budget",
            Self::CertificateMalformed => "stock-certificate-malformed",
            Self::CertificateUnregistered => "stock-certificate-unregistered",
            Self::ToolchainMismatch => "stock-toolchain-mismatch",
            Self::TrustedInputMismatch => "stock-trusted-input-mismatch",
            Self::PchMismatch => "stock-pch-mismatch",
            Self::InputClosure => "stock-input-closure",
            Self::LexicalBudget => "stock-lexical-budget",
            Self::UnsupportedToken => "stock-unsupported-token",
            Self::Temporal => "stock-temporal",
            Self::Assembly => "stock-assembly",
            Self::PrivateMacro => "stock-private-macro",
            Self::MacroDefinition => "stock-macro-definition",
            Self::Directive => "stock-directive",
            Self::Include => "stock-include",
            Self::Probe => "stock-probe",
            Self::CertificateMacroMismatch => "stock-certificate-macro-mismatch",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Toolchain {
    pub compiler_sha256: String,
    pub sysroot_sha256: String,
    pub exceptions_sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity { pub path: String, pub sha256: String, pub bytes: usize }

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MacroCertificate {
    pub header_path: String,
    pub name: String,
    #[serde(deserialize_with = "required_nullable")]
    pub parameters: Option<Vec<String>>,
    pub variadic: bool,
    pub replacement: String,
    pub visibility: Visibility,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Visibility { Public, Private }

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetadataProof {
    pub has_timestamps: bool,
    pub input_count: usize,
    pub stored_times_zero: bool,
    pub has_imports: bool,
    pub parser_evidence_sha256: String,
    pub macro_state_evidence_sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PchCertificate {
    pub path: String,
    pub sha256: String,
    pub bytes: usize,
    pub flags: Vec<String>,
    pub inputs: Vec<FileIdentity>,
    pub metadata_proof: MetadataProof,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Certificate {
    pub schema_version: u32,
    pub policy: String,
    pub required_toolchain: Toolchain,
    pub trusted_headers: Vec<FileIdentity>,
    pub macros: Vec<MacroCertificate>,
    pub protected_identifiers: Vec<String>,
    pub protected_prefixes: Vec<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub pch: Option<PchCertificate>,
}

fn required_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}
fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn identifier(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_TEXT && s.as_bytes()[0].is_ascii_alphabetic_or_underscore()
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
trait IdentifierByte { fn is_ascii_alphabetic_or_underscore(self) -> bool; }
impl IdentifierByte for u8 { fn is_ascii_alphabetic_or_underscore(self) -> bool { self.is_ascii_alphabetic() || self == b'_' } }
fn digest(s: &str) -> bool { s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f')) }
fn canonical(s: &str) -> bool {
    s.starts_with('/') && s.len() <= MAX_TEXT && s.len() > 1 && !s.chars().any(|c| c.is_control() || c == '\\')
        && s[1..].split('/').all(|p| !matches!(p, "" | "." | ".."))
}
fn unique<T: Ord>(v: impl IntoIterator<Item = T>) -> bool {
    let mut seen = BTreeSet::new(); v.into_iter().all(|s| seen.insert(s))
}
fn valid_files(files: &[FileIdentity]) -> bool {
    files.len() <= MAX_FILES && unique(files.iter().map(|f| &f.path))
        && files.iter().all(|f| canonical(&f.path) && digest(&f.sha256) && f.bytes <= MAX_INPUT_BYTES)
}

/// Private worker-produced metadata, never a caller certificate or authority.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredAdmission { pub schema: u32, pub certificate_sha256: String, pub source: String, pub dependencies: Vec<String> }
impl StoredAdmission {
    pub fn parse(raw: &str, certificate: &str, source: &str, limit: usize) -> Result<Self, &'static str> {
        if raw.len() > limit.min(MAX_CERTIFICATE_BYTES) { return Err("stock-metadata-budget"); }
        let value: Self = serde_json::from_str(raw).map_err(|_| "stock-metadata-malformed")?;
        if value.schema != 1 || !digest(certificate) || value.certificate_sha256 != certificate || value.source != source ||
            !canonical(source) || value.dependencies.len() > MAX_FILES || !unique(&value.dependencies) ||
            !value.dependencies.iter().any(|p| p == source) || value.dependencies.iter().any(|p| !canonical(p)) {
            return Err("stock-metadata-malformed");
        }
        Ok(value)
    }
}

/// Exact raw serialization is part of identity, including whitespace. Inspection
/// of an unregistered prototype is useful for tests but cannot grant authority.
pub struct Policy { certificate: Certificate, sha256: String }
impl Policy {
    pub fn parse(bytes: &[u8]) -> Checked<Self> {
        if bytes.len() > MAX_CERTIFICATE_BYTES { return Err(Bypass::CertificateBudget); }
        let c: Certificate = serde_json::from_slice(bytes).map_err(|_| Bypass::CertificateMalformed)?;
        let toolchain = &c.required_toolchain;
        if c.schema_version != 1 || c.policy != PROFILE ||
            ![&toolchain.compiler_sha256, &toolchain.sysroot_sha256, &toolchain.exceptions_sha256].into_iter().all(|s| digest(s)) ||
            !valid_files(&c.trusted_headers) || c.macros.len() > MAX_MACROS ||
            !unique(c.macros.iter().map(|m| (&m.header_path, &m.name))) ||
            c.macros.iter().any(|m| !canonical(&m.header_path) || !identifier(&m.name) || m.replacement.len() > MAX_TEXT ||
                !c.trusted_headers.iter().any(|f| f.path == m.header_path) ||
                m.parameters.as_ref().is_some_and(|p| p.len() > 64 || !unique(p) || p.iter().any(|v| !identifier(v))) ||
                (m.variadic && m.parameters.is_none())) ||
            c.protected_identifiers.len() > MAX_MACROS || !unique(&c.protected_identifiers) || c.protected_identifiers.iter().any(|s| !identifier(s)) ||
            c.protected_prefixes.len() > 256 || !unique(&c.protected_prefixes) || c.protected_prefixes.iter().any(|s| !identifier(s)) {
            return Err(Bypass::CertificateMalformed);
        }
        if let Some(p) = &c.pch {
            let proof = &p.metadata_proof;
            if !canonical(&p.path) || !digest(&p.sha256) || p.bytes == 0 || p.bytes > MAX_INPUT_BYTES ||
                p.flags.len() > 128 || p.flags.iter().any(|v| v.len() > MAX_TEXT || v.chars().any(char::is_control)) ||
                !valid_files(&p.inputs) || proof.has_timestamps || !proof.stored_times_zero || proof.has_imports ||
                proof.input_count != p.inputs.len() || p.inputs.is_empty() ||
                !digest(&proof.parser_evidence_sha256) || !digest(&proof.macro_state_evidence_sha256) ||
                p.inputs.iter().any(|f| !c.trusted_headers.iter().any(|h| h.path == f.path && h.sha256 == f.sha256 && h.bytes == f.bytes)) {
                return Err(Bypass::CertificateMalformed);
            }
        }
        Ok(Self { certificate: c, sha256: hash(bytes) })
    }
    pub fn sha256(&self) -> &str { &self.sha256 }
    pub fn registered(&self) -> bool {
        serde_json::from_str::<Vec<String>>(APPROVED_CERTIFICATES).is_ok_and(|approved| approved.iter().any(|value| value == &self.sha256))
    }
    /// Call before any cache lookup. An application cannot extend this registry.
    pub fn require_registered(&self) -> Checked<()> {
        if self.registered() { Ok(()) } else { Err(Bypass::CertificateUnregistered) }
    }
    pub fn require_toolchain(&self, toolchain: &Toolchain) -> Checked<()> {
        if toolchain == &self.certificate.required_toolchain { Ok(()) } else { Err(Bypass::ToolchainMismatch) }
    }
    pub fn pch_inputs(&self, path: Option<&str>) -> Checked<Vec<&str>> {
        match path {
            None => Ok(Vec::new()),
            Some(path) => {
                let pch = self.certificate.pch.as_ref().filter(|p| p.path == path).ok_or(Bypass::PchMismatch)?;
                Ok(pch.inputs.iter().map(|f| f.path.as_str()).chain(std::iter::once(pch.path.as_str())).collect())
            }
        }
    }
    pub fn pch_flags(&self, path: Option<&str>) -> Checked<Option<&[String]>> {
        match path { None => Ok(None), Some(path) => self.certificate.pch.as_ref().filter(|p| p.path == path).map(|p| Some(p.flags.as_slice())).ok_or(Bypass::PchMismatch) }
    }
    /// Known source-level exclusions are checked before warmup dispatch. This is
    /// a potential-admission check; actual includes and diagnostics are unknown
    /// until a normal miss executes, and still require the full closure check.
    pub fn preflight_source(&self, bytes: &[u8]) -> Checked<()> {
        let private = self.certificate.macros.iter().filter(|m| m.visibility == Visibility::Private).map(|m| m.name.clone()).collect();
        let public = self.certificate.macros.iter().filter(|m| m.visibility == Visibility::Public).map(|m| m.name.clone()).collect();
        let mut protected: BTreeSet<String> = self.certificate.protected_identifiers.iter().cloned().collect();
        for definition in &self.certificate.macros {
            protected.extend(lex(definition.replacement.as_bytes())?.into_iter().filter(|t| t.kind == Kind::Identifier).map(|t| t.text));
        }
        guard(&lex(bytes)?, &private, &public, &protected, &self.certificate.protected_prefixes)
    }

    /// Analysis only: verifies exact actual effective mount bytes and derives a
    /// conservative policy over them. Optional trusted files need not be mounted;
    /// the certificate never adds a path to the guest namespace. `dependencies`
    /// must come from complete actual observation, not a host-provided assertion.
    pub fn inspect_closure(
        &self, actual: &BTreeMap<String, Vec<u8>>, dependencies: &[String],
        source: &str, selected_sources: &[String], toolchain: &Toolchain,
        pch: Option<(&str, &[String])>,
    ) -> Checked<Inspection> {
        if toolchain != &self.certificate.required_toolchain { return Err(Bypass::ToolchainMismatch); }
        if dependencies.len() > MAX_FILES || !unique(dependencies) || !dependencies.iter().any(|p| p == source) ||
            dependencies.iter().any(|p| !canonical(p) || !actual.contains_key(p)) ||
            dependencies.iter().any(|p| p != source && selected_sources.contains(p)) {
            return Err(Bypass::InputClosure);
        }
        let mut closure: BTreeSet<&str> = dependencies.iter().map(String::as_str).collect();
        let pch_path = if let Some((path, flags)) = pch {
            let p = self.certificate.pch.as_ref().ok_or(Bypass::PchMismatch)?;
            if path != p.path || flags != p.flags || !matches_file(actual.get(path), &p.sha256, p.bytes) { return Err(Bypass::PchMismatch); }
            for f in &p.inputs {
                if !matches_file(actual.get(&f.path), &f.sha256, f.bytes) { return Err(Bypass::PchMismatch); }
                closure.insert(&f.path);
            }
            Some(path)
        } else { None };
        let trusted: BTreeMap<_, _> = self.certificate.trusted_headers.iter().map(|f| (f.path.as_str(), f)).collect();
        let mut tokens = BTreeMap::new();
        let mut total = 0usize;
        let mut token_total = 0usize;
        for path in &closure {
            if Some(*path) == pch_path { continue; }
            let bytes = actual.get(*path).ok_or(Bypass::InputClosure)?;
            total = total.checked_add(bytes.len()).ok_or(Bypass::LexicalBudget)?;
            if total > MAX_INPUT_BYTES { return Err(Bypass::LexicalBudget); }
            if let Some(expected) = trusted.get(path) {
                if !matches_file(Some(bytes), &expected.sha256, expected.bytes) { return Err(Bypass::TrustedInputMismatch); }
            }
            let file_tokens = lex_with_limit(bytes, MAX_TOKENS - token_total)?;
            token_total = token_total.checked_add(file_tokens.len()).ok_or(Bypass::LexicalBudget)?;
            if token_total > MAX_TOKENS { return Err(Bypass::LexicalBudget); }
            tokens.insert(*path, file_tokens);
        }
        let mut macros: BTreeMap<String, Vec<MacroDefinition>> = BTreeMap::new();
        let mut protected: BTreeSet<String> = self.certificate.protected_identifiers.iter().cloned().collect();
        for (path, file) in &tokens {
            if !trusted.contains_key(path) { continue; }
            // Exact trusted bytes are not a blanket temporal exemption. These
            // tokens are rejected even in inactive branches/macro definitions;
            // the approved inventory must contain none in its used closure.
            if file.iter().any(|t| t.kind == Kind::Identifier && matches!(t.text.as_str(), "__DATE__" | "__TIME__" | "__TIMESTAMP__")) {
                return Err(Bypass::Temporal);
            }
            if lines(file).any(|l| l.len() >= 4 && l[0].text == "#" && l[1].text == "pragma" && matches!(l[2].text.as_str(), "GCC" | "clang") && l[3].text == "dependency") {
                return Err(Bypass::Directive);
            }
            protected.extend(file.iter().filter(|t| t.kind == Kind::Identifier).map(|t| t.text.clone()));
            for definition in definitions(file)? {
                macros.entry(definition.name.clone()).or_default().push(definition);
            }
        }
        if macros.len() > MAX_MACROS || protected.len() > MAX_TOKENS { return Err(Bypass::LexicalBudget); }
        let mut public = BTreeSet::new();
        let mut private: BTreeSet<String> = self.certificate.macros.iter().filter(|m| m.visibility == Visibility::Private).map(|m| m.name.clone()).collect();
        for certified in &self.certificate.macros {
            let Some(file) = tokens.get(certified.header_path.as_str()) else { continue; };
            let matching: Vec<_> = definitions(file)?.into_iter().filter(|m| m.name == certified.name).collect();
            let expected = lex(certified.replacement.as_bytes())?;
            if matching.is_empty() || matching.iter().any(|m| m.parameters != certified.parameters || m.variadic != certified.variadic || !same_tokens(&m.body, &expected)) {
                return Err(Bypass::CertificateMacroMismatch);
            }
            if certified.visibility == Visibility::Public { public.insert(certified.name.clone()); }
        }
        // Over-approximate every conditional definition. Object-like aliases and
        // nested callers are tainted too, including use inside public arguments.
        for (name, defs) in &macros {
            if defs.iter().any(|m| m.body.iter().any(root_hazard)) { private.insert(name.clone()); }
        }
        let mut callers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (name, defs) in &macros {
            for token in defs.iter().flat_map(|d| &d.body).filter(|t| t.kind == Kind::Identifier) {
                callers.entry(&token.text).or_default().insert(name);
            }
        }
        let mut work: Vec<String> = private.iter().cloned().collect();
        while let Some(name) = work.pop() {
            if let Some(parents) = callers.get(name.as_str()) {
                for parent in parents {
                    if private.insert((*parent).to_string()) { work.push((*parent).to_string()); }
                }
            }
        }
        // A public certificate cannot accidentally authorize a different macro
        // with the same name from another trusted header in the closure.
        for name in &public {
            let declared = self.certificate.macros.iter().find(|m| &m.name == name && m.visibility == Visibility::Public).ok_or(Bypass::CertificateMacroMismatch)?;
            let expected = lex(declared.replacement.as_bytes())?;
            if macros[name].iter().any(|m| m.parameters != declared.parameters || m.variadic != declared.variadic || !same_tokens(&m.body, &expected)) {
                return Err(Bypass::CertificateMacroMismatch);
            }
        }
        let mut user_definitions = BTreeSet::new();
        for (path, file) in &tokens {
            if trusted.contains_key(path) { continue; }
            for definition in definitions(file)? {
                if !user_definitions.insert(definition.name) { return Err(Bypass::MacroDefinition); }
            }
            guard(file, &private, &public, &protected, &self.certificate.protected_prefixes)?;
        }
        Ok(Inspection { certificate_sha256: self.sha256.clone(), registered: self.registered(), input_count: closure.len(), token_count: token_total, private_macro_count: private.len(), public_macro_count: public.len() })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub certificate_sha256: String,
    pub registered: bool,
    pub input_count: usize,
    pub token_count: usize,
    pub private_macro_count: usize,
    pub public_macro_count: usize,
}
fn matches_file(bytes: Option<&Vec<u8>>, expected: &str, len: usize) -> bool {
    bytes.is_some_and(|b| b.len() == len && hash(b) == expected)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind { Identifier, Number, String, Character, Punctuation, Newline }
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token { kind: Kind, text: String, start: usize, end: usize }
fn same_tokens(a: &[Token], b: &[Token]) -> bool {
    a.iter().filter(|t| t.kind != Kind::Newline).map(|t| (&t.kind, &t.text)).eq(b.iter().filter(|t| t.kind != Kind::Newline).map(|t| (&t.kind, &t.text)))
}

/// Phase-2 splicing is applied before comments. Raw literals restore phase-2
/// characters: rather than implement that uncommon form, decline raw literals
/// containing a removed splice. This cannot hide following active tokens.
fn lex(input: &[u8]) -> Checked<Vec<Token>> {
    lex_with_limit(input, MAX_TOKENS)
}
fn lex_with_limit(input: &[u8], token_limit: usize) -> Checked<Vec<Token>> {
    if input.len() > MAX_INPUT_BYTES { return Err(Bypass::LexicalBudget); }
    let text = std::str::from_utf8(input).map_err(|_| Bypass::UnsupportedToken)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if ["??=", "??/", "??'", "??(", "??)", "??!", "??<", "??>", "??-"].iter().any(|s| text.contains(s)) { return Err(Bypass::UnsupportedToken); }
    let mut bytes = Vec::new(); bytes.try_reserve_exact(text.len()).map_err(|_| Bypass::LexicalBudget)?;
    let mut splices = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text.as_bytes()[i..];
        let splice = if rest.starts_with(b"\\\r\n") { 3 } else if rest.starts_with(b"\\\n") || rest.starts_with(b"\\\r") { 2 } else { 0 };
        if splice != 0 {
            if splices.len() == MAX_TOKENS { return Err(Bypass::LexicalBudget); }
            splices.try_reserve(1).map_err(|_| Bypass::LexicalBudget)?;
            splices.push(bytes.len()); i += splice;
        } else if rest.starts_with(b"\r\n") { bytes.push(b'\n'); i += 2; }
        else if rest[0] == b'\r' { bytes.push(b'\n'); i += 1; }
        else { bytes.push(rest[0]); i += 1; }
    }
    let mut out = Vec::new();
    i = 0;
    while i < bytes.len() {
        let start = i;
        let b = bytes[i];
        if matches!(b, b' ' | b'\t' | 11 | 12) { i += 1; continue; }
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            i += 2;
            while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                // A block comment is one whitespace token, including its
                // internal physical newlines. They do not end a directive.
                i += 1;
            }
            if i == bytes.len() { return Err(Bypass::UnsupportedToken); }
            i += 2; continue;
        }
        let raw_prefix = [b"u8R\"".as_slice(), b"uR\"", b"UR\"", b"LR\"", b"R\""].into_iter().find(|p| bytes[i..].starts_with(p));
        let kind;
        if let Some(prefix) = raw_prefix {
            let delimiter_start = i + prefix.len();
            let mut end = delimiter_start;
            while end < bytes.len() && bytes[end] != b'(' {
                if end - delimiter_start >= 16 || !matches!(bytes[end], 33..=126) || matches!(bytes[end], b')' | b'\\') { return Err(Bypass::UnsupportedToken); }
                end += 1;
            }
            if end == bytes.len() { return Err(Bypass::UnsupportedToken); }
            let mut closing = vec![b')']; closing.extend_from_slice(&bytes[delimiter_start..end]); closing.push(b'"');
            let offset = bytes[end + 1..].windows(closing.len()).position(|w| w == closing).ok_or(Bypass::UnsupportedToken)?;
            i = end + 1 + offset + closing.len();
            if splices.get(splices.partition_point(|at| *at < start)).is_some_and(|at| *at < i) { return Err(Bypass::UnsupportedToken); }
            kind = Kind::String;
        } else if let Some((prefix, quote)) = [b"u8\"".as_slice(), b"u\"", b"U\"", b"L\"", b"\"", b"u8'", b"u'", b"U'", b"L'", b"'"].into_iter().find(|p| bytes[i..].starts_with(p)).map(|p| (p, *p.last().unwrap())) {
            i += prefix.len();
            let mut closed = false;
            while i < bytes.len() {
                if bytes[i] == quote { i += 1; closed = true; break; }
                if bytes[i] == b'\n' || bytes[i] == 0 { return Err(Bypass::UnsupportedToken); }
                if bytes[i] == b'\\' { i += 1; }
                i += 1;
            }
            if !closed { return Err(Bypass::UnsupportedToken); }
            kind = if quote == b'"' { Kind::String } else { Kind::Character };
        } else if b.is_ascii_alphabetic_or_underscore() {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
            kind = Kind::Identifier;
        } else if b.is_ascii_digit() || b == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'\'') || matches!(c, b'+' | b'-') && matches!(bytes[i - 1], b'e' | b'E' | b'p' | b'P') { i += 1; } else { break; }
            }
            kind = Kind::Number;
        } else if b == b'\n' { i += 1; kind = Kind::Newline; }
        else {
            if !matches!(b, 33..=126) || b == b'\\' { return Err(Bypass::UnsupportedToken); }
            // C++ [lex.pptoken]: <:: followed by neither : nor > begins with
            // the single < token. Ordinary :: is maximal-munch, so its second
            // colon must not be misread as the :> alternative spelling.
            let less_scope = bytes[i..].starts_with(b"<::") && bytes.get(i + 3).is_none_or(|c| !matches!(c, b':' | b'>'));
            let multi = ["%:%:", "<<=", ">>=", "<=>", "->*", "...", "::", "##", ".*", "->", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "*=", "/=", "%=", "+=", "-=", "&=", "^=", "|="].into_iter().find(|p| bytes[i..].starts_with(p.as_bytes()));
            if let Some(p) = multi {
                if p == "%:%:" { return Err(Bypass::UnsupportedToken); }
                i += p.len();
            } else {
                if !less_scope && ["%:", "<:", ":>", "<%", "%>"].into_iter().any(|p| bytes[i..].starts_with(p.as_bytes())) { return Err(Bypass::UnsupportedToken); }
                i += 1;
            }
            kind = Kind::Punctuation;
        }
        if out.len() == token_limit { return Err(Bypass::LexicalBudget); }
        out.try_reserve(1).map_err(|_| Bypass::LexicalBudget)?;
        let value = std::str::from_utf8(&bytes[start..i]).map_err(|_| Bypass::UnsupportedToken)?;
        let mut text = String::new(); text.try_reserve_exact(value.len()).map_err(|_| Bypass::LexicalBudget)?; text.push_str(value);
        out.push(Token { kind, text, start, end: i });
    }
    Ok(out)
}

#[derive(Debug)]
struct MacroDefinition { name: String, parameters: Option<Vec<String>>, variadic: bool, body: Vec<Token> }
fn lines(tokens: &[Token]) -> impl Iterator<Item = &[Token]> { tokens.split(|t| t.kind == Kind::Newline).filter(|line| !line.is_empty()) }
fn definitions(tokens: &[Token]) -> Checked<Vec<MacroDefinition>> {
    let mut result = Vec::new();
    for line in lines(tokens) {
        if line.len() < 2 || line[0].text != "#" || line[1].text != "define" { continue; }
        let name = line.get(2).filter(|t| t.kind == Kind::Identifier).ok_or(Bypass::UnsupportedToken)?;
        let mut position = 3;
        let mut parameters = None;
        let mut variadic = false;
        if line.get(position).is_some_and(|t| t.text == "(" && t.start == name.end) {
            position += 1;
            let mut params = Vec::new();
            while line.get(position).is_some_and(|t| t.text != ")") {
                let token = line.get(position).ok_or(Bypass::UnsupportedToken)?;
                if token.text == "..." { variadic = true; position += 1; break; }
                if token.kind != Kind::Identifier || params.contains(&token.text) { return Err(Bypass::UnsupportedToken); }
                params.push(token.text.clone()); position += 1;
                // GNU named varargs are outside this profile.
                if line.get(position).is_some_and(|t| t.text == ")") { break; }
                if line.get(position).is_none_or(|t| t.text != ",") { return Err(Bypass::UnsupportedToken); }
                position += 1;
            }
            if line.get(position).is_none_or(|t| t.text != ")") || params.len() > 64 { return Err(Bypass::UnsupportedToken); }
            position += 1; parameters = Some(params);
        }
        result.push(MacroDefinition { name: name.text.clone(), parameters, variadic, body: line[position..].to_vec() });
        if result.len() > MAX_MACROS { return Err(Bypass::LexicalBudget); }
    }
    Ok(result)
}
fn root_hazard(t: &Token) -> bool {
    t.text == "##" || t.kind == Kind::Identifier && matches!(t.text.as_str(),
        "__DATE__" | "__TIME__" | "__TIMESTAMP__" | "_Pragma" | "__pragma" |
        "asm" | "__asm" | "__asm__" | "__has_include" | "__has_include_next" | "__has_embed")
}
fn reserved_macro_name(name: &str) -> bool {
    name.starts_with('_') || "alignas alignof and and_eq asm atomic_cancel atomic_commit atomic_noexcept auto bitand bitor bool break case catch char char8_t char16_t char32_t class compl concept const consteval constexpr constinit const_cast continue co_await co_return co_yield decltype default delete do double dynamic_cast else enum explicit export extern false float for friend goto if inline int long mutable namespace new noexcept not not_eq nullptr operator or or_eq private protected public reflexpr register reinterpret_cast requires return short signed sizeof static static_assert static_cast struct switch synchronized template this thread_local throw true try typedef typeid typename union unsigned using virtual void volatile wchar_t while xor xor_eq".split(' ').any(|word| word == name)
}

fn guard(tokens: &[Token], private: &BTreeSet<String>, public: &BTreeSet<String>, protected: &BTreeSet<String>, prefixes: &[String]) -> Checked<()> {
    for token in tokens {
        if token.text == "##" { return Err(Bypass::MacroDefinition); }
        if token.kind != Kind::Identifier { continue; }
        match token.text.as_str() {
            "__DATE__" | "__TIME__" | "__TIMESTAMP__" => return Err(Bypass::Temporal),
            "_Pragma" | "__pragma" => return Err(Bypass::Directive),
            "asm" | "__asm" | "__asm__" => return Err(Bypass::Assembly),
            // A later policy can admit represented literal probes. Initially all
            // user probes decline; compiler-internal output bookkeeping is not a
            // semantic probe and still needs independent WASI classification.
            "__has_include" | "__has_include_next" | "__has_embed" => return Err(Bypass::Probe),
            _ => (),
        }
        if (private.contains(&token.text) && !public.contains(&token.text)) || prefixes.iter().any(|p| token.text.starts_with(p)) { return Err(Bypass::PrivateMacro); }
    }
    let lines: Vec<_> = lines(tokens).collect();
    let definitions = definitions(tokens)?;
    let mut guard_name = None;
    if let [first, second, ..] = lines.as_slice() {
        if first.len() == 3 && first[0].text == "#" && first[1].text == "ifndef" && first[2].kind == Kind::Identifier &&
            second.len() == 3 && second[0].text == "#" && second[1].text == "define" && second[2].text == first[2].text &&
            lines.last().is_some_and(|l| l.len() == 2 && l[0].text == "#" && l[1].text == "endif") {
            let name = &first[2].text;
            if !protected.contains(name) && !reserved_macro_name(name) && !prefixes.iter().any(|p| name.starts_with(p)) &&
                tokens.iter().filter(|t| t.kind == Kind::Identifier && &t.text == name).count() == 2 {
                guard_name = Some(name);
            }
        }
    }
    let mut depth = 0usize;
    for (index, line) in lines.iter().enumerate() {
        if line[0].text != "#" { continue; }
        let Some(directive) = line.get(1) else { continue; };
        let rest = &line[2..];
        match directive.text.as_str() {
            "define" if index == 1 && rest.len() == 1 && guard_name == Some(&rest[0].text) => (),
            "define" => {
                let definition = definitions.iter().find(|d| rest.first().is_some_and(|t| t.text == d.name)).ok_or(Bypass::MacroDefinition)?;
                // No user macro can replace a token consumed by trusted code,
                // override a helper/configuration guard, or fabricate a token.
                // Arguments and aliases are already scanned above for hazards.
                if protected.contains(&definition.name) || private.contains(&definition.name) || public.contains(&definition.name) ||
                    reserved_macro_name(&definition.name) || prefixes.iter().any(|p| definition.name.starts_with(p)) ||
                    definition.body.iter().any(|t| matches!(t.text.as_str(), "#" | "##")) {
                    return Err(Bypass::MacroDefinition);
                }
            }
            "undef" => return Err(Bypass::MacroDefinition),
            "pragma" if rest.len() == 1 && rest[0].text == "once" => (),
            "include" => {
                let include = if rest.len() == 1 && rest[0].kind == Kind::String && rest[0].text.starts_with('"') {
                    rest[0].text[1..rest[0].text.len() - 1].to_string()
                } else if rest.len() >= 3 && rest[0].text == "<" && rest.last().is_some_and(|t| t.text == ">") {
                    rest[1..rest.len() - 1].iter().map(|t| t.text.as_str()).collect()
                } else { return Err(Bypass::Include); };
                if include.is_empty() || include.contains('\\') || include.contains('\n') ||
                    matches!(include.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str(), "c" | "cc" | "cp" | "cpp" | "cxx" | "c++" | "o" | "tmp") { return Err(Bypass::Include); }
            }
            "line" if matches!(rest, [number] if number.kind == Kind::Number && number.text.bytes().all(|b| b.is_ascii_digit())) ||
                matches!(rest, [number, name] if number.kind == Kind::Number && number.text.bytes().all(|b| b.is_ascii_digit()) && name.kind == Kind::String && name.text.starts_with('"')) => (),
            "if" | "ifdef" | "ifndef" => { depth += 1; if depth > 256 { return Err(Bypass::Directive); } },
            "elif" | "else" if depth > 0 && !(guard_name.is_some() && depth == 1) => (),
            "endif" if depth > 0 => {
                depth -= 1;
                if guard_name.is_some() && depth == 0 && index != lines.len() - 1 { return Err(Bypass::MacroDefinition); }
            }
            _ => return Err(Bypass::Directive),
        }
    }
    if depth != 0 { return Err(Bypass::Directive); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "#ifndef TRUSTED_H\n#define TRUSTED_H\n#define CAT_IMPL(a,b) a##b\n#define CAT(a,b) CAT_IMPL(a,b)\n#define WRAP_I(n) static int CAT(fixed_,n);\n#define WRAP() WRAP_I(__COUNTER__)\n#define CHECK(x,...) check((x),#x,##__VA_ARGS__)\n#define HARMLESS 1\n#endif\n";
    fn toolchain() -> Toolchain { Toolchain { compiler_sha256: "a".repeat(64), sysroot_sha256: "b".repeat(64), exceptions_sha256: "c".repeat(64) } }
    fn certificate() -> Certificate {
        Certificate {
            schema_version: 1, policy: PROFILE.into(), required_toolchain: toolchain(),
            trusted_headers: vec![FileIdentity { path: "/trusted.h".into(), sha256: hash(HEADER.as_bytes()), bytes: HEADER.len() }],
            macros: vec![
                MacroCertificate { header_path: "/trusted.h".into(), name: "WRAP".into(), parameters: Some(vec![]), variadic: false, replacement: "WRAP_I(__COUNTER__)".into(), visibility: Visibility::Public },
                MacroCertificate { header_path: "/trusted.h".into(), name: "CHECK".into(), parameters: Some(vec!["x".into()]), variadic: true, replacement: "check((x),#x,##__VA_ARGS__)".into(), visibility: Visibility::Public },
            ],
            protected_identifiers: vec![], protected_prefixes: vec!["fixed_".into()], pch: None,
        }
    }
    fn policy(c: Certificate) -> Policy { Policy::parse(&serde_json::to_vec(&c).unwrap()).unwrap() }
    fn inspect(code: &str) -> Checked<Inspection> {
        let actual = BTreeMap::from([("/main.cpp".into(), code.as_bytes().to_vec()), ("/trusted.h".into(), HEADER.as_bytes().to_vec())]);
        policy(certificate()).inspect_closure(&actual, &["/main.cpp".into(), "/trusted.h".into()], "/main.cpp", &["/main.cpp".into()], &toolchain(), None)
    }
    fn bare(code: &str) -> Checked<()> { guard(&lex(code.as_bytes())?, &BTreeSet::new(), &BTreeSet::new(), &BTreeSet::new(), &[]) }

    #[test] #[ignore = "native evidence diagnostic, needs STOCK_REQUEST; never product authority"]
    fn diagnose_actual_inventory() {
        let request: serde_json::Value = serde_json::from_slice(&std::fs::read(std::env::var("STOCK_REQUEST").unwrap()).unwrap()).unwrap();
        for file in request["files"].as_array().unwrap() {
            let path = file["path"].as_str().unwrap();
            if path.ends_with(".pch") { continue; }
            let bytes = std::fs::read(file["diskPath"].as_str().unwrap()).unwrap();
            match lex(&bytes) {
                Ok(tokens) => { if let Err(reason) = definitions(&tokens) { println!("definitions {path}: {reason:?}"); } },
                Err(reason) => println!("lex {path}: {reason:?}"),
            }
        }
    }

    #[test] fn stored_admission_rejects_forged_schema_paths_duplicates_and_budgets() {
        let certificate = "a".repeat(64);
        let record = serde_json::json!({"schema":1,"certificateSha256":certificate,"source":"/main.cpp","dependencies":["/main.cpp","/a.h"]});
        let raw = record.to_string();
        assert!(StoredAdmission::parse(&raw,&certificate,"/main.cpp",raw.len()).is_ok());
        assert_eq!(StoredAdmission::parse(&raw,&certificate,"/main.cpp",raw.len()-1).unwrap_err(),"stock-metadata-budget");
        for (field, value) in [("schema",serde_json::json!(2)),("certificateSha256",serde_json::json!("b".repeat(64))),
            ("source",serde_json::json!("/other.cpp")),("extra",serde_json::json!(0)),
            ("dependencies",serde_json::json!(["/main.cpp","/main.cpp"])),
            ("dependencies",serde_json::json!(["/main.cpp","/a/../b"])),
            ("dependencies",serde_json::json!(["/a.h"]))] {
            let mut changed=record.clone(); changed[field]=value;
            assert!(StoredAdmission::parse(&changed.to_string(),&certificate,"/main.cpp",MAX_CERTIFICATE_BYTES).is_err());
        }
        let mut duplicate=raw;duplicate.pop();duplicate.push_str(",\"schema\":1}");
        assert!(StoredAdmission::parse(&duplicate,&certificate,"/main.cpp",MAX_CERTIFICATE_BYTES).is_err());
    }
    #[test] fn generated_trusted_identifier_certificate_blocks_safe_looking_macro_poison() {
        let mut c=certificate(); c.protected_identifiers.push("nie220100".into());
        let actual=BTreeMap::from([("/main.cpp".into(),b"#define nie220100 replacement\nWRAP()".to_vec()),("/trusted.h".into(),HEADER.as_bytes().to_vec())]);
        assert_eq!(policy(c).inspect_closure(&actual,&["/main.cpp".into(),"/trusted.h".into()],"/main.cpp",&["/main.cpp".into()],&toolchain(),None).unwrap_err(),Bypass::MacroDefinition);
    }
    #[test] fn exact_wrappers_and_counter_are_potentially_eligible_but_never_authority() {
        let result = inspect("#include \"trusted.h\"\nWRAP() CHECK(HARMLESS); int x=__COUNTER__;\n").unwrap();
        assert!(!result.registered);
        assert_eq!(result.public_macro_count, 2);
        assert_eq!(policy(certificate()).require_registered(), Err(Bypass::CertificateUnregistered));
    }
    #[test] fn private_paste_transitive_alias_and_nested_arguments_decline() {
        for code in ["CAT_IMPL(__TI,ME__)", "CAT(__TIM,ESTAMP__)", "WRAP_I(__COUNTER__)", "CHECK(true,CAT(__DA,TE__))"] {
            assert_eq!(inspect(code).unwrap_err(), Bypass::PrivateMacro, "{code}");
        }
    }
    #[test] fn colliding_definitions_and_generated_names_cannot_poison_rescan() {
        for code in ["#define check evil\nCHECK(true)", "#undef HARMLESS\n", "#define fixed_0 changed\nWRAP()", "#define fixed_ changed\nWRAP()", "int fixed_999=0;", "#define WRAP CHECK\n", "#define __COUNTER__ 1\n", "#define static replacement\n", "#define int replacement\n"] {
            assert!(inspect(code).is_err(), "{code}");
        }
    }
    #[test] fn strict_empty_include_guard_is_isolated() {
        assert!(inspect("#ifndef LOCAL_HEADER_GUARD\n#define LOCAL_HEADER_GUARD\nint x;\n#endif\n").is_ok());
        for code in [
            "#ifndef check\n#define check\n#endif", "#ifndef __COUNTER__\n#define __COUNTER__\n#endif",
            "#ifndef LOCAL\n#define LOCAL\n#else\nint x;\n#endif", "#ifndef LOCAL\n#define LOCAL\n#endif\n#define LOCAL",
        ] { assert!(inspect(code).is_err(), "{code}"); }
    }
    #[test] fn safe_user_macros_and_public_aliases_need_no_source_allowlist() {
        for code in ["#define LOCAL_CONSTANT 1\nint x=LOCAL_CONSTANT;", "#define TWICE(x) ((x)+(x))\nint x=TWICE(2);", "#define MY_CHECK CHECK\nMY_CHECK(true);", "#define NEXT_COUNTER __COUNTER__\nint x=NEXT_COUNTER;", "#ifndef LOCAL_GUARD\n#define LOCAL_GUARD\n#define LOCAL_CONSTANT 1\n#endif"] {
            assert!(inspect(code).is_ok(), "{code}");
        }
        for code in ["#define ALIAS CAT\nALIAS(a,b)", "#define MY_CHECK CHECK\nMY_CHECK(true,CAT(__TI,ME__))", "#define BAD(a,b) a##b\n", "#define STRINGIFY(x) #x\n", "#define X 1\n#define X 1\n", "#define LOCAL_CLOCK __TIME__\n"] {
            assert!(inspect(code).is_err(), "{code}");
        }
    }
    #[test] fn temporal_spellings_are_tokens_not_literal_substrings() {
        assert!(inspect("const char* x=\"__TIME__ __TIMESTAMP__\"; /* __DATE__ */ int stable__TIME__suffix=1;").is_ok());
        for code in ["__TIME__", "__TI\\\nME__", "__DATE__", "__TIMESTAMP__", "CHECK(__TIME__)"] { assert_eq!(inspect(code).unwrap_err(), Bypass::Temporal, "{code}"); }
    }
    #[test] fn raw_strings_do_not_hide_following_live_tokens() {
        for prefix in ["R", "u8R", "uR", "UR", "LR"] {
            let literal = format!("auto x={prefix}\"tag(/* fake #define X __TIME__\n// ## _Pragma )tag\";\nint x=__COUNTER__;");
            assert!(inspect(&literal).is_ok(), "{literal}");
            assert_eq!(inspect(&(literal + "\n__TIME__")).unwrap_err(), Bypass::Temporal);
        }
        assert!(inspect("auto x=R\"a( text )\\\na\"; /*)a\"; __TIME__; //").is_err());
        assert!(inspect("auto x=R\"abcdefghijklmnopq(text)abcdefghijklmnopq\";").is_err());
    }
    #[test] fn comments_splices_numbers_and_encoded_literals_are_bounded() {
        assert!(inspect("// __TIME__ \\\n__TIMESTAMP__\nint x=1'000; double y=0x1.fp+2; char c='\\\''; auto s=u8\"ok\";").is_ok());
        assert!(inspect("/* unterminated").is_err());
        assert!(inspect("\"unterminated").is_err());
        assert!(inspect("1'000; __TIME__").is_err());
        assert!(inspect("/* hi */ __TI/**/ME__").is_ok()); // two identifiers, not a macro token
    }
    #[test] fn block_comment_newlines_are_whitespace_not_directive_boundaries() {
        assert!(lex_with_limit(b"/*\n\n\n*/", 0).is_ok());
        assert_eq!(lex_with_limit(b"a b /*\n*/ c", 2), Err(Bypass::LexicalBudget));
        assert_eq!(lex_with_limit(b"a", 0), Err(Bypass::LexicalBudget));
        for code in ["# /* comment\n*/ define check changed\n", "# /*\n*/ undef HARMLESS\n", "# /*\n*/ pragma clang diagnostic ignored \"-Wunused-variable\"\n"] {
            assert!(inspect(code).is_err(), "{code}");
        }
        assert!(inspect("# /*\n*/ define LOCAL_CONSTANT 43\nint a=LOCAL_CONSTANT;\n").is_ok());
    }
    #[test] fn alternate_directives_unicode_and_unknown_tokens_decline() {
        for code in ["%:define X 1", "??=define X 1", "\\u005f_TIME__", "int café;", "#pragma GCC system_header", "_Pragma(\"once\")", "#warning hi", "# 1 \"x\" 3", "#include NEXT", "#include_next <x>"] { assert!(bare(code).is_err(), "{code}"); }
        assert!(bare("#pragma once\n#line 12 \"original.cpp\"\nint x;\n").is_ok());
    }
    #[test] fn cpp_less_scope_exception_and_maximal_punctuation_match_compiler_rules() {
        let tokens = lex(b"static_cast<::std::x>(y); A<B::> v;").unwrap();
        assert!(tokens.windows(3).any(|t| t[0].text == "<" && t[1].text == "::" && t[2].text == "std"));
        assert!(tokens.windows(2).any(|t| t[0].text == "::" && t[1].text == ">"));
        assert!(lex(b"<::>").is_err());
        assert!(lex(b"<:::").is_err());
        assert!(lex(b"<: :>").is_err());
    }
    #[test] fn direct_and_aliased_hazards_decline_even_in_unreachable_code() {
        for code in ["asm(\"nop\")", "__asm__(\"nop\")", "#if 0\n__TIME__\n#endif", "#if __has_include(\"optional.h\")\n#endif", "#if __has_embed(\"x\")\n#endif"] { assert!(inspect(code).is_err(), "{code}"); }
    }
    #[test] fn macro_definition_matching_retains_function_vs_object_distinction() {
        let defs = definitions(&lex(b"#define F(x) x\n#define G (x)\n#define V(x,...) x,##__VA_ARGS__\n#define H/**/(x) x\n").unwrap()).unwrap();
        assert_eq!(defs[0].parameters, Some(vec!["x".into()]));
        assert_eq!(defs[1].parameters, None);
        assert!(defs[2].variadic);
        assert_eq!(defs[2].parameters, Some(vec!["x".into()]));
        assert_eq!(defs[3].parameters, None);
    }
    #[test] fn mutated_wrapper_body_and_unknown_certificate_fields_decline() {
        let mut c = certificate(); c.macros[0].replacement = "CAT(__TI,ME__)".into();
        let actual = BTreeMap::from([("/main.cpp".into(), b"WRAP()".to_vec()), ("/trusted.h".into(), HEADER.as_bytes().to_vec())]);
        assert_eq!(policy(c).inspect_closure(&actual, &["/main.cpp".into(), "/trusted.h".into()], "/main.cpp", &["/main.cpp".into()], &toolchain(), None).unwrap_err(), Bypass::CertificateMacroMismatch);
        let raw = serde_json::to_string(&certificate()).unwrap();
        assert!(Policy::parse(raw.replacen('{', "{\"trusted\":true,", 1).as_bytes()).is_err());
        assert!(Policy::parse(raw.replacen('{', "{\"schemaVersion\":1,", 1).as_bytes()).is_err());
        assert!(Policy::parse(raw.replace("\"pch\":null", "\"pchX\":null").as_bytes()).is_err());
        assert!(Policy::parse(&vec![b' '; MAX_CERTIFICATE_BYTES + 1]).is_err());
    }
    #[test] fn exact_raw_certificate_bytes_define_identity() {
        let raw = serde_json::to_vec(&certificate()).unwrap();
        let mut whitespace = raw.clone(); whitespace.push(b'\n');
        assert_ne!(Policy::parse(&raw).unwrap().sha256(), Policy::parse(&whitespace).unwrap().sha256());
        assert!(!Policy::parse(&raw).unwrap().registered());
    }
    #[test] fn unused_optional_headers_do_not_create_guest_inputs() {
        let c = policy(certificate());
        let actual = BTreeMap::from([("/main.cpp".into(), b"int x=__COUNTER__;".to_vec())]);
        assert!(c.inspect_closure(&actual, &["/main.cpp".into()], "/main.cpp", &["/main.cpp".into()], &toolchain(), None).is_ok());
    }
    #[test] fn changed_trusted_header_and_cross_selected_input_decline() {
        let c = policy(certificate());
        let actual = BTreeMap::from([("/main.cpp".into(), b"int x;".to_vec()), ("/trusted.h".into(), HEADER.replace("check", "other").into_bytes()), ("/other.cpp".into(), b"int x;".to_vec())]);
        assert_eq!(c.inspect_closure(&actual, &["/main.cpp".into(), "/trusted.h".into()], "/main.cpp", &["/main.cpp".into()], &toolchain(), None).unwrap_err(), Bypass::TrustedInputMismatch);
        assert_eq!(c.inspect_closure(&actual, &["/main.cpp".into(), "/other.cpp".into()], "/main.cpp", &["/main.cpp".into(), "/other.cpp".into()], &toolchain(), None).unwrap_err(), Bypass::InputClosure);
        assert!(inspect("#include \"other.cpp\"").is_err());
    }
    #[test] fn exact_pch_requires_bound_metadata_proof_and_every_input_byte() {
        let mut c = certificate();
        c.pch = Some(PchCertificate { path: "/known.pch".into(), sha256: hash(b"parsed-exact-pch"), bytes: 16, flags: vec!["-fno-pch-timestamp".into()], inputs: c.trusted_headers.clone(), metadata_proof: MetadataProof { has_timestamps: false, input_count: 1, stored_times_zero: true, has_imports: false, parser_evidence_sha256: "d".repeat(64), macro_state_evidence_sha256: "e".repeat(64) } });
        c.pch.as_mut().unwrap().bytes = b"parsed-exact-pch".len();
        let mut actual = BTreeMap::from([("/main.cpp".into(), b"WRAP()".to_vec()), ("/trusted.h".into(), HEADER.as_bytes().to_vec()), ("/known.pch".into(), b"parsed-exact-pch".to_vec())]);
        let flags = vec!["-fno-pch-timestamp".into()];
        let p = policy(c.clone());
        assert!(p.inspect_closure(&actual, &["/main.cpp".into(), "/known.pch".into()], "/main.cpp", &["/main.cpp".into()], &toolchain(), Some(("/known.pch", &flags))).is_ok());
        actual.get_mut("/known.pch").unwrap()[0] ^= 1;
        assert_eq!(p.inspect_closure(&actual, &["/main.cpp".into()], "/main.cpp", &["/main.cpp".into()], &toolchain(), Some(("/known.pch", &flags))).unwrap_err(), Bypass::PchMismatch);
        c.pch.as_mut().unwrap().metadata_proof.has_timestamps = true;
        assert!(Policy::parse(&serde_json::to_vec(&c).unwrap()).is_err());
    }
}
