//! Minimal .torrent (bencode) reader: info hash, piece length and the byte offset of every file.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};

#[derive(Clone, Debug)]
pub struct Layout {
    pub hash: String,
    pub piece_length: u64,
    /// "Torrent name/sub/file.mkv" (qBittorrent's file naming) -> offset of the file inside the torrent
    pub offsets: HashMap<String, u64>,
}

enum B<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(Vec<B<'a>>),
    Dict(Vec<(&'a [u8], B<'a>)>),
}

impl<'a> B<'a> {
    fn get(&self, key: &str) -> Option<&B<'a>> {
        match self {
            B::Dict(entries) => entries.iter().find(|(k, _)| *k == key.as_bytes()).map(|(_, v)| v),
            _ => None,
        }
    }
    fn str(&self) -> Option<String> {
        match self {
            B::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    }
    fn int(&self) -> Option<i64> {
        match self {
            B::Int(n) => Some(*n),
            _ => None,
        }
    }
}

struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
    info_span: Option<(usize, usize)>,
}

impl<'a> Parser<'a> {
    fn parse(&mut self, depth: usize) -> Result<B<'a>> {
        let c = *self.buf.get(self.pos).context("truncated .torrent file")?;
        match c {
            b'i' => {
                let end = self.find(b'e')?;
                let n = std::str::from_utf8(&self.buf[self.pos + 1..end])?.parse()?;
                self.pos = end + 1;
                Ok(B::Int(n))
            }
            b'l' => {
                self.pos += 1;
                let mut items = Vec::new();
                while self.peek()? != b'e' {
                    items.push(self.parse(depth + 1)?);
                }
                self.pos += 1;
                Ok(B::List(items))
            }
            b'd' => {
                self.pos += 1;
                let mut entries = Vec::new();
                while self.peek()? != b'e' {
                    let B::Bytes(key) = self.parse(depth + 1)? else { bail!("invalid dictionary key") };
                    let start = self.pos;
                    let value = self.parse(depth + 1)?;
                    if depth == 0 && key == b"info" {
                        self.info_span = Some((start, self.pos));
                    }
                    entries.push((key, value));
                }
                self.pos += 1;
                Ok(B::Dict(entries))
            }
            b'0'..=b'9' => {
                let colon = self.find(b':')?;
                let len: usize = std::str::from_utf8(&self.buf[self.pos..colon])?.parse()?;
                let start = colon + 1;
                let end = start.checked_add(len).filter(|e| *e <= self.buf.len()).context("truncated .torrent file")?;
                self.pos = end;
                Ok(B::Bytes(&self.buf[start..end]))
            }
            _ => bail!("invalid .torrent file"),
        }
    }
    fn peek(&self) -> Result<u8> {
        self.buf.get(self.pos).copied().context("truncated .torrent file")
    }
    fn find(&self, byte: u8) -> Result<usize> {
        self.buf[self.pos..].iter().position(|b| *b == byte).map(|i| self.pos + i).context("invalid .torrent file")
    }
}

pub fn parse_layout(buf: &[u8]) -> Result<Layout> {
    let mut p = Parser { buf, pos: 0, info_span: None };
    let root = p.parse(0)?;
    let info = root.get("info").context("invalid .torrent file (no info)")?;
    let (start, end) = p.info_span.context("invalid .torrent file")?;
    let hash: String = Sha1::digest(&buf[start..end]).iter().map(|b| format!("{b:02x}")).collect();
    let name = info.get("name.utf-8").or(info.get("name")).and_then(B::str).unwrap_or_default();
    let mut offsets = HashMap::new();
    if let Some(B::List(files)) = info.get("files") {
        let mut offset = 0u64;
        for f in files {
            let parts: Vec<String> = match f.get("path.utf-8").or(f.get("path")) {
                Some(B::List(parts)) => parts.iter().filter_map(B::str).collect(),
                _ => vec![],
            };
            offsets.insert(std::iter::once(name.clone()).chain(parts).collect::<Vec<_>>().join("/"), offset);
            offset += f.get("length").and_then(B::int).unwrap_or(0).max(0) as u64;
        }
    } else if info.get("length").is_some() {
        offsets.insert(name.clone(), 0);
    }
    let piece_length = info.get("piece length").and_then(B::int).context("invalid .torrent file (no piece length)")?;
    Ok(Layout { hash, piece_length: piece_length.max(1) as u64, offsets })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_file_layout() {
        let t = b"d8:announce3:url4:infod5:filesld6:lengthi100e4:pathl5:a.mkveed6:lengthi5e4:pathl3:sub5:b.srteee4:name4:Test12:piece lengthi64eee";
        let l = parse_layout(t).unwrap();
        assert_eq!(l.piece_length, 64);
        assert_eq!(l.offsets["Test/a.mkv"], 0);
        assert_eq!(l.offsets["Test/sub/b.srt"], 100);
        assert_eq!(l.hash.len(), 40);
    }
}
