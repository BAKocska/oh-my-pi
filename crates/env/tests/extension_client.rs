#![cfg(unix)]

use std::{
	io,
	path::PathBuf,
	time::{SystemTime, UNIX_EPOCH},
};

use bytes::{Bytes, BytesMut};
use omp_env::{ClientError, DataScope, ExtensionEnvClient, frame, prost::Message as _};
use tokio::{
	io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
	net::{UnixListener, UnixStream},
};

fn socket_path(label: &str) -> PathBuf {
	let nonce = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("clock after epoch")
		.as_nanos();
	std::env::temp_dir().join(format!("omp-env-{label}-{}-{nonce}.sock", std::process::id()))
}

fn scope() -> DataScope {
	DataScope::new("invocation-7", Bytes::from_static(b"effect-ticket"), 11, 13).deny_pty()
}

fn assert_scope(frame: &frame::ClientFrame) {
	let scope = frame.scope.as_ref().expect("extension frame scope");
	assert_eq!(scope.invocation_id, "invocation-7");
	assert_eq!(scope.effect_token, Bytes::from_static(b"effect-ticket"));
	assert_eq!(scope.host_generation, 11);
	assert_eq!(scope.session_generation, 13);
	assert!(scope.pty_denied);
}

#[tokio::test]
async fn uds_client_handshakes_once_and_scopes_data_and_cancellation() {
	let path = socket_path("scope");
	let listener = UnixListener::bind(&path).expect("bind extension socket");
	let server = tokio::spawn(async move {
		let (mut stream, _) = listener.accept().await.expect("accept extension");
		let hello = read_frame(&mut stream).await.expect("read hello");
		assert_eq!(hello.request_id, 0);
		assert!(hello.scope.is_none(), "the constructor handshake is not invocation traffic");
		assert!(matches!(hello.body, Some(frame::client_frame::Body::Hello(_))));
		write_frame(&mut stream, &frame::ServerFrame {
			request_id: 0,
			body: Some(frame::server_frame::Body::Hello(frame::ServerHello {
				server_epoch: Bytes::from_static(b"epoch"),
				..Default::default()
			})),
			..Default::default()
		})
		.await
		.expect("write hello");

		let request = read_frame(&mut stream).await.expect("read DATA request");
		assert_scope(&request);
		assert!(matches!(request.body, Some(frame::client_frame::Body::Data(_))));
		let cancel = read_frame(&mut stream).await.expect("read cancellation");
		assert_scope(&cancel);
		match cancel.body {
			Some(frame::client_frame::Body::Cancel(cancel)) => {
				assert_eq!(
					cancel.target,
					Some(frame::cancel_request::Target::TargetRequestId(request.request_id))
				);
			},
			body => panic!("expected cancellation, got {body:?}"),
		}
	});

	let client = ExtensionEnvClient::connect_uds(
		&path,
		&frame::ClientHello { client: "python-extension".into(), ..Default::default() },
		scope(),
	)
	.await
	.expect("connect scoped extension");
	assert_eq!(client.info().expect("cached handshake").server_epoch, Bytes::from_static(b"epoch"));
	let stream = client
		.stream(frame::DataRequest {
			body: Some(frame::data_request::Body::HostInfo(frame::HostInfoRequest::default())),
			..Default::default()
		})
		.await
		.expect("open DATA stream");
	stream.cancel();
	server.await.expect("server task");
	std::fs::remove_file(path).expect("remove extension socket");
}

#[tokio::test]
async fn scoped_http_preserves_typed_remote_errors() {
	let path = socket_path("error");
	let listener = UnixListener::bind(&path).expect("bind extension socket");
	let server = tokio::spawn(async move {
		let (mut stream, _) = listener.accept().await.expect("accept extension");
		let _hello = read_frame(&mut stream).await.expect("read hello");
		write_frame(&mut stream, &frame::ServerFrame {
			request_id: 0,
			body: Some(frame::server_frame::Body::Hello(frame::ServerHello::default())),
			..Default::default()
		})
		.await
		.expect("write hello");
		let request = read_frame(&mut stream).await.expect("read HTTP request");
		assert_scope(&request);
		assert!(matches!(request.body, Some(frame::client_frame::Body::HttpRequest(_))));
		write_frame(&mut stream, &frame::ServerFrame {
			request_id: request.request_id,
			body: Some(frame::server_frame::Body::Error(frame::ProtocolError {
				code: frame::ProtocolErrorCode::PermissionDenied as i32,
				message: "policy denied upstream".into(),
				..Default::default()
			})),
			..Default::default()
		})
		.await
		.expect("write typed error");
	});

	let client = ExtensionEnvClient::connect_uds(
		&path,
		&frame::ClientHello { client: "python-extension".into(), ..Default::default() },
		scope(),
	)
	.await
	.expect("connect scoped extension");
	let error = client
		.http(frame::HttpRequest {
			method: "GET".into(),
			url: "https://example.invalid".into(),
			..Default::default()
		})
		.await
		.expect_err("remote policy error");
	match error {
		ClientError::Protocol(error) => {
			assert_eq!(error.code, frame::ProtocolErrorCode::PermissionDenied as i32);
			assert_eq!(error.message, "policy denied upstream");
		},
		other => panic!("expected typed protocol error, got {other:?}"),
	}
	server.await.expect("server task");
	std::fs::remove_file(path).expect("remove extension socket");
}

async fn read_frame(stream: &mut UnixStream) -> io::Result<frame::ClientFrame> {
	let length = read_length(stream)
		.await?
		.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
	let mut payload = vec![0; length];
	stream.read_exact(&mut payload).await?;
	frame::ClientFrame::decode(&payload[..]).map_err(io::Error::other)
}

async fn write_frame(stream: &mut UnixStream, frame: &frame::ServerFrame) -> io::Result<()> {
	let mut encoded = BytesMut::new();
	frame
		.encode_length_delimited(&mut encoded)
		.map_err(io::Error::other)?;
	stream.write_all(&encoded).await
}

async fn read_length<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<usize>> {
	let mut value = 0_u64;
	for shift in (0..70).step_by(7) {
		let byte = match reader.read_u8().await {
			Ok(byte) => byte,
			Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && shift == 0 => {
				return Ok(None);
			},
			Err(error) => return Err(error),
		};
		value |= u64::from(byte & 0x7f) << shift;
		if byte & 0x80 == 0 {
			return usize::try_from(value).map(Some).map_err(io::Error::other);
		}
	}
	Err(io::Error::new(io::ErrorKind::InvalidData, "invalid frame length"))
}
