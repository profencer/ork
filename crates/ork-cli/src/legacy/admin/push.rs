//! `ork legacy admin push` — JWKS rotation operations (ADR-0009).

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum PushAdminCommand {
    /// Force generation of a new ES256 signing key. The previous key stays
    /// in JWKS for the configured overlap window so subscribers cached by
    /// `kid` keep verifying in-flight requests.
    RotateKeys,
}

pub async fn run(cmd: PushAdminCommand, verbose: bool) -> Result<()> {
    match cmd {
        PushAdminCommand::RotateKeys => rotate_keys(verbose).await,
    }
}

/// Opens the same Postgres pool the API uses, derives the KEK from
/// `auth.jwt_secret`, and triggers a forced rotation. Prints a small JSON
/// envelope on stdout so operators can pipe through `jq`.
async fn rotate_keys(verbose: bool) -> Result<()> {
    let config = ork_common::config::AppConfig::load()
        .context("load AppConfig (ORK__ env or config/default.toml)")?;
    if verbose {
        eprintln!(
            "Connecting to Postgres at {} (max_connections={})",
            config.database.url, config.database.max_connections
        );
    }
    let pool = ork_persistence::postgres::create_pool(
        &config.database.url,
        config.database.max_connections,
    )
    .await
    .context("connect to database")?;
    let repo: Arc<dyn ork_core::ports::a2a_signing_key_repo::A2aSigningKeyRepository> = Arc::new(
        ork_persistence::postgres::a2a_signing_key_repo::PgA2aSigningKeyRepository::new(pool),
    );
    let kek = ork_push::encryption::derive_kek(&config.auth.jwt_secret);
    let policy = ork_push::signing::RotationPolicy {
        rotation_days: config.push.key_rotation_days,
        overlap_days: config.push.key_overlap_days,
    };
    let provider = ork_push::JwksProvider::new(repo, kek, policy)
        .await
        .context("build JWKS provider")?;
    let outcome = provider
        .rotate_if_due(Utc::now(), true)
        .await
        .context("rotate signing key")?;
    match outcome {
        Some(o) => {
            let body = serde_json::json!({
                "rotated": true,
                "new_kid": o.new_kid,
                "new_expires_at": o.new_expires_at,
                "previous_kid": o.previous_kid,
            });
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
        None => {
            println!("{{\"rotated\": false}}");
        }
    }
    Ok(())
}
