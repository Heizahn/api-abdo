use tower_governor::{
    governor::GovernorConfigBuilder,
    key_extractor::SmartIpKeyExtractor,
    GovernorLayer,
};
use std::time::Duration;

/// Crea un rate limiter general para la API
pub fn create_rate_limiter(per_second: u64, burst: u32) -> GovernorLayer<'static, SmartIpKeyExtractor, tower_governor::governor::DefaultKeyedStateStore<SmartIpKeyExtractor>> {
    let config = GovernorConfigBuilder::default()
        .per_second(per_second)
        .burst_size(burst)
        .finish()
        .expect("Failed to create rate limiter config");

    GovernorLayer {
        config: std::sync::Arc::new(config),
    }
}

/// Crea un rate limiter específico para endpoints de auth
/// Más restrictivo para prevenir ataques de fuerza bruta
pub fn create_auth_rate_limiter(per_minute: u64) -> GovernorLayer<'static, SmartIpKeyExtractor, tower_governor::governor::DefaultKeyedStateStore<SmartIpKeyExtractor>> {
    let config = GovernorConfigBuilder::default()
        .period(Duration::from_secs(60))
        .burst_size(per_minute as u32)
        .finish()
        .expect("Failed to create auth rate limiter config");

    GovernorLayer {
        config: std::sync::Arc::new(config),
    }
}
