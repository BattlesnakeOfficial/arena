//! The one module that talks to object storage for game archives.
//! `backup.rs` writes through `put`; `archive.rs` reads through `get`.
//! Each implementation owns its destination (bucket) — DEV-1664's S3
//! support lands here as another impl, nowhere else.

use std::collections::HashMap;

use google_cloud_storage::{
    client::{Client as GcsClient, ClientConfig},
    http::objects::download::Range,
    http::objects::get::GetObjectRequest,
    http::objects::upload::{Media, UploadObjectRequest, UploadType},
};

/// Byte-level object storage. `get` returns `Ok(None)` iff the object does
/// not exist; every other failure is an `Err`.
#[async_trait::async_trait]
pub trait ObjectStore: Send + Sync {
    async fn put(&self, path: &str, bytes: Vec<u8>) -> cja::Result<()>;
    async fn get(&self, path: &str) -> cja::Result<Option<Vec<u8>>>;

    /// Create the object only if the path is absent. `Ok(true)` = created,
    /// `Ok(false)` = an object already exists (nothing written). Never
    /// overwrites (DEV-2128: the bucket has no versioning, so a plain
    /// `put` to an existing path is an irreversible overwrite). No default
    /// impl — a get-then-put default would be racy.
    async fn put_if_absent(&self, path: &str, bytes: Vec<u8>) -> cja::Result<bool>;

    /// Whether this store can do work. `false` only for
    /// [`DisabledObjectStore`]; lets callers (backup) fail fast before
    /// expensive work without reading bucket config themselves.
    fn is_configured(&self) -> bool {
        true
    }
}

/// Shared GCS client. Building one per job re-ran TLS setup and a metadata
/// token fetch for every archived game; the client refreshes its own token.
static GCS_CLIENT: tokio::sync::OnceCell<GcsClient> = tokio::sync::OnceCell::const_new();

async fn gcs_client() -> cja::Result<&'static GcsClient> {
    GCS_CLIENT
        .get_or_try_init(|| async {
            let config = ClientConfig::default()
                .with_auth()
                .await
                .wrap_err("Failed to configure GCS client")?;
            Ok(GcsClient::new(config))
        })
        .await
}

use color_eyre::eyre::Context as _;

/// GCS-backed store: wraps the process-global client (moved from
/// `backup.rs`, which owned the only other copy).
pub struct GcsObjectStore {
    bucket: String,
}

impl GcsObjectStore {
    pub fn new(bucket: String) -> Self {
        Self { bucket }
    }
}

#[async_trait::async_trait]
impl ObjectStore for GcsObjectStore {
    async fn put(&self, path: &str, bytes: Vec<u8>) -> cja::Result<()> {
        let upload_type = UploadType::Simple(Media::new(path.to_string()));
        gcs_client()
            .await?
            .upload_object(
                &UploadObjectRequest {
                    bucket: self.bucket.clone(),
                    ..Default::default()
                },
                bytes,
                &upload_type,
            )
            .await
            .wrap_err("Failed to upload to GCS")?;

        Ok(())
    }

    async fn put_if_absent(&self, path: &str, bytes: Vec<u8>) -> cja::Result<bool> {
        let upload_type = UploadType::Simple(Media::new(path.to_string()));
        match gcs_client()
            .await?
            .upload_object(
                &UploadObjectRequest {
                    bucket: self.bucket.clone(),
                    if_generation_match: Some(0),
                    ..Default::default()
                },
                bytes,
                &upload_type,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if is_precondition_failed(&e) => Ok(false),
            Err(e) => Err(cja::color_eyre::eyre::eyre!(
                "Failed to create object at GCS {}/{path}: {e}",
                self.bucket
            )),
        }
    }

    async fn get(&self, path: &str) -> cja::Result<Option<Vec<u8>>> {
        let client = gcs_client().await?;
        match client
            .download_object(
                &GetObjectRequest {
                    bucket: self.bucket.clone(),
                    object: path.to_string(),
                    ..Default::default()
                },
                &Range::default(),
            )
            .await
        {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(cja::color_eyre::eyre::eyre!(
                "GCS download failed for {}/{path}: {e}",
                self.bucket
            )),
        }
    }
}

/// GCS signals a missing object as an HTTP 404 error response.
fn is_not_found(err: &google_cloud_storage::http::Error) -> bool {
    matches!(err, google_cloud_storage::http::Error::Response(resp) if resp.code == 404)
}

/// GCS signals "object already exists" (ifGenerationMatch=0 refused) as
/// HTTP 412 — as a JSON ErrorResponse when the body parses, or as a bare
/// reqwest status error when it doesn't (check_response_status falls back
/// to Error::HttpClient), so both variants mean 412 here.
fn is_precondition_failed(err: &google_cloud_storage::http::Error) -> bool {
    match err {
        google_cloud_storage::http::Error::Response(resp) => resp.code == 412,
        google_cloud_storage::http::Error::HttpClient(e) => {
            e.status().is_some_and(|s| s.as_u16() == 412)
        }
        _ => false,
    }
}

/// What AppState holds when no bucket is configured: every operation
/// errors (call-time "GCS bucket not configured" failure, mapping to 500
/// for archive reads) and `is_configured()` is false.
pub struct DisabledObjectStore;

#[async_trait::async_trait]
impl ObjectStore for DisabledObjectStore {
    async fn put(&self, _path: &str, _bytes: Vec<u8>) -> cja::Result<()> {
        Err(cja::color_eyre::eyre::eyre!("GCS bucket not configured"))
    }

    async fn get(&self, _path: &str) -> cja::Result<Option<Vec<u8>>> {
        Err(cja::color_eyre::eyre::eyre!("GCS bucket not configured"))
    }

    async fn put_if_absent(&self, _path: &str, _bytes: Vec<u8>) -> cja::Result<bool> {
        Err(cja::color_eyre::eyre::eyre!("GCS bucket not configured"))
    }

    fn is_configured(&self) -> bool {
        false
    }
}

/// Test double: a path-keyed map with get/put counters and one-shot
/// storage-fault knobs.
#[derive(Default)]
pub struct InMemoryObjectStore {
    objects: std::sync::Mutex<HashMap<String, Vec<u8>>>,
    gets: std::sync::atomic::AtomicUsize,
    puts: std::sync::atomic::AtomicUsize,
    /// When set, the next `get` returns `Err` once, then clears itself.
    fail_next_get: std::sync::atomic::AtomicBool,
    /// When set, the next `put` returns `Err` once, then clears itself.
    fail_next_put: std::sync::atomic::AtomicBool,
}

impl InMemoryObjectStore {
    pub fn get_count(&self) -> usize {
        self.gets.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Number of `put` calls so far — the "no put happened" control for
    /// idempotency tests (DEV-1984).
    pub fn put_count(&self) -> usize {
        self.puts.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn fail_next_get(&self) {
        self.fail_next_get
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn fail_next_put(&self) {
        self.fail_next_put
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ObjectStore for InMemoryObjectStore {
    async fn put(&self, path: &str, bytes: Vec<u8>) -> cja::Result<()> {
        if self
            .fail_next_put
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(cja::color_eyre::eyre::eyre!("Injected storage failure"));
        }
        self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_string(), bytes);
        Ok(())
    }

    async fn put_if_absent(&self, path: &str, bytes: Vec<u8>) -> cja::Result<bool> {
        if self
            .fail_next_put
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(cja::color_eyre::eyre::eyre!("Injected storage failure"));
        }
        let mut objects = self.objects.lock().unwrap_or_else(|e| e.into_inner());
        if objects.contains_key(path) {
            return Ok(false);
        }
        objects.insert(path.to_string(), bytes);
        // Counted only when the object is actually created, so put_count()
        // stays a valid "no write happened" control for create-only paths.
        self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(true)
    }

    async fn get(&self, path: &str) -> cja::Result<Option<Vec<u8>>> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .fail_next_get
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(cja::color_eyre::eyre::eyre!("Injected storage failure"));
        }
        Ok(self
            .objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_put_get_round_trip() {
        let store = InMemoryObjectStore::default();
        store.put("a/b.json.zst", vec![1, 2, 3]).await.unwrap();
        assert_eq!(
            store.get("a/b.json.zst").await.unwrap(),
            Some(vec![1, 2, 3])
        );
    }

    #[tokio::test]
    async fn in_memory_absent_path_is_none() {
        let store = InMemoryObjectStore::default();
        assert_eq!(store.get("missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn in_memory_counts_gets() {
        let store = InMemoryObjectStore::default();
        let _ = store.get("x").await;
        let _ = store.get("y").await;
        assert_eq!(store.get_count(), 2);
    }

    #[tokio::test]
    async fn in_memory_counts_puts() {
        let store = InMemoryObjectStore::default();
        store.put("p", vec![1]).await.unwrap();
        store.put("p", vec![2]).await.unwrap();
        assert_eq!(store.put_count(), 2);
    }

    #[tokio::test]
    async fn fail_next_get_fires_exactly_once() {
        let store = InMemoryObjectStore::default();
        store.put("p", vec![9]).await.unwrap();
        store.fail_next_get();
        assert!(store.get("p").await.is_err());
        assert_eq!(store.get("p").await.unwrap(), Some(vec![9]));
        assert!(store.get("p").await.is_ok());
    }

    #[tokio::test]
    async fn puts_are_counted_and_fail_next_put_fires_once() {
        let store = InMemoryObjectStore::default();
        store.put("a", vec![1]).await.unwrap();
        store.fail_next_put();
        assert!(store.put("b", vec![2]).await.is_err());
        store.put("c", vec![3]).await.unwrap();
        // The failed put counts nothing; two real puts went through.
        assert_eq!(store.put_count(), 2);
        assert!(store.get("b").await.unwrap().is_none());
        assert_eq!(store.get("c").await.unwrap(), Some(vec![3]));
    }

    #[tokio::test]
    async fn disabled_store_reports_unconfigured_and_errors() {
        let store = DisabledObjectStore;
        assert!(!store.is_configured());
        assert!(store.get("any").await.is_err());
        assert!(store.put("any", vec![]).await.is_err());
    }

    #[tokio::test]
    async fn disabled_store_put_if_absent_errors() {
        let store = DisabledObjectStore;
        assert!(store.put_if_absent("any", vec![]).await.is_err());
    }

    /// put_if_absent creates once, then refuses without touching the bytes
    /// or counting a put; the fault knob fires for it too.
    #[tokio::test]
    async fn put_if_absent_creates_then_refuses() {
        let store = InMemoryObjectStore::default();
        assert!(store.put_if_absent("p", vec![7]).await.unwrap());
        assert_eq!(store.get("p").await.unwrap(), Some(vec![7]));

        // An existing path is refused and nothing changes.
        assert!(!store.put_if_absent("p", vec![9]).await.unwrap());
        assert_eq!(
            store.get("p").await.unwrap(),
            Some(vec![7]),
            "bytes unchanged"
        );
        assert_eq!(store.put_count(), 1, "the refused call counts nothing");

        // The fault knob fires before the existence check and counts nothing.
        store.fail_next_put();
        assert!(store.put_if_absent("q", vec![1]).await.is_err());
        assert_eq!(store.put_count(), 1);
        assert_eq!(store.get("q").await.unwrap(), None);
    }

    /// The 412 classifier must accept both shapes google-cloud-storage
    /// 0.22.1 produces: a JSON `ErrorResponse` and a bare reqwest status
    /// error (check_response_status falls back to the latter when the
    /// error body doesn't parse). The `HttpClient` arm can't be unit-
    /// tested — `reqwest::Error` has no public constructor — so its
    /// `as_u16()` comparison is covered by review here.
    #[test]
    fn is_precondition_failed_matches_both_412_variants() {
        use google_cloud_storage::http::Error;
        use google_cloud_storage::http::error::ErrorResponse;

        let response = |code: u16| {
            Error::Response(ErrorResponse {
                code,
                errors: vec![],
                message: String::new(),
            })
        };
        assert!(is_precondition_failed(&response(412)));
        assert!(!is_precondition_failed(&response(404)));
        assert!(!is_precondition_failed(&response(500)));
        // Non-response variants are never a precondition failure.
        let token_error = std::io::Error::other("unauth");
        assert!(!is_precondition_failed(&Error::TokenSource(Box::new(
            token_error
        ))));
    }

    #[tokio::test]
    async fn configured_stores_inherit_the_true_default() {
        assert!(GcsObjectStore::new("b".to_string()).is_configured());
        assert!(InMemoryObjectStore::default().is_configured());
    }
}
