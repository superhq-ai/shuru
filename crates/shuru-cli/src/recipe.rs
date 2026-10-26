//! Recipes (RFC 0005): a directory with recipe.json and one or more step
//! scripts, fetched from GitHub at an exact commit or read from a local
//! path, and built into a checkpoint through the normal checkpoint flow.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

use shuru_vm::default_data_dir;

use crate::assets;
use crate::checkpoint;
use crate::cli::VmArgs;
use crate::config::ShuruConfig;

pub(crate) const OFFICIAL_OWNER: &str = "superhq-ai";
pub(crate) const OFFICIAL_REPO: &str = "recipes";
const INDEX_TTL: Duration = Duration::from_secs(24 * 3600);
const USER_AGENT: &str = concat!("shuru/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RecipeSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub min_shuru_version: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub cpus: Option<usize>,
    #[serde(default)]
    pub memory: Option<u64>,
    #[serde(default)]
    pub disk_size: Option<u64>,
    #[serde(default)]
    pub network: Option<RecipeNetwork>,
    #[serde(default)]
    pub services: BTreeMap<String, serde_json::Value>,
    #[serde(default = "default_steps")]
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RecipeNetwork {
    #[serde(default)]
    pub allow: Vec<String>,
}

fn default_steps() -> Vec<String> {
    vec!["build.sh".to_string()]
}

/// Where a recipe reference points.
#[derive(Debug, Clone)]
enum Source {
    Local(PathBuf),
    GitHub {
        owner: String,
        repo: String,
        path: String,
        reference: String,
    },
}

/// A recipe with its step scripts loaded and its revision fixed.
pub(crate) struct Recipe {
    pub spec: RecipeSpec,
    /// (file name, contents) in run order.
    pub steps: Vec<(String, String)>,
    /// Human readable origin, e.g. `superhq-ai/recipes/docker` or a path.
    pub source: String,
    /// Commit SHA for GitHub, content hash for local directories.
    pub revision: String,
    /// Official catalog or local path: no confirmation needed.
    pub trusted: bool,
}

impl Recipe {
    pub fn short_revision(&self) -> &str {
        &self.revision[..self.revision.len().min(7)]
    }
}

/// Sidecar written next to a checkpoint built from a recipe.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CheckpointMeta {
    pub recipe: RecipeMeta,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RecipeMeta {
    pub name: String,
    pub source: String,
    pub revision: String,
}

#[derive(Debug, Deserialize)]
struct IndexEntry {
    name: String,
    #[serde(default)]
    description: String,
}

// --- Reference parsing ---

/// Only an explicit path form is local. A bare name is never looked up in
/// the working directory: a cloned repository must not be able to shadow
/// an official recipe and inherit its trust.
fn parse_reference(reference: &str) -> Result<Source> {
    let looks_local = reference.starts_with("./")
        || reference.starts_with("../")
        || reference.starts_with('/')
        || reference.starts_with("~/")
        || reference == "."
        || reference == "..";
    if looks_local {
        let path = if let Some(rest) = reference.strip_prefix("~/") {
            let home = std::env::var("HOME").context("HOME is not set")?;
            PathBuf::from(home).join(rest)
        } else {
            PathBuf::from(reference)
        };
        return Ok(Source::Local(path));
    }

    let (locator, git_ref) = match reference.rsplit_once('@') {
        Some((l, r)) if !r.is_empty() && !r.contains('/') => (l, r.to_string()),
        _ => (reference, "HEAD".to_string()),
    };
    let parts: Vec<&str> = locator.split('/').collect();
    if parts.iter().any(|p| p.is_empty() || *p == "..") {
        bail!("invalid recipe reference: {}", reference);
    }
    let (owner, repo, path) = match parts.len() {
        1 => {
            if Path::new(parts[0]).join("recipe.json").is_file() {
                eprintln!(
                    "shuru: note: ./{} has a recipe.json, but a bare name means the official catalog; pass ./{} to build the local one",
                    parts[0], parts[0]
                );
            }
            (
                OFFICIAL_OWNER.to_string(),
                OFFICIAL_REPO.to_string(),
                parts[0].to_string(),
            )
        }
        2 => (parts[0].to_string(), parts[1].to_string(), String::new()),
        _ => (
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2..].join("/"),
        ),
    };
    Ok(Source::GitHub {
        owner,
        repo,
        path,
        reference: git_ref,
    })
}

// --- Fetching ---

fn http_get(url: &str, accept: &str) -> Result<String> {
    let mut req = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", accept);
    if url.starts_with("https://api.github.com/") {
        if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
            if !token.is_empty() {
                req = req.header("Authorization", &format!("Bearer {}", token));
            }
        }
    }
    let mut resp = req
        .call()
        .with_context(|| format!("GET {} failed", url))?;
    resp.body_mut()
        .read_to_string()
        .with_context(|| format!("reading {}", url))
}

fn resolve_sha(owner: &str, repo: &str, reference: &str) -> Result<String> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/commits/{}",
        owner, repo, reference
    );
    let sha = http_get(&url, "application/vnd.github.sha")
        .with_context(|| format!("resolving {}/{}@{}", owner, repo, reference))?;
    let sha = sha.trim().to_string();
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("unexpected commit id from GitHub for {}/{}@{}: {}", owner, repo, reference, sha);
    }
    Ok(sha)
}

fn raw_url(owner: &str, repo: &str, sha: &str, path: &str, file: &str) -> String {
    if path.is_empty() {
        format!("https://raw.githubusercontent.com/{}/{}/{}/{}", owner, repo, sha, file)
    } else {
        format!(
            "https://raw.githubusercontent.com/{}/{}/{}/{}/{}",
            owner, repo, sha, path, file
        )
    }
}

fn parse_spec(text: &str, origin: &str) -> Result<RecipeSpec> {
    let spec: RecipeSpec = serde_json::from_str(text)
        .with_context(|| format!("invalid recipe.json in {}", origin))?;
    validate_spec(&spec, origin)?;
    Ok(spec)
}

fn valid_name(name: &str, allow_dots: bool) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || (allow_dots && c == '.')
    })
}

fn validate_spec(spec: &RecipeSpec, origin: &str) -> Result<()> {
    if !valid_name(&spec.name, true) {
        bail!(
            "{}: recipe name {:?} must match [a-z0-9][a-z0-9._-]*",
            origin,
            spec.name
        );
    }
    if spec.steps.is_empty() {
        bail!("{}: steps must list at least one script", origin);
    }
    for step in &spec.steps {
        if step.contains('/') || step.contains("..") || step.is_empty() {
            bail!("{}: step {:?} must be a file name in the recipe directory", origin, step);
        }
    }
    for (name, value) in &spec.services {
        if !valid_name(name, false) {
            bail!("{}: service name {:?} must match [a-z0-9][a-z0-9-]*", origin, name);
        }
        if !value.is_object() {
            bail!("{}: service {} must be a JSON object", origin, name);
        }
    }
    if let Some(ref from) = spec.from {
        shuru_vm::validate_checkpoint_name(from).map_err(|e| anyhow::anyhow!("{}: {}", origin, e))?;
    }
    if let Some(ref min) = spec.min_shuru_version {
        let need = parse_version(min)
            .with_context(|| format!("{}: invalid min_shuru_version {:?}", origin, min))?;
        let have = parse_version(assets::CURRENT_VERSION)?;
        if have < need {
            bail!(
                "{}: recipe needs shuru {} or newer, this is {}. Run `shuru upgrade`.",
                origin,
                min,
                assets::CURRENT_VERSION
            );
        }
    }
    Ok(())
}

fn parse_version(v: &str) -> Result<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.').map(|p| p.parse::<u64>());
    let major = parts.next().transpose()?.context("missing major version")?;
    let minor = parts.next().transpose()?.unwrap_or(0);
    let patch = parts.next().transpose()?.unwrap_or(0);
    Ok((major, minor, patch))
}

fn load_local(dir: &Path) -> Result<Recipe> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("recipe directory {} not found", dir.display()))?;
    let origin = dir.display().to_string();
    let spec_text = std::fs::read_to_string(dir.join("recipe.json"))
        .with_context(|| format!("reading {}/recipe.json", origin))?;
    let spec = parse_spec(&spec_text, &origin)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(spec_text.as_bytes());
    let mut steps = Vec::new();
    for step in &spec.steps {
        let text = std::fs::read_to_string(dir.join(step))
            .with_context(|| format!("reading step {} in {}", step, origin))?;
        hasher.update(step.as_bytes());
        hasher.update(text.as_bytes());
        steps.push((step.clone(), text));
    }
    Ok(Recipe {
        spec,
        steps,
        source: origin,
        revision: hasher.finalize().to_hex().to_string(),
        trusted: true,
    })
}

fn load_github(owner: &str, repo: &str, path: &str, reference: &str) -> Result<Recipe> {
    let sha = resolve_sha(owner, repo, reference)?;
    let origin = if path.is_empty() {
        format!("{}/{}", owner, repo)
    } else {
        format!("{}/{}/{}", owner, repo, path)
    };
    let spec_text = http_get(&raw_url(owner, repo, &sha, path, "recipe.json"), "text/plain")
        .with_context(|| format!("fetching recipe.json for {}@{}", origin, &sha[..7]))?;
    let spec = parse_spec(&spec_text, &origin)?;
    let mut steps = Vec::new();
    for step in &spec.steps {
        let text = http_get(&raw_url(owner, repo, &sha, path, step), "text/plain")
            .with_context(|| format!("fetching step {} for {}@{}", step, origin, &sha[..7]))?;
        steps.push((step.clone(), text));
    }
    Ok(Recipe {
        spec,
        steps,
        source: origin,
        revision: sha,
        trusted: owner == OFFICIAL_OWNER && repo == OFFICIAL_REPO,
    })
}

/// Resolve a reference to a recipe with its scripts and exact revision.
pub(crate) fn resolve(reference: &str) -> Result<Recipe> {
    match parse_reference(reference)? {
        Source::Local(dir) => load_local(&dir),
        Source::GitHub {
            owner,
            repo,
            path,
            reference,
        } => load_github(&owner, &repo, &path, &reference),
    }
}

// --- Display and trust ---

fn print_summary(recipe: &Recipe, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "name:        {}", recipe.spec.name)?;
    if !recipe.spec.description.is_empty() {
        writeln!(out, "description: {}", recipe.spec.description)?;
    }
    writeln!(out, "source:      {}@{}", recipe.source, recipe.short_revision())?;
    if let Some(ref from) = recipe.spec.from {
        writeln!(out, "from:        checkpoint {}", from)?;
    }
    match &recipe.spec.network {
        None => writeln!(out, "network:     offline during build")?,
        Some(n) if n.allow.is_empty() => writeln!(out, "network:     unrestricted during build")?,
        Some(n) => writeln!(out, "network:     {}", n.allow.join(", "))?,
    }
    if !recipe.spec.services.is_empty() {
        let names: Vec<&str> = recipe.spec.services.keys().map(String::as_str).collect();
        writeln!(out, "services:    {}", names.join(", "))?;
    }
    let steps: Vec<String> = recipe
        .steps
        .iter()
        .map(|(n, t)| format!("{} ({} bytes)", n, t.len()))
        .collect();
    writeln!(out, "steps:       {}", steps.join(", "))?;
    Ok(())
}

/// Official catalog and local directories build without asking. Anything
/// else prints its provenance and needs a yes, interactive or by flag.
fn confirm(recipe: &Recipe, yes: bool) -> Result<()> {
    if recipe.trusted {
        return Ok(());
    }
    let mut err = std::io::stderr();
    writeln!(err, "shuru: third-party recipe")?;
    print_summary(recipe, &mut err)?;
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "refusing to build third-party recipe {} without confirmation; pass --yes",
            recipe.source
        );
    }
    write!(err, "Build it? [y/N] ")?;
    err.flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        bail!("aborted");
    }
    Ok(())
}

// --- Commands ---

pub(crate) fn show(reference: &str) -> Result<()> {
    let recipe = resolve(reference)?;
    let mut out = std::io::stdout();
    print_summary(&recipe, &mut out)?;
    if !recipe.spec.services.is_empty() {
        writeln!(out)?;
        for (name, value) in &recipe.spec.services {
            writeln!(out, "--- /etc/shuru/services/{}.json ---", name)?;
            writeln!(out, "{}", serde_json::to_string_pretty(value)?)?;
        }
    }
    for (name, text) in &recipe.steps {
        writeln!(out)?;
        writeln!(out, "--- {} ---", name)?;
        write!(out, "{}", text)?;
        if !text.ends_with('\n') {
            writeln!(out)?;
        }
    }
    Ok(())
}

fn index_cache_path() -> String {
    format!("{}/recipes-index.json", default_data_dir())
}

fn fetch_index(refresh: bool) -> Result<Vec<IndexEntry>> {
    let cache = index_cache_path();
    if !refresh {
        if let Ok(meta) = std::fs::metadata(&cache) {
            let fresh = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .map(|age| age < INDEX_TTL)
                .unwrap_or(false);
            if fresh {
                if let Ok(text) = std::fs::read_to_string(&cache) {
                    if let Ok(entries) = serde_json::from_str::<Vec<IndexEntry>>(&text) {
                        return Ok(entries);
                    }
                }
            }
        }
    }
    let url = format!(
        "https://raw.githubusercontent.com/{}/{}/HEAD/index.json",
        OFFICIAL_OWNER, OFFICIAL_REPO
    );
    let text = http_get(&url, "application/json").context("fetching the recipe index")?;
    let entries: Vec<IndexEntry> =
        serde_json::from_str(&text).context("parsing the recipe index")?;
    if let Some(parent) = Path::new(&cache).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&cache, &text);
    Ok(entries)
}

pub(crate) fn list(refresh: bool) -> Result<()> {
    let entries = fetch_index(refresh)?;
    if entries.is_empty() {
        eprintln!("No recipes published yet.");
        return Ok(());
    }
    let width = entries.iter().map(|e| e.name.len()).max().unwrap_or(4).max(4);
    println!("{:<width$}  DESCRIPTION", "NAME", width = width);
    for e in &entries {
        println!("{:<width$}  {}", e.name, e.description, width = width);
    }
    eprintln!();
    eprintln!(
        "Source: github.com/{}/{}. Build one with `shuru run --recipe NAME` or `shuru checkpoint create NAME --recipe NAME`.",
        OFFICIAL_OWNER, OFFICIAL_REPO
    );
    Ok(())
}

// --- Building ---

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

/// The guest command for one step: write service files on the first step,
/// then run the script from tmpfs so it never lands in the checkpoint.
fn step_command(recipe: &Recipe, index: usize, script: &str) -> Result<Vec<String>> {
    let mut payload = String::from("set -e\n");
    if index == 0 && !recipe.spec.services.is_empty() {
        payload.push_str("mkdir -p /etc/shuru/services\n");
        for (name, value) in &recipe.spec.services {
            let json = serde_json::to_string_pretty(value)?;
            payload.push_str(&format!(
                "echo {} | base64 -d > /etc/shuru/services/{}.json\n",
                b64(&json),
                name
            ));
        }
    }
    payload.push_str(&format!(
        "echo {} | base64 -d > /tmp/shuru-recipe-step.sh\n",
        b64(script)
    ));
    payload.push_str("exec sh /tmp/shuru-recipe-step.sh\n");
    Ok(vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("echo {} | base64 -d | sh", b64(&payload)),
    ])
}

/// VM settings for a build: the recipe decides size and network, the user
/// may enlarge the VM, and nothing from the project config or the user
/// mounts, ports and secrets reaches the build.
fn build_args(vm: &VmArgs, spec: &RecipeSpec) -> VmArgs {
    VmArgs {
        cpus: vm.cpus.or(spec.cpus),
        memory: vm.memory.or(spec.memory),
        disk_size: vm.disk_size.or(spec.disk_size),
        kernel: vm.kernel.clone(),
        rootfs: vm.rootfs.clone(),
        initrd: vm.initrd.clone(),
        allow_net: spec.network.is_some(),
        dns_resolver: vm.dns_resolver,
        allow_host_writes: false,
        port: Vec::new(),
        mount: Vec::new(),
        secret: Vec::new(),
        allow_host: spec
            .network
            .as_ref()
            .map(|n| n.allow.clone())
            .unwrap_or_default(),
        expose_host: Vec::new(),
        config: None,
        verbose: vm.verbose,
    }
}

fn meta_path(name: &str) -> String {
    format!("{}/checkpoints/{}.json", default_data_dir(), name)
}

pub(crate) fn read_meta(name: &str) -> Option<CheckpointMeta> {
    let text = std::fs::read_to_string(meta_path(name)).ok()?;
    serde_json::from_str(&text).ok()
}

pub(crate) fn remove_meta(name: &str) {
    let _ = std::fs::remove_file(meta_path(name));
}

/// Build `name` from the recipe, one checkpoint per step.
pub(crate) fn build(name: &str, recipe: &Recipe, vm: &VmArgs) -> Result<()> {
    shuru_vm::validate_checkpoint_name(name).map_err(|e| anyhow::anyhow!(e))?;
    let total = recipe.steps.len();
    eprintln!(
        "shuru: building recipe {} from {}@{} ({} step{})",
        recipe.spec.name,
        recipe.source,
        recipe.short_revision(),
        total,
        if total == 1 { "" } else { "s" }
    );
    let args = build_args(vm, &recipe.spec);
    let cfg = ShuruConfig::default();
    let mut from = recipe.spec.from.clone();
    let mut intermediates: Vec<String> = Vec::new();

    for (i, (step_name, script)) in recipe.steps.iter().enumerate() {
        let last = i + 1 == total;
        let target = if last {
            name.to_string()
        } else {
            format!("{}-step{}", name, i + 1)
        };
        eprintln!("shuru: step {}/{}: {}", i + 1, total, step_name);
        let command = step_command(recipe, i, script)?;
        let code = checkpoint::create_with_config(target.clone(), &args, &cfg, from.as_deref(), command)?;
        if code != 0 {
            let _ = checkpoint::delete(&target);
            if let Some(kept) = intermediates.last() {
                eprintln!(
                    "shuru: intermediate checkpoint {} kept for debugging; delete it with `shuru checkpoint delete {}`",
                    kept, kept
                );
            }
            bail!("recipe step {} exited with status {}", step_name, code);
        }
        if !last {
            intermediates.push(target.clone());
        }
        from = Some(target);
    }

    for t in &intermediates {
        checkpoint::delete(t)?;
    }

    let meta = CheckpointMeta {
        recipe: RecipeMeta {
            name: recipe.spec.name.clone(),
            source: recipe.source.clone(),
            revision: recipe.revision.clone(),
        },
    };
    std::fs::write(meta_path(name), serde_json::to_string_pretty(&meta)?)
        .context("writing checkpoint metadata")?;
    Ok(())
}

/// `shuru checkpoint create NAME --recipe REF`.
pub(crate) fn create_checkpoint(name: &str, reference: &str, vm: &VmArgs, yes: bool) -> Result<()> {
    let recipe = resolve(reference)?;
    confirm(&recipe, yes)?;
    if checkpoint::exists(name) {
        bail!(
            "checkpoint {} already exists; use `shuru checkpoint update` to replace it",
            name
        );
    }
    build(name, &recipe, vm)
}

/// `shuru checkpoint update NAME --recipe REF`: rebuild even when the
/// revision matches. The old checkpoint stays until the build succeeds.
pub(crate) fn update_checkpoint(name: &str, reference: &str, vm: &VmArgs, yes: bool) -> Result<()> {
    let recipe = resolve(reference)?;
    confirm(&recipe, yes)?;
    if !checkpoint::exists(name) {
        return build(name, &recipe, vm);
    }
    let staging = checkpoint::staging_name(name);
    if checkpoint::exists(&staging) {
        checkpoint::delete(&staging)?;
    }
    build(&staging, &recipe, vm)?;
    checkpoint::replace(name, &staging)
}

/// `shuru run --recipe REF [--checkpoint NAME]`: make sure a checkpoint
/// named after the recipe, or as given, exists at the resolved revision, and
/// return its name.
pub(crate) fn ensure_checkpoint(
    reference: &str,
    vm: &VmArgs,
    yes: bool,
    name: Option<&str>,
) -> Result<String> {
    let recipe = resolve(reference)?;
    confirm(&recipe, yes)?;
    let name = name
        .map(str::to_string)
        .unwrap_or_else(|| recipe.spec.name.clone());
    if checkpoint::exists(&name) {
        match read_meta(&name) {
            Some(meta) if meta.recipe.revision == recipe.revision => {
                eprintln!(
                    "shuru: using checkpoint {} ({}@{})",
                    name,
                    meta.recipe.source,
                    recipe.short_revision()
                );
                return Ok(name);
            }
            Some(meta) => {
                eprintln!(
                    "shuru: recipe changed ({}@{} -> {}@{}), rebuilding checkpoint {}",
                    meta.recipe.source,
                    &meta.recipe.revision[..meta.recipe.revision.len().min(7)],
                    recipe.source,
                    recipe.short_revision(),
                    name
                );
                checkpoint::delete(&name)?;
            }
            None => bail!(
                "checkpoint {} exists but was not built from a recipe; delete it or run with --from {}",
                name,
                name
            ),
        }
    }
    build(&name, &recipe, vm)?;
    Ok(name)
}
