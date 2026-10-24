//! Small, bounded client for the fixed internal embedding endpoint.
use moka::future::Cache;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Semaphore,
};

/// Query embeddings are the only model work permitted in the request path.
pub struct Client {
    address: String,
    cache: Cache<String, Arc<[f32]>>,
    concurrent: Semaphore,
    requests: Semaphore,
}
impl Client {
    /// Uses a startup-configured internal host and a ten-minute, thousand-entry cache.
    #[must_use]
    pub fn new(address: String) -> Self {
        Self {
            address,
            cache: Cache::builder()
                .max_capacity(1000)
                .time_to_live(Duration::from_secs(600))
                .build(),
            concurrent: Semaphore::new(16),
            requests: Semaphore::new(16),
        }
    }
    pub(crate) fn permit(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        self.requests.try_acquire().ok()
    }
    /// Falls back on any timeout, overload, malformed vector or unavailable service.
    pub async fn embed(&self, text: &str) -> Option<Arc<[f32]>> {
        if let Some(vector) = self.cache.get(text).await {
            return Some(vector);
        }
        let result = tokio::time::timeout(Duration::from_millis(100), self.request(text))
            .await
            .ok()
            .flatten();
        if let Some(vector) = &result {
            self.cache.insert(text.to_owned(), vector.clone()).await;
        } else {
            tracing::warn!("query embedding unavailable");
            metrics::counter!("yhotos_search_embedding_failures_total").increment(1);
        }
        result
    }
    async fn request(&self, text: &str) -> Option<Arc<[f32]>> {
        let _permit = self.concurrent.try_acquire().ok()?;
        let body = serde_json::to_vec(&serde_json::json!({"text":text})).ok()?;
        let mut stream = TcpStream::connect(&self.address).await.ok()?;
        let header = format!(
            "POST /embed-text HTTP/1.1\r\nHost: yhotos-ml\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).await.ok()?;
        stream.write_all(&body).await.ok()?;
        let mut response = Vec::with_capacity(16_384);
        stream.take(32_769).read_to_end(&mut response).await.ok()?;
        if response.len() > 32_768 {
            return None;
        }
        let separator = response.windows(4).position(|s| s == b"\r\n\r\n")?;
        let header = std::str::from_utf8(response.get(..separator)?).ok()?;
        if !header.lines().next()?.starts_with("HTTP/1.1 200 ") {
            return None;
        }
        let value: Embedding = serde_json::from_slice(response.get(separator + 4..)?).ok()?;
        if value.vector.len() != 768 || value.vector.iter().any(|n| !n.is_finite()) {
            return None;
        }
        let norm: f32 = value.vector.iter().map(|x| x * x).sum();
        if !(0.99..=1.01).contains(&norm) {
            return None;
        }
        Some(value.vector.into())
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Embedding {
    vector: Vec<f32>,
}
