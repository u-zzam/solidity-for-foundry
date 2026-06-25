//! Engine A: drive `foundry-compilers` exactly like `forge build` and return
//! solc's diagnostics. We read the handful of `foundry.toml` fields that affect
//! compilation (solc version, layout, optimizer, via-ir, evm version,
//! remappings) and let foundry-compilers do the rest: import resolution, svm
//! version management, and the solc invocation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};

use foundry_compilers::artifacts::{Error as SolcError, EvmVersion, Optimizer, Remapping, Settings};
use foundry_compilers::solc::{Solc, SolcCompiler, SolcLanguage, SolcSettings};
use foundry_compilers::{ProjectBuilder, ProjectPathsConfig};
use semver::Version;

/// solc warning codes `forge build` suppresses by default: license (1878),
/// code-size (5574), init-code-size (3860), transient-storage (2394).
const DEFAULT_IGNORED_CODES: [u64; 4] = [1878, 5574, 3860, 2394];

/// foundry.toml `ignored_error_codes` accepts either a numeric code or a named
/// alias; map the aliases forge defines, fall back to parsing an integer.
fn error_code(s: &str) -> Option<u64> {
    match s {
        "license" => Some(1878),
        "code-size" => Some(5574),
        "init-code-size" => Some(3860),
        "transient-storage" => Some(2394),
        _ => s.parse().ok(),
    }
}

/// Format `src` with `forge fmt`, honoring the project's `[fmt]` config (via
/// `--root`). Returns `None` if forge is unavailable or the input doesn't parse.
pub fn format(root: Option<&Path>, src: &str) -> Option<String> {
    let mut cmd = Command::new("forge");
    cmd.arg("fmt").arg("--raw").arg("-");
    if let Some(r) = root {
        cmd.arg("--root").arg(r);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());

    let mut child = cmd.spawn().ok()?;
    // Feed stdin from a thread so a large formatted result on stdout can't
    // fill the pipe and deadlock us mid-write.
    let mut stdin = child.stdin.take()?;
    let buf = src.as_bytes().to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&buf));
    let out = child.wait_with_output().ok()?;
    let _ = writer.join();

    if !out.status.success() {
        return None;
    }
    let formatted = String::from_utf8(out.stdout).ok()?;
    (!formatted.is_empty()).then_some(formatted)
}

/// Walk up from a file (or dir) to the nearest directory containing `foundry.toml`.
pub fn locate_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() { Some(start) } else { start.parent() };
    while let Some(d) = dir {
        if d.join("foundry.toml").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// The subset of `foundry.toml [profile.default]` that affects diagnostics.
struct Config {
    solc: Option<Version>,
    src: PathBuf,
    tests: PathBuf,
    scripts: PathBuf,
    libs: Vec<PathBuf>,
    optimizer: Option<bool>,
    optimizer_runs: Option<usize>,
    via_ir: Option<bool>,
    evm_version: Option<EvmVersion>,
    /// Paths whose warnings forge suppresses (`ignored_warnings_from`).
    ignored_warnings_from: Vec<PathBuf>,
    /// Warning codes forge suppresses (`ignored_error_codes`).
    ignored_error_codes: Vec<u64>,
    /// Explicit remappings declared inline in foundry.toml.
    remappings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            solc: None,
            src: "src".into(),
            tests: "test".into(),
            scripts: "script".into(),
            libs: vec!["lib".into()],
            optimizer: None,
            optimizer_runs: None,
            via_ir: None,
            evm_version: None,
            ignored_warnings_from: Vec::new(),
            ignored_error_codes: DEFAULT_IGNORED_CODES.to_vec(),
            remappings: Vec::new(),
        }
    }
}

fn parse_config(root: &Path) -> Config {
    let mut cfg = Config::default();
    let Ok(text) = std::fs::read_to_string(root.join("foundry.toml")) else {
        return cfg;
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return cfg;
    };
    let Some(p) = table
        .get("profile")
        .and_then(|p| p.get("default"))
        .and_then(|d| d.as_table())
    else {
        return cfg;
    };

    let str_field = |k: &str| p.get(k).and_then(|v| v.as_str());
    if let Some(v) = str_field("solc").or_else(|| str_field("solc_version")) {
        // foundry accepts a bare version or a path; only a version pins svm.
        cfg.solc = Version::parse(v.trim_start_matches(['=', '^', '~', 'v', ' '])).ok();
    }
    if let Some(s) = str_field("src") {
        cfg.src = s.into();
    }
    if let Some(s) = str_field("test") {
        cfg.tests = s.into();
    }
    if let Some(s) = str_field("script") {
        cfg.scripts = s.into();
    }
    if let Some(arr) = p.get("libs").and_then(|v| v.as_array()) {
        cfg.libs = arr.iter().filter_map(|v| v.as_str()).map(PathBuf::from).collect();
    }
    if let Some(b) = p.get("optimizer").and_then(|v| v.as_bool()) {
        cfg.optimizer = Some(b);
    }
    if let Some(n) = p.get("optimizer_runs").and_then(|v| v.as_integer()) {
        cfg.optimizer_runs = Some(n as usize);
    }
    if let Some(b) = p.get("via_ir").and_then(|v| v.as_bool()) {
        cfg.via_ir = Some(b);
    }
    if let Some(s) = str_field("evm_version") {
        cfg.evm_version = EvmVersion::from_str(s).ok();
    }
    if let Some(arr) = p.get("ignored_warnings_from").and_then(|v| v.as_array()) {
        cfg.ignored_warnings_from =
            arr.iter().filter_map(|v| v.as_str()).map(PathBuf::from).collect();
    }
    if let Some(arr) = p.get("ignored_error_codes").and_then(|v| v.as_array()) {
        // An explicit list replaces forge's defaults.
        cfg.ignored_error_codes = arr
            .iter()
            .filter_map(|v| match v {
                toml::Value::Integer(n) => Some(*n as u64),
                toml::Value::String(s) => error_code(s),
                _ => None,
            })
            .collect();
    }
    if let Some(arr) = p.get("remappings").and_then(|v| v.as_array()) {
        cfg.remappings = arr.iter().filter_map(|v| v.as_str()).map(String::from).collect();
    }
    cfg
}

/// Resolve remappings the way forge does: auto-detect from each lib, then let
/// inline foundry.toml remappings and `remappings.txt` override by key.
fn resolve_remappings(root: &Path, cfg: &Config) -> Vec<Remapping> {
    let mut map: HashMap<(Option<String>, String), Remapping> = HashMap::new();
    let mut insert = |mut r: Remapping| {
        if Path::new(&r.path).is_relative() {
            r.path = root.join(&r.path).to_string_lossy().into_owned();
        }
        map.insert((r.context.clone(), r.name.clone()), r);
    };

    for lib in &cfg.libs {
        for r in Remapping::find_many(&root.join(lib)) {
            insert(r);
        }
    }
    for line in &cfg.remappings {
        if let Ok(r) = Remapping::from_str(line) {
            insert(r);
        }
    }
    if let Ok(text) = std::fs::read_to_string(root.join("remappings.txt")) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Ok(r) = Remapping::from_str(line) {
                insert(r);
            }
        }
    }
    map.into_values().collect()
}

/// Compile the project rooted at `root` and return solc's diagnostics, applying
/// the same warning suppression `forge build` does (`ignored_error_codes` and
/// `ignored_warnings_from`).
pub fn compile(root: &Path) -> Result<Vec<SolcError>, String> {
    let cfg = parse_config(root);

    let paths: ProjectPathsConfig<SolcLanguage> = ProjectPathsConfig::builder()
        .root(root)
        .sources(root.join(&cfg.src))
        .tests(root.join(&cfg.tests))
        .scripts(root.join(&cfg.scripts))
        .libs(cfg.libs.iter().map(|l| root.join(l)))
        .remappings(resolve_remappings(root, &cfg))
        .build()
        .map_err(|e| e.to_string())?;

    // Isolate our build cache/artifacts from the user's `out/` so we never race
    // `forge build`; `cache/` is gitignored by every Foundry project.
    let mut paths = paths;
    let work = root.join("cache").join("solidity-lsp");
    paths.cache = work.join("solidity-files-cache.json");
    paths.artifacts = work.join("out");
    paths.build_infos = work.join("out").join("build-info");

    let mut settings = Settings::default();
    settings.optimizer = Optimizer {
        enabled: cfg.optimizer,
        runs: cfg.optimizer_runs,
        details: None,
    };
    settings.via_ir = cfg.via_ir;
    if let Some(evm) = cfg.evm_version {
        settings.evm_version = Some(evm);
    }

    let compiler = match &cfg.solc {
        Some(v) => SolcCompiler::Specific(Solc::find_or_install(v).map_err(|e| e.to_string())?),
        None => SolcCompiler::AutoDetect,
    };

    let project = ProjectBuilder::<SolcCompiler>::default()
        .paths(paths)
        .settings(SolcSettings { settings, cli_settings: Default::default() })
        .build(compiler)
        .map_err(|e| e.to_string())?;

    let mut errors = project.compile().map_err(|e| e.to_string())?.into_output().errors;

    // Apply forge's display filtering: suppress warnings with an ignored code or
    // from an ignored path. Errors always surface.
    let ignored_codes: HashSet<u64> = cfg.ignored_error_codes.iter().copied().collect();
    let ignored_paths: Vec<PathBuf> = cfg.ignored_warnings_from.iter().map(|p| root.join(p)).collect();
    errors.retain(|e| {
        if e.severity.is_error() {
            return true;
        }
        if e.error_code.is_some_and(|c| ignored_codes.contains(&c)) {
            return false;
        }
        if let Some(loc) = &e.source_location {
            let file = root.join(&loc.file);
            if ignored_paths.iter().any(|ig| file.starts_with(ig)) {
                return false;
            }
        }
        true
    });

    Ok(errors)
}
