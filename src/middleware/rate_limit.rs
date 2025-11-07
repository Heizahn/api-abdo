use tower_governor::{
    governor::GovernorConfigBuilder,
    key_extractor::SmartIpKeyExtractor,
};

/// Crea un rate limiter general para la API
pub fn create_rate_limiter(per_second: u64, burst: u32) -> tower_governor::GovernorLayer<'static, SmartIpKeyExtractor> {
    let config = Box::leak(Box::new(
        GovernorConfigBuilder::default()
            .per_second(per_second)
            .burst_size(burst)
            .finish()
            .expect("Failed to create rate limiter config")
    ));

    tower_governor::GovernorLayer {
        config,
    }
}

/// Crea un rate limiter específico para endpoints de auth
/// Más restrictivo para prevenir ataques de fuerza bruta
pub fn create_auth_rate_limiter(per_minute: u64) -> tower_governor::GovernorLayer<'static, SmartIpKeyExtractor> {
    let config = Box::leak(Box::new(
        GovernorConfigBuilder::default()
            .per_second(per_minute / 60) // Convertir a por segundo
            .burst_size(per_minute as u32)
            .finish()
            .expect("Failed to create auth rate limiter config")
    ));

    tower_governor::GovernorLayer {
        config,
    }
}
