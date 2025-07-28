use std::time::Duration;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::{routing::get, Router};
use bot::Bot;
use dotenv::dotenv;
use grammers_client::{Client, Config, InitParams};
use grammers_mtsender::{FixedReconnect, ReconnectionPolicy};
use grammers_session::Session;
use log::info;
use simplelog::TermLogger;

mod bot;
mod command;
mod models;

async fn health_check() -> &'static str {
    "OK"
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file
    dotenv().ok();

    // Initialize logging
    TermLogger::init(
        log::LevelFilter::Info,
        simplelog::ConfigBuilder::new()
            .set_time_format_rfc3339()
            .build(),
        simplelog::TerminalMode::Mixed,
        simplelog::ColorChoice::Auto,
    )
    .expect("error initializing termlogger");

    // Load environment variables
    let api_id = std::env::var("API_ID")
        .context("API_ID env is not set")?
        .parse()?;
    let api_hash = std::env::var("API_HASH").context("API_HASH env is not set")?;
    let bot_token = std::env::var("BOT_TOKEN").context("BOT_TOKEN env is not set")?;
    let mongo_uri = std::env::var("MONGO_URI").context("MONGO_URI env is not set")?;
    let input_channel_id = std::env::var("INPUT_CHANNEL_ID")
        .context("INPUT_CHANNEL_ID env is not set")?
        .parse::<i64>()?;
    let store_channel_id = std::env::var("STORE_CHANNEL_ID")
        .context("STORE_CHANNEL_ID env is not set")?
        .parse::<i64>()?;

    // Fill in the configuration and connect to Telegram
    static RECONNECTION_POLICY: &dyn ReconnectionPolicy = &FixedReconnect {
        attempts: 3,
        delay: Duration::from_secs(5),
    };
    let config = Config {
        api_id,
        api_hash: api_hash.clone(),
        session: Session::load_file_or_create("session.bin")?,
        params: InitParams {
            reconnection_policy: RECONNECTION_POLICY,
            ..Default::default()
        },
    };
    let client = Client::connect(config).await?;

    // Authorize as a bot if needed
    if !client.is_authorized().await? {
        info!("Not authorized, signing in");
        client.bot_sign_in(&bot_token).await?;
    }

    // Save the session to a file
    client.session().save_to_file("session.bin")?;

    // Create the bot
    let bot = Bot::new(client, &mongo_uri, input_channel_id, store_channel_id).await?;
    
    // Create the HTTP server
    let app = Router::new().route("/", get(health_check));
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{}", port).parse::<SocketAddr>()?;
    
    // Run both the bot and HTTP server concurrently
    tokio::select! {
        _ = bot.run() => {},
        _ = axum::serve(tokio::net::TcpListener::bind(addr).await?, app) => {},
    }

    Ok(())
}
