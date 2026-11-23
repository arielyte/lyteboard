use std::{io, net::SocketAddr};

use axum::{Router, routing::get};
use tokio::net::TcpListener;

fn router() -> Router {
    Router::new().route("/healthz", get(|| async { "ok" }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let address = SocketAddr::from(([127, 0, 0, 1], 8787));
    let listener = TcpListener::bind(address).await?;
    eprintln!("Lyteboard relay scaffold listening on http://{address}/healthz");

    axum::serve(listener, router())
        .with_graceful_shutdown(shutdown_signal())
        .await
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("Could not listen for Ctrl-C: {error}");
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::router;

    #[tokio::test]
    async fn health_returns_ok() {
        let request = Request::get("/healthz").body(Body::empty()).unwrap();
        let response = router().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(to_bytes(response.into_body(), 16).await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn relay_route_is_unavailable() {
        let request = Request::get("/ws").body(Body::empty()).unwrap();
        let response = router().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
