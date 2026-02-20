use clap::{Parser, Subcommand};
use waloy::{BackupManager, S3Config};

#[derive(Parser)]
#[command(name = "waloy", about = "CLI for waloy SQLite backup management")]
struct Cli {
    /// S3 endpoint URL
    #[arg(long, env = "WALOY_S3_ENDPOINT")]
    endpoint: String,

    /// S3 region
    #[arg(long, env = "WALOY_S3_REGION")]
    region: String,

    /// S3 bucket name
    #[arg(long, env = "WALOY_S3_BUCKET")]
    bucket: String,

    /// S3 access key
    #[arg(long, env = "WALOY_S3_ACCESS_KEY")]
    access_key: String,

    /// S3 secret key
    #[arg(long, env = "WALOY_S3_SECRET_KEY")]
    secret_key: String,

    /// S3 key prefix
    #[arg(long, env = "WALOY_S3_PREFIX", default_value = "")]
    prefix: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Restore a database from S3
    Restore {
        /// Target path for the restored database
        #[arg(short, long)]
        output: String,

        /// Optional: restore to a specific point in time (milliseconds since epoch)
        #[arg(long)]
        timestamp: Option<u64>,
    },
    /// List all generations in S3
    Generations,
    /// Inspect a specific generation or the latest
    Inspect {
        /// Generation ID to inspect (defaults to latest)
        #[arg(short, long)]
        generation: Option<String>,
    },
}

fn s3_config(cli: &Cli) -> S3Config {
    S3Config {
        endpoint: cli.endpoint.clone(),
        region: cli.region.clone(),
        bucket: cli.bucket.clone(),
        access_key: cli.access_key.clone(),
        secret_key: cli.secret_key.clone(),
        prefix: cli.prefix.clone(),
    }
}

// S3 helper for CLI commands that need direct bucket access
struct CliS3 {
    bucket: Box<s3::Bucket>,
    prefix: String,
}

fn create_cli_s3(config: &S3Config) -> waloy::Result<CliS3> {
    use s3::creds::Credentials;
    use s3::{Bucket, Region};

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
    .map_err(|e| waloy::Error::S3(e.to_string()))?;

    let bucket = Bucket::new(&config.bucket, region, credentials)
        .map_err(|e| waloy::Error::S3(e.to_string()))?
        .with_path_style();

    Ok(CliS3 {
        bucket,
        prefix: config.prefix.clone(),
    })
}

impl CliS3 {
    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", self.prefix, key)
        }
    }

    async fn get_object(&self, key: &str) -> waloy::Result<Vec<u8>> {
        let full_key = self.full_key(key);
        let response = self
            .bucket
            .get_object(&full_key)
            .await
            .map_err(|e| waloy::Error::S3(e.to_string()))?;
        Ok(response.to_vec())
    }

    async fn list_keys(&self, prefix: &str) -> waloy::Result<Vec<String>> {
        let full_prefix = self.full_key(prefix);
        let results = self
            .bucket
            .list(full_prefix.clone(), None)
            .await
            .map_err(|e| waloy::Error::S3(e.to_string()))?;

        let mut keys = Vec::new();
        for page in &results {
            for obj in &page.contents {
                let rel = if self.prefix.is_empty() {
                    obj.key.clone()
                } else {
                    obj.key
                        .strip_prefix(&format!("{}/", self.prefix))
                        .unwrap_or(&obj.key)
                        .to_string()
                };
                keys.push(rel);
            }
        }
        keys.sort();
        Ok(keys)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let s3 = s3_config(&cli);

    match cli.command {
        Commands::Restore { output, timestamp } => {
            if let Some(ts) = timestamp {
                println!("Restoring to point-in-time: {ts}ms");
                BackupManager::restore_to_time(&s3, &output, ts).await?;
            } else {
                println!("Restoring latest backup...");
                BackupManager::restore(&s3, &output).await?;
            }
            println!("Restored to: {output}");
        }
        Commands::Generations => {
            let client = create_cli_s3(&s3)?;
            let keys = client.list_keys("").await?;

            let mut generations: Vec<String> = keys
                .iter()
                .filter(|k| k.ends_with("/manifest.json"))
                .map(|k| k.trim_end_matches("/manifest.json").to_string())
                .collect();
            generations.sort();

            let latest = client
                .get_object("latest")
                .await
                .ok()
                .and_then(|b| String::from_utf8(b).ok());

            println!("Generations ({}):", generations.len());
            for generation in &generations {
                let marker = if latest.as_deref() == Some(generation) {
                    " (latest)"
                } else {
                    ""
                };
                let manifest_key = format!("{generation}/manifest.json");
                if let Ok(data) = client.get_object(&manifest_key).await
                    && let Ok(m) = serde_json::from_slice::<waloy::GenerationManifest>(&data)
                {
                    println!(
                        "  {generation}{marker}  created={}  segments={}",
                        m.created_at_ms,
                        m.segments.len()
                    );
                    continue;
                }
                println!("  {generation}{marker}");
            }
        }
        Commands::Inspect { generation } => {
            let client = create_cli_s3(&s3)?;

            let gen_id = match generation {
                Some(g) => g,
                None => {
                    let bytes = client.get_object("latest").await?;
                    String::from_utf8(bytes).map_err(|e| waloy::Error::Other(e.to_string()))?
                }
            };

            println!("Generation: {gen_id}");

            let manifest_key = format!("{gen_id}/manifest.json");
            match client.get_object(&manifest_key).await {
                Ok(data) => {
                    let m: waloy::GenerationManifest = serde_json::from_slice(&data)
                        .map_err(|e| waloy::Error::Other(e.to_string()))?;
                    println!("Created: {}ms", m.created_at_ms);
                    println!("Snapshot timestamp: {}ms", m.snapshot_timestamp_ms);
                    println!("WAL segments: {}", m.segments.len());
                    for seg in &m.segments {
                        println!(
                            "  [{:08}] offset={} size={} timestamp={}ms",
                            seg.index, seg.offset, seg.size, seg.timestamp_ms
                        );
                    }
                }
                Err(_) => {
                    println!("No manifest found (legacy generation without manifest)");
                    let prefix = format!("{gen_id}/");
                    let keys = client.list_keys(&prefix).await?;
                    for key in &keys {
                        println!("  {key}");
                    }
                }
            }
        }
    }

    Ok(())
}
