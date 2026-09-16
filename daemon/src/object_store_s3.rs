// SPDX-License-Identifier: Apache-2.0
//! S3-compatible [`ObjectStore`] backend.
//!
//! The CLI supplies `s3://bucket/optional/prefix`; credentials and region are
//! resolved by the standard AWS SDK chain (`AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY`, optional `AWS_SESSION_TOKEN`, `AWS_REGION`, shared
//! profiles, container/instance roles). A custom endpoint enables path-style
//! requests, which is the usual configuration for MinIO.

use aws_config::{meta::region::RegionProviderChain, BehaviorVersion};
use aws_sdk_s3::{error::ProvideErrorMetadata, primitives::ByteStream, Client};

use crate::object_store::{validate_length_range, ObjectStore, ObjectStoreError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
struct S3Location {
    bucket: String,
    prefix: String,
}

impl S3Location {
    fn parse(location: &str) -> Result<Self> {
        let remainder = location.strip_prefix("s3://").ok_or_else(|| {
            ObjectStoreError::InvalidKey("object location must start with s3://".to_string())
        })?;
        let (bucket, prefix) = remainder.split_once('/').unwrap_or((remainder, ""));
        if bucket.is_empty()
            || bucket.contains(['?', '#', '@'])
            || bucket.chars().any(char::is_whitespace)
        {
            return Err(ObjectStoreError::InvalidKey(
                "S3 bucket name is empty or invalid".to_string(),
            ));
        }
        if prefix.contains(['?', '#']) || prefix.split('/').any(|part| part == "..") {
            return Err(ObjectStoreError::InvalidKey(
                "S3 prefix contains an invalid component".to_string(),
            ));
        }

        Ok(Self {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
        })
    }

    fn object_key(&self, key: &str) -> Result<String> {
        validate_key(key)?;
        if self.prefix.is_empty() {
            Ok(key.to_string())
        } else {
            Ok(format!("{}/{key}", self.prefix))
        }
    }
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.starts_with('/') || key.split('/').any(|part| part == "..") {
        return Err(ObjectStoreError::InvalidKey(
            "object key must be non-empty, relative, and contain no '..' component".to_string(),
        ));
    }
    Ok(())
}

fn is_not_found_code(code: Option<&str>) -> bool {
    matches!(code, Some("NoSuchKey" | "NotFound" | "404"))
}

/// An S3/MinIO implementation of [`ObjectStore`].
#[derive(Clone)]
pub struct S3ObjectStore {
    client: Client,
    location: S3Location,
}

impl S3ObjectStore {
    /// Builds an S3 client. This does not create the bucket; the configured
    /// bucket must already exist.
    pub async fn new(location: &str, endpoint: Option<&str>) -> Result<Self> {
        let location = S3Location::parse(location)?;
        if endpoint
            .is_some_and(|value| !(value.starts_with("http://") || value.starts_with("https://")))
        {
            return Err(ObjectStoreError::InvalidKey(
                "S3 endpoint must start with http:// or https://".to_string(),
            ));
        }

        let region = RegionProviderChain::default_provider().or_else("us-east-1");
        let shared_config = aws_config::defaults(BehaviorVersion::latest())
            .region(region)
            .load()
            .await;
        let mut config = aws_sdk_s3::config::Builder::from(&shared_config);
        if let Some(endpoint) = endpoint {
            config = config.endpoint_url(endpoint).force_path_style(true);
        }

        Ok(Self {
            client: Client::from_conf(config.build()),
            location,
        })
    }

    async fn get_checked(
        &self,
        key: &str,
        expected_range: Option<(usize, usize)>,
    ) -> Result<Vec<u8>> {
        let object_key = self.location.object_key(key)?;
        let output = self
            .client
            .get_object()
            .bucket(&self.location.bucket)
            .key(&object_key)
            .send()
            .await
            .map_err(|error| {
                if is_not_found_code(error.as_service_error().and_then(|value| value.code())) {
                    ObjectStoreError::NotFound(key.to_string())
                } else {
                    ObjectStoreError::Io(format!("S3 GET {object_key} failed: {error}"))
                }
            })?;
        let declared_len = output.content_length().map(|length| {
            usize::try_from(length).map_err(|_| {
                ObjectStoreError::Integrity(format!(
                    "S3 GET {object_key} returned invalid Content-Length {length}"
                ))
            })
        }).transpose()?;
        if let (Some((min_len, max_len)), Some(declared)) = (expected_range, declared_len) {
            validate_length_range(key, min_len, max_len, declared)?;
        }
        let bytes = output.body.collect().await.map_err(|error| {
            ObjectStoreError::Io(format!("S3 GET {object_key} body failed: {error}"))
        })?;
        let value = bytes.into_bytes().to_vec();
        if let Some(declared) = declared_len {
            validate_length_range(key, declared, declared, value.len())?;
        }
        if let Some((min_len, max_len)) = expected_range {
            validate_length_range(key, min_len, max_len, value.len())?;
        }
        Ok(value)
    }

    #[cfg(test)]
    pub(crate) async fn ensure_bucket_for_test(&self) {
        if std::env::var("S3_CREATE_BUCKET").as_deref() != Ok("1") {
            return;
        }
        match self
            .client
            .create_bucket()
            .bucket(&self.location.bucket)
            .send()
            .await
        {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.as_service_error().and_then(|value| value.code()),
                    Some("BucketAlreadyExists" | "BucketAlreadyOwnedByYou")
                ) => {}
            Err(error) => panic!("failed to create integration-test bucket: {error}"),
        }
    }
}

#[async_trait::async_trait]
impl ObjectStore for S3ObjectStore {
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.get_checked(key, None).await
    }

    async fn get_exact(&self, key: &str, expected_len: usize) -> Result<Vec<u8>> {
        self.get_checked(key, Some((expected_len, expected_len))).await
    }

    async fn get_range(&self, key: &str, min_len: usize, max_len: usize) -> Result<Vec<u8>> {
        self.get_checked(key, Some((min_len, max_len))).await
    }

    async fn put(&self, key: String, value: Vec<u8>) -> Result<()> {
        let object_key = self.location.object_key(&key)?;
        self.client
            .put_object()
            .bucket(&self.location.bucket)
            .key(&object_key)
            .body(ByteStream::from(value))
            .send()
            .await
            .map_err(|error| {
                ObjectStoreError::Io(format!("S3 PUT {object_key} failed: {error}"))
            })?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let object_key = self.location.object_key(key)?;
        match self
            .client
            .delete_object()
            .bucket(&self.location.bucket)
            .key(&object_key)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(error)
                if is_not_found_code(error.as_service_error().and_then(|value| value.code())) =>
            {
                Ok(())
            }
            Err(error) => Err(ObjectStoreError::Io(format!(
                "S3 DELETE {object_key} failed: {error}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn parses_bucket_prefix_and_builds_object_keys() {
        let root = S3Location::parse("s3://bucket").unwrap();
        assert_eq!(root.bucket, "bucket");
        assert_eq!(root.object_key("slice/0").unwrap(), "slice/0");

        let nested = S3Location::parse("s3://bucket/kestrel/data/").unwrap();
        assert_eq!(nested.prefix, "kestrel/data");
        assert_eq!(
            nested.object_key("slice/0").unwrap(),
            "kestrel/data/slice/0"
        );
    }

    #[test]
    fn rejects_invalid_locations_and_keys_without_network_access() {
        for location in ["bucket/prefix", "s3://", "s3://bad bucket/prefix"] {
            assert!(matches!(
                S3Location::parse(location),
                Err(ObjectStoreError::InvalidKey(_))
            ));
        }
        let location = S3Location::parse("s3://bucket/prefix").unwrap();
        for key in ["", "/absolute", "a/../b"] {
            assert!(matches!(
                location.object_key(key),
                Err(ObjectStoreError::InvalidKey(_))
            ));
        }
    }

    /// Opt-in integration test for MinIO or another S3-compatible service.
    /// The bucket must already exist. A random prefix isolates concurrent runs.
    #[tokio::test]
    async fn s3_environment_gated_put_get_overwrite_and_delete() {
        let (Ok(endpoint), Ok(bucket), Ok(_access_key), Ok(_secret_key)) = (
            std::env::var("S3_ENDPOINT"),
            std::env::var("S3_BUCKET"),
            std::env::var("AWS_ACCESS_KEY_ID"),
            std::env::var("AWS_SECRET_ACCESS_KEY"),
        ) else {
            eprintln!(
                "S3_ENDPOINT/S3_BUCKET/AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY not all set; \
                 skipping S3 integration assertions"
            );
            return;
        };
        let base_prefix = std::env::var("S3_PREFIX").unwrap_or_else(|_| "kestrelfs-tests".into());
        let prefix = format!("{base_prefix}/{}", Uuid::new_v4());
        let store = S3ObjectStore::new(&format!("s3://{bucket}/{prefix}"), Some(&endpoint))
            .await
            .unwrap();
        store.ensure_bucket_for_test().await;
        let key = "slice/0";

        store.put(key.to_string(), b"first".to_vec()).await.unwrap();
        assert_eq!(store.get_exact(key, 5).await.unwrap(), b"first");
        assert!(matches!(
            store.get_exact(key, 6).await,
            Err(ObjectStoreError::Integrity(_))
        ));
        store
            .put(key.to_string(), b"replacement".to_vec())
            .await
            .unwrap();
        assert_eq!(store.get(key).await.unwrap(), b"replacement");
        store.delete(key).await.unwrap();
        store.delete(key).await.unwrap();
        assert!(matches!(
            store.get(key).await,
            Err(ObjectStoreError::NotFound(_))
        ));
    }
}
