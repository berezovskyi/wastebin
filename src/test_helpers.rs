use crate::db::{self, Database};
use crate::{cache, token};
use axum::body::HttpBody;
use axum::BoxError;
use http::Request;
use hyper::{Body, Server};
use reqwest::RequestBuilder;
use std::net::{SocketAddr, TcpListener};
use std::num::NonZeroUsize;
use std::sync::Arc;
use tower::make::Shared;
use tower_service::Service;

pub(crate) struct Client {
    client: reqwest::Client,
    addr: SocketAddr,
}

impl Client {
    pub(crate) fn new<S, ResBody>(svc: S) -> Self
    where
        S: Service<Request<Body>, Response = http::Response<ResBody>> + Clone + Send + 'static,
        ResBody: HttpBody + Send + 'static,
        ResBody::Data: Send,
        ResBody::Error: Into<BoxError>,
        S::Future: Send,
        S::Error: Into<BoxError>,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Could not bind ephemeral socket");
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let server = Server::from_tcp(listener).unwrap().serve(Shared::new(svc));
            server.await.expect("server error");
        });

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        Self { client, addr }
    }

    pub(crate) fn get(&self, url: &str) -> RequestBuilder {
        self.client.get(format!("http://{}{}", self.addr, url))
    }

    pub(crate) fn post(&self, url: &str) -> RequestBuilder {
        self.client.post(format!("http://{}{}", self.addr, url))
    }

    pub(crate) fn delete(&self, url: &str) -> RequestBuilder {
        self.client.delete(format!("http://{}{}", self.addr, url))
    }
}

pub(crate) fn make_app() -> Result<axum::Router, Box<dyn std::error::Error>> {
    let database = Database::new(db::Open::Memory)?;
    let cache_layer = cache::Layer::new(database.clone(), NonZeroUsize::new(128).unwrap());
    let issuer = token::Issuer::new(&[1, 2, 3, 4], "test".to_string());
    Ok(crate::make_app(cache_layer, Arc::new(issuer), 4096))
}
