use crate::config::S3Config;
use crate::error::{Error, Result};
use s3::creds::Credentials;
use s3::{Bucket, Region};

pub struct S3Client {
    bucket: Box<Bucket>,
    prefix: String,
}

impl S3Client {
    pub fn new(config: &S3Config) -> Result<Self> {
        let region = Region::Custom {
            region: config.region.clone(),
            endpoint: config.endpoint.clone(),
        };
        let credentials = Credentials::new(
            Some(&config.access_key),
            Some(&config.secret_key),
            None,
            None,
            None,
        )
        .map_err(|e| Error::S3(e.to_string()))?;

        let bucket = Bucket::new(&config.bucket, region, credentials)
            .map_err(|e| Error::S3(e.to_string()))?
            .with_path_style();

        Ok(Self {
            bucket,
            prefix: config.prefix.clone(),
        })
    }

    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", self.prefix, key)
        }
    }

    pub async fn put_object(&self, key: &str, data: &[u8]) -> Result<()> {
        let full_key = self.full_key(key);
        self.bucket
            .put_object(&full_key, data)
            .await
            .map_err(|e| Error::S3(e.to_string()))?;
        Ok(())
    }

    pub async fn get_object(&self, key: &str) -> Result<Vec<u8>> {
        let full_key = self.full_key(key);
        let response = self
            .bucket
            .get_object(&full_key)
            .await
            .map_err(|e| Error::S3(e.to_string()))?;
        Ok(response.to_vec())
    }

    pub async fn delete_object(&self, key: &str) -> Result<()> {
        let full_key = self.full_key(key);
        self.bucket
            .delete_object(&full_key)
            .await
            .map_err(|e| Error::S3(e.to_string()))?;
        Ok(())
    }

    pub async fn delete_objects(&self, keys: &[String]) -> Result<()> {
        for key in keys {
            self.delete_object(key).await?;
        }
        Ok(())
    }

    pub async fn list_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.full_key(prefix);
        let results = self
            .bucket
            .list(full_prefix.clone(), None)
            .await
            .map_err(|e| Error::S3(e.to_string()))?;

        let mut keys = Vec::new();
        for page in &results {
            for obj in &page.contents {
                // Strip our prefix to return relative keys
                let rel = if self.prefix.is_empty() {
                    &obj.key
                } else {
                    obj.key
                        .strip_prefix(&format!("{}/", self.prefix))
                        .unwrap_or(&obj.key)
                };
                keys.push(rel.to_string());
            }
        }
        keys.sort();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_config(prefix: &str) -> S3Config {
        S3Config {
            endpoint: "http://localhost:3900".into(),
            region: "us-east-1".into(),
            bucket: "test-bucket".into(),
            access_key: "test-key".into(),
            secret_key: "test-secret".into(),
            prefix: prefix.into(),
        }
    }

    #[test]
    fn new_with_dummy_config_succeeds() {
        let client = S3Client::new(&dummy_config("backups"));
        assert!(client.is_ok());
    }

    #[test]
    fn full_key_with_prefix() {
        let client = S3Client::new(&dummy_config("backups")).unwrap();
        assert_eq!(client.full_key("foo"), "backups/foo");
        assert_eq!(client.full_key("a/b/c"), "backups/a/b/c");
    }

    #[test]
    fn full_key_without_prefix() {
        let client = S3Client::new(&dummy_config("")).unwrap();
        assert_eq!(client.full_key("foo"), "foo");
        assert_eq!(client.full_key("a/b/c"), "a/b/c");
    }
}
