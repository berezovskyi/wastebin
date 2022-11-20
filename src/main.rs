use crate::db::Database;
use anyhow::{Context, Result};
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::{Extension, Server};
use clap::{Parser, Subcommand};
use once_cell::sync::Lazy;
use std::env::{self, VarError};
use std::io;
use std::net::SocketAddr;
use std::num::{NonZeroUsize, TryFromIntError};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower_http::compression::CompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

mod cache;
mod db;
mod handler;
mod highlight;
mod id;
mod pages;
#[cfg(test)]
mod test_helpers;
mod token;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the server
    Serve,
    /// Issue a new token for a user
    Token {
        /// Name of the user for which the token is issued
        name: String,
        /// Use if the user has administrative capabilities
        #[arg(long)]
        is_admin: bool,
    },
}

pub static TITLE: Lazy<String> =
    Lazy::new(|| env::var("WASTEBIN_TITLE").unwrap_or_else(|_| "wastebin".to_string()));

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("axum http error: {0}")]
    Axum(#[from] axum::http::Error),
    #[error("deletion time expired")]
    DeletionTimeExpired,
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migrations error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("wrong size")]
    WrongSize,
    #[error("illegal characters")]
    IllegalCharacters,
    #[error("integer conversion error: {0}")]
    IntConversion(#[from] TryFromIntError),
    #[error("join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("syntax highlighting error: {0}")]
    SyntaxHighlighting(#[from] syntect::Error),
    #[error("syntax parsing error: {0}")]
    SyntaxParsing(#[from] syntect::parsing::ParsingError),
    #[error("time formatting error: {0}")]
    TimeFormatting(#[from] time::error::Format),
    #[error("failed to create token: {0}")]
    TokenCreation(String),
    #[error("failed to validate token: {0}")]
    TokenValidation(String),
}

pub type Router = axum::Router<http_body::Limited<axum::body::Body>>;

impl From<Error> for StatusCode {
    fn from(err: Error) -> Self {
        match err {
            Error::Sqlite(err) => match err {
                rusqlite::Error::QueryReturnedNoRows => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            Error::IllegalCharacters | Error::WrongSize | Error::DeletionTimeExpired => {
                StatusCode::BAD_REQUEST
            }
            Error::Join(_)
            | Error::IntConversion(_)
            | Error::TimeFormatting(_)
            | Error::Migration(_)
            | Error::SyntaxHighlighting(_)
            | Error::SyntaxParsing(_)
            | Error::TokenCreation(_)
            | Error::Axum(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Error::TokenValidation(_) => StatusCode::UNAUTHORIZED,
        }
    }
}

pub(crate) fn make_app(
    cache_layer: cache::Layer,
    issuer: Arc<token::Issuer>,
    max_body_size: usize,
) -> axum::Router {
    Router::new()
        .merge(handler::routes())
        .layer(Extension(cache_layer))
        .layer(Extension(issuer))
        .layer(TimeoutLayer::new(Duration::from_secs(5)))
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(max_body_size))
}

async fn serve(issuer: token::Issuer) -> Result<()> {
    const VAR_DATABASE_PATH: &str = "WASTEBIN_DATABASE_PATH";
    const VAR_CACHE_SIZE: &str = "WASTEBIN_CACHE_SIZE";
    const VAR_ADDRESS_PORT: &str = "WASTEBIN_ADDRESS_PORT";
    const VAR_MAX_BODY_SIZE: &str = "WASTEBIN_MAX_BODY_SIZE";

    let database = match env::var(VAR_DATABASE_PATH) {
        Ok(path) => Ok(Database::new(db::Open::Path(PathBuf::from(path)))?),
        Err(VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{VAR_DATABASE_PATH} contains non-unicode data"),
        )),
        Err(VarError::NotPresent) => Ok(Database::new(db::Open::Memory)?),
    }?;

    let cache_size = env::var(VAR_CACHE_SIZE)
        .map_or_else(
            |_| Ok(NonZeroUsize::new(128).unwrap()),
            |s| s.parse::<NonZeroUsize>(),
        )
        .with_context(|| format!("failed to parse {VAR_CACHE_SIZE}, expect number of elements"))?;

    let cache_layer = cache::Layer::new(database, cache_size);

    let addr: SocketAddr = env::var(VAR_ADDRESS_PORT)
        .unwrap_or_else(|_| "0.0.0.0:8088".to_string())
        .parse()
        .with_context(|| format!("failed to parse {VAR_ADDRESS_PORT}, expect `host:port`"))?;

    let max_body_size = env::var(VAR_MAX_BODY_SIZE)
        .map_or_else(|_| Ok(1024 * 1024), |s| s.parse::<usize>())
        .with_context(|| format!("failed to parse {VAR_MAX_BODY_SIZE}, expect number of bytes"))?;

    tracing::debug!("serving on {addr}");
    tracing::debug!("caching {cache_size} paste highlights");
    tracing::debug!("restricting maximum body size to {max_body_size} bytes");

    let service =
        make_app(cache_layer.clone(), Arc::new(issuer), max_body_size).into_make_service();

    let server = Server::bind(&addr)
        .serve(service)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen to ctrl-c");
        });

    tokio::select! {
        res = server => {
            res?;
        },
        res = cache::purge_periodically(cache_layer) => {
            res?;
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    const VAR_JWT_SECRET: &str = "WASTEBIN_JWT_SECRET";
    const VAR_JWT_ISSUER: &str = "WASTEBIN_JWT_ISSUER";

    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    let secret = env::var(VAR_JWT_SECRET).with_context(|| format!("{VAR_JWT_SECRET} not set"))?;
    let iss = env::var(VAR_JWT_ISSUER).with_context(|| format!("{VAR_JWT_ISSUER} not set"))?;
    let issuer = token::Issuer::new(secret.as_bytes(), iss);

    match cli.command {
        Commands::Serve => serve(issuer).await,
        Commands::Token { name, is_admin } => {
            let role = if is_admin {
                token::Role::Admin
            } else {
                token::Role::User
            };

            let user = token::User { name, role };
            let token = issuer.issue(user)?;

            println!("Store this token securely: {token}");

            Ok(())
        }
    }
}
