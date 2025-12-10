mod config;
mod db;
mod email;
mod feed;
mod render;
mod worker;

use crate::email::Mailer;
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use db::init_db;
use lettre::message::Mailbox;
use lettre::{AsyncSmtpTransport, Tokio1Executor};
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use worker::Worker;

pub async fn run() -> Result<()> {
    let config_path =
        std::env::var("YAF2M_CONFIG_PATH").unwrap_or_else(|_| "config/config.toml".to_string());

    let database_url =
        std::env::var("POSTGRES_URL").wrap_err("POSTGRES_URL environment variable not set")?;

    let pool = PgPoolOptions::new()
        .max_connections(20)
        .acquire_slow_threshold(Duration::from_secs(10))
        .acquire_timeout(Duration::from_mins(2))
        .connect(&database_url)
        .await?;

    init_db(&pool).await?;

    let from_str = std::env::var("SMTP_FROM").wrap_err("SMTP_FROM environment variable not set")?;
    let from = from_str.parse::<Mailbox>().wrap_err("Invalid SMTP_FROM")?;

    let smtp_url = std::env::var("SMTP_URL").wrap_err("SMTP_URL environment variable not set")?;
    let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(&smtp_url)?.build();

    let mailer = Mailer { from, transport };

    Worker::new(pool, config_path, mailer).run().await
}
