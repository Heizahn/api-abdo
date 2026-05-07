//! Estado IA persistido por conversación — pliegue de patches + formateo de prompt.
//!
//! El dispatch lee `WaConversation.ai_conv_state`, lo formatea como bloque
//! `[conversation_state]` para el system_instruction, corre el chain loop,
//! y al final pliega todos los `StatePatch` emitidos por las tools en un
//! nuevo `WaConversationAiState` que escribe de vuelta en DB.

use chrono::Utc;

use crate::models::whatsapp::{FailedAttempt, StatePatch, WaConversationAiState};

// ============================================
// Caps — ver ADR-5. Tunable en un solo lugar.
// ============================================

/// Máximo de llaves en `collected_data`. Al llegar al cap se rechaza la nueva
/// llave (PRESERVE las viejas — el `client_id` temprano vale más que el
/// `last_zone_mentioned` tardío).
pub const COLLECTED_DATA_KEY_CAP: usize = 20;

/// Máximo de caracteres por valor en `collected_data`. Los valores más largos
/// se truncan silenciosamente con un `warn!`.
pub const COLLECTED_DATA_VALUE_CHAR_CAP: usize = 500;

/// Máximo de entradas en `pending_data`. Al llegar al cap se rechaza la nueva.
#[allow(dead_code)]
pub const PENDING_DATA_CAP: usize = 20;

/// Máximo de entradas en `completed_actions`. FIFO al superar.
pub const COMPLETED_ACTIONS_CAP: usize = 50;

/// Máximo de entradas en `failed_attempts`. FIFO al superar.
pub const FAILED_ATTEMPTS_CAP: usize = 5;

// ============================================
// apply_state_patches
// ============================================

/// Aplica una lista de patches en orden sobre `state`. Semántica LWW dentro
/// de la lista (el último `SetIntent` gana, el último `SetCurrentStep` gana).
/// Siempre setea `updated_at = Utc::now()`.
///
/// Pura (no hay I/O). El dispatch la llama después del chain loop con la
/// unión de todos los patches del turno.
pub fn apply_state_patches(
    mut state: WaConversationAiState,
    patches: &[StatePatch],
) -> WaConversationAiState {
    for p in patches {
        match p {
            StatePatch::SetIntent { intent, confidence } => {
                state.current_intent = Some(intent.clone());
                state.intent_confidence = Some(*confidence);
            }
            StatePatch::SetCollectedData { key, value } => {
                let truncated = truncate_chars(value, COLLECTED_DATA_VALUE_CHAR_CAP);
                if truncated.len() < value.len() {
                    tracing::warn!(
                        "[ai_agent.state] collected_data value truncado a {} chars (key='{}')",
                        COLLECTED_DATA_VALUE_CHAR_CAP,
                        key
                    );
                }
                // Actualizar la llave si ya existe; insertar si hay espacio.
                if state.collected_data.contains_key(key)
                    || state.collected_data.len() < COLLECTED_DATA_KEY_CAP
                {
                    state.collected_data.insert(key.clone(), truncated);
                } else {
                    tracing::warn!(
                        "[ai_agent.state] collected_data cap ({}) alcanzado; descartando key='{}'",
                        COLLECTED_DATA_KEY_CAP,
                        key
                    );
                }
            }
            StatePatch::AddCompletedAction(name) => {
                if !state.completed_actions.iter().any(|a| a == name) {
                    state.completed_actions.push(name.clone());
                    // FIFO trim al superar el cap.
                    while state.completed_actions.len() > COMPLETED_ACTIONS_CAP {
                        state.completed_actions.remove(0);
                    }
                }
            }
            StatePatch::SetCurrentStep(s) => {
                state.current_step = Some(s.clone());
            }
            StatePatch::AddFailedAttempt { tool, error } => {
                state.failed_attempts.push(FailedAttempt {
                    tool: tool.clone(),
                    error: error.clone(),
                    at: Utc::now(),
                });
                // FIFO trim al superar el cap.
                while state.failed_attempts.len() > FAILED_ATTEMPTS_CAP {
                    state.failed_attempts.remove(0);
                }
            }
        }
    }
    state.updated_at = Utc::now();
    state
}

// ============================================
// format_conversation_state
// ============================================

/// Formatea el estado IA como el cuerpo del bloque `[conversation_state]`.
/// `build_system_instruction` agrega la cabecera del bloque.
/// Los campos vacíos / None se omiten para mantener el prompt lean.
pub fn format_conversation_state(state: &WaConversationAiState) -> String {
    let mut lines: Vec<String> = Vec::new();

    if let Some(intent) = &state.current_intent {
        lines.push(format!("current_intent: {}", intent));
    }
    if let Some(conf) = state.intent_confidence {
        lines.push(format!("intent_confidence: {:.2}", conf));
    }
    if !state.collected_data.is_empty() {
        let pairs: Vec<String> = state
            .collected_data
            .iter()
            .map(|(k, v)| format!("  {}: {}", k, v))
            .collect();
        lines.push(format!("collected_data:\n{}", pairs.join("\n")));
    }
    if !state.pending_data.is_empty() {
        lines.push(format!("pending_data: {}", state.pending_data.join(", ")));
    }
    if !state.completed_actions.is_empty() {
        lines.push(format!(
            "completed_actions: {}",
            state.completed_actions.join(", ")
        ));
    }
    if let Some(step) = &state.current_step {
        lines.push(format!("current_step: {}", step));
    }
    if !state.failed_attempts.is_empty() {
        // Mostrar sólo el nombre de la tool (no el error completo) para no
        // inflar el prompt con mensajes de error técnicos.
        let recent: Vec<String> = state
            .failed_attempts
            .iter()
            .map(|f| f.tool.clone())
            .collect();
        lines.push(format!("recent_failed_attempts: [{}]", recent.join(", ")));
    }

    lines.join("\n")
}

// ============================================
// slugify_label
// ============================================

/// Convierte una etiqueta de agente en un slug para `current_step`.
///
/// Ejemplos:
/// - `"Ventas y Contrataciones"` → `"ventas_y_contrataciones"`
/// - `"  -- Soporte --  "` → `"soporte"`
/// - `"área de pagos"` → `"area_de_pagos"`
pub fn slugify_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_is_underscore = true; // empieza en true para omitir leading `_`
    for c in label.chars() {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_is_underscore = false;
        } else if !prev_is_underscore {
            out.push('_');
            prev_is_underscore = true;
        }
    }
    // Quitar trailing `_` si quedó.
    while out.ends_with('_') {
        out.pop();
    }
    out
}

// ============================================
// Helpers privados
// ============================================

/// Trunca `s` a `max` caracteres Unicode. Si `s.chars().count() <= max`
/// devuelve una copia directa (sin conversión intermedia).
#[inline]
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
