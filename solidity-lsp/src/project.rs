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

use foundry_compilers::artifacts::output_selection::OutputSelection;
use foundry_compilers::artifacts::{
    Error as SolcError, EvmVersion, Optimizer, Remapping, Settings, SolcInput, Source,
};
use foundry_compilers::solc::{Solc, SolcCompiler, SolcLanguage, SolcSettings};
use foundry_compilers::{ProjectBuilder, ProjectPathsConfig};
use semver::Version;

/// One source file's solc AST, as raw JSON for generic traversal, tagged with
/// the source-unit index solc uses in `src` locations.
pub struct SourceAst {
    pub index: usize,
    pub path: PathBuf,
    pub ast: serde_json::Value,
}

/// Result of compiling a project: diagnostics plus the typed ASTs to index.
pub struct CompileOutput {
    pub errors: Vec<SolcError>,
    pub sources: Vec<SourceAst>,
}

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

/// A fix `forge lint` suggests for a finding (replace a byte span with text).
pub struct Suggestion {
    pub byte_start: usize,
    pub byte_end: usize,
    pub text: String,
}

/// One `forge lint` finding (a solar lint), located by byte offsets.
pub struct LintFinding {
    pub file: PathBuf,
    pub byte_start: usize,
    pub byte_end: usize,
    pub level: String,
    pub code: Option<String>,
    pub message: String,
    pub suggestion: Option<Suggestion>,
}

/// Run `forge lint --json` and parse its rustc-style NDJSON diagnostics. forge
/// applies the project's `[lint]` config (ignored paths, excluded lints), so the
/// findings match `forge lint` exactly. Returns empty if forge is unavailable.
pub fn lint(root: &Path) -> Vec<LintFinding> {
    let Ok(out) = Command::new("forge")
        .arg("lint")
        .arg("--json")
        .arg("--root")
        .arg(root)
        .output()
    else {
        return Vec::new();
    };
    // Diagnostics may land on either stream depending on forge version.
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&out.stderr));

    let mut findings = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("$message_type").and_then(|m| m.as_str()) != Some("diagnostic") {
            continue;
        }
        let spans = v.get("spans").and_then(|s| s.as_array());
        let span = spans.and_then(|arr| {
            arr.iter()
                .find(|sp| sp.get("is_primary").and_then(|p| p.as_bool()).unwrap_or(false))
                .or_else(|| arr.first())
        });
        let Some(span) = span else {
            continue;
        };
        let (Some(file), Some(bs), Some(be)) = (
            span.get("file_name").and_then(|f| f.as_str()),
            span.get("byte_start").and_then(|b| b.as_u64()),
            span.get("byte_end").and_then(|b| b.as_u64()),
        ) else {
            continue;
        };
        // A `consider using` child carries the suggested replacement + its span.
        let suggestion = v.get("children").and_then(|c| c.as_array()).and_then(|children| {
            children.iter().find_map(|child| {
                child.get("spans").and_then(|s| s.as_array()).and_then(|spans| {
                    spans.iter().find_map(|sp| {
                        Some(Suggestion {
                            byte_start: sp.get("byte_start")?.as_u64()? as usize,
                            byte_end: sp.get("byte_end")?.as_u64()? as usize,
                            text: sp.get("suggested_replacement")?.as_str()?.to_string(),
                        })
                    })
                })
            })
        });

        findings.push(LintFinding {
            file: PathBuf::from(file),
            byte_start: bs as usize,
            byte_end: be as usize,
            level: v.get("level").and_then(|l| l.as_str()).unwrap_or("note").to_string(),
            code: v
                .get("code")
                .and_then(|c| c.get("code"))
                .and_then(|c| c.as_str())
                .map(String::from),
            message: v.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string(),
            suggestion,
        });
    }
    findings
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

fn build_paths(root: &Path, cfg: &Config) -> Result<ProjectPathsConfig<SolcLanguage>, String> {
    ProjectPathsConfig::builder()
        .root(root)
        .sources(root.join(&cfg.src))
        .tests(root.join(&cfg.tests))
        .scripts(root.join(&cfg.scripts))
        .libs(cfg.libs.iter().map(|l| root.join(l)))
        .remappings(resolve_remappings(root, cfg))
        .build()
        .map_err(|e| e.to_string())
}

fn build_settings(cfg: &Config) -> Settings {
    let mut settings = Settings {
        optimizer: Optimizer {
            enabled: cfg.optimizer,
            runs: cfg.optimizer_runs,
            details: None,
        },
        via_ir: cfg.via_ir,
        ..Default::default()
    };
    // Only override the version-appropriate default when explicitly configured.
    if let Some(evm) = cfg.evm_version {
        settings.evm_version = Some(evm);
    }
    settings
}

fn build_compiler(cfg: &Config) -> Result<SolcCompiler, String> {
    Ok(match &cfg.solc {
        Some(v) => SolcCompiler::Specific(Solc::find_or_install(v).map_err(|e| e.to_string())?),
        None => SolcCompiler::AutoDetect,
    })
}

/// Apply forge's display filtering: suppress warnings with an ignored code or
/// from an ignored path. Errors always surface.
fn filter_errors(mut errors: Vec<SolcError>, root: &Path, cfg: &Config) -> Vec<SolcError> {
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
    errors
}

/// Compile the project rooted at `root`, returning diagnostics (with the same
/// warning suppression `forge build` applies) and the per-source ASTs.
///
/// `full` bypasses the incremental cache so solc compiles every file in one
/// invocation. That is required for the navigation index, where node ids and
/// source indices must be consistent across all files (they are assigned per
/// compilation). Diagnostics use the fast cached path (`full = false`).
pub fn compile(root: &Path, full: bool) -> Result<CompileOutput, String> {
    let cfg = parse_config(root);
    let mut paths = build_paths(root, &cfg)?;

    // Isolate our build cache/artifacts from the user's `out/` so we never race
    // `forge build`; `cache/` is gitignored by every Foundry project. The index
    // uses a separate, wiped cache so it always gets a cold, full compile (all
    // files in one solc run -> consistent node ids); diagnostics reuse a warm
    // cache for fast incremental rebuilds.
    let work = if full {
        let dir = root.join("cache").join("solidity-lsp-index");
        let _ = std::fs::remove_dir_all(&dir);
        dir
    } else {
        root.join("cache").join("solidity-lsp")
    };
    paths.cache = work.join("solidity-files-cache.json");
    paths.artifacts = work.join("out");
    paths.build_infos = work.join("out").join("build-info");

    let mut settings = build_settings(&cfg);
    if full {
        // The navigation index only consumes the typed AST, so request just that
        // and let solc skip code generation, the optimizer and via-ir — the bulk
        // of the compile cost on real projects (the index build that drives the
        // editor's "Indexing…" spinner). Diagnostics keep their own full compile.
        settings.output_selection = OutputSelection::ast_output_selection();
    }

    let project = ProjectBuilder::<SolcCompiler>::default()
        .paths(paths)
        .settings(SolcSettings { settings, cli_settings: Default::default() })
        .build(build_compiler(&cfg)?)
        .map_err(|e| e.to_string())?;

    let output = project.compile().map_err(|e| e.to_string())?.into_output();

    let mut sources = Vec::new();
    for (path, sf) in output.sources.sources() {
        if let Some(ast) = &sf.ast {
            if let Ok(value) = serde_json::to_value(ast) {
                let abs = if path.is_absolute() { path.clone() } else { root.join(path) };
                sources.push(SourceAst { index: sf.id as usize, path: abs, ast: value });
            }
        }
    }
    let errors = filter_errors(output.errors, root, &cfg);
    Ok(CompileOutput { errors, sources })
}

/// Type-check the unsaved `buffer` for `target` against the project's imports,
/// for live (as-you-type) diagnostics. Builds solc's standard-json input from
/// the on-disk import graph, swaps in the buffer, and type-checks with no
/// codegen (so via-ir projects stay fast). Requires a pinned solc version.
pub fn check_buffer(root: &Path, target: &Path, buffer: &str) -> Result<Vec<SolcError>, String> {
    let cfg = parse_config(root);
    let version = cfg.solc.clone().ok_or("project does not pin a solc version")?;
    let solc = Solc::find_or_install(&version).map_err(|e| e.to_string())?;

    let project = ProjectBuilder::<SolcCompiler>::default()
        .paths(build_paths(root, &cfg)?)
        .settings(SolcSettings { settings: build_settings(&cfg), cli_settings: Default::default() })
        .build(SolcCompiler::Specific(solc.clone()))
        .map_err(|e| e.to_string())?;

    let mut input = project.standard_json_input(target).map_err(|e| e.to_string())?;
    // Type-check only: an empty output selection skips codegen.
    input.settings.output_selection = Default::default();

    // Swap the edited file's on-disk content for the unsaved buffer.
    let rel = target.strip_prefix(root).unwrap_or(target);
    let mut found = false;
    for (p, source) in input.sources.iter_mut() {
        if p == rel || p == target {
            *source = Source::new(buffer);
            found = true;
        }
    }
    if !found {
        return Err("target is not in the project source graph".to_string());
    }

    let input = input.normalize_evm_version(&version);
    let output = solc.compile(&input).map_err(|e| e.to_string())?;
    Ok(filter_errors(output.errors, root, &cfg))
}

/// The remapping prefixes configured for a project (e.g. `@openzeppelin/`),
/// sorted and deduped, for import-path completion.
pub fn remapping_prefixes(root: &Path) -> Vec<String> {
    let cfg = parse_config(root);
    let mut names: Vec<String> =
        resolve_remappings(root, &cfg).into_iter().map(|r| r.name).collect();
    names.sort();
    names.dedup();
    names
}

/// Detect a concrete solc version from the file's `pragma solidity` line, for
/// config-less single-file checking. Returns the first `x.y.z` the line names
/// (e.g. the lower bound of `>=0.8.0 <0.9.0`, or the base of `^0.8.20`).
fn detect_solc(text: &str) -> Option<Version> {
    let line = text.lines().find(|l| l.trim_start().starts_with("pragma solidity"))?;
    line.split(|c: char| !c.is_ascii_digit() && c != '.')
        .find_map(|tok| Version::parse(tok).ok())
}

/// Type-check a single self-contained buffer that has no Foundry project, for
/// config-less live diagnostics. Files with imports are skipped: without a
/// project their imports can't be resolved, and surfacing false "file not found"
/// errors is precisely the failure mode this server exists to avoid. solc is
/// taken from the buffer's pragma and auto-installed via svm.
pub fn check_standalone(target: &Path, buffer: &str) -> Result<Vec<SolcError>, String> {
    if buffer.lines().any(|l| l.trim_start().starts_with("import ")) {
        return Ok(Vec::new());
    }
    let version = detect_solc(buffer).ok_or("no concrete solc version in pragma")?;
    let solc = Solc::find_or_install(&version).map_err(|e| e.to_string())?;

    let mut input = SolcInput::default();
    input.sources.insert(target.to_path_buf(), Source::new(buffer));
    input.settings.output_selection = Default::default(); // type-check only
    let input = input.sanitized(&version);

    let output = solc.compile_exact(&input).map_err(|e| e.to_string())?;
    // Apply forge's default warning suppression (license, code-size, …).
    let root = target.parent().unwrap_or(target);
    Ok(filter_errors(output.errors, root, &Config::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("solidity-lsp-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn locates_nearest_foundry_root() {
        // A monorepo: an outer root and a nested package, each with foundry.toml.
        let root = temp_dir();
        let pkg = root.join("pkg");
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::write(root.join("foundry.toml"), "[profile.default]\n").unwrap();
        std::fs::write(pkg.join("foundry.toml"), "[profile.default]\n").unwrap();

        let nested = pkg.join("src").join("A.sol");
        std::fs::write(&nested, "contract A {}").unwrap();
        assert_eq!(locate_root(&nested).as_deref(), Some(pkg.as_path()));

        let outer = root.join("X.sol");
        std::fs::write(&outer, "contract X {}").unwrap();
        assert_eq!(locate_root(&outer).as_deref(), Some(root.as_path()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parses_pinned_solc_version() {
        let root = temp_dir();
        std::fs::write(root.join("foundry.toml"), "[profile.default]\nsolc = \"0.8.19\"\n").unwrap();
        assert_eq!(parse_config(&root).solc, Some(Version::new(0, 8, 19)));

        // The `solc_version` alias and a leading caret both pin the same version.
        std::fs::write(root.join("foundry.toml"), "[profile.default]\nsolc_version = \"^0.8.19\"\n")
            .unwrap();
        assert_eq!(parse_config(&root).solc, Some(Version::new(0, 8, 19)));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn detects_pragma_solc_version() {
        assert_eq!(detect_solc("pragma solidity 0.8.20;"), Some(Version::new(0, 8, 20)));
        assert_eq!(detect_solc("pragma solidity ^0.8.19;"), Some(Version::new(0, 8, 19)));
        // A range takes the first (lower-bound) concrete version.
        assert_eq!(
            detect_solc("pragma solidity >=0.8.0 <0.9.0;"),
            Some(Version::new(0, 8, 0))
        );
        // No pragma, or no concrete x.y.z, yields nothing.
        assert_eq!(detect_solc("contract C {}"), None);
        assert_eq!(detect_solc("pragma solidity ^0.8;"), None);
    }

    #[test]
    fn resolves_unusual_remapping_prefixes() {
        let root = temp_dir();
        std::fs::write(root.join("foundry.toml"), "[profile.default]\n").unwrap();
        std::fs::write(root.join("remappings.txt"), "@odd-prefix.v2/=node_modules/@odd/v2/\n#c\n\n")
            .unwrap();
        let cfg = parse_config(&root);
        let remappings = resolve_remappings(&root, &cfg);
        assert!(
            remappings.iter().any(|r| r.name == "@odd-prefix.v2/"),
            "unusual prefix not resolved: {:?}",
            remappings.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
