#![allow(dead_code)]

use std::fmt;

use serde_json::json;

use super::{
    dto::{TemplateMediaBinding, TemplateMediaComponent, TemplateMediaSource, TemplateMediaType},
    template_resolver::{
        resolve_campaign_template_components, CampaignTemplateRecipientSnapshot,
        CampaignTemplateResolveError, CampaignTemplateVariableBindingLike,
    },
};

pub type CampaignTemplateSendComponent = serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CampaignTemplateSendBuildError {
    TemplateResolve(CampaignTemplateResolveError),
    MissingTemplateMediaBinding,
    InvalidTemplateMediaBinding,
    InvalidMediaLink,
    MismatchedTemplateMediaType,
    DuplicateTemplateMediaBinding,
    UnexpectedTemplateMediaBinding,
    UnsupportedTemplateHeaderCombination,
}

impl CampaignTemplateSendBuildError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::TemplateResolve(err) => err.code(),
            Self::MissingTemplateMediaBinding => "missing_template_media_binding",
            Self::InvalidTemplateMediaBinding => "invalid_template_media_binding",
            Self::InvalidMediaLink => "invalid_media_link",
            Self::MismatchedTemplateMediaType => "mismatched_template_media_type",
            Self::DuplicateTemplateMediaBinding => "duplicate_template_media_binding",
            Self::UnexpectedTemplateMediaBinding => "unexpected_template_media_binding",
            Self::UnsupportedTemplateHeaderCombination => "unsupported_template_header_combination",
        }
    }
}

impl fmt::Display for CampaignTemplateSendBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for CampaignTemplateSendBuildError {}

impl From<CampaignTemplateResolveError> for CampaignTemplateSendBuildError {
    fn from(value: CampaignTemplateResolveError) -> Self {
        Self::TemplateResolve(value)
    }
}

pub fn build_campaign_template_send_components<B>(
    template_components: Option<&[serde_json::Value]>,
    template_variable_bindings: Option<&[B]>,
    template_media_bindings: Option<&[TemplateMediaBinding]>,
    recipient: &CampaignTemplateRecipientSnapshot,
) -> Result<Vec<CampaignTemplateSendComponent>, CampaignTemplateSendBuildError>
where
    B: CampaignTemplateVariableBindingLike,
{
    let mut resolved_components = resolve_campaign_template_components(
        template_components,
        template_variable_bindings,
        recipient,
    )?;

    let required_media_type = required_header_media_type(template_components);
    let media_bindings = template_media_bindings.unwrap_or(&[]);

    if required_media_type.is_none() {
        if media_bindings.is_empty() {
            return Ok(resolved_components);
        }
        return Err(CampaignTemplateSendBuildError::UnexpectedTemplateMediaBinding);
    }

    if resolved_components.iter().any(is_header_component) {
        return Err(CampaignTemplateSendBuildError::UnsupportedTemplateHeaderCombination);
    }

    let required_media_type = required_media_type.expect("checked above");
    let header_bindings = media_bindings
        .iter()
        .filter(|binding| matches!(binding.component, TemplateMediaComponent::Header))
        .collect::<Vec<_>>();

    if header_bindings.is_empty() {
        return Err(CampaignTemplateSendBuildError::MissingTemplateMediaBinding);
    }
    if header_bindings.len() > 1 {
        return Err(CampaignTemplateSendBuildError::DuplicateTemplateMediaBinding);
    }

    let binding = header_bindings[0];
    if binding.media_type != required_media_type {
        return Err(CampaignTemplateSendBuildError::MismatchedTemplateMediaType);
    }

    let header_media_component = build_header_media_component(binding)?;
    resolved_components.insert(0, header_media_component);
    Ok(resolved_components)
}

fn build_header_media_component(
    binding: &TemplateMediaBinding,
) -> Result<serde_json::Value, CampaignTemplateSendBuildError> {
    let media_type = media_type_name(&binding.media_type);
    let value = binding.value.trim();

    if value.is_empty() {
        return Err(CampaignTemplateSendBuildError::InvalidTemplateMediaBinding);
    }

    let media_ref = match binding.source {
        TemplateMediaSource::Link => {
            if !value.starts_with("https://") {
                return Err(CampaignTemplateSendBuildError::InvalidMediaLink);
            }
            json!({ "link": value })
        }
        TemplateMediaSource::MediaId => json!({ "id": value }),
        TemplateMediaSource::TemplateMediaId => {
            return Err(CampaignTemplateSendBuildError::InvalidTemplateMediaBinding)
        }
    };

    let mut parameter = serde_json::Map::new();
    parameter.insert(
        "type".to_string(),
        serde_json::Value::String(media_type.to_string()),
    );
    parameter.insert(media_type.to_string(), media_ref);

    Ok(json!({
        "type": "HEADER",
        "parameters": [serde_json::Value::Object(parameter)]
    }))
}

fn required_header_media_type(
    template_components: Option<&[serde_json::Value]>,
) -> Option<TemplateMediaType> {
    template_components?.iter().find_map(header_media_type)
}

fn header_media_type(component: &serde_json::Value) -> Option<TemplateMediaType> {
    let component_type = component.get("type")?.as_str()?;
    if !component_type.eq_ignore_ascii_case("HEADER") {
        return None;
    }

    match component
        .get("format")?
        .as_str()?
        .to_ascii_uppercase()
        .as_str()
    {
        "IMAGE" => Some(TemplateMediaType::Image),
        "VIDEO" => Some(TemplateMediaType::Video),
        "DOCUMENT" => Some(TemplateMediaType::Document),
        _ => None,
    }
}

fn is_header_component(component: &serde_json::Value) -> bool {
    component
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|component_type| component_type.eq_ignore_ascii_case("HEADER"))
}

fn media_type_name(media_type: &TemplateMediaType) -> &'static str {
    match media_type {
        TemplateMediaType::Image => "image",
        TemplateMediaType::Video => "video",
        TemplateMediaType::Document => "document",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::modules::whatsapp::campaigns::dto::{
        DerivedClientState, TemplateClientField, TemplateMediaSource, TemplateVariableBinding,
        TemplateVariableComponent, TemplateVariableSource,
    };

    fn recipient() -> CampaignTemplateRecipientSnapshot {
        CampaignTemplateRecipientSnapshot {
            client_name: "Maria Perez".to_string(),
            balance: 42.5,
            payment_due_day: Some(15),
            sector_name: Some("Centro".to_string()),
            customer_status_derived: DerivedClientState::Moroso,
            phone_normalized: Some("584121234567".to_string()),
        }
    }

    fn body_binding(field: TemplateClientField) -> TemplateVariableBinding {
        TemplateVariableBinding {
            component: TemplateVariableComponent::Body,
            index: 1,
            placeholder: "{{1}}".to_string(),
            source: TemplateVariableSource::ClientField,
            value: None,
            client_field: Some(field),
            button_index: None,
        }
    }

    fn body_binding_at(index: i32, field: TemplateClientField) -> TemplateVariableBinding {
        TemplateVariableBinding {
            component: TemplateVariableComponent::Body,
            index,
            placeholder: format!("{{{{{index}}}}}"),
            source: TemplateVariableSource::ClientField,
            value: None,
            client_field: Some(field),
            button_index: None,
        }
    }

    fn header_text_binding() -> TemplateVariableBinding {
        TemplateVariableBinding {
            component: TemplateVariableComponent::Header,
            index: 1,
            placeholder: "{{1}}".to_string(),
            source: TemplateVariableSource::ClientField,
            value: None,
            client_field: Some(TemplateClientField::ClientName),
            button_index: None,
        }
    }

    fn button_binding() -> TemplateVariableBinding {
        TemplateVariableBinding {
            component: TemplateVariableComponent::Button,
            index: 1,
            placeholder: "{{1}}".to_string(),
            source: TemplateVariableSource::ClientField,
            value: None,
            client_field: Some(TemplateClientField::PhoneNormalized),
            button_index: Some(0),
        }
    }

    fn media_binding(
        media_type: TemplateMediaType,
        source: TemplateMediaSource,
        value: &str,
    ) -> TemplateMediaBinding {
        TemplateMediaBinding {
            component: TemplateMediaComponent::Header,
            media_type,
            source,
            value: value.to_string(),
        }
    }

    fn body_template() -> Vec<serde_json::Value> {
        vec![json!({ "type": "BODY", "text": "Hola {{1}}" })]
    }

    fn media_template(format: &str) -> Vec<serde_json::Value> {
        vec![
            json!({ "type": "HEADER", "format": format }),
            json!({ "type": "BODY", "text": "Hola {{1}}" }),
        ]
    }

    fn assert_error(
        result: Result<Vec<serde_json::Value>, CampaignTemplateSendBuildError>,
        expected: CampaignTemplateSendBuildError,
    ) {
        assert_eq!(result.unwrap_err(), expected);
    }

    #[test]
    fn builds_body_variables_only_like_resolver() {
        let components = body_template();
        let bindings = vec![body_binding(TemplateClientField::ClientName)];

        let result = build_campaign_template_send_components(
            Some(&components),
            Some(&bindings),
            None,
            &recipient(),
        )
        .unwrap();

        assert_eq!(
            result,
            vec![json!({
                "type": "BODY",
                "parameters": [{ "type": "text", "text": "Maria Perez" }]
            })]
        );
    }

    #[test]
    fn builds_body_and_header_image_link() {
        let components = media_template("IMAGE");
        let bindings = vec![body_binding(TemplateClientField::ClientName)];
        let media = vec![media_binding(
            TemplateMediaType::Image,
            TemplateMediaSource::Link,
            "https://example.com/header.jpg",
        )];

        let result = build_campaign_template_send_components(
            Some(&components),
            Some(&bindings),
            Some(&media),
            &recipient(),
        )
        .unwrap();

        assert_eq!(
            result,
            vec![
                json!({
                    "type": "HEADER",
                    "parameters": [{
                        "type": "image",
                        "image": { "link": "https://example.com/header.jpg" }
                    }]
                }),
                json!({
                    "type": "BODY",
                    "parameters": [{ "type": "text", "text": "Maria Perez" }]
                })
            ]
        );
    }

    #[test]
    fn rejects_unresolved_template_media_id() {
        let components = media_template("IMAGE");
        let bindings = vec![TemplateMediaBinding {
            component: TemplateMediaComponent::Header,
            media_type: TemplateMediaType::Image,
            source: TemplateMediaSource::TemplateMediaId,
            value: "665f00000000000000000001".to_string(),
        }];

        let err = build_campaign_template_send_components(
            Some(&components),
            Some(&[body_binding(TemplateClientField::ClientName)]),
            Some(&bindings),
            &recipient(),
        )
        .unwrap_err();

        assert_eq!(
            err,
            CampaignTemplateSendBuildError::InvalidTemplateMediaBinding
        );
    }

    #[test]
    fn builds_header_image_media_id() {
        let components = media_template("IMAGE");
        let bindings = vec![body_binding(TemplateClientField::ClientName)];
        let media = vec![media_binding(
            TemplateMediaType::Image,
            TemplateMediaSource::MediaId,
            "123456789",
        )];

        let result = build_campaign_template_send_components(
            Some(&components),
            Some(&bindings),
            Some(&media),
            &recipient(),
        )
        .unwrap();

        assert_eq!(
            result[0]["parameters"][0]["image"],
            json!({ "id": "123456789" })
        );
    }

    #[test]
    fn builds_header_video_link() {
        let components = media_template("VIDEO");
        let bindings = vec![body_binding(TemplateClientField::ClientName)];
        let media = vec![media_binding(
            TemplateMediaType::Video,
            TemplateMediaSource::Link,
            "https://example.com/header.mp4",
        )];

        let result = build_campaign_template_send_components(
            Some(&components),
            Some(&bindings),
            Some(&media),
            &recipient(),
        )
        .unwrap();

        assert_eq!(
            result[0],
            json!({
                "type": "HEADER",
                "parameters": [{
                    "type": "video",
                    "video": { "link": "https://example.com/header.mp4" }
                }]
            })
        );
    }

    #[test]
    fn builds_header_document_link() {
        let components = media_template("DOCUMENT");
        let bindings = vec![body_binding(TemplateClientField::ClientName)];
        let media = vec![media_binding(
            TemplateMediaType::Document,
            TemplateMediaSource::Link,
            "https://example.com/header.pdf",
        )];

        let result = build_campaign_template_send_components(
            Some(&components),
            Some(&bindings),
            Some(&media),
            &recipient(),
        )
        .unwrap();

        assert_eq!(
            result[0],
            json!({
                "type": "HEADER",
                "parameters": [{
                    "type": "document",
                    "document": { "link": "https://example.com/header.pdf" }
                }]
            })
        );
    }

    #[test]
    fn errors_when_required_media_binding_is_missing() {
        let components = media_template("IMAGE");
        let bindings = vec![body_binding(TemplateClientField::ClientName)];

        assert_error(
            build_campaign_template_send_components(
                Some(&components),
                Some(&bindings),
                None,
                &recipient(),
            ),
            CampaignTemplateSendBuildError::MissingTemplateMediaBinding,
        );
    }

    #[test]
    fn errors_when_media_type_is_mismatched() {
        let components = media_template("IMAGE");
        let bindings = vec![body_binding(TemplateClientField::ClientName)];
        let media = vec![media_binding(
            TemplateMediaType::Video,
            TemplateMediaSource::Link,
            "https://example.com/header.mp4",
        )];

        assert_error(
            build_campaign_template_send_components(
                Some(&components),
                Some(&bindings),
                Some(&media),
                &recipient(),
            ),
            CampaignTemplateSendBuildError::MismatchedTemplateMediaType,
        );
    }

    #[test]
    fn errors_when_link_is_not_https() {
        let components = media_template("IMAGE");
        let bindings = vec![body_binding(TemplateClientField::ClientName)];
        let media = vec![media_binding(
            TemplateMediaType::Image,
            TemplateMediaSource::Link,
            "http://example.com/header.jpg",
        )];

        assert_error(
            build_campaign_template_send_components(
                Some(&components),
                Some(&bindings),
                Some(&media),
                &recipient(),
            ),
            CampaignTemplateSendBuildError::InvalidMediaLink,
        );
    }

    #[test]
    fn errors_when_media_id_is_empty() {
        let components = media_template("IMAGE");
        let bindings = vec![body_binding(TemplateClientField::ClientName)];
        let media = vec![media_binding(
            TemplateMediaType::Image,
            TemplateMediaSource::MediaId,
            "   ",
        )];

        assert_error(
            build_campaign_template_send_components(
                Some(&components),
                Some(&bindings),
                Some(&media),
                &recipient(),
            ),
            CampaignTemplateSendBuildError::InvalidTemplateMediaBinding,
        );
    }

    #[test]
    fn errors_when_header_media_binding_is_duplicated() {
        let components = media_template("IMAGE");
        let bindings = vec![body_binding(TemplateClientField::ClientName)];
        let media = vec![
            media_binding(
                TemplateMediaType::Image,
                TemplateMediaSource::Link,
                "https://example.com/one.jpg",
            ),
            media_binding(
                TemplateMediaType::Image,
                TemplateMediaSource::Link,
                "https://example.com/two.jpg",
            ),
        ];

        assert_error(
            build_campaign_template_send_components(
                Some(&components),
                Some(&bindings),
                Some(&media),
                &recipient(),
            ),
            CampaignTemplateSendBuildError::DuplicateTemplateMediaBinding,
        );
    }

    #[test]
    fn errors_when_media_binding_is_unexpected() {
        let components = body_template();
        let bindings = vec![body_binding(TemplateClientField::ClientName)];
        let media = vec![media_binding(
            TemplateMediaType::Image,
            TemplateMediaSource::Link,
            "https://example.com/header.jpg",
        )];

        assert_error(
            build_campaign_template_send_components(
                Some(&components),
                Some(&bindings),
                Some(&media),
                &recipient(),
            ),
            CampaignTemplateSendBuildError::UnexpectedTemplateMediaBinding,
        );
    }

    #[test]
    fn errors_when_header_text_and_header_media_would_be_combined() {
        let components = vec![
            json!({ "type": "HEADER", "format": "TEXT", "text": "Hola {{1}}" }),
            json!({ "type": "HEADER", "format": "IMAGE" }),
            json!({ "type": "BODY", "text": "Contenido" }),
        ];
        let bindings = vec![header_text_binding()];
        let media = vec![media_binding(
            TemplateMediaType::Image,
            TemplateMediaSource::Link,
            "https://example.com/header.jpg",
        )];

        assert_error(
            build_campaign_template_send_components(
                Some(&components),
                Some(&bindings),
                Some(&media),
                &recipient(),
            ),
            CampaignTemplateSendBuildError::UnsupportedTemplateHeaderCombination,
        );
    }

    #[test]
    fn keeps_button_url_variables_working() {
        let components = vec![
            json!({ "type": "BODY", "text": "Hola {{1}}" }),
            json!({
                "type": "BUTTONS",
                "buttons": [{ "type": "URL", "text": "Ver", "url": "https://example.com/{{1}}" }]
            }),
        ];
        let bindings = vec![
            body_binding(TemplateClientField::ClientName),
            button_binding(),
        ];

        let result = build_campaign_template_send_components(
            Some(&components),
            Some(&bindings),
            None,
            &recipient(),
        )
        .unwrap();

        assert_eq!(
            result,
            vec![
                json!({
                    "type": "BODY",
                    "parameters": [{ "type": "text", "text": "Maria Perez" }]
                }),
                json!({
                    "type": "BUTTON",
                    "sub_type": "url",
                    "index": "0",
                    "parameters": [{ "type": "text", "text": "584121234567" }]
                })
            ]
        );
    }

    #[test]
    fn combines_payment_due_day_with_header_media() {
        let components = media_template("IMAGE");
        let bindings = vec![body_binding_at(1, TemplateClientField::PaymentDueDay)];
        let media = vec![media_binding(
            TemplateMediaType::Image,
            TemplateMediaSource::Link,
            "https://example.com/header.jpg",
        )];

        let result = build_campaign_template_send_components(
            Some(&components),
            Some(&bindings),
            Some(&media),
            &recipient(),
        )
        .unwrap();

        assert_eq!(result[1]["parameters"][0]["text"], json!("15"));
    }
}
