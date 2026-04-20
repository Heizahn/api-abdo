use utoipa::OpenApi;

use crate::models::auth::{
    LoginRequest, LoginResponse, RefreshRequest, RefreshResponse,
    TokenPair, VerifyNumberRequest, VerifyNumberResponse,
};
use crate::models::whatsapp::{
    AssignConversationRequest, ConversationDetail, ConversationListItem,
    ConversationMessagesResponse, ConversationsListResponse, CreateSettingsRequest,
    MessageItem, SendMessageRequest, SendMessageResponse, SettingsItem, SettingsListResponse,
    SettingsResponse, UpdateConversationStatusRequest, UpdateResponse, UpdateSettingsRequest,
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
        crate::modules::whatsapp::handler::get_conversation_messages_handler,
        crate::modules::whatsapp::handler::send_message_handler,
        crate::modules::whatsapp::handler::update_status_handler,
        crate::modules::whatsapp::handler::assign_conversation_handler,
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
            // WhatsApp
            SendMessageRequest, SendMessageResponse,
            UpdateConversationStatusRequest,
            AssignConversationRequest,
            ConversationsListResponse,
            ConversationListItem,
            ConversationMessagesResponse,
            ConversationDetail,
            MessageItem,
            UpdateResponse,
            CreateSettingsRequest, UpdateSettingsRequest,
            SettingsListResponse, SettingsResponse, SettingsItem,
        )
    ),
    tags(
        (name = "Auth — Clientes", description = "Autenticación de clientes vía teléfono + OTP"),
        (name = "WhatsApp — Soporte", description = "Chat de soporte vía WhatsApp Business API"),
    )
)]
pub struct ApiDoc;
