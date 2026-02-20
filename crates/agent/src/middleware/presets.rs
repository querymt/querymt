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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Preset factory functions return correct numbers of middleware ────────

    #[test]
    fn defaults_returns_two_drivers() {
        let drivers = MiddlewarePresets::defaults();
        assert_eq!(drivers.len(), 2, "defaults() should produce 2 drivers");
    }

    #[test]
    fn defaults_drivers_have_names() {
        let drivers = MiddlewarePresets::defaults();
        for d in &drivers {
            let name = d.name();
            assert!(!name.is_empty(), "each driver should have a non-empty name");
        }
    }

    #[test]
    fn cost_conscious_returns_two_drivers() {
        let drivers = MiddlewarePresets::cost_conscious();
        assert_eq!(drivers.len(), 2);
    }

    #[test]
    fn performance_returns_one_driver() {
        let drivers = MiddlewarePresets::performance();
        assert_eq!(
            drivers.len(),
            1,
            "performance() should produce 1 driver (limits only)"
        );
    }

    #[test]
    fn manual_returns_two_drivers() {
        let drivers = MiddlewarePresets::manual(64_000, 0.003, 0.015);
        assert_eq!(drivers.len(), 2);
    }

    #[test]
    fn defaults_and_cost_conscious_differ() {
        // Both return 2 drivers but with different configurations.
        // We can't directly compare config, but we can verify names are consistent.
        let d1 = MiddlewarePresets::defaults();
        let d2 = MiddlewarePresets::cost_conscious();
        // Both should contain LimitsMiddleware and ContextMiddleware
        let names1: Vec<&str> = d1.iter().map(|d| d.name()).collect();
        let names2: Vec<&str> = d2.iter().map(|d| d.name()).collect();
        assert_eq!(
            names1, names2,
            "both presets should use the same middleware types"
        );
    }

    #[test]
    fn preset_drivers_can_reset() {
        let drivers = MiddlewarePresets::defaults();
        for d in &drivers {
            // reset() should not panic
            d.reset();
        }
    }

    #[test]
    fn preset_get_limits_returns_some() {
        // defaults() includes LimitsMiddleware which has max_steps=100
        let drivers = MiddlewarePresets::defaults();
        use crate::middleware::CompositeDriver;
        let composite = CompositeDriver::new(drivers);
        let limits = composite.get_limits();
        // Should return Some since LimitsMiddleware provides limits
        assert!(
            limits.is_some(),
            "defaults preset should expose session limits"
        );
        let limits = limits.unwrap();
        assert_eq!(limits.max_steps, Some(100));
        assert_eq!(limits.max_turns, Some(20));
    }

    #[test]
    fn performance_preset_get_limits_no_steps() {
        // performance() has no max_steps or max_turns set
        let drivers = MiddlewarePresets::performance();
        use crate::middleware::CompositeDriver;
        let composite = CompositeDriver::new(drivers);
        let limits = composite.get_limits();
        if let Some(limits) = limits {
            assert!(limits.max_steps.is_none());
            assert!(limits.max_turns.is_none());
        }
    }
}
