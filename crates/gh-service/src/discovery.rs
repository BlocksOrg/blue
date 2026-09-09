//! Unauthenticated first-run discovery for a metaharness deployment.

use gh_common::GhError;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DiscoveryDocument {
    pub version: u32,
    pub control_api_url: String,
    pub oauth: DiscoveryOAuth,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DiscoveryOAuth {
    pub issuer: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Validate a public deployment URL. Plain HTTP is intentionally limited to
/// loopback so first-run setup cannot silently downgrade credentials.
pub fn validate_deployment_url(value: &str, label: &str) -> Result<reqwest::Url, GhError> {
    let url = reqwest::Url::parse(value.trim())
        .map_err(|error| GhError::config(format!("invalid {label}: {error}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| GhError::config(format!("{label} must have a host")))?;
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback(host)) {
        return Err(GhError::config(format!(
            "{label} must use HTTPS (plain HTTP is allowed only for loopback development)"
        )));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(GhError::config(format!(
            "{label} cannot contain credentials, a query, or a fragment"
        )));
    }
    Ok(url)
}

pub fn discover(deployment: &str) -> Result<DiscoveryDocument, GhError> {
    let base = validate_deployment_url(deployment, "Control API URL")?;
    let endpoint = base
        .join("/.well-known/metaharness")
        .map_err(|error| GhError::config(format!("invalid discovery endpoint: {error}")))?;
    let response = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| GhError::service(format!("building discovery client: {error}")))?
        .get(endpoint.clone())
        .send()
        .map_err(|error| GhError::service(format!("contacting {endpoint}: {error}")))?;
    if !response.status().is_success() {
        return Err(GhError::service(format!(
            "{endpoint} returned HTTP {}",
            response.status()
        )));
    }
    let document: DiscoveryDocument = response
        .json()
        .map_err(|error| GhError::service(format!("invalid discovery document: {error}")))?;
    validate_document(&document)?;
    Ok(document)
}

pub fn validate_document(document: &DiscoveryDocument) -> Result<(), GhError> {
    if document.version != 1 {
        return Err(GhError::service(format!(
            "unsupported metaharness discovery version {}",
            document.version
        )));
    }
    validate_deployment_url(&document.control_api_url, "canonical Control API URL")?;
    validate_deployment_url(&document.oauth.issuer, "OAuth issuer")?;
    if document.oauth.client_id.trim().is_empty() {
        return Err(GhError::service("discovery OAuth client_id is empty"));
    }
    if document
        .oauth
        .scopes
        .iter()
        .any(|scope| scope.trim().is_empty())
    {
        return Err(GhError::service(
            "discovery OAuth scopes contain an empty value",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn permits_secure_and_loopback_urls() {
        assert!(validate_deployment_url("https://control.example.com", "URL").is_ok());
        assert!(validate_deployment_url("http://127.0.0.1:8080", "URL").is_ok());
        assert!(validate_deployment_url("http://localhost:8080", "URL").is_ok());
    }

    #[test]
    fn rejects_remote_plain_http_and_secrets() {
        assert!(validate_deployment_url("http://control.example.com", "URL").is_err());
        assert!(validate_deployment_url("https://user:pass@example.com", "URL").is_err());
    }

    #[test]
    fn rejects_unknown_versions() {
        let document = DiscoveryDocument {
            version: 2,
            control_api_url: "https://control.example.com".into(),
            oauth: DiscoveryOAuth {
                issuer: "https://auth.example.com".into(),
                client_id: "cli".into(),
                scopes: vec![],
            },
        };
        assert!(validate_document(&document).is_err());
    }

    #[test]
    fn fetches_well_known_document() {
        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            // Some hermetic test runners prohibit even loopback sockets.
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("binding mock discovery server: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let size = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..size])
                .starts_with("GET /.well-known/metaharness "));
            let body = format!(
                r#"{{"version":1,"control_api_url":"http://{address}","oauth":{{"issuer":"http://127.0.0.1:3000/api/auth","client_id":"blue-cli","scopes":["openid"]}}}}"#
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            )
            .unwrap();
        });
        let document = discover(&format!("http://{address}/api")).unwrap();
        assert_eq!(document.version, 1);
        assert_eq!(document.oauth.client_id, "blue-cli");
        server.join().unwrap();
    }
}
