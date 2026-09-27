pub(crate) fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

pub(crate) fn is_valid_quoted_string(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes.first() != Some(&b'"') || bytes.last() != Some(&b'"') {
        return false;
    }

    let mut index = 1;
    while index < bytes.len() - 1 {
        let byte = bytes[index];
        if byte == b'\\' {
            index += 1;
            if index >= bytes.len() - 1 || !is_quoted_pair_byte(bytes[index]) {
                return false;
            }
        } else if !is_quoted_text_byte(byte) {
            return false;
        }
        index += 1;
    }
    true
}

pub(crate) fn is_valid_accept_extension(parameter: &str) -> bool {
    let parameter = parameter.trim();
    let (name, value) = match parameter.split_once('=') {
        Some((name, value)) => (name.trim(), Some(value.trim())),
        None => (parameter, None),
    };
    if !is_http_token(name) {
        return false;
    }

    match value {
        None => true,
        Some(value) => is_http_token(value) || is_valid_quoted_string(value),
    }
}

fn is_quoted_text_byte(byte: u8) -> bool {
    matches!(byte, b'\t' | b' ' | b'!' | 0x23..=0x5b | 0x5d..=0x7e | 0x80..=0xff)
}

fn is_quoted_pair_byte(byte: u8) -> bool {
    matches!(byte, b'\t' | b' ' | 0x21..=0x7e | 0x80..=0xff)
}
