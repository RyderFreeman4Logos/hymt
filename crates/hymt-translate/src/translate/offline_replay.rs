//! Retained output is replay authority only, never a runtime for new requests.
use super::*;
use hymt_core::runtime::BackendRuntimeInfo;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const VERSION: u64 = 1;

fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

fn configured_digest(
    text: &str,
    target: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    ctx: &TranslationCtx<'_>,
    streaming: bool,
) -> Result<String> {
    let config = ctx.config;
    let configured = config.detached_snapshot();
    configured.clear_backend_runtime_info();
    let options = template_options_hash(opts, effective_document_translation_policy(opts, config));
    Ok(digest(serde_json::to_vec(&json!({
        "source": digest(text), "target": target,
        "inference": configured.inference_fingerprint(template.as_str(), &options)?.hash(),
        "limits": [configured.total_context(), configured.parallel_slots(),
            configured.per_request_context(), configured.max_source_tokens_per_segment()],
        "version": configured.config_version(),
        "strict_preflight": configured.strict_backend_preflight(),
        "strict_budget": configured.strict_token_budget(),
        "warn_only": opts.warn_only_completeness.unwrap_or_else(|| configured.completeness_warn_only()),
        "tokenizer": ctx.segmenter.tokenizer_identity(),
        "first_chunk_priority": configured.first_chunk_priority(),
        "streaming": streaming, "concurrency": ctx.client.concurrency(),
        "configured_concurrency": configured.concurrency(),
    }))?))
}

// Length framing retains request order and avoids ambiguous concatenations.
pub(super) fn request_plan_digest(
    plan: &TranslationPlan,
    target: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    client: &TranslationClient,
    streaming: bool,
) -> Result<String> {
    let mut hash = Sha256::new();
    let mut bind = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    bind(&serde_json::to_vec(&json!({
        "source_tokens": plan.source_tokens,
        "available_source_tokens": plan.available_source_tokens,
        "budget": format!("{:?}", plan.token_budget),
        "document": plan.document_plan,
        "indexes": plan.segment_section_indexes,
        "groups": plan.segment_section_groups,
        "original_paragraphs": format!("{:?}", plan.original_paragraphs),
        "coherent_paragraphs": plan.coherent_paragraphs,
    }))?);
    for (index, segment) in plan.segments.iter().enumerate() {
        let prompt = build_prompt(segment, target, template, opts)?;
        bind(segment.as_bytes());
        bind(prompt.as_bytes());
        // Eligible plans have no cached segments: segment 0 is the stream route.
        bind(&client.request_bytes(&prompt, streaming && index == 0)?);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(super) fn population_proof(
    text: (&str, &str),
    target: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    ctx: &TranslationCtx<'_>,
    plan: &TranslationPlan,
    streaming: bool,
) -> Result<Option<(String, String)>> {
    let (source, output) = text;
    let Some(runtime) = ctx.config.backend_runtime_info().filter(valid_runtime) else {
        return Ok(None);
    };
    let options = template_options_hash(
        opts,
        effective_document_translation_policy(opts, ctx.config),
    );
    let inference = ctx
        .config
        .inference_fingerprint(template.as_str(), &options)?;
    if !inference.is_cache_verified()
        || output.is_empty()
        || output.len() > 16_777_216
        || !complete(source, output, target, ctx.config, plan)
    {
        return Ok(None);
    }
    let configured = configured_digest(source, target, template, opts, ctx, streaming)?;
    let proof = serde_json::to_string(&json!({
        "version": VERSION, "configured": configured, "runtime": runtime,
        "inference": inference.hash(),
        "requests": request_plan_digest(plan, target, template, opts, ctx.client, streaming)?,
        "output": digest(output),
    }))?;
    Ok((proof.len() <= 16_384).then_some((configured, proof)))
}

fn valid_runtime(runtime: &BackendRuntimeInfo) -> bool {
    runtime.is_verified()
        && runtime
            .served_model
            .as_ref()
            .is_some_and(|model| !model.is_empty())
        && match (&runtime.false_eos_token, runtime.false_eos_token_id) {
            (None, None) => true,
            (Some(token), Some(_)) => {
                token.len() == 1 && token.as_bytes()[0].is_ascii_punctuation()
            }
            _ => false,
        }
}

fn complete(
    source: &str,
    output: &str,
    target: &str,
    config: &HotConfig,
    plan: &TranslationPlan,
) -> bool {
    ensure_document_identities_preserved(source, output).is_ok()
        && ensure_markdown_structure_preserved(source, output).is_ok()
        && check_completeness(
            source,
            output,
            target,
            config,
            &CompletenessContext::default(),
        )
        .is_complete
        && (hymt_core::completeness::parse_json_document(source).is_some()
            || cache_proof::cached_owned_payloads_match(plan, output, target, config))
}

pub(crate) struct Replay {
    pub(crate) plan: TranslationPlan,
    pub(crate) record: TaskRecord,
    observed_at: u64,
}

impl Replay {
    pub(super) fn finish(mut self, history: &HistoryDB) -> Result<TranslationOutcome> {
        let output = self
            .record
            .output_text
            .take()
            .ok_or_else(|| anyhow!("missing replay output"))?;
        eprintln!(
            "Application cache replay: historical runtime observed_at={} (replay only)",
            self.observed_at
        );
        self.record.id = None;
        self.record.started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        self.record.finished_at = self.record.started_at.clone();
        self.record.duration_seconds = 0.0;
        self.record.tokens_per_second = 0.0;
        self.record.cache_hits = Some(self.plan.segment_count() as i64);
        self.record.cold_source_tokens = None;
        self.record.estimated_seconds = None;
        self.record.inference_fingerprint =
            format!("historical-replay:{}", self.record.inference_fingerprint);
        self.record.output_text = Some(output.clone());
        if let Err(error) = history.insert_task(&self.record) {
            eprintln!("Warning: failed to record cache replay: {error}");
        }
        Ok(TranslationOutcome {
            text: output,
            completeness_degraded_segments: Vec::new(),
            retained_regions: Vec::new(),
        })
    }
}

/// Replay validation uses a disposable snapshot. No retained runtime reaches the
/// caller, its client runtime cache, live planning, population, or timing scopes.
pub(crate) fn find_replay(
    source: &str,
    target: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    ctx: &TranslationCtx<'_>,
    streaming: bool,
) -> Result<Option<Replay>> {
    let config = ctx.config;
    let configured = configured_digest(source, target, template, opts, ctx, streaming)?;
    for (record, bytes) in ctx
        .history
        .find_replay_candidates(&digest(source), &configured)?
    {
        let validate = || -> Result<Option<Replay>> {
            let proof: Value = serde_json::from_str(&bytes)?;
            if proof["version"].as_u64() != Some(VERSION) || proof["configured"] != configured {
                return Ok(None);
            }
            let runtime: BackendRuntimeInfo = serde_json::from_value(proof["runtime"].clone())?;
            if !valid_runtime(&runtime) || runtime.backend != config.generation_backend()? {
                return Ok(None);
            }
            let isolated = config.detached_snapshot();
            isolated.set_backend_runtime_info(runtime.clone());
            let options =
                template_options_hash(opts, effective_document_translation_policy(opts, &isolated));
            let fingerprint = isolated.inference_fingerprint(template.as_str(), &options)?;
            if !fingerprint.is_cache_verified()
                || proof["inference"] != fingerprint.hash()
                || record.inference_fingerprint != fingerprint.hash()
            {
                return Ok(None);
            }
            let plan = plan_translation(source, target, &isolated, ctx.segmenter, template, opts)?;
            let client =
                TranslationClient::with_concurrency(isolated.clone(), ctx.client.concurrency())?;
            let Some(output) = record.output_text.as_deref() else {
                return Ok(None);
            };
            if proof["output"] != digest(output)
                || proof["requests"]
                    != request_plan_digest(&plan, target, template, opts, &client, streaming)?
                || !complete(source, output, target, &isolated, &plan)
            {
                return Ok(None);
            }
            Ok(Some(Replay {
                plan,
                record: record.clone(),
                observed_at: runtime.observed_at_unix_secs,
            }))
        };
        if let Ok(Some(replay)) = validate() {
            return Ok(Some(replay));
        }
    }
    Ok(None)
}
