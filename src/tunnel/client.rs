use super::{JwtTunnelConfig, RemoteAddr, TransportScheme, JWT_DECODE};
use crate::tunnel::connectors::TunnelConnector;
use crate::tunnel::listeners::TunnelListener;
use crate::tunnel::transport::{TunnelReader, TunnelWriter};
use crate::{tunnel, WsClientConfig};
use futures_util::pin_mut;
use hyper::header::COOKIE;
use jsonwebtoken::TokenData;
use log::debug;
use std::ops::Deref;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;
use tokio_stream::StreamExt;
use tracing::{error, event, span, Instrument, Level, Span};
use url::Host;
use uuid::Uuid;

async fn connect_to_server<R, W>(
    request_id: Uuid,
    client_cfg: &WsClientConfig,
    remote_cfg: &RemoteAddr,
    duplex_stream: (R, W),
) -> anyhow::Result<()>
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    // Connect to server with the correct protocol
    let (ws_rx, ws_tx, response) = match client_cfg.remote_addr.scheme() {
        TransportScheme::Ws | TransportScheme::Wss => {
            tunnel::transport::websocket::connect(request_id, client_cfg, remote_cfg)
                .await
                .map(|(r, w, response)| (TunnelReader::Websocket(r), TunnelWriter::Websocket(w), response))?
        }
        TransportScheme::Http | TransportScheme::Https => {
            tunnel::transport::http2::connect(request_id, client_cfg, remote_cfg)
                .await
                .map(|(r, w, response)| (TunnelReader::Http2(r), TunnelWriter::Http2(w), response))?
        }
    };

    debug!("Server response: {:?}", response);
    let (local_rx, local_tx) = duplex_stream;
    let (close_tx, close_rx) = oneshot::channel::<()>();

    // Forward local tx to websocket tx
    let ping_frequency = client_cfg.websocket_ping_frequency;
    tokio::spawn(
        super::transport::io::propagate_local_to_remote(local_rx, ws_tx, close_tx, Some(ping_frequency))
            .instrument(Span::current()),
    );

    // Forward websocket rx to local rx
    let _ = super::transport::io::propagate_remote_to_local(local_tx, ws_rx, close_rx).await;

    Ok(())
}

pub async fn run_tunnel(client_config: Arc<WsClientConfig>, incoming_cnx: impl TunnelListener) -> anyhow::Result<()> {
    pin_mut!(incoming_cnx);
    while let Some(cnx) = incoming_cnx.next().await {
        let (cnx_stream, remote_addr) = match cnx {
            Ok((cnx_stream, remote_addr)) => (cnx_stream, remote_addr),
            Err(err) => {
                error!("Error accepting connection: {:?}", err);
                continue;
            }
        };

        let request_id = Uuid::now_v7();
        let span = span!(
            Level::INFO,
            "tunnel",
            id = request_id.to_string(),
            remote = format!("{}:{}", remote_addr.host, remote_addr.port)
        );
        let client_config = client_config.clone();

        let tunnel = async move {
            let _ = connect_to_server(request_id, &client_config, &remote_addr, cnx_stream)
                .await
                .map_err(|err| error!("{:?}", err));
        }
        .instrument(span);

        tokio::spawn(tunnel);
    }

    Ok(())
}

pub async fn run_reverse_tunnel(
    client_cfg: Arc<WsClientConfig>,
    remote_addr: RemoteAddr,
    connector: impl TunnelConnector,
) -> anyhow::Result<()> {
    loop {
        let client_config = client_cfg.clone();
        let request_id = Uuid::now_v7();
        let span = span!(
            Level::INFO,
            "tunnel",
            id = request_id.to_string(),
            remote = format!("{}:{}", remote_addr.host, remote_addr.port)
        );
        // Correctly configure tunnel cfg
        let (ws_rx, ws_tx, response) = match client_cfg.remote_addr.scheme() {
            TransportScheme::Ws | TransportScheme::Wss => {
                match tunnel::transport::websocket::connect(request_id, &client_cfg, &remote_addr)
                    .instrument(span.clone())
                    .await
                {
                    Ok((r, w, response)) => (TunnelReader::Websocket(r), TunnelWriter::Websocket(w), response),
                    Err(err) => {
                        event!(parent: &span, Level::ERROR, "Retrying in 1sec, cannot connect to remote server: {:?}", err);
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                }
            }
            TransportScheme::Http | TransportScheme::Https => {
                match tunnel::transport::http2::connect(request_id, &client_cfg, &remote_addr)
                    .instrument(span.clone())
                    .await
                {
                    Ok((r, w, response)) => (TunnelReader::Http2(r), TunnelWriter::Http2(w), response),
                    Err(err) => {
                        event!(parent: &span, Level::ERROR, "Retrying in 1sec, cannot connect to remote server: {:?}", err);
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                }
            }
        };

        // Connect to endpoint
        event!(parent: &span, Level::DEBUG, "Server response: {:?}", response);
        let remote = response
            .headers
            .get(COOKIE)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| {
                let (validation, decode_key) = JWT_DECODE.deref();
                let jwt: Option<TokenData<JwtTunnelConfig>> = jsonwebtoken::decode(h, decode_key, validation).ok();
                jwt
            })
            .map(|jwt| RemoteAddr {
                protocol: jwt.claims.p,
                host: Host::parse(&jwt.claims.r).unwrap_or_else(|_| Host::Domain(String::new())),
                port: jwt.claims.rp,
            });

        let (local_rx, local_tx) = match connector.connect(&remote).instrument(span.clone()).await {
            Ok(s) => s,
            Err(err) => {
                event!(parent: &span, Level::ERROR, "Cannot connect to {remote:?}: {err:?}");
                continue;
            }
        };

        let (close_tx, close_rx) = oneshot::channel::<()>();
        let tunnel = async move {
            let ping_frequency = client_config.websocket_ping_frequency;
            tokio::spawn(
                super::transport::io::propagate_local_to_remote(local_rx, ws_tx, close_tx, Some(ping_frequency))
                    .in_current_span(),
            );

            // Forward websocket rx to local rx
            let _ = super::transport::io::propagate_remote_to_local(local_tx, ws_rx, close_rx).await;
        }
        .instrument(span.clone());
        tokio::spawn(tunnel);
    }
}
