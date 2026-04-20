use mongodb::bson::oid::ObjectId;
use std::sync::Arc;

use crate::{db::WhatsAppRepository, state::AppState};

use super::ws::{broadcast_all, send_to_agent, WsServerEvent};

/// Lee la lista de IDs de agentes desde la variable de entorno WA_AGENT_IDS.
/// Formato: "uuid1,uuid2,uuid3"
pub fn get_agent_ids() -> Vec<String> {
    std::env::var("WA_AGENT_IDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Selecciona el agente con menor carga y le asigna la conversación.
/// Usa un lock Redis para evitar doble asignación en condición de carrera.
pub async fn assign_conversation(state: Arc<AppState>, conv_id: ObjectId) {
    let conv_id_str = conv_id.to_hex();

    // Lock de asignación para evitar duplicados
    if !state.redis.try_lock_conversation(&conv_id_str).await {
        tracing::debug!("[assignment] lock ocupado para conv {}, skip", conv_id_str);
        return;
    }

    let agents = get_agent_ids();
    if agents.is_empty() {
        tracing::warn!("[assignment] WA_AGENT_IDS vacío, no se puede asignar conv {}", conv_id_str);
        return;
    }

    // Cargar carga de cada agente desde Redis
    let mut loads = Vec::with_capacity(agents.len());
    for agent_id in &agents {
        let load = state.redis.get_agent_load(agent_id).await;
        loads.push((agent_id.clone(), load));
    }

    // Elegir el menos ocupado
    let (chosen_agent, _) = loads
        .iter()
        .min_by_key(|(_, load)| *load)
        .cloned()
        .unwrap();

    tracing::info!(
        "[assignment] asignando conv {} a agente {} (cargas: {:?})",
        conv_id_str, chosen_agent, loads
    );

    // Actualizar MongoDB
    if let Err(e) = state.db.assign_conversation(&conv_id, Some(&chosen_agent)).await {
        tracing::error!("[assignment] error actualizando MongoDB: {}", e);
        return;
    }

    // Incrementar carga del agente
    state.redis.incr_agent_load(&chosen_agent).await;

    // Obtener conversación actualizada para enviar al front
    let conv = match state.db.find_conversation_by_id(&conv_id).await {
        Ok(Some(c)) => c,
        _ => return,
    };

    let event = WsServerEvent::MensajeAsignado {
        conversation_id: conv_id_str.clone(),
        phone: conv.phone.clone(),
        name: conv.name.clone(),
        last_message_preview: conv.last_message_preview.clone(),
        assigned_to: chosen_agent.clone(),
    };

    // Notificar al agente asignado
    send_to_agent(&state.ws_registry, &chosen_agent, &event).await;

    // Broadcast a todos para que actualicen la lista de conversaciones
    let broadcast = WsServerEvent::ConversacionActualizada {
        conversation_id: conv_id_str,
        status: conv.status,
        assigned_to: Some(chosen_agent),
        unread_count: conv.unread_count,
    };
    broadcast_all(&state.ws_registry, &broadcast).await;
}
