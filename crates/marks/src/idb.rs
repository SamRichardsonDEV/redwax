//! Firefox IndexedDB keys, and the protobuf inside the entity store ones.
//!
//! Only keys are read. The rows beside them hold the encrypted audio chunks,
//! which redwax cannot decrypt.

use base64::Engine;

/// Firefox tags an IndexedDB key with its type in the first byte.
const STRING_KEY: u8 = 0x30;

/// The string a Firefox IndexedDB key holds, or `None` for any other type.
///
/// Every byte of the UTF-8 is stored one higher than it is, so the terminator
/// stays unique. Bytes above 0x7f use a wider scheme that YouTube ids never need.
pub fn decode_key(encoded_key: &[u8]) -> Option<String> {
    let (key_type, encoded_text) = encoded_key.split_first()?;
    if *key_type != STRING_KEY {
        return None;
    }
    let mut decoded_text = String::with_capacity(encoded_text.len());
    for byte in encoded_text {
        let decoded_byte = byte.checked_sub(1)?;
        if !decoded_byte.is_ascii() {
            return None;
        }
        decoded_text.push(decoded_byte as char);
    }
    Some(decoded_text)
}

/// One row of the entity store: what the web app stored, and what kind it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityKey {
    pub id: String,
    pub entity_type: u64,
}

/// The id and type inside an entity store key.
///
/// The key is percent-encoded base64 around a protobuf of two fields: field 2
/// carries the id, field 4 the entity type. Anything else is skipped.
pub fn parse_entity_key(key: &str) -> Option<EntityKey> {
    let base64_key = percent_decode(key)?;
    let unpadded_base64_key = base64_key.trim_end_matches('=');
    let protobuf_bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(unpadded_base64_key)
        .ok()?;
    let mut entity_id = None;
    let mut entity_type = None;
    let mut byte_offset = 0;
    while byte_offset < protobuf_bytes.len() {
        let (field_tag, next_byte_offset) = varint(&protobuf_bytes, byte_offset)?;
        byte_offset = next_byte_offset;
        match field_tag & 7 {
            0 => {
                let (field_value, next_byte_offset) = varint(&protobuf_bytes, byte_offset)?;
                byte_offset = next_byte_offset;
                if field_tag >> 3 == 4 {
                    entity_type = Some(field_value);
                }
            }
            2 => {
                let (field_length_bytes, next_byte_offset) = varint(&protobuf_bytes, byte_offset)?;
                let field_end_offset =
                    next_byte_offset.checked_add(usize::try_from(field_length_bytes).ok()?)?;
                let field_bytes = protobuf_bytes.get(next_byte_offset..field_end_offset)?;
                byte_offset = field_end_offset;
                if field_tag >> 3 == 2 {
                    entity_id = Some(String::from_utf8(field_bytes.to_vec()).ok()?);
                }
            }
            _ => return None,
        }
    }
    Some(EntityKey {
        id: entity_id?,
        entity_type: entity_type?,
    })
}

fn varint(bytes: &[u8], mut byte_offset: usize) -> Option<(u64, usize)> {
    let mut decoded_value: u64 = 0;
    let mut bit_shift = 0;
    loop {
        let byte = *bytes.get(byte_offset)?;
        byte_offset += 1;
        decoded_value |= u64::from(byte & 0x7f) << bit_shift;
        if byte & 0x80 == 0 {
            return Some((decoded_value, byte_offset));
        }
        bit_shift += 7;
        if bit_shift > 63 {
            return None;
        }
    }
}

/// `%XX` back to the byte it names. The web app writes keys through
/// `encodeURIComponent`, which escapes the `+`, `/` and `=` of base64.
fn percent_decode(encoded_text: &str) -> Option<String> {
    if !encoded_text.contains('%') {
        return Some(encoded_text.to_owned());
    }
    let encoded_bytes = encoded_text.as_bytes();
    let mut decoded_text = String::with_capacity(encoded_bytes.len());
    let mut byte_offset = 0;
    while byte_offset < encoded_bytes.len() {
        if encoded_bytes[byte_offset] == b'%' {
            let hex_digits = encoded_bytes.get(byte_offset + 1..byte_offset + 3)?;
            let byte = u8::from_str_radix(std::str::from_utf8(hex_digits).ok()?, 16).ok()?;
            decoded_text.push(char::from(byte));
            byte_offset += 3;
        } else {
            decoded_text.push(char::from(encoded_bytes[byte_offset]));
            byte_offset += 1;
        }
    }
    Some(decoded_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plus-one encoding Firefox writes, built the way the browser does.
    fn encode_key(text: &str) -> Vec<u8> {
        let mut raw = vec![STRING_KEY];
        raw.extend(text.bytes().map(|byte| byte + 1));
        raw
    }

    /// A protobuf key the way the web app builds it: field 2 the id, field 4
    /// the entity type, then the trailing field 5 the store itself adds.
    fn encode_entity_key(id: &str, entity_type: u64) -> String {
        let mut proto = vec![0x12, id.len() as u8];
        proto.extend(id.as_bytes());
        proto.push(0x20);
        let mut left = entity_type;
        while left >= 0x80 {
            proto.push((left as u8) | 0x80);
            left >>= 7;
        }
        proto.push(left as u8);
        proto.extend([0x28, 0x01]);
        let text = base64::engine::general_purpose::STANDARD.encode(&proto);
        text.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
    }

    #[test]
    fn a_string_key_decodes_by_subtracting_one() {
        assert_eq!(
            decode_key(&encode_key("1-Fxb9jsOuI|a")).as_deref(),
            Some("1-Fxb9jsOuI|a")
        );
        assert_eq!(
            decode_key(b"\x30\x31\x4f\x39\x42\x56\x50\x71\x2e\x2e\x72\x39\x7d\x62").as_deref(),
            Some("0N8AUOp--q8|a")
        );
        assert_eq!(decode_key(&[]), None);
        // A number key is a different type byte and must not read as text.
        assert_eq!(decode_key(&[0x10, 0x41]), None);
    }

    #[test]
    fn an_entity_key_yields_its_id_and_type() {
        let key = encode_entity_key("4L56DPmFl8w", 141);
        assert_eq!(
            parse_entity_key(&key),
            Some(EntityKey {
                id: "4L56DPmFl8w".to_owned(),
                entity_type: 141
            })
        );
        assert_eq!(
            parse_entity_key(&encode_entity_key("PLtestfixture600000000000000000000", 248)),
            Some(EntityKey {
                id: "PLtestfixture600000000000000000000".to_owned(),
                entity_type: 248
            })
        );
        assert_eq!(
            parse_entity_key(&encode_entity_key("MPREb_ExampleAlbum", 999)),
            Some(EntityKey {
                id: "MPREb_ExampleAlbum".to_owned(),
                entity_type: 999
            })
        );
    }

    #[test]
    fn a_key_that_is_not_an_entity_reads_as_none() {
        assert_eq!(parse_entity_key("not base64 at all!!"), None);
        assert_eq!(parse_entity_key(""), None);
        // Field 2 alone carries no type, so the row cannot be classified.
        assert_eq!(parse_entity_key("EgNhYmM%3D"), None);
    }

    #[test]
    fn a_literal_entity_key_shape_round_trips() {
        let key = concat!("EgpmaXh0dXJl", "LWlkIOoBKAE=");
        assert_eq!(
            parse_entity_key(key),
            Some(EntityKey {
                id: "fixture-id".to_owned(),
                entity_type: 234
            })
        );
    }
}
