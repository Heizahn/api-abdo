//! WhatsApp webhook handlers extracted from `handler.rs` during PR2.
//!
//! The implementation currently remains in the compatibility owner module (`handler`)
//! until the next PRs continue the modularization. This file provides a stable
//! ownership boundary so routing and OpenAPI wiring can be moved first.

pub use crate::modules::whatsapp::handler::{
    debug_last_webhook_handler, receive_webhook, verify_webhook,
};
