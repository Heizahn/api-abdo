use governor::middleware::NoOpMiddleware;
use tower_governor::{
    governor::GovernorConfigBuilder,
    key_extractor::SmartIpKeyExtractor,
    GovernorLayer,
};

/// Crea un rate limiter general para la API
// pub fn create_rate_limiter(
//     per_second: u64,
//     burst: u32,
// ) -> GovernorLayer<'static, SmartIpKeyExtractor, NoOpMiddleware> {
//     let config = Box::leak(Box::new(
//         GovernorConfigBuilder::default()
//             .per_second(per_second)
//             .burst_size(burst)
//             .key_extractor(SmartIpKeyExtractor)
//             .finish()
//             .expect("Failed to create rate limiter config"),
//     ));
// 
//     GovernorLayer { config }
// }

/// Crea un rate limiter permisivo para webhooks externos (ej: Meta WhatsApp).
/// 200 req/min por IP — suficiente para tráfico real de Meta sin bloquear entregas legítimas.
pub fn create_webhook_rate_limiter(
    per_minute: u64,
) -> GovernorLayer<'static, SmartIpKeyExtractor, NoOpMiddleware> {
    let per_second = std::cmp::max(1, per_minute / 60);
    let burst      = std::cmp::max(1, per_minute as u32);

    let config = Box::leak(Box::new(
        GovernorConfigBuilder::default()
            .per_second(per_second)
            .burst_size(burst)
            .key_extractor(SmartIpKeyExtractor)
            .finish()
            .expect("Failed to create webhook rate limiter config"),
    ));

    GovernorLayer { config }
}

/// Crea un rate limiter específico para endpoints de auth
/// Más restrictivo para prevenir ataques de fuerza bruta
pub fn create_auth_rate_limiter(
    per_minute: u64,
) -> GovernorLayer<'static, SmartIpKeyExtractor, NoOpMiddleware> {
    // Asegurar que per_second sea al menos 1 para evitar división por 0
    let per_second = std::cmp::max(1, per_minute / 60);
    let burst = std::cmp::max(1, per_minute as u32);

    let config = Box::leak(Box::new(
        GovernorConfigBuilder::default()
            .per_second(per_second)
            .burst_size(burst)
            .key_extractor(SmartIpKeyExtractor)
            .finish()
            .expect("Failed to create auth rate limiter config"),
    ));

    GovernorLayer { config }
}