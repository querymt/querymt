use crate::middleware::{
    ContextConfig, ContextMiddleware, LimitsConfig, LimitsMiddleware, MiddlewareDriver,
};
use crate::model_info::ModelInfoSource;
use std::sync::Arc;

pub struct MiddlewarePresets;

impl MiddlewarePresets {
    /// Sensible defaults with dynamic model info lookup
    pub fn defaults() -> Vec<Arc<dyn MiddlewareDriver>> {
        vec![
            Arc::new(LimitsMiddleware::new(LimitsConfig {
                max_steps: Some(100),
                max_turns: Some(20),
                max_price_usd: None,
                model_info_source: ModelInfoSource::FromSession,
            })),
            Arc::new(ContextMiddleware::new(ContextConfig {
                warn_at_percent: 80,
                compact_at_percent: 85,
                auto_compact: true,
                context_source: ModelInfoSource::FromSession,
                fallback_max_tokens: 32_000,
            })),
        ]
    }

    /// Strict limits for cost control with dynamic model info
    pub fn cost_conscious() -> Vec<Arc<dyn MiddlewareDriver>> {
        vec![
            Arc::new(LimitsMiddleware::new(LimitsConfig {
                max_steps: Some(50),
                max_turns: Some(10),
                max_price_usd: None,
                model_info_source: ModelInfoSource::FromSession,
            })),
            Arc::new(ContextMiddleware::new(ContextConfig {
                warn_at_percent: 70,
                compact_at_percent: 85,
                auto_compact: true,
                context_source: ModelInfoSource::FromSession,
                fallback_max_tokens: 16_000,
            })),
        ]
    }

    /// Minimal middleware for maximum speed
    pub fn performance() -> Vec<Arc<dyn MiddlewareDriver>> {
        vec![Arc::new(LimitsMiddleware::new(LimitsConfig {
            max_steps: None,
            max_turns: None,
            max_price_usd: None,
            model_info_source: ModelInfoSource::FromSession,
        }))]
    }

    /// Manual configuration when model info is not available
    pub fn manual(
        context_limit: usize,
        input_cost: f64,
        output_cost: f64,
    ) -> Vec<Arc<dyn MiddlewareDriver>> {
        let model_info_source = ModelInfoSource::manual()
            .context_limit(context_limit)
            .pricing(input_cost, output_cost);

        vec![
            Arc::new(LimitsMiddleware::new(LimitsConfig {
                max_steps: Some(100),
                max_turns: Some(20),
                max_price_usd: None,
                model_info_source: model_info_source.clone(),
            })),
            Arc::new(ContextMiddleware::new(ContextConfig {
                warn_at_percent: 80,
                compact_at_percent: 85,
                auto_compact: true,
                context_source: model_info_source,
                fallback_max_tokens: context_limit,
            })),
        ]
    }
}
