use utoipa::OpenApi;

use crate::db::mongo::{PaymentDetails, ResultGroupedByDate};
use crate::models::ai_agent::{
    AiAgentDeleteResponse, AiAgentFaqItem, AiAgentFaqListResponse, AiAgentFaqResponse, AiAgentItem,
    AiAgentMetricsDailyBucketDto, AiAgentMetricsData, AiAgentMetricsResponse, AiAgentMode,
    AiAgentPreClassBreakdown, AiAgentPurpose, AiAgentResponse, AiAgentsListResponse,
    AiBusinessDataDeleteResponse, AiConfigDto, AiConfigPatchRequest, AiConfigResponse,
    AiCoverageZoneItem, AiCoverageZoneResponse, AiCoverageZonesListResponse, AiEscalationRulesDto,
    AiEscalationRulesInput, AiInstallationConfigItem, AiInstallationConfigResponse,
    AiInstallationConfigsListResponse, AiLimitsDto, AiLimitsInput, AiModelConfigDto,
    AiModelConfigInput, AiPersonalityDto, AiPersonalityInput, AiPlanItem, AiPlanResponse,
    AiPlansListResponse, AiPromotionItem, AiPromotionResponse, AiPromotionsListResponse,
    AiScheduleDto, AiScheduleInput, AiToolConfigDto, AiToolConfigInput, ConnectionType,
    CreateAiAgentFaqRequest, CreateAiAgentRequest, CreateAiCoverageZoneRequest,
    CreateAiPlanRequest, CreateAiPromotionRequest, PoliticalDivisionItem,
    PoliticalDivisionsResponse, TestConnectionData, TestConnectionRequest, TestConnectionResponse,
    TestConnectionSource, UpdateAiAgentFaqRequest, UpdateAiAgentRequest,
    UpdateAiCoverageZoneRequest, UpdateAiInstallationConfigRequest, UpdateAiPlanRequest,
    UpdateAiPromotionRequest,
};
use crate::models::auth::{
    LoginRequest, LoginResponse, RefreshRequest, RefreshResponse, TokenPair, VerifyNumberRequest,
    VerifyNumberResponse,
};
use crate::models::db::{
    BcvResponse, ClientDetail, ClientListItem, ClientOnu, ClientStatusHistoryItem,
    CustomerInfoItem, DailyPaymentChartPoint, LatestPayment, LatestVersion, LatestVersionResponse,
    PaymentReportListItem, PingResponse, SolvencyCounts,
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
    ConversationClientLinkData, ConversationClientLinkItem, ConversationClientLinkResponse,
    ConversationDetailResponse, ConversationItem, ConversationMessagesResponse, ConversationStats,
    ConversationStatsResponse, ConversationsListResponse, CreateQuickReplyRequest,
    CreateSettingsRequest, CreateTicketRequest, CreateWaTemplateRequest, DeleteWaTemplateData,
    DeleteWaTemplateResponse, DuplicateQuickReplyRequest, HeaderMediaUploadData,
    HeaderMediaUploadResponse, InitiateConversationRequest, LocationPayload, MarkReadData,
    MarkReadResponse, MediaLimitsResponse, MediaSendPayload, MediaTypeLimit, MediaUploadData,
    MediaUploadResponse, MessageItem, QuickRepliesListResponse, QuickReplyButton, QuickReplyCtaUrl,
    QuickReplyHeader, QuickReplyItem, QuickReplyList, QuickReplyListRow, QuickReplyListSection,
    QuickReplyResponse, ReplyToItem, SendMessageData, SendMessageRequest, SendMessageResponse,
    SendTemplatePayload, SettingsItem, SettingsListResponse, SettingsResponse,
    TakeConversationResponse, TicketCategoriesResponse, TicketCategoryItem, TicketItem,
    TicketResponse, TicketTimelineEntryItem, TicketsListResponse, ToggleActiveRequest,
    TransferAndTicketData, TransferAndTicketRequest, TransferAndTicketResponse,
    TransferConversationRequest, TransferableAgentItem, TransferableAgentsResponse,
    TrivialResponse, UpdateQuickReplyRequest, UpdateResponse, UpdateSettingsRequest,
    UpdateTicketStatusRequest, UpdateWaTemplateRequest, UrlPreview, WaPurposeConfig,
    WaPurposeUsage, WaPurposes, WaPurposesPatch, WaTemplateButtonInput, WaTemplateCategory,
    WaTemplateHeaderInput, WaTemplateItem, WaTemplateResponse, WaTemplateStatus,
    WaTemplatesListResponse, WaTestConnectionData, WaTestConnectionRequest,
    WaTestConnectionResponse, WaTestConnectionSource, WaTicketTimelineEntry,
};
use crate::models::zabbix::{MonthlyTraffic, ZabbixTrafficResponse};
use crate::modules::ai_agent::business_data::{AiToolMetaItem, AiToolsListResponse};
use crate::modules::ai_agent::sandbox::{
    SandboxData, SandboxHistoryEntry, SandboxRequest, SandboxResponse, SandboxToolCall,
    SandboxUsage,
};
use crate::modules::calculations::handler::{
    CalculationRequest, CalculationRequestV2, CalculationResponse, CalculationResponseV2, Currency,
};
use crate::modules::dashboard::handler::{MonthlyClosingData, MonthlyClosingResponse};
use crate::modules::payments::handler::RejectReportRequest;
use crate::modules::whatsapp::handler::{InterveneData, InterveneResponse, ResetAiStateResponse};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "API ABDO",
        version = "0.3.27",
        description = "API REST para gestión de clientes ISP. Autenticación vía cookies HttpOnly.\n\n\
            **Canal recomendado**: cookies `access_token` + `refresh_token` con `Secure` y `SameSite`.\n\
            **Compatibilidad temporal**: Bearer header / body refresh / WS query token sólo durante ventana de migración."
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
        crate::modules::payments::handler::list_payment_reports_handler,
        crate::modules::payments::handler::approve_payment_report_handler,
        crate::modules::payments::handler::reject_payment_report_handler,
        crate::modules::auth_user::handler::check_reference_handler,
        // Dashboard
        crate::modules::dashboard::handler::latest_payments_handler,
        crate::modules::dashboard::handler::solvency_handler,
        crate::modules::dashboard::handler::monthly_closing_handler,

        crate::modules::dashboard::handler::payments_chart_handler,
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
        crate::modules::whatsapp::conversations::handlers::list_conversations_handler,
        crate::modules::whatsapp::conversations::handlers::conversations_stats_handler,
        crate::modules::whatsapp::conversations::handlers::get_conversation_handler,
        crate::modules::whatsapp::conversations::handlers::get_conversation_client_link_handler,
        crate::modules::whatsapp::conversations::handlers::get_conversation_messages_handler,
        crate::modules::whatsapp::conversations::handlers::send_message_handler,
        crate::modules::whatsapp::conversations::handlers::mark_read_handler,
        crate::modules::whatsapp::conversations::handlers::take_conversation_handler,
        crate::modules::whatsapp::conversations::handlers::transfer_conversation_handler,
        crate::modules::whatsapp::conversations::handlers::close_conversation_handler,
        crate::modules::whatsapp::conversations::handlers::reopen_conversation_handler,
        crate::modules::whatsapp::conversations::handlers::reset_ai_conv_state_handler,
        crate::modules::whatsapp::conversations::handlers::intervene_conversation_handler,
        crate::modules::whatsapp::conversations::handlers::initiate_conversation_handler,
        crate::modules::whatsapp::conversations::handlers::list_transferable_agents_handler,
        crate::modules::whatsapp::handler::list_settings_handler,
        crate::modules::whatsapp::handler::create_settings_handler,
        crate::modules::whatsapp::handler::update_settings_handler,
        crate::modules::whatsapp::handler::delete_settings_handler,
        crate::modules::whatsapp::handler::test_settings_connection_raw_handler,
        crate::modules::whatsapp::handler::test_settings_connection_stored_handler,
        crate::modules::whatsapp::handler::get_media_handler,
        crate::modules::whatsapp::handler::upload_media_handler,
        crate::modules::whatsapp::handler::get_media_limits_handler,
        crate::modules::whatsapp::handler::list_quick_replies_handler,
        crate::modules::whatsapp::handler::create_quick_reply_handler,
        crate::modules::whatsapp::handler::update_quick_reply_handler,
        crate::modules::whatsapp::handler::delete_quick_reply_handler,
        crate::modules::whatsapp::handler::set_quick_reply_active_handler,
        crate::modules::whatsapp::handler::duplicate_quick_reply_handler,
        crate::modules::whatsapp::handler::list_templates_handler,
        crate::modules::whatsapp::handler::create_template_handler,
        crate::modules::whatsapp::handler::get_template_handler,
        crate::modules::whatsapp::handler::update_template_handler,
        crate::modules::whatsapp::handler::delete_template_handler,
        crate::modules::whatsapp::handler::resync_template_handler,
        crate::modules::whatsapp::handler::upload_template_header_media_handler,
        crate::modules::whatsapp::handler::react_message_handler,
        // WhatsApp — Tickets
        crate::modules::whatsapp::tickets::list_ticket_categories_handler,
        crate::modules::whatsapp::tickets::list_tickets_handler,
        crate::modules::whatsapp::tickets::create_ticket_handler,
        crate::modules::whatsapp::tickets::get_ticket_handler,
        crate::modules::whatsapp::tickets::update_ticket_handler,
        crate::modules::whatsapp::tickets::transfer_and_ticket_handler,
        // WhatsApp — AI Agent
        crate::modules::ai_agent::handler::list_ai_agents_handler,
        crate::modules::ai_agent::handler::get_ai_agent_handler,
        crate::modules::ai_agent::handler::create_ai_agent_handler,
        crate::modules::ai_agent::handler::update_ai_agent_handler,
        crate::modules::ai_agent::handler::delete_ai_agent_handler,
        crate::modules::ai_agent::handler::list_ai_agent_faqs_handler,
        crate::modules::ai_agent::handler::create_ai_agent_faq_handler,
        crate::modules::ai_agent::handler::update_ai_agent_faq_handler,
        crate::modules::ai_agent::handler::delete_ai_agent_faq_handler,
        crate::modules::ai_agent::sandbox::sandbox_handler,
        crate::modules::ai_agent::handler::get_ai_agent_metrics_handler,
        crate::modules::ai_agent::handler::test_connection_raw_handler,
        crate::modules::ai_agent::handler::test_connection_for_agent_handler,
        crate::modules::ai_agent::handler::get_ai_config_handler,
        crate::modules::ai_agent::handler::patch_ai_config_handler,
        // AI Agent — datos de negocio (planes, cobertura) + discovery de tools
        crate::modules::ai_agent::business_data::list_plans_handler,
        crate::modules::ai_agent::business_data::create_plan_handler,
        crate::modules::ai_agent::business_data::update_plan_handler,
        crate::modules::ai_agent::business_data::delete_plan_handler,
        crate::modules::ai_agent::business_data::list_coverage_zones_handler,
        crate::modules::ai_agent::business_data::create_coverage_zone_handler,
        crate::modules::ai_agent::business_data::update_coverage_zone_handler,
        crate::modules::ai_agent::business_data::delete_coverage_zone_handler,
        crate::modules::ai_agent::business_data::list_political_divisions_handler,
        crate::modules::ai_agent::business_data::list_tools_handler,
        // AI Agent — instalaciones + promociones
        crate::modules::ai_agent::business_data::list_installations_handler,
        crate::modules::ai_agent::business_data::update_installation_handler,
        crate::modules::ai_agent::business_data::list_promotions_handler,
        crate::modules::ai_agent::business_data::create_promotion_handler,
        crate::modules::ai_agent::business_data::update_promotion_handler,
        crate::modules::ai_agent::business_data::delete_promotion_handler,
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
            PaymentReportListItem, RejectReportRequest,
            CheckReferenceRequest, CheckReferenceResponse, CheckReferenceData, ReferenceDetails,
            // Dashboard
            LatestPayment, SolvencyCounts, MonthlyClosingResponse, MonthlyClosingData,
            DailyPaymentChartPoint,
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
            MediaSendPayload,
            CreateSettingsRequest, UpdateSettingsRequest,
            WaPurposeConfig, WaPurposes, WaPurposesPatch,
            CreateQuickReplyRequest, UpdateQuickReplyRequest, DuplicateQuickReplyRequest,
            ToggleActiveRequest,
            QuickReplyHeader, QuickReplyButton, QuickReplyList, QuickReplyListSection,
            QuickReplyListRow, QuickReplyCtaUrl,
            // WhatsApp — Responses
            ConversationsListResponse,
            ConversationDetailResponse,
            ConversationClientLinkResponse, ConversationClientLinkData, ConversationClientLinkItem,
            ConversationMessagesResponse,
            ConversationStats, ConversationStatsResponse,
            SendMessageResponse, SendMessageData,
            MediaUploadResponse, MediaUploadData, MediaLimitsResponse, MediaTypeLimit,
            MarkReadResponse, MarkReadData,
            TakeConversationResponse,
            TransferableAgentsResponse,
            SettingsListResponse, SettingsResponse,
            QuickRepliesListResponse, QuickReplyResponse,
            // WhatsApp — Templates CRUD
            WaTemplateItem, WaTemplateCategory, WaTemplateStatus,
            WaTemplateHeaderInput, WaTemplateButtonInput,
            CreateWaTemplateRequest, UpdateWaTemplateRequest,
            WaTemplateResponse, WaTemplatesListResponse,
            DeleteWaTemplateResponse, DeleteWaTemplateData,
            HeaderMediaUploadResponse, HeaderMediaUploadData,
            WaPurposeUsage,
            UpdateResponse,
            // Users — CRUD
            UserItem, UserListResponse, UserResponseEnvelope, OkResponse,
            SetUserVisibleRequest, UpdateUserRequest, CreateUserBody, SetUserPasswordRequest,
            ChangeMyPasswordRequest,
            // WhatsApp — Reactions
            crate::models::whatsapp::MessageReaction,
            crate::modules::whatsapp::handler::ReactMessageRequest,
            crate::modules::whatsapp::handler::ReactMessageResponse,
            // WhatsApp — Items
            ConversationItem, MessageItem, SettingsItem,
            TransferableAgentItem, ReplyToItem, UrlPreview, LocationPayload, QuickReplyItem,
            // WhatsApp — Test connection
            WaTestConnectionRequest, WaTestConnectionResponse, WaTestConnectionData, WaTestConnectionSource,
            // WhatsApp — Tickets
            CreateTicketRequest, UpdateTicketStatusRequest, TransferAndTicketRequest,
            TicketItem, TicketTimelineEntryItem, WaTicketTimelineEntry,
            TicketCategoryItem, TicketCategoriesResponse,
            TicketResponse, TicketsListResponse,
            TransferAndTicketData, TransferAndTicketResponse,
            // WhatsApp — AI Agent
            AiAgentMode,
            AiScheduleDto, AiScheduleInput,
            AiModelConfigDto, AiModelConfigInput,
            AiPersonalityDto, AiPersonalityInput,
            AiToolConfigDto, AiToolConfigInput,
            AiEscalationRulesDto, AiEscalationRulesInput,
            AiLimitsDto, AiLimitsInput,
            AiAgentItem,
            CreateAiAgentRequest, UpdateAiAgentRequest,
            AiAgentResponse, AiAgentsListResponse,
            AiAgentFaqItem, CreateAiAgentFaqRequest, UpdateAiAgentFaqRequest,
            AiAgentFaqResponse, AiAgentFaqListResponse,
            AiAgentDeleteResponse,
            // AI Agent — Sandbox
            SandboxRequest, SandboxHistoryEntry,
            SandboxResponse, SandboxData, SandboxToolCall, SandboxUsage,
            // AI Agent — Test connection
            TestConnectionRequest, TestConnectionResponse, TestConnectionData,
            TestConnectionSource,
            // AI Agent — Global config
            AiConfigDto, AiConfigPatchRequest, AiConfigResponse,
            // AI Agent — Phase 3a (Pre-classifier + Metrics)
            AiAgentPurpose,
            TrivialResponse,
            AiAgentMetricsResponse, AiAgentMetricsData, AiAgentPreClassBreakdown,
            AiAgentMetricsDailyBucketDto,
            // AI Agent — datos de negocio
            AiPlanItem, CreateAiPlanRequest, UpdateAiPlanRequest,
            AiPlanResponse, AiPlansListResponse,
            AiCoverageZoneItem, CreateAiCoverageZoneRequest, UpdateAiCoverageZoneRequest,
            AiCoverageZoneResponse, AiCoverageZonesListResponse,
            ConnectionType,
            AiInstallationConfigItem, AiInstallationConfigResponse, AiInstallationConfigsListResponse,
            UpdateAiInstallationConfigRequest,
            AiPromotionItem, AiPromotionResponse, AiPromotionsListResponse,
            CreateAiPromotionRequest, UpdateAiPromotionRequest,
            PoliticalDivisionItem, PoliticalDivisionsResponse,
            AiBusinessDataDeleteResponse,
            // AI Agent — discovery
            AiToolMetaItem, AiToolsListResponse,
            // AI Agent — conversation state
            ResetAiStateResponse,
            InterveneResponse,
            InterveneData,
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
        (name = "WhatsApp — Templates", description = "CRUD de plantillas de WhatsApp (WaTemplates). Writes requieren SUPERADMIN; GET list requiere bCanChat."),
        (name = "WhatsApp — Tickets", description = "Tickets de soporte derivados de conversaciones WA. POST cierra la conv referenciada; estados: open/in_progress/resolved/closed/cancelled. Acceso requiere bCanChat."),
        (name = "WhatsApp — AI Agent", description = "Configuración del Asistente Virtual (Gemini) por workspace WhatsApp. PR 1: settings + FAQs. SUPERADMIN-only."),
        (name = "Users — CRUD", description = "Gestión de usuarios (staff/admin). Requiere rol SUPERADMIN (nRole == 0.0) salvo `/me/password`."),
    )
)]
pub struct ApiDoc;
