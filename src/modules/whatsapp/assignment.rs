use mongodb::bson::oid::ObjectId;
use std::collections::HashSet;
use std::sync::Arc;

use crate::{
    db::{UserRepository, WhatsAppRepository},
    state::AppState,
};

use super::ws::{broadcast_all, WsServerEvent};

/// Selecciona el agente con menor carga y le asigna la conversación.
/// La lista de candidatos se re-resuelve desde `WaSettings` usando el
/// `business_phone` persistido en la conversación: la auto-asignación nunca
/// confía en una lista de agentes pasada por el caller.
///
/// Estrategia: prefiere agentes **online** (presentes en `WsRegistry`) sobre
/// los que están desconectados, incluso si tienen más carga. Si nadie está
/// online, hace fallback a min-load global para que la conv quede asignada y
/// aparezca en la cola del agente cuando se conecte.
///
/// Importante: esta auto-asignación NO implica que un humano haya "tomado" la
/// conversación. La conv permanece en `pending` para que la IA pueda atender
/// primero y el front no la marque prematuramente como `in_progress`.
pub async fn assign_conversation(state: Arc<AppState>, conv_id: ObjectId) {
    let conv_id_str = conv_id.to_hex();

    // Lock de asignación para evitar duplicados
    if !state.redis.try_lock_conversation(&conv_id_str).await {
        tracing::debug!("[assignment] lock ocupado para conv {}, skip", conv_id_str);
        return;
    }

    let event = async {
        // Re-leer conversación dentro del lock — otro webhook pudo haberla asignado ya.
        let conv = match state.db.find_conversation_by_id(&conv_id).await {
            Ok(Some(c)) if c.assigned_to.is_some() => {
                tracing::debug!("[assignment] conv {} ya asignada, skip", conv_id_str);
                return None;
            }
            Ok(Some(c)) => c,
            Ok(None) => {
                tracing::warn!("[assignment] conv {} no existe, skip", conv_id_str);
                return None;
            }
            Err(e) => {
                tracing::error!("[assignment] error releyendo conv {}: {}", conv_id_str, e);
                return None;
            }
        };

        let settings = match state.db.find_wa_settings_by_phone(&conv.business_phone).await {
            Ok(Some(settings)) => settings,
            Ok(None) => {
                tracing::warn!(
                    "[assignment] workspace no encontrado conv={} business_phone={}",
                    conv_id_str,
                    conv.business_phone
                );
                return None;
            }
            Err(e) => {
                tracing::error!(
                    "[assignment] error cargando workspace conv={} business_phone={}: {}",
                    conv_id_str,
                    conv.business_phone,
                    e
                );
                return None;
            }
        };
        let configured_agents = settings.agents;
        if configured_agents.is_empty() {
            tracing::warn!(
                "[assignment] workspace sin agentes configurados conv={} business_phone={}",
                conv_id_str,
                conv.business_phone
            );
            return None;
        }

        let mut eligible_agents = Vec::with_capacity(configured_agents.len());
        for agent_id in configured_agents {
            match state.db.find_user_by_id(&agent_id).await {
                Ok(Some(user)) if user.visible && user.can_chat && !user.is_bot => {
                    eligible_agents.push(agent_id);
                }
                Ok(Some(user)) => {
                    tracing::warn!(
                        "[assignment] agente no elegible en WaSettings conv={} agent={} visible={} can_chat={} is_bot={}",
                        conv_id_str,
                        agent_id,
                        user.visible,
                        user.can_chat,
                        user.is_bot
                    );
                }
                Ok(None) => {
                    tracing::warn!(
                        "[assignment] agente configurado no existe conv={} agent={}",
                        conv_id_str,
                        agent_id
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "[assignment] no se pudo validar agente conv={} agent={}: {}",
                        conv_id_str,
                        agent_id,
                        e
                    );
                }
            }
        }
        if eligible_agents.is_empty() {
            tracing::warn!(
                "[assignment] sin agentes elegibles para conv {}",
                conv_id_str
            );
            return None;
        }

        // Snapshot de agentes online (presentes en WsRegistry).
        let online: HashSet<String> = {
            let map = state.ws_registry.read().await;
            map.keys().cloned().collect()
        };

        // Filtrar candidatos por online primero. Si ninguno está conectado,
        // hacer fallback a la lista completa para que la conv no quede huérfana.
        let online_candidates: Vec<String> = eligible_agents
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
                eligible_agents, conv_id_str
            );
            &eligible_agents
        };

        // Cargar carga de cada candidato desde Redis
        let mut loads = Vec::with_capacity(candidates.len());
        for agent_id in candidates {
            let load = state.redis.get_agent_load(agent_id).await;
            loads.push((agent_id.clone(), load));
        }

        // Elegir el menos ocupado entre los candidatos
        let (chosen_agent, _) = loads.iter().min_by_key(|(_, load)| *load).cloned().unwrap();

        tracing::info!(
            "[assignment] asignando conv {} a agente {} (online_filter={}, cargas: {:?})",
            conv_id_str,
            chosen_agent,
            used_online_filter,
            loads
        );

        match state.db.find_wa_settings_by_phone(&conv.business_phone).await {
            Ok(Some(settings)) if settings.agents.iter().any(|agent_id| agent_id == &chosen_agent) => {}
            Ok(Some(_)) => {
                tracing::warn!(
                    "[assignment] agente elegido ya no pertenece al workspace conv={} agent={} business_phone={} — skip",
                    conv_id_str,
                    chosen_agent,
                    conv.business_phone
                );
                return None;
            }
            Ok(None) => {
                tracing::warn!(
                    "[assignment] workspace desapareció antes de asignar conv={} business_phone={} — skip",
                    conv_id_str,
                    conv.business_phone
                );
                return None;
            }
            Err(e) => {
                tracing::error!(
                    "[assignment] error revalidando workspace conv={} business_phone={}: {}",
                    conv_id_str,
                    conv.business_phone,
                    e
                );
                return None;
            }
        }

        // Actualizar MongoDB
        if let Err(e) = state
            .db
            .assign_conversation(&conv_id, Some(&chosen_agent))
            .await
        {
            tracing::error!("[assignment] error actualizando MongoDB: {}", e);
            return None;
        }

        // Incrementar carga del agente
        state.redis.incr_agent_load(&chosen_agent).await;

        // Resolver el nombre del agente para que el front pueda patchear la
        // sidebar sin necesitar refetch.
        let taken_by_name = state
            .db
            .find_user_by_id(&chosen_agent)
            .await
            .ok()
            .flatten()
            .map(|u| u.name);

        // Broadcast: conversación auto-asignada. Aunque reutilizamos CHAT_TOMADO
        // para patchear caches del front, el status real sigue siendo `pending`.
        Some(WsServerEvent::ChatTomado {
            conversation_id: conv_id_str.clone(),
            taken_by: chosen_agent,
            taken_by_name,
            status: "pending".to_string(),
            previous_status: "pending".to_string(),
        })
    }
    .await;

    state.redis.release_conversation_lock(&conv_id_str).await;

    if let Some(event) = event {
        broadcast_all(&state.ws_registry, &event).await;
    }
}
