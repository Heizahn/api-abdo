use utoipa::OpenApi;

use crate::models::auth::{
    LoginRequest, LoginResponse, RefreshRequest, RefreshResponse,
    TokenPair, VerifyNumberRequest, VerifyNumberResponse,
};
use crate::models::whatsapp::{
    ConversationDetailResponse, ConversationItem, ConversationMessagesResponse,
    ConversationsListResponse, CreateSettingsRequest, MessageItem, SendMessageRequest,
    SendMessageResponse, SettingsItem, SettingsListResponse, SettingsResponse,
    TakeConversationResponse, TransferConversationRequest, TransferableAgentItem,
    TransferableAgentsResponse, UpdateResponse, UpdateSettingsRequest,
};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "API ABDO",
        version = "0.2.0",
        description = "API REST para gestión de clientes ISP. Autenticación via JWT.\n\n\
            **Clientes**: usar token del header `Authorization: Bearer <token>`\n\
            **Staff/Admin**: misma cabecera, token emitido por `/v1/auth-user/login`"
    ),
    paths(
        // Auth — Clientes
        crate::modules::auth_client::handler::verify_number_handler,
        crate::modules::auth_client::handler::login_handler,
        crate::modules::auth_client::handler::refresh_handler,
        // WhatsApp — Soporte
        crate::modules::whatsapp::handler::list_conversations_handler,
        crate::modules::whatsapp::handler::get_conversation_handler,
        crate::modules::whatsapp::handler::get_conversation_messages_handler,
        crate::modules::whatsapp::handler::send_message_handler,
        crate::modules::whatsapp::handler::take_conversation_handler,
        crate::modules::whatsapp::handler::transfer_conversation_handler,
        crate::modules::whatsapp::handler::close_conversation_handler,
        crate::modules::whatsapp::handler::list_transferable_agents_handler,
        crate::modules::whatsapp::handler::list_settings_handler,
        crate::modules::whatsapp::handler::create_settings_handler,
        crate::modules::whatsapp::handler::update_settings_handler,
        crate::modules::whatsapp::handler::delete_settings_handler,
    ),
    components(
        schemas(
            // Auth
            VerifyNumberRequest, VerifyNumberResponse,
            LoginRequest, LoginResponse,
            RefreshRequest, RefreshResponse,
            TokenPair,
            // WhatsApp — Requests
            SendMessageRequest, TransferConversationRequest,
            CreateSettingsRequest, UpdateSettingsRequest,
            // WhatsApp — Responses
            ConversationsListResponse,
            ConversationDetailResponse,
            ConversationMessagesResponse,
            SendMessageResponse,
            TakeConversationResponse,
            TransferableAgentsResponse,
            SettingsListResponse, SettingsResponse,
            UpdateResponse,
            // WhatsApp — Items
            ConversationItem, MessageItem, SettingsItem,
            TransferableAgentItem,
        )
    ),
    tags(
        (name = "Auth — Clientes", description = "Autenticación de clientes vía teléfono + OTP"),
        (name = "WhatsApp — Soporte", description = "Chat de soporte vía WhatsApp Business API"),
    )
)]
pub struct ApiDoc;
