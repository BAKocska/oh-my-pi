//! Minimal JSON-over-websocket link used by the CDP and `BiDi` drivers.

use futures::{SinkExt, StreamExt};
use omp_core::fmts;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::{Error, Result};

/// One websocket connection speaking newline-free JSON text messages.
pub struct WsLink {
	inner: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl WsLink {
	/// Connect to `url` (`ws://127.0.0.1:<port>/...`).
	pub async fn connect(url: &str) -> Result<Self> {
		let (inner, _) = tokio_tungstenite::connect_async(url).await?;
		Ok(Self { inner })
	}

	/// Send one JSON message.
	pub async fn send_json(&mut self, value: &serde_json::Value) -> Result<()> {
		self.inner.send(Message::text(value.to_string())).await?;
		Ok(())
	}

	/// Receive the next JSON message; `None` once the peer closed.
	/// Ping/pong frames are handled by the transport and skipped here.
	pub async fn recv_json(&mut self) -> Result<Option<serde_json::Value>> {
		while let Some(msg) = self.inner.next().await {
			match msg? {
				Message::Text(text) => {
					return serde_json::from_str(&text)
						.map(Some)
						.map_err(|err| Error::Protocol(fmts!("malformed message: {err}")));
				},
				Message::Close(_) => return Ok(None),
				_ => {},
			}
		}
		Ok(None)
	}
}
