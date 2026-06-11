#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde_json::json;

use super::dto::{
    DerivedClientState, TemplateClientField, TemplateVariableBinding, TemplateVariableComponent,
    TemplateVariableSource,
};

pub type ResolvedTemplateComponent = serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CampaignTemplateResolveError {
    MissingTemplateBinding,
    InvalidTemplateBindingSource,
    UnsupportedClientField,
    UnsupportedTemplateVariableComponent,
    InvalidTemplateBindingIndex,
    DuplicateTemplateBinding,
    InvalidStaticBinding,
    MissingRecipientField,
}

impl CampaignTemplateResolveError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingTemplateBinding => "missing_template_binding",
            Self::InvalidTemplateBindingSource => "invalid_template_binding_source",
            Self::UnsupportedClientField => "unsupported_client_field",
            Self::UnsupportedTemplateVariableComponent => "unsupported_template_variable_component",
            Self::InvalidTemplateBindingIndex => "invalid_template_binding_index",
            Self::DuplicateTemplateBinding => "duplicate_template_binding",
            Self::InvalidStaticBinding => "invalid_static_binding",
            Self::MissingRecipientField => "missing_recipient_field",
        }
    }
}

impl fmt::Display for CampaignTemplateResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for CampaignTemplateResolveError {}

#[derive(Debug, Clone)]
pub struct CampaignTemplateRecipientSnapshot {
    pub client_name: String,
    pub balance: f64,
    pub payment_due_day: Option<i32>,
    pub sector_name: Option<String>,
    pub customer_status_derived: DerivedClientState,
    pub phone_normalized: Option<String>,
}

pub trait CampaignTemplateVariableBindingLike {
    fn component(&self) -> Option<TemplateVariableComponent>;
    fn index(&self) -> i32;
    fn source(&self) -> Option<TemplateVariableSource>;
    fn value(&self) -> Option<&str>;
    fn client_field(&self) -> Option<TemplateClientField>;
    fn has_unsupported_client_field(&self) -> bool;
    fn button_index(&self) -> Option<i32>;
}

impl CampaignTemplateVariableBindingLike for TemplateVariableBinding {
    fn component(&self) -> Option<TemplateVariableComponent> {
        Some(self.component.clone())
    }

    fn index(&self) -> i32 {
        self.index
    }

    fn source(&self) -> Option<TemplateVariableSource> {
        Some(self.source.clone())
    }

    fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    fn client_field(&self) -> Option<TemplateClientField> {
        self.client_field.clone()
    }

    fn has_unsupported_client_field(&self) -> bool {
        false
    }

    fn button_index(&self) -> Option<i32> {
        self.button_index
    }
}

pub fn resolve_campaign_template_components<B>(
    campaign_template_components: Option<&[serde_json::Value]>,
    template_variable_bindings: Option<&[B]>,
    recipient: &CampaignTemplateRecipientSnapshot,
) -> Result<Vec<ResolvedTemplateComponent>, CampaignTemplateResolveError>
where
    B: CampaignTemplateVariableBindingLike,
{
    let components = campaign_template_components.unwrap_or(&[]);
    let placeholders = extract_template_placeholders(components)?;
    let bindings = template_variable_bindings.unwrap_or(&[]);

    validate_binding_keys(bindings)?;

    if placeholders.is_empty() {
        return Ok(Vec::new());
    }

    let bindings_by_key = bindings
        .iter()
        .map(|binding| {
            let component = binding
                .component()
                .ok_or(CampaignTemplateResolveError::UnsupportedTemplateVariableComponent)?;
            Ok((
                (component, binding.index(), binding.button_index()),
                binding,
            ))
        })
        .collect::<Result<HashMap<_, _>, CampaignTemplateResolveError>>()?;

    let mut body_params = Vec::new();
    let mut header_params = Vec::new();
    let mut button_params: HashMap<i32, Vec<serde_json::Value>> = HashMap::new();
    let mut sorted_placeholders = placeholders;
    sorted_placeholders.sort_by_key(|placeholder| {
        (
            component_sort_key(&placeholder.component),
            placeholder.button_index.unwrap_or(-1),
            placeholder.index,
        )
    });

    for placeholder in sorted_placeholders {
        let binding = bindings_by_key
            .get(&(
                placeholder.component.clone(),
                placeholder.index,
                placeholder.button_index,
            ))
            .ok_or(CampaignTemplateResolveError::MissingTemplateBinding)?;
        let text = resolve_binding_text(*binding, recipient)?;
        let param = json!({ "type": "text", "text": text });

        match placeholder.component {
            TemplateVariableComponent::Body => body_params.push(param),
            TemplateVariableComponent::Header => header_params.push(param),
            TemplateVariableComponent::Button => {
                let button_index = placeholder
                    .button_index
                    .ok_or(CampaignTemplateResolveError::UnsupportedTemplateVariableComponent)?;
                button_params.entry(button_index).or_default().push(param);
            }
        }
    }

    let mut resolved = Vec::new();
    if !header_params.is_empty() {
        resolved.push(json!({ "type": "HEADER", "parameters": header_params }));
    }
    if !body_params.is_empty() {
        resolved.push(json!({ "type": "BODY", "parameters": body_params }));
    }

    let mut sorted_buttons = button_params.into_iter().collect::<Vec<_>>();
    sorted_buttons.sort_by_key(|(button_index, _)| *button_index);
    for (button_index, parameters) in sorted_buttons {
        resolved.push(json!({
            "type": "BUTTON",
            "sub_type": "url",
            "index": button_index.to_string(),
            "parameters": parameters,
        }));
    }

    Ok(resolved)
}

fn validate_binding_keys<B>(bindings: &[B]) -> Result<(), CampaignTemplateResolveError>
where
    B: CampaignTemplateVariableBindingLike,
{
    let mut seen = HashSet::new();
    for binding in bindings {
        if binding.index() < 1 || binding.button_index().is_some_and(|index| index < 0) {
            return Err(CampaignTemplateResolveError::InvalidTemplateBindingIndex);
        }
        let component = binding
            .component()
            .ok_or(CampaignTemplateResolveError::UnsupportedTemplateVariableComponent)?;
        if !seen.insert((component, binding.index(), binding.button_index())) {
            return Err(CampaignTemplateResolveError::DuplicateTemplateBinding);
        }
    }
    Ok(())
}

fn resolve_binding_text<B>(
    binding: &B,
    recipient: &CampaignTemplateRecipientSnapshot,
) -> Result<String, CampaignTemplateResolveError>
where
    B: CampaignTemplateVariableBindingLike,
{
    match binding
        .source()
        .ok_or(CampaignTemplateResolveError::InvalidTemplateBindingSource)?
    {
        TemplateVariableSource::Static => binding
            .value()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .ok_or(CampaignTemplateResolveError::InvalidStaticBinding),
        TemplateVariableSource::ClientField => {
            if binding.has_unsupported_client_field() {
                return Err(CampaignTemplateResolveError::UnsupportedClientField);
            }
            let field = binding
                .client_field()
                .ok_or(CampaignTemplateResolveError::UnsupportedClientField)?;
            resolve_client_field_text(field, recipient)
        }
    }
}

fn resolve_client_field_text(
    field: TemplateClientField,
    recipient: &CampaignTemplateRecipientSnapshot,
) -> Result<String, CampaignTemplateResolveError> {
    match field {
        TemplateClientField::ClientName => Ok(recipient.client_name.clone()),
        TemplateClientField::Balance => Ok(format_stable_amount(recipient.balance)),
        TemplateClientField::PaymentDueDay => recipient
            .payment_due_day
            .map(|day| day.to_string())
            .ok_or(CampaignTemplateResolveError::MissingRecipientField),
        TemplateClientField::SectorName => required_optional_text(recipient.sector_name.as_deref()),
        TemplateClientField::CustomerStatusDerived => Ok(match recipient.customer_status_derived {
            DerivedClientState::Moroso => "moroso",
            DerivedClientState::Solvente => "solvente",
            DerivedClientState::Suspended => "suspended",
        }
        .to_string()),
        TemplateClientField::PhoneNormalized => {
            required_optional_text(recipient.phone_normalized.as_deref())
        }
    }
}

fn required_optional_text(value: Option<&str>) -> Result<String, CampaignTemplateResolveError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or(CampaignTemplateResolveError::MissingRecipientField)
}

fn format_stable_amount(value: f64) -> String {
    if value.fract().abs() < 0.000_000_1 {
        return format!("{value:.0}");
    }
    let formatted = format!("{value:.2}");
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TemplatePlaceholder {
    pub(crate) component: TemplateVariableComponent,
    pub(crate) index: i32,
    pub(crate) button_index: Option<i32>,
}

pub(crate) fn extract_template_placeholders(
    components: &[serde_json::Value],
) -> Result<Vec<TemplatePlaceholder>, CampaignTemplateResolveError> {
    let mut placeholders = Vec::new();

    for component in components {
        let component_type = component
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase();

        match component_type.as_str() {
            "BODY" => extract_text_placeholders(
                component.get("text").and_then(serde_json::Value::as_str),
                TemplateVariableComponent::Body,
                None,
                &mut placeholders,
            ),
            "HEADER" => {
                let format = component
                    .get("format")
                    .and_then(serde_json::Value::as_str)
                    .map(|value| value.to_ascii_uppercase());

                if format.as_deref() != Some("TEXT") {
                    if value_has_placeholder(component) {
                        return Err(
                            CampaignTemplateResolveError::UnsupportedTemplateVariableComponent,
                        );
                    }
                } else {
                    extract_text_placeholders(
                        component.get("text").and_then(serde_json::Value::as_str),
                        TemplateVariableComponent::Header,
                        None,
                        &mut placeholders,
                    );

                    if value_has_placeholder_excluding(component, &["text"]) {
                        return Err(
                            CampaignTemplateResolveError::UnsupportedTemplateVariableComponent,
                        );
                    }
                }
            }
            "BUTTONS" => {
                let buttons = component
                    .get("buttons")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                for (button_index, button) in buttons.iter().enumerate() {
                    let button_type = button
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .map(|value| value.to_ascii_uppercase());

                    if button_type.as_deref() != Some("URL") {
                        if value_has_placeholder(button) {
                            return Err(
                                CampaignTemplateResolveError::UnsupportedTemplateVariableComponent,
                            );
                        }
                        continue;
                    }

                    extract_text_placeholders(
                        button.get("url").and_then(serde_json::Value::as_str),
                        TemplateVariableComponent::Button,
                        Some(button_index as i32),
                        &mut placeholders,
                    );

                    if value_has_placeholder_excluding(button, &["url"]) {
                        return Err(
                            CampaignTemplateResolveError::UnsupportedTemplateVariableComponent,
                        );
                    }
                }
            }
            _ => {
                if value_has_placeholder(component) {
                    return Err(CampaignTemplateResolveError::UnsupportedTemplateVariableComponent);
                }
            }
        }
    }

    placeholders.sort_by_key(|placeholder| {
        (
            component_sort_key(&placeholder.component),
            placeholder.button_index.unwrap_or(-1),
            placeholder.index,
        )
    });
    placeholders.dedup();
    Ok(placeholders)
}

fn extract_text_placeholders(
    text: Option<&str>,
    component: TemplateVariableComponent,
    button_index: Option<i32>,
    output: &mut Vec<TemplatePlaceholder>,
) {
    if let Some(text) = text {
        for index in placeholder_indices(text) {
            output.push(TemplatePlaceholder {
                component: component.clone(),
                index,
                button_index,
            });
        }
    }
}

fn placeholder_indices(value: &str) -> impl Iterator<Item = i32> + '_ {
    value.match_indices("{{").filter_map(|(start, _)| {
        let rest = &value[start + 2..];
        let end = rest.find("}}")?;
        rest[..end]
            .trim()
            .parse::<i32>()
            .ok()
            .filter(|index| *index >= 1)
    })
}

fn value_has_placeholder(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(value) => placeholder_indices(value).next().is_some(),
        serde_json::Value::Array(values) => values.iter().any(value_has_placeholder),
        serde_json::Value::Object(map) => map.values().any(value_has_placeholder),
        _ => false,
    }
}

fn value_has_placeholder_excluding(value: &serde_json::Value, excluded_keys: &[&str]) -> bool {
    match value {
        serde_json::Value::String(value) => placeholder_indices(value).next().is_some(),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| value_has_placeholder_excluding(value, excluded_keys)),
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            !excluded_keys.contains(&key.as_str())
                && value_has_placeholder_excluding(value, excluded_keys)
        }),
        _ => false,
    }
}

fn component_sort_key(component: &TemplateVariableComponent) -> i32 {
    match component {
        TemplateVariableComponent::Header => 0,
        TemplateVariableComponent::Body => 1,
        TemplateVariableComponent::Button => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::whatsapp::campaigns::service::{
        StoredTemplateClientField, StoredTemplateVariableBinding,
    };

    #[derive(Debug, Clone)]
    struct ResolverBinding {
        component: Option<TemplateVariableComponent>,
        index: i32,
        source: Option<TemplateVariableSource>,
        value: Option<String>,
        client_field: Option<TemplateClientField>,
        unsupported_client_field: bool,
        button_index: Option<i32>,
    }

    impl CampaignTemplateVariableBindingLike for ResolverBinding {
        fn component(&self) -> Option<TemplateVariableComponent> {
            self.component.clone()
        }

        fn index(&self) -> i32 {
            self.index
        }

        fn source(&self) -> Option<TemplateVariableSource> {
            self.source.clone()
        }

        fn value(&self) -> Option<&str> {
            self.value.as_deref()
        }

        fn client_field(&self) -> Option<TemplateClientField> {
            self.client_field.clone()
        }

        fn has_unsupported_client_field(&self) -> bool {
            self.unsupported_client_field
        }

        fn button_index(&self) -> Option<i32> {
            self.button_index
        }
    }

    fn recipient() -> CampaignTemplateRecipientSnapshot {
        CampaignTemplateRecipientSnapshot {
            client_name: "Ada Lovelace".to_string(),
            balance: 12.50,
            payment_due_day: Some(15),
            sector_name: Some("Downtown".to_string()),
            customer_status_derived: DerivedClientState::Moroso,
            phone_normalized: Some("584121234567".to_string()),
        }
    }

    fn static_binding(index: i32, value: &str) -> ResolverBinding {
        ResolverBinding {
            component: Some(TemplateVariableComponent::Body),
            index,
            source: Some(TemplateVariableSource::Static),
            value: Some(value.to_string()),
            client_field: None,
            unsupported_client_field: false,
            button_index: None,
        }
    }

    fn client_field_binding(index: i32, field: TemplateClientField) -> ResolverBinding {
        ResolverBinding {
            component: Some(TemplateVariableComponent::Body),
            index,
            source: Some(TemplateVariableSource::ClientField),
            value: None,
            client_field: Some(field),
            unsupported_client_field: false,
            button_index: None,
        }
    }

    fn body(text: &str) -> Vec<serde_json::Value> {
        vec![json!({ "type": "BODY", "text": text })]
    }

    fn resolve(
        components: Vec<serde_json::Value>,
        bindings: Vec<ResolverBinding>,
        recipient: &CampaignTemplateRecipientSnapshot,
    ) -> Result<Vec<ResolvedTemplateComponent>, CampaignTemplateResolveError> {
        resolve_campaign_template_components(Some(&components), Some(&bindings), recipient)
    }

    #[test]
    fn template_without_variables_returns_no_components() {
        let components = body("Hello customer");

        let resolved = resolve_campaign_template_components::<ResolverBinding>(
            Some(&components),
            None,
            &recipient(),
        )
        .unwrap();

        assert!(resolved.is_empty());
    }

    #[test]
    fn body_static_variable_becomes_text_parameter() {
        let resolved = resolve(
            body("Hello {{1}}"),
            vec![static_binding(1, "World")],
            &recipient(),
        )
        .unwrap();

        assert_eq!(
            resolved,
            vec![json!({ "type": "BODY", "parameters": [{ "type": "text", "text": "World" }] })]
        );
    }

    #[test]
    fn body_client_name_becomes_text_parameter() {
        let resolved = resolve(
            body("Hello {{1}}"),
            vec![client_field_binding(1, TemplateClientField::ClientName)],
            &recipient(),
        )
        .unwrap();

        assert_eq!(resolved[0]["parameters"][0]["text"], "Ada Lovelace");
    }

    #[test]
    fn body_multiple_variables_are_sorted_by_index() {
        let bindings = vec![static_binding(2, "second"), static_binding(1, "first")];

        let resolved = resolve(body("{{1}} {{2}}"), bindings, &recipient()).unwrap();

        assert_eq!(resolved[0]["parameters"][0]["text"], "first");
        assert_eq!(resolved[0]["parameters"][1]["text"], "second");
    }

    #[test]
    fn balance_formats_as_stable_string() {
        let mut recipient = recipient();
        recipient.balance = 10.50;

        let resolved = resolve(
            body("Balance {{1}}"),
            vec![client_field_binding(1, TemplateClientField::Balance)],
            &recipient,
        )
        .unwrap();

        assert_eq!(resolved[0]["parameters"][0]["text"], "10.5");
    }

    #[test]
    fn payment_due_day_resolves_or_fails_when_missing() {
        let resolved = resolve(
            body("Due {{1}}"),
            vec![client_field_binding(1, TemplateClientField::PaymentDueDay)],
            &recipient(),
        )
        .unwrap();
        assert_eq!(resolved[0]["parameters"][0]["text"], "15");

        let mut missing = recipient();
        missing.payment_due_day = None;
        let err = resolve(
            body("Due {{1}}"),
            vec![client_field_binding(1, TemplateClientField::PaymentDueDay)],
            &missing,
        )
        .unwrap_err();
        assert_eq!(err, CampaignTemplateResolveError::MissingRecipientField);
    }

    #[test]
    fn sector_status_and_phone_fields_resolve() {
        let bindings = vec![
            client_field_binding(1, TemplateClientField::SectorName),
            client_field_binding(2, TemplateClientField::CustomerStatusDerived),
            client_field_binding(3, TemplateClientField::PhoneNormalized),
        ];

        let resolved = resolve(body("{{1}} {{2}} {{3}}"), bindings, &recipient()).unwrap();

        assert_eq!(resolved[0]["parameters"][0]["text"], "Downtown");
        assert_eq!(resolved[0]["parameters"][1]["text"], "moroso");
        assert_eq!(resolved[0]["parameters"][2]["text"], "584121234567");
    }

    #[test]
    fn provider_name_returns_unsupported_client_field() {
        let mut binding = client_field_binding(1, TemplateClientField::ClientName);
        binding.client_field = None;
        binding.unsupported_client_field = true;

        let err = resolve(body("Hello {{1}}"), vec![binding], &recipient()).unwrap_err();

        assert_eq!(err, CampaignTemplateResolveError::UnsupportedClientField);
    }

    #[test]
    fn missing_optional_sector_returns_missing_recipient_field() {
        let mut recipient = recipient();
        recipient.sector_name = Some("  ".to_string());

        let err = resolve(
            body("Sector {{1}}"),
            vec![client_field_binding(1, TemplateClientField::SectorName)],
            &recipient,
        )
        .unwrap_err();

        assert_eq!(err, CampaignTemplateResolveError::MissingRecipientField);
    }

    #[test]
    fn duplicate_binding_returns_duplicate_template_binding() {
        let err = resolve(
            body("{{1}}"),
            vec![static_binding(1, "a"), static_binding(1, "b")],
            &recipient(),
        )
        .unwrap_err();

        assert_eq!(err, CampaignTemplateResolveError::DuplicateTemplateBinding);
    }

    #[test]
    fn empty_static_returns_invalid_static_binding() {
        let err = resolve(body("{{1}}"), vec![static_binding(1, " ")], &recipient()).unwrap_err();

        assert_eq!(err, CampaignTemplateResolveError::InvalidStaticBinding);
    }

    #[test]
    fn invalid_client_field_returns_unsupported_client_field() {
        let mut binding = client_field_binding(1, TemplateClientField::ClientName);
        binding.client_field = None;

        let err = resolve(body("{{1}}"), vec![binding], &recipient()).unwrap_err();

        assert_eq!(err, CampaignTemplateResolveError::UnsupportedClientField);
    }

    #[test]
    fn header_text_variable_becomes_header_parameter() {
        let components = vec![json!({ "type": "HEADER", "format": "TEXT", "text": "Hi {{1}}" })];
        let mut binding = static_binding(1, "Header");
        binding.component = Some(TemplateVariableComponent::Header);

        let resolved = resolve(components, vec![binding], &recipient()).unwrap();

        assert_eq!(
            resolved,
            vec![json!({ "type": "HEADER", "parameters": [{ "type": "text", "text": "Header" }] })]
        );
    }

    #[test]
    fn button_url_variable_becomes_button_parameter() {
        let components = vec![
            json!({ "type": "BUTTONS", "buttons": [{ "type": "URL", "url": "https://example.com/{{1}}" }] }),
        ];
        let mut binding = static_binding(1, "abc");
        binding.component = Some(TemplateVariableComponent::Button);
        binding.button_index = Some(0);

        let resolved = resolve(components, vec![binding], &recipient()).unwrap();

        assert_eq!(
            resolved,
            vec![
                json!({ "type": "BUTTON", "sub_type": "url", "index": "0", "parameters": [{ "type": "text", "text": "abc" }] })
            ]
        );
    }

    #[test]
    fn unsupported_component_with_placeholder_returns_error() {
        let components = vec![json!({ "type": "FOOTER", "text": "Footer {{1}}" })];

        let err = resolve(components, vec![static_binding(1, "x")], &recipient()).unwrap_err();

        assert_eq!(
            err,
            CampaignTemplateResolveError::UnsupportedTemplateVariableComponent
        );
    }

    #[test]
    fn button_placeholder_in_unsupported_field_returns_error() {
        let components = vec![json!({
            "type": "BUTTONS",
            "buttons": [{ "type": "URL", "text": "Go {{1}}", "url": "https://example.com" }]
        })];

        let err = resolve_campaign_template_components::<ResolverBinding>(
            Some(&components),
            None,
            &recipient(),
        )
        .unwrap_err();

        assert_eq!(
            err,
            CampaignTemplateResolveError::UnsupportedTemplateVariableComponent
        );
    }

    #[test]
    fn non_url_button_with_url_placeholder_returns_error() {
        let components = vec![json!({
            "type": "BUTTONS",
            "buttons": [{ "type": "QUICK_REPLY", "url": "https://example.com/{{1}}" }]
        })];

        let err = resolve_campaign_template_components::<ResolverBinding>(
            Some(&components),
            None,
            &recipient(),
        )
        .unwrap_err();

        assert_eq!(
            err,
            CampaignTemplateResolveError::UnsupportedTemplateVariableComponent
        );
    }

    #[test]
    fn header_placeholder_in_non_text_field_returns_error() {
        let components = vec![json!({
            "type": "HEADER",
            "format": "IMAGE",
            "image": "https://example.com/{{1}}"
        })];

        let err = resolve_campaign_template_components::<ResolverBinding>(
            Some(&components),
            None,
            &recipient(),
        )
        .unwrap_err();

        assert_eq!(
            err,
            CampaignTemplateResolveError::UnsupportedTemplateVariableComponent
        );
    }

    #[test]
    fn stored_legacy_provider_name_binding_returns_unsupported_client_field() {
        let components = body("Hello {{1}}");
        let bindings = vec![StoredTemplateVariableBinding {
            component: TemplateVariableComponent::Body,
            index: 1,
            placeholder: "{{1}}".to_string(),
            source: TemplateVariableSource::ClientField,
            value: None,
            client_field: Some(StoredTemplateClientField::ProviderName),
            button_index: None,
        }];

        let err =
            resolve_campaign_template_components(Some(&components), Some(&bindings), &recipient())
                .unwrap_err();

        assert_eq!(err, CampaignTemplateResolveError::UnsupportedClientField);
    }
}
