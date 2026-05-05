use mongodb::bson::oid::ObjectId;
use std::collections::HashSet;
use std::sync::Arc;

use crate::{db::WhatsAppRepository, state::AppState};

use super::ws::{broadcast_all, WsServerEvent};

/// Selecciona el agente con menor carga y le asigna la conversación.
/// `agents` viene de la configuración `wa_settings` del número que originó el mensaje.
///
/// Estrategia: prefiere agentes **online** (presentes en `WsRegistry`) sobre
/// los que están desconectados, incluso si tienen más carga. Si nadie está
/// online, hace fallback a min-load global para que la conv quede asignada y
/// aparezca en la cola del agente cuando se conecte.
pub async fn assign_conversation(state: Arc<AppState>, conv_id: ObjectId, agents: Vec<String>) {
    let conv_id_str = conv_id.to_hex();

    // Lock de asignación para evitar duplicados
    if !state.redis.try_lock_conversation(&conv_id_str).await {
        tracing::debug!("[assignment] lock ocupado para conv {}, skip", conv_id_str);
        return;
    }

    // Re-leer conversación dentro del lock — otro webhook pudo haberla asignado ya
    match state.db.find_conversation_by_id(&conv_id).await {
        Ok(Some(c)) if c.assigned_to.is_some() => {
            tracing::debug!("[assignment] conv {} ya asignada, skip", conv_id_str);
            return;
        }
        Ok(None) => {
            tracing::warn!("[assignment] conv {} no existe, skip", conv_id_str);
            return;
        }
        Err(e) => {
            tracing::error!("[assignment] error releyendo conv {}: {}", conv_id_str, e);
            return;
        }
        _ => {}
    }

    if agents.is_empty() {
        tracing::warn!("[assignment] lista de agentes vacía para conv {}", conv_id_str);
        return;
    }

    // Snapshot de agentes online (presentes en WsRegistry).
    let online: HashSet<String> = {
        let map = state.ws_registry.read().await;
        map.keys().cloned().collect()
    };

    // Filtrar candidatos por online primero. Si ninguno está conectado,
    // hacer fallback a la lista completa para que la conv no quede huérfana.
    let online_candidates: Vec<String> = agents
        .iter()
        .filter(|a| online.contains(*a))
        .cloned()
        .collect();
    let used_online_filter = !online_candidates.is_empty();
    let candidates: &[String] = if used_online_filter {
        &online_candidates
    } else {
        tracing::warn!(
            "[assignment] ningún agente online (configurados={:?}) — fallback a min-load global para conv {}",
            agents, conv_id_str
        );
        &agents
    };

    // Cargar carga de cada candidato desde Redis
    let mut loads = Vec::with_capacity(candidates.len());
    for agent_id in candidates {
        let load = state.redis.get_agent_load(agent_id).await;
        loads.push((agent_id.clone(), load));
    }

    // Elegir el menos ocupado entre los candidatos
    let (chosen_agent, _) = loads
        .iter()
        .min_by_key(|(_, load)| *load)
        .cloned()
        .unwrap();

    tracing::info!(
        "[assignment] asignando conv {} a agente {} (online_filter={}, cargas: {:?})",
        conv_id_str, chosen_agent, used_online_filter, loads
    );

    // Actualizar MongoDB
    if let Err(e) = state.db.assign_conversation(&conv_id, Some(&chosen_agent)).await {
        tracing::error!("[assignment] error actualizando MongoDB: {}", e);
        return;
    }

    // Incrementar carga del agente
    state.redis.incr_agent_load(&chosen_agent).await;

    // Resolver el nombre del agente para que el front pueda patchear la
    // sidebar sin necesitar refetch.
    use crate::db::UserRepository;
    let taken_by_name = state
        .db
        .find_user_by_id(&chosen_agent)
        .await
        .ok()
        .flatten()
        .map(|u| u.name);

    // Broadcast: conversación tomada (auto-asignada). El front filtra por `taken_by`.
    let event = WsServerEvent::ChatTomado {
        conversation_id: conv_id_str,
        taken_by: chosen_agent,
        taken_by_name,
        status: "in_progress".to_string(),
        previous_status: "pending".to_string(),
    };
    broadcast_all(&state.ws_registry, &event).await;
}
