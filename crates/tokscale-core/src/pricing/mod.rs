pub mod aliases;
pub mod cache;
pub mod custom;
mod fetch;
pub mod litellm;
pub mod lookup;
pub mod models_dev;
pub mod openrouter;

use custom::CustomPricing;
use lookup::{compute_cost, LookupResult, PricingLookup};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::OnceCell;

use crate::{provider_identity, TokenBreakdown};

pub use litellm::ModelPricing;

static PRICING_SERVICE: OnceCell<Arc<PricingService>> = OnceCell::const_new();

/// Copilot-served models whose only available rates are an upstream vendor's,
/// not GitHub's, and which therefore must resolve to no price at all.
///
/// models.dev's Copilot catalog generally tracks GitHub's own published table —
/// 24 of its 29 `github-copilot/*` entries match it exactly, and
/// `github-copilot/grok-4.5` proves the sourcing by carrying GitHub's $0.50/1M
/// cache-read rate rather than xAI's native $0.30. These two are the exception:
/// they are absent from GitHub's table entirely, and their models.dev rates
/// ($1.75 / $14.00 / $0.175 per 1M) are byte-identical to models.dev's own
/// `openai/gpt-5.2` row. That is an OpenAI passthrough quote, not what a
/// Copilot subscriber is billed under AI Credits.
///
/// Withholding a price is the conservative failure mode, but only because
/// `exclude_generic_unpriced_submission_messages` pairs this refusal with an
/// exclusion arm keyed on the same predicate: the row is dropped from the
/// submission and reported, instead of reaching `validate_priced_messages` and
/// erroring the whole submission. A wrongly priced row, by contrast, silently
/// corrupts a public spend leaderboard. Do not add an id here without checking
/// that pairing still holds — an unpriced id with no exclusion arm blocks
/// submission entirely. This mirrors the reasoning that leaves Sakana's `fugu`
/// router unpriced in `build_sakana_overrides`.
///
/// GitHub's rate table:
/// <https://raw.githubusercontent.com/github/docs/main/data/tables/copilot/models-and-pricing.yml>
/// (accessed 2026-08-05).
const COPILOT_VENDOR_PASSTHROUGH_MODELS: &[&str] = &["gpt-5.2", "gpt-5.2-codex"];

/// Reasoning-effort decorations that `PricingLookup` strips before it resolves
/// a key, listed here so the guard strips exactly the same set.
///
/// `strip_parenthesized_reasoning_tier` handles the `model(high)` spelling;
/// these are the dashed spelling that `normalize_model_name` folds away. If the
/// guard matched the raw id instead, `gpt-5.2-codex(high)` and
/// `gpt-5.2-codex-high` would sail past it and then normalize straight onto the
/// passthrough row — the wrong price by a spelling technicality.
const REASONING_TIER_SUFFIXES: &[&str] = &[
    "-minimal", "-low", "-medium", "-high", "-xhigh", "-auto", "-none",
];

/// Whether the lookup arguments identify one of the Copilot passthrough models
/// above **and** carry enough provenance to know the request is Copilot's.
///
/// Two argument shapes carry that provenance, and both occur:
///
/// 1. `model_id` is namespaced — `github-copilot/gpt-5.2` (models.dev's
///    spelling) or `github_copilot/gpt-5.2` (LiteLLM's). Both are recognized
///    because the two datasets disagree and either can reach a lookup.
/// 2. `model_id` is bare and `provider_id` canonicalizes to `github_copilot`.
///    `tests/fixtures/local_model_ids.txt` lines 73 and 76 are harvested from
///    real sessions and carry exactly this shape for both ids, so it is not
///    hypothetical.
///
/// WHAT THIS DELIBERATELY DOES NOT COVER, and why. The first-party Copilot
/// parsers (`sessions/copilot.rs`, `copilot_vscode.rs`, `copilot_desktop.rs`)
/// derive `provider_id` from the model NAME —
/// `inferred_provider_from_model(&model_id).unwrap_or("github-copilot")` — and
/// that helper answers `openai` for anything containing `gpt`. A Copilot VS
/// Code record for gpt-5.2 therefore reaches this function as exactly
/// `("gpt-5.2", Some("openai"))`: byte-identical to a genuine direct-OpenAI
/// gpt-5.2 record, which must keep its correct $1.75/$14.00 OpenAI rate.
///
/// What actually separates the two cases is which client wrote the session
/// file, and that is information this layer never receives — `model_id` and
/// `provider_id` are the only arguments, and `UnifiedMessage::client` is not
/// among them. So the discriminator cannot be reconstructed here at any level
/// of cleverness; firing on `("gpt-5.2", Some("openai"))` would unprice every
/// direct OpenAI user to catch the Copilot ones. Restoring that shape's
/// provenance is a parser change (stop collapsing Copilot's provider to the
/// upstream vendor), with its own cache-version bump and its own effect on
/// provider attribution, and it belongs in its own commit rather than being
/// smuggled in behind a pricing guard.
///
/// Deliberately narrow otherwise: after tier stripping the terminal segment
/// must EQUAL one of the two ids, so the other 27 `github-copilot/*` entries —
/// and ids like `gpt-5.2-codex-max` that GitHub may yet publish a rate for —
/// keep pricing normally.
pub(crate) fn is_copilot_vendor_passthrough(model_id: &str, provider_id: Option<&str>) -> bool {
    let lower = model_id.trim().to_lowercase();
    let scoped_by_key =
        lower.starts_with("github-copilot/") || lower.starts_with("github_copilot/");
    let terminal = if scoped_by_key {
        lower.split('/').next_back().unwrap_or(&lower)
    } else {
        lower.as_str()
    };

    let copilot_scoped = scoped_by_key
        || provider_id.is_some_and(|provider| {
            provider_identity::canonical_provider(provider).as_deref() == Some("github_copilot")
        });
    if !copilot_scoped {
        return false;
    }

    let base = crate::strip_parenthesized_reasoning_tier(terminal).unwrap_or(terminal);
    let base = REASONING_TIER_SUFFIXES
        .iter()
        .find_map(|suffix| base.strip_suffix(suffix))
        .unwrap_or(base);

    COPILOT_VENDOR_PASSTHROUGH_MODELS.contains(&base)
}

// @keep: explains why we do not just print the error.
/// Flatten an error and its `source()` chain into one line.
///
/// `reqwest::Error`'s `Display` is deliberately terse: a body-decode failure
/// renders as the bare string "error decoding response body", and the
/// `serde_json` cause that names the offending field and byte offset hangs off
/// `source()`, which `{}` never walks. Issue #1002 was reported with exactly
/// that message, which is why it was impossible to tell a transport failure
/// from an upstream schema change and the reporter guessed at TLS. Printing the
/// chain makes the next such report actionable.
pub(crate) fn describe_error(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(inner) = source {
        parts.push(inner.to_string());
        source = inner.source();
    }
    parts.join(": ")
}

pub struct PricingService {
    custom: CustomPricing,
    lookup: PricingLookup,
    github: HashMap<String, ModelPricing>,
}

impl PricingService {
    pub fn new(
        litellm_data: HashMap<String, ModelPricing>,
        openrouter_data: HashMap<String, ModelPricing>,
    ) -> Self {
        Self::new_with_custom(CustomPricing::default(), litellm_data, openrouter_data)
    }

    pub fn new_with_custom(
        custom: CustomPricing,
        litellm_data: HashMap<String, ModelPricing>,
        openrouter_data: HashMap<String, ModelPricing>,
    ) -> Self {
        Self::new_with_custom_and_models_dev(custom, litellm_data, openrouter_data, HashMap::new())
    }

    pub fn new_with_custom_and_models_dev(
        custom: CustomPricing,
        litellm_data: HashMap<String, ModelPricing>,
        openrouter_data: HashMap<String, ModelPricing>,
        models_dev_data: HashMap<String, ModelPricing>,
    ) -> Self {
        Self {
            custom,
            lookup: PricingLookup::new_with_models_dev(
                litellm_data,
                openrouter_data,
                Self::build_cursor_overrides(),
                Self::build_sakana_overrides(),
                Self::filter_models_dev_data(models_dev_data),
            ),
            github: Self::build_github_overrides(),
        }
    }

    /// Drop the Copilot vendor-passthrough rows before they can be indexed.
    ///
    /// The resolution guard in `lookup_with_source_and_provider` is what makes
    /// these ids unpriced; this removes them from the dataset as well so they
    /// cannot leak in sideways. `PricingLookup` builds a model-part index over
    /// models.dev keys, and `gpt-5.2` in that index could otherwise resolve to
    /// `github-copilot/gpt-5.2` for a lookup that never mentions Copilot at
    /// all.
    fn filter_models_dev_data(
        mut data: HashMap<String, ModelPricing>,
    ) -> HashMap<String, ModelPricing> {
        data.retain(|key, _| !is_copilot_vendor_passthrough(key, None));
        data
    }

    // @keep: records why the `github_copilot/` prefix filter that used to live
    // here is gone, so nobody reinstates it from the old "subscription pricing"
    // premise.
    /// Drop LiteLLM rows that publish no usable base rate.
    ///
    /// `github_copilot/` rows were previously discarded wholesale on the theory
    /// that Copilot is subscription-billed at $0.00. That premise expired on
    /// 2026-06-01, when GitHub moved Copilot from premium-request billing to
    /// usage-based AI Credits (1 credit = $0.01) charged at published per-token
    /// rates; the legacy premium-request scheme now covers only annual Pro/Pro+
    /// subscribers who stayed on it.
    ///
    /// The guard had also stopped describing the data. Of the 33
    /// `github_copilot/` rows in LiteLLM's live dataset (accessed 2026-08-05),
    /// 31 carry `input_cost_per_token: null` — not 0.0 — and are dropped by the
    /// retain below regardless of any prefix. The guard's only live effect was
    /// discarding the two rows that DO carry rates,
    /// `github_copilot/mai-code-1-flash` and `github_copilot/mai-code-1-flash-internal`,
    /// both at $0.75/$4.50 per 1M — exactly GitHub's published MAI-Code-1-Flash
    /// rate. It was throwing away the only correct data it touched.
    ///
    /// GitHub's authoritative rate table (prices per 1M tokens):
    /// <https://raw.githubusercontent.com/github/docs/main/data/tables/copilot/models-and-pricing.yml>
    /// rendered at
    /// <https://docs.github.com/en/copilot/reference/copilot-billing/models-and-pricing>
    /// (accessed 2026-08-05).
    fn filter_litellm_data(
        mut data: HashMap<String, ModelPricing>,
    ) -> HashMap<String, ModelPricing> {
        data.retain(|_, pricing| pricing.has_any_usable_base_rate());
        data
    }

    // @keep: Cursor-sourced pricing for models not yet in LiteLLM/OpenRouter.
    // Checked after exact/prefix matches but before fuzzy matching in PricingLookup,
    // so real upstream entries (including provider-prefixed like openai/gpt-5.3-codex)
    // always win. Source citations are required for audit trail.
    fn build_cursor_overrides() -> HashMap<String, ModelPricing> {
        let entries: &[(&str, f64, f64, Option<f64>)] = &[
            // GPT-5.3 family: $1.75/$14.00 per 1M tokens, $0.175 cache read
            // Source: Cursor docs (cursor.com/en-US/docs/models), llm-stats.com
            ("gpt-5.3", 0.00000175, 0.000014, Some(1.75e-7)),
            ("gpt-5.3-codex", 0.00000175, 0.000014, Some(1.75e-7)),
            ("gpt-5.3-codex-spark", 0.00000175, 0.000014, Some(1.75e-7)),
            // Composer 1: $1.25/$10.00 per 1M tokens, $0.125 cache read
            // Source: Cursor docs (cursor.com/docs/models#model-pricing)
            ("composer 1", 0.00000125, 0.00001, Some(1.25e-7)),
            ("composer-1", 0.00000125, 0.00001, Some(1.25e-7)),
            // Composer 1.5: $3.50/$17.50 per 1M tokens, $0.35 cache read
            // Source: Cursor docs (cursor.com/docs/models#model-pricing), issue #276
            ("composer 1.5", 0.0000035, 0.0000175, Some(3.5e-7)),
            ("composer-1.5", 0.0000035, 0.0000175, Some(3.5e-7)),
            // Composer 2: $0.50/$2.50 per 1M input/output, $0.20/M cache read; cache creation free
            // Composer 2 Fast: $1.50/$7.50 per 1M, $0.35/M cache read; cache creation free
            // Source: Cursor docs (cursor.com/docs/models#model-pricing)
            ("composer 2", 5e-7, 2.5e-6, Some(2e-7)),
            ("composer-2", 5e-7, 2.5e-6, Some(2e-7)),
            ("composer 2 fast", 1.5e-6, 7.5e-6, Some(3.5e-7)),
            ("composer-2-fast", 1.5e-6, 7.5e-6, Some(3.5e-7)),
            // Composer 2: $0.50/$2.50 per 1M input/output, $0.20/M cache read; cache creation free
            // Composer 2 Fast: $1.50/$7.50 per 1M, $0.35/M cache read; cache creation free
            // Source: Cursor docs (cursor.com/docs/models#model-pricing)
            ("composer-2.5", 5e-7, 2.5e-6, Some(2e-7)),
            ("composer-2.5-fast", 1.5e-6, 7.5e-6, Some(3.5e-7)),
        ];

        let mut overrides = HashMap::with_capacity(entries.len());
        for (model_id, input, output, cache_read) in entries {
            overrides.insert(
                model_id.to_string(),
                ModelPricing {
                    input_cost_per_token: Some(*input),
                    output_cost_per_token: Some(*output),
                    cache_read_input_token_cost: *cache_read,
                    cache_creation_input_token_cost: None,
                    ..Default::default()
                },
            );
        }
        overrides
    }

    // @keep: Sakana-sourced pricing for `fugu-ultra`, a model not carried by
    // LiteLLM/OpenRouter/models.dev. Reports source label "Sakana" (NOT "Cursor")
    // and is consulted at the same precedence as the Cursor overrides in
    // PricingLookup — after exact/normalized/prefix upstream matches, before the
    // fuzzy stage — so any real upstream entry always wins. The ModelPricing
    // struct is built directly (not via the 4-tuple shorthand) so the >272K
    // long-context tier fields can be populated; compute_cost DOES read those
    // *_above_272k_tokens fields when input/output/cache-read token volume
    // crosses 272K, so they are live, not inert.
    //
    // Rates source: https://console.sakana.ai/pricing and https://sakana.ai/fugu/
    // (accessed 2026-06-22).
    //   fugu-ultra base: input $5/1M, output $30/1M, cache-read $0.50/1M.
    //   fugu-ultra >272K-context tier: input $10/1M, output $45/1M, cache-read $1/1M.
    //
    // NOTE: there is deliberately NO `fugu` (non-ultra) entry. `fugu` is a
    // router/orchestrator billed at "the standard rate of the underlying
    // top-tier model involved" (https://sakana.ai/fugu/, accessed 2026-06-22):
    // the effective rate is variable per request and is NOT recoverable from the
    // session log, which only records model="fugu" with no record of which
    // underlying model actually served the request. Assigning any fixed
    // per-token rate to bare `fugu` would therefore be incorrect, so it is left
    // unpriced (callers fall through to the normal lookup chain / report no price).
    fn build_sakana_overrides() -> HashMap<String, ModelPricing> {
        let mut overrides = HashMap::with_capacity(1);
        overrides.insert(
            "fugu-ultra".to_string(),
            ModelPricing {
                // Base rates.
                input_cost_per_token: Some(5e-6),
                output_cost_per_token: Some(3e-5),
                cache_read_input_token_cost: Some(5e-7),
                cache_creation_input_token_cost: None,
                // >272K-context tier (consumed by compute_cost's tiered walk).
                input_cost_per_token_above_272k_tokens: Some(1e-5),
                output_cost_per_token_above_272k_tokens: Some(4.5e-5),
                cache_read_input_token_cost_above_272k_tokens: Some(1e-6),
                ..Default::default()
            },
        );
        overrides
    }

    // @keep: GitHub-published pricing for a model no upstream dataset carries,
    // plus the provenance caveat on the id -> display-name binding. Both halves
    // are load-bearing; see the ID BINDING note below before trusting the key.
    //
    // `oswe-vscode-prime` is GitHub Copilot's "Raptor mini" (GA, fine-tuned by
    // GitHub). Neither models.dev nor LiteLLM carries it — a search of both live
    // datasets on 2026-08-05 returned zero hits for `oswe` and zero for
    // `raptor` — so without this override every Raptor mini session is unpriced.
    //
    // Rates source: GitHub's own table, verbatim
    // (https://raw.githubusercontent.com/github/docs/main/data/tables/copilot/models-and-pricing.yml,
    // rendered at
    // https://docs.github.com/en/copilot/reference/copilot-billing/models-and-pricing,
    // accessed 2026-08-05):
    //   - model: 'Raptor mini', provider: github, release_status: GA,
    //     input: $0.25, cached_input: $0.025, output: $2.00,
    //     notes: "Uses GPT-5 mini pricing"
    // Per token: input 2.5e-7, output 2e-6, cache read 2.5e-8. GitHub publishes
    // NO cache-write rate, so `cache_creation_input_token_cost` stays None
    // rather than being guessed, and publishes no long-context tiers, so no
    // `*_above_*` field is populated.
    //
    // ID BINDING (confidence: HIGH on the rate, MEDIUM-HIGH on the id).
    // GitHub publishes no model ids anywhere — its table keys on display names
    // only — so the `oswe-vscode-prime` -> "Raptor mini" binding does not come
    // from GitHub. It comes from third-party clients relaying Copilot's own
    // /models API: zed-industries/zed#49514 lists `"model": "oswe-vscode-prime"`
    // under the display name "Raptor mini", and the pi extension
    // WSeubring/pi-extension-raptor-mini hardcodes
    // `const RAPTOR_ID = "oswe-vscode-prime"`. Two independent signals
    // corroborate it: those clients report the model's capability family as
    // `gpt-5-mini`, and GitHub's own note says "Uses GPT-5 mini pricing" at a
    // rate byte-identical to GPT-5 mini's. That is strong but not first-party,
    // and it is recorded here rather than left implicit so the next reader can
    // re-check the binding instead of assuming GitHub documented it. If GitHub
    // ever reassigns the id, this entry misprices silently — revisit it, do not
    // extend it to sibling ids on the strength of this comment.
    //
    // Reports source label "GitHub" (the publisher of the rate) and is
    // consulted only AFTER the entire upstream lookup chain returns nothing, so
    // any real LiteLLM/OpenRouter/models.dev row always wins — strictly more
    // deferential than the Cursor/Sakana overrides, which outrank fuzzy
    // matching.
    fn build_github_overrides() -> HashMap<String, ModelPricing> {
        let mut overrides = HashMap::with_capacity(1);
        overrides.insert(
            "oswe-vscode-prime".to_string(),
            ModelPricing {
                input_cost_per_token: Some(2.5e-7),
                output_cost_per_token: Some(2e-6),
                cache_read_input_token_cost: Some(2.5e-8),
                cache_creation_input_token_cost: None,
                ..Default::default()
            },
        );
        overrides
    }

    /// Exact-match the built-in GitHub overrides, accepting the Copilot-scoped
    /// spelling (`github-copilot/oswe-vscode-prime`, or LiteLLM's
    /// `github_copilot/`) as well as the bare id. Mirrors how `PricingLookup`
    /// matches its Cursor and Sakana overrides.
    ///
    /// The namespaced retry is restricted to the Copilot namespaces on
    /// purpose. Stripping any namespace would hand GitHub's Raptor mini rate —
    /// and the `GitHub` source label, which claims GitHub published this
    /// number for this key — to `openai/oswe-vscode-prime` or
    /// `anthropic/oswe-vscode-prime`, ids GitHub says nothing about. These
    /// rates are only GitHub's within GitHub's own namespace.
    fn lookup_github_override(&self, model_id: &str) -> Option<LookupResult> {
        let lower = model_id.trim().to_lowercase();
        let key = if self.github.contains_key(&lower) {
            lower
        } else {
            let terminal = ["github-copilot/", "github_copilot/"]
                .iter()
                .find_map(|prefix| lower.strip_prefix(prefix))?;
            if !self.github.contains_key(terminal) {
                return None;
            }
            terminal.to_string()
        };

        Some(LookupResult {
            pricing: self.github.get(&key)?.clone(),
            source: "GitHub".into(),
            matched_key: key,
        })
    }

    async fn fetch_inner() -> Result<Self, String> {
        let (litellm_result, openrouter_data, models_dev_result) = tokio::join!(
            litellm::fetch(),
            openrouter::fetch_all_mapped(),
            models_dev::fetch()
        );

        Self::combine_fetched_sources(
            litellm_result,
            openrouter_data,
            models_dev_result,
            litellm::load_cached_any_age,
            openrouter::load_cached_any_age,
            models_dev::load_cached_any_age,
            CustomPricing::load_from_default_path(),
        )
    }

    /// Degrade one failed source to its own stale cache, else to nothing.
    fn degrade_source(
        label: &str,
        result: Result<HashMap<String, ModelPricing>, String>,
        cached: impl FnOnce() -> Option<HashMap<String, ModelPricing>>,
    ) -> HashMap<String, ModelPricing> {
        match result {
            Ok(data) => data,
            Err(error) => {
                let cached = cached();
                eprintln!(
                    "[tokscale] Warning: {} pricing fetch failed ({}); {}",
                    label,
                    error,
                    if cached.is_some() {
                        "falling back to the cached copy"
                    } else {
                        "continuing with the remaining pricing sources"
                    }
                );
                cached.unwrap_or_default()
            }
        }
    }

    // @keep: the asymmetry this removes was load-bearing and non-obvious.
    /// Assemble a service from whatever the three upstream sources returned.
    ///
    /// No single source may be fatal. LiteLLM is the largest dataset, but it is
    /// not the only one, and propagating its fetch error made every command
    /// that prices tokens — `submit` included — dead in the water whenever
    /// raw.githubusercontent.com was unreachable or served something we could
    /// not decode (#1002). Every dynamic source now preserves fetch failure as
    /// an error here, degrades to its own stale cache, and finally to nothing;
    /// the surviving sources still price what they cover. Submission safety is
    /// checked against the actual filtered messages later, rather than treating
    /// an empty dynamic dataset as a construction failure: custom and bundled
    /// pricing remain useful during an outage.
    fn combine_fetched_sources(
        litellm_result: Result<HashMap<String, ModelPricing>, String>,
        openrouter_result: Result<HashMap<String, ModelPricing>, String>,
        models_dev_result: Result<HashMap<String, ModelPricing>, String>,
        litellm_cached: impl FnOnce() -> Option<HashMap<String, ModelPricing>>,
        openrouter_cached: impl FnOnce() -> Option<HashMap<String, ModelPricing>>,
        models_dev_cached: impl FnOnce() -> Option<HashMap<String, ModelPricing>>,
        custom: CustomPricing,
    ) -> Result<Self, String> {
        let litellm_data = Self::filter_litellm_data(Self::degrade_source(
            "LiteLLM",
            litellm_result,
            litellm_cached,
        ));
        let models_dev_data =
            Self::degrade_source("models.dev", models_dev_result, models_dev_cached);
        let openrouter_data =
            Self::degrade_source("OpenRouter", openrouter_result, openrouter_cached);

        Ok(Self::new_with_custom_and_models_dev(
            custom,
            litellm_data,
            openrouter_data,
            models_dev_data,
        ))
    }

    fn from_cached_datasets(
        litellm_data: Option<HashMap<String, ModelPricing>>,
        openrouter_data: Option<HashMap<String, ModelPricing>>,
        models_dev_data: Option<HashMap<String, ModelPricing>>,
    ) -> Option<Self> {
        if litellm_data.is_none() && openrouter_data.is_none() && models_dev_data.is_none() {
            return None;
        }

        Some(Self::new_with_custom_and_models_dev(
            CustomPricing::load_from_default_path(),
            Self::filter_litellm_data(litellm_data.unwrap_or_default()),
            openrouter_data.unwrap_or_default(),
            models_dev_data.unwrap_or_default(),
        ))
    }

    pub fn load_cached_any_age() -> Option<Self> {
        Self::from_cached_datasets(
            litellm::load_cached_any_age(),
            openrouter::load_cached_any_age(),
            models_dev::load_cached_any_age(),
        )
    }

    pub async fn get_or_init() -> Result<Arc<PricingService>, String> {
        PRICING_SERVICE
            .get_or_try_init(|| async { Self::fetch_inner().await.map(Arc::new) })
            .await
            .map(Arc::clone)
    }

    pub fn lookup_with_source(
        &self,
        model_id: &str,
        force_source: Option<&str>,
    ) -> Option<LookupResult> {
        self.lookup_with_source_and_provider(model_id, force_source, None)
    }

    pub fn lookup_with_source_and_provider(
        &self,
        model_id: &str,
        force_source: Option<&str>,
        provider_id: Option<&str>,
    ) -> Option<LookupResult> {
        match force_source {
            Some(source) if source.eq_ignore_ascii_case("custom") => {
                return self.lookup_custom(model_id);
            }
            None => {
                if let Some(result) = self.lookup_custom(model_id) {
                    return Some(result);
                }
            }
            Some(_) => {}
        }

        // Refusing here, rather than only dropping the dataset rows, is what
        // makes the refusal stick: the lookup chain strips an unrecognized
        // `github-copilot/` prefix and retries the terminal segment, so a
        // suppressed row would otherwise fall straight through to the
        // `openai/gpt-5.2` quote it was copied from — the same wrong number by
        // another route. It sits below the custom pass on purpose: a user who
        // writes their own rate for these ids has stated an intent we should
        // not override.
        if is_copilot_vendor_passthrough(model_id, provider_id) {
            return None;
        }

        if let Some(result) =
            self.lookup
                .lookup_with_source_and_provider(model_id, force_source, provider_id)
        {
            return Some(result);
        }

        // Built-in GitHub-published rates are the last resort, so a real
        // upstream row always wins. `force_source` names an upstream dataset,
        // and answering it from a built-in override would misreport where the
        // number came from.
        if force_source.is_none() {
            return self.lookup_github_override(model_id);
        }
        None
    }

    pub fn calculate_cost(
        &self,
        model_id: &str,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
        reasoning: i64,
    ) -> f64 {
        let usage = TokenBreakdown {
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
        };
        self.calculate_cost_with_provider(model_id, None, &usage)
    }

    pub fn calculate_cost_with_provider(
        &self,
        model_id: &str,
        provider_id: Option<&str>,
        usage: &TokenBreakdown,
    ) -> f64 {
        if let Some(result) = self.custom.lookup_with_key(model_id) {
            return compute_cost(
                result.pricing,
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write,
                usage.reasoning,
            );
        }

        if is_copilot_vendor_passthrough(model_id, provider_id) {
            return 0.0;
        }

        if let Some(result) = self.github_override_for_unresolved(model_id, provider_id) {
            return compute_cost(
                &result.pricing,
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write,
                usage.reasoning,
            );
        }

        self.lookup
            .calculate_cost_with_provider(model_id, provider_id, usage)
    }

    pub fn covers_usage_with_provider(
        &self,
        model_id: &str,
        provider_id: Option<&str>,
        usage: &TokenBreakdown,
    ) -> bool {
        if let Some(result) = self.custom.lookup_with_key(model_id) {
            return result.pricing.covers_usage(usage);
        }

        if is_copilot_vendor_passthrough(model_id, provider_id) {
            return false;
        }

        if let Some(result) = self.github_override_for_unresolved(model_id, provider_id) {
            return result.pricing.covers_usage(usage);
        }

        self.lookup
            .covers_usage_with_provider(model_id, provider_id, usage)
    }

    /// The built-in GitHub override for `model_id`, but only when the upstream
    /// chain cannot resolve it at all.
    ///
    /// `calculate_cost_with_provider` and `covers_usage_with_provider` delegate
    /// to `PricingLookup`, which applies cross-row rate borrowing and OpenAI's
    /// full-request tiering on top of a resolution — so the override cannot be
    /// substituted for that path, only consulted when it yields nothing.
    fn github_override_for_unresolved(
        &self,
        model_id: &str,
        provider_id: Option<&str>,
    ) -> Option<LookupResult> {
        if self
            .lookup
            .lookup_with_provider(model_id, provider_id)
            .is_some()
        {
            return None;
        }
        self.lookup_github_override(model_id)
    }

    fn lookup_custom(&self, model_id: &str) -> Option<LookupResult> {
        self.custom
            .lookup_with_key(model_id)
            .map(|result| LookupResult {
                pricing: result.pricing.clone(),
                source: "Custom".into(),
                matched_key: result.matched_key.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_pricing(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input_cost_per_token: Some(input),
            output_cost_per_token: Some(output),
            ..Default::default()
        }
    }

    fn custom_service(
        custom: HashMap<String, ModelPricing>,
        litellm: HashMap<String, ModelPricing>,
        openrouter: HashMap<String, ModelPricing>,
    ) -> PricingService {
        PricingService::new_with_custom(CustomPricing::from_models(custom), litellm, openrouter)
    }

    fn fixture_models_dev() -> HashMap<String, ModelPricing> {
        models_dev::parse_dataset(include_str!("../../tests/fixtures/models_dev_pricing.json"))
            .unwrap()
    }

    fn custom_service_with_models_dev(
        custom: HashMap<String, ModelPricing>,
        litellm: HashMap<String, ModelPricing>,
        openrouter: HashMap<String, ModelPricing>,
        models_dev: HashMap<String, ModelPricing>,
    ) -> PricingService {
        PricingService::new_with_custom_and_models_dev(
            CustomPricing::from_models(custom),
            litellm,
            openrouter,
            models_dev,
        )
    }

    fn cache_read_usage() -> TokenBreakdown {
        TokenBreakdown {
            input: 1_000_000,
            output: 0,
            cache_read: 1_000_000,
            cache_write: 0,
            reasoning: 0,
        }
    }

    // Regression: #1013. Submission validation judged bucket coverage against
    // the provider-hinted row alone. For `openai/gpt-5.2-codex` the hint lands
    // on an OpenRouter row with no cache-read rate while the canonical LiteLLM
    // row publishes one, so every Codex session — which always carries cached
    // tokens — was reported as unpriced and aborted the whole submission.
    #[test]
    fn hinted_row_missing_a_cache_rate_still_covers_usage() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "azure/codex-cache-gap".to_string(),
            ModelPricing {
                input_cost_per_token: Some(1.75e-6),
                output_cost_per_token: Some(1.4e-5),
                ..Default::default()
            },
        );
        litellm.insert(
            "codex-cache-gap".to_string(),
            ModelPricing {
                input_cost_per_token: Some(1.75e-6),
                output_cost_per_token: Some(1.4e-5),
                cache_read_input_token_cost: Some(1.75e-7),
                ..Default::default()
            },
        );
        let service = PricingService::new(litellm, HashMap::new());
        let usage = cache_read_usage();

        assert!(service.covers_usage_with_provider("codex-cache-gap", Some("azure"), &usage));
        let cost = service.calculate_cost_with_provider("codex-cache-gap", Some("azure"), &usage);
        assert!((cost - 1.925).abs() < 1e-9, "unexpected cost: {cost}");
    }

    #[test]
    fn reasonix_uses_the_inferred_upstream_provider_for_pricing() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "deepseek/reasonix-fixture".to_string(),
            ModelPricing {
                input_cost_per_token: Some(2e-6),
                output_cost_per_token: Some(8e-6),
                ..Default::default()
            },
        );
        let service = PricingService::new(litellm, HashMap::new());
        let usage = TokenBreakdown {
            input: 1_000,
            output: 1_000,
            ..Default::default()
        };

        assert!(service.covers_usage_with_provider(
            "opencode/reasonix-fixture",
            Some("deepseek"),
            &usage,
        ));
        assert!(
            (service.calculate_cost_with_provider(
                "opencode/reasonix-fixture",
                Some("deepseek"),
                &usage,
            ) - 0.01)
                .abs()
                < 1e-12
        );
    }

    // The two rows must be the same deal before one lends the other a rate.
    // `azure_ai/grok-code-fast-1` bills $3.50/$17.50 per million with no
    // cache-read rate while the canonical `xai/` row bills $0.20/$1.50 with
    // one; borrowing across them would invent an Azure-base, xAI-cache tariff
    // that neither provider charges.
    #[test]
    fn differently_priced_canonical_row_does_not_lend_its_cache_rate() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "azure/grok-tariff-guard".to_string(),
            ModelPricing {
                input_cost_per_token: Some(3.5e-6),
                output_cost_per_token: Some(1.75e-5),
                ..Default::default()
            },
        );
        litellm.insert(
            "grok-tariff-guard".to_string(),
            ModelPricing {
                input_cost_per_token: Some(2e-7),
                output_cost_per_token: Some(1.5e-6),
                cache_read_input_token_cost: Some(2e-8),
                ..Default::default()
            },
        );
        let service = PricingService::new(litellm, HashMap::new());
        let usage = cache_read_usage();

        assert!(
            !service.covers_usage_with_provider("grok-tariff-guard", Some("azure"), &usage),
            "a differently priced row must not make the usage look priceable"
        );
        let cost = service.calculate_cost_with_provider("grok-tariff-guard", Some("azure"), &usage);
        assert!(
            (cost - 3.5).abs() < 1e-9,
            "the reseller's own rates must be the only ones applied: {cost}"
        );
    }

    // Guard for the fix above: borrowing must never reach a bucket the hinted
    // row already prices, otherwise a reseller row (e.g. `azure_ai/` at a
    // markup over `xai/`) would silently reprice to the author's cheaper rate.
    #[test]
    fn covered_hinted_row_is_not_replaced_by_the_canonical_row() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "azure/marked-up-model".to_string(),
            ModelPricing {
                input_cost_per_token: Some(1e-5),
                cache_read_input_token_cost: Some(1e-6),
                ..Default::default()
            },
        );
        litellm.insert(
            "marked-up-model".to_string(),
            ModelPricing {
                input_cost_per_token: Some(1e-7),
                cache_read_input_token_cost: Some(1e-8),
                ..Default::default()
            },
        );
        let service = PricingService::new(litellm, HashMap::new());
        let usage = cache_read_usage();

        assert!(service.covers_usage_with_provider("marked-up-model", Some("azure"), &usage));
        let cost = service.calculate_cost_with_provider("marked-up-model", Some("azure"), &usage);
        assert!(
            (cost - 11.0).abs() < 1e-9,
            "reseller markup must survive: {cost}"
        );
    }

    // A model nothing can price must still be rejected, so submissions never
    // silently bill genuinely unknown usage at zero.
    #[test]
    fn usage_stays_uncovered_when_no_resolution_prices_the_bucket() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "azure/no-cache-anywhere".to_string(),
            model_pricing(1e-5, 1e-4),
        );
        litellm.insert("no-cache-anywhere".to_string(), model_pricing(1e-6, 1e-5));
        let service = PricingService::new(litellm, HashMap::new());

        assert!(!service.covers_usage_with_provider(
            "no-cache-anywhere",
            Some("azure"),
            &cache_read_usage()
        ));
    }

    // Custom overrides are exact-only and provider-agnostic, so they must be
    // consulted before any provider-hinted resolution or bucket borrowing.
    #[test]
    fn custom_pricing_decides_coverage_before_any_fallback() {
        let mut custom = HashMap::new();
        custom.insert(
            "custom-covered-model".to_string(),
            ModelPricing {
                input_cost_per_token: Some(1e-6),
                cache_read_input_token_cost: Some(1e-7),
                ..Default::default()
            },
        );
        let service = custom_service(custom, HashMap::new(), HashMap::new());

        assert!(service.covers_usage_with_provider(
            "custom-covered-model",
            Some("azure"),
            &cache_read_usage()
        ));
    }

    // Regression: #1002. A LiteLLM fetch failure used to propagate out of
    // fetch_inner, so `tokscale submit` died with "error decoding response
    // body" even though models.dev and openrouter were both reachable and
    // carried usable pricing.
    #[test]
    fn litellm_fetch_failure_is_not_fatal_when_another_source_has_data() {
        let mut models_dev = HashMap::new();
        models_dev.insert("test-model-alpha".to_string(), model_pricing(1e-6, 2e-6));

        let service = PricingService::combine_fetched_sources(
            Err("error decoding response body".to_string()),
            Err("OpenRouter unavailable".to_string()),
            Ok(models_dev),
            // Fresh install, as in the report: nothing cached yet.
            || None,
            || None,
            || None,
            CustomPricing::default(),
        )
        .expect("a LiteLLM failure must not be fatal while another source has pricing");

        let cost = service.calculate_cost("test-model-alpha", 1_000_000, 0, 0, 0, 0);
        assert!(
            (cost - 1.0).abs() < 1e-9,
            "models.dev pricing should still resolve after LiteLLM fails, got {}",
            cost
        );
    }

    // Regression: #1002. The reporter's workaround was hand-populating the
    // cache file. A cached copy older than the 1h TTL must be preferred over
    // dropping LiteLLM entirely, so that workaround keeps working unattended.
    #[test]
    fn litellm_fetch_failure_falls_back_to_stale_cache() {
        let mut cached = HashMap::new();
        cached.insert("test-model-beta".to_string(), model_pricing(3e-6, 4e-6));

        let service = PricingService::combine_fetched_sources(
            Err("error decoding response body".to_string()),
            Err("OpenRouter unavailable".to_string()),
            Ok(HashMap::new()),
            || Some(cached),
            || None,
            || None,
            CustomPricing::default(),
        )
        .expect("a stale LiteLLM cache must keep the service usable");

        let cost = service.calculate_cost("test-model-beta", 1_000_000, 0, 0, 0, 0);
        assert!(
            (cost - 3.0).abs() < 1e-9,
            "stale LiteLLM cache should price the model, got {}",
            cost
        );
    }

    // Regression: models.dev is a degradable source too. Its errors used to be
    // dropped straight to an empty map even though it keeps a cache of its own,
    // so a models.dev outage discarded pricing that was sitting on disk.
    #[test]
    fn models_dev_fetch_failure_falls_back_to_stale_cache() {
        let mut cached = HashMap::new();
        cached.insert("test-model-gamma".to_string(), model_pricing(5e-6, 6e-6));

        let service = PricingService::combine_fetched_sources(
            Ok(HashMap::new()),
            Err("OpenRouter unavailable".to_string()),
            Err("models.dev unreachable".to_string()),
            || None,
            || None,
            || Some(cached),
            CustomPricing::default(),
        )
        .expect("a stale models.dev cache must keep the service usable");

        let cost = service.calculate_cost("test-model-gamma", 1_000_000, 0, 0, 0, 0);
        assert!(
            (cost - 5.0).abs() < 1e-9,
            "stale models.dev cache should price the model, got {}",
            cost
        );
    }

    #[test]
    fn custom_pricing_keeps_service_available_during_dynamic_outage() {
        let mut custom = HashMap::new();
        custom.insert("custom-only".to_string(), model_pricing(3e-6, 4e-6));
        let service = PricingService::combine_fetched_sources(
            Err("error decoding response body: expected f64".to_string()),
            Err("OpenRouter unreachable".to_string()),
            Err("models.dev unreachable".to_string()),
            || None,
            || None,
            || None,
            CustomPricing::from_models(custom),
        )
        .expect("custom pricing should remain usable during an upstream outage");
        assert!(service.lookup_with_source("custom-only", None).is_some());
    }

    #[test]
    fn openrouter_fetch_failure_falls_back_to_stale_cache() {
        let mut cached = HashMap::new();
        cached.insert("openrouter-only".to_string(), model_pricing(7e-6, 8e-6));

        let service = PricingService::combine_fetched_sources(
            Err("LiteLLM unavailable".to_string()),
            Err("OpenRouter unavailable".to_string()),
            Err("models.dev unavailable".to_string()),
            || None,
            || Some(cached),
            || None,
            CustomPricing::default(),
        )
        .expect("a stale OpenRouter cache must keep the service usable");

        assert!(service
            .lookup_with_source("openrouter-only", None)
            .is_some());
    }

    #[test]
    fn models_dev_parses_fixture_prices_per_token() {
        let data = fixture_models_dev();
        let pricing = data.get("openai/gpt-fixture-model").unwrap();

        assert_eq!(pricing.input_cost_per_token, Some(0.00000125));
        assert_eq!(pricing.output_cost_per_token, Some(0.00001));
        assert_eq!(pricing.cache_read_input_token_cost, Some(0.000000125));
        assert_eq!(pricing.cache_creation_input_token_cost, Some(0.000001875));
        assert!(!data.contains_key("openai/missing-output-price"));
    }

    #[test]
    fn models_dev_fills_provider_aware_fallback_prices() {
        let service = custom_service_with_models_dev(
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            fixture_models_dev(),
        );

        let result = service
            .lookup_with_source_and_provider("gpt-fixture-model", None, Some("openai"))
            .unwrap();

        assert_eq!(result.source, "Models.dev");
        assert_eq!(result.matched_key, "openai/gpt-fixture-model");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.00000125));
    }

    #[test]
    fn models_dev_cache_prices_are_used_for_cost_fallback() {
        let service = custom_service_with_models_dev(
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            fixture_models_dev(),
        );
        let usage = TokenBreakdown {
            input: 1_000_000,
            output: 100_000,
            cache_read: 50_000,
            cache_write: 20_000,
            reasoning: 0,
        };

        let cost =
            service.calculate_cost_with_provider("gpt-fixture-model", Some("openai"), &usage);

        let expected = 1.25 + 1.0 + 0.00625 + 0.0375;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn existing_sources_beat_models_dev_fallback() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gpt-fixture-model".into(),
            model_pricing(0.000002, 0.000008),
        );
        let mut openrouter = HashMap::new();
        openrouter.insert(
            "anthropic/claude-fixture-sonnet".into(),
            model_pricing(0.000004, 0.000016),
        );

        let service = custom_service_with_models_dev(
            HashMap::new(),
            litellm,
            openrouter,
            fixture_models_dev(),
        );

        let litellm_result = service
            .lookup_with_source_and_provider("gpt-fixture-model", None, Some("openai"))
            .unwrap();
        assert_eq!(litellm_result.source, "LiteLLM");
        assert_eq!(litellm_result.pricing.input_cost_per_token, Some(0.000002));

        let openrouter_result = service
            .lookup_with_source_and_provider("claude-fixture-sonnet", None, Some("anthropic"))
            .unwrap();
        assert_eq!(openrouter_result.source, "OpenRouter");
        assert_eq!(
            openrouter_result.pricing.input_cost_per_token,
            Some(0.000004)
        );
    }

    #[test]
    fn models_dev_respects_forced_source_boundaries() {
        let service = custom_service_with_models_dev(
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            fixture_models_dev(),
        );

        assert!(service
            .lookup_with_source_and_provider("gpt-fixture-model", Some("litellm"), Some("openai"))
            .is_none());
        assert!(service
            .lookup_with_source_and_provider(
                "gpt-fixture-model",
                Some("openrouter"),
                Some("openai")
            )
            .is_none());

        let result = service
            .lookup_with_source_and_provider(
                "gpt-fixture-model",
                Some("models.dev"),
                Some("openai"),
            )
            .unwrap();
        assert_eq!(result.source, "Models.dev");
    }

    #[test]
    fn custom_override_beats_models_dev_fallback() {
        let mut custom = HashMap::new();
        custom.insert(
            "gpt-fixture-model".into(),
            model_pricing(0.000009, 0.000018),
        );

        let service = custom_service_with_models_dev(
            custom,
            HashMap::new(),
            HashMap::new(),
            fixture_models_dev(),
        );

        let result = service
            .lookup_with_source_and_provider("gpt-fixture-model", None, Some("openai"))
            .unwrap();

        assert_eq!(result.source, "Custom");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.000009));
    }

    /// models.dev rows shaped like the live Copilot catalog: the two vendor
    /// passthrough entries, the `openai/` row they were copied from, and two
    /// entries that do track GitHub's own published numbers.
    fn copilot_models_dev() -> HashMap<String, ModelPricing> {
        let openai_passthrough = ModelPricing {
            input_cost_per_token: Some(1.75e-6),
            output_cost_per_token: Some(1.4e-5),
            cache_read_input_token_cost: Some(1.75e-7),
            ..Default::default()
        };
        let mut data = HashMap::new();
        data.insert("github-copilot/gpt-5.2".into(), openai_passthrough.clone());
        data.insert(
            "github-copilot/gpt-5.2-codex".into(),
            openai_passthrough.clone(),
        );
        data.insert("openai/gpt-5.2".into(), openai_passthrough);
        // Verbatim from live models.dev (accessed 2026-08-05): $2.00 / $6.00 /
        // $0.50 per 1M. The cache-read rate is GitHub's own rather than xAI's
        // native $0.30 — the evidence that models.dev tracks GitHub for this
        // row, and the reason the numbers here have to be the real ones.
        data.insert(
            "github-copilot/grok-4.5".into(),
            ModelPricing {
                input_cost_per_token: Some(2e-6),
                output_cost_per_token: Some(6e-6),
                cache_read_input_token_cost: Some(5e-7),
                ..Default::default()
            },
        );
        data.insert(
            "github-copilot/claude-sonnet-4.5".into(),
            ModelPricing {
                input_cost_per_token: Some(3e-6),
                output_cost_per_token: Some(1.5e-5),
                cache_read_input_token_cost: Some(3e-7),
                ..Default::default()
            },
        );
        data
    }

    fn copilot_service() -> PricingService {
        custom_service_with_models_dev(
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            copilot_models_dev(),
        )
    }

    // `github-copilot/gpt-5.2` and `-codex` are absent from GitHub's published
    // rate table, and models.dev prices them byte-identically to its own
    // `openai/gpt-5.2` row — vendor passthrough, not what a Copilot subscriber
    // is billed. Pricing them silently corrupts a public spend leaderboard;
    // leaving them unpriced merely excludes and reports the row.
    #[test]
    fn copilot_gpt_5_2_passthrough_rows_resolve_to_no_price() {
        let service = copilot_service();
        let usage = TokenBreakdown {
            input: 1_000_000,
            output: 100_000,
            ..Default::default()
        };

        for model in ["github-copilot/gpt-5.2", "github-copilot/gpt-5.2-codex"] {
            assert!(
                service.lookup_with_source(model, None).is_none(),
                "{model} must not resolve to a price"
            );
            assert!(
                !service.covers_usage_with_provider(model, None, &usage),
                "{model} must report as unpriced so submission excludes it"
            );
            let cost = service.calculate_cost_with_provider(model, None, &usage);
            assert_eq!(cost, 0.0, "{model} priced at {cost}");
        }
    }

    // The bare id plus a `github-copilot` provider is one of the two shapes
    // that carry Copilot provenance into the lookup, and it is real:
    // `tests/fixtures/local_model_ids.txt` lines 73 and 76 are harvested from
    // live sessions and carry exactly it. Suppressing only the slash-qualified
    // spelling would leave that path open.
    #[test]
    fn copilot_gpt_5_2_stays_unpriced_under_a_provider_hint() {
        let service = copilot_service();

        for model in ["gpt-5.2", "gpt-5.2-codex"] {
            assert!(
                service
                    .lookup_with_source_and_provider(model, None, Some("github-copilot"))
                    .is_none(),
                "{model} under a github-copilot hint must not resolve to a price"
            );
        }
    }

    // `PricingLookup` strips reasoning-effort decorations before it resolves a
    // key, so a guard that matched the raw id would let the decorated spellings
    // normalize onto the passthrough row it just refused. The `openai` half of
    // each pair proves the escape route is real rather than theoretical: the
    // same decorated id does resolve once Copilot provenance is absent.
    #[test]
    fn copilot_gpt_5_2_stays_unpriced_through_reasoning_tier_spellings() {
        let service = copilot_service();

        for model in [
            "gpt-5.2(high)",
            "gpt-5.2-codex(high)",
            "gpt-5.2-codex-high",
            "GPT-5.2-Codex(xhigh)",
        ] {
            assert!(
                service
                    .lookup_with_source_and_provider(model, None, Some("github-copilot"))
                    .is_none(),
                "{model} under a github-copilot hint must not resolve to a price"
            );
            let scoped = format!("github-copilot/{model}");
            assert!(
                service.lookup_with_source(&scoped, None).is_none(),
                "{scoped} must not resolve to a price"
            );

            let openai = service
                .lookup_with_source_and_provider(model, None, Some("openai"))
                .unwrap_or_else(|| {
                    panic!("{model} must still normalize onto OpenAI's own row without Copilot")
                });
            assert_eq!(openai.pricing.input_cost_per_token, Some(1.75e-6));
        }
    }

    // Guard against over-suppression: the other 27 `github-copilot/*` entries
    // track GitHub's table and must keep pricing, as must `openai/gpt-5.2`
    // itself for actual OpenAI usage.
    #[test]
    fn other_copilot_models_and_openai_gpt_5_2_still_price() {
        let service = copilot_service();

        for (model, expected_input) in [
            ("github-copilot/grok-4.5", 2e-6),
            ("github-copilot/claude-sonnet-4.5", 3e-6),
        ] {
            let result = service
                .lookup_with_source(model, None)
                .unwrap_or_else(|| panic!("{model} must still price from models.dev"));
            assert_eq!(result.source, "Models.dev");
            assert_eq!(result.matched_key, model);
            assert_eq!(result.pricing.input_cost_per_token, Some(expected_input));
        }

        let openai = service
            .lookup_with_source_and_provider("gpt-5.2", None, Some("openai"))
            .expect("OpenAI's own gpt-5.2 must keep pricing");
        assert_eq!(openai.matched_key, "openai/gpt-5.2");
        assert_eq!(openai.pricing.input_cost_per_token, Some(1.75e-6));
    }

    // The tier strip must not become a prefix match. `gpt-5.2-codex-max` is not
    // one of the two suppressed ids, and `-max` is not a reasoning tier, so it
    // keeps resolving — if GitHub publishes a rate for it, tokscale must use it.
    #[test]
    fn copilot_suppression_does_not_extend_to_sibling_ids() {
        let mut data = copilot_models_dev();
        data.insert(
            "github-copilot/gpt-5.2-codex-max".into(),
            ModelPricing {
                input_cost_per_token: Some(4e-6),
                output_cost_per_token: Some(2e-5),
                ..Default::default()
            },
        );
        let service =
            custom_service_with_models_dev(HashMap::new(), HashMap::new(), HashMap::new(), data);

        let result = service
            .lookup_with_source_and_provider("gpt-5.2-codex-max", None, Some("github-copilot"))
            .expect("gpt-5.2-codex-max is not a suppressed id and must price");
        assert_eq!(result.pricing.input_cost_per_token, Some(4e-6));
    }

    // The crux the guard cannot resolve, pinned so nobody "fixes" it here by
    // unpricing every direct OpenAI user. The first-party Copilot parsers set
    // `provider_id = inferred_provider_from_model(&model_id)`, which answers
    // `openai` for anything containing `gpt`, so a Copilot VS Code gpt-5.2
    // record and a direct OpenAI gpt-5.2 record reach this layer as the same
    // two arguments. The client that wrote the session file is what separates
    // them, and it is not passed here. Restoring that provenance is a parser
    // change, not a pricing change.
    #[test]
    fn copilot_provider_erased_to_openai_is_indistinguishable_and_stays_priced() {
        let service = copilot_service();

        let result = service
            .lookup_with_source_and_provider("gpt-5.2", None, Some("openai"))
            .expect("this tuple is also genuine direct-OpenAI usage and must stay priced");
        assert_eq!(result.matched_key, "openai/gpt-5.2");
        assert!(!is_copilot_vendor_passthrough("gpt-5.2", Some("openai")));
    }

    // Regression: GitHub moved Copilot off premium-request billing onto
    // usage-based AI Credits at published per-token rates on 2026-06-01, so a
    // `github_copilot/` row is no longer a meaningless $0.00 subscription
    // placeholder. The blanket prefix filter discarded the only two rows in
    // that namespace that carry real rates.
    #[test]
    fn litellm_github_copilot_rows_with_real_rates_survive_filtering() {
        let mut data = HashMap::new();
        data.insert(
            "github_copilot/mai-code-1-flash".to_string(),
            ModelPricing {
                input_cost_per_token: Some(7.5e-7),
                output_cost_per_token: Some(4.5e-6),
                ..Default::default()
            },
        );
        // 31 of the 33 live `github_copilot/` rows look like this: every rate
        // null, already dropped by the usable-base-rate retain.
        data.insert(
            "github_copilot/gpt-5.2".to_string(),
            ModelPricing::default(),
        );

        let filtered = PricingService::filter_litellm_data(data);

        assert!(!filtered.contains_key("github_copilot/gpt-5.2"));
        let flash = filtered
            .get("github_copilot/mai-code-1-flash")
            .expect("a github_copilot row carrying GitHub's published rate must survive");
        assert_eq!(flash.input_cost_per_token, Some(7.5e-7));
        assert_eq!(flash.output_cost_per_token, Some(4.5e-6));
    }

    #[test]
    fn litellm_github_copilot_flash_prices_at_githubs_published_rate() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "github_copilot/mai-code-1-flash".to_string(),
            ModelPricing {
                input_cost_per_token: Some(7.5e-7),
                output_cost_per_token: Some(4.5e-6),
                ..Default::default()
            },
        );
        let service = PricingService::from_cached_datasets(Some(litellm), None, None).unwrap();

        let result = service
            .lookup_with_source("github_copilot/mai-code-1-flash", None)
            .expect("MAI-Code-1-Flash must price from LiteLLM");
        assert_eq!(result.source, "LiteLLM");
        assert_eq!(result.matched_key, "github_copilot/mai-code-1-flash");
        // GitHub publishes $0.75 / $4.50 per 1M for MAI-Code-1-Flash.
        let cost = service.calculate_cost("github_copilot/mai-code-1-flash", 1_000_000, 0, 0, 0, 0);
        assert!((cost - 0.75).abs() < 1e-9, "unexpected cost: {cost}");
    }

    #[test]
    fn test_filter_drops_rows_without_a_usable_base_rate() {
        let mut data = HashMap::new();
        data.insert(
            "github_copilot/gpt-5.3-codex".into(),
            ModelPricing::default(),
        );
        data.insert("github_copilot/gpt-4o".into(), ModelPricing::default());
        data.insert(
            "gpt-5.2".into(),
            ModelPricing {
                input_cost_per_token: Some(0.00000175),
                ..Default::default()
            },
        );
        data.insert(
            "openai/gpt-5.2".into(),
            ModelPricing {
                output_cost_per_token: Some(0.000014),
                ..Default::default()
            },
        );
        data.insert(
            "tier-only".into(),
            ModelPricing {
                input_cost_per_token_above_272k_tokens: Some(0.00001),
                ..Default::default()
            },
        );

        let filtered = PricingService::filter_litellm_data(data);
        assert!(!filtered.contains_key("github_copilot/gpt-5.3-codex"));
        assert!(!filtered.contains_key("github_copilot/gpt-4o"));
        assert!(filtered.contains_key("gpt-5.2"));
        assert!(filtered.contains_key("openai/gpt-5.2"));
        assert!(!filtered.contains_key("tier-only"));
    }

    #[test]
    fn test_cursor_returns_pricing_when_not_in_upstream() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("gpt-5.3-codex", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.00000175));
        assert_eq!(result.pricing.output_cost_per_token, Some(0.000014));
        assert_eq!(result.pricing.cache_read_input_token_cost, Some(1.75e-7));
    }

    #[test]
    fn test_cursor_yields_to_litellm_exact() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gpt-5.3-codex".into(),
            ModelPricing {
                input_cost_per_token: Some(0.002),
                output_cost_per_token: Some(0.016),
                ..Default::default()
            },
        );
        let service = PricingService::new(litellm, HashMap::new());
        let result = service.lookup_with_source("gpt-5.3-codex", None).unwrap();
        assert_eq!(result.source, "LiteLLM");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.002));
    }

    #[test]
    fn test_cursor_yields_to_openrouter_prefix() {
        let mut openrouter = HashMap::new();
        openrouter.insert(
            "openai/gpt-5.3-codex".into(),
            ModelPricing {
                input_cost_per_token: Some(0.003),
                output_cost_per_token: Some(0.012),
                ..Default::default()
            },
        );
        let service = PricingService::new(HashMap::new(), openrouter);
        let result = service.lookup_with_source("gpt-5.3-codex", None).unwrap();
        assert_eq!(result.source, "OpenRouter");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.003));
    }

    #[test]
    fn test_cursor_skipped_when_force_source_set() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        assert!(service
            .lookup_with_source("gpt-5.3-codex", Some("litellm"))
            .is_none());
        assert!(service
            .lookup_with_source("gpt-5.3-codex", Some("openrouter"))
            .is_none());
    }

    #[test]
    fn test_cursor_matches_after_version_normalization() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("gpt-5-3-codex", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "gpt-5.3-codex");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.00000175));
    }

    #[test]
    fn test_cursor_matches_provider_prefixed_input() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service
            .lookup_with_source("openai/gpt-5.3-codex", None)
            .unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "gpt-5.3-codex");
    }

    #[test]
    fn test_cursor_provider_prefix_yields_to_upstream() {
        let mut openrouter = HashMap::new();
        openrouter.insert(
            "openai/gpt-5.3-codex".into(),
            ModelPricing {
                input_cost_per_token: Some(0.003),
                output_cost_per_token: Some(0.012),
                ..Default::default()
            },
        );
        let service = PricingService::new(HashMap::new(), openrouter);
        let result = service
            .lookup_with_source("openai/gpt-5.3-codex", None)
            .unwrap();
        assert_eq!(result.source, "OpenRouter");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.003));
    }

    #[test]
    fn test_cursor_matches_via_suffix_stripping() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service
            .lookup_with_source("gpt-5.3-codex-high", None)
            .unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "gpt-5.3-codex");
    }

    #[test]
    fn test_cursor_calculate_cost() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let cost = service.calculate_cost("gpt-5.3-codex", 1_000_000, 100_000, 0, 0, 0);
        let expected = 1_000_000.0 * 0.00000175 + 100_000.0 * 0.000014;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_cursor_returns_pricing_for_composer_1() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("Composer 1", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer 1");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.00000125));
        assert_eq!(result.pricing.output_cost_per_token, Some(0.00001));
        assert_eq!(result.pricing.cache_read_input_token_cost, Some(1.25e-7));
    }

    #[test]
    fn test_cursor_calculate_cost_for_composer_1() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let cost = service.calculate_cost("Composer 1", 1_000_000, 100_000, 50_000, 0, 0);
        let expected = 1_000_000.0 * 0.00000125 + 100_000.0 * 0.00001 + 50_000.0 * 1.25e-7;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_cursor_returns_pricing_for_hyphenated_composer_1() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("composer-1", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer-1");
    }

    #[test]
    fn test_cursor_returns_pricing_for_composer_1_5() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("Composer 1.5", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer 1.5");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.0000035));
        assert_eq!(result.pricing.output_cost_per_token, Some(0.0000175));
        assert_eq!(result.pricing.cache_read_input_token_cost, Some(3.5e-7));
    }

    #[test]
    fn test_cursor_calculate_cost_for_composer_1_5() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let cost = service.calculate_cost("Composer 1.5", 1_000_000, 100_000, 50_000, 0, 0);
        let expected = 1_000_000.0 * 0.0000035 + 100_000.0 * 0.0000175 + 50_000.0 * 3.5e-7;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_cursor_returns_pricing_for_hyphenated_composer_1_5() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("composer-1.5", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer-1.5");
    }

    #[test]
    fn test_cursor_returns_pricing_for_composer_2() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("composer-2", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer-2");
        assert_eq!(result.pricing.input_cost_per_token, Some(5e-7));
        assert_eq!(result.pricing.output_cost_per_token, Some(2.5e-6));
        assert_eq!(result.pricing.cache_read_input_token_cost, Some(2e-7));
        assert_eq!(result.pricing.cache_creation_input_token_cost, None);
    }

    #[test]
    fn test_cursor_returns_pricing_for_composer_2_spaced() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("Composer 2", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer 2");
    }

    #[test]
    fn test_cursor_returns_pricing_for_composer_2_fast() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("composer-2-fast", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer-2-fast");
        assert_eq!(result.pricing.input_cost_per_token, Some(1.5e-6));
        assert_eq!(result.pricing.output_cost_per_token, Some(7.5e-6));
        assert_eq!(result.pricing.cache_read_input_token_cost, Some(3.5e-7));
        assert_eq!(result.pricing.cache_creation_input_token_cost, None);
    }

    #[test]
    fn test_cursor_returns_pricing_for_composer_2_fast_spaced() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("Composer 2 Fast", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer 2 fast");
    }

    #[test]
    fn test_cursor_calculate_cost_for_composer_2() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let cost = service.calculate_cost("composer-2", 1_000_000, 100_000, 50_000, 0, 0);
        let expected = 1_000_000.0 * 5e-7 + 100_000.0 * 2.5e-6 + 50_000.0 * 2e-7;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_cursor_calculate_cost_composer_2_cache_write_free() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let with_write = service.calculate_cost("composer-2", 0, 0, 0, 500_000, 0);
        let without_write = service.calculate_cost("composer-2", 0, 0, 0, 0, 0);
        assert!((with_write - without_write).abs() < 1e-10);
    }

    #[test]
    fn test_cursor_calculate_cost_for_composer_2_fast() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let cost = service.calculate_cost("composer-2-fast", 1_000_000, 100_000, 50_000, 0, 0);
        let expected = 1_000_000.0 * 1.5e-6 + 100_000.0 * 7.5e-6 + 50_000.0 * 3.5e-7;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_cursor_calculate_cost_composer_2_fast_cache_write_free() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let with_write = service.calculate_cost("composer-2-fast", 0, 0, 0, 500_000, 0);
        let without_write = service.calculate_cost("composer-2-fast", 0, 0, 0, 0, 0);
        assert!(
            (with_write - without_write).abs() < 1e-10,
            "Cache creation should be free for Composer 2 Fast"
        );
    }

    #[test]
    fn test_cursor_returns_pricing_for_composer_2_5() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("composer-2.5", None).unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer-2.5");
        assert_eq!(result.pricing.input_cost_per_token, Some(5e-7));
        assert_eq!(result.pricing.output_cost_per_token, Some(2.5e-6));
        assert_eq!(result.pricing.cache_read_input_token_cost, Some(2e-7));
        assert_eq!(result.pricing.cache_creation_input_token_cost, None);
    }

    #[test]
    fn test_cursor_returns_pricing_for_composer_2_5_fast() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service
            .lookup_with_source("composer-2.5-fast", None)
            .unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer-2.5-fast");
        assert_eq!(result.pricing.input_cost_per_token, Some(1.5e-6));
        assert_eq!(result.pricing.output_cost_per_token, Some(7.5e-6));
        assert_eq!(result.pricing.cache_read_input_token_cost, Some(3.5e-7));
        assert_eq!(result.pricing.cache_creation_input_token_cost, None);
    }

    #[test]
    fn test_grok_composer_2_5_fast_uses_composer_2_5_fast_override() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service
            .lookup_with_source("grok-composer-2.5-fast", None)
            .unwrap();
        assert_eq!(result.source, "Cursor");
        assert_eq!(result.matched_key, "composer-2.5-fast");
        assert_eq!(result.pricing.input_cost_per_token, Some(1.5e-6));
        assert_eq!(result.pricing.output_cost_per_token, Some(7.5e-6));
        assert_eq!(result.pricing.cache_read_input_token_cost, Some(3.5e-7));

        let cost =
            service.calculate_cost("grok-composer-2.5-fast", 1_000_000, 100_000, 50_000, 0, 0);
        let expected = 1_000_000.0 * 1.5e-6 + 100_000.0 * 7.5e-6 + 50_000.0 * 3.5e-7;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_cursor_calculate_cost_for_composer_2_5() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let cost = service.calculate_cost("composer-2.5", 1_000_000, 100_000, 50_000, 0, 0);
        let expected = 1_000_000.0 * 5e-7 + 100_000.0 * 2.5e-6 + 50_000.0 * 2e-7;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_cursor_calculate_cost_composer_2_5_cache_write_free() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let with_write = service.calculate_cost("composer-2.5", 0, 0, 0, 500_000, 0);
        let without_write = service.calculate_cost("composer-2.5", 0, 0, 0, 0, 0);
        assert!((with_write - without_write).abs() < 1e-10);
    }

    #[test]
    fn test_cursor_calculate_cost_for_composer_2_5_fast() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let cost = service.calculate_cost("composer-2.5-fast", 1_000_000, 100_000, 50_000, 0, 0);
        let expected = 1_000_000.0 * 1.5e-6 + 100_000.0 * 7.5e-6 + 50_000.0 * 3.5e-7;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_cursor_calculate_cost_composer_2_5_fast_cache_write_free() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let with_write = service.calculate_cost("composer-2.5-fast", 0, 0, 0, 500_000, 0);
        let without_write = service.calculate_cost("composer-2.5-fast", 0, 0, 0, 0, 0);
        assert!(
            (with_write - without_write).abs() < 1e-10,
            "Cache creation should be free for Composer 2.5 Fast"
        );
    }

    #[test]
    fn test_cursor_composer_lookup_case_insensitive() {
        let service = PricingService::new(HashMap::new(), HashMap::new());

        let lower = service.lookup_with_source("composer 1", None);
        let upper = service.lookup_with_source("COMPOSER 1", None);
        let mixed = service.lookup_with_source("Composer 1", None);

        assert!(lower.is_some(), "lowercase should resolve");
        assert!(upper.is_some(), "UPPERCASE should resolve");
        assert!(mixed.is_some(), "Mixed Case should resolve");

        assert_eq!(
            lower.unwrap().pricing.input_cost_per_token,
            upper.unwrap().pricing.input_cost_per_token
        );
    }

    #[test]
    fn test_sakana_returns_pricing_for_fugu_ultra() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service.lookup_with_source("fugu-ultra", None).unwrap();
        assert_eq!(result.source, "Sakana");
        assert_eq!(result.matched_key, "fugu-ultra");
        assert_eq!(result.pricing.input_cost_per_token, Some(5e-6));
        assert_eq!(result.pricing.output_cost_per_token, Some(3e-5));
        assert_eq!(result.pricing.cache_read_input_token_cost, Some(5e-7));
        assert_eq!(result.pricing.cache_creation_input_token_cost, None);
        // >272K tier fields are populated (compute_cost reads them).
        assert_eq!(
            result.pricing.input_cost_per_token_above_272k_tokens,
            Some(1e-5)
        );
        assert_eq!(
            result.pricing.output_cost_per_token_above_272k_tokens,
            Some(4.5e-5)
        );
        assert_eq!(
            result.pricing.cache_read_input_token_cost_above_272k_tokens,
            Some(1e-6)
        );
    }

    #[test]
    fn test_sakana_calculate_cost_for_fugu_ultra() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        // Stay under the 272K threshold so only base rates apply.
        let cost = service.calculate_cost("fugu-ultra", 100_000, 10_000, 50_000, 0, 0);
        let expected = 100_000.0 * 5e-6 + 10_000.0 * 3e-5 + 50_000.0 * 5e-7;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn test_sakana_yields_to_litellm_exact() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "fugu-ultra".into(),
            ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let service = PricingService::new(litellm, HashMap::new());
        let result = service.lookup_with_source("fugu-ultra", None).unwrap();
        assert_eq!(result.source, "LiteLLM");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.001));
    }

    #[test]
    fn test_sakana_does_not_price_bare_fugu() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        // Bare `fugu` is a router/orchestrator — deliberately unpriced by Sakana.
        let result = service.lookup_with_source("fugu", None);
        assert!(
            result.as_ref().is_none_or(|r| r.source != "Sakana"),
            "bare `fugu` must not resolve to a Sakana price"
        );
    }

    #[test]
    fn test_sakana_resolves_dated_fugu_ultra_alias() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service
            .lookup_with_source("fugu-ultra-20260615", None)
            .unwrap();
        assert_eq!(result.source, "Sakana");
        assert_eq!(result.matched_key, "fugu-ultra");
        assert_eq!(result.pricing.input_cost_per_token, Some(5e-6));
    }

    // GitHub publishes a rate for "Raptor mini" ($0.25 / $2.00 / $0.025 per
    // 1M); neither models.dev nor LiteLLM carries the model at all.
    #[test]
    fn test_github_returns_pricing_for_oswe_vscode_prime() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service
            .lookup_with_source("oswe-vscode-prime", None)
            .expect("Raptor mini must price from the built-in GitHub override");

        assert_eq!(result.source, "GitHub");
        assert_eq!(result.matched_key, "oswe-vscode-prime");
        assert_eq!(result.pricing.input_cost_per_token, Some(2.5e-7));
        assert_eq!(result.pricing.output_cost_per_token, Some(2e-6));
        assert_eq!(result.pricing.cache_read_input_token_cost, Some(2.5e-8));
        // GitHub publishes no cache-write rate and no long-context tier, so
        // neither is invented here.
        assert_eq!(result.pricing.cache_creation_input_token_cost, None);
        assert_eq!(result.pricing.input_cost_per_token_above_272k_tokens, None);
        assert_eq!(result.pricing.output_cost_per_token_above_272k_tokens, None);
    }

    #[test]
    fn test_github_override_resolves_copilot_scoped_oswe_vscode_prime() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let result = service
            .lookup_with_source("github-copilot/oswe-vscode-prime", None)
            .expect("the copilot-scoped spelling must resolve too");

        assert_eq!(result.source, "GitHub");
        assert_eq!(result.matched_key, "oswe-vscode-prime");
    }

    // The rates are GitHub's only inside GitHub's namespace. Stripping any
    // namespace would hand Raptor mini's rate — and a `GitHub` source label
    // asserting GitHub published it for that key — to ids GitHub says nothing
    // about.
    #[test]
    fn test_github_override_rejects_foreign_namespaces() {
        let service = PricingService::new(HashMap::new(), HashMap::new());

        for model in [
            "openai/oswe-vscode-prime",
            "anthropic/oswe-vscode-prime",
            "some-router/openai/oswe-vscode-prime",
        ] {
            assert!(
                service.lookup_with_source(model, None).is_none(),
                "{model} is not GitHub's key and must not receive GitHub's rate"
            );
        }

        // LiteLLM's underscore spelling of the same namespace stays accepted.
        let litellm_spelling = service
            .lookup_with_source("github_copilot/oswe-vscode-prime", None)
            .expect("LiteLLM's namespace spelling must still resolve");
        assert_eq!(litellm_spelling.matched_key, "oswe-vscode-prime");
    }

    #[test]
    fn test_github_override_calculate_cost_and_coverage() {
        let service = PricingService::new(HashMap::new(), HashMap::new());
        let usage = TokenBreakdown {
            input: 1_000_000,
            output: 100_000,
            cache_read: 50_000,
            cache_write: 0,
            reasoning: 0,
        };

        assert!(service.covers_usage_with_provider(
            "oswe-vscode-prime",
            Some("github-copilot"),
            &usage
        ));
        let cost = service.calculate_cost_with_provider(
            "oswe-vscode-prime",
            Some("github-copilot"),
            &usage,
        );
        let expected = 1_000_000.0 * 2.5e-7 + 100_000.0 * 2e-6 + 50_000.0 * 2.5e-8;
        assert!((cost - expected).abs() < 1e-10, "unexpected cost: {cost}");
    }

    // Mirrors `test_sakana_yields_to_litellm_exact`: the override is a
    // stopgap for a model upstream does not carry, so the day upstream does
    // carry it, upstream wins.
    #[test]
    fn test_github_override_yields_to_litellm_exact() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "oswe-vscode-prime".into(),
            ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let service = PricingService::new(litellm, HashMap::new());
        let result = service
            .lookup_with_source("oswe-vscode-prime", None)
            .unwrap();

        assert_eq!(result.source, "LiteLLM");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.001));
    }

    #[test]
    fn test_github_override_skipped_when_force_source_set() {
        let service = PricingService::new(HashMap::new(), HashMap::new());

        for source in ["litellm", "openrouter", "models.dev", "custom"] {
            assert!(
                service
                    .lookup_with_source("oswe-vscode-prime", Some(source))
                    .is_none(),
                "forcing {source} must not surface the built-in override"
            );
        }
    }

    #[test]
    fn test_from_cached_datasets_returns_none_when_both_sources_missing() {
        assert!(PricingService::from_cached_datasets(None, None, None).is_none());
    }

    #[test]
    fn test_from_cached_datasets_filters_unpriced_litellm_entries() {
        let mut litellm = HashMap::new();
        // Live shape for 31 of the 33 `github_copilot/` rows: every rate null.
        litellm.insert(
            "github_copilot/gpt-5.3-codex".into(),
            ModelPricing::default(),
        );
        litellm.insert(
            "gpt-5.2".into(),
            ModelPricing {
                input_cost_per_token: Some(0.00000175),
                ..Default::default()
            },
        );

        let service = PricingService::from_cached_datasets(Some(litellm), None, None).unwrap();

        assert!(service
            .lookup_with_source("github_copilot/gpt-5.3-codex", Some("litellm"))
            .is_none());
        assert!(service
            .lookup_with_source("gpt-5.2", Some("litellm"))
            .is_some());
    }

    #[test]
    fn test_from_cached_datasets_uses_models_dev_when_other_sources_missing() {
        let service =
            PricingService::from_cached_datasets(None, None, Some(fixture_models_dev())).unwrap();

        let result = service
            .lookup_with_source_and_provider("gpt-fixture-model", None, Some("openai"))
            .unwrap();

        assert_eq!(result.source, "Models.dev");
        assert_eq!(result.matched_key, "openai/gpt-fixture-model");
    }

    #[test]
    fn custom_override_wins_over_litellm() {
        let mut custom = HashMap::new();
        custom.insert("gpt-4o".into(), model_pricing(0.000002, 0.000008));
        let mut litellm = HashMap::new();
        litellm.insert("gpt-4o".into(), model_pricing(0.00001, 0.00003));

        let service = custom_service(custom, litellm, HashMap::new());
        let result = service.lookup_with_source("gpt-4o", None).unwrap();

        assert_eq!(result.source, "Custom");
        assert_eq!(result.matched_key, "gpt-4o");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.000002));
    }

    #[test]
    fn custom_override_wins_over_openrouter() {
        let mut custom = HashMap::new();
        custom.insert("grok-code".into(), model_pricing(0.000002, 0.000008));
        let mut openrouter = HashMap::new();
        openrouter.insert("x-ai/grok-code".into(), model_pricing(0.00001, 0.00003));

        let service = custom_service(custom, HashMap::new(), openrouter);
        let result = service.lookup_with_source("grok-code", None).unwrap();

        assert_eq!(result.source, "Custom");
        assert_eq!(result.matched_key, "grok-code");
        assert_eq!(result.pricing.output_cost_per_token, Some(0.000008));
    }

    #[test]
    fn custom_override_respects_force_source() {
        let mut custom = HashMap::new();
        custom.insert("gpt-4o".into(), model_pricing(0.000002, 0.000008));
        let mut litellm = HashMap::new();
        litellm.insert("gpt-4o".into(), model_pricing(0.00001, 0.00003));
        let mut openrouter = HashMap::new();
        openrouter.insert("openai/gpt-4o".into(), model_pricing(0.000003, 0.000012));

        let service = custom_service(custom, litellm, openrouter);

        let litellm_result = service
            .lookup_with_source("gpt-4o", Some("litellm"))
            .unwrap();
        assert_eq!(litellm_result.source, "LiteLLM");
        assert_eq!(litellm_result.pricing.input_cost_per_token, Some(0.00001));

        let openrouter_result = service
            .lookup_with_source("gpt-4o", Some("openrouter"))
            .unwrap();
        assert_eq!(openrouter_result.source, "OpenRouter");
        assert_eq!(
            openrouter_result.pricing.input_cost_per_token,
            Some(0.000003)
        );

        let custom_result = service
            .lookup_with_source("gpt-4o", Some("custom"))
            .unwrap();
        assert_eq!(custom_result.source, "Custom");
        assert_eq!(custom_result.pricing.input_cost_per_token, Some(0.000002));
    }

    #[test]
    fn custom_force_source_does_not_fall_through_on_miss() {
        let mut litellm = HashMap::new();
        litellm.insert("gpt-4o".into(), model_pricing(0.0000025, 0.00001));

        let service = custom_service(HashMap::new(), litellm, HashMap::new());

        assert!(service
            .lookup_with_source("gpt-4o", Some("custom"))
            .is_none());
    }

    #[test]
    fn custom_override_raw_match_wins() {
        let mut custom = HashMap::new();
        custom.insert(
            "accounts/fireworks/routers/kimi-k2p6-turbo".into(),
            model_pricing(0.000002, 0.000008),
        );
        let mut litellm = HashMap::new();
        litellm.insert("kimi-k2.6".into(), model_pricing(0.00000095, 0.000004));

        let service = custom_service(custom, litellm, HashMap::new());
        let result = service
            .lookup_with_source("accounts/fireworks/routers/kimi-k2p6-turbo", None)
            .unwrap();

        assert_eq!(result.source, "Custom");
        assert_eq!(
            result.matched_key,
            "accounts/fireworks/routers/kimi-k2p6-turbo"
        );
        assert_eq!(result.pricing.input_cost_per_token, Some(0.000002));
    }

    #[test]
    fn custom_override_normalized_match_wins() {
        let mut custom = HashMap::new();
        custom.insert("kimi-k2p6".into(), model_pricing(0.00000095, 0.000004));
        let mut litellm = HashMap::new();
        litellm.insert("gpt-4-turbo".into(), model_pricing(0.00001, 0.00003));

        let service = custom_service(custom, litellm, HashMap::new());
        let result = service
            .lookup_with_source("accounts/fireworks/models/kimi-k2p6", None)
            .unwrap();

        assert_eq!(result.source, "Custom");
        assert_eq!(result.matched_key, "kimi-k2p6");
        assert_eq!(result.pricing.output_cost_per_token, Some(0.000004));
    }

    #[test]
    fn custom_override_raw_beats_normalized() {
        let mut custom = HashMap::new();
        custom.insert("kimi-k2p6-turbo".into(), model_pricing(0.000001, 0.000004));
        custom.insert(
            "accounts/fireworks/models/kimi-k2p6-turbo".into(),
            model_pricing(0.000002, 0.000008),
        );

        let service = custom_service(custom, HashMap::new(), HashMap::new());
        let result = service
            .lookup_with_source("accounts/fireworks/models/kimi-k2p6-turbo", None)
            .unwrap();

        assert_eq!(
            result.matched_key,
            "accounts/fireworks/models/kimi-k2p6-turbo"
        );
        assert_eq!(result.pricing.input_cost_per_token, Some(0.000002));
    }

    #[test]
    fn custom_override_skips_fuzzy_chain() {
        let mut custom = HashMap::new();
        custom.insert("kimi-k2p6-turbo".into(), model_pricing(0.000002, 0.000008));

        let service = custom_service(custom, HashMap::new(), HashMap::new());

        assert!(service
            .lookup_with_source("my-kimi-k2p6-turbo", None)
            .is_none());
    }

    #[test]
    fn no_custom_falls_through_to_litellm() {
        let mut litellm = HashMap::new();
        litellm.insert("gpt-4o".into(), model_pricing(0.0000025, 0.00001));

        let service = custom_service(HashMap::new(), litellm, HashMap::new());
        let result = service.lookup_with_source("gpt-4o", None).unwrap();

        assert_eq!(result.source, "LiteLLM");
        assert_eq!(result.pricing.input_cost_per_token, Some(0.0000025));
    }

    #[test]
    fn custom_calculate_cost_uses_override() {
        let mut custom = HashMap::new();
        custom.insert(
            "accounts/fireworks/routers/kimi-k2p6-turbo".into(),
            model_pricing(0.000002, 0.000008),
        );
        let mut litellm = HashMap::new();
        litellm.insert(
            "accounts/fireworks/routers/kimi-k2p6-turbo".into(),
            model_pricing(0.00001, 0.00003),
        );

        let service = custom_service(custom, litellm, HashMap::new());
        let cost = service.calculate_cost(
            "accounts/fireworks/routers/kimi-k2p6-turbo",
            1_000_000,
            100_000,
            0,
            0,
            0,
        );

        let expected = 1_000_000.0 * 0.000002 + 100_000.0 * 0.000008;
        assert!((cost - expected).abs() < 1e-10);
    }
}
