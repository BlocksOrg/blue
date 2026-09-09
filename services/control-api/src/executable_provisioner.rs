use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use gh_gateway_provisioner::{
    EnsureRequest, GatewayProvisioner, ProvisionedCredential, ProvisionerError, RevokeRequest,
    RevokeResponse,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const PROTOCOL_VERSION: u8 = 1;
const MAX_STDOUT: u64 = 1024 * 1024;
const MAX_STDERR: u64 = 16 * 1024;

pub struct ExecutableGatewayProvisioner {
    path: PathBuf,
    kind: String,
    policy_revision: String,
    timeout: Duration,
}

#[derive(Serialize)]
struct RequestEnvelope<T> {
    protocol_version: u8,
    operation: &'static str,
    request: T,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum ResponseEnvelope<T> {
    Success {
        protocol_version: u8,
        result: T,
    },
    Error {
        protocol_version: u8,
        error: WireError,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireError {
    code: String,
    message: String,
}

impl ExecutableGatewayProvisioner {
    pub async fn load(config: &crate::GatewayProvisionerConfig) -> Result<Self, String> {
        let path = config
            .executable_path
            .as_ref()
            .ok_or_else(|| "gateway.provisioner.executable_path is required".to_owned())?;
        if !path.is_absolute() {
            return Err("gateway.provisioner.executable_path must be absolute".into());
        }
        let expected = config
            .executable_sha256
            .as_deref()
            .ok_or_else(|| "gateway.provisioner.executable_sha256 is required".to_owned())?;
        if expected.len() != 64
            || !expected
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(
                "gateway.provisioner.executable_sha256 must be 64 lowercase hexadecimal characters"
                    .into(),
            );
        }
        let revision = config
            .policy_revision
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| "gateway.provisioner.policy_revision must not be empty".to_owned())?;
        if config.timeout_seconds == 0 {
            return Err("gateway.provisioner.timeout_seconds must be greater than zero".into());
        }
        let metadata = tokio::fs::metadata(path).await.map_err(|e| {
            format!(
                "failed to inspect provisioner executable {}: {e}",
                path.display()
            )
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "provisioner executable {} must be a regular file",
                path.display()
            ));
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "provisioner executable {} is not executable",
                path.display()
            ));
        }
        let bytes = tokio::fs::read(path).await.map_err(|e| {
            format!(
                "failed to read provisioner executable {}: {e}",
                path.display()
            )
        })?;
        let actual = hex::encode(Sha256::digest(bytes));
        if actual != expected {
            return Err(format!(
                "provisioner executable SHA-256 mismatch for {}: expected {expected}, got {actual}",
                path.display()
            ));
        }
        Ok(Self {
            path: path.clone(),
            kind: config.kind.clone(),
            policy_revision: revision.to_owned(),
            timeout: Duration::from_secs(config.timeout_seconds),
        })
    }

    async fn invoke<I: Serialize, O: DeserializeOwned>(
        &self,
        operation: &'static str,
        request: I,
    ) -> Result<O, ProvisionerError> {
        let input = serde_json::to_vec(&RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            operation,
            request,
        })
        .map_err(|e| ProvisionerError::InvalidConfig(e.to_string()))?;
        let mut child = Command::new(&self.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| unavailable(format!("failed to start provisioner executable: {e}")))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let io = async move {
            let write = async move {
                stdin.write_all(&input).await?;
                stdin.shutdown().await?;
                Ok::<_, std::io::Error>(())
            };
            let (write_result, stdout, stderr) = tokio::join!(
                write,
                read_bounded(stdout, MAX_STDOUT as usize),
                read_bounded(stderr, MAX_STDERR as usize)
            );
            Ok::<_, std::io::Error>((write_result, stdout?, stderr?))
        };
        let outcome =
            tokio::time::timeout(self.timeout, async { tokio::try_join!(child.wait(), io) }).await;
        let (status, (write_result, stdout, stderr)) = match outcome {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => return Err(unavailable(format!("provisioner process I/O failed: {e}"))),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(unavailable(format!(
                    "provisioner invocation exceeded {} seconds",
                    self.timeout.as_secs()
                )));
            }
        };
        if let Err(error) = write_result {
            if stdout.is_empty() {
                return Err(unavailable(format!(
                    "failed to send request to provisioner process: {error}"
                )));
            }
            tracing::debug!(%error, "provisioner closed stdin before consuming the request");
        }
        if !stderr.is_empty() {
            tracing::warn!(bytes = stderr.len(), "provisioner wrote to stderr");
        }
        if stdout.len() > MAX_STDOUT as usize {
            return Err(unavailable("provisioner stdout exceeded limit"));
        }
        let mut deserializer = serde_json::Deserializer::from_slice(&stdout);
        let envelope = ResponseEnvelope::<O>::deserialize(&mut deserializer)
            .map_err(|_| unavailable("provisioner returned invalid JSON"))?;
        deserializer
            .end()
            .map_err(|_| unavailable("provisioner returned multiple JSON documents"))?;
        let protocol_version = match &envelope {
            ResponseEnvelope::Success {
                protocol_version, ..
            }
            | ResponseEnvelope::Error {
                protocol_version, ..
            } => *protocol_version,
        };
        if protocol_version != PROTOCOL_VERSION {
            return Err(unavailable("provisioner protocol version mismatch"));
        }
        match (status.success(), envelope) {
            (true, ResponseEnvelope::Success { result, .. }) => Ok(result),
            (false, ResponseEnvelope::Error { error, .. }) => Err(map_error(error)),
            (true, ResponseEnvelope::Error { .. }) => Err(unavailable(
                "provisioner exited successfully with an error response",
            )),
            // The only remaining combination is a non-zero exit paired with
            // a success envelope. Keep this wildcard reachable so future
            // envelope changes also fail closed instead of making the match
            // non-exhaustive.
            _ => Err(unavailable(
                "provisioner exited unsuccessfully with a success response",
            )),
        }
    }
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut captured = Vec::with_capacity(limit.min(8192));
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_add(1).saturating_sub(captured.len());
        captured.extend_from_slice(&chunk[..count.min(remaining)]);
    }
    Ok(captured)
}

fn unavailable(message: impl Into<String>) -> ProvisionerError {
    ProvisionerError::Unavailable(message.into())
}
fn map_error(error: WireError) -> ProvisionerError {
    match error.code.as_str() {
        "invalid_config" => ProvisionerError::InvalidConfig(error.message),
        "account_missing" => ProvisionerError::AccountMissing(error.message),
        "conflict" => ProvisionerError::Conflict(error.message),
        "credential_invalid" => ProvisionerError::CredentialInvalid(error.message),
        "unavailable" => ProvisionerError::Unavailable(error.message),
        "rejected" => ProvisionerError::Rejected(error.message),
        _ => unavailable("provisioner returned an unsupported error code"),
    }
}

#[async_trait]
impl GatewayProvisioner for ExecutableGatewayProvisioner {
    fn kind(&self) -> &str {
        &self.kind
    }
    fn policy_revision(&self) -> Option<&str> {
        Some(&self.policy_revision)
    }
    async fn ensure(
        &self,
        request: EnsureRequest,
    ) -> Result<ProvisionedCredential, ProvisionerError> {
        let result: ProvisionedCredential = self.invoke("ensure", request).await?;
        if result.expires_at.as_deref().is_some_and(|value| {
            time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                .is_err()
        }) {
            return Err(unavailable(
                "provisioner returned an invalid RFC 3339 expires_at",
            ));
        }
        Ok(result)
    }
    async fn revoke(&self, request: RevokeRequest) -> Result<RevokeResponse, ProvisionerError> {
        self.invoke("revoke", request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_gateway_provisioner::{EnsureReason, UserIdentity};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn script(contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("blue-provisioner-{}", uuid::Uuid::new_v4()));
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn provisioner(path: PathBuf) -> ExecutableGatewayProvisioner {
        ExecutableGatewayProvisioner {
            path,
            kind: "test".into(),
            policy_revision: "v1".into(),
            timeout: Duration::from_secs(2),
        }
    }

    fn ensure_request() -> EnsureRequest {
        EnsureRequest {
            identity: UserIdentity {
                id: "u1".into(),
                email: "u@example.com".into(),
                organization_id: "o1".into(),
                groups: vec!["dev".into()],
            },
            reason: EnsureReason::Missing,
            previous: None,
        }
    }

    #[tokio::test]
    async fn executes_shebang_and_sends_versioned_request() {
        let path = script("#!/bin/sh\ninput=$(cat)\ncase \"$input\" in *'\"protocol_version\":1'*'\"operation\":\"ensure\"'*'u@example.com'*) printf '%s' '{\"protocol_version\":1,\"status\":\"success\",\"result\":{\"credential\":\"secret\",\"external_id\":\"x1\",\"alias\":\"alias\",\"metadata\":{},\"expires_at\":\"2026-12-01T00:00:00Z\"}}';; *) exit 2;; esac\n");
        let result = provisioner(path.clone())
            .ensure(ensure_request())
            .await
            .unwrap();
        assert_eq!(result.credential.unwrap().expose(), "secret");
        assert_eq!(result.expires_at.as_deref(), Some("2026-12-01T00:00:00Z"));
        fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn maps_typed_nonzero_error_and_rejects_exit_mismatch() {
        let path = script("#!/bin/sh\nprintf '%s' '{\"protocol_version\":1,\"status\":\"error\",\"error\":{\"code\":\"conflict\",\"message\":\"duplicate\"}}'\nexit 255\n");
        let error = provisioner(path.clone())
            .ensure(ensure_request())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ProvisionerError::Conflict(message) if message == "duplicate"),
            "unexpected error: {error}"
        );
        fs::remove_file(path).unwrap();

        let path = script("#!/bin/sh\nprintf '%s' '{\"protocol_version\":1,\"status\":\"success\",\"result\":{\"credential\":null,\"external_id\":\"x\",\"alias\":\"a\",\"expires_at\":null}}'\nexit 1\n");
        let error = provisioner(path.clone())
            .ensure(ensure_request())
            .await
            .unwrap_err();
        assert!(matches!(error, ProvisionerError::Unavailable(_)));
        fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn validates_digest_regular_file_permissions_and_revision() {
        let path = script("#!/bin/sh\nexit 0\n");
        let digest = hex::encode(Sha256::digest(fs::read(&path).unwrap()));
        let config = crate::GatewayProvisionerConfig {
            kind: "custom".into(),
            reconcile_ttl_seconds: 60,
            executable_path: Some(path.clone()),
            executable_sha256: Some(digest),
            policy_revision: Some("v1".into()),
            timeout_seconds: 1,
            max_concurrency: 8,
        };
        assert!(ExecutableGatewayProvisioner::load(&config).await.is_ok());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ExecutableGatewayProvisioner::load(&config)
            .await
            .err()
            .unwrap()
            .contains("not executable"));
        fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn rejects_multiple_documents_and_times_out() {
        let path = script("#!/bin/sh\nprintf '{}{}'\n");
        assert!(matches!(
            provisioner(path.clone()).ensure(ensure_request()).await,
            Err(ProvisionerError::Unavailable(_))
        ));
        fs::remove_file(path).unwrap();

        let path = script(
            "#!/bin/sh\nprintf '%s' '{\"protocol_version\":2,\"status\":\"success\",\"result\":{\"credential\":null,\"external_id\":\"x\",\"alias\":\"a\",\"expires_at\":null}}'\n",
        );
        let error = provisioner(path.clone())
            .ensure(ensure_request())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ProvisionerError::Unavailable(message) if message.contains("protocol version mismatch")),
            "unexpected error: {error}"
        );
        fs::remove_file(path).unwrap();

        let path = script("#!/bin/sh\nsleep 5\n");
        let mut host = provisioner(path.clone());
        host.timeout = Duration::from_millis(50);
        assert!(matches!(
            host.ensure(ensure_request()).await,
            Err(ProvisionerError::Unavailable(_))
        ));
        fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn rejects_oversized_output_and_signal_termination() {
        let path = script("#!/bin/sh\nyes x | head -c 1100000\n");
        assert!(matches!(
            provisioner(path.clone()).ensure(ensure_request()).await,
            Err(ProvisionerError::Unavailable(_))
        ));
        fs::remove_file(path).unwrap();
        let path = script("#!/bin/sh\nkill -TERM $$\n");
        assert!(matches!(
            provisioner(path.clone()).ensure(ensure_request()).await,
            Err(ProvisionerError::Unavailable(_))
        ));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn supports_all_public_error_codes() {
        let cases = [
            ("invalid_config", 400),
            ("account_missing", 403),
            ("conflict", 409),
            ("credential_invalid", 409),
            ("unavailable", 502),
            ("rejected", 502),
        ];
        for (code, expected_status) in cases {
            let error = map_error(WireError {
                code: code.into(),
                message: "public".into(),
            });
            let actual = match error {
                ProvisionerError::InvalidConfig(message) => {
                    assert_eq!(message, "public");
                    400
                }
                ProvisionerError::AccountMissing(message) => {
                    assert_eq!(message, "public");
                    403
                }
                ProvisionerError::CredentialInvalid(message) => {
                    assert_eq!(message, "public");
                    409
                }
                ProvisionerError::Conflict(message) => {
                    assert_eq!(message, "public");
                    409
                }
                ProvisionerError::Unavailable(message) | ProvisionerError::Rejected(message) => {
                    assert_eq!(message, "public");
                    502
                }
            };
            assert_eq!(actual, expected_status);
        }
    }
}
