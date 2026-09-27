//! The account's saved items through Spotify's collection sync API.
//!
//! `POST /collection/v2/paging` answers only its vendor content type; anything
//! else is an empty-bodied 400. librespot does not compile the proto, so the
//! few small messages involved are encoded by hand.

use http::{HeaderMap, HeaderValue, Method};
use librespot_core::session::Session;

use crate::error::BotError;

const PAGING: &str = "/collection/v2/paging";
const CONTENT_TYPE: &str = "application/vnd.collection-v2.spotify.proto";

/// Liked tracks and saved albums.
pub const SET_COLLECTION: &str = "collection";

const PAGE_LIMIT: u64 = 300;
const MAX_PAGES: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub struct SavedItem {
    pub uri: String,
    /// Unix seconds.
    pub added_at: i64,
}

/// Every item in `set`, removed ones dropped.
pub async fn saved_items(session: &Session, set: &str) -> Result<Vec<SavedItem>, BotError> {
    let username = session.username();
    let mut items = Vec::new();
    let mut token = String::new();

    for _ in 0..MAX_PAGES {
        let request = page_request(&username, set, &token);
        let payload = session
            .spclient()
            .request(&Method::POST, PAGING, Some(headers()), Some(&request))
            .await
            .map_err(|e| BotError::Playback(format!("Collection fetch failed: {e}")))?;

        let page = parse_page(&payload);
        items.extend(page.items);
        match page.next_page_token {
            Some(next) if !next.is_empty() => token = next,
            _ => return Ok(items),
        }
    }
    Err(BotError::Playback(format!(
        "Collection paging did not finish after {MAX_PAGES} pages"
    )))
}

/// Liked track uris, most recently liked first.
pub async fn liked_track_uris(session: &Session) -> Result<Vec<String>, BotError> {
    let items = saved_items(session, SET_COLLECTION).await?;
    Ok(liked_tracks_newest_first(items))
}

fn liked_tracks_newest_first(mut items: Vec<SavedItem>) -> Vec<String> {
    items.retain(|item| item.uri.starts_with("spotify:track:"));
    items.sort_by_key(|item| std::cmp::Reverse(item.added_at));
    items.into_iter().map(|item| item.uri).collect()
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
    headers.insert(http::header::ACCEPT, HeaderValue::from_static(CONTENT_TYPE));
    headers
}

fn page_request(username: &str, set: &str, token: &str) -> Vec<u8> {
    let mut request = Vec::new();
    wire::string(1, username, &mut request);
    wire::string(2, set, &mut request);
    if !token.is_empty() {
        wire::string(3, token, &mut request);
    }
    wire::varint(4, PAGE_LIMIT, &mut request);
    request
}

struct Page {
    items: Vec<SavedItem>,
    next_page_token: Option<String>,
}

fn parse_page(payload: &[u8]) -> Page {
    let mut items = Vec::new();
    let mut next_page_token = None;
    for (field, value) in wire::fields(payload) {
        match (field, value) {
            (1, wire::Field::Bytes(item)) => {
                let mut uri = String::new();
                let mut added_at = 0i64;
                let mut removed = false;
                for (f, v) in wire::fields(item) {
                    match (f, v) {
                        (1, wire::Field::Bytes(b)) => uri = String::from_utf8_lossy(b).into_owned(),
                        // Sometimes sent as fixed32; read as u32 so it cannot sign-extend.
                        (2, v) => {
                            if let Some(n) = v.number() {
                                added_at = n as u32 as i64;
                            }
                        }
                        (3, v) => removed = v.number().unwrap_or(0) != 0,
                        _ => {}
                    }
                }
                if !removed && !uri.is_empty() {
                    items.push(SavedItem { uri, added_at });
                }
            }
            (2, wire::Field::Bytes(b)) => {
                next_page_token = Some(String::from_utf8_lossy(b).into_owned());
            }
            _ => {}
        }
    }
    Page { items, next_page_token }
}

/// Protobuf wire format for single-byte tags (fields below 16).
mod wire {
    fn raw_varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }

    pub(super) fn string(field: u32, value: &str, out: &mut Vec<u8>) {
        out.push(((field << 3) | 2) as u8);
        raw_varint(value.len() as u64, out);
        out.extend_from_slice(value.as_bytes());
    }

    pub(super) fn varint(field: u32, value: u64, out: &mut Vec<u8>) {
        out.push((field << 3) as u8);
        raw_varint(value, out);
    }

    #[cfg(test)]
    pub(super) fn message(field: u32, message: &[u8], out: &mut Vec<u8>) {
        out.push(((field << 3) | 2) as u8);
        raw_varint(message.len() as u64, out);
        out.extend_from_slice(message);
    }

    pub(super) enum Field<'a> {
        Number(u64),
        Bytes(&'a [u8]),
    }

    impl Field<'_> {
        pub(super) fn number(&self) -> Option<u64> {
            match self {
                Field::Number(v) => Some(*v),
                Field::Bytes(_) => None,
            }
        }
    }

    /// One level of a message. An unknown or truncated field ends the walk.
    pub(super) fn fields(mut data: &[u8]) -> Vec<(u32, Field<'_>)> {
        fn take_varint(data: &mut &[u8]) -> Option<u64> {
            let mut v: u64 = 0;
            let mut shift = 0;
            while let Some((&byte, rest)) = data.split_first() {
                *data = rest;
                v |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return Some(v);
                }
                shift += 7;
                if shift >= 64 {
                    return None;
                }
            }
            None
        }
        fn take_fixed(data: &mut &[u8], width: usize) -> Option<u64> {
            if data.len() < width {
                return None;
            }
            let (bytes, rest) = data.split_at(width);
            *data = rest;
            Some(bytes.iter().enumerate().fold(0u64, |v, (i, b)| v | (u64::from(*b) << (8 * i))))
        }

        let mut out = Vec::new();
        while !data.is_empty() {
            let Some(tag) = take_varint(&mut data) else { break };
            let field = (tag >> 3) as u32;
            let value = match tag & 7 {
                0 => take_varint(&mut data).map(Field::Number),
                1 => take_fixed(&mut data, 8).map(Field::Number),
                5 => take_fixed(&mut data, 4).map(Field::Number),
                2 => take_varint(&mut data).and_then(|len| {
                    let len = usize::try_from(len).ok().filter(|len| *len <= data.len())?;
                    let (bytes, rest) = data.split_at(len);
                    data = rest;
                    Some(Field::Bytes(bytes))
                }),
                _ => None,
            };
            let Some(value) = value else { break };
            out.push((field, value));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(uri: &str, added_at: u64, removed: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        wire::string(1, uri, &mut bytes);
        wire::varint(2, added_at, &mut bytes);
        if removed {
            wire::varint(3, 1, &mut bytes);
        }
        bytes
    }

    fn page(items: &[Vec<u8>], next: Option<&str>) -> Vec<u8> {
        let mut bytes = Vec::new();
        for item in items {
            wire::message(1, item, &mut bytes);
        }
        if let Some(next) = next {
            wire::string(2, next, &mut bytes);
        }
        wire::string(3, "sync-token", &mut bytes);
        bytes
    }

    #[test]
    fn a_page_answers_its_items_with_timestamps() {
        let payload = page(
            &[
                item("spotify:track:aaa", 1_700_000_000, false),
                item("spotify:track:bbb", 1_800_000_000, false),
            ],
            None,
        );
        let parsed = parse_page(&payload);
        assert_eq!(
            parsed.items,
            vec![
                SavedItem { uri: "spotify:track:aaa".into(), added_at: 1_700_000_000 },
                SavedItem { uri: "spotify:track:bbb".into(), added_at: 1_800_000_000 },
            ]
        );
        assert!(parsed.next_page_token.is_none());
    }

    #[test]
    fn a_removed_item_is_not_saved() {
        let payload = page(&[item("spotify:track:gone", 0, true)], None);
        assert!(parse_page(&payload).items.is_empty());
    }

    #[test]
    fn the_page_token_is_read() {
        let payload = page(&[], Some("more-please"));
        assert_eq!(parse_page(&payload).next_page_token.as_deref(), Some("more-please"));
    }

    #[test]
    fn a_page_request_encodes_username_set_and_limit() {
        let mut expected = vec![0x0A, 7];
        expected.extend_from_slice(b"someone");
        expected.extend_from_slice(&[0x12, 10]);
        expected.extend_from_slice(b"collection");
        expected.extend_from_slice(&[0x20, 0xAC, 0x02]);
        assert_eq!(page_request("someone", "collection", ""), expected);
    }

    #[test]
    fn a_page_request_carries_the_token_when_there_is_one() {
        let request = page_request("u", "collection", "tok");
        let fields = wire::fields(&request);
        assert!(fields
            .iter()
            .any(|(f, v)| *f == 3 && matches!(v, wire::Field::Bytes(b) if *b == b"tok")));
    }

    #[test]
    fn garbage_parses_to_nothing() {
        let parsed = parse_page(&[0xFF, 0x03, 0x9C, 0x01]);
        assert!(parsed.items.is_empty());
        assert!(parsed.next_page_token.is_none());
    }

    #[test]
    fn a_fixed32_timestamp_reads_as_the_liked_date() {
        let mut item = Vec::new();
        wire::string(1, "spotify:track:ccc", &mut item);
        item.push((2 << 3) | 5);
        item.extend_from_slice(&1_700_000_000u32.to_le_bytes());
        let mut payload = Vec::new();
        wire::message(1, &item, &mut payload);

        assert_eq!(
            parse_page(&payload).items,
            vec![SavedItem { uri: "spotify:track:ccc".into(), added_at: 1_700_000_000 }]
        );
    }

    #[test]
    fn liked_keeps_tracks_only_newest_first() {
        let items = vec![
            SavedItem { uri: "spotify:track:old".into(), added_at: 1 },
            SavedItem { uri: "spotify:album:saved".into(), added_at: 5 },
            SavedItem { uri: "spotify:track:new".into(), added_at: 9 },
        ];
        assert_eq!(
            liked_tracks_newest_first(items),
            vec!["spotify:track:new".to_string(), "spotify:track:old".to_string()]
        );
    }
}
