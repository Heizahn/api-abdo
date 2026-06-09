#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhoneError {
    Invalid,
}

pub fn normalize_phone_to_whatsapp(phone: &str) -> Result<String, PhoneError> {
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();

    let normalized = if digits.starts_with("58") && digits.len() == 12 {
        digits
    } else if digits.starts_with("04") && digits.len() == 11 {
        format!("58{}", &digits[1..])
    } else if digits.starts_with('4') && digits.len() == 10 {
        format!("58{}", digits)
    } else {
        return Err(PhoneError::Invalid);
    };

    if normalized.starts_with("584") && normalized.len() == 12 {
        Ok(normalized)
    } else {
        Err(PhoneError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_venezuelan_whatsapp_numbers() {
        assert_eq!(
            normalize_phone_to_whatsapp("+58 412-123-4567").unwrap(),
            "584121234567"
        );
        assert_eq!(
            normalize_phone_to_whatsapp("0412 123 4567").unwrap(),
            "584121234567"
        );
        assert_eq!(
            normalize_phone_to_whatsapp("4121234567").unwrap(),
            "584121234567"
        );
        assert_eq!(
            normalize_phone_to_whatsapp("58(412)123-4567").unwrap(),
            "584121234567"
        );
    }

    #[test]
    fn rejects_invalid_numbers() {
        assert_eq!(
            normalize_phone_to_whatsapp("0212-1234567"),
            Err(PhoneError::Invalid)
        );
        assert_eq!(
            normalize_phone_to_whatsapp("58412123456"),
            Err(PhoneError::Invalid)
        );
        assert_eq!(normalize_phone_to_whatsapp(""), Err(PhoneError::Invalid));
    }
}
