use anyhow::Result;
use serde::Deserialize;

use super::helpers::capitalize;
use super::{UsageMetric, UsageOutput};

const BETA_HEADER: &str = "oauth-2025-04-20";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

// `~/.claude/.credentials.json` belongs to Claude Code, and this module is a
// quota viewer: it reads that file and never writes it. Tokscale used to
// exchange Claude Code's refresh token on 401/403 and write the result back,
// but the write reconstructed the document from the four fields below, dropping
// every field tokscale does not model -- `expiresAt` and `scopes` among them --
// which left Claude Code reporting "Not logged in" (#1001). An expired access
// token is Claude Code's to refresh on its next run, so a rejected token is
// reported as unavailable usage instead.

#[derive(Debug, Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<Oauth>,
}

/// Deliberately does not model `refreshToken`: tokscale has no use for a
/// credential it must not spend.
#[derive(Debug, Deserialize)]
struct Oauth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
    seven_day_opus: Option<Window>,
}

#[derive(Debug, Deserialize)]
struct Window {
    utilization: f64,
    resets_at: Option<String>,
}

fn credentials_path() -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".claude").join(".credentials.json")
}

fn read_keychain() -> Result<String> {
    super::helpers::read_keychain("Claude Code-credentials")
}

pub fn has_credentials() -> bool {
    credentials_path().exists() || read_keychain().is_ok()
}

fn read_credentials() -> Result<Credentials> {
    let path = credentials_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(creds) = serde_json::from_str::<Credentials>(&content) {
                return Ok(creds);
            }
        }
    }
    let content = read_keychain()?;
    Ok(serde_json::from_str(&content)?)
}

async fn fetch_usage(
    client: &reqwest::Client,
    usage_url: &str,
    token: &str,
) -> Result<UsageResponse> {
    let resp = client
        .get(usage_url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("anthropic-beta", BETA_HEADER)
        .send()
        .await?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!(
            "Claude usage unavailable: stored access token was rejected (HTTP {status}). \
             Run 'claude' so Claude Code can refresh its own login, then retry."
        );
    }
    if !status.is_success() {
        anyhow::bail!("Claude usage request failed (HTTP {status})");
    }
    Ok(resp.json().await?)
}

fn window_metric(label: &str, w: &Window) -> UsageMetric {
    let used = w.utilization.clamp(0.0, 100.0);
    UsageMetric {
        label: label.into(),
        used_percent: used,
        remaining_percent: 100.0 - used,
        remaining_label: None,
        resets_at: w.resets_at.clone(),
    }
}

async fn fetch_with_endpoint(
    client: &reqwest::Client,
    usage_url: &str,
    oauth: &Oauth,
) -> Result<UsageOutput> {
    let access_token = oauth
        .access_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("No Claude access token."))?;
    let plan = oauth.subscription_type.as_ref().map(|s| {
        let tier = oauth
            .rate_limit_tier
            .as_deref()
            .and_then(|t| t.rsplit('_').next());
        match tier {
            Some(mult) => format!("{} {}", capitalize(s), mult),
            None => capitalize(s),
        }
    });

    let resp = fetch_usage(client, usage_url, access_token).await?;

    let mut metrics = Vec::new();
    if let Some(ref w) = resp.five_hour {
        metrics.push(window_metric("Session", w));
    }
    if let Some(ref w) = resp.seven_day {
        metrics.push(window_metric("Weekly", w));
    }
    if let Some(ref w) = resp.seven_day_opus {
        metrics.push(window_metric("Opus", w));
    }

    Ok(UsageOutput {
        provider: "Claude".into(),
        account: None,
        plan,
        email: None,
        metrics,
        reset_credits: None,
        credit_status: None,
        spend_control: None,
    })
}

pub fn fetch() -> Result<UsageOutput> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let creds = read_credentials()?;
        let oauth = creds.claude_ai_oauth.ok_or_else(|| {
            anyhow::anyhow!("No Claude OAuth credentials. Run 'claude' to log in.")
        })?;
        let client = reqwest::Client::new();
        fetch_with_endpoint(&client, USAGE_URL, &oauth).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A Claude Code credential document with the fields tokscale models
    /// (`accessToken`, `subscriptionType`, `rateLimitTier`), the fields it does
    /// not (`refreshToken`, `expiresAt`, `scopes`), and a key that does not
    /// exist today -- Claude Code owns the schema and may add more.
    const FIXTURE: &str = r#"{
  "claudeAiOauth": {
    "accessToken": "stale-access-token",
    "refreshToken": "claude-code-owned-refresh-token",
    "expiresAt": 1757000000000,
    "scopes": ["user:inference", "user:profile"],
    "subscriptionType": "max",
    "rateLimitTier": "default_max_20x"
  },
  "someKeyTokscaleDoesNotModel": { "keep": true }
}"#;

    const USAGE_BODY: &str =
        r#"{"five_hour":{"utilization":12.5,"resets_at":"2026-08-03T12:00:00Z"}}"#;

    fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            401 => "Unauthorized",
            403 => "Forbidden",
            _ => "Unknown",
        }
    }

    /// Blocking HTTP/1.1 server on an ephemeral port that answers by request
    /// path. `Connection: close` keeps one request per socket so the accept
    /// loop stays trivial. The thread is left blocked on `accept` when the test
    /// ends; the test process tears it down.
    fn spawn_server(routes: Vec<(String, u16, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while let Ok(n) = stream.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&buf).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, body) = routes
                    .iter()
                    .find(|(p, _, _)| *p == path)
                    .map(|(_, s, b)| (*s, b.clone()))
                    .unwrap_or_else(|| (404, "{}".to_string()));
                let response = format!(
                    "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    reason(status),
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    struct HomeGuard {
        previous: Option<std::ffi::OsString>,
        dir: std::path::PathBuf,
    }

    impl HomeGuard {
        fn new(name: &str) -> Self {
            let previous = std::env::var_os("HOME");
            let dir = std::env::temp_dir().join(format!(
                "tokscale-claude-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".claude")).expect("create fake home");
            unsafe {
                std::env::set_var("HOME", &dir);
            }
            Self { previous, dir }
        }

        fn credentials(&self) -> std::path::PathBuf {
            self.dir.join(".claude").join(".credentials.json")
        }

        fn write_fixture(&self) {
            std::fs::write(self.credentials(), FIXTURE).expect("write fixture credentials");
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn oauth_from_fixture() -> Oauth {
        serde_json::from_str::<Credentials>(FIXTURE)
            .expect("fixture parses")
            .claude_ai_oauth
            .expect("fixture has claudeAiOauth")
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(f)
    }

    fn routes(usage_status: u16) -> Vec<(String, u16, String)> {
        vec![
            (
                "/api/oauth/usage".to_string(),
                usage_status,
                USAGE_BODY.to_string(),
            ),
            // The endpoint the removed refresh path used to POST to. Serving it
            // locally means a regression reaches it here instead of reaching
            // platform.claude.com, and the assertions below still catch it.
            (
                "/v1/oauth/token".to_string(),
                200,
                r#"{"access_token":"rotated-access-token","refresh_token":"rotated-refresh-token"}"#
                    .to_string(),
            ),
        ]
    }

    /// #1001: a rejected access token must not make tokscale rewrite Claude
    /// Code's credential file. Byte equality is the assertion that matters --
    /// any reconstruction of the document fails it, whatever fields it keeps.
    #[test]
    #[serial_test::serial]
    fn rejected_token_leaves_claude_credentials_untouched() {
        let home = HomeGuard::new("401");
        home.write_fixture();
        let before = std::fs::read(home.credentials()).expect("read before");
        let base = spawn_server(routes(401));

        let result = block_on(async {
            let client = reqwest::Client::new();
            fetch_with_endpoint(
                &client,
                &format!("{base}/api/oauth/usage"),
                &oauth_from_fixture(),
            )
            .await
        });

        let err = result.expect_err("401 must surface as an error, not a refresh");
        assert!(
            err.to_string().contains("Run 'claude'"),
            "error should point at Claude Code's own login, got: {err}"
        );

        let after = std::fs::read(home.credentials()).expect("credentials must still exist");
        assert_eq!(
            String::from_utf8_lossy(&after),
            String::from_utf8_lossy(&before),
            "tokscale rewrote Claude Code's credential file"
        );
    }

    /// A 403 takes the same branch as a 401 and must be just as inert.
    #[test]
    #[serial_test::serial]
    fn forbidden_response_leaves_claude_credentials_untouched() {
        let home = HomeGuard::new("403");
        home.write_fixture();
        let before = std::fs::read(home.credentials()).expect("read before");
        let base = spawn_server(routes(403));

        let result = block_on(async {
            let client = reqwest::Client::new();
            fetch_with_endpoint(
                &client,
                &format!("{base}/api/oauth/usage"),
                &oauth_from_fixture(),
            )
            .await
        });

        assert!(result.is_err(), "403 must surface as an error");
        let after = std::fs::read(home.credentials()).expect("credentials must still exist");
        assert_eq!(
            String::from_utf8_lossy(&after),
            String::from_utf8_lossy(&before),
            "tokscale rewrote Claude Code's credential file"
        );
    }

    /// On macOS the credentials live in the Keychain and the file does not
    /// exist. Tokscale must not conjure a partial one.
    #[test]
    #[serial_test::serial]
    fn rejected_token_does_not_create_a_credential_file() {
        let home = HomeGuard::new("nofile");
        let base = spawn_server(routes(401));

        let result = block_on(async {
            let client = reqwest::Client::new();
            fetch_with_endpoint(
                &client,
                &format!("{base}/api/oauth/usage"),
                &oauth_from_fixture(),
            )
            .await
        });

        assert!(result.is_err(), "401 must surface as an error");
        assert!(
            !home.credentials().exists(),
            "tokscale created a credential file Claude Code did not have"
        );
    }

    #[test]
    #[serial_test::serial]
    fn successful_usage_fetch_leaves_claude_credentials_untouched() {
        let home = HomeGuard::new("200");
        home.write_fixture();
        let before = std::fs::read(home.credentials()).expect("read before");
        let base = spawn_server(routes(200));

        let output = block_on(async {
            let client = reqwest::Client::new();
            fetch_with_endpoint(
                &client,
                &format!("{base}/api/oauth/usage"),
                &oauth_from_fixture(),
            )
            .await
        })
        .expect("200 usage response should parse");

        assert_eq!(output.plan.as_deref(), Some("Max 20x"));
        assert_eq!(output.metrics.len(), 1);
        assert_eq!(output.metrics[0].label, "Session");
        assert!((output.metrics[0].used_percent - 12.5).abs() < f64::EPSILON);

        let after = std::fs::read(home.credentials()).expect("credentials must still exist");
        assert_eq!(
            String::from_utf8_lossy(&after),
            String::from_utf8_lossy(&before),
            "tokscale rewrote Claude Code's credential file"
        );
    }
}
