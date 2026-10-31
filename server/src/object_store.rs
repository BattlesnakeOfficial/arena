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

    fn is_configured(&self) -> bool {
        false
    }
}

/// Test double: a path-keyed map with a get-counter and a one-shot
/// storage-fault knob.
#[derive(Default)]
pub struct InMemoryObjectStore {
    objects: std::sync::Mutex<HashMap<String, Vec<u8>>>,
    gets: std::sync::atomic::AtomicUsize,
    /// When set, the next `get` returns `Err` once, then clears itself.
    fail_next_get: std::sync::atomic::AtomicBool,
}

impl InMemoryObjectStore {
    pub fn get_count(&self) -> usize {
        self.gets.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn fail_next_get(&self) {
        self.fail_next_get
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ObjectStore for InMemoryObjectStore {
    async fn put(&self, path: &str, bytes: Vec<u8>) -> cja::Result<()> {
        self.objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_string(), bytes);
        Ok(())
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
    async fn fail_next_get_fires_exactly_once() {
        let store = InMemoryObjectStore::default();
        store.put("p", vec![9]).await.unwrap();
        store.fail_next_get();
        assert!(store.get("p").await.is_err());
        assert_eq!(store.get("p").await.unwrap(), Some(vec![9]));
        assert!(store.get("p").await.is_ok());
    }

    #[tokio::test]
    async fn disabled_store_reports_unconfigured_and_errors() {
        let store = DisabledObjectStore;
        assert!(!store.is_configured());
        assert!(store.get("any").await.is_err());
        assert!(store.put("any", vec![]).await.is_err());
    }

    #[tokio::test]
    async fn configured_stores_inherit_the_true_default() {
        assert!(GcsObjectStore::new("b".to_string()).is_configured());
        assert!(InMemoryObjectStore::default().is_configured());
    }
}
