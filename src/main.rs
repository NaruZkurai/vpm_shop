//! vpm-upload-api — standalone VPM shop upload service.
//!
//! POST /upload: multipart (username, password, name, version, category, file)
//!   - validates creds (default CC / cc)
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

const CATEGORIES: &[&str] = &[
    "Avatars",
    "BetterPB",
    "Shaders",
    "Tools",
    "Props",
    "Animations",
    "Models3D",
    "PointlessAssets",
    "Misc",
];

#[derive(Clone)]
struct Config {
    shop: PathBuf,
    validator: PathBuf,
    username: String,
    password: String,
    master_host: String,
}

impl Config {
    fn from_env() -> Config {
        let shop = PathBuf::from(
            env::var("VPM_SHOP").unwrap_or_else(|_| "/mnt/data/sda2/shop".into()),
        );
        let validator = PathBuf::from(
            env::var("VALIDATOR")
                .unwrap_or_else(|_| "/opt/vpm-upload-api/validator/vpmval.dll".into()),
        );
        Config {
            shop,
            validator,
            username: env::var("API_USER").unwrap_or_else(|_| "CC".into()),
            password: env::var("API_PASS").unwrap_or_else(|_| "cc".into()),
            master_host: env::var("MASTER_HOST")
                .unwrap_or_else(|_| "https://vpm.example.com".into()),
        }
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = env::temp_dir().join(format!("vpm-upload-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&base).ok();
    base
}

// ─── HTTP layer ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cfg = Arc::new(Config::from_env());
    let app = Router::new()
        .route("/", get(index))
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
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024 * 1024)) // 10 GiB ceiling
        .with_state(cfg);

    let addr = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:55555".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("vpm-upload-api listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> &'static str {
    "ok"
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
            _ => {
                let _ = field.bytes().await;
            }
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
    if !CATEGORIES.contains(&category.as_str()) {
        bail!(
            "invalid category '{category}' (choose from: {})",
            CATEGORIES.join(", ")
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
    fs::rename(&tmp_zip, &zip_path)?;
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
    run_cmd_in("python3", &["gen_category_repos.py"], Some(shop), Some(shop))
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

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>VPM Shop Manager</title>
<style>
:root {
  --bg:#0d1117; --bg2:#161b22; --bg3:#21262d; --border:#30363d;
  --text:#e6edf3; --muted:#8b949e; --accent:#58a6ff; --green:#3fb950;
  --red:#f85149; --yellow:#d29922; --mono:ui-monospace,SFMono-Regular,Menlo,monospace;
}
* { box-sizing:border-box; }
body { margin:0; background:var(--bg); color:var(--text); font:15px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif; }
header { display:flex; align-items:center; gap:16px; padding:12px 24px; background:var(--bg2); border-bottom:1px solid var(--border); position:sticky; top:0; z-index:10; }
header h1 { font-size:18px; margin:0; }
header .sub { color:var(--muted); font-size:12px; margin-top:2px; }
.tabs { display:flex; gap:6px; margin-left:auto; }
.tab { padding:6px 14px; border:1px solid var(--border); border-radius:6px; background:var(--bg3); color:var(--muted); cursor:pointer; font-size:13px; }
.tab.active { background:var(--accent); color:#fff; border-color:var(--accent); }
main { width:100%; max-width:none; margin:0 auto; padding:4px 8px; box-sizing:border-box; }
.card { display: flex; flex-direction: column; background:var(--bg2); border:1px solid var(--border); border-radius:8px; padding:4px 8px; margin-bottom:8px; }
.card h2 { margin:0 0 8px; font-size:16px; }
.grid { display:grid; grid-template-columns:1fr 1fr; gap:12px; }
@media(max-width:800px){ .grid{grid-template-columns:1fr;} }
label { display:block; font-size:12px; color:var(--muted); margin-bottom:4px; }
input,select,textarea { width:100%; padding:8px 10px; background:var(--bg); color:var(--text); border:1px solid var(--border); border-radius:6px; font-size:14px; }
input:focus,select:focus,textarea:focus { outline:none; border-color:var(--accent); }
button { padding:8px 16px; background:var(--accent); color:#fff; border:none; border-radius:6px; font-size:14px; cursor:pointer; }
button:hover { filter:brightness(1.15); }
button.danger { background:var(--red); }
button.ghost { background:transparent; border:1px solid var(--border); color:var(--muted); }
#status { display:none; padding:10px 14px; border-radius:6px; margin-bottom:16px; font-size:14px; white-space:pre-wrap; word-break:break-word; }
#status.ok { background:#12261a; border:1px solid #1f6f43; color:var(--green); }
#status.err { background:#2d1617; border:1px solid #8e3a3f; color:var(--red); }
.warn-box { margin-top:12px; padding:10px 14px; border-radius:6px; background:#2d2417; border:1px solid #8e6a3f; color:#f0c98a; font-size:13px; line-height:1.5; }
.warn-box .muted { color:var(--muted); font-size:12px; }
.warn-box code { background:rgba(0,0,0,.35); padding:1px 5px; border-radius:4px; }
.layout { display:grid; grid-template-columns:300px 1fr; gap:20px; }
@media(max-width:900px){ .layout{grid-template-columns:1fr;} }
.pkg-list { flex: 1; overflow-y: auto; }
.pkg-item { padding:6px 8px; border:1px solid var(--border); border-radius:4px; margin-bottom:4px; cursor:pointer; background:var(--bg); }
.pkg-item:hover { border-color:var(--accent); }
.pkg-item.active { border-color:var(--accent); background:var(--bg3); }
.pkg-item .nm { font-weight:600; font-size:13px; }
.pkg-item .meta { font-size:10px; color:var(--muted); margin-top:0; }
.pkg-item .cat { display:inline-block; padding:1px 7px; border-radius:10px; background:var(--bg3); color:var(--accent); font-size:10px; }
.files { border:1px solid var(--border); border-radius:6px; }
.files .frow { padding:6px 12px; font-family:var(--mono); font-size:12px; cursor:pointer; border-bottom:1px solid var(--border); display:flex; justify-content:space-between; gap:10px; }
.files .frow:hover { background:var(--bg3); }
.files .frow.selected { background:#1f3a5f; }
.files .frow.dir { color:var(--muted); cursor:default; }
.files .fsz { color:var(--muted); white-space:nowrap; }
.files .fdir { padding:6px 8px; font-family:var(--mono); font-size:12px; cursor:pointer; border-bottom:1px solid var(--border); display:flex; flex-wrap:wrap; align-items:center; gap:4px 8px; }
.files .fdir:hover { background:var(--bg3); }
.files .fdir .caret { font-size:9px; color:var(--muted); width:12px; display:inline-block; }
.files .tree-sub { flex-basis:100%; margin-left:6px; padding-left:8px; border-left:1px dashed var(--border); }
#viewer { margin-top:14px; }
#viewer pre { background:#010409; border:1px solid var(--border); border-radius:6px; padding:8px; overflow:auto; font-family:var(--mono); font-size:12px; max-height:50vh; text-align:left; }
#viewer .fhead { font-family:var(--mono); font-size:12px; color:var(--muted); margin-bottom:6px; }
.hidden { display:none !important; }
.pkg-detail-head { display:flex; align-items:center; gap:8px; margin-bottom:6px; flex-wrap:wrap; }
.pkg-detail-head h3 { margin:0; font-size:17px; word-break:break-all; }
.badge { font-size:11px; padding:2px 8px; border-radius:10px; background:var(--bg3); color:var(--muted); }
.version-list { border:1px solid var(--border); border-radius:6px; margin-bottom:14px; overflow:hidden; }
.ver-row { border-bottom:1px solid var(--border); }
.ver-row:last-child { border-bottom:none; }
.ver-row-btn { width:100%; display:flex; align-items:center; gap:10px; padding:8px 12px; background:transparent; border:none; color:var(--text); font-family:var(--mono); font-size:13px; text-align:left; cursor:pointer; border-radius:0; }
.ver-row-btn:hover { background:var(--bg3); }
.ver-row.active > .ver-row-btn { background:var(--bg3); color:var(--accent); font-weight:600; }
.ver-row .caret { width:12px; color:var(--muted); }
.ver-row.active .caret { color:var(--accent); }
.ver-body { padding:4px 12px 12px 34px; border-top:1px solid var(--border); background:var(--bg); }
.pkg-json { border:1px solid var(--border); border-radius:6px; margin-bottom:12px; overflow:hidden; background:var(--bg); }
.pkg-json-head { display:flex; align-items:center; gap:8px; padding:7px 12px; cursor:pointer; font-size:12px; color:var(--muted); background:var(--bg3); font-family:var(--mono); }
.pkg-json-head:hover { color:var(--text); }
.pkg-json-caret { width:12px; text-align:center; }
.pkg-json-content { margin:0; padding:4px 8px; font-family:var(--mono); font-size:10px; line-height:1.3; max-height:24vh; overflow:auto; white-space:pre; text-align:left; }
.deps-toggle { padding:2px 8px; font-size:12px; }
.toolbar { display:flex; gap:8px; align-items:center; margin-bottom:12px; }
.toolbar input { flex:1; }
.modal { position:fixed; inset:0; z-index:100; display:flex; align-items:center; justify-content:center; }
.modal-back { position:absolute; inset:0; background:rgba(0,0,0,.65); }
.modal-box { position:relative; background:var(--bg2); border:1px solid var(--border); border-radius:10px; width:min(660px,94vw); max-height:90vh; overflow-y:auto; padding:22px; box-shadow:0 24px 70px rgba(0,0,0,.55); }
.modal-head { display:flex; align-items:center; gap:10px; margin-bottom:14px; }
.modal-head h2 { margin:0; font-size:17px; }
.add-ver { padding:2px 9px; font-size:12px; margin-left:8px; }
.deps-card { padding:8px; margin-bottom:14px; }
.deps-head { display:flex; align-items:center; gap:8px; margin-bottom:8px; }
.deps-title { font-size:13px; font-weight:600; }
.deps-count { font-size:11px; background:var(--bg3); border-radius:10px; padding:1px 8px; color:var(--muted); }
.dep-head-row, .dep-row { display:grid; grid-template-columns:130px 1.5fr auto 1fr 30px; gap:6px; align-items:center; }
.dep-head-row { font-size:10px; color:var(--muted); text-transform:uppercase; letter-spacing:.06em; margin-bottom:4px; }
.dep-row { margin-bottom:4px; }
.dep-row input, .dep-row select { padding:4px 8px; font-size:12px; font-family:var(--mono); }
.dep-arrow { color:var(--muted); font-family:var(--mono); }
.dep-del { padding:2px 8px; font-size:12px; }
.cl-item { display:grid; grid-template-columns:auto 1fr auto; gap:8px; align-items:center; padding:7px 10px; border:1px solid var(--border); border-radius:6px; margin-bottom:6px; background:var(--bg); }
.cl-item input[type=checkbox] { width:auto; }
.cl-item .cl-text { font-size:13px; padding:4px 8px; }
.cl-item.done .cl-text { text-decoration:line-through; color:var(--muted); }
.cl-del { padding:2px 8px; font-size:12px; }
#cl-items { margin-top:8px; }
.empty { color:var(--muted); text-align:center; padding:40px 0; }
a { color:var(--accent); text-decoration:none; }
</style>
</head>
<body>
<header>
  <div>
    <h1>🛒 VPM Shop Manager</h1>
    <div class="sub"> <button class="ghost" id="btn-nav-upload" onclick="openUploadModal()">upload</button> · <button class="ghost" id="btn-nav-browse" onclick="loadPackages()">browse</button> · <button class="ghost" id="btn-nav-registry" onclick="showRegistry()">inspect registry</button> · <button class="ghost" id="btn-nav-vault" onclick="openVault()">vault</button> </div>
  </div>
  <div class="tabs">
    <button class="tab active" data-tab="browse">Browse</button>
    <button class="tab" data-tab="upload">Upload</button>
    <button class="tab" data-tab="checklist">Checklist</button>
  </div>
</header>
<main>
  <div id="status"></div>

  <!-- BROWSE TAB -->
  <section id="tab-browse">
    <div class="layout">
      <div class="card">
        <h2>Packages</h2>
        <div class="toolbar">
          <input id="pkg-search" type="search" name="pkg_search" placeholder="Filter packages…" autocomplete="off" autocorrect="off" autocapitalize="off" spellcheck="false" readonly onfocus="this.removeAttribute('readonly')">
        </div>
        <div class="pkg-list" id="pkg-list"></div>
      </div>
      <div class="card">
        <div id="pkg-detail" class="empty">Select a package on the left to inspect it.</div>
      </div>
    </div>
  </section>

  <!-- CHECKLIST TAB -->
  <section id="tab-checklist" class="hidden">
    <div class="card" style="max-width:640px">
      <h2>📋 Upload Checklist</h2>
      <div id="checklist-lock">
        <p class="muted" style="margin:0 0 10px">Enter the password to view your private to-upload list.</p>
        <div class="toolbar">
          <input id="cl-password" type="password" placeholder="password" autocomplete="off">
          <button id="cl-unlock">Unlock</button>
        </div>
        <div id="cl-lock-err" class="warn-box hidden"></div>
      </div>
      <div id="checklist-body" class="hidden">
        <p class="muted" style="margin:0 0 4px">Things to upload — check them off as you go.</p>
        <div id="cl-items"></div>
        <div class="toolbar" style="margin-top:10px">
          <input id="cl-new" type="text" placeholder="Add something to upload…" autocomplete="off">
          <button class="ghost" id="cl-add">+ Add</button>
        </div>
        <div style="margin-top:12px;display:flex;gap:8px;align-items:center">
          <button id="cl-save">💾 Save</button>
          <button class="ghost" id="cl-clear-done">Clear done</button>
          <span class="muted" id="cl-saved-hint" style="font-size:11px"></span>
          <button class="ghost" id="cl-lock" style="margin-left:auto">🔒 Lock</button>
        </div>
      </div>
    </div>
  </section>

  <!-- UPLOAD MODAL -->
  <div id="upload-modal" class="modal hidden">
    <div class="modal-back" onclick="closeUploadModal()"></div>
    <div class="modal-box">
      <div class="modal-head">
        <h2>Upload package files</h2>
        <button type="button" class="ghost" onclick="closeUploadModal()" style="margin-left:auto">✕</button>
      </div>
      <form id="upload-form" enctype="multipart/form-data">
        <div class="grid">
          <div>
            <label>Username</label>
            <input name="username" placeholder="CC" autocomplete="username" required>
          </div>
          <div>
            <label>Password</label>
            <input name="password" type="password" placeholder="••••••" autocomplete="current-password" required>
          </div>
          <div>
            <label>Package ID <span style="color:var(--muted)">(pick existing or type new)</span></label>
            <input name="name" id="up-name" list="pkg-ids" placeholder="com.author.package" required autocomplete="off">
            <datalist id="pkg-ids"></datalist>
          </div>
          <div>
            <label>Version <span style="color:var(--muted)">(semver)</span></label>
            <input name="version" id="up-version" list="pkg-versions" placeholder="1.0.0" required autocomplete="off">
            <datalist id="pkg-versions"></datalist>
          </div>
          <div>
            <label>Category</label>
            <select name="category" id="up-category">
              <option>Avatars</option><option>BetterPB</option><option>Shaders</option>
              <option>Tools</option><option>Props</option><option>Animations</option>
              <option>Models3D</option><option>PointlessAssets</option><option selected>Misc</option>
            </select>
          </div>
          <div>
            <label>Package file(s) <span style="color:var(--muted)">(unitypackage · zip · raw files)</span></label>
            <input type="file" name="file" accept=".unitypackage,.zip,.blend,.fbx,.obj,.png,.jpg,.jpeg,.gif,.mp3,.wav,.json,.txt,.unity,.prefab,.mat,.asset,.anim,.controller,.bundle" multiple required>
          </div>
        </div>
        <div id="up-warn" class="warn-box hidden"></div>
        <p style="margin-top:16px"><button type="submit">⬆ Upload files &amp; publish</button></p>
      </form>
    </div>
  </div>
</main>
<script>
const repoUrl = 'https://vpm.example.com';
const vaultUrl = 'https://vault.example.com';
const $ = s => document.querySelector(s);
let PACKAGES = {};
let currentPkg = null, currentVer = null;

function showStatus(msg, ok) {
  const s = $('#status');
  s.textContent = msg;
  s.className = ok ? 'ok' : 'err';
  s.style.display = 'block';
}
function fmtSize(n) {
  if (n < 1024) return n + ' B';
  if (n < 1048576) return (n/1024).toFixed(1) + ' KB';
  return (n/1048576).toFixed(1) + ' MB';
}
function esc(s) { return String(s).replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }

// ── tabs ──
document.querySelectorAll('.tab').forEach(t => t.addEventListener('click', () => {
  document.querySelectorAll('.tab').forEach(x => x.classList.remove('active'));
  t.classList.add('active');
  $('#tab-browse').classList.toggle('hidden', t.dataset.tab !== 'browse');
  $('#tab-checklist').classList.toggle('hidden', t.dataset.tab !== 'checklist');
  if (t.dataset.tab === 'upload') openUploadModal();
  if (t.dataset.tab === 'checklist') {
    if (CL.pass) loadChecklist();
    else $('#cl-password').focus();
  }
}));

// ── private checklist ──
const CL = { pass: sessionStorage.getItem('clpass') || '', items: [] };
async function loadChecklist() {
  if (!CL.pass) return lockChecklist();
  try {
    const r = await fetch('/api/checklist', { headers: { 'X-Api-Password': CL.pass } });
    if (r.status === 401) return lockChecklist();
    if (!r.ok) return showStatus('Failed to load checklist', false);
    CL.items = (await r.json()).items || [];
    $('#checklist-lock').classList.add('hidden');
    $('#checklist-body').classList.remove('hidden');
    renderChecklist();
  } catch { showStatus('Network error loading checklist', false); }
}
function unlockChecklist() {
  const pass = $('#cl-password').value;
  fetch('/api/checklist', { headers: { 'X-Api-Password': pass } }).then(async r => {
    if (r.status === 401) {
      const box = $('#cl-lock-err'); box.classList.remove('hidden'); box.textContent = 'Wrong password.';
      return;
    }
    if (!r.ok) {
      const box = $('#cl-lock-err'); box.classList.remove('hidden'); box.textContent = 'Error loading checklist.';
      return;
    }
    CL.pass = pass;
    sessionStorage.setItem('clpass', pass);
    CL.items = (await r.json()).items || [];
    $('#cl-lock-err').classList.add('hidden');
    $('#checklist-lock').classList.add('hidden');
    $('#checklist-body').classList.remove('hidden');
    renderChecklist();
  });
}
function lockChecklist() {
  CL.pass = ''; sessionStorage.removeItem('clpass');
  $('#checklist-body').classList.add('hidden');
  $('#checklist-lock').classList.remove('hidden');
  $('#cl-lock-err').classList.add('hidden');
  $('#cl-password').value = '';
}
function renderChecklist() {
  const box = $('#cl-items');
  if (!CL.items.length) {
    box.innerHTML = '<div class="empty" style="padding:12px">Nothing to upload yet — add something below.</div>';
    return;
  }
  box.innerHTML = CL.items.map((it, i) => `
    <div class="cl-item${it.done ? ' done' : ''}">
      <input type="checkbox" class="cl-check" ${it.done ? 'checked' : ''} data-i="${i}">
      <input class="cl-text" value="${esc(it.text)}" data-i="${i}" autocomplete="off">
      <button class="ghost cl-del" data-i="${i}" title="remove">✕</button>
    </div>`).join('');
  box.querySelectorAll('.cl-check').forEach(c => c.onchange = () => {
    CL.items[+c.dataset.i].done = c.checked; renderChecklist();
  });
  box.querySelectorAll('.cl-text').forEach(t => t.oninput = () => {
    CL.items[+t.dataset.i].text = t.value;
  });
  box.querySelectorAll('.cl-del').forEach(b => b.onclick = () => {
    CL.items.splice(+b.dataset.i, 1); renderChecklist();
  });
}
function saveChecklist() {
  const hint = $('#cl-saved-hint'); hint.textContent = 'saving…';
  fetch('/api/checklist', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-Api-Password': CL.pass },
    body: JSON.stringify({ items: CL.items }),
  }).then(async r => {
    const j = await r.json();
    hint.textContent = j.ok ? 'saved ✓' : (j.error || 'save failed');
    if (!j.ok) showStatus(j.error || 'save failed', false);
    else setTimeout(() => hint.textContent = '', 2000);
  }).catch(() => { hint.textContent = 'error'; showStatus('Network error saving checklist', false); });
}
$('#cl-unlock').onclick = unlockChecklist;
$('#cl-password').addEventListener('keydown', e => { if (e.key === 'Enter') unlockChecklist(); });
$('#cl-add').onclick = () => {
  const v = $('#cl-new').value.trim();
  if (!v) return;
  CL.items.push({ text: v, done: false });
  $('#cl-new').value = ''; renderChecklist(); $('#cl-new').focus();
};
$('#cl-new').addEventListener('keydown', e => { if (e.key === 'Enter') $('#cl-add').click(); });
$('#cl-save').onclick = saveChecklist;
$('#cl-clear-done').onclick = () => { CL.items = CL.items.filter(i => !i.done); renderChecklist(); };
$('#cl-lock').onclick = lockChecklist;

// ── upload modal ──
function openUploadModal(pkgName) {
  $('#upload-modal').classList.remove('hidden');
  if (pkgName) {
    $('#up-name').value = pkgName;
    $('#up-version').value = '';
    refreshUploadMeta();
    $('#up-version').focus();
  } else {
    $('#up-name').focus();
  }
}
function closeUploadModal() {
  $('#upload-modal').classList.add('hidden');
}

// ── browse ──
async function loadPackages() {
  const r = await fetch('/api/packages');
  if (!r.ok) { showStatus('Failed to load packages', false); return; }
  PACKAGES = await r.json();
  renderList();
  // package-id datalist for the upload form
  const ids = $('#pkg-ids');
  ids.innerHTML = '';
  for (const nm of Object.keys(PACKAGES.packages || {}).sort()) {
    const o = document.createElement('option'); o.value = nm; ids.appendChild(o);
  }
  refreshUploadMeta();
}
function renderList() {
  const q = ($('#pkg-search').value || '').toLowerCase();
  const list = $('#pkg-list');
  list.innerHTML = '';
  const names = Object.keys(PACKAGES.packages || {}).sort();
  let count = 0;
  for (const nm of names) {
    if (q && !nm.toLowerCase().includes(q)) continue;
    const pkg = PACKAGES.packages[nm];
    const vers = Object.keys(pkg.versions || {}).sort();
    const v = pkg.versions[vers[vers.length-1]];
    const cat = v && v.category ? v.category : 'Misc';
    const div = document.createElement('div');
    div.className = 'pkg-item' + (nm === currentPkg ? ' active' : '');
    div.innerHTML = `<div class="nm"><button class="ghost add-ver" title="Add a new version" type="button">+ ver</button> ${esc(nm)}</div>
      <div class="meta"><span class="cat">${esc(cat)}</span> · ${vers.length} version${vers.length>1?'s':''} · latest ${esc(vers[vers.length-1]||'')}</div>`;
    div.onclick = () => selectPackage(nm);
    div.querySelector('.add-ver').onclick = (e) => { e.stopPropagation(); openUploadModal(nm); };
    list.appendChild(div);
    count++;
  }
  if (!count) list.innerHTML = '<div class="empty">No packages match.</div>';
}
async function selectPackage(nm) {
  currentPkg = nm;
  currentVer = null;
  renderList();
  const pkg = PACKAGES.packages[nm];
  const vers = Object.keys(pkg.versions || {}).sort();
  if (!vers.length) { $('#pkg-detail').innerHTML = '<div class="empty">No versions.</div>'; return; }
  selectVersion(vers[vers.length-1]);
}
async function selectVersion(ver) {
  currentVer = ver;
  const pkg = PACKAGES.packages[currentPkg];
  const vers = Object.keys(pkg.versions || {}).sort();
  const v = pkg.versions[ver] || {};
  const cat = v.category || 'Misc';
  const d = $('#pkg-detail');
  const rows = vers.map(x => `
    <div class="ver-row ${x===ver?'active':''}" data-ver="${esc(x)}">
      <button class="ver-row-btn" type="button">
        <span class="caret">${x===ver?'▼':'▶'}</span><span class="ver-nm">${esc(x)}</span>
      </button>
      ${x===ver ? `<div class="ver-body">
        <div id="deps-panel"></div>
        <div id="file-list"><div class="empty">Loading files…</div></div>
        <div id="viewer" class="hidden"><div class="fhead" id="viewer-head"></div><pre id="viewer-body"></pre></div>
      </div>` : ''}
    </div>`).join('');
  d.innerHTML = `
    <div class="pkg-detail-head">
      <button class="ghost" id="btn-addver">+ Add version</button>
      <h3>${esc(currentPkg)}</h3>
      <span class="badge">${esc(cat)}</span>
      <button class="danger" id="btn-del" style="margin-left:auto">Delete v${esc(ver)}</button>
    </div>
    <div class="pkg-json" id="pkg-json-block">
      <div class="pkg-json-head"><span class="pkg-json-caret">▾</span>package.json</div>
      <pre class="pkg-json-content" id="pkg-json-content">loading…</pre>
    </div>
    <div class="version-list">${rows}</div>`;
  d.querySelectorAll('.ver-row-btn').forEach(btn => {
    btn.onclick = () => selectVersion(btn.closest('.ver-row').dataset.ver);
  });
  $('#pkg-json-block .pkg-json-head').onclick = () => {
    const body = $('#pkg-json-content');
    const hidden = body.classList.toggle('hidden');
    $('#pkg-json-block .pkg-json-caret').textContent = hidden ? '▸' : '▾';
  };
  $('#btn-del').onclick = async () => {
    if (!confirm(`Delete ${currentPkg} v${ver}? This removes the version from the registry.`)) return;
    const r = await fetch(`/api/delete/${encodeURIComponent(currentPkg)}/${encodeURIComponent(ver)}`);
    const j = await r.json();
    if (j.ok) { showStatus(j.message, true); await loadPackages(); selectPackage(currentPkg); }
    else showStatus(j.error || 'delete failed', false);
  };
  $('#btn-addver').onclick = () => openUploadModal(currentPkg);
  fetch(`/api/package/${encodeURIComponent(currentPkg)}/${encodeURIComponent(ver)}/json`)
    .then(r => r.json())
    .then(j => { $('#pkg-json-content').textContent = JSON.stringify(j, null, 2); })
    .catch(() => { $('#pkg-json-content').textContent = 'failed to load package.json'; });
  const r = await fetch(`/api/package/${encodeURIComponent(currentPkg)}/${encodeURIComponent(ver)}/files`);
  const j = await r.json();
  if (!r.ok || !j.files) { $('#file-list').innerHTML = `<div class="empty">${esc(j.error||'error')}</div>`; return; }
  renderFileTree(j.files);
  loadDeps(ver);
}

// ── xplore-style folder view ──
function renderFileTree(files) {
  const container = $('#file-list');
  container.innerHTML = '';
  const tree = {};
  for (const f of files) {
    const parts = f.name.split('/').filter(Boolean);
    let node = tree;
    for (let i = 0; i < parts.length - 1; i++) {
      node[parts[i]] = node[parts[i]] || {};
      node = node[parts[i]];
    }
    node[parts[parts.length - 1]] = { file: f };
  }
  const rootEl = document.createElement('div');
  rootEl.className = 'files tree';
  let build = (obj, depth, parent, rel_path) => {
    for (const k of Object.keys(obj).sort()) {
      const v = obj[k];
      const row = document.createElement('div');
      row.style.paddingLeft = (depth * 16 + 8) + 'px';
      if (v && v.file) {
        const f = v.file;
        row.className = 'frow';
        row.dataset.path = f.name;
        const left = document.createElement('span');
        left.textContent = f.name.split('/').pop();
        const size = document.createElement('span');
        size.className = 'fsz'; size.textContent = fmtSize(f.size);
        row.append(left, size);
        row.onclick = (e) => { e.stopPropagation(); viewFile(f.name, f.size); };
        parent.appendChild(row);
      } else {
        const folderPath = rel_path ? rel_path + '/' + k : k;
        row.className = 'fdir';
        row.dataset.path = folderPath;
        row.innerHTML = `<span class="caret">▶</span><span>📁 ${esc(k)}</span><button class="convert-dep" title="Convert folder to a VPM dependency and remove it from the package" style="margin-left:4px; padding:0 4px; font-size:10px">+<span class="muted" style="font-size:8px">dep</span></button>`;
        const sub = document.createElement('div');
        sub.className = 'tree-sub hidden';
        build(v, depth + 1, sub, folderPath);
        row.appendChild(sub);
        row.onclick = (e) => {
          e.stopPropagation();
          const hidden = sub.classList.toggle('hidden');
          row.querySelector('.caret').textContent = hidden ? '▶' : '▼';
        };
        row.querySelector('.convert-dep').onclick = async (e) => {
          e.stopPropagation();
          const full = row.dataset.path;
          const folderName = full.split('/').pop();
          if (!confirm(`Convert folder '${folderName}' to a VPM dependency and remove it from ${currentPkg} v${currentVer}?`)) return;
          const rr = await fetch(`/api/package/${encodeURIComponent(currentPkg)}/${encodeURIComponent(currentVer)}/convert-to-dep?path=${encodeURIComponent(full)}`);
          const jj = await rr.json();
          if (jj.ok) { showStatus(jj.message, true); selectVersion(currentVer); }
          else showStatus(jj.error || 'convert failed', false);
        };
        parent.appendChild(row);
      }
    }
  };
  build(tree, 0, rootEl, "");
  container.appendChild(rootEl);
}
async function viewFile(path, size) {
  const fl = $('#file-list');
  fl.querySelectorAll('.frow').forEach(x => x.classList.remove('selected'));
  fl.querySelectorAll('.frow').forEach(row => {
    if (row.dataset.path === path) row.classList.add('selected');
  });
  const rr = await fetch(`/api/package/${encodeURIComponent(currentPkg)}/${encodeURIComponent(currentVer)}/file?path=${encodeURIComponent(path)}`);
  const jj = await rr.json();
  $('#viewer').classList.remove('hidden');
  $('#viewer-head').textContent = `${path} · ${fmtSize(jj.size)}`;
  $('#viewer-body').textContent = jj.content;
}

// ── dependencies editor ──
async function loadDeps(ver) {
  const panel = $('#deps-panel');
  const r = await fetch(`/api/package/${encodeURIComponent(currentPkg)}/${encodeURIComponent(ver)}/deps`);
  const j = await r.json();
  if (!r.ok) { panel.innerHTML = `<div class="empty">${esc(j.error||'error')}</div>`; return; }
  renderDeps(ver, j.dependencies || {}, j.vpmDependencies || {});
}
function depsRows(obj) {
  return Object.entries(obj || {}).map(([k, v]) => ({ k, v: String(v) }));
}
function renderDeps(ver, deps, vpmDeps) {
  const panel = $('#deps-panel');
  const rows = [
    ...depsRows(deps).map(r => ({ sec: 'dependencies', k: r.k, v: r.v })),
    ...depsRows(vpmDeps).map(r => ({ sec: 'vpmDependencies', k: r.k, v: r.v })),
  ];
  panel.innerHTML = `<div class="card deps-card">
    <div class="deps-head" id="deps-head" title="toggle">
      <button class="ghost deps-toggle" id="deps-toggle">▶</button>
      <span class="deps-title">Dependencies</span>
      <span class="deps-count">${rows.length}</span>
      <span class="muted" style="font-size:11px">deps · vpmDeps</span>
      <button class="ghost" id="deps-save" style="margin-left:auto">💾 Save</button>
    </div>
    <div id="deps-rows" class="hidden">
      <div class="dep-head-row"><span>type</span><span>package</span><span></span><span>version</span><span></span></div>
      ${rows.length ? rows.map(r => rowHtml(r.sec, r.k, r.v)).join('')
        : `<div class="empty" style="padding:6px">No dependencies.</div>`}
      <div class="grid" style="grid-template-columns:1fr 1fr; gap:12px">
      <button class="ghost add-row" style="width:100%">+ add by ID</button>
      <button class="ghost add-row-dir" style="width:100%">+ add from dir</button>
    </div>
    <input type="file" id="deps-dir-input" style="display:none" webkitdirectory>
    </div>
  </div>`;
  const toggle = () => {
    const box = $('#deps-rows');
    const hidden = box.classList.toggle('hidden');
    $('#deps-toggle').textContent = hidden ? '▶' : '▼';
  };
  $('#deps-head').onclick = toggle;
  $('#deps-toggle').onclick = (e) => { e.stopPropagation(); toggle(); };
  panel.querySelector('.add-row').onclick = () => {
    const box = $('#deps-rows');
    const empty = box.querySelector('.empty');
    if (empty) empty.remove();
    box.insertAdjacentHTML('beforeend', rowHtml('dependencies', '', ''));
    box.querySelector('.dep-row:last-child .dep-k').focus();
  };
  panel.querySelector('.add-row-dir').onclick = (e) => {
    e.stopPropagation();
    $('#deps-dir-input').click();
  };
  $('#deps-dir-input').onchange = (e) => {
    const files = e.target.files || [];
    if (!files.length) return;
    // use the folder name (first path segment) as the dependency id
    const folder = files[0].webkitRelativePath.split('/')[0];
    const box = $('#deps-rows');
    const empty = box.querySelector('.empty');
    if (empty) empty.remove();
    box.insertAdjacentHTML('beforeend', rowHtml('vpmDependencies', folder || '', '1.0.0'));
    e.target.value = '';
  };
  $('#deps-save').onclick = async (e) => {
    e.stopPropagation();
    const out = { dependencies: {}, vpmDependencies: {} };
    $('#deps-rows').querySelectorAll('.dep-row').forEach(row => {
      const sec = row.querySelector('.dep-sec').value;
      const k = row.querySelector('.dep-k').value.trim();
      const v = row.querySelector('.dep-v').value.trim();
      if (k) out[sec][k] = v;
    });
    const payload = JSON.stringify(out);
    const r = await fetch(`/api/package/${encodeURIComponent(currentPkg)}/${encodeURIComponent(ver)}/deps`, {
      method: 'POST', headers: {'Content-Type': 'application/json'}, body: payload,
    });
    const j = await r.json();
    showStatus(j.ok ? j.message : (j.error || 'save failed'), j.ok);
    if (j.ok) { await loadDeps(ver); }
  };
}
function rowHtml(sec, k, v) {
  return `<div class="dep-row">
    <select class="dep-sec">${sec === 'vpmDependencies'
      ? '<option value="dependencies">dependencies</option><option value="vpmDependencies" selected>vpmDependencies</option>'
      : '<option value="dependencies" selected>dependencies</option><option value="vpmDependencies">vpmDependencies</option>'}</select>
    <input class="dep-k" value="${esc(k)}" placeholder="com.package.id" spellcheck="false" title="Repo: ${esc(k)}">
    <span class="dep-arrow">→</span>
    <input class="dep-v" value="${esc(v)}" placeholder="1.0.0 or ^1.2.3">
    <button class="ghost dep-del" title="remove">✕</button>
  </div>`;
}
document.addEventListener('click', (e) => {
  if (e.target.classList && e.target.classList.contains('dep-del')) {
    e.target.closest('.dep-row').remove();
  }
});

// ── upload: existing id/version helpers ──
function pkgInfo() {
  const nm = $('#up-name').value.trim();
  return (PACKAGES.packages || {})[nm];
}
function latestVersion(info) {
  const vs = info ? Object.keys(info.versions || {}) : [];
  return vs.length ? vs.slice().sort().at(-1) : null;
}
function refreshUploadMeta() {
  const info = pkgInfo();
  // autofill category from existing package
  const infoCat = info && Object.values(info.versions || {}).length
    ? Object.values(info.versions)[0].category : null;
  if (infoCat && $('#up-category').value !== infoCat) $('#up-category').value = infoCat;
  // version datalist for this package
  const vl = $('#pkg-versions');
  vl.innerHTML = '';
  if (info) {
    for (const v of Object.keys(info.versions || {}).sort()) {
      const o = document.createElement('option'); o.value = v; vl.appendChild(o);
    }
  }
  checkVersionConflict();
}
function checkVersionConflict() {
  const nm = $('#up-name').value.trim();
  const ver = $('#up-version').value.trim();
  const warn = $('#up-warn');
  const info = pkgInfo();
  const existing = info && ver ? info.versions[ver] : null;
  if (!info || !existing) { warn.classList.add('hidden'); warn.innerHTML = ''; return; }
  const latest = latestVersion(info);
  const prefix = ver + '-rc.';
  const rcs = Object.keys(info.versions || {}).filter(v => v.startsWith(prefix)).sort();
  let slots = '';
  if (rcs.length) {
    slots = `<label style="margin-top:8px">Insert into slot
      <select name="rc_slot" id="rc-slot" style="width:auto">
        <option value="">next free slot</option>`;
    for (const r of rcs) {
      const n = r.slice(prefix.length);
      slots += `<option value="${n}">rc.${n} (currently ${r})</option>`;
    }
    slots += `</select></label>`;
  }
  const autoRc = `${ver}-rc.${rcs.length + 1}`;
  warn.innerHTML =
    `<div>⚠ <b>Version ${ver} already exists</b>` +
    (latest && latest !== ver ? ` — latest version is <b>${latest}</b>.` : '.') + `</div>` +
    `<label style="display:flex;align-items:center;gap:8px;margin-top:8px">
      <input type="checkbox" name="demote" id="up-demote" value="1" style="width:auto">
      Demote the existing <b>${ver}</b> to an RC candidate, so this upload becomes the new <b>${ver}</b>
    </label>` +
    `<div id="rc-slot-wrap" class="hidden" style="margin-left:26px">` +
    (slots || '<div class="muted" style="margin-top:6px">No existing rc candidates — the old version becomes <code>rc.1</code>.</div>') +
    `<div class="muted" style="margin-top:4px">Existing rc candidates at/after the chosen slot are upgraded by 1.</div></div>` +
    `<div class="muted" style="margin-top:8px">If left unchecked, your upload is published as <code>${autoRc}</code> and the existing <b>${ver}</b> is kept intact.</div>`;
  warn.classList.remove('hidden');
  $('#up-demote').addEventListener('change', () => {
    $('#rc-slot-wrap').classList.toggle('hidden', !$('#up-demote').checked);
  });
}
$('#up-name').addEventListener('input', refreshUploadMeta);
$('#up-version').addEventListener('input', checkVersionConflict);

// ── upload ──
$('#upload-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const fd = new FormData(e.target);
  const btn = e.target.querySelector('button');
  btn.disabled = true; btn.textContent = 'Uploading…';
  showStatus('Uploading and publishing…', true);
  try {
    const r = await fetch('/upload', { method: 'POST', body: fd });
    const html = await r.text();
    // response is an HTML page; extract status + body text
    const m = html.match(/<h2[^>]*>(.*?)<\/h2>/);
    const body = html.replace(/<[^>]+>/g, ' ').replace(/&amp;/g,'&').replace(/&lt;/g,'<').replace(/&gt;/g,'>').replace(/&quot;/g,'"').replace(/&#x27;/g,"'").replace(/<br>/g,'\n').trim();
    const ok = (m && m[1].includes('successful')) || html.includes('Upload successful');
    showStatus(body, ok);
    if (ok) { loadPackages(); closeUploadModal(); }
  } catch (err) {
    showStatus('Network error: ' + err, false);
  }
  btn.disabled = false; btn.textContent = '⬆ Upload & publish';
});

$('#pkg-search').addEventListener('input', renderList);

loadPackages();

function showRegistry() {
  window.open(repoUrl, '_blank');
}
function openVault() {
  window.open(vaultUrl, '_blank');
}
</script>
</body></html>"#;
