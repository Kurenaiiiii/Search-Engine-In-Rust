//! HTTP JSON API + static web UI, standard library only (no dependencies).
//! Endpoints:
//!   GET  /                    static UI
//!   GET  /health  /ready
//!   GET  /api/stats
//!   POST /api/docs            {"title": "..", "body": ".."} -> {"id": N}
//!   POST /api/docs/bulk       NDJSON or JSON array -> {"added": [ids]}
//!   GET  /api/docs/:id  DELETE /api/docs/:id  PUT /api/docs/:id
//!   GET  /api/search?q=..&k=..&offset=..&fields=..&highlight=1&scorer=bm25&msm=1
//!   GET  /api/suggest?prefix=..&field=body&limit=8
//!   GET  /api/spell?term=..&field=body

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use se_core::search::{did_you_mean, snippet, suggest, Scorer, SearchOptions};
use se_core::Index;

const UI_HTML: &str = include_str!("ui.html");
const MAX_BODY: usize = 8 * 1024 * 1024;

// ---------- tiny JSON ----------

#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn get(&self, key: &str) -> Option<&J> {
        if let J::Obj(pairs) = self {
            pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
        } else {
            None
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        if let J::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        if let J::Num(n) = self {
            Some(*n as u64)
        } else {
            None
        }
    }
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && (self.b[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", c as char, self.i))
        }
    }
    fn value(&mut self) -> Result<J, String> {
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(J::Str(self.string()?)),
            Some(b't') => self.lit("true", J::Bool(true)),
            Some(b'f') => self.lit("false", J::Bool(false)),
            Some(b'n') => self.lit("null", J::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(format!("unexpected character at byte {}", self.i)),
        }
    }
    fn lit(&mut self, s: &str, v: J) -> Result<J, String> {
        if self.b.get(self.i..self.i + s.len()) == Some(s.as_bytes()) {
            self.i += s.len();
            Ok(v)
        } else {
            Err(format!("invalid literal at byte {}", self.i))
        }
    }
    fn number(&mut self) -> Result<J, String> {
        let start = self.i;
        while self.i < self.b.len()
            && matches!(
                self.b[self.i],
                b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'
            )
        {
            self.i += 1;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .map(J::Num)
            .ok_or_else(|| format!("invalid number at byte {start}"))
    }
    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut s = String::new();
        loop {
            let c = *self.b.get(self.i).ok_or("unterminated string")?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("bad escape")?;
                    self.i += 1;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        b'u' => {
                            let hex = self.b.get(self.i..self.i + 4).ok_or("bad \\u escape")?;
                            self.i += 4;
                            let cp =
                                u32::from_str_radix(std::str::from_utf8(hex).unwrap_or(""), 16)
                                    .unwrap_or(0xFFFD);
                            s.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err("bad escape".to_string()),
                    }
                }
                _ => {
                    // raw UTF-8 char
                    let mut len = 1;
                    if c >= 0x80 {
                        len = if c >= 0xF0 {
                            4
                        } else if c >= 0xE0 {
                            3
                        } else {
                            2
                        };
                    }
                    let bytes = self.b.get(self.i - 1..self.i - 1 + len).ok_or("bad utf8")?;
                    s.push_str(std::str::from_utf8(bytes).unwrap_or("\u{FFFD}"));
                    self.i += len - 1;
                }
            }
        }
        Ok(s)
    }
    fn object(&mut self) -> Result<J, String> {
        self.eat(b'{')?;
        let mut pairs = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(J::Obj(pairs));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.eat(b':')?;
            let v = self.value()?;
            pairs.push((k, v));
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return Err("expected ',' or '}' in object".to_string()),
            }
        }
        Ok(J::Obj(pairs))
    }
    fn array(&mut self) -> Result<J, String> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(J::Arr(items));
        }
        loop {
            items.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                _ => return Err("expected ',' or ']' in array".to_string()),
            }
        }
        Ok(J::Arr(items))
    }
}

pub fn parse_json(s: &str) -> Result<J, String> {
    let mut p = P {
        b: s.as_bytes(),
        i: 0,
    };
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err("trailing characters after JSON value".to_string());
    }
    Ok(v)
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

// ---------- server ----------

pub struct Server {
    pub index: Arc<Index>,
    pub token: Option<String>,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn log(method: &str, path: &str, status: u16, ms: u128) {
    eprintln!(
        "{{\"ts\":{},\"method\":\"{}\",\"path\":\"{}\",\"status\":{},\"ms\":{}}}",
        now_ms(),
        method,
        path.replace('"', "'"),
        status,
        ms
    );
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn respond(stream: &mut std::net::TcpStream, code: u16, ctype: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        code,
        reason(code),
        ctype,
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn err(stream: &mut std::net::TcpStream, code: u16, msg: &str) {
    respond(
        stream,
        code,
        "application/json",
        &format!("{{\"error\":\"{}\"}}", esc(msg)),
    );
}

fn decode_url(s: &str) -> String {
    let mut o = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                o.push(' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                if let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3]) {
                    if let Ok(v) = u8::from_str_radix(hex, 16) {
                        o.push(v as char);
                        i += 3;
                        continue;
                    }
                }
                o.push('%');
                i += 1;
            }
            c => {
                o.push(c as char);
                i += 1;
            }
        }
    }
    o
}

fn query_params(qs: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        match pair.find('=') {
            Some(i) => {
                m.insert(decode_url(&pair[..i]), decode_url(&pair[i + 1..]));
            }
            None => {
                m.insert(decode_url(pair), String::new());
            }
        }
    }
    m
}

fn jobj_to_fields(j: &J) -> Result<BTreeMap<String, String>, String> {
    match j {
        J::Obj(pairs) => {
            let mut m = BTreeMap::new();
            for (k, v) in pairs {
                match v {
                    J::Str(s) => {
                        m.insert(k.clone(), s.clone());
                    }
                    J::Num(n) => {
                        m.insert(k.clone(), n.to_string());
                    }
                    J::Bool(b) => {
                        m.insert(k.clone(), b.to_string());
                    }
                    J::Null => {}
                    _ => return Err(format!("field '{k}' must be a string, number or boolean")),
                }
            }
            if m.is_empty() {
                return Err("document must have at least one field".to_string());
            }
            Ok(m)
        }
        _ => Err("document must be a JSON object".to_string()),
    }
}

impl Server {
    fn authorized(&self, headers: &BTreeMap<String, String>) -> bool {
        match &self.token {
            None => true,
            Some(t) => headers
                .get("authorization")
                .map(|h| h == &format!("Bearer {t}"))
                .unwrap_or(false),
        }
    }

    fn handle(
        &self,
        stream: &mut std::net::TcpStream,
        method: &str,
        target: &str,
        headers: &BTreeMap<String, String>,
        body: &str,
    ) {
        let t0 = Instant::now();
        let (path, qs) = match target.find('?') {
            Some(i) => (&target[..i], &target[i + 1..]),
            None => (target, ""),
        };
        let params = query_params(qs);
        let mut status = 200u16;
        if path == "/" && method == "GET" {
            respond(stream, 200, "text/html; charset=utf-8", UI_HTML);
        } else if (path == "/health" || path == "/ready") && method == "GET" {
            respond(stream, 200, "application/json", "{\"ok\":true}");
        } else if path.starts_with("/api/") {
            if !self.authorized(headers) {
                status = 401;
                err(stream, 401, "unauthorized: missing or invalid bearer token");
            } else if path == "/api/stats" && method == "GET" {
                let s = self.index.stats();
                respond(
                    stream,
                    200,
                    "application/json",
                    &format!(
                        "{{\"live_docs\":{},\"segments\":{},\"buffered_docs\":{},\"postings_bytes\":{},\"dict_bytes\":{},\"next_id\":{}}}",
                        s.live_docs, s.segments, s.buffered_docs, s.postings_bytes, s.dict_bytes,
                        s.next_id
                    ),
                );
            } else if path == "/api/docs" && method == "POST" {
                match parse_json(body).and_then(|j| jobj_to_fields(&j)) {
                    Ok(fields) => match self.index.add(fields) {
                        Ok(id) => {
                            let _ = self.index.flush_and_maybe_merge();
                            respond(stream, 200, "application/json", &format!("{{\"id\":{id}}}"));
                        }
                        Err(e) => {
                            status = 500;
                            err(stream, 500, &e.to_string());
                        }
                    },
                    Err(e) => {
                        status = 400;
                        err(stream, 400, &e);
                    }
                }
            } else if path == "/api/docs/bulk" && method == "POST" {
                let trimmed = body.trim();
                let docs: Result<Vec<BTreeMap<String, String>>, String> =
                    if trimmed.starts_with('[') {
                        parse_json(trimmed).and_then(|j| match j {
                            J::Arr(items) => items.iter().map(jobj_to_fields).collect(),
                            _ => Err("bulk body must be NDJSON or a JSON array".to_string()),
                        })
                    } else {
                        let mut v = Vec::new();
                        let mut line_err: Option<String> = None;
                        for (n, line) in trimmed.lines().enumerate() {
                            if line.trim().is_empty() {
                                continue;
                            }
                            match parse_json(line.trim()).and_then(|j| jobj_to_fields(&j)) {
                                Ok(doc) => v.push(doc),
                                Err(e) => {
                                    line_err = Some(format!("line {}: {e}", n + 1));
                                    break;
                                }
                            }
                        }
                        match line_err {
                            Some(e) => Err(e),
                            None => Ok(v),
                        }
                    };
                match docs {
                    Ok(ds) => {
                        if ds.len() > 100_000 {
                            status = 400;
                            err(
                                stream,
                                400,
                                "too many documents in one bulk request (max 100000)",
                            );
                        } else {
                            let mut ids = Vec::with_capacity(ds.len());
                            for d in ds {
                                match self.index.add(d) {
                                    Ok(id) => ids.push(id),
                                    Err(e) => {
                                        status = 500;
                                        err(stream, 500, &e.to_string());
                                        break;
                                    }
                                }
                            }
                            let _ = self.index.flush_and_maybe_merge();
                            let list = ids
                                .iter()
                                .map(|i| i.to_string())
                                .collect::<Vec<_>>()
                                .join(",");
                            respond(
                                stream,
                                200,
                                "application/json",
                                &format!("{{\"added\":[{list}]}}"),
                            );
                        }
                    }
                    Err(e) => {
                        status = 400;
                        err(stream, 400, &e);
                    }
                }
            } else if path.starts_with("/api/docs/") && path.len() > 10 {
                let idstr = &path["/api/docs/".len()..];
                match idstr.parse::<u32>() {
                    Ok(id) => match method {
                        "GET" => match self.index.stored(id) {
                            Some(fields) => {
                                let mut o = format!("{{\"id\":{id},\"fields\":{{");
                                let mut first = true;
                                for (k, v) in &fields {
                                    if !first {
                                        o.push(',');
                                    }
                                    first = false;
                                    o.push_str(&format!("\"{}\":\"{}\"", esc(k), esc(v)));
                                }
                                o.push_str("}}");
                                respond(stream, 200, "application/json", &o);
                            }
                            None => {
                                status = 404;
                                err(stream, 404, "document not found");
                            }
                        },
                        "DELETE" => match self.index.delete(id) {
                            Ok(true) => {
                                let _ = self.index.flush_and_maybe_merge();
                                respond(stream, 200, "application/json", "{\"deleted\":true}");
                            }
                            Ok(false) => {
                                status = 404;
                                err(stream, 404, "document not found");
                            }
                            Err(e) => {
                                status = 500;
                                err(stream, 500, &e.to_string());
                            }
                        },
                        "PUT" => match parse_json(body).and_then(|j| jobj_to_fields(&j)) {
                            Ok(fields) => match self.index.update(id, fields) {
                                Ok(Some(new_id)) => {
                                    let _ = self.index.flush_and_maybe_merge();
                                    respond(
                                        stream,
                                        200,
                                        "application/json",
                                        &format!("{{\"id\":{new_id}}}"),
                                    );
                                }
                                Ok(None) => {
                                    status = 404;
                                    err(stream, 404, "document not found");
                                }
                                Err(e) => {
                                    status = 500;
                                    err(stream, 500, &e.to_string());
                                }
                            },
                            Err(e) => {
                                status = 400;
                                err(stream, 400, &e);
                            }
                        },
                        _ => {
                            status = 405;
                            err(stream, 405, "method not allowed");
                        }
                    },
                    Err(_) => {
                        status = 400;
                        err(stream, 400, "invalid document id");
                    }
                }
            } else if path == "/api/search" && (method == "GET" || method == "POST") {
                let q = if method == "GET" {
                    params.get("q").cloned().unwrap_or_default()
                } else {
                    parse_json(body)
                        .ok()
                        .and_then(|j| j.get("q").and_then(|v| v.as_str()).map(|s| s.to_string()))
                        .unwrap_or_default()
                };
                if q.is_empty() {
                    status = 400;
                    err(stream, 400, "missing required parameter 'q'");
                } else if q.len() > 4096 {
                    status = 400;
                    err(stream, 400, "query too long (max 4096 chars)");
                } else {
                    let k: usize = params
                        .get("k")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(10)
                        .min(100);
                    let offset: usize = params
                        .get("offset")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let highlight = params
                        .get("highlight")
                        .map(|v| v == "1" || v == "true")
                        .unwrap_or(false);
                    let field_sel: Option<Vec<String>> = params.get("fields").map(|v| {
                        v.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    });
                    let mut opts = SearchOptions::top(k + offset);
                    if params.get("scorer").map(|v| v == "tfidf").unwrap_or(false) {
                        opts.scorer = Scorer::Tfidf;
                    }
                    // minimum_should_match applies to free-text Should queries
                    opts.minimum_should_match = params
                        .get("msm")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1)
                        .max(1);
                    let t = Instant::now();
                    match self.index.search(&q, &opts) {
                        Ok(hits) => {
                            let ms = t.elapsed().as_millis();
                            let total = hits.len();
                            let snap = self.index.snapshot();
                            let terms = se_core::parse(&q)
                                .map(|qq| qq.literal_terms())
                                .unwrap_or_default();
                            let mut o = format!("{{\"total\":{total},\"took_ms\":{ms},\"hits\":[");
                            let mut first = true;
                            for h in hits.into_iter().skip(offset) {
                                if !first {
                                    o.push(',');
                                }
                                first = false;
                                let stored = snap.stored.get(&h.doc);
                                o.push_str(&format!("{{\"id\":{},\"score\":{:.4}", h.doc, h.score));
                                o.push_str(",\"fields\":{");
                                let mut ff = true;
                                if let Some(fields) = stored {
                                    for (fk, fv) in fields {
                                        if let Some(sel) = &field_sel {
                                            if !sel.contains(fk) {
                                                continue;
                                            }
                                        }
                                        if !ff {
                                            o.push(',');
                                        }
                                        ff = false;
                                        o.push_str(&format!("\"{}\":\"{}\"", esc(fk), esc(fv)));
                                    }
                                }
                                o.push('}');
                                if highlight {
                                    let text = stored
                                        .and_then(|m| m.get("body").or_else(|| m.values().next()))
                                        .cloned()
                                        .unwrap_or_default();
                                    o.push_str(&format!(
                                        ",\"snippet\":\"{}\"",
                                        esc(&snippet(&text, &terms, 12))
                                    ));
                                }
                                o.push('}');
                            }
                            o.push(']');
                            // did-you-mean on single-term queries with no hits
                            if total == 0 {
                                let toks: Vec<&str> = q.split_whitespace().collect();
                                if toks.len() == 1 {
                                    let field = snap
                                        .all_fields()
                                        .iter()
                                        .find(|f| *f == "body")
                                        .cloned()
                                        .unwrap_or_else(|| {
                                            snap.all_fields()
                                                .into_iter()
                                                .next()
                                                .unwrap_or_else(|| "body".to_string())
                                        });
                                    if let Some(sug) = did_you_mean(&snap, &field, toks[0]) {
                                        o.push_str(&format!(",\"did_you_mean\":\"{}\"", esc(&sug)));
                                    }
                                }
                            }
                            o.push('}');
                            respond(stream, 200, "application/json", &o);
                        }
                        Err(e) => {
                            status = 400;
                            err(stream, 400, &e);
                        }
                    }
                }
            } else if path == "/api/suggest" && method == "GET" {
                let prefix = params.get("prefix").cloned().unwrap_or_default();
                let field = params
                    .get("field")
                    .cloned()
                    .unwrap_or_else(|| "body".to_string());
                let limit: usize = params
                    .get("limit")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(8)
                    .min(50);
                if prefix.is_empty() {
                    status = 400;
                    err(stream, 400, "missing required parameter 'prefix'");
                } else {
                    let snap = self.index.snapshot();
                    let out = suggest(&snap, &field, &prefix, limit);
                    let list = out
                        .iter()
                        .map(|s| format!("\"{}\"", esc(s)))
                        .collect::<Vec<_>>()
                        .join(",");
                    respond(
                        stream,
                        200,
                        "application/json",
                        &format!("{{\"suggestions\":[{list}]}}"),
                    );
                }
            } else if path == "/api/spell" && method == "GET" {
                let term = params.get("term").cloned().unwrap_or_default();
                let field = params
                    .get("field")
                    .cloned()
                    .unwrap_or_else(|| "body".to_string());
                if term.is_empty() {
                    status = 400;
                    err(stream, 400, "missing required parameter 'term'");
                } else {
                    let snap = self.index.snapshot();
                    match did_you_mean(&snap, &field, &term) {
                        Some(s) => respond(
                            stream,
                            200,
                            "application/json",
                            &format!("{{\"suggestion\":\"{}\"}}", esc(&s)),
                        ),
                        None => respond(stream, 200, "application/json", "{\"suggestion\":null}"),
                    }
                }
            } else {
                status = 404;
                err(stream, 404, "not found");
            }
        } else {
            status = 404;
            err(stream, 404, "not found");
        }
        log(method, path, status, t0.elapsed().as_millis());
    }
}

fn read_request(
    stream: &mut std::net::TcpStream,
) -> Option<(String, String, BTreeMap<String, String>, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // read until end of headers
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > 64 * 1024 + MAX_BODY {
                    return None;
                }
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() >= 64 * 1024 {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let hdr_end = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buf[..hdr_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next()?.to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some(i) = line.find(':') {
            headers.insert(
                line[..i].trim().to_lowercase(),
                line[i + 1..].trim().to_string(),
            );
        }
    }
    let content_len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if content_len > MAX_BODY {
        return Some((method, target, headers, String::new()));
    }
    let mut body = buf[hdr_end..].to_vec();
    while body.len() < content_len {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    body.truncate(content_len);
    let body_str = String::from_utf8_lossy(&body).to_string();
    Some((method, target, headers, body_str))
}

/// Serve forever on `addr` using `dir` as the data directory.
pub fn run(addr: &str, dir: &Path) -> std::io::Result<()> {
    let token = std::env::var("SE_TOKEN").ok().filter(|s| !s.is_empty());
    let server = Arc::new(Server {
        index: Arc::new(Index::open(dir)?),
        token,
    });
    let listener = TcpListener::bind(addr)?;
    eprintln!("se-server listening on {addr}");
    for stream in listener.incoming() {
        match stream {
            Ok(mut s) => {
                let srv = Arc::clone(&server);
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(30)));
                    match read_request(&mut s) {
                        Some((method, target, headers, body)) => {
                            let cl: usize = headers
                                .get("content-length")
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(0);
                            if cl > MAX_BODY {
                                err(&mut s, 413, "request body too large (max 8 MiB)");
                                log(&method, &target, 413, 0);
                            } else {
                                srv.handle(&mut s, &method, &target, &headers, &body);
                            }
                        }
                        None => {
                            err(&mut s, 400, "could not parse HTTP request");
                        }
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_basics() {
        let j = parse_json("{\"a\": 1, \"b\": [true, null, \"x\\n\"]}").unwrap();
        assert_eq!(j.get("a").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(j.get("missing"), None);
    }

    #[test]
    fn json_rejects_trailing() {
        assert!(parse_json("{} {}").is_err());
    }

    #[test]
    fn url_decoding() {
        assert_eq!(decode_url("a+b%20c"), "a b c");
    }
}
