//! Legacy compatibility shim for callers that still import `whatsapp::handler`.

#[allow(unused_imports)]
pub use crate::modules::whatsapp::webhook::handler::{
    debug_last_webhook_handler, receive_webhook, verify_webhook,
};
