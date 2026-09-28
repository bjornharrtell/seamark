//! Shared HTTP media-type parsing for JSON:API negotiation.

use axum::http::HeaderMap;

use crate::document::is_valid_absolute_uri;

pub(crate) const JSONAPI_MEDIA_TYPE: &str = "application/vnd.api+json";

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

pub(crate) fn unquote_http_quoted_string(value: &str) -> Option<String> {
    if !is_valid_quoted_string(value) {
        return None;
    }

    let mut unquoted = Vec::with_capacity(value.len() - 2);
    let mut quoted_pair = false;
    for byte in &value.as_bytes()[1..value.len() - 1] {
        if quoted_pair {
            unquoted.push(*byte);
            quoted_pair = false;
        } else if *byte == b'\\' {
            quoted_pair = true;
        } else {
            unquoted.push(*byte);
        }
    }
    String::from_utf8(unquoted).ok()
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

/// A tokenized media type parameter.
pub(crate) struct MediaParameter {
    pub(crate) name: String,
    pub(crate) value: Option<String>,
    pub(crate) quoted: bool,
}

/// Splits a media type or media range into its type and parameters.
pub(crate) fn parse_media_type(value: &str) -> Option<(String, Vec<MediaParameter>)> {
    let mut segments = split_quoted(value, ';').into_iter();
    let media_type = segments.next()?.trim();
    if media_type.is_empty() {
        return None;
    }
    let mut parameters = Vec::new();
    for segment in segments {
        let segment = segment.trim();
        let (name, raw_value) = match segment.split_once('=') {
            Some((name, raw_value)) => (name.trim(), Some(raw_value.trim())),
            None => (segment, None),
        };
        if !is_http_token(name) {
            return None;
        }
        let (value, quoted) = match raw_value {
            None => (None, false),
            Some(raw_value) => match unquote_http_quoted_string(raw_value) {
                Some(value) => (Some(value), true),
                None if is_http_token(raw_value) => (Some(raw_value.to_owned()), false),
                None => return None,
            },
        };
        parameters.push(MediaParameter {
            name: name.to_owned(),
            value,
            quoted,
        });
    }
    Some((media_type.to_owned(), parameters))
}

/// The `ext` and `profile` capabilities declared by a JSON:API media type.
#[derive(Default)]
pub(crate) struct CapabilityParameters {
    pub(crate) extensions: Vec<String>,
    /// Validated but currently only advertised; profile application is deferred.
    #[allow(dead_code)]
    pub(crate) profiles: Vec<String>,
}

/// Parses the `ext` and `profile` parameters, rejecting any other parameter.
pub(crate) fn capability_parameters(
    parameters: &[MediaParameter],
) -> Result<CapabilityParameters, &'static str> {
    let mut extensions = None;
    let mut profiles = None;
    for parameter in parameters {
        match parameter.name.to_ascii_lowercase().as_str() {
            "ext" if extensions.is_none() => {
                if !parameter.quoted {
                    return Err("the JSON:API extension parameter must be quoted");
                }
                let value = parameter
                    .value
                    .as_deref()
                    .ok_or("invalid extension parameter")?;
                if !has_valid_uri_list(value) {
                    return Err("the JSON:API extension parameter must be a URI list");
                }
                extensions = Some(split_uri_list(value));
            }
            "profile" if profiles.is_none() => {
                if !parameter.quoted {
                    return Err("the JSON:API profile parameter must be quoted");
                }
                let value = parameter
                    .value
                    .as_deref()
                    .ok_or("invalid profile parameter")?;
                if !has_valid_uri_list(value) {
                    return Err("the JSON:API profile parameter must be a URI list");
                }
                profiles = Some(split_uri_list(value));
            }
            _ => return Err("only the JSON:API ext and profile parameters are supported"),
        }
    }
    Ok(CapabilityParameters {
        extensions: extensions.unwrap_or_default(),
        profiles: profiles.unwrap_or_default(),
    })
}

/// Parses the `ext`/`profile` capabilities of a JSON:API `Content-Type` value.
pub(crate) fn parse_jsonapi_content_type(
    value: &str,
) -> Result<CapabilityParameters, &'static str> {
    let Some((media_type, parameters)) = parse_media_type(value) else {
        return Err("the JSON:API media type is malformed");
    };
    if !media_type.eq_ignore_ascii_case(JSONAPI_MEDIA_TYPE) {
        return Err("request bodies must use the JSON:API media type");
    }
    capability_parameters(&parameters)
}

/// Returns whether the `Accept` header accepts a JSON:API representation whose
/// extension URIs satisfy `extension_policy`.
///
/// A missing `Accept` header accepts any representation. Ranges modified by a
/// parameter other than `ext`, `profile`, or `q` do not match.
pub(crate) fn accepts_jsonapi<F>(headers: &HeaderMap, extension_policy: F) -> bool
where
    F: Fn(&[String]) -> bool,
{
    let values = headers.get_all(axum::http::header::ACCEPT);
    if values.iter().next().is_none() {
        return true;
    }

    let mut best_match: Option<(u8, f32)> = None;
    for value in values {
        let Ok(value) = value.to_str() else {
            return false;
        };
        for range in split_quoted(value, ',') {
            let Some((specificity, quality, extensions)) = parse_accept_range(range) else {
                continue;
            };
            if !extension_policy(&extensions) {
                continue;
            }
            match best_match {
                Some((best_specificity, _)) if best_specificity > specificity => {}
                Some((best_specificity, best_quality))
                    if best_specificity == specificity && best_quality >= quality => {}
                _ => best_match = Some((specificity, quality)),
            }
        }
    }
    best_match.is_some_and(|(_, quality)| quality > 0.0)
}

fn parse_accept_range(range: &str) -> Option<(u8, f32, Vec<String>)> {
    let mut segments = split_quoted(range, ';').into_iter();
    let media_type = segments.next()?.trim();
    let specificity = if media_type.eq_ignore_ascii_case(JSONAPI_MEDIA_TYPE) {
        2
    } else if media_type.eq_ignore_ascii_case("application/*") {
        1
    } else if media_type == "*/*" {
        0
    } else {
        return None;
    };

    let mut quality = 1.0_f32;
    let mut has_quality = false;
    let mut extensions: Option<Vec<String>> = None;
    let mut profile_seen = false;
    for segment in segments {
        let segment = segment.trim();
        if has_quality {
            // Parameters after q are Accept extensions and do not affect this
            // representation.
            if !is_valid_accept_extension(segment) {
                return None;
            }
            continue;
        }
        let (name, raw_value) = match segment.split_once('=') {
            Some((name, raw_value)) => (name.trim(), Some(raw_value.trim())),
            None => (segment, None),
        };
        if !is_http_token(name) {
            return None;
        }
        match name.to_ascii_lowercase().as_str() {
            "q" => {
                has_quality = true;
                let raw_value = raw_value?;
                if is_valid_quoted_string(raw_value) {
                    return None;
                }
                quality = parse_quality_value(raw_value)?;
            }
            "ext" if extensions.is_none() => {
                let value = unquote_http_quoted_string(raw_value?)?;
                if !has_valid_uri_list(&value) {
                    return None;
                }
                extensions = Some(split_uri_list(&value));
            }
            "profile" if !profile_seen => {
                let value = unquote_http_quoted_string(raw_value?)?;
                if !has_valid_uri_list(&value) {
                    return None;
                }
                profile_seen = true;
            }
            _ => return None,
        }
    }
    Some((specificity, quality, extensions.unwrap_or_default()))
}

pub(crate) fn parse_quality_value(value: &str) -> Option<f32> {
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    if fractional.len() > 3 || !fractional.bytes().all(|digit| digit.is_ascii_digit()) {
        return None;
    }
    match whole {
        "0" => value.parse().ok(),
        "1" if fractional.bytes().all(|digit| digit == b'0') => value.parse().ok(),
        _ => None,
    }
}

pub(crate) fn has_valid_uri_list(value: &str) -> bool {
    !value.is_empty()
        && value
            .split(' ')
            .all(|uri| !uri.is_empty() && is_valid_absolute_uri(uri))
}

fn split_uri_list(value: &str) -> Vec<String> {
    value.split(' ').map(str::to_owned).collect()
}

pub(crate) fn split_quoted(value: &str, delimiter: char) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut segment_start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == delimiter && !quoted {
            segments.push(&value[segment_start..index]);
            segment_start = index + character.len_utf8();
        }
    }
    if quoted || escaped {
        return Vec::new();
    }
    segments.push(&value[segment_start..]);
    segments
}

fn is_quoted_text_byte(byte: u8) -> bool {
    matches!(byte, b'\t' | b' ' | b'!' | 0x23..=0x5b | 0x5d..=0x7e | 0x80..=0xff)
}

fn is_quoted_pair_byte(byte: u8) -> bool {
    matches!(byte, b'\t' | b' ' | 0x21..=0x7e | 0x80..=0xff)
}
