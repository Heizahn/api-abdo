//! Conversation REST handlers (routes + OpenAPI entry points) for PR2.
//!
//! The concrete implementation is currently hosted in `crate::modules::whatsapp::handler`
//! and re-exported here to complete the ownership migration without changing
//! semantics.

pub use crate::modules::whatsapp::handler::{
    __path_close_conversation_handler, __path_get_conversation_messages_handler,
    __path_initiate_conversation_handler, __path_intervene_conversation_handler,
    __path_list_transferable_agents_handler, __path_mark_read_handler,
    __path_reopen_conversation_handler, __path_reset_ai_conv_state_handler,
    __path_send_message_handler, __path_take_conversation_handler,
    __path_transfer_conversation_handler, close_conversation_handler,
    get_conversation_messages_handler, initiate_conversation_handler,
    intervene_conversation_handler, list_transferable_agents_handler, mark_read_handler,
    reopen_conversation_handler, reset_ai_conv_state_handler, send_message_handler,
    take_conversation_handler, transfer_conversation_handler,
};

pub use crate::modules::whatsapp::conversations::queries::{
    __path_conversations_stats_handler, __path_get_conversation_client_link_handler,
    __path_get_conversation_handler, __path_list_conversations_handler,
    conversations_stats_handler, get_conversation_client_link_handler, get_conversation_handler,
    list_conversations_handler,
};
