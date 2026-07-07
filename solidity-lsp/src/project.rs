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
/// the source-unit index solc uses in `src` locations. `text` is read here, next
/// to the AST, so the index's byte offsets and its staleness check use the same
/// snapshot — reading it later (in `Index::build`) could pick up a save that
/// landed after the compile, pairing new text with old offsets.
pub struct SourceAst {
    pub index: usize,
    pub path: PathBuf,
    pub ast: serde_json::Value,
    pub text: String,
}

/// Result of compiling a project: diagnostics plus the typed ASTs to index.
pub struct CompileOutput {
    pub errors: Vec<SolcError>,
    pub sources: Vec<SourceAst>,
    /// Absolute paths solc actually ran this compile. A warm incremental
    /// compile omits unchanged files (their diagnostics aren't re-emitted, and
    /// foundry's cache doesn't persist them), so diagnostics use this to clear
    /// and replace only what was really re-checked, leaving cache-hit files'
    /// still-valid warnings in place.
    pub compiled: HashSet<PathBuf>,
}

/// solc warning codes `forge build` suppresses by default: license (1878),
/// code-size (5574), init-code-size (3860), transient-storage (2394).
const DEFAULT_IGNORED_CODES: [u64; 4] = [1878, 5574, 3860, 2394];

/// foundry.toml `ignored_error_codes` accepts either a numeric code or a named
/// alias; map every alias forge's `SolidityErrorCode` defines, fall back to
/// parsing an integer. A missing alias would silently keep a warning forge
/// suppresses, so this mirrors forge's set exactly.
fn error_code(s: &str) -> Option<u64> {
    match s {
        "license" => Some(1878),
        "constructor-visibility" => Some(2462),
        "code-size" => Some(5574),
        "init-code-size" => Some(3860),
        "func-mutability" => Some(2018),
        "unused-var" => Some(2072),
        "unused-param" => Some(5667),
        "unused-return" => Some(9302),
        "virtual-interfaces" => Some(5815),
        "missing-receive-ether" => Some(3628),
        "shadowing" => Some(2519),
        "same-varname" => Some(8760),
        "unnamed-return" => Some(6321),
        "unreachable" => Some(5740),
        "pragma-solidity" => Some(3420),
        "transient-storage" => Some(2394),
        "too-many-warnings" => Some(4591),
        "transfer-deprecated" => Some(9207),
        "natspec-memory-safe-assembly-deprecated" => Some(2424),
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
#[derive(Clone)]
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
    /// `skip` globs: files forge excludes from the build entirely (typically
    /// ones that don't compile), so none of their diagnostics surface.
    skip: Vec<String>,
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
            skip: Vec::new(),
        }
    }
}

/// Match a root-relative `path` against a foundry `skip` glob. `*`/`**` match any
/// run of characters (crossing `/`, as forge's default globset with
/// `literal_separator = false`) and `?` matches one; a `**/` at a component
/// boundary additionally matches zero or more whole path components, so
/// `**/foo.sol` also skips a top-level `foo.sol`. Other glob syntax (`[...]`,
/// `{a,b}`) is treated literally — the `skip` patterns projects use to exclude
/// non-compiling files are simple.
fn glob_match(pat: &str, path: &str) -> bool {
    glob_match_bytes(pat.as_bytes(), path.as_bytes())
}

// ponytail: naive backtracking matcher, O(n^stars) worst case — skip globs are
// short and few, so it is never hot; reach for the `globset` crate if that ever
// changes.
fn glob_match_bytes(p: &[u8], t: &[u8]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    // `**/` matches zero or more whole path components: match the rest at this
    // component boundary, or peel off one leading `/`-terminated component and
    // retry. Anchoring to a `/` keeps `test` a whole component, not a suffix.
    if let Some(rest) = p.strip_prefix(b"**/") {
        return glob_match_bytes(rest, t)
            || (0..t.len()).any(|i| t[i] == b'/' && glob_match_bytes(rest, &t[i + 1..]));
    }
    match p[0] {
        b'*' => {
            let stars = p.iter().take_while(|&&c| c == b'*').count();
            let rest = &p[stars..];
            // `*`/`**` (not at a `**/` boundary) match any run of characters,
            // separators included.
            (0..=t.len()).any(|i| glob_match_bytes(rest, &t[i..]))
        }
        b'?' => !t.is_empty() && glob_match_bytes(&p[1..], &t[1..]),
        c => !t.is_empty() && t[0] == c && glob_match_bytes(&p[1..], &t[1..]),
    }
}

/// If `root/foundry.toml` exists but isn't valid TOML, the parse error.
/// `parse_config` silently falls back to defaults on a broken file (losing the
/// pinned solc, inline remappings, and layout), so a caller can surface this.
pub fn config_parse_error(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("foundry.toml")).ok()?;
    text.parse::<toml::Table>().err().map(|e| e.to_string())
}

fn parse_config(root: &Path) -> Config {
    // forge selects the active profile from `FOUNDRY_PROFILE` (default
    // "default"), so an editor launched from a shell with that set must compile
    // with the same settings forge would.
    let profile = std::env::var("FOUNDRY_PROFILE").unwrap_or_default();
    config_for(root, &profile)
}

/// Parse `foundry.toml` for the named profile. The selected profile inherits
/// from `profile.default` and overrides it key by key (figment-style), matching
/// forge's profile resolution.
fn config_for(root: &Path, profile: &str) -> Config {
    let mut cfg = Config::default();
    let Ok(text) = std::fs::read_to_string(root.join("foundry.toml")) else {
        return cfg;
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return cfg;
    };
    let profiles = table.get("profile").and_then(|p| p.as_table());
    let get = |name: &str| profiles.and_then(|t| t.get(name)).and_then(|d| d.as_table());
    let default = get("default");
    // The active profile's keys overlay `default`. An unset/"default" profile is
    // just `default`; a named profile with no `default` table starts from the
    // built-in Config::default() (forge's defaults), same as forge.
    let selected = (!profile.is_empty() && profile != "default")
        .then(|| get(profile))
        .flatten();
    let p = match (default, selected) {
        (None, None) => return cfg,
        (Some(d), None) => d.clone(),
        (None, Some(s)) => s.clone(),
        (Some(d), Some(s)) => {
            let mut merged = d.clone();
            for (k, v) in s {
                merged.insert(k.clone(), v.clone());
            }
            merged
        }
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
    if let Some(arr) = p.get("skip").and_then(|v| v.as_array()) {
        cfg.skip = arr.iter().filter_map(|v| v.as_str()).map(String::from).collect();
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

/// The parsed config and resolved remappings for `root`, memoized so the hot
/// live-check and import-completion paths don't repeat the work on every
/// 300ms-debounced keystroke: `resolve_remappings` walks every lib/ dir
/// recursively (`Remapping::find_many`) and re-reads foundry.toml. The cache is
/// keyed on the bytes of foundry.toml + remappings.txt (plus `FOUNDRY_PROFILE`),
/// so an in-editor config edit is picked up immediately; `invalidate_root` drops
/// the entry for external changes (forge install, branch switch) the file
/// watcher reports, which can add libs without touching those two files.
struct CachedConfig {
    key: u64,
    config: Config,
    remappings: Vec<Remapping>,
}

fn config_cache() -> &'static std::sync::Mutex<HashMap<PathBuf, CachedConfig>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, CachedConfig>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn config_and_remappings(root: &Path) -> (Config, Vec<Remapping>) {
    let profile = std::env::var("FOUNDRY_PROFILE").unwrap_or_default();
    let key = config_key(root, &profile);
    if let Some(c) = config_cache().lock().unwrap_or_else(|e| e.into_inner()).get(root) {
        if c.key == key {
            return (c.config.clone(), c.remappings.clone());
        }
    }
    let config = config_for(root, &profile);
    let remappings = resolve_remappings(root, &config);
    config_cache().lock().unwrap_or_else(|e| e.into_inner()).insert(
        root.to_path_buf(),
        CachedConfig { key, config: config.clone(), remappings: remappings.clone() },
    );
    (config, remappings)
}

/// Drop the memoized config for `root` so the next `config_and_remappings`
/// recomputes it. Called when the file watcher reports an external change (a
/// `forge install` or branch switch) that can alter remappings by adding or
/// removing lib/ sources without editing foundry.toml or remappings.txt.
pub fn invalidate_root(root: &Path) {
    config_cache().lock().unwrap_or_else(|e| e.into_inner()).remove(root);
}

/// A content signature for the config files a memoized `config_and_remappings`
/// depends on. Hashing the bytes (rather than mtimes) sidesteps coarse mtime
/// resolution, so two edits within the same tick still invalidate.
fn config_key(root: &Path, profile: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    profile.hash(&mut h);
    std::fs::read(root.join("foundry.toml")).unwrap_or_default().hash(&mut h);
    std::fs::read(root.join("remappings.txt")).unwrap_or_default().hash(&mut h);
    h.finish()
}

fn build_paths(
    root: &Path,
    cfg: &Config,
    remappings: Vec<Remapping>,
) -> Result<ProjectPathsConfig<SolcLanguage>, String> {
    ProjectPathsConfig::builder()
        .root(root)
        .sources(root.join(&cfg.src))
        .tests(root.join(&cfg.tests))
        .scripts(root.join(&cfg.scripts))
        .libs(cfg.libs.iter().map(|l| root.join(l)))
        .remappings(remappings)
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
        // `skip` drops a file from the build entirely, so forge reports none of
        // its diagnostics — errors too. Match before the error check below.
        if !cfg.skip.is_empty() {
            if let Some(loc) = &e.source_location {
                let rel = loc.file.replace('\\', "/");
                if cfg.skip.iter().any(|g| glob_match(g, &rel)) {
                    return false;
                }
            }
        }
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
    let (cfg, remappings) = config_and_remappings(root);
    let mut paths = build_paths(root, &cfg, remappings)?;

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
    let mut compiled = HashSet::new();
    for (path, sf) in output.sources.sources() {
        let abs = if path.is_absolute() { path.clone() } else { root.join(path) };
        compiled.insert(abs.clone());
        if let Some(ast) = &sf.ast {
            if let Ok(value) = serde_json::to_value(ast) {
                let Ok(text) = std::fs::read_to_string(&abs) else {
                    continue;
                };
                sources.push(SourceAst { index: sf.id as usize, path: abs, ast: value, text });
            }
        }
    }
    let errors = filter_errors(output.errors, root, &cfg);
    Ok(CompileOutput { errors, sources, compiled })
}

/// Type-check the unsaved `buffer` for `target` against the project's imports,
/// for live (as-you-type) diagnostics. Builds solc's standard-json input from
/// the on-disk import graph, swaps in the buffer, and type-checks with no
/// codegen (so via-ir projects stay fast). Requires a pinned solc version.
pub fn check_buffer(
    root: &Path,
    target: &Path,
    buffer: &str,
    open: &HashMap<PathBuf, String>,
) -> Result<Vec<SolcError>, String> {
    let (cfg, remappings) = config_and_remappings(root);
    // A project that pins no solc (e.g. version comes from each file's pragma)
    // otherwise got no live diagnostics at all; fall back to the buffer's pragma.
    let version = cfg
        .solc
        .clone()
        .or_else(|| detect_solc(buffer))
        .ok_or("no solc version pinned or in the buffer's pragma")?;
    let mut solc = Solc::find_or_install(&version).map_err(|e| e.to_string())?;
    // Resolve imports against the project root explicitly, so a just-typed
    // `import "./New.sol"` whose file exists on disk still resolves even when the
    // server's working directory isn't the root.
    solc.base_path = Some(root.to_path_buf());

    let project = ProjectBuilder::<SolcCompiler>::default()
        .paths(build_paths(root, &cfg, remappings)?)
        .settings(SolcSettings { settings: build_settings(&cfg), cli_settings: Default::default() })
        .build(SolcCompiler::Specific(solc.clone()))
        .map_err(|e| e.to_string())?;

    let mut input = project.standard_json_input(target).map_err(|e| e.to_string())?;
    // Type-check only: an empty output selection skips codegen.
    input.settings.output_selection = Default::default();

    // Swap every open buffer's on-disk content for its unsaved text — the target
    // from `buffer`, any other open file from `open`. Checking against stale disk
    // for an imported-but-unsaved file produced phantom errors ("member not
    // found", …) until that file was saved.
    let target_id = path_identity(target);
    let open_ids: HashMap<PathBuf, &str> =
        open.iter().map(|(p, t)| (path_identity(p), t.as_str())).collect();
    let mut found = false;
    for (p, source) in input.sources.iter_mut() {
        let abs = path_identity(&if p.is_absolute() { p.clone() } else { root.join(p) });
        if abs == target_id {
            *source = Source::new(buffer);
            found = true;
        } else if let Some(text) = open_ids.get(&abs) {
            *source = Source::new(*text);
        }
    }
    if !found {
        // The target isn't in the project's source graph (outside src/test/
        // script/libs). Nothing to live-check; the on-disk compile owns it.
        return Ok(Vec::new());
    }

    let input = input.normalize_evm_version(&version);
    let output = solc.compile(&input).map_err(|e| e.to_string())?;
    Ok(filter_errors(output.errors, root, &cfg))
}

/// The project's configured library directories (absolute), where vendored
/// dependency sources live. Rename refuses to edit declarations under these.
pub fn lib_dirs(root: &Path) -> Vec<PathBuf> {
    parse_config(root).libs.iter().map(|l| root.join(l)).collect()
}

/// The remappings configured for a project as `(name, target dir)` pairs — e.g.
/// `("@openzeppelin/", "/abs/lib/openzeppelin-contracts/")` — for import-path
/// completion. The name is what the user types; the target is the directory to
/// list once they've entered it (`Remapping::path` is already absolute). Sorted
/// by name and deduped. Uses the memoized config so keystroke-time completion
/// doesn't re-walk lib/.
pub fn remapping_targets(root: &Path) -> Vec<(String, PathBuf)> {
    let (_, remappings) = config_and_remappings(root);
    let mut out: Vec<(String, PathBuf)> =
        remappings.into_iter().map(|r| (r.name, PathBuf::from(r.path))).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// A cheap change signature for a project's source tree: every `.sol` file under
/// the configured source/test/script/lib dirs (plus foundry.toml and
/// remappings.txt), hashed by path, mtime and length. Two calls return the same
/// value only when nothing was added, removed or modified, so the navigation
/// index can skip a full cold recompile that would just reproduce the same AST.
/// A file whose metadata can't be read still contributes its path, so a new file
/// always changes the signature even if its mtime is momentarily unreadable.
pub fn source_fingerprint(root: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let (cfg, remappings) = config_and_remappings(root);
    let mut files: Vec<(PathBuf, u64, u64)> = Vec::new();
    let mut dirs: Vec<PathBuf> = [cfg.src, cfg.tests, cfg.scripts]
        .into_iter()
        .chain(cfg.libs)
        .map(|d| root.join(d))
        .collect();
    // A remapping can point outside src/test/script/lib (e.g.
    // `@oz/=node_modules/@openzeppelin/`); the full compile indexes those
    // sources, so an on-disk change there must move the fingerprint too. Skip
    // targets already under a walked dir so lib/ isn't re-walked.
    for r in &remappings {
        let target = PathBuf::from(&r.path);
        if !dirs.iter().any(|d| target.starts_with(d)) {
            dirs.push(target);
        }
    }
    for d in &dirs {
        collect_sol(d, &mut files);
    }
    for name in ["foundry.toml", "remappings.txt"] {
        let p = root.join(name);
        if let Ok(m) = std::fs::metadata(&p) {
            files.push((p, mtime_nanos(&m), m.len()));
        }
    }
    files.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    files.hash(&mut h);
    h.finish()
}

/// Every `.sol` file under the project's configured source, test and script
/// directories, for the whole-project background parse that keeps navigation
/// working before the first successful compile and through broken builds. `lib/`
/// is intentionally excluded — vendored dependencies aren't renamed or listed as
/// project symbols, and parsing an entire dependency tree on every root is
/// costly; the solc index already covers them once a compile succeeds.
pub fn source_files(root: &Path) -> Vec<PathBuf> {
    let cfg = parse_config(root);
    let mut files = Vec::new();
    for d in [cfg.src, cfg.tests, cfg.scripts] {
        collect_sol(&root.join(d), &mut files);
    }
    files.into_iter().map(|(p, _, _)| p).collect()
}

/// Recursively collect every `.sol` file under `dir` with its mtime and length.
/// `read_dir`'s file type doesn't follow symlinks, so a symlinked directory is
/// simply skipped — no risk of a cyclic walk.
fn collect_sol(dir: &Path, out: &mut Vec<(PathBuf, u64, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ft.is_dir() {
            collect_sol(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("sol") {
            let (mtime, len) =
                entry.metadata().map(|m| (mtime_nanos(&m), m.len())).unwrap_or((0, 0));
            out.push((path, mtime, len));
        }
    }
}

fn mtime_nanos(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Detect a concrete solc version from the file's `pragma solidity` line, for
/// config-less single-file checking. Returns the first `x.y.z` the line names
/// (e.g. the lower bound of `>=0.8.0 <0.9.0`, or the base of `^0.8.20`).
fn detect_solc(text: &str) -> Option<Version> {
    let line = text.lines().find(|l| l.trim_start().starts_with("pragma solidity"))?;
    line.split(|c: char| !c.is_ascii_digit() && c != '.')
        .find_map(|tok| Version::parse(tok).ok())
}

/// Type-check a single buffer that has no Foundry project, for config-less live
/// diagnostics. Relative (`./`, `../`) imports are resolved transitively from
/// disk and fed to solc alongside the buffer. Any import that can't be resolved
/// this way — a remapped or library import, or a file missing on disk — means we
/// can't reproduce the real graph, so the whole check is skipped rather than
/// surface false "file not found" errors (precisely the failure mode this server
/// exists to avoid). solc is taken from the buffer's pragma and installed via svm.
pub fn check_standalone(target: &Path, buffer: &str) -> Result<Vec<SolcError>, String> {
    let version = detect_solc(buffer).ok_or("no concrete solc version in pragma")?;

    // Collect the buffer plus every relative import reachable from it. Bail to an
    // empty result the moment an import isn't a resolvable relative file.
    let mut sources: HashMap<PathBuf, String> = HashMap::new();
    let target = normalize_path(target);
    let mut queue = vec![(target.clone(), buffer.to_string())];
    sources.insert(target.clone(), buffer.to_string());
    while let Some((path, text)) = queue.pop() {
        let dir = path.parent().unwrap_or(&path);
        for imp in relative_imports(&text) {
            let Some(imp) = imp else {
                return Ok(Vec::new()); // an import we can't resolve standalone
            };
            let resolved = normalize_path(&dir.join(imp));
            if sources.contains_key(&resolved) {
                continue;
            }
            let Ok(dep) = std::fs::read_to_string(&resolved) else {
                return Ok(Vec::new()); // a referenced file is missing on disk
            };
            sources.insert(resolved.clone(), dep.clone());
            queue.push((resolved, dep));
        }
    }

    let solc = Solc::find_or_install(&version).map_err(|e| e.to_string())?;
    let mut input = SolcInput::default();
    for (path, text) in sources {
        input.sources.insert(path, Source::new(text));
    }
    input.settings.output_selection = Default::default(); // type-check only
    let input = input.sanitized(&version);

    let output = solc.compile_exact(&input).map_err(|e| e.to_string())?;
    // Apply forge's default warning suppression (license, code-size, …).
    let root = target.parent().unwrap_or(&target);
    Ok(filter_errors(output.errors, root, &Config::default()))
}

/// The import paths of `text`, one entry per `import` line: `Some(path)` for a
/// relative `./`/`../` import, `None` for anything else (a bare, remapped or
/// library import, or a line we can't extract a path from) — the caller treats a
/// `None` as unresolvable and skips the standalone check.
fn relative_imports(text: &str) -> Vec<Option<&str>> {
    text.lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("import "))
        .map(|l| {
            quoted(l).filter(|p| p.starts_with("./") || p.starts_with("../"))
        })
        .collect()
}

/// The contents of the first `"..."` or `'...'` in `s`, if any.
fn quoted(s: &str) -> Option<&str> {
    let start = s.find(['"', '\''])?;
    let q = s.as_bytes()[start];
    let rest = &s[start + 1..];
    rest.find(q as char).map(|end| &rest[..end])
}

/// Lexically normalize a path (fold away `.` and `..`), without touching the
/// filesystem, so a key matches how solc names the same source unit.
fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// A stable identity for a filesystem path, so different spellings of the same
/// file compare equal: a percent-encoded vs literal drive colon (once both are
/// decoded through `to_file_path`), `.`/`..` segments, and `c:` vs `C:`
/// drive-letter case on Windows. Canonicalizes when the file exists (resolving
/// symlinks and real case); otherwise a lexical normalize. Used only for
/// comparison — never for I/O or for a URI published back to the client — so the
/// canonical form needn't be a "nice" path.
pub fn path_identity(p: &Path) -> PathBuf {
    let norm = std::fs::canonicalize(p).unwrap_or_else(|_| normalize_path(p));
    identity_key(&norm, cfg!(windows))
}

/// Reduce a normalized path to a comparison key. Windows' filesystem is
/// case-insensitive, so two drive/letter cases are the same file; elsewhere
/// paths are case-sensitive and kept verbatim. Split out so the case-folding
/// rule is unit-testable without a Windows host.
fn identity_key(norm: &Path, windows: bool) -> PathBuf {
    if windows {
        PathBuf::from(norm.as_os_str().to_ascii_lowercase())
    } else {
        norm.to_path_buf()
    }
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
    fn lib_dirs_reflect_config() {
        let root = temp_dir();
        // No config: forge's default single `lib` directory.
        assert_eq!(lib_dirs(&root), vec![root.join("lib")]);
        // An explicit list replaces the default.
        std::fs::write(root.join("foundry.toml"), "[profile.default]\nlibs = [\"lib\", \"deps\"]\n")
            .unwrap();
        assert_eq!(lib_dirs(&root), vec![root.join("lib"), root.join("deps")]);
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
    fn config_parse_error_flags_broken_toml() {
        let root = temp_dir();
        // No file at all: nothing to report.
        assert_eq!(config_parse_error(&root), None);
        // Valid TOML: no error.
        std::fs::write(root.join("foundry.toml"), "[profile.default]\nsolc = \"0.8.19\"\n").unwrap();
        assert_eq!(config_parse_error(&root), None);
        // Broken TOML: an error string is returned.
        std::fs::write(root.join("foundry.toml"), "[profile.default\nsolc = ").unwrap();
        assert!(config_parse_error(&root).is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn standalone_import_resolution() {
        // quoted extracts the first quoted string, either quote style.
        assert_eq!(quoted("import \"./A.sol\";"), Some("./A.sol"));
        assert_eq!(quoted("import {X} from '../lib/B.sol';"), Some("../lib/B.sol"));
        assert_eq!(quoted("no quotes"), None);

        // Relative imports resolve; a remapped/library import is None, which
        // makes the caller skip the standalone check.
        let src = "pragma solidity 0.8.20;\nimport \"./A.sol\";\nimport \"@oz/C.sol\";\nx";
        assert_eq!(relative_imports(src), vec![Some("./A.sol"), None]);

        // Keys are lexically normalized so they match solc's source unit names.
        assert_eq!(normalize_path(Path::new("/a/b/./c")), PathBuf::from("/a/b/c"));
        assert_eq!(normalize_path(Path::new("/a/b/../c/D.sol")), PathBuf::from("/a/c/D.sol"));
    }

    #[test]
    fn skip_globs_parse_and_match() {
        // `*`/`**` cross path separators (forge's default globset behavior).
        assert!(glob_match("test/**", "test/Foo.sol"));
        assert!(glob_match("*.t.sol", "test/Counter.t.sol"));
        assert!(glob_match("src/legacy/*.sol", "src/legacy/Old.sol"));
        assert!(!glob_match("src/legacy/*.sol", "src/current/New.sol"));
        assert!(glob_match("src/?.sol", "src/A.sol"));
        assert!(!glob_match("src/?.sol", "src/AB.sol"));

        // A leading `**/` matches zero or more path components, so a top-level
        // file is skipped just like a nested one (globset semantics).
        assert!(glob_match("**/test/**", "test/Counter.t.sol"));
        assert!(glob_match("**/test/**", "pkg/test/Counter.t.sol"));
        assert!(glob_match("**/*.t.sol", "Counter.t.sol"));
        // Interior `/**/` also collapses to zero components.
        assert!(glob_match("a/**/b", "a/b"));
        // But a `**/` literal segment still matches a whole component, not a
        // suffix — `**/test/**` must not match `xtest/…`.
        assert!(!glob_match("**/test/**", "xtest/Counter.t.sol"));

        let root = temp_dir();
        std::fs::write(
            root.join("foundry.toml"),
            "[profile.default]\nskip = [\"test/**\", \"*.t.sol\"]\n",
        )
        .unwrap();
        assert_eq!(config_for(&root, "default").skip, vec!["test/**", "*.t.sol"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ignored_error_code_aliases_resolve() {
        let root = temp_dir();
        std::fs::write(
            root.join("foundry.toml"),
            "[profile.default]\nignored_error_codes = [\"unused-param\", \"func-mutability\", 1234]\n",
        )
        .unwrap();
        let cfg = config_for(&root, "default");
        // Named aliases map to their solc codes; a bare integer passes through.
        assert_eq!(cfg.ignored_error_codes, vec![5667, 2018, 1234]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn selected_profile_overlays_default() {
        let root = temp_dir();
        std::fs::write(
            root.join("foundry.toml"),
            "[profile.default]\nsolc = \"0.8.19\"\nvia_ir = false\n\
             [profile.coverage]\nvia_ir = true\n",
        )
        .unwrap();
        // The default profile is used as-is.
        assert_eq!(config_for(&root, "default").via_ir, Some(false));
        // The coverage profile overrides via_ir but inherits solc from default.
        let cov = config_for(&root, "coverage");
        assert_eq!(cov.via_ir, Some(true));
        assert_eq!(cov.solc, Some(Version::new(0, 8, 19)));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn source_fingerprint_tracks_tree_changes() {
        let root = temp_dir();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(root.join("foundry.toml"), "[profile.default]\n").unwrap();
        std::fs::write(src.join("A.sol"), "contract A {}").unwrap();

        // Stable across repeated calls when nothing changes.
        let fp1 = source_fingerprint(&root);
        assert_eq!(fp1, source_fingerprint(&root));

        // A new source file changes the signature (so a rebuild is not skipped).
        std::fs::write(src.join("B.sol"), "contract B {}").unwrap();
        let fp2 = source_fingerprint(&root);
        assert_ne!(fp1, fp2);

        // Editing content (longer file) changes it too.
        std::fs::write(src.join("B.sol"), "contract B { uint256 x; }").unwrap();
        assert_ne!(fp2, source_fingerprint(&root));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn source_fingerprint_tracks_remapped_out_of_tree_dirs() {
        let root = temp_dir();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let dep = root.join("vendor/pkg");
        std::fs::create_dir_all(&dep).unwrap();
        // A remapping target outside src/test/script/lib.
        std::fs::write(
            root.join("foundry.toml"),
            "[profile.default]\nremappings = [\"@pkg/=vendor/pkg/\"]\n",
        )
        .unwrap();
        std::fs::write(dep.join("Dep.sol"), "contract Dep {}").unwrap();

        let fp1 = source_fingerprint(&root);
        // Editing the remapped dependency on disk must move the fingerprint, or a
        // needed reindex would be skipped and navigation would stay stale.
        std::fs::write(dep.join("Dep.sol"), "contract Dep { uint256 x; }").unwrap();
        assert_ne!(fp1, source_fingerprint(&root));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn path_identity_folds_case_only_on_windows() {
        // Windows' filesystem is case-insensitive: two drive-letter cases name the
        // same file. Elsewhere paths are case-sensitive and stay distinct.
        let upper = Path::new("C:/Users/Me/Src/A.sol");
        let lower = Path::new("c:/users/me/src/a.sol");
        assert_eq!(identity_key(upper, true), identity_key(lower, true));
        assert_ne!(identity_key(upper, false), identity_key(lower, false));

        // For a path with no file on disk (canonicalize can't resolve it) the
        // lexical normalize still folds `.`/`..`, so two spellings of one file
        // compare equal.
        let a = path_identity(Path::new("/no-such-solidity-lsp/a/../b/C.sol"));
        let b = path_identity(Path::new("/no-such-solidity-lsp/b/C.sol"));
        assert_eq!(a, b);
    }

    #[test]
    fn source_files_span_project_dirs_but_not_libs() {
        let root = temp_dir();
        for d in ["src", "test", "script", "lib/oz"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("foundry.toml"), "[profile.default]\n").unwrap();
        std::fs::write(root.join("src/A.sol"), "contract A {}").unwrap();
        std::fs::write(root.join("test/A.t.sol"), "contract AT {}").unwrap();
        std::fs::write(root.join("script/A.s.sol"), "contract AS {}").unwrap();
        std::fs::write(root.join("src/README.md"), "not solidity").unwrap();
        std::fs::write(root.join("lib/oz/O.sol"), "contract O {}").unwrap();

        let files = source_files(&root);
        let names: Vec<String> =
            files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"A.sol".to_string()), "{names:?}");
        assert!(names.contains(&"A.t.sol".to_string()), "{names:?}");
        assert!(names.contains(&"A.s.sol".to_string()), "{names:?}");
        // Non-Solidity files and vendored lib sources are excluded.
        assert!(!names.contains(&"README.md".to_string()), "{names:?}");
        assert!(!names.contains(&"O.sol".to_string()), "lib excluded: {names:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn config_memo_tracks_content_and_invalidation() {
        let root = temp_dir();
        std::fs::write(root.join("foundry.toml"), "[profile.default]\n").unwrap();
        std::fs::write(root.join("remappings.txt"), "@a/=lib/a/\n").unwrap();

        let names = |root: &Path| {
            let (_, r) = config_and_remappings(root);
            r.into_iter().map(|r| r.name).collect::<Vec<_>>()
        };
        assert!(names(&root).contains(&"@a/".to_string()));

        // Changing remappings.txt content changes the key, so the memo recomputes
        // rather than serving the stale set.
        std::fs::write(root.join("remappings.txt"), "@a/=lib/a/\n@b/=lib/b/\n").unwrap();
        assert!(names(&root).contains(&"@b/".to_string()));

        // invalidate_root drops the entry, forcing a fresh resolve on next call.
        invalidate_root(&root);
        assert!(names(&root).contains(&"@b/".to_string()));

        std::fs::remove_dir_all(&root).ok();
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
