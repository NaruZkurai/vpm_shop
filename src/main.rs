//! vpm-upload-api — standalone VPM shop upload service.
//!
//! POST /upload: multipart (username, password, name, version, category, file)
//!   - validates creds from API_USER/API_PASS env (vpm-shop.conf)
//!   - extracts the .unitypackage into VPM/<Category>/<dirname>/
//!   - builds a deterministic VPM zip (package.json + assets)
//!   - updates the master vpm-repo.json
//!   - regenerates category repos via gen_category_repos.py
//!   - validates with the real VCC DLL (vpm-core-lib) via the vpmval harness

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use axum::extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use flate2::read::GzDecoder;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tar::Archive;

#[derive(Clone)]
struct Config {
    shop: PathBuf,
    validator: PathBuf,
    username: String,
    password: String,
    master_host: String,
    vault_host: String,
    categories: Vec<String>,
    /// Editable repo metadata (names + url-ids) backed by repos.conf. The
    /// on-disk file is the source of truth for gen_category_repos.py; the
    /// /api/repos editor reads/writes it directly.
    repos_conf_path: PathBuf,
}

/// One editable category's per-repo metadata from repos.conf.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct RepoCategory {
    name: String,
    sub: String,
    repo_name: String,
    repo_id: String,
}

/// The editable fields of repos.conf relevant to the web UI.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct RepoConf {
    master_host: String,
    subdomain_host: String,
    author: String,
    master_name: String,
    master_id: String,
    categories: Vec<RepoCategory>,
}

/// Build a default repo-category entry for a category name. Used when
/// repos.conf is missing or doesn't yet list the category.
fn default_repo_category(name: &str) -> RepoCategory {
    let sub = name.to_lowercase();
    RepoCategory {
        name: name.to_string(),
        sub: sub.clone(),
        repo_name: format!("VPM – {name}"),
        repo_id: format!("com.vpm.{sub}"),
    }
}

/// Load repos.conf into a RepoConf. When the file is missing it falls back to
/// safe defaults derived from the editable category names (mirroring how
/// categories.txt behaves), so a fresh checkout still serves a working editor.
fn load_repo_conf(path: &Path, default_cats: &[String]) -> Result<RepoConf> {
    if !path.is_file() {
        // No repos.conf yet — empty display/domain fields (no fabricated
        // example.com values). master_host stays empty so callers can detect
        // that it's unset and refuse to publish rather than guess a URL.
        return Ok(RepoConf {
            master_host: String::new(),
            subdomain_host: String::new(),
            author: String::new(),
            master_name: String::new(),
            master_id: String::new(),
            categories: default_cats
                .iter()
                .map(|c| default_repo_category(c))
                .collect(),
        });
    }
    let txt = fs::read_to_string(path)
        .with_context(|| format!("cannot read repos.conf {}", path.display()))?;
    let v: Value = serde_json::from_str(&txt)
        .with_context(|| format!("cannot parse repos.conf {}", path.display()))?;

    let g = |k: &str, d: &str| -> String {
        v.get(k).and_then(|x| x.as_str()).unwrap_or(d).to_string()
    };

    let mut categories: Vec<RepoCategory> = Vec::new();
    if let Some(arr) = v.get("categories").and_then(|x| x.as_array()) {
        for c in arr {
            let name = c.get("name").and_then(|x| x.as_str()).unwrap_or("Misc");
            let sub = c.get("sub").and_then(|x| x.as_str()).unwrap_or("misc");
            categories.push(RepoCategory {
                name: name.to_string(),
                sub: sub.to_string(),
                repo_name: c
                    .get("repo_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or(&format!("VPM – {name}"))
                    .to_string(),
                repo_id: c
                    .get("repo_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    // Ensure every editable category name appears (e.g. categories.txt grew).
    for c in default_cats {
        if !categories.iter().any(|rc| &rc.name == c) {
            categories.push(default_repo_category(c));
        }
    }

    Ok(RepoConf {
        master_host: g("master_host", ""),
        subdomain_host: g("subdomain_host", ""),
        author: g("author", ""),
        master_name: g("master_name", "VPM Shop – Master"),
        master_id: g("master_id", ""),
        categories,
    })
}

/// Read categories from `<shop>/categories.txt` (one per line). Categories are
/// file-driven only — no hardcoded fallback list. When the file is missing it
/// is created from `conf_cats` (the category names in repos.conf), so the shop
/// derives its category list from config instead of a hardcoded default.
fn load_categories(shop: &Path, conf_cats: &[String]) -> Result<Vec<String>> {
    let path = shop.join("categories.txt");
    if !path.exists() {
        if conf_cats.is_empty() {
            bail!(
                "no categories file {} and no categories in repos.conf — add a 'categories' entry or create the file",
                path.display()
            );
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, conf_cats.join("\n") + "\n")
            .with_context(|| format!("cannot write categories file {}", path.display()))?;
    }
    let txt = fs::read_to_string(&path)
        .with_context(|| format!("cannot read categories file {}", path.display()))?;
    let cats: Vec<String> = txt
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if cats.is_empty() {
        bail!("categories file {} is empty", path.display());
    }
    Ok(cats)
}

/// Read `key` from the environment, falling back to `default`. The default is
/// only ever a real path/URL baked into this repo — never a fabricated
/// credential.
fn env_or(key: &str, default: &str) -> String {
    match env::var(key) {
        Ok(v) => v,
        Err(_) => default.to_string(),
    }
}

/// Make a path absolute relative to the current dir, without requiring it to
/// exist yet. Falls back to the raw path if the process cwd is unreachable.
fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p),
        Err(_) => p.to_path_buf(),
    }
}

impl Config {
    fn from_env() -> Result<Config> {
        // Resolve shop/validator to absolute paths up front. Keeping them
        // relative ties every later path (incl. run_regen's current_dir +
        // VPM_SHOP env) to the process cwd, which breaks when the binary is
        // launched from anywhere but the repo root.
        let shop = absolutize(&PathBuf::from(env_or("VPM_SHOP", "./mnt/shop")));
        let validator = absolutize(&PathBuf::from(env_or("VALIDATOR", "./validator/vpmval.dll")));
        let repos_conf_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("repos.conf");
        // Derive fallback category names from repos.conf so categories.txt can
        // be created from config on first run (no hardcoded category list).
        let conf_cats = match load_repo_conf(&repos_conf_path, &[]) {
            Ok(rc) => rc.categories.iter().map(|c| c.name.clone()).collect(),
            Err(_) => Vec::new(),
        };
        let categories = load_categories(&shop, &conf_cats)?;
        // MASTER_HOST comes from the env, else from repos.conf (the source of
        // truth for gen_category_repos.py). No fabricated default: if neither
        // provides it, refuse to start rather than publish under a fake URL.
        let conf_master = load_repo_conf(&repos_conf_path, &categories)
            .map(|rc| rc.master_host)
            .unwrap_or_default();
        let master_host = match env::var("MASTER_HOST") {
            Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ if !conf_master.trim().is_empty() => conf_master.trim().to_string(),
            _ => bail!(
                "MASTER_HOST is not set — set it in vpm-shop.conf (or repos.conf \"master_host\") before publishing"
            ),
        };
        // Credentials are NOT hardcoded. They must come from the environment
        // (vmp-shop.conf, loaded by ./launch.sh, provides them).
        let username = env::var("API_USER")
            .context("API_USER is not set — run ./launch.sh to generate vpm-shop.conf")?;
        let password = env::var("API_PASS")
            .context("API_PASS is not set — run ./launch.sh to generate vpm-shop.conf")?;
        // No fabricated vault URL either — leave empty when unset; the UI
        // simply won't show the link.
        let vault_host = env::var("VAULT_HOST").unwrap_or_default();
        Ok(Config {
            shop,
            validator,
            username,
            password,
            master_host,
            vault_host,
            categories,
            repos_conf_path,
        })
    }
}

/// Create the shop dir + category subdirs and an empty master `vpm-repo.json`
/// if they don't already exist, so a fresh local checkout (./mnt/shop) works
/// without manual setup.
fn bootstrap_shop(cfg: &Config) -> Result<()> {
    fs::create_dir_all(&cfg.shop)?;
    for cat in &cfg.categories {
        fs::create_dir_all(cfg.shop.join("VPM").join(cat))?;
    }
    let repo_path = cfg.shop.join("vpm-repo.json");
    if !repo_path.exists() {
        let init = json!({
            "name": "VPM Shop",
            "url": format!("{}/index.json", cfg.master_host.trim_end_matches('/')),
            "packages": {}
        });
        fs::write(&repo_path, serde_json::to_string_pretty(&init)?)?;
    }
    Ok(())
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = env::temp_dir().join(format!("vpm-upload-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&base).ok();
    base
}

/// Move a file into place, falling back to copy+delete when the source and
/// destination live on different filesystems (EXDEV, os error 18). This allows
/// scratch files under /tmp to be moved into the shop dir even when /tmp is a
/// separate mount.
fn move_path(src: &Path, dst: &Path) -> Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            fs::copy(src, dst)?;
            let _ = fs::remove_file(src);
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

// ─── HTTP layer ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => panic!("{e:#}"),
    };
    if let Err(e) = bootstrap_shop(&cfg) {
        panic!("failed to initialize shop dirs: {e:#}");
    }
    let cfg = Arc::new(cfg);
    let app = Router::new()
        .route("/", get(index))
        .route("/css/index.css", get(css_handler))
        .route("/js/app.js", get(js_handler))
        .route("/upload", post(upload))
        .route("/health", get(health))
        .route("/api/packages", get(api_packages))
        .route("/api/package/{name}/{version}/files", get(api_package_files))
        .route("/api/package/{name}/{version}/file", get(api_package_file))
        .route("/api/package/{name}/{version}/json", get(api_package_json))
        .route("/api/package/{name}/{version}/deps", get(api_package_deps))
        .route("/api/package/{name}/{version}/deps", post(api_set_deps))
        .route("/api/package/{name}/{version}/convert-to-dep", get(api_convert_to_vpm_dep))
        .route("/api/delete/{name}/{version}", get(delete_version))
        .route("/api/checklist", get(api_checklist_get))
        .route("/api/checklist", post(api_checklist_save))
        .route("/api/categories", get(api_categories))
        .route("/api/config", get(api_config))
        .route("/api/repos", get(api_repos_get))
        .route("/api/repos", post(api_repos_save))
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024 * 1024)) // 100 GiB ceiling
        .with_state(cfg);

    let addr = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:55555".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("vpm-upload-api listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn index() -> Html<&'static str> { Html(index_html()) }

async fn health() -> &'static str { "ok" }

/// The embedded HTML shell (source of truth: webpagerootdir/Index.html).
fn index_html() -> &'static str { include_str!("webpagerootdir/Index.html") }

/// The stylesheet (source of truth: webpagerootdir/index.css).
fn css() -> &'static str { include_str!("webpagerootdir/index.css") }

/// The client-side JS (source of truth: src/js/app.js).
fn js() -> &'static str { include_str!("js/app.js") }

async fn css_handler() -> Response {
    (
        [("Content-Type", "text/css; charset=utf-8")],
        css(),
    )
    .into_response()
}

async fn js_handler() -> Response {
    (
        [("Content-Type", "application/javascript; charset=utf-8")],
        js(),
    )
    .into_response()
}

async fn upload(State(cfg): State<Arc<Config>>, mut mp: Multipart) -> Response {
    let mut username = None;
    let mut password = None;
    let mut name = None;
    let mut version = None;
    let mut category = None;
    let mut demote: Option<String> = None;
    let mut rc_slot: Option<String> = None;
    let mut files: Vec<(String, PathBuf)> = Vec::new();

    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return html_result(false, &format!("multipart error: {e}")),
        };
        let fname = field.name().unwrap_or("").to_string();
        match fname.as_str() {
            "username" => username = field.text().await.ok(),
            "password" => password = field.text().await.ok(),
            "name" => name = field.text().await.ok(),
            "version" => version = field.text().await.ok(),
            "category" => category = field.text().await.ok(),
            "demote" => demote = field.text().await.ok(),
            "rc_slot" => rc_slot = field.text().await.ok(),
            "file" => {
                let fname = field
                    .file_name()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "upload.bin".to_string());
                match stream_field(field).await {
                    Ok(p) => files.push((fname, p)),
                    Err(e) => return html_result(false, &format!("saving upload failed: {e}")),
                }
            }
            _ => { let _ = field.bytes().await; }
        }
    }

    match run_upload(
        &cfg,
        username,
        password,
        name,
        version,
        category,
        demote,
        rc_slot,
        files,
    )
    .await
    {
        Ok(msg) => html_result(true, &msg),
        Err(e) => html_result(false, &format!("{e:#}")),
    }
}

async fn stream_field(mut field: axum::extract::multipart::Field<'_>) -> Result<PathBuf> {
    let dir = scratch_dir("body");
    let path = dir.join("upload.bin");
    let mut f = fs::File::create(&path)?;
    while let Some(chunk) = field.chunk().await? {
        f.write_all(&chunk)?;
    }
    f.flush()?;
    Ok(path)
}

// ─── API: browse packages / view zip contents ───────────────────────────────

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

fn master_repo(cfg: &Config) -> Result<Value> {
    let txt = fs::read_to_string(cfg.shop.join("vpm-repo.json"))
        .context("cannot read master vpm-repo.json")?;
    Ok(serde_json::from_str(&txt)?)
}

/// Does the exact `version` key already exist for `name` in the registry?
fn version_exists(cfg: &Config, name: &str, version: &str) -> Result<bool> {
    Ok(master_repo(cfg)?
        .get("packages")
        .and_then(|p| p.get(name))
        .and_then(|p| p.get("versions"))
        .and_then(|v| v.as_object())
        .map(|v| v.contains_key(version))
        .unwrap_or(false))
}

/// Existing category for `name` (package-level, else any version entry's),
/// so re-uploads of an existing package don't drift categories.
fn existing_category(cfg: &Config, name: &str) -> Result<Option<String>> {
    let pkg = master_repo(cfg)?
        .get("packages")
        .and_then(|p| p.get(name))
        .cloned();
    let Some(pkg) = pkg else { return Ok(None) };
    if let Some(c) = pkg.get("category").and_then(|c| c.as_str()) {
        return Ok(Some(c.to_string()));
    }
    Ok(pkg
        .get("versions")
        .and_then(|v| v.as_object())
        .and_then(|o| o.values().next())
        .and_then(|e| e.get("category"))
        .and_then(|c| c.as_str())
        .map(|c| c.to_string()))
}

/// If `requested` already exists in the registry, keep the old stable version and
/// return `<version>-rc.<N>` where N is the next free rc candidate number
/// (max existing `-rc.<n>` + 1). Returns `(version, true)` when bumped.
fn resolve_upload_version(cfg: &Config, name: &str, requested: &str) -> Result<(String, bool)> {
    let repo = master_repo(cfg)?;
    let versions = repo
        .get("packages")
        .and_then(|p| p.get(name))
        .and_then(|p| p.get("versions"))
        .and_then(|v| v.as_object());
    if versions.map(|v| v.contains_key(requested)).unwrap_or(false) {
        let prefix = format!("{requested}-rc.");
        let max_n = versions
            .iter()
            .flat_map(|v| v.keys())
            .filter_map(|k| k.strip_prefix(&prefix))
            .filter_map(|s| s.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        return Ok((format!("{prefix}{}", max_n + 1), true));
    }
    Ok((requested.to_string(), false))
}

/// Demote the existing stable `version` to an RC candidate at the given slot
/// (or the next free slot when `rc_slot` is None), bumping any existing rc
/// candidates at/after the slot up by 1. Renames the zip on disk and updates
/// each ver_entry's `version` and `url`. Returns a human-readable summary.
fn demote_existing_version(
    cfg: &Config,
    name: &str,
    version: &str,
    rc_slot: Option<u64>,
) -> Result<String> {
    let repo_path = cfg.shop.join("vpm-repo.json");
    let txt = fs::read_to_string(&repo_path)?;
    let mut repo: Value = serde_json::from_str(&txt)?;
    let packages = repo
        .get_mut("packages")
        .ok_or_else(|| anyhow!("master repo has no 'packages'"))?;
    let pkg = packages
        .get_mut(name)
        .ok_or_else(|| anyhow!("package '{name}' not found in registry"))?;
    let versions = pkg
        .get_mut("versions")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow!("no versions for '{name}'"))?;

    if !versions.contains_key(version) {
        bail!("version '{version}' not found for '{name}' — nothing to demote");
    }

    // Existing rc candidates for this base version.
    let prefix = format!("{version}-rc.");
    let mut rcs: Vec<u64> = versions
        .keys()
        .filter_map(|k| k.strip_prefix(&prefix))
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();
    rcs.sort_unstable();

    // Chosen slot: user-provided, else next free after the highest existing rc.
    let slot = match rc_slot {
        Some(s) if s >= 1 => s,
        Some(_) => bail!("rc_slot must be >= 1"),
        None => rcs.last().map(|n| n + 1).unwrap_or(1),
    };

    // Bump existing rc candidates at/after the slot up by 1 (descending to
    // avoid key collisions while renaming).
    let mut bumped: Vec<(String, String)> = Vec::new();
    for n in rcs.iter().rev().copied() {
        if n >= slot {
            let old_key = format!("{prefix}{n}");
            let new_key = format!("{prefix}{}", n + 1);
            if let Some(mut entry) = versions.remove(&old_key) {
                rename_zip_version(cfg, &mut entry, &new_key)?;
                versions.insert(new_key.clone(), entry);
                bumped.push((old_key, new_key));
            }
        }
    }

    // Demote the old stable version to the chosen rc slot.
    let rc_key = format!("{prefix}{slot}");
    if let Some(mut entry) = versions.remove(version) {
        rename_zip_version(cfg, &mut entry, &rc_key)?;
        versions.insert(rc_key.clone(), entry);
    }

    let tmp = repo_path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&repo)?)?;
    fs::rename(&tmp, &repo_path)?;

    let mut msg = format!("Existing stable version <b>{version}</b> demoted to <b>{rc_key}</b>.");
    if !bumped.is_empty() {
        msg.push_str(&format!(
            " Existing rc candidates at/after slot {slot} upgraded by 1: {}.",
            bumped
                .iter()
                .map(|(a, b)| format!("{a}→{b}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(msg)
}

/// Rename a ver_entry's zip on disk and update its `version` + `url` fields.
fn rename_zip_version(cfg: &Config, entry: &mut Value, new_version: &str) -> Result<()> {
    let url = entry
        .get("url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("ver_entry has no url"))?;
    let url_path = url.split('?').next().unwrap_or(url);
    let master = format!("{}/", cfg.master_host.trim_end_matches('/'));
    let url_path = url_path.strip_prefix(&master).unwrap_or(url_path);
    // url_path = <Category>/<dirname>/<dirname>-<version>.zip
    let mut parts = url_path.split('/');
    let category = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("cannot derive category from url {url}"))?
        .to_string();
    let dirname = parts
        .next()
        .ok_or_else(|| anyhow!("cannot derive dirname from url {url}"))?
        .to_string();
    let old_zip = cfg.shop.join("VPM").join(url_path);
    let new_zip = old_zip.with_file_name(format!("{dirname}-{new_version}.zip"));
    if old_zip.is_file() && old_zip != new_zip {
        fs::rename(&old_zip, &new_zip)?;
    }
    entry["version"] = json!(new_version);
    entry["url"] = json!(format!(
        "{}/{category}/{dirname}/{dirname}-{new_version}.zip",
        cfg.master_host.trim_end_matches('/')
    ));
    Ok(())
}

fn find_zip(cfg: &Config, name: &str, version: &str) -> Result<PathBuf> {
    let repo = master_repo(cfg)?;
    let pkg = repo
        .get("packages")
        .and_then(|p| p.get(name))
        .ok_or_else(|| anyhow!("package '{name}' not found in registry"))?;
    let ver = pkg
        .get("versions")
        .and_then(|v| v.get(version))
        .ok_or_else(|| anyhow!("version '{version}' not found for '{name}'"))?;
    // The on-disk dirname lives in the ver_entry URL, e.g.
    // https://<MASTER_HOST>/Props/<dirname>/<dirname>-<ver>.zip
    let url = ver
        .get("url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("no url in ver_entry for '{name}@{version}'"))?;
    let url_path = url.split('?').next().unwrap_or(url);
    let master = format!("{}/", cfg.master_host.trim_end_matches('/'));
    let url_path = url_path.strip_prefix(&master).unwrap_or(url_path);
    // url_path = <Category>/<dirname>/<dirname>-<version>.zip
    let rel = cfg.shop.join("VPM").join(url_path);
    if !rel.is_file() {
        bail!("zip not found on disk: {}", rel.display());
    }
    Ok(rel)
}

async fn api_packages(State(cfg): State<Arc<Config>>) -> Response {
    match master_repo(&cfg) {
        Ok(repo) => json_response(repo),
        Err(e) => err_response(&format!("{e:#}")),
    }
}

async fn api_categories(State(cfg): State<Arc<Config>>) -> Response {
    json_response(json!(cfg.categories))
}

/// GET /api/config — resolved run-time URLs (master repo + optional vault),
/// so the UI doesn't hardcode example.com hosts.
async fn api_config(State(cfg): State<Arc<Config>>) -> Response {
    json_response(json!({
        "master_url": cfg.master_host,
        "vault_url": cfg.vault_host,
        "master_repo_url": format!("{}/index.json", cfg.master_host.trim_end_matches('/')),
    }))
}

// ─── Repo metadata editor (repos.conf) ─────────────────────────────────────

/// GET /api/repos — return the editable repo metadata (master name/id + each
/// category's repo_name/repo_id) so the UI can render an editor. Re-read from
/// disk each call so the UI always reflects the saved state.
async fn api_repos_get(State(cfg): State<Arc<Config>>) -> Response {
    match load_repo_conf(&cfg.repos_conf_path, &cfg.categories) {
        Ok(rc) => json_response(json!({
            "master_name": rc.master_name,
            "master_id": rc.master_id,
            "categories": rc.categories,
        })),
        Err(e) => err_response(&format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct RepoCategoryUpdate {
    name: String,
    #[serde(default)]
    repo_name: Option<String>,
    #[serde(default)]
    repo_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct ReposUpdate {
    #[serde(default)]
    master_name: Option<String>,
    #[serde(default)]
    master_id: Option<String>,
    #[serde(default)]
    categories: Vec<RepoCategoryUpdate>,
}

/// POST /api/repos — update master name/id and per-category repo_name/repo_id,
/// write the result back to repos.conf, then regenerate the category repos so
/// the new names/ids take effect in the published vpm-repo.json files.
async fn api_repos_save(
    State(cfg): State<Arc<Config>>,
    axum::Json(body): axum::Json<ReposUpdate>,
) -> Response {
    match run_repos_save(&cfg, &body) {
        Ok(msg) => json_response(json!({ "ok": true, "message": msg })),
        Err(e) => err_response(&format!("{e:#}")),
    }
}

fn run_repos_save(cfg: &Config, body: &ReposUpdate) -> Result<String> {
    let path = &cfg.repos_conf_path;
    // Keep any fields we don't edit (master_host, subdomain_host, author, sub):
    // read the existing file, mutate only the editable keys, write it back.
    let mut v: Value = if path.is_file() {
        serde_json::from_str(&fs::read_to_string(path)?)
            .with_context(|| format!("cannot parse repos.conf {}", path.display()))?
    } else {
        json!({ "categories": [] })
    };
    if let Some(mn) = &body.master_name {
        if !mn.trim().is_empty() {
            v["master_name"] = json!(mn);
        }
    }
    if let Some(mi) = &body.master_id {
        if !mi.trim().is_empty() {
            v["master_id"] = json!(mi);
        }
    }
    let cats = v
        .get_mut("categories")
        .and_then(|c| c.as_array_mut())
        .ok_or_else(|| anyhow!("repos.conf has no 'categories' array"))?;
    for update in &body.categories {
        let Some(entry) = cats
            .iter_mut()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some(&update.name))
        else {
            continue;
        };
        if let Some(rn) = &update.repo_name {
            if !rn.trim().is_empty() {
                entry["repo_name"] = json!(rn);
            }
        }
        if let Some(ri) = &update.repo_id {
            if !ri.trim().is_empty() {
                entry["repo_id"] = json!(ri);
            }
        }
    }
    let tmp = path.with_extension("conf.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&v)?)?;
    fs::rename(&tmp, path)?;
    // Regenerate the category repos with the new names/ids.
    let regen = run_regen(&cfg.shop)?;
    Ok(format!("Saved repos.conf and regenerated category repos.\n{regen}"))
}

// ─── Private checklist (password-protected personal notes) ─────────────────

fn checklist_path(cfg: &Config) -> PathBuf {
    cfg.shop.join(".checklist.json")
}

fn checklist_auth_ok(cfg: &Config, headers: &HeaderMap) -> bool {
    headers
        .get("x-api-password")
        .and_then(|v| v.to_str().ok())
        .map(|p| p == cfg.password)
        .unwrap_or(false)
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({ "ok": false, "error": "authentication required" })),
    )
        .into_response()
}

async fn api_checklist_get(State(cfg): State<Arc<Config>>, headers: HeaderMap) -> Response {
    if !checklist_auth_ok(&cfg, &headers) {
        return unauthorized_response();
    }
    match fs::read_to_string(checklist_path(&cfg)) {
        Ok(txt) => match serde_json::from_str::<Value>(&txt) {
            Ok(v) => json_response(v),
            Err(_) => json_response(json!({ "items": [] })),
        },
        Err(_) => json_response(json!({ "items": [] })),
    }
}

async fn api_checklist_save(
    State(cfg): State<Arc<Config>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !checklist_auth_ok(&cfg, &headers) {
        return unauthorized_response();
    }
    match body.get("items") {
        None => return err_response("body must include an 'items' array"),
        Some(i) if !i.is_array() => return err_response("'items' must be an array"),
        _ => {}
    }
    let path = checklist_path(&cfg);
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp, serde_json::to_string_pretty(&body).unwrap_or_default())
        .and_then(|_| fs::rename(&tmp, &path))
    {
        return err_response(&format!("failed to save checklist: {e:#}"));
    }
    json_response(json!({ "ok": true, "message": "checklist saved" }))
}

async fn api_package_files(
    State(cfg): State<Arc<Config>>,
    AxumPath((name, version)): AxumPath<(String, String)>,
) -> Response {
    let zip_path = match find_zip(&cfg, &name, &version) {
        Ok(p) => p,
        Err(e) => return err_response(&format!("{e:#}")),
    };
    let file = match fs::File::open(&zip_path) {
        Ok(f) => f,
        Err(e) => return err_response(&format!("cannot open zip: {e}")),
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => return err_response(&format!("zip read error: {e}")),
    };
    let mut files: Vec<Value> = Vec::new();
    for i in 0..archive.len() {
        let entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        files.push(json!({
            "name": entry.name(),
            "size": entry.size(),
            "is_dir": entry.is_dir(),
        }));
    }
    json_response(json!({
        "package": name,
        "version": version,
        "zip": zip_path.to_string_lossy(),
        "files": files,
    }))
}

async fn api_package_file(
    State(cfg): State<Arc<Config>>,
    AxumPath((name, version)): AxumPath<(String, String)>,
    Query(q): Query<FileQuery>,
) -> Response {
    let zip_path = match find_zip(&cfg, &name, &version) {
        Ok(p) => p,
        Err(e) => return err_response(&format!("{e:#}")),
    };
    let file = match fs::File::open(&zip_path) {
        Ok(f) => f,
        Err(e) => return err_response(&format!("cannot open zip: {e}")),
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => return err_response(&format!("zip read error: {e}")),
    };
    let mut entry = match archive.by_name(&q.path) {
        Ok(e) => e,
        Err(e) => return err_response(&format!("file not found in zip: {e}")),
    };
    if entry.is_dir() {
        return err_response("that's a directory, not a file");
    }
    let mut buf = Vec::new();
    if entry.read_to_end(&mut buf).is_err() {
        return err_response("error reading file");
    }
    let content = if buf.iter().any(|b| *b == 0) || buf.len() > 2 * 1024 * 1024 {
        format!("[binary file, {} bytes — not shown]", buf.len())
    } else {
        String::from_utf8_lossy(&buf).to_string()
    };
    json_response(json!({
        "name": q.path,
        "size": buf.len(),
        "content": content,
        "is_binary": content.starts_with("[binary"),
    }))
}

async fn api_package_deps(
    State(cfg): State<Arc<Config>>,
    AxumPath((name, version)): AxumPath<(String, String)>,
) -> Response {
    let zip_path = match find_zip(&cfg, &name, &version) {
        Ok(p) => p,
        Err(e) => return err_response(&format!("{e:#}")),
    };
    let file = match fs::File::open(&zip_path) {
        Ok(f) => f,
        Err(e) => return err_response(&format!("cannot open zip: {e}")),
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => return err_response(&format!("zip read error: {e}")),
    };
    let content: Result<Vec<u8>> = (|| {
        let mut e = archive
            .by_name("package.json")
            .map_err(|e| anyhow!("no package.json in zip: {e}"))?;
        let mut buf = Vec::new();
        e.read_to_end(&mut buf).map_err(|e| anyhow!("read error: {e}"))?;
        Ok(buf)
    })();
    let buf = match content {
        Ok(b) => b,
        Err(e) => return err_response(&format!("{e:#}")),
    };
    let pj: Value = match serde_json::from_slice(&buf) {
        Ok(v) => v,
        Err(_) => return err_response("package.json in zip is not valid JSON"),
    };
    json_response(json!({
        "dependencies": pj.get("dependencies").cloned().unwrap_or_else(|| json!({})),
        "vpmDependencies": pj.get("vpmDependencies").cloned().unwrap_or_else(|| json!({})),
        "unity": pj.get("unity").and_then(|u| u.as_str()).unwrap_or("").to_string(),
    }))
}

#[derive(serde::Deserialize)]
struct ConvertQuery { path: String }

async fn api_convert_to_vpm_dep(
    State(cfg): State<Arc<Config>>,
    AxumPath((name, version)): AxumPath<(String, String)>,
    Query(q): Query<ConvertQuery>,
) -> Response {
    let path = q.path;
    let folder_name = path.split('/').last().unwrap_or(&path);
    match run_convert_to_vpm_dep(&cfg, &name, &version, &path, &folder_name) {
        Ok(msg) => json_response(json!({ "ok": true, "message": msg })),
        Err(e) => err_response(&format!("{e:#}")),
    }
}

fn run_convert_to_vpm_dep(cfg: &Config, name: &str, version: &str, path: &str, folder_name: &str) -> Result<String> {
    let zip_path = find_zip(cfg, name, version)?;
    let work_dir = scratch_dir("convert");
    let out_dir = work_dir.join("out");
    fs::create_dir_all(&out_dir)?;

    // Read the original package.json so we can preserve category/url/metadata.
    let mut archive = zip::ZipArchive::new(fs::File::open(&zip_path)?)?;
    let orig_pkg_json: Value = {
        let mut e = archive
            .by_name("package.json")
            .map_err(|e| anyhow!("no package.json in zip: {e}"))?;
        let mut buf = Vec::new();
        e.read_to_end(&mut buf)?;
        serde_json::from_slice(&buf).unwrap_or_else(|_| json!({}))
    };

    // Extract every file except the folder (and its subtree) being converted and
    // except package.json (build_zip writes that itself).
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let entry_name = entry.name().to_string();
        if entry.is_dir()
            || entry_name == "package.json"
            || entry_name.is_empty()
            || entry_name.split('/').any(|s| s == "..")
            || entry_name == path
            || entry_name.starts_with(&format!("{path}/"))
        {
            continue;
        }
        let dest = out_dir.join(&entry_name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&dest)?;
        io::copy(&mut entry, &mut f)?;
    }

    // Rebuild the zip in place (over the served file) with the folder removed.
    build_zip(&out_dir, &zip_path, &orig_pkg_json)?;
    let zip_sha = sha256_file(&zip_path)?;

    // Update repo.json: attach the converted folder as a VPM dependency.
    let repo_path = cfg.shop.join("vpm-repo.json");
    let txt = fs::read_to_string(&repo_path)?;
    let mut repo: Value = serde_json::from_str(&txt)?;
    let pkg = repo["packages"][name].as_object_mut().ok_or_else(|| anyhow!("package not found"))?;
    let ver_entry = pkg["versions"][version].as_object_mut().ok_or_else(|| anyhow!("version not found"))?;

    let vpm_deps = ver_entry.get("vpmDependencies").cloned().unwrap_or_else(|| json!({}));
    let mut vpm_deps_map = match vpm_deps.as_object() {
        Some(m) => m.clone(),
        None => serde_json::Map::new(),
    };
    vpm_deps_map.insert(folder_name.to_string(), json!("1.0.0"));
    ver_entry["vpmDependencies"] = json!(vpm_deps_map);
    ver_entry["zipSHA256"] = json!(zip_sha);

    let tmp_repo = repo_path.with_extension("json.tmp");
    fs::write(&tmp_repo, serde_json::to_string_pretty(&repo)?)?;
    fs::rename(&tmp_repo, &repo_path)?;

    run_regen(&cfg.shop)?;
    let _ = run_cmd("dotnet", &["--manifest", &cfg.shop.join("VPM/vpm-repo.json").to_string_lossy()]).with_context(|| "validation failed")?;

    let _ = fs::remove_dir_all(&work_dir);
    Ok(format!("converted folder '{path}' to VPM dependency '{folder_name}'"))
}

#[derive(serde::Deserialize)]
struct SetDepsBody {
    #[serde(default)]
    dependencies: Value,
    #[serde(default)]
    #[allow(non_snake_case)]
    vpmDependencies: Value,
}

async fn api_set_deps(
    State(cfg): State<Arc<Config>>,
    AxumPath((name, version)): AxumPath<(String, String)>,
    axum::Json(body): axum::Json<SetDepsBody>,
) -> Response {
    match run_set_deps(
        &cfg,
        &name,
        &version,
        &body.dependencies,
        &body.vpmDependencies,
    ) {
        Ok(msg) => json_response(json!({ "ok": true, "message": msg })),
        Err(e) => err_response(&format!("{e:#}")),
    }
}

fn run_set_deps(cfg: &Config, name: &str, version: &str, deps: &Value, vpm_deps: &Value) -> Result<String> {
    let zip_path = find_zip(cfg, name, version)?;

    // 1. read existing package.json from the zip
    let file = fs::File::open(&zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut pj_bytes = Vec::new();
    {
        let mut e = archive
            .by_name("package.json")
            .map_err(|e| anyhow!("no package.json in zip: {e}"))?;
        e.read_to_end(&mut pj_bytes)?;
    }
    let mut pj: Value = serde_json::from_slice(&pj_bytes)?;

    // 2. apply new deps
    pj["dependencies"] = deps.clone();
    pj["vpmDependencies"] = vpm_deps.clone();

    // 3. rebuild zip deterministically from extracted contents + new package.json
    let work = scratch_dir("deps");
    let out_dir = work.join("out");
    fs::create_dir_all(&out_dir)?;
    {
        let mut archive = zip::ZipArchive::new(fs::File::open(&zip_path)?)?;
        for i in 0..archive.len() {
            let mut e = archive.by_index(i)?;
            if e.is_dir() || e.name() == "package.json" {
                continue;
            }
            let dest = out_dir.join(e.name());
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut f = fs::File::create(&dest)?;
            io::copy(&mut e, &mut f)?;
        }
    }
    // build_zip writes package.json itself from `pj` — do NOT place it in out_dir
    build_zip(&out_dir, &zip_path, &pj)?;
    let zip_sha = sha256_file(&zip_path)?;

    // 4. update master repo entry
    let repo_path = cfg.shop.join("vpm-repo.json");
    let txt = fs::read_to_string(&repo_path)?;
    let mut repo: Value = serde_json::from_str(&txt)?;
    let ver = repo["packages"][name]["versions"][version]
        .as_object_mut()
        .ok_or_else(|| anyhow!("version entry missing for {name}@{version}"))?;
    ver.insert("dependencies".into(), deps.clone());
    ver.insert("vpmDependencies".into(), vpm_deps.clone());
    ver.insert("zipSHA256".into(), json!(zip_sha));
    let tmp = repo_path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&repo)?)?;
    fs::rename(&tmp, &repo_path)?;

    // 5. regenerate category repos + validate
    run_regen(&cfg.shop)?;
    let _ = run_cmd(
        "dotnet",
        &[
            &cfg.validator.to_string_lossy(),
            &cfg.shop.join("VPM/vpm-repo.json").to_string_lossy(),
        ],
    )?;

    let _ = fs::remove_dir_all(&work);
    Ok(format!(
        "updated dependencies for {name}@{version} (zipSHA256: {})",
        &zip_sha[..16]
    ))
}

async fn api_package_json(
    State(cfg): State<Arc<Config>>,
    AxumPath((name, version)): AxumPath<(String, String)>,
) -> Response {
    let zip_path = match find_zip(&cfg, &name, &version) {
        Ok(p) => p,
        Err(e) => return err_response(&format!("{e:#}")),
    };
    let file = match fs::File::open(&zip_path) {
        Ok(f) => f,
        Err(e) => return err_response(&format!("cannot open zip: {e}")),
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => return err_response(&format!("zip read error: {e}")),
    };
    let content: Result<Vec<u8>> = (|| {
        let mut e = archive
            .by_name("package.json")
            .map_err(|e| anyhow!("no package.json in zip: {e}"))?;
        let mut buf = Vec::new();
        e.read_to_end(&mut buf).map_err(|e| anyhow!("read error: {e}"))?;
        Ok(buf)
    })();
    let buf = match content {
        Ok(b) => b,
        Err(e) => return err_response(&format!("{e:#}")),
    };
    match serde_json::from_slice::<Value>(&buf) {
        Ok(v) => json_response(v),
        Err(_) => json_response(json!({ "raw": String::from_utf8_lossy(&buf).to_string() })),
    }
}

async fn delete_version(
    State(cfg): State<Arc<Config>>,
    AxumPath((name, version)): AxumPath<(String, String)>,
) -> Response {
    match run_delete_version(&cfg, &name, &version) {
        Ok(msg) => json_response(json!({"ok": true, "message": msg})),
        Err(e) => err_response(&format!("{e:#}")),
    }
}

fn run_delete_version(cfg: &Config, name: &str, version: &str) -> Result<String> {
    let repo_path = cfg.shop.join("vpm-repo.json");
    let txt = fs::read_to_string(&repo_path)?;
    let mut repo: Value = serde_json::from_str(&txt)?;
    let pkg = repo
        .get_mut("packages")
        .and_then(|p| p.get_mut(name))
        .ok_or_else(|| anyhow!("package '{name}' not found"))?;
    let versions = pkg
        .get_mut("versions")
        .ok_or_else(|| anyhow!("no versions"))?;
    versions
        .as_object_mut()
        .ok_or_else(|| anyhow!("versions not an object"))?
        .remove(version)
        .ok_or_else(|| anyhow!("version '{version}' not found"))?;

    // remove zip file on disk
    let zip_path = find_zip(cfg, name, version)?;
    let _ = fs::remove_file(&zip_path);

    // if no versions left, drop whole package
    let versions_obj = versions.as_object().map(|o| o.len()).unwrap_or(0);
    if versions_obj == 0 {
        repo.get_mut("packages")
            .and_then(|p| p.as_object_mut())
            .map(|o| o.remove(name));
    }

    let tmp = repo_path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&repo)?)?;
    fs::rename(&tmp, &repo_path)?;

    let regen = run_regen(&cfg.shop)?;
    let _ = run_cmd(
        "dotnet",
        &[
            &cfg.validator.to_string_lossy(),
            &cfg.shop.join("VPM/vpm-repo.json").to_string_lossy(),
        ],
    )?;
    Ok(format!("removed {name}@{version} ({regen})"))
}

fn json_response(v: Value) -> Response {
    (StatusCode::OK, axum::Json(v)).into_response()
}

fn err_response(msg: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({ "ok": false, "error": msg })),
    )
        .into_response()
}

// ─── Core pipeline ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_upload(
    cfg: &Config,
    username: Option<String>,
    password: Option<String>,
    name: Option<String>,
    version: Option<String>,
    category: Option<String>,
    demote: Option<String>,
    rc_slot: Option<String>,
    files: Vec<(String, PathBuf)>,
) -> Result<String> {
    if username.as_deref() != Some(&cfg.username) || password.as_deref() != Some(&cfg.password) {
        bail!("authentication failed (wrong username/password)");
    }
    let name = name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let version = version
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if files.is_empty() {
        bail!("no file uploaded");
    }

    // Validate inputs
    let name = name.ok_or_else(|| anyhow!("missing 'name'"))?;
    let name = sanitize_name(&name);
    if !is_valid_pkg_id(&name) {
        bail!("invalid package id '{name}' (expected lowercase reverse-domain, e.g. com.author.package)");
    }
    let version = version.ok_or_else(|| anyhow!("missing 'version'"))?;
    if !is_valid_version(&version) {
        bail!("invalid version '{version}' (expected semver, e.g. 1.0.0)");
    }

    // Version collision policy:
    //  - demote=true: the existing stable version is demoted to an RC candidate
    //    (at rc_slot, or the next free slot) and this upload becomes the stable
    //    version; existing rc candidates at/after the slot are bumped up by 1.
    //  - demote=false (default): the old stable version is kept and this upload is
    //    published as <version>-rc.<N>.
    let want_demote = demote
        .as_deref()
        .map(|d| matches!(d.to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"))
        .unwrap_or(false);
    let rc_slot: Option<u64> = match rc_slot.as_deref() {
        Some(s) if !s.trim().is_empty() => Some(
            s.trim()
                .parse()
                .context("rc_slot must be a positive integer")?,
        ),
        _ => None,
    };
    if let Some(s) = rc_slot {
        if s < 1 {
            bail!("rc_slot must be >= 1");
        }
    }
    let demote_requested = want_demote;
    let (version, rc_bumped) = if demote_requested {
        (version, false) // new upload takes the stable version; demote runs later
    } else {
        resolve_upload_version(cfg, &name, &version)?
    };
    let mut demote_note = String::new();
    let category = match category {
        Some(c) => c,
        // Re-uploads of an existing package inherit its current category.
        None => existing_category(cfg, &name)?.unwrap_or_else(|| "Misc".into()),
    };
    if !cfg.categories.contains(&category) {
        bail!(
            "invalid category '{category}' (choose from: {})",
            cfg.categories.join(", ")
        );
    }
    for (fname, fpath) in &files {
        if !fpath.is_file() || fs::metadata(fpath)?.len() == 0 {
            bail!("uploaded file '{fname}' is empty or unreadable");
        }
    }

    let work = scratch_dir("work");
    let out_dir = work.join("out");
    fs::create_dir_all(&out_dir)?;

    // 1. Ingest every uploaded file by format:
    //      .unitypackage -> guid/tar layout expanded to Assets/
    //      .zip           -> extracted in place (raw asset zip or existing VPM zip)
    //      anything else  -> copied as a raw file at the package root
    for (fname, fpath) in &files {
        let lower = fname.to_lowercase();
        if lower.ends_with(".unitypackage") {
            extract_unitypackage(fpath, &out_dir).with_context(|| {
                let got = fs::metadata(fpath).map(|m| m.len()).unwrap_or(0);
                format!("'{fname}' is not a valid .unitypackage ({got} bytes)")
            })?;
        } else if lower.ends_with(".zip") {
            extract_zip_file(fpath, &out_dir).with_context(|| format!("'{fname}' is not a valid .zip"))?;
        } else {
            let dest = out_dir.join(fname);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(fpath, &dest)
                .with_context(|| format!("failed to ingest raw file '{fname}'"))?;
        }
    }

    // 2. Optional internal package.json metadata
    let internal = find_internal_pkg_json(&out_dir);

    // 3. Build the VPM zip to a temp path first, so an existing stable version
    //    can be demoted to an RC candidate before its zip filename is reused.
    let dirname = sanitize_dir(&name);
    let cat_dir = cfg.shop.join("VPM").join(&category).join(&dirname);
    fs::create_dir_all(&cat_dir)?;

    let zip_path = cat_dir.join(format!("{dirname}-{version}.zip"));
    let zip_url = format!(
        "{}/{category}/{dirname}/{dirname}-{version}.zip",
        cfg.master_host.trim_end_matches('/')
    );

    let pkg_json = build_pkg_json(&name, &version, &category, &zip_url, internal.as_ref());
    let pkg_json_path = cat_dir.join("package.json");
    fs::write(&pkg_json_path, serde_json::to_string_pretty(&pkg_json)?)?;

    let tmp_zip = work.join(format!("{dirname}-{version}.zip"));
    build_zip(&out_dir, &tmp_zip, &pkg_json)?;

    // 4. Validate the manifest with the real VCC DLL before touching the repo
    let man_val = run_cmd(
        "dotnet",
        &[
            &cfg.validator.to_string_lossy(),
            "--manifest",
            &pkg_json_path.to_string_lossy(),
        ],
    )?;

    // 4b. Demote the existing stable version to an RC candidate (if requested),
    //     BEFORE the new zip claims the stable filename.
    if demote_requested {
        demote_note = if version_exists(cfg, &name, &version)? {
            demote_existing_version(cfg, &name, &version, rc_slot)?
        } else {
            format!(
                "No existing stable version <b>{version}</b> to demote — published as-is.<br>"
            )
        };
    }

    // 4c. Move the validated zip into its final place.
    //     Use move_path so /tmp (scratch) -> shop cross-filesystem works (EXDEV).
    move_path(&tmp_zip, &zip_path)?;
    let zip_sha = sha256_file(&zip_path)?;

    // 5. Update the master repo
    let ver_entry = json!({
        "name": name,
        "displayName": pkg_json["displayName"],
        "version": version,
        "unity": pkg_json["unity"],
        "description": pkg_json["description"],
        "author": pkg_json["author"],
        "url": zip_url,
        "dependencies": pkg_json["dependencies"],
        "vpmDependencies": pkg_json["vpmDependencies"],
        "category": category,
        "zipSHA256": zip_sha,
    });
    update_master_repo(cfg, &name, &version, &ver_entry)?;

    // 6. Regenerate category repos + published master
    let gen = run_regen(&cfg.shop)?;

    // 7. Validate the final master listing with the VCC DLL
    let list_val = run_cmd(
        "dotnet",
        &[
            &cfg.validator.to_string_lossy(),
            &cfg.shop.join("VPM/vpm-repo.json").to_string_lossy(),
        ],
    )?;

    // cleanup — remove ONLY the per-upload scratch dirs, never /tmp itself
    for (_fname, fpath) in &files {
        if let Some(parent) = fpath.parent() {
            if parent != Path::new("/tmp") {
                let _ = fs::remove_dir_all(parent);
            }
        }
    }
    let _ = fs::remove_dir_all(&work);

    let repo_url = format!("{}/index.json", cfg.master_host.trim_end_matches('/'));
    let bump_note = if rc_bumped {
        format!(
            "Version <b>{version}</b> already existed, so this upload was published as <b>rc candidate {version}</b> and the old version was kept intact.<br>"
        )
    } else {
        String::new()
    };
    Ok(format!(
        "{demote_note}{bump_note}Package <b>{name}</b> v{version} uploaded and published.<br>\
         <b>Zip:</b> <a href=\"{zip_url}\">{name}</a><br>\
         <b>Master repo:</b> <a href=\"{repo_url}\">{repo_url}</a><br>\
         <b>zipSHA256:</b> <code>{zip_sha}</code><br>\
         <hr><b>Manifest validation (VCC DLL):</b><br><pre>{man_val}</pre>\
         <b>Repo validation:</b><br><pre>{list_val}</pre>\
         <b>Category regen:</b><br><pre>{gen}</pre>"
    ))
}

// ─── Unitypackage extraction ────────────────────────────────────────────────

fn is_gzip(path: &Path) -> bool {
    let mut buf = [0u8; 2];
    if let Ok(mut f) = fs::File::open(path) {
        if f.read_exact(&mut buf).is_ok() {
            return buf[0] == 0x1f && buf[1] == 0x8b;
        }
    }
    false
}

fn extract_unitypackage(pkg: &Path, out: &Path) -> Result<()> {
    let raw = scratch_dir("extract");
    let file = fs::File::open(pkg)?;
    let reader: Box<dyn Read> = if is_gzip(pkg) {
        Box::new(GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = Archive::new(reader);
    archive
        .unpack(&raw)
        .with_context(|| format!("tar unpack failed for {}", pkg.display()))?;

    for entry in fs::read_dir(&raw)? {
        let guid_dir = entry?.path();
        if !guid_dir.is_dir() {
            continue;
        }
        let pn = guid_dir.join("pathname");
        if !pn.is_file() {
            continue;
        }
        let target = fs::read_to_string(&pn)?.trim().to_string();
        if target.is_empty() {
            continue;
        }
        let dest = out.join(&target);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let asset = guid_dir.join("asset");
        if asset.is_file() {
            fs::copy(&asset, &dest)?;
        }
        let meta = guid_dir.join("asset.meta");
        if meta.is_file() {
            let meta_dest = PathBuf::from(format!("{}.meta", dest.to_string_lossy()));
            fs::copy(&meta, &meta_dest)?;
        }
    }
    let _ = fs::remove_dir_all(&raw);
    Ok(())
}

fn extract_zip_file(pkg: &Path, out: &Path) -> Result<()> {
    let file = fs::File::open(pkg)?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("zip open failed for {}", pkg.display()))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if name.is_empty() || name.split('/').any(|s| s == "..") {
            continue; // skip empty or path-traversal entries
        }
        let dest = out.join(&name);
        if entry.is_dir() {
            fs::create_dir_all(&dest)?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&dest)?;
        io::copy(&mut entry, &mut f)?;
    }
    Ok(())
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_files(&p, out);
            } else {
                out.push(p);
            }
        }
    }
}
fn find_internal_pkg_json(out: &Path) -> Option<Value> {
    let mut all = Vec::new();
    collect_files(out, &mut all);
    let mut found: Vec<(usize, PathBuf)> = all
        .into_iter()
        .filter(|p| p.file_name().map(|n| n == "package.json").unwrap_or(false))
        .map(|p| (p.ancestors().count(), p))
        .collect();
    found.sort_by_key(|(d, _)| *d);
    for (_d, p) in found {
        if let Ok(txt) = fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                if v.get("name").is_some() {
                    return Some(v);
                }
            }
        }
    }
    None
}

// ─── VPM zip building ───────────────────────────────────────────────────────

fn build_pkg_json(name: &str, version: &str, category: &str, url: &str, internal: Option<&Value>) -> Value {
    let mut display_name = name.to_string();
    let mut unity = "2022.3".to_string();
    let mut description = String::new();
    let mut author = json!({"name": env::var("PKG_AUTHOR").unwrap_or_else(|_| "VPM Shop".into())});
    let mut keywords: Value = json!([]);
    let mut dependencies: Value = json!({});
    let mut vpm_dependencies: Value = json!({});
    let mut unity_release = "";
    let mut legacy_packages: Value = json!([]);

    if let Some(meta) = internal {
        if let Some(v) = meta.get("displayName").and_then(|x| x.as_str()) {
            display_name = v.to_string();
        }
        if let Some(v) = meta.get("unity").and_then(|x| x.as_str()) {
            unity = v.to_string();
        }
        if let Some(v) = meta.get("unityRelease").and_then(|x| x.as_str()) {
            unity_release = v;
        }
        if let Some(v) = meta.get("description").and_then(|x| x.as_str()) {
            description = v.to_string();
        }
        if let Some(v) = meta.get("author") {
            author = v.clone();
        }
        if let Some(v) = meta.get("keywords") {
            keywords = v.clone();
        }
        if let Some(v) = meta.get("dependencies") {
            dependencies = v.clone();
        }
        if let Some(v) = meta.get("vpmDependencies") {
            vpm_dependencies = v.clone();
        }
        if let Some(v) = meta.get("legacyPackages") {
            legacy_packages = v.clone();
        }
    }

    let mut m = serde_json::Map::new();
    m.insert("name".into(), json!(name));
    m.insert("displayName".into(), json!(display_name));
    m.insert("version".into(), json!(version));
    m.insert("unity".into(), json!(unity));
    m.insert("description".into(), json!(description));
    m.insert("keywords".into(), keywords);
    m.insert("author".into(), author);
    m.insert("dependencies".into(), dependencies);
    m.insert("vpmDependencies".into(), vpm_dependencies);
    m.insert("category".into(), json!(category));
    m.insert("url".into(), json!(url));
    if !unity_release.is_empty() {
        m.insert("unityRelease".into(), json!(unity_release));
    }
    if legacy_packages.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
        m.insert("legacyPackages".into(), legacy_packages);
    }
    Value::Object(m)
}

fn build_zip(src: &Path, zip_path: &Path, pkg_json: &Value) -> Result<()> {
    let tmp = zip_path.with_extension("tmp.zip");
    let file = fs::File::create(&tmp)?;
    let mut writer = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::from_date_and_time(2024, 1, 1, 0, 0, 0)?);

    writer.start_file("package.json", opts)?;
    writer.write_all(serde_json::to_string_pretty(pkg_json)?.as_bytes())?;

    let mut files = Vec::new();
    collect_files(src, &mut files);
    files.sort();
    for f in files {
        let rel = f.strip_prefix(src)?.to_string_lossy().replace('\\', "/");
        writer.start_file(rel, opts)?;
        let mut fh = fs::File::open(&f)?;
        io::copy(&mut fh, &mut writer)?;
    }
    let _inner = writer.finish()?;
    fs::rename(&tmp, zip_path)?;
    Ok(())
}

// ─── Repo updates ───────────────────────────────────────────────────────────

fn update_master_repo(cfg: &Config, name: &str, version: &str, ver_entry: &Value) -> Result<()> {
    let repo_path = cfg.shop.join("vpm-repo.json");
    let txt = fs::read_to_string(&repo_path)?;
    let mut repo: Value = serde_json::from_str(&txt)?;
    let packages = repo
        .get_mut("packages")
        .ok_or_else(|| anyhow!("master repo has no 'packages'"))?;

    let mut pkg = packages
        .get(name)
        .cloned()
        .unwrap_or_else(|| json!({ "versions": {} }));
    pkg["versions"][version] = ver_entry.clone();
    packages[name] = pkg;

    let tmp = repo_path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&repo)?)?;
    fs::rename(&tmp, &repo_path)?;
    Ok(())
}

// ─── Command helpers ────────────────────────────────────────────────────────

fn run_cmd(prog: &str, args: &[&str]) -> Result<String> {
    run_cmd_in(prog, args, None, None)
}

fn run_regen(shop: &Path) -> Result<String> {
    // The category-repo generator is bundled with this crate (see src/python/).
    // It writes into the shop (cwd + VPM_SHOP env), auto-creating any missing
    // category VPM/ subdirs, and reads its domain/category settings from the
    // gitignored repos.conf (REPOS_CONF), falling back to safe defaults.
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/python/gen_category_repos.py");
    if !script.is_file() {
        bail!(
            "bundled category-repo generator missing: {} — regenerate master repo by hand",
            script.display()
        );
    }
    let conf = Path::new(env!("CARGO_MANIFEST_DIR")).join("repos.conf");
    let mut cmd = Command::new("python3");
    cmd.arg(&script)
        .current_dir(shop)
        .env("VPM_SHOP", shop)
        .env("REPOS_CONF", &conf);
    let out = cmd.output().with_context(|| "failed to run gen_category_repos.py")?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        bail!("gen_category_repos.py exited {}:\n{stdout}\n{stderr}", out.status);
    }
    Ok(format!("{stdout}\n{stderr}").trim().to_string())
}

fn run_cmd_in(
    prog: &str,
    args: &[&str],
    cwd: Option<&Path>,
    env_shop: Option<&Path>,
) -> Result<String> {
    let mut cmd = Command::new(prog);
    cmd.args(args);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    if let Some(shop) = env_shop {
        cmd.env("VPM_SHOP", shop);
    }
    let out = cmd.output().with_context(|| format!("failed to run {prog}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        bail!("{prog} exited {}:\n{stdout}\n{stderr}", out.status);
    }
    Ok(format!("{stdout}\n{stderr}").trim().to_string())
}

// ─── Misc helpers ───────────────────────────────────────────────────────────

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sanitize_name(s: &str) -> String {
    s.trim().to_lowercase().replace(' ', ".")
}

fn sanitize_dir(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn is_valid_pkg_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'_')
        && s
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn is_valid_version(s: &str) -> bool {
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn html_result(ok: bool, body: &str) -> Response {
    let color = if ok { "#1a7f37" } else { "#b35900" };
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    let html = format!(
        "<!doctype html><html><head><meta charset=utf-8><title>Upload result</title></head>\
         <body style=\"font-family:system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem\">\
         <h2 style=\"color:{color}\">{}</h2>{}\
         <hr><p><a href=\"/\">← back</a></p></body></html>",
        if ok { "✅ Upload successful" } else { "❌ Upload failed" },
        body
    );
    (status, Html(html)).into_response()
}

