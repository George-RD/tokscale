use anyhow::Result;
use serde::Deserialize;

use super::helpers::capitalize;
use super::{UsageMetric, UsageOutput};

#[derive(Debug, Deserialize)]
struct QuotaResp {
    data: Option<QuotaData>,
}

#[derive(Debug, Deserialize)]
struct QuotaData {
    limits: Option<Vec<Limit>>,
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Limit {
    #[serde(rename = "type")]
    limit_type: Option<String>,
    #[allow(dead_code)]
    usage: Option<f64>,
    remaining: Option<f64>,
    percentage: Option<f64>,
    #[allow(dead_code)]
    current_value: Option<f64>,
    number: Option<i64>,
    unit: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SubResp {
    data: Option<Vec<Sub>>,
}

#[derive(Debug, Deserialize)]
struct Sub {
    product_name: Option<String>,
    next_renew_time: Option<String>,
}

async fn fetch_quota(client: &reqwest::Client, key: &str) -> Result<QuotaResp> {
    let resp = client
        .get("https://api.z.ai/api/monitor/usage/quota/limit")
        .header("Authorization", format!("Bearer {key}"))
        .header("Accept", "application/json")
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Z.ai quota request failed (HTTP {})", resp.status());
    }
    Ok(resp.json().await?)
}

async fn fetch_sub(client: &reqwest::Client, key: &str) -> Result<SubResp> {
    let resp = client
        .get("https://api.z.ai/api/biz/subscription/list")
        .header("Authorization", format!("Bearer {key}"))
        .header("Accept", "application/json")
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Z.ai subscription request failed (HTTP {})", resp.status());
    }
    Ok(resp.json().await?)
}

pub fn has_credentials() -> bool {
    std::env::var("ZAI_API_KEY")
        .or_else(|_| std::env::var("GLM_API_KEY"))
        .is_ok()
}

/// Translate Z.ai's limit windows into the session/weekly/web-search metrics
/// tokscale surfaces.
///
/// Z.ai encodes each window as an opaque `(unit, number)` code pair rather than
/// a name: `(3, 5)` is the 5-hour rolling session window and `(6, 1)` is the
/// 1-week window. Unrecognized codes are skipped rather than guessed at.
fn build_metrics(
    limits: &[Limit],
    search_reset: Option<String>,
    session_metric: &mut Option<UsageMetric>,
    weekly_metric: &mut Option<UsageMetric>,
    search_metric: &mut Option<UsageMetric>,
) {
    for limit in limits.iter() {
        // Skip limits with no percentage rather than fabricating
        // "0% used / 100% left" from a missing field.
        let pct = match limit.percentage {
            Some(p) => p.clamp(0.0, 100.0),
            None => continue,
        };

        match limit.limit_type.as_deref() {
            // V3 GLM Coding plans report the same (unit, number)
            // windows as CREDIT_LIMIT instead of TOKENS_LIMIT. Prefer
            // CREDIT_LIMIT so a plan that ever emits both for one
            // window cannot silently last-write-wins.
            Some("TOKENS_LIMIT") | Some("CREDIT_LIMIT") => {
                let prefer = limit.limit_type.as_deref() == Some("CREDIT_LIMIT");
                let (target, label) = match (limit.unit, limit.number) {
                    (Some(3), Some(5)) => (&mut *session_metric, "Session"),
                    (Some(6), Some(1)) => (&mut *weekly_metric, "Weekly"),
                    _ => continue,
                };
                if target.is_none() || prefer {
                    *target = Some(UsageMetric {
                        label: label.to_string(),
                        used_percent: pct,
                        remaining_percent: 100.0 - pct,
                        remaining_label: None,
                        resets_at: None,
                    });
                }
            }
            Some("TIME_LIMIT") => {
                let remaining_label = limit.remaining.map(|r| format!("{:.0} left", r));
                *search_metric = Some(UsageMetric {
                    label: "Web Search".into(),
                    used_percent: pct,
                    remaining_percent: 100.0 - pct,
                    remaining_label,
                    resets_at: search_reset.clone(),
                });
            }
            _ => {}
        }
    }
}

pub fn fetch() -> Result<UsageOutput> {
    let api_key = std::env::var("ZAI_API_KEY")
        .or_else(|_| std::env::var("GLM_API_KEY"))
        .map_err(|_| anyhow::anyhow!("No ZAI_API_KEY or GLM_API_KEY set."))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let client = tokscale_core::http::client();
        let quota = fetch_quota(&client, &api_key).await?;
        let sub = fetch_sub(&client, &api_key).await.ok();

        let plan = sub
            .as_ref()
            .and_then(|s| s.data.as_ref())
            .and_then(|d| d.first())
            .and_then(|s| s.product_name.clone())
            .or_else(|| {
                quota
                    .data
                    .as_ref()
                    .and_then(|d| d.level.clone())
                    .map(|l| capitalize(&l))
            });

        let mut session_metric = None;
        let mut weekly_metric = None;
        let mut search_metric = None;

        let search_reset = sub
            .as_ref()
            .and_then(|s| s.data.as_ref())
            .and_then(|d| d.first())
            .and_then(|s| s.next_renew_time.clone());

        if let Some(limits) = quota.data.as_ref().and_then(|d| d.limits.as_ref()) {
            build_metrics(
                limits,
                search_reset,
                &mut session_metric,
                &mut weekly_metric,
                &mut search_metric,
            );
        }

        let mut metrics = Vec::new();
        if let Some(m) = session_metric {
            metrics.push(m);
        }
        if let Some(m) = weekly_metric {
            metrics.push(m);
        }
        if let Some(m) = search_metric {
            metrics.push(m);
        }

        Ok(UsageOutput {
            provider: "Z.ai".into(),
            account: None,
            credential_source: None,
            plan,
            email: None,
            metrics,
            reset_credits: None,
            credit_status: None,
            spend_control: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(kind: &str, unit: i64, number: i64, percentage: f64) -> Limit {
        serde_json::from_value(serde_json::json!({
            "type": kind,
            "unit": unit,
            "number": number,
            "percentage": percentage,
        }))
        .expect("valid Limit fixture")
    }

    fn run(
        limits: &[Limit],
    ) -> (
        Option<UsageMetric>,
        Option<UsageMetric>,
        Option<UsageMetric>,
    ) {
        let mut session = None;
        let mut weekly = None;
        let mut search = None;
        build_metrics(limits, None, &mut session, &mut weekly, &mut search);
        (session, weekly, search)
    }

    #[test]
    fn credit_limit_session_window_maps_to_session_metric() {
        let (session, weekly, _) = run(&[limit("CREDIT_LIMIT", 3, 5, 40.0)]);
        let session = session.expect("session metric present");
        assert_eq!(session.label, "Session");
        assert_eq!(session.used_percent, 40.0);
        assert_eq!(session.remaining_percent, 60.0);
        assert!(weekly.is_none());
    }

    #[test]
    fn credit_limit_weekly_window_maps_to_weekly_metric() {
        let (session, weekly, _) = run(&[limit("CREDIT_LIMIT", 6, 1, 25.0)]);
        let weekly = weekly.expect("weekly metric present");
        assert_eq!(weekly.label, "Weekly");
        assert_eq!(weekly.used_percent, 25.0);
        assert!(session.is_none());
    }

    #[test]
    fn credit_limit_is_preferred_over_tokens_limit_for_same_window() {
        // TOKENS_LIMIT first, then CREDIT_LIMIT: credit must win.
        let (session, _, _) = run(&[
            limit("TOKENS_LIMIT", 3, 5, 10.0),
            limit("CREDIT_LIMIT", 3, 5, 90.0),
        ]);
        assert_eq!(session.expect("session metric present").used_percent, 90.0);

        // Reversed order: CREDIT_LIMIT must not be clobbered by a later
        // TOKENS_LIMIT for the same window.
        let (session, _, _) = run(&[
            limit("CREDIT_LIMIT", 3, 5, 90.0),
            limit("TOKENS_LIMIT", 3, 5, 10.0),
        ]);
        assert_eq!(session.expect("session metric present").used_percent, 90.0);
    }

    #[test]
    fn unrecognized_window_codes_are_skipped() {
        let (session, weekly, search) = run(&[limit("CREDIT_LIMIT", 9, 9, 50.0)]);
        assert!(session.is_none());
        assert!(weekly.is_none());
        assert!(search.is_none());
    }
}
