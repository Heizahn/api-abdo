use utoipa::OpenApi;

use crate::db::mongo::{PaymentDetails, ResultGroupedByDate};
use crate::models::auth::{
    LoginRequest, LoginResponse, RefreshRequest, RefreshResponse,
    TokenPair, VerifyNumberRequest, VerifyNumberResponse,
};
use crate::models::db::{
    BcvResponse, ClientDetail, ClientListItem, ClientOnu, ClientStatusHistoryItem,
    CustomerInfoItem, LatestPayment, LatestVersion, LatestVersionResponse, PingResponse,
    SolvencyCounts,
};
use crate::models::payment::{
    Bank, BankListResponse, CheckReferenceData, CheckReferenceRequest, CheckReferenceResponse,
    PagoMovilData, PaymentMethodResponse, ReferenceDetails,
};
use crate::models::profile::{ClientData, ClientSummary, MeGroupResponse, MePhoneResponse};
use crate::models::receivable::{
    PaymentData, ReceivableByIdResponse, ReceivableData, ReceivablesResponse, RejectedPayment,
    RejectedPaymentsResponse,
};
use crate::models::users::{
    ChangeMyPasswordRequest, CreateUserBody, OkResponse, ProviderResponse, RefreshTokenRequest,
    RefreshTokenResponse, SetUserPasswordRequest, SetUserVisibleRequest, UpdateUserRequest,
    UserItem, UserListResponse, UserLoginRequest, UserLoginResponse, UserResponse,
    UserResponseEnvelope,
};
use crate::models::whatsapp::{
    ConversationDetailResponse, ConversationItem, ConversationMessagesResponse, ConversationStats,
    ConversationStatsResponse, ConversationsListResponse, CreateQuickReplyRequest,
    CreateSettingsRequest, DuplicateQuickReplyRequest, InitiateConversationRequest,
    LocationPayload, MarkReadResponse, MessageItem, QuickRepliesListResponse, QuickReplyButton,
    QuickReplyCtaUrl, QuickReplyHeader, QuickReplyItem, QuickReplyList, QuickReplyListRow,
    QuickReplyListSection, QuickReplyResponse, ReplyToItem, SendMessageRequest, SendMessageResponse,
    SendTemplatePayload, SettingsItem, SettingsListResponse, SettingsResponse,
    TakeConversationResponse, TemplatesListResponse, ToggleActiveRequest,
    TransferConversationRequest, TransferableAgentItem, TransferableAgentsResponse,
    UpdateQuickReplyRequest, UpdateResponse, UpdateSettingsRequest, UrlPreview, WaPurposeConfig,
    WaPurposes, WaPurposesPatch, WhatsAppTemplate,
};
use crate::models::zabbix::{MonthlyTraffic, ZabbixTrafficResponse};
use crate::modules::calculations::handler::{
    CalculationRequest, CalculationRequestV2, CalculationResponse, CalculationResponseV2, Currency,
};
use crate::modules::dashboard::handler::{MonthlyClosingData, MonthlyClosingResponse};

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
        // Auth — Staff
        crate::modules::auth_user::handler::login_handler,
        crate::modules::auth_user::handler::refresh_token_handler,
        crate::modules::auth_user::handler::me_handler,
        // Profile — Clientes
        crate::modules::profile::handler::me_group_handler,
        crate::modules::profile::handler::me_phone_handler,
        // Receivables — Clientes
        crate::modules::receivables::handler::me_receivables_handler,
        crate::modules::receivables::handler::me_paid_receivables_handler,
        crate::modules::receivables::handler::get_receivable_by_id_handler,
        crate::modules::receivables::handler::get_rejected_payments_by_receivable_handler,
        // Payments
        crate::modules::payments::handler::get_pago_movil_data_handler,
        crate::modules::payments::handler::get_pago_movil_data_by_client_handler,
        crate::modules::payments::handler::get_pago_movil_data_by_client_user_handler,
        crate::modules::payments::handler::report_payment_handler,
        crate::modules::payments::handler::report_payment_user_handler,
        crate::modules::auth_user::handler::check_reference_handler,
        // Dashboard
        crate::modules::dashboard::handler::latest_payments_handler,
        crate::modules::dashboard::handler::solvency_handler,
        crate::modules::dashboard::handler::monthly_closing_handler,
        // Clients — Staff
        crate::modules::clients::handler::get_all_clients_handler,
        crate::modules::clients::handler::get_client_by_id_handler,
        crate::modules::clients::handler::get_customers_info_handler,
        crate::modules::clients::handler::get_status_history_handler,
        // Calculations
        crate::modules::calculations::handler::calculate_bs_handler,
        crate::modules::calculations::handler::calculate_handler,
        // Providers
        crate::modules::providers::handler::get_agents_handler,
        crate::modules::providers::handler::get_providers_handler,
        // Utils
        crate::modules::api_utils::handler::get_ping_response,
        crate::modules::api_utils::handler::get_latest_version_response,
        crate::modules::api_utils::handler::get_privacy_policy,
        crate::modules::api_utils::handler::get_bcv,
        crate::modules::api_utils::handler::get_bank_list,
        crate::modules::api_utils::handler::get_bank_list_user,
        crate::modules::api_utils::handler::get_image,
        crate::modules::api_utils::handler::get_ip_pppoe,
        crate::modules::api_utils::handler::get_zabbix,
        // WhatsApp — Soporte
        crate::modules::whatsapp::handler::list_conversations_handler,
        crate::modules::whatsapp::handler::conversations_stats_handler,
        crate::modules::whatsapp::handler::get_conversation_handler,
        crate::modules::whatsapp::handler::get_conversation_messages_handler,
        crate::modules::whatsapp::handler::send_message_handler,
        crate::modules::whatsapp::handler::mark_read_handler,
        crate::modules::whatsapp::handler::take_conversation_handler,
        crate::modules::whatsapp::handler::transfer_conversation_handler,
        crate::modules::whatsapp::handler::close_conversation_handler,
        crate::modules::whatsapp::handler::initiate_conversation_handler,
        crate::modules::whatsapp::handler::list_transferable_agents_handler,
        crate::modules::whatsapp::handler::list_settings_handler,
        crate::modules::whatsapp::handler::create_settings_handler,
        crate::modules::whatsapp::handler::update_settings_handler,
        crate::modules::whatsapp::handler::delete_settings_handler,
        crate::modules::whatsapp::handler::get_media_handler,
        crate::modules::whatsapp::handler::list_quick_replies_handler,
        crate::modules::whatsapp::handler::create_quick_reply_handler,
        crate::modules::whatsapp::handler::update_quick_reply_handler,
        crate::modules::whatsapp::handler::delete_quick_reply_handler,
        crate::modules::whatsapp::handler::set_quick_reply_active_handler,
        crate::modules::whatsapp::handler::duplicate_quick_reply_handler,
        crate::modules::whatsapp::handler::list_templates_handler,
        // Users — CRUD
        crate::modules::users::handler::list_users_handler,
        crate::modules::users::handler::create_user_handler,
        crate::modules::users::handler::set_user_visible_handler,
        crate::modules::users::handler::update_user_handler,
        crate::modules::users::handler::set_user_password_handler,
        crate::modules::users::handler::change_my_password_handler,
    ),
    components(
        schemas(
            // Auth — Clientes
            VerifyNumberRequest, VerifyNumberResponse,
            LoginRequest, LoginResponse,
            RefreshRequest, RefreshResponse,
            TokenPair,
            // Auth — Staff
            UserLoginRequest, UserLoginResponse,
            RefreshTokenRequest, RefreshTokenResponse,
            UserResponse,
            // Profile — Clientes
            MeGroupResponse, MePhoneResponse, ClientSummary, ClientData,
            ResultGroupedByDate, PaymentDetails,
            // Receivables
            ReceivablesResponse, ReceivableByIdResponse, ReceivableData, PaymentData,
            RejectedPayment, RejectedPaymentsResponse,
            // Payments
            PaymentMethodResponse, PagoMovilData,
            CheckReferenceRequest, CheckReferenceResponse, CheckReferenceData, ReferenceDetails,
            // Dashboard
            LatestPayment, SolvencyCounts, MonthlyClosingResponse, MonthlyClosingData,
            // Clients — Staff
            ClientDetail, ClientOnu, ClientListItem, ClientStatusHistoryItem, CustomerInfoItem,
            // Calculations
            CalculationRequest, CalculationResponse, CalculationRequestV2, CalculationResponseV2,
            Currency,
            // Providers
            ProviderResponse,
            // Utils
            PingResponse, LatestVersionResponse, LatestVersion, BcvResponse,
            Bank, BankListResponse, ZabbixTrafficResponse, MonthlyTraffic,
            // WhatsApp — Requests
            SendMessageRequest, SendTemplatePayload, InitiateConversationRequest,
            TransferConversationRequest,
            CreateSettingsRequest, UpdateSettingsRequest,
            WaPurposeConfig, WaPurposes, WaPurposesPatch,
            CreateQuickReplyRequest, UpdateQuickReplyRequest, DuplicateQuickReplyRequest,
            ToggleActiveRequest,
            QuickReplyHeader, QuickReplyButton, QuickReplyList, QuickReplyListSection,
            QuickReplyListRow, QuickReplyCtaUrl,
            // WhatsApp — Responses
            ConversationsListResponse,
            ConversationDetailResponse,
            ConversationMessagesResponse,
            ConversationStats, ConversationStatsResponse,
            SendMessageResponse,
            MarkReadResponse,
            TakeConversationResponse,
            TransferableAgentsResponse,
            SettingsListResponse, SettingsResponse,
            QuickRepliesListResponse, QuickReplyResponse,
            TemplatesListResponse,
            UpdateResponse,
            // Users — CRUD
            UserItem, UserListResponse, UserResponseEnvelope, OkResponse,
            SetUserVisibleRequest, UpdateUserRequest, CreateUserBody, SetUserPasswordRequest,
            ChangeMyPasswordRequest,
            // WhatsApp — Items
            ConversationItem, MessageItem, SettingsItem,
            TransferableAgentItem, ReplyToItem, UrlPreview, LocationPayload, QuickReplyItem,
            WhatsAppTemplate,
        )
    ),
    tags(
        (name = "Auth — Clientes", description = "Autenticación de clientes vía teléfono + OTP"),
        (name = "Auth — Staff", description = "Autenticación staff/admin vía email + password"),
        (name = "Profile — Clientes", description = "Perfil del cliente autenticado (teléfono + cuentas asociadas)"),
        (name = "Receivables — Clientes", description = "Deudas del cliente autenticado (activas, pagadas, rechazos)"),
        (name = "Payments", description = "Métodos de pago móvil, reporte de pagos, validación de referencias"),
        (name = "Dashboard", description = "Métricas agregadas: solvencia, últimos pagos, cierre mensual"),
        (name = "Clients — Staff", description = "Gestión y consulta de clientes (para staff/admin/provider)"),
        (name = "Calculations", description = "Conversiones USD↔Bs con tasa BCV e IVA"),
        (name = "Providers", description = "Listados de agentes (staff) y providers"),
        (name = "Utils", description = "Helpers: ping, versión, BCV, bancos, IP PPPoE, Zabbix, imágenes, política de privacidad"),
        (name = "WhatsApp — Soporte", description = "Chat de soporte vía WhatsApp Business API"),
        (name = "Users — CRUD", description = "Gestión de usuarios (staff/admin). Requiere rol SUPERADMIN (nRole == 0.0) salvo `/me/password`."),
    )
)]
pub struct ApiDoc;
