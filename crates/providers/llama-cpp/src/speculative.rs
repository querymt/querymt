use crate::backend::llama_backend;
use crate::config::LlamaCppConfig;
use crate::context::{apply_context_params, resolve_n_batch, resolve_n_ubatch};
use crate::tools::sampler::{SamplingParams, build_standard_sampler};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::{LlamaContextParams, LlamaContextType};
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::speculative::Speculative;
use llama_cpp_2::token::LlamaToken;
use querymt::error::LLMError;
use std::num::NonZeroU32;
use std::sync::Arc;

const SEQ_ID: i32 = 0;

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SpeculativeRunStats {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub rounds: u32,
    pub drafted: u32,
    pub accepted: u32,
}

fn clear_from(ctx: &mut LlamaContext<'_>, pos: i32, which: &str) -> Result<(), LLMError> {
    let from = u32::try_from(pos).map_err(|_| {
        LLMError::ProviderError(format!(
            "Invalid negative speculative {which} rollback position {pos}"
        ))
    })?;
    ctx.kv_cache_seq_rm(SEQ_ID, Some(from), None).map_err(|e| {
        LLMError::ProviderError(format!("speculative {which} rollback at {pos} failed: {e}"))
    })
}

fn draft_head_layers(model: &LlamaModel) -> u32 {
    let Ok(arch) = model.meta_val_str("general.architecture") else {
        return 0;
    };
    model
        .meta_val_str(&format!("{arch}.nextn_predict_layers"))
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

fn validate_models(target: &LlamaModel, draft: &LlamaModel) -> Result<(), LLMError> {
    if draft_head_layers(draft) == 0 {
        return Err(LLMError::InvalidRequest(
            "speculative was requested, but the selected bundled model or sidecar has no nextn_predict_layers metadata"
                .into(),
        ));
    }
    if draft.n_embd_out() != target.n_embd() {
        return Err(LLMError::InvalidRequest(format!(
            "speculative draft embedding width {} does not match target width {}",
            draft.n_embd_out(),
            target.n_embd()
        )));
    }
    if !std::ptr::eq(target, draft) && draft.n_vocab() != target.n_vocab() {
        return Err(LLMError::InvalidRequest(format!(
            "speculative draft vocabulary {} does not match target vocabulary {}",
            draft.n_vocab(),
            target.n_vocab()
        )));
    }
    Ok(())
}

/// Run speculative decoding and deliver each target-approved, non-EOG token immediately.
///
/// The callback returns `false` to stop generation, for example after a stop
/// sequence or when a stream receiver has disconnected.
pub(crate) fn run_speculative(
    model: &Arc<LlamaModel>,
    draft_model: Option<&Arc<LlamaModel>>,
    cfg: &LlamaCppConfig,
    prompt: &str,
    max_tokens: u32,
    temperature: Option<f32>,
    sampler: Option<LlamaSampler>,
    mut on_token: impl FnMut(LlamaToken) -> Result<bool, LLMError>,
) -> Result<SpeculativeRunStats, LLMError> {
    let speculative_cfg = cfg
        .speculative
        .as_ref()
        .ok_or_else(|| LLMError::InvalidRequest("speculative decoding is not configured".into()))?;
    let spec_params = speculative_cfg.params().map_err(LLMError::InvalidRequest)?;
    let draft_model = draft_model.map_or(model.as_ref(), Arc::as_ref);
    if speculative_cfg.kind == crate::config::SpeculativeType::Mtp {
        validate_models(model, draft_model)?;
    }

    let tokens = model
        .str_to_token(
            prompt,
            if cfg.add_bos.unwrap_or(true) {
                AddBos::Always
            } else {
                AddBos::Never
            },
        )
        .map_err(|e| LLMError::ProviderError(e.to_string()))?;
    if tokens.is_empty() {
        return Err(LLMError::InvalidRequest(
            "Prompt tokenization resulted in an empty sequence".into(),
        ));
    }
    let mut stats = SpeculativeRunStats {
        input_tokens: tokens.len() as u32,
        ..SpeculativeRunStats::default()
    };
    if max_tokens == 0 {
        return Ok(stats);
    }

    let needed = tokens.len() as u32 + max_tokens;
    let n_ctx_raw = cfg.n_ctx.unwrap_or_else(|| needed.min(model.n_ctx_train()));
    if needed > n_ctx_raw {
        return Err(LLMError::InvalidRequest(format!(
            "Prompt + max_tokens ({needed}) exceeds context window ({n_ctx_raw})"
        )));
    }
    let n_ctx = NonZeroU32::new(n_ctx_raw)
        .ok_or_else(|| LLMError::InvalidRequest("n_ctx must be greater than zero".into()))?;
    let n_batch = resolve_n_batch(cfg, n_ctx.get()).max(spec_params.n_max as u32 + 1);
    let n_ubatch = resolve_n_ubatch(cfg, n_batch, false);

    let mut target_params = LlamaContextParams::default()
        .with_n_ctx(Some(n_ctx))
        .with_n_batch(n_batch)
        .with_n_ubatch(n_ubatch)
        .with_n_rs_seq(spec_params.n_max as u32);
    if let Some(n) = cfg.n_threads {
        target_params = target_params.with_n_threads(n);
    }
    if let Some(n) = cfg.n_threads_batch {
        target_params = target_params.with_n_threads_batch(n);
    }
    target_params = apply_context_params(cfg, target_params)?;

    // The draft context intentionally does not inherit target KV quantization,
    // flash-attention, or recurrent snapshots.
    let context_type = match speculative_cfg.kind {
        crate::config::SpeculativeType::Mtp => LlamaContextType::Mtp,
        crate::config::SpeculativeType::DraftDflash => LlamaContextType::Default,
    };
    let mut draft_params = LlamaContextParams::default()
        .with_n_ctx(Some(n_ctx))
        .with_n_batch(n_batch)
        .with_n_ubatch(n_ubatch)
        .with_n_rs_seq(0)
        .with_context_type(context_type)
        .with_no_perf(false);
    if let Some(n) = cfg.n_threads {
        draft_params = draft_params.with_n_threads(n);
    }
    if let Some(n) = cfg.n_threads_batch {
        draft_params = draft_params.with_n_threads_batch(n);
    }

    let backend = llama_backend()?;
    let target_ctx = model.new_context(&backend, target_params).map_err(|e| {
        LLMError::ProviderError(format!("Failed to create speculative target context: {e}"))
    })?;
    let draft_ctx = draft_model
        .new_context_with_ctx_other(&backend, draft_params, &target_ctx)
        .map_err(|e| {
            LLMError::ProviderError(format!("Failed to create speculative draft context: {e}"))
        })?;
    let mut spec = Speculative::new(target_ctx, draft_ctx, spec_params)
        .map_err(|e| LLMError::ProviderError(format!("Failed to initialize speculative: {e}")))?;

    let mut batch = LlamaBatch::new(n_batch as usize, 1);
    for chunk_start in (0..tokens.len()).step_by(n_batch as usize) {
        batch.clear();
        let end = (chunk_start + n_batch as usize).min(tokens.len());
        for (position, token) in tokens.iter().enumerate().take(end).skip(chunk_start) {
            batch
                .add(*token, position as i32, &[SEQ_ID], true)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }
        spec.target_context_mut().decode(&mut batch).map_err(|e| {
            LLMError::ProviderError(format!("speculative prompt decode failed: {e}"))
        })?;
        spec.process(&batch)
            .map_err(|e| LLMError::ProviderError(format!("speculative prompt sync failed: {e}")))?;
    }
    spec.begin(&tokens)
        .map_err(|e| LLMError::ProviderError(format!("speculative begin failed: {e}")))?;

    let sampling_params = SamplingParams::from_config(cfg, temperature);
    let mut sampler = sampler.unwrap_or_else(|| build_standard_sampler(model, &sampling_params));
    let mut committed_prefix = tokens;
    let mut pending_position = committed_prefix.len() as i32;
    let mut pending = sampler.sample(spec.target_context(), batch.n_tokens() - 1);

    'generate: while stats.output_tokens < max_tokens {
        if model.is_eog_token(pending) {
            break;
        }
        stats.output_tokens += 1;
        if !on_token(pending)? || stats.output_tokens >= max_tokens {
            break;
        }

        let drafts = spec
            .draft(
                pending_position,
                pending,
                &committed_prefix,
                sampling_params.temperature.unwrap_or(0.0),
                sampling_params.seed,
            )
            .map_err(|e| {
                LLMError::ProviderError(format!(
                    "speculative draft failed at position {pending_position}: {e}"
                ))
            })?;
        stats.rounds += 1;
        stats.drafted += drafts.len() as u32;

        batch.clear();
        batch
            .add(pending, pending_position, &[SEQ_ID], true)
            .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        for (i, token) in drafts.iter().enumerate() {
            batch
                .add(*token, pending_position + 1 + i as i32, &[SEQ_ID], true)
                .map_err(|e| LLMError::ProviderError(e.to_string()))?;
        }

        // draft() writes speculative cells. process() must re-decode this region
        // using target embeddings, so remove those cells before verification.
        clear_from(
            spec.draft_context_mut(),
            pending_position,
            "pre-verification draft",
        )?;
        spec.target_context_mut().decode(&mut batch).map_err(|e| {
            LLMError::ProviderError(format!(
                "speculative verification decode failed at position {pending_position}: {e}"
            ))
        })?;
        spec.process(&batch).map_err(|e| {
            LLMError::ProviderError(format!(
                "speculative verification sync failed at position {pending_position}, draft_len={}: {e}",
                drafts.len()
            ))
        })?;

        let remaining = (max_tokens - stats.output_tokens) as usize;
        let (accepted, next_pending) = if drafts.is_empty() {
            (0, Some(sampler.sample(spec.target_context(), 0)))
        } else {
            let verification = spec.verify(&mut sampler).map_err(|e| {
                LLMError::ProviderError(format!(
                    "speculative verification failed at position {pending_position}: {e}"
                ))
            })?;
            let accepted = verification.accepted.min(remaining);
            let next_pending = verification.tokens.get(accepted).copied();
            spec.accept(accepted as u16).map_err(|e| {
                LLMError::ProviderError(format!(
                    "speculative accept failed at position {pending_position}, accepted={accepted}: {e}"
                ))
            })?;
            (accepted, next_pending)
        };
        stats.accepted += accepted as u32;

        let committed_end = pending_position + 1 + accepted as i32;
        clear_from(spec.target_context_mut(), committed_end, "target")?;
        clear_from(spec.draft_context_mut(), committed_end, "draft")?;
        committed_prefix.push(pending);

        for token in drafts.iter().take(accepted) {
            committed_prefix.push(*token);
            if model.is_eog_token(*token) {
                break 'generate;
            }
            stats.output_tokens += 1;
            if !on_token(*token)? || stats.output_tokens >= max_tokens {
                break 'generate;
            }
        }

        pending_position = committed_end;
        pending = next_pending.ok_or_else(|| {
            LLMError::ProviderError(
                "speculative verification produced no continuation token".into(),
            )
        })?;
    }

    log::debug!(
        "speculative complete: input={}, output={}, rounds={}, drafted={}, accepted={}",
        stats.input_tokens,
        stats.output_tokens,
        stats.rounds,
        stats.drafted,
        stats.accepted
    );
    log::info!(
        "llama.cpp speculative target timings:\n{}",
        spec.target_context_mut().timings()
    );
    log::info!(
        "llama.cpp speculative draft timings:\n{}",
        spec.draft_context_mut().timings()
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    fn accepted_prefix(drafts: &[i32], truth: &[i32]) -> (usize, i32, Vec<(usize, Option<i32>)>) {
        let mut accepted = 0;
        let mut calls = Vec::new();
        let mut previous = None;
        loop {
            calls.push((accepted, previous));
            let token = truth[accepted];
            match drafts.get(accepted) {
                Some(draft) if *draft == token => {
                    accepted += 1;
                    previous = Some(token);
                }
                _ => return (accepted, token, calls),
            }
        }
    }

    #[test]
    fn wrong_first_proposal_accepts_nothing() {
        assert_eq!(accepted_prefix(&[99, 11], &[10, 11]).0, 0);
    }

    #[test]
    fn partial_acceptance_stops_at_target_token() {
        let (accepted, next, calls) = accepted_prefix(&[10, 99, 12], &[10, 11, 12]);
        assert_eq!((accepted, next), (1, 11));
        assert_eq!(calls, vec![(0, None), (1, Some(10))]);
    }

    #[test]
    fn full_acceptance_samples_one_continuation() {
        let (accepted, next, calls) = accepted_prefix(&[10, 11, 12], &[10, 11, 12, 13]);
        assert_eq!((accepted, next), (3, 13));
        assert_eq!(calls.len(), 4);
    }

    #[test]
    fn empty_draft_is_a_single_target_step() {
        assert_eq!(accepted_prefix(&[], &[10]), (0, 10, vec![(0, None)]));
    }

    #[test]
    fn commit_boundary_includes_pending_and_accepted_drafts() {
        let pending_position = 619;
        assert_eq!(pending_position + 1, 620);
        assert_eq!(pending_position + 1 + 2, 622);
    }
}
