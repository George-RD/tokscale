use super::litellm::ModelPricing;
use super::{cache, describe_error, fetch};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

const CACHE_FILENAME: &str = "pricing-openrouter.json";
const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
const MAX_CONCURRENT_REQUESTS: usize = 10;

/// Structs for `/api/v1/models` endpoint (list all models).

#[derive(Deserialize)]
struct ModelListPricing {
    prompt: String,
    completion: String,
}

#[derive(Deserialize)]
struct ModelListItem {
    id: String,
    pricing: Option<ModelListPricing>,
}

#[derive(Deserialize)]
struct ModelsListResponse {
    data: Vec<ModelListItem>,
}

/// Structs for `/api/v1/models/{id}/endpoints` endpoint (author pricing).

#[derive(Deserialize)]
struct EndpointPricing {
    prompt: String,
    completion: String,
    #[serde(default)]
    input_cache_read: Option<String>,
    #[serde(default)]
    input_cache_write: Option<String>,
}

#[derive(Deserialize)]
struct Endpoint {
    provider_name: String,
    pricing: EndpointPricing,
}

#[derive(Deserialize)]
struct EndpointData {
    #[allow(dead_code)]
    id: String,
    endpoints: Vec<Endpoint>,
}

#[derive(Deserialize)]
struct EndpointsResponse {
    data: EndpointData,
}

/// Model ID prefix to provider name mapping.
///
/// Translates model ID prefixes like `z-ai` to their corresponding
/// provider names in the endpoints API, such as `Z.AI`.
fn get_author_provider_name(model_id: &str) -> Option<&'static str> {
    let prefix = model_id.split('/').next()?;

    match prefix.to_lowercase().as_str() {
        "z-ai" => Some("Z.AI"),
        "x-ai" => Some("xAI"),
        "anthropic" => Some("Anthropic"),
        "openai" => Some("OpenAI"),
        "google" => Some("Google"),
        "meta-llama" => Some("Meta"),
        "mistralai" => Some("Mistral"),
        "deepseek" => Some("DeepSeek"),
        "qwen" => Some("Alibaba"),
        "cohere" => Some("Cohere"),
        "perplexity" => Some("Perplexity"),
        "moonshotai" => Some("Moonshot AI"),
        _ => None,
    }
}

pub fn load_cached() -> Option<HashMap<String, ModelPricing>> {
    cache::load_cache(CACHE_FILENAME)
}

pub fn load_cached_any_age() -> Option<HashMap<String, ModelPricing>> {
    cache::load_cache_any_age(CACHE_FILENAME)
}

fn parse_price(s: &str) -> Option<f64> {
    s.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

async fn fetch_author_pricing(
    client: Arc<reqwest::Client>,
    model_id: String,
    semaphore: Arc<Semaphore>,
    fallback_pricing: Option<ModelPricing>,
) -> Option<(String, ModelPricing)> {
    let _permit = semaphore.acquire().await.ok()?;

    let author_name = match get_author_provider_name(&model_id) {
        Some(name) => name,
        None => return fallback_pricing.map(|p| (model_id, p)),
    };

    let url = format!("https://openrouter.ai/api/v1/models/{}/endpoints", model_id);

    let response = match client
        .get(&url)
        .header("Content-Type", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            return fallback_pricing.map(|p| (model_id, p));
        }
    };

    if !response.status().is_success() {
        return fallback_pricing.map(|p| (model_id, p));
    }

    let data: EndpointsResponse = match response.json().await {
        Ok(d) => d,
        Err(_) => {
            return fallback_pricing.map(|p| (model_id, p));
        }
    };

    // Find the endpoint from the author provider
    let author_endpoint = match data
        .data
        .endpoints
        .iter()
        .find(|e| e.provider_name == author_name)
    {
        Some(ep) => ep,
        None => {
            return fallback_pricing.map(|p| (model_id, p));
        }
    };

    let input_cost = parse_price(&author_endpoint.pricing.prompt);
    let output_cost = parse_price(&author_endpoint.pricing.completion);

    if input_cost.is_none() || output_cost.is_none() {
        return fallback_pricing.map(|p| (model_id, p));
    }

    let pricing = ModelPricing {
        input_cost_per_token: input_cost,
        output_cost_per_token: output_cost,
        cache_read_input_token_cost: author_endpoint
            .pricing
            .input_cache_read
            .as_ref()
            .and_then(|s| parse_price(s)),
        cache_creation_input_token_cost: author_endpoint
            .pricing
            .input_cache_write
            .as_ref()
            .and_then(|s| parse_price(s)),
        ..Default::default()
    };

    Some((model_id, pricing))
}

/// Fetch all models and get author pricing for each
pub async fn fetch_all_models() -> Result<HashMap<String, ModelPricing>, String> {
    fetch_all_models_from_url(MODELS_URL, true).await
}

async fn fetch_all_models_from_url(
    models_url: &str,
    use_cache: bool,
) -> Result<HashMap<String, ModelPricing>, String> {
    if use_cache {
        if let Some(cached) = load_cached() {
            return Ok(cached);
        }
    }

    let client = Arc::new(fetch::pricing_client()?);
    let response = fetch::get_with_retry(&client, models_url, "OpenRouter").await?;
    let data: ModelsListResponse = response.json().await.map_err(|error| {
        format!(
            "OpenRouter models JSON parse failed: {}",
            describe_error(&error)
        )
    })?;
    let models_with_fallback: Vec<(String, Option<ModelPricing>)> = data
        .data
        .into_iter()
        .map(|m| {
            let fallback = m.pricing.and_then(|p| {
                let input = parse_price(&p.prompt)?;
                let output = parse_price(&p.completion)?;
                Some(ModelPricing {
                    input_cost_per_token: Some(input),
                    output_cost_per_token: Some(output),
                    cache_read_input_token_cost: None,
                    cache_creation_input_token_cost: None,
                    ..Default::default()
                })
            });
            (m.id, fallback)
        })
        .collect();

    if models_with_fallback.is_empty() {
        return Err("OpenRouter returned no models".to_string());
    }

    let models_with_authors: Vec<(String, Option<ModelPricing>)> = models_with_fallback
        .into_iter()
        .filter(|(id, _)| get_author_provider_name(id).is_some())
        .collect();

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));

    let mut handles = Vec::with_capacity(models_with_authors.len());

    for (model_id, fallback) in models_with_authors {
        let client = Arc::clone(&client);
        let sem = Arc::clone(&semaphore);

        let handle =
            tokio::spawn(
                async move { fetch_author_pricing(client, model_id, sem, fallback).await },
            );

        handles.push(handle);
    }

    // Collect results
    let mut result = HashMap::new();

    for handle in handles {
        if let Ok(Some((model_id, pricing))) = handle.await {
            result.insert(model_id, pricing);
        }
    }

    if !result.is_empty() {
        if let Err(e) = cache::save_cache(CACHE_FILENAME, &result) {
            eprintln!(
                "[tokscale] Warning: Failed to cache OpenRouter pricing at {}: {}",
                cache::get_cache_path(CACHE_FILENAME).display(),
                e
            );
        }
    }

    if result.is_empty() {
        return Err("OpenRouter returned no usable pricing rows".to_string());
    }

    Ok(result)
}

pub async fn fetch_all_mapped() -> Result<HashMap<String, ModelPricing>, String> {
    fetch_all_models().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn response_server(status: &'static str, body: &'static str, requests: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for _ in 0..requests {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0; 1024];
                let _ = stream.read(&mut buffer);
                let response = format!(
                    "{status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        url
    }

    #[tokio::test]
    async fn list_status_and_decode_failures_remain_explicit() {
        let status = response_server("HTTP/1.1 503 Service Unavailable", "", 3);
        assert!(fetch_all_models_from_url(&status, false)
            .await
            .unwrap_err()
            .contains("HTTP 503"));

        let malformed = response_server("HTTP/1.1 200 OK", "not json", 1);
        assert!(fetch_all_models_from_url(&malformed, false)
            .await
            .unwrap_err()
            .contains("JSON parse failed"));
    }
}
