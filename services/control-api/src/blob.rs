use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_s3::presigning::PresigningConfig;

use crate::{ApiError, AppConfig};

#[derive(Clone)]
pub struct BlobStore {
    backend: Arc<dyn BlobBackend>,
}

struct S3Backend {
    client: aws_sdk_s3::Client,
    presign_client: aws_sdk_s3::Client,
    bucket: String,
    presign_ttl: Duration,
}

pub struct PresignedRequest {
    pub url: String,
    pub method: String,
    pub headers: BTreeMap<String, String>,
}

pub struct ObjectMetadata {
    pub size_bytes: i64,
    pub sha256: Option<String>,
}

#[async_trait]
trait BlobBackend: Send + Sync {
    async fn health(&self) -> Result<(), ApiError>;
    async fn presign_put(
        &self,
        key: &str,
        content_type: &str,
        sha256: &str,
    ) -> Result<PresignedRequest, ApiError>;
    async fn presign_get(&self, key: &str) -> Result<PresignedRequest, ApiError>;
    async fn head(&self, key: &str) -> Result<ObjectMetadata, ApiError>;
    async fn put(
        &self,
        key: &str,
        content_type: &str,
        sha256: &str,
        bytes: Vec<u8>,
    ) -> Result<(), ApiError>;
}

impl BlobStore {
    pub async fn from_config(config: &AppConfig) -> Result<Self, ApiError> {
        Self::from_config_with_bucket(config, config.s3_bucket.clone()).await
    }

    pub async fn from_config_with_bucket(
        config: &AppConfig,
        bucket: String,
    ) -> Result<Self, ApiError> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(config.s3_region.clone()));
        if let Some(endpoint) = &config.s3_endpoint {
            loader = loader.endpoint_url(endpoint);
        }
        let shared = loader.load().await;
        let s3_config = aws_sdk_s3::config::Builder::from(&shared)
            .force_path_style(config.s3_force_path_style)
            .build();
        let presign_config = if let Some(endpoint) = &config.s3_public_endpoint {
            aws_sdk_s3::config::Builder::from(&shared)
                .endpoint_url(endpoint)
                .force_path_style(config.s3_force_path_style)
                .build()
        } else {
            s3_config.clone()
        };
        let backend = S3Backend {
            client: aws_sdk_s3::Client::from_conf(s3_config),
            presign_client: aws_sdk_s3::Client::from_conf(presign_config),
            bucket,
            presign_ttl: Duration::from_secs(config.blob_presign_ttl_seconds),
        };
        Ok(Self {
            backend: Arc::new(backend),
        })
    }

    pub async fn health(&self) -> Result<(), ApiError> {
        self.backend.health().await
    }

    pub async fn presign_put(
        &self,
        key: &str,
        content_type: &str,
        sha256: &str,
    ) -> Result<PresignedRequest, ApiError> {
        self.backend.presign_put(key, content_type, sha256).await
    }

    pub async fn presign_get(&self, key: &str) -> Result<PresignedRequest, ApiError> {
        self.backend.presign_get(key).await
    }

    pub async fn head(&self, key: &str) -> Result<ObjectMetadata, ApiError> {
        self.backend.head(key).await
    }

    pub async fn put(
        &self,
        key: &str,
        content_type: &str,
        sha256: &str,
        bytes: Vec<u8>,
    ) -> Result<(), ApiError> {
        self.backend.put(key, content_type, sha256, bytes).await
    }
}

impl S3Backend {
    fn presigning_config(&self) -> Result<PresigningConfig, ApiError> {
        PresigningConfig::builder()
            .expires_in(self.presign_ttl)
            .build()
            .map_err(|error| ApiError::internal(format!("building presign config: {error}")))
    }
}

#[async_trait]
impl BlobBackend for S3Backend {
    async fn health(&self) -> Result<(), ApiError> {
        self.client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .map_err(|error| {
                ApiError::internal(format!("checking blob bucket {}: {error}", self.bucket))
            })?;
        Ok(())
    }

    async fn put(
        &self,
        key: &str,
        content_type: &str,
        sha256: &str,
        bytes: Vec<u8>,
    ) -> Result<(), ApiError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .metadata("sha256", sha256)
            .body(aws_sdk_s3::primitives::ByteStream::from(bytes))
            .send()
            .await
            .map_err(|error| ApiError::internal(format!("uploading package artifact: {error}")))?;
        Ok(())
    }

    async fn presign_put(
        &self,
        key: &str,
        content_type: &str,
        sha256: &str,
    ) -> Result<PresignedRequest, ApiError> {
        let request = self
            .presign_client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .metadata("sha256", sha256)
            .presigned(self.presigning_config()?)
            .await
            .map_err(|error| ApiError::internal(format!("presigning blob upload: {error}")))?;
        Ok(PresignedRequest {
            url: request.uri().to_string(),
            method: request.method().to_string(),
            headers: request
                .headers()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        })
    }

    async fn presign_get(&self, key: &str) -> Result<PresignedRequest, ApiError> {
        let request = self
            .presign_client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(self.presigning_config()?)
            .await
            .map_err(|error| ApiError::internal(format!("presigning blob download: {error}")))?;
        Ok(PresignedRequest {
            url: request.uri().to_string(),
            method: request.method().to_string(),
            headers: request
                .headers()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        })
    }

    async fn head(&self, key: &str) -> Result<ObjectMetadata, ApiError> {
        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| ApiError::bad_request(format!("uploaded blob not found: {error}")))?;
        Ok(ObjectMetadata {
            size_bytes: result.content_length().unwrap_or_default(),
            sha256: result
                .metadata()
                .and_then(|values| values.get("sha256"))
                .cloned(),
        })
    }
}
