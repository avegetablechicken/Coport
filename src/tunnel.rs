//! Byte tunnels: destinations are explicitly configured; TLS payloads stay opaque.
use crate::{Error, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub(crate) type Socket = Box<dyn Io>;

pub(crate) fn authority(value: &str) -> Result<(String, u16)> {
    let invalid = || Error::new(400, "CONNECT requires an explicit host:port destination.");
    let authority = value
        .parse::<hyper::http::uri::Authority>()
        .map_err(|_| invalid())?;
    let port = authority
        .port_u16()
        .filter(|p| *p != 0)
        .ok_or_else(invalid)?;
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if host.is_empty()
        || value.contains(['@', '/', '?', '#', '\\', '%'])
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_:".contains(&b))
    {
        return Err(invalid());
    }
    Ok((host.to_ascii_lowercase(), port))
}

fn io_error(error: impl std::error::Error + 'static) -> Error {
    let error: &dyn std::error::Error = &error;
    if error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
    {
        Error::config("CONNECT connection refused.")
    } else {
        Error::config("CONNECT transport failed.")
    }
}

pub(crate) async fn open(host: &str, port: u16, endpoint: &str) -> Result<Socket> {
    open_with_tls(host, port, endpoint, || {
        native_tls::TlsConnector::new().map_err(io_error)
    })
    .await
}

async fn open_with_tls(
    host: &str,
    port: u16,
    endpoint: &str,
    tls: impl FnOnce() -> Result<native_tls::TlsConnector>,
) -> Result<Socket> {
    if endpoint == "none" {
        return Ok(Box::new(
            TcpStream::connect((host, port)).await.map_err(io_error)?,
        ));
    }
    let proxy = crate::config::validate_proxy(endpoint)?;
    let proxy_host = proxy
        .host_str()
        .unwrap()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let tcp = TcpStream::connect((proxy_host, proxy.port_or_known_default().unwrap()))
        .await
        .map_err(io_error)?;
    let mut socket: Socket = if proxy.scheme() == "https" {
        let connector = tls()?;
        Box::new(
            tokio_native_tls::TlsConnector::from(connector)
                .connect(proxy_host, tcp)
                .await
                .map_err(io_error)?,
        )
    } else {
        Box::new(tcp)
    };
    if proxy.scheme() == "socks5" {
        socks(&mut socket, &proxy, host, port).await?;
    } else {
        http_connect(&mut socket, &proxy, host, port).await?;
    }
    Ok(socket)
}

fn credentials(proxy: &Url) -> Result<(String, String)> {
    let decode = |value: &str| {
        percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .map(|v| v.into_owned())
            .map_err(io_error)
    };
    Ok((
        decode(proxy.username())?,
        decode(proxy.password().unwrap_or(""))?,
    ))
}

async fn http_connect(socket: &mut Socket, proxy: &Url, host: &str, port: u16) -> Result<()> {
    let destination = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n");
    if !proxy.username().is_empty() {
        let (user, password) = credentials(proxy)?;
        request.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            STANDARD.encode(format!("{user}:{password}"))
        ));
    }
    request.push_str("\r\n");
    socket
        .write_all(request.as_bytes())
        .await
        .map_err(io_error)?;
    // Read precisely the headers so early tunnel bytes remain on the socket.
    let mut head = Vec::new();
    loop {
        if head.len() >= 65536 {
            return Err(Error::config("CONNECT proxy headers exceed 64 KiB."));
        }
        head.push(socket.read_u8().await.map_err(io_error)?);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut response = httparse::Response::new(&mut headers);
    response.parse(&head).map_err(io_error)?;
    if response.code == Some(407) {
        return Err(Error::new(407, "Proxy authentication failed."));
    }
    if !response
        .code
        .is_some_and(|status| (200..300).contains(&status))
    {
        return Err(Error::config("Outbound proxy rejected CONNECT."));
    }
    Ok(())
}

async fn socks(socket: &mut Socket, proxy: &Url, host: &str, port: u16) -> Result<()> {
    let auth = !proxy.username().is_empty();
    socket
        .write_all(&[5, 1, if auth { 2 } else { 0 }])
        .await
        .map_err(io_error)?;
    let mut reply = [0; 2];
    socket.read_exact(&mut reply).await.map_err(io_error)?;
    if reply != [5, if auth { 2 } else { 0 }] {
        return Err(Error::new(407, "SOCKS5 authentication negotiation failed."));
    }
    if auth {
        let (user, password) = credentials(proxy)?;
        let mut login = vec![1, user.len() as u8];
        login.extend_from_slice(user.as_bytes());
        login.push(password.len() as u8);
        login.extend_from_slice(password.as_bytes());
        socket.write_all(&login).await.map_err(io_error)?;
        socket.read_exact(&mut reply).await.map_err(io_error)?;
        if reply != [1, 0] {
            return Err(Error::new(407, "SOCKS5 authentication failed."));
        }
    }
    let mut request = vec![5, 1, 0];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            if host.len() > 255 {
                return Err(Error::new(400, "CONNECT hostname is too long."));
            }
            request.extend_from_slice(&[3, host.len() as u8]);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    socket.write_all(&request).await.map_err(io_error)?;
    let mut response = [0; 4];
    socket.read_exact(&mut response).await.map_err(io_error)?;
    if response[..3] != [5, 0, 0] {
        return Err(Error::config("SOCKS5 CONNECT failed."));
    }
    let length = match response[3] {
        1 => 4,
        4 => 16,
        3 => socket.read_u8().await.map_err(io_error)? as usize,
        _ => return Err(Error::config("Invalid SOCKS5 response.")),
    };
    let mut bound = vec![0; length + 2];
    socket.read_exact(&mut bound).await.map_err(io_error)?;
    Ok(())
}

pub(crate) fn header_token(headers: &hyper::HeaderMap, name: &str, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .any(|s| s.trim().eq_ignore_ascii_case(token))
}

pub(crate) fn websocket(incoming: &hyper::Request<hyper::body::Incoming>) -> Result<bool> {
    let headers = incoming.headers();
    if !headers.contains_key("upgrade") {
        return Ok(false);
    }
    if incoming.method() != "GET"
        || incoming.version() != hyper::Version::HTTP_11
        || !header_token(headers, "connection", "upgrade")
        || headers
            .get("upgrade")
            .and_then(|v| v.to_str().ok())
            .is_none_or(|s| !s.eq_ignore_ascii_case("websocket"))
        || headers
            .get("sec-websocket-version")
            .is_none_or(|v| v != "13")
    {
        return Err(Error::new(
            426,
            "A WebSocket version 13 GET upgrade is required.",
        ));
    }
    if headers
        .get("sec-websocket-key")
        .and_then(|v| STANDARD.decode(v.as_bytes()).ok())
        .is_none_or(|v| v.len() != 16)
        || headers.contains_key("transfer-encoding")
        || headers.get("content-length").is_some_and(|v| v != "0")
    {
        return Err(Error::new(400, "Invalid WebSocket handshake."));
    }
    Ok(true)
}

pub(crate) fn websocket_accept(key: &[u8]) -> String {
    let mut value = key.to_vec();
    value.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    STANDARD.encode(ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        &value,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_config_validates() {
        let config = crate::config::Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\nconnect:\n  'api.anthropic.com:443': none\n  '[::1]:8443': none\n").unwrap();
        assert_eq!(config.connect.len(), 2);
        for value in [
            "api.anthropic.com",
            "api.anthropic.com:0",
            "user@api.anthropic.com:443",
            "api.anthropic.com:443/path",
            "https://api.anthropic.com:443",
        ] {
            assert!(authority(value).is_err(), "{value}");
        }
        assert_eq!(authority("[::1]:8443").unwrap(), ("::1".into(), 8443));
        assert!(crate::config::Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\nconnect:\n  'API.ANTHROPIC.COM:443': none\n  'api.anthropic.com:443': none\n").is_err());
        assert!(crate::config::Config::parse("listen_port: 8787\nrequest_timeout_seconds: 3\nconnect:\n  'api.anthropic.com:443': missing\n").is_err());
        assert_eq!(
            websocket_accept(b"dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[tokio::test]
    async fn socks5_remote_dns_and_optional_authentication() {
        for auth in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let peer = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut greeting = [0; 3];
                socket.read_exact(&mut greeting).await.unwrap();
                assert_eq!(greeting, [5, 1, if auth { 2 } else { 0 }]);
                socket
                    .write_all(&[5, if auth { 2 } else { 0 }])
                    .await
                    .unwrap();
                if auth {
                    let mut login = [0; 5];
                    socket.read_exact(&mut login).await.unwrap();
                    assert_eq!(login, [1, 1, b'u', 1, b'p']);
                    socket.write_all(&[1, 0]).await.unwrap();
                }
                let mut request = [0; 5];
                socket.read_exact(&mut request).await.unwrap();
                assert_eq!(&request[..4], &[5, 1, 0, 3]);
                let mut host = vec![0; request[4] as usize];
                socket.read_exact(&mut host).await.unwrap();
                assert_eq!(host, b"destination.invalid");
                assert_eq!(socket.read_u16().await.unwrap(), 443);
                socket
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 1, 1])
                    .await
                    .unwrap();
                socket.write_all(b"hello").await.unwrap();
                let mut data = [0; 5];
                socket.read_exact(&mut data).await.unwrap();
                assert_eq!(&data, b"world");
            });
            let endpoint = format!("socks5://{}{address}", if auth { "u:p@" } else { "" });
            let mut socket = open("destination.invalid", 443, &endpoint).await.unwrap();
            let mut data = [0; 5];
            socket.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"hello");
            socket.write_all(b"world").await.unwrap();
            peer.await.unwrap();
        }
    }

    #[tokio::test]
    async fn https_proxy_verifies_tls_and_tunnels_bytes() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let trusted = native_tls::Certificate::from_der(cert.cert.der()).unwrap();
        let tls = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
                    cert.signing_key.serialize_der(),
                )
                .into(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(tls));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = tokio::spawn(async move {
            // First connection must fail because its certificate is untrusted.
            let (socket, _) = listener.accept().await.unwrap();
            if let Ok(mut socket) = acceptor.accept(socket).await {
                let mut byte = [0];
                assert!(socket.read_exact(&mut byte).await.is_err());
            }
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = acceptor.accept(socket).await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            assert!(head.starts_with(b"CONNECT destination.invalid:443 HTTP/1.1"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\n\r\nhello")
                .await
                .unwrap();
            socket.flush().await.unwrap();
            let mut data = [0; 5];
            socket.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"world");
        });
        let endpoint = format!("https://localhost:{port}");
        assert!(open("destination.invalid", 443, &endpoint).await.is_err());
        let tls = native_tls::TlsConnector::builder()
            .add_root_certificate(trusted)
            .build()
            .unwrap();
        let mut socket = open_with_tls("destination.invalid", 443, &endpoint, || Ok(tls))
            .await
            .unwrap();
        let mut data = [0; 5];
        socket.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"hello");
        socket.write_all(b"world").await.unwrap();
        socket.flush().await.unwrap();
        peer.await.unwrap();
    }
}
