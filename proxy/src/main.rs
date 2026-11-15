use axum::{
    body::Body,
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose, Engine as _};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{error, info};

#[derive(Clone)]
struct Config {
    repo: String,
    registry: String,
    port: u16,
    listen_addr: String,
    github_token: String,
    index_ttl: u64,
    index_file: PathBuf,
    upstream_caches: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct CacheEntry {
    narinfo: String,
    nar_digest: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct CacheIndexData {
    entries: HashMap<String, CacheEntry>,
    public_key: Option<String>,
}

struct AppState {
    config: Config,
    index: RwLock<Arc<CacheIndexData>>,
    client: reqwest::Client,
    token: RwLock<Option<(String, Instant)>>,
}

impl AppState {
    fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            index: RwLock::new(Arc::new(CacheIndexData::default())),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap(),
            token: RwLock::new(None),
        })
    }

    async fn get_oci_token(&self) -> String {
        if let Some((token, time)) = &*self.token.read().await {
            if time.elapsed() < Duration::from_secs(240) {
                return token.clone();
            }
        }

        let mut token_guard = self.token.write().await;
        if let Some((token, time)) = &*token_guard {
            if time.elapsed() < Duration::from_secs(240) {
                return token.clone();
            }
        }

        let scope = format!("repository:{}/nix-cache:pull", self.config.repo);
        let url = format!(
            "https://{}/token?scope={}&service={}",
            self.config.registry, scope, self.config.registry
        );

        let mut req = self.client.get(&url);
        if !self.config.github_token.is_empty() {
            let creds = general_purpose::STANDARD.encode(format!("token:{}", self.config.github_token));
            req = req.header(header::AUTHORIZATION, format!("Basic {}", creds));
        }

        let mut token = String::new();
        if let Ok(resp) = req.send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(t) = json.get("token").and_then(|v| v.as_str()) {
                    token = t.to_string();
                }
            }
        }

        if token.is_empty() {
            token = self.config.github_token.clone();
        }

        *token_guard = Some((token.clone(), Instant::now()));
        token
    }

    async fn cleanup_leftovers(&self) {
        let Some(dir) = self.config.index_file.parent() else { return };
        let Ok(mut entries) = tokio::fs::read_dir(dir).await else { return };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("cache-index.json.tmp") {
                if tokio::fs::remove_file(entry.path()).await.is_ok() {
                    info!("Removed leftover: {}", name);
                }
            }
        }
    }

    async fn load_index_from_disk(&self) {
        let path = &self.config.index_file;
        let data = match tokio::fs::read(path).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                error!("Failed to read {}: {}", path.display(), e);
                return;
            }
        };

        match serde_json::from_slice::<CacheIndexData>(&data) {
            Ok(parsed) => {
                let count = parsed.entries.len();
                *self.index.write().await = Arc::new(parsed);
                info!("Loaded {} entries from disk ({})", count, path.display());
            }
            Err(e) => {
                error!("Disk cache corrupt ({}), removing it", e);
                let _ = tokio::fs::remove_file(path).await;
            }
        }
    }

    async fn save_index_to_disk(&self, data: &CacheIndexData) -> std::io::Result<()> {
        let path = &self.config.index_file;
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));

        let json = serde_json::to_vec(data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let mut f = tokio::fs::File::create(&tmp).await?;
        tokio::io::AsyncWriteExt::write_all(&mut f, &json).await?;
        f.sync_all().await?;
        drop(f);

        tokio::fs::rename(&tmp, path).await
    }

    async fn refresh_index(&self) {
        let token = self.get_oci_token().await;
        let manifest_url = format!("https://{}/v2/{}/nix-cache/manifests/cache-index", self.config.registry, self.config.repo);

        let mut req = self.client.get(&manifest_url).header("Accept", "application/vnd.oci.image.manifest.v1+json");
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", token));
        }

        if let Ok(resp) = req.send().await {
            if let Ok(manifest) = resp.json::<serde_json::Value>().await {
                if let Some(layers) = manifest.get("layers").and_then(|l| l.as_array()) {
                    if let Some(digest) = layers.get(0).and_then(|l| l.get("digest")).and_then(|d| d.as_str()) {
                        let blob_url = format!("https://{}/v2/{}/nix-cache/blobs/{}", self.config.registry, self.config.repo, digest);
                        let mut blob_req = self.client.get(&blob_url);
                        if !token.is_empty() {
                            blob_req = blob_req.header("Authorization", format!("Bearer {}", token));
                        }

                        if let Ok(blob_resp) = blob_req.send().await {
                            if let Ok(data) = blob_resp.json::<CacheIndexData>().await {
                                let count = data.entries.len();
                                let data_arc = Arc::new(data);

                                // Update RAM first so the running proxy serves fresh data immediately
                                *self.index.write().await = data_arc.clone();

                                // Persist to disk (best-effort)
                                if let Err(e) = self.save_index_to_disk(&data_arc).await {
                                    error!("Failed to save index to disk: {}", e);
                                }

                                info!("Index refreshed: {} entries", count);
                                return;
                            }
                        }
                    }
                }
            }
        }
        error!("Failed to refresh index from GHCR");
    }
}

async fn handle_nix_cache_info() -> impl IntoResponse {
    let body = "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n";
    ([(header::CONTENT_TYPE, "text/x-nix-cache-info")], body)
}

async fn handle_public_key(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> Response {
    let index = state.index.read().await.clone();
    match &index.public_key {
        Some(pk) => ([(header::CONTENT_TYPE, "text/plain")], format!("{}\n", pk)).into_response(),
        None => (StatusCode::NOT_FOUND, "No public key configured".to_string()).into_response(),
    }
}

async fn handle_status(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> impl IntoResponse {
    let index = state.index.read().await.clone();
    let status = serde_json::json!({
        "index_entries": index.entries.len(),
        "index_ttl": state.config.index_ttl,
        "repo": state.config.repo,
        "index_file": state.config.index_file.display().to_string(),
        "upstream": state.config.upstream_caches,
    });
    axum::Json(status)
}

async fn handle_refresh(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> impl IntoResponse {
    state.refresh_index().await;
    let index = state.index.read().await.clone();
    axum::Json(serde_json::json!({"refreshed": true, "entries": index.entries.len()}))
}

async fn handle_narinfo(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Response {
    let store_hash = path.trim_end_matches(".narinfo");
    let index = state.index.read().await.clone();

    if let Some(entry) = index.entries.get(store_hash) {
        return ([(header::CONTENT_TYPE, "text/x-nix-narinfo")], entry.narinfo.clone()).into_response();
    }

    for cache in &state.config.upstream_caches {
        let url = format!("{}/{}.narinfo", cache, store_hash);
        if let Ok(resp) = state.client.get(&url).send().await {
            if resp.status().is_success() {
                if let Ok(text) = resp.text().await {
                    return ([(header::CONTENT_TYPE, "text/x-nix-narinfo")], text).into_response();
                }
            }
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn handle_nar(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Response {
    let nar_basename = path.trim_start_matches('/');
    let index = state.index.read().await.clone();

    let store_hash = nar_basename.trim_end_matches(".nar.xz").trim_end_matches(".nar");

    let mut digest = None;
    if let Some(entry) = index.entries.get(store_hash) {
        digest = Some(entry.nar_digest.clone());
    }

    if let Some(d) = digest {
        let token = state.get_oci_token().await;
        let url = format!("https://{}/v2/{}/nix-cache/blobs/{}", state.config.registry, state.config.repo, d);
        let mut req = state.client.get(&url);
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", token));
        }
        if let Ok(resp) = req.send().await {
            if resp.status().is_success() {
                let stream = resp.bytes_stream().map_ok(|chunk| chunk).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
                let body = Body::from_stream(stream);
                return Response::builder()
                    .header(header::CONTENT_TYPE, "application/x-xz")
                    .body(body)
                    .unwrap();
            }
        }
    }

    for cache in &state.config.upstream_caches {
        let url = format!("{}/nar/{}", cache, nar_basename);
        if let Ok(resp) = state.client.get(&url).send().await {
            if resp.status().is_success() {
                let stream = resp.bytes_stream().map_ok(|chunk| chunk).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
                let body = Body::from_stream(stream);
                return Response::builder()
                    .header(header::CONTENT_TYPE, "application/x-xz")
                    .body(body)
                    .unwrap();
            }
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let index_dir = env::var("NIXCACHE_INDEX_DIR").unwrap_or_else(|_| "/var/cache/nixcache-proxy".to_string());
    let index_file = PathBuf::from(&index_dir).join("cache-index.json");

    let config = Config {
        repo: env::var("NIXCACHE_REPO").unwrap_or_else(|_| "cmspam/mynixcache-oci".to_string()),
        registry: env::var("NIXCACHE_REGISTRY").unwrap_or_else(|_| "ghcr.io".to_string()),
        port: env::var("NIXCACHE_PORT").unwrap_or_else(|_| "37515".to_string()).parse().unwrap(),
        listen_addr: env::var("NIXCACHE_LISTEN").unwrap_or_else(|_| "127.0.0.1".to_string()),
        github_token: env::var("GITHUB_TOKEN").unwrap_or_else(|_| env::var("GH_TOKEN").unwrap_or_default()),
        index_ttl: env::var("NIXCACHE_INDEX_TTL").unwrap_or_else(|_| "300".to_string()).parse().unwrap(),
        index_file,
        upstream_caches: env::var("NIXCACHE_UPSTREAM").unwrap_or_else(|_| "https://cache.nixos.org".to_string()).split_whitespace().map(String::from).collect(),
    };

    if let Some(parent) = config.index_file.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            error!("Failed to create {}: {}", parent.display(), e);
        }
    }

    let state = AppState::new(config.clone());

    state.cleanup_leftovers().await;
    state.load_index_from_disk().await;

    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            state_clone.refresh_index().await;
            tokio::time::sleep(Duration::from_secs(state_clone.config.index_ttl)).await;
        }
    });

    let app = Router::new()
        .route("/nix-cache-info", get(handle_nix_cache_info).head(handle_nix_cache_info))
        .route("/public-key", get(handle_public_key).head(handle_public_key))
        .route("/_status", get(handle_status))
        .route("/_refresh", post(handle_refresh))
        .route("/nar/*path", get(handle_nar).head(handle_nar))
        .route("/:hash.narinfo", get(handle_narinfo).head(handle_narinfo))
        .with_state(state);

    let addr = format!("{}:{}", config.listen_addr, config.port);
    info!("nixcache-proxy starting on http://{}", addr);
    info!("Index file: {}", config.index_file.display());

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}