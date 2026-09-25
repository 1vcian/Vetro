//! Decodificatori del corpo: JSON, `application/x-www-form-urlencoded`,
//! `multipart/*` e protobuf senza schema (formato sul filo, come
//! `protoc --decode_raw`), scelti dal `Content-Type` e, se manca o è
//! generico, dal contenuto.

use std::fmt::Write as _;

use super::json;

/// Un corpo decodificato.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decoded {
    Empty,
    Json(json::Value),
    Form(Vec<(String, String)>),
    Multipart(Vec<Part>),
    Protobuf(Vec<Field>),
    /// Testo UTF-8 non strutturato.
    Text(String),
    /// Byte non riconosciuti (con il motivo, se un decodificatore scelto
    /// dal `Content-Type` ha fallito).
    Binary {
        len: usize,
        note: Option<String>,
    },
}

/// Una parte di un corpo multipart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub headers: Vec<(String, String)>,
    /// `name` di `Content-Disposition`.
    pub name: Option<String>,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
    /// Il contenuto della parte, decodificato a sua volta.
    pub decoded: Box<Decoded>,
}

/// Un campo protobuf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub number: u64,
    pub value: Wire,
}

/// Valore di un campo secondo il tipo sul filo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wire {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    /// Lunghezza delimitata che si decodifica come messaggio.
    Message(Vec<Field>),
    /// Lunghezza delimitata in UTF-8 (e non un messaggio).
    String(String),
    Bytes(Vec<u8>),
    /// Gruppo (tipi 3/4, deprecati).
    Group(Vec<Field>),
}

/// Tipo di media del `Content-Type`, in minuscolo e senza parametri.
pub fn media_type(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

/// Parametro `name` del `Content-Type` (virgolette tolte).
pub fn param(content_type: &str, name: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().trim_matches('"').to_string())
    })
}

fn is_json_type(m: &str) -> bool {
    m == "application/json" || m == "text/json" || m.ends_with("+json")
}

fn is_protobuf_type(m: &str) -> bool {
    matches!(
        m,
        "application/x-protobuf"
            | "application/protobuf"
            | "application/x-google-protobuf"
            | "application/vnd.google.protobuf"
            | "application/octet-stream+protobuf"
    ) || m.ends_with("+proto")
        || m.ends_with("+protobuf")
}

fn is_grpc_type(m: &str) -> bool {
    m == "application/grpc" || m.starts_with("application/grpc+proto") || m == "application/grpc-web+proto"
}

/// Decodifica un corpo secondo il `Content-Type` (se c'è).
pub fn decode(content_type: Option<&str>, data: &[u8]) -> Decoded {
    decode_depth(content_type, data, 0)
}

fn decode_depth(content_type: Option<&str>, data: &[u8], depth: usize) -> Decoded {
    if data.is_empty() {
        return Decoded::Empty;
    }
    let ct = content_type.unwrap_or("");
    let m = media_type(ct);
    let fail =
        |what: &str, e: String| Decoded::Binary { len: data.len(), note: Some(format!("{what}: {e}")) };
    if is_json_type(&m) {
        return match json::parse(data) {
            Ok(v) => Decoded::Json(v),
            Err(e) => match std::str::from_utf8(data) {
                Ok(t) => Decoded::Text(t.to_string()),
                Err(_) => fail("JSON", e.to_string()),
            },
        };
    }
    if m == "application/x-www-form-urlencoded" {
        return match std::str::from_utf8(data) {
            Ok(t) => Decoded::Form(form(t)),
            Err(_) => fail("form", "non è testo".into()),
        };
    }
    if m.starts_with("multipart/") && depth < 4 {
        return match param(ct, "boundary") {
            Some(b) => match multipart(data, &b, depth) {
                Some(p) => Decoded::Multipart(p),
                None => fail("multipart", "delimitatore non trovato".into()),
            },
            None => fail("multipart", "manca boundary".into()),
        };
    }
    if is_grpc_type(&m) {
        return match grpc(data) {
            Some(f) => Decoded::Protobuf(f),
            None => fail("gRPC", "cornici non valide".into()),
        };
    }
    if is_protobuf_type(&m) {
        return match protobuf(data) {
            Some(f) => Decoded::Protobuf(f),
            None => fail("protobuf", "formato sul filo non valido".into()),
        };
    }
    // Tipo generico o assente: dal contenuto.
    if let Ok(t) = std::str::from_utf8(data) {
        let trimmed = t.trim_start();
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && let Ok(v) = json::parse(data)
        {
            return Decoded::Json(v);
        }
        if t.chars().all(|c| !c.is_control() || c.is_ascii_whitespace()) {
            return Decoded::Text(t.to_string());
        }
    }
    Decoded::Binary { len: data.len(), note: None }
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Percent-decoding con `+` come spazio (lossy su UTF-8 non valido).
pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => match (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Coppie di un form urlencoded (o di una query string).
pub fn form(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (url_decode(k), url_decode(v)),
            None => (url_decode(p), String::new()),
        })
        .collect()
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn disposition_param(v: &str, name: &str) -> Option<String> {
    v.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().trim_matches('"').to_string())
    })
}

fn multipart(data: &[u8], boundary: &str, depth: usize) -> Option<Vec<Part>> {
    let delim = format!("--{boundary}").into_bytes();
    let mut pos = find(data, &delim, 0)? + delim.len();
    let mut parts = Vec::new();
    loop {
        if data[pos..].starts_with(b"--") {
            return Some(parts);
        }
        // Fine della riga del delimitatore.
        pos = find(data, b"\r\n", pos)? + 2;
        let head_end = find(data, b"\r\n\r\n", pos).filter(|&e| e >= pos);
        let (headers, body_start) = match head_end {
            Some(e) if !data[pos..].starts_with(b"\r\n") => {
                let text = String::from_utf8_lossy(&data[pos..e]);
                let h = text
                    .split("\r\n")
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect::<Vec<_>>();
                (h, e + 4)
            }
            _ => (Vec::new(), pos + 2),
        };
        let mut next_delim = b"\r\n".to_vec();
        next_delim.extend(&delim);
        let (end, after) = match find(data, &next_delim, body_start) {
            Some(e) => (e, e + next_delim.len()),
            None => (data.len(), data.len()),
        };
        let get = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
        let disp = get("content-disposition");
        let content_type = get("content-type");
        let body = data.get(body_start..end).unwrap_or_default().to_vec();
        parts.push(Part {
            name: disp.as_deref().and_then(|d| disposition_param(d, "name")),
            filename: disp.as_deref().and_then(|d| disposition_param(d, "filename")),
            decoded: Box::new(decode_depth(content_type.as_deref(), &body, depth + 1)),
            content_type,
            headers,
            data: body,
        });
        if after >= data.len() {
            // Senza delimitatore finale: parti fino alla fine.
            return Some(parts);
        }
        pos = after;
    }
}

fn varint(d: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let b = *d.get(*pos)?;
        *pos += 1;
        if i == 9 && b > 1 {
            return None;
        }
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

const MAX_PROTO_DEPTH: usize = 32;

/// Campi fino alla fine di `d` (o a un end-group se `group` non è `None`).
fn fields(d: &[u8], pos: &mut usize, depth: usize, group: Option<u64>) -> Option<Vec<Field>> {
    let mut out = Vec::new();
    while *pos < d.len() {
        let key = varint(d, pos)?;
        let (number, wt) = (key >> 3, key & 7);
        if number == 0 || number > (1 << 29) - 1 {
            return None;
        }
        let value = match wt {
            0 => Wire::Varint(varint(d, pos)?),
            1 => {
                let b = d.get(*pos..*pos + 8)?;
                *pos += 8;
                Wire::Fixed64(u64::from_le_bytes(b.try_into().ok()?))
            }
            5 => {
                let b = d.get(*pos..*pos + 4)?;
                *pos += 4;
                Wire::Fixed32(u32::from_le_bytes(b.try_into().ok()?))
            }
            2 => {
                let len = usize::try_from(varint(d, pos)?).ok()?;
                let b = d.get(*pos..pos.checked_add(len)?)?;
                *pos += len;
                length_delimited(b, depth)
            }
            3 => {
                if depth >= MAX_PROTO_DEPTH {
                    return None;
                }
                Wire::Group(fields(d, pos, depth + 1, Some(number))?)
            }
            4 => {
                return (group == Some(number)).then_some(out);
            }
            _ => return None,
        };
        out.push(Field { number, value });
    }
    group.is_none().then_some(out)
}

fn length_delimited(b: &[u8], depth: usize) -> Wire {
    if !b.is_empty() && depth < MAX_PROTO_DEPTH {
        let mut p = 0;
        if let Some(f) = fields(b, &mut p, depth + 1, None) {
            // Un testo stampabile che per caso è anche un messaggio valido
            // resta testo (come le euristiche degli strumenti usuali).
            let printable = std::str::from_utf8(b).is_ok_and(|t| t.chars().all(|c| !c.is_control()));
            if !printable {
                return Wire::Message(f);
            }
        }
    }
    match std::str::from_utf8(b) {
        Ok(t) => Wire::String(t.to_string()),
        Err(_) => Wire::Bytes(b.to_vec()),
    }
}

/// Decodifica protobuf senza schema; `None` se i byte non sono un
/// messaggio valido sul filo.
pub fn protobuf(d: &[u8]) -> Option<Vec<Field>> {
    let mut pos = 0;
    fields(d, &mut pos, 0, None)
}

/// Messaggi gRPC (cornici da 5 byte: compresso, lunghezza), non
/// compressi, decodificati come protobuf; i campi di ogni messaggio sotto
/// un campo sintetico con il numero del messaggio (da 1).
fn grpc(d: &[u8]) -> Option<Vec<Field>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < d.len() {
        let h = d.get(pos..pos + 5)?;
        if h[0] != 0 {
            return None;
        }
        let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let m = d.get(pos + 5..pos + 5 + len)?;
        out.push(Field { number: out.len() as u64 + 1, value: Wire::Message(protobuf(m)?) });
        pos += 5 + len;
    }
    Some(out)
}

/// Resa testuale dei campi, come `protoc --decode_raw`.
pub fn protobuf_text(fields: &[Field]) -> String {
    let mut s = String::new();
    proto_into(&mut s, fields, 0);
    s
}

fn proto_into(s: &mut String, fields: &[Field], depth: usize) {
    let pad = "  ".repeat(depth);
    for f in fields {
        match &f.value {
            Wire::Varint(v) => {
                let _ = writeln!(s, "{pad}{}: {v}", f.number);
            }
            Wire::Fixed64(v) => {
                let _ = writeln!(s, "{pad}{}: 0x{v:016x}", f.number);
            }
            Wire::Fixed32(v) => {
                let _ = writeln!(s, "{pad}{}: 0x{v:08x}", f.number);
            }
            Wire::String(t) => {
                let _ = writeln!(s, "{pad}{}: {}", f.number, json::quote(t));
            }
            Wire::Bytes(b) => {
                let mut e = String::new();
                for &c in b {
                    let _ = write!(e, "\\{c:03o}");
                }
                let _ = writeln!(s, "{pad}{}: \"{e}\"", f.number);
            }
            Wire::Message(m) | Wire::Group(m) => {
                let _ = writeln!(s, "{pad}{} {{", f.number);
                proto_into(s, m, depth + 1);
                let _ = writeln!(s, "{pad}}}");
            }
        }
    }
}

impl Decoded {
    /// Nome breve del decodificatore usato.
    pub fn kind(&self) -> &'static str {
        match self {
            Decoded::Empty => "vuoto",
            Decoded::Json(_) => "json",
            Decoded::Form(_) => "form",
            Decoded::Multipart(_) => "multipart",
            Decoded::Protobuf(_) => "protobuf",
            Decoded::Text(_) => "testo",
            Decoded::Binary { .. } => "binario",
        }
    }

    /// Resa testuale leggibile (per l'ispettore e i commenti dell'HAR).
    pub fn to_text(&self) -> String {
        match self {
            Decoded::Empty => String::new(),
            Decoded::Json(v) => v.to_pretty(),
            Decoded::Form(p) => p.iter().map(|(k, v)| format!("{k} = {v}\n")).collect(),
            Decoded::Multipart(parts) => {
                let mut s = String::new();
                for (i, p) in parts.iter().enumerate() {
                    let _ = writeln!(
                        s,
                        "--- parte {} name={} filename={} type={} ({} byte)",
                        i + 1,
                        p.name.as_deref().unwrap_or("-"),
                        p.filename.as_deref().unwrap_or("-"),
                        p.content_type.as_deref().unwrap_or("-"),
                        p.data.len()
                    );
                    let t = p.decoded.to_text();
                    s.push_str(&t);
                    if !t.ends_with('\n') && !t.is_empty() {
                        s.push('\n');
                    }
                }
                s
            }
            Decoded::Protobuf(f) => protobuf_text(f),
            Decoded::Text(t) => t.clone(),
            Decoded::Binary { len, note } => match note {
                Some(n) => format!("({len} byte non decodificati: {n})"),
                None => format!("({len} byte binari)"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_dal_tipo_e_dal_contenuto() {
        let d = decode(Some("application/json; charset=utf-8"), br#"{"id":7,"ok":true}"#);
        let Decoded::Json(v) = &d else { panic!("{d:?}") };
        assert_eq!(v.get("id"), Some(&json::Value::Number("7".into())));
        assert!(matches!(decode(Some("application/vnd.api+json"), b"[1]"), Decoded::Json(_)));
        assert!(matches!(decode(None, b"  {\"a\":1}\n"), Decoded::Json(_)));
        assert!(matches!(decode(Some("text/plain"), b"{non json"), Decoded::Text(_)));
        assert_eq!(decode(Some("application/json"), b"{rotto"), Decoded::Text("{rotto".into()));
        assert_eq!(decode(None, b""), Decoded::Empty);
    }

    #[test]
    fn form_urlencoded() {
        let d = decode(
            Some("application/x-www-form-urlencoded"),
            b"nome=Mario+Rossi&citt%C3%A0=Mil%61no&vuoto=&solo&x=%zz%",
        );
        assert_eq!(
            d,
            Decoded::Form(vec![
                ("nome".into(), "Mario Rossi".into()),
                ("città".into(), "Milano".into()),
                ("vuoto".into(), "".into()),
                ("solo".into(), "".into()),
                ("x".into(), "%zz%".into()),
            ])
        );
        assert!(d.to_text().contains("città = Milano\n"));
    }

    #[test]
    fn multipart_con_file_e_json() {
        let body = b"preambolo\r\n--XyZ\r\nContent-Disposition: form-data; name=\"campo\"\r\n\r\nvalore\r\n--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.json\"\r\nContent-Type: application/json\r\n\r\n{\"k\":[1,2]}\r\n--XyZ\r\nContent-Disposition: form-data; name=\"bin\"\r\nContent-Type: application/octet-stream\r\n\r\n\x00\x01\xff\r\n--XyZ--\r\n";
        let d = decode(Some("multipart/form-data; boundary=\"XyZ\""), body);
        let Decoded::Multipart(p) = &d else { panic!("{d:?}") };
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].name.as_deref(), p[0].data.as_slice()), (Some("campo"), &b"valore"[..]));
        assert_eq!(*p[0].decoded, Decoded::Text("valore".into()));
        assert_eq!(
            (p[1].filename.as_deref(), p[1].content_type.as_deref()),
            (Some("a.json"), Some("application/json"))
        );
        assert!(matches!(*p[1].decoded, Decoded::Json(_)));
        assert_eq!(p[2].data, [0, 1, 0xff]);
        assert!(d.to_text().contains("--- parte 2 name=file filename=a.json"));
        assert!(matches!(decode(Some("multipart/form-data"), body), Decoded::Binary { note: Some(_), .. }));
    }

    #[test]
    fn protobuf_senza_schema() {
        // Esempi della documentazione del formato: 150 nel campo 1, "testing"
        // nel campo 2, un messaggio annidato nel campo 3.
        let d = [
            0x08, 0x96, 0x01, 0x12, 0x07, b't', b'e', b's', b't', b'i', b'n', b'g', 0x1a, 0x03, 0x08, 0x96,
            0x01,
        ];
        let f = protobuf(&d).unwrap();
        assert_eq!(
            f,
            [
                Field { number: 1, value: Wire::Varint(150) },
                Field { number: 2, value: Wire::String("testing".into()) },
                Field {
                    number: 3,
                    value: Wire::Message(vec![Field { number: 1, value: Wire::Varint(150) }])
                },
            ]
        );
        assert_eq!(protobuf_text(&f), "1: 150\n2: \"testing\"\n3 {\n  1: 150\n}\n");
        let mut x = vec![0x21];
        x.extend(1.5f64.to_le_bytes());
        x.extend([0x2d, 1, 0, 0, 0, 0x32, 2, 0xff, 0xfe, 0x3b, 0x08, 0x01, 0x3c]);
        let f = protobuf(&x).unwrap();
        assert_eq!(f[0].value, Wire::Fixed64(1.5f64.to_bits()));
        assert_eq!(f[1].value, Wire::Fixed32(1));
        assert_eq!(f[2].value, Wire::Bytes(vec![0xff, 0xfe]));
        assert_eq!(f[3].value, Wire::Group(vec![Field { number: 1, value: Wire::Varint(1) }]));
        assert!(matches!(decode(Some("application/x-protobuf"), &d), Decoded::Protobuf(_)));
    }

    #[test]
    fn protobuf_non_validi_e_grpc() {
        for bad in
            [&[0x08][..], &[0x00, 0x01], &[0x12, 0x05, 1], &[0x0f], &[0x0c], &[0xff; 11], &[0x1b, 0x08, 0x01]]
        {
            assert_eq!(protobuf(bad), None, "{bad:?}");
        }
        assert!(matches!(
            decode(Some("application/x-protobuf"), &[0x0f]),
            Decoded::Binary { note: Some(_), .. }
        ));
        let mut g = vec![0, 0, 0, 0, 3, 0x08, 0x96, 0x01];
        g.extend([0, 0, 0, 0, 2, 0x10, 0x01]);
        let Decoded::Protobuf(f) = decode(Some("application/grpc"), &g) else { panic!() };
        assert_eq!(protobuf_text(&f), "1 {\n  1: 150\n}\n2 {\n  2: 1\n}\n");
    }

    #[test]
    fn testo_e_binario() {
        assert_eq!(decode(Some("text/html"), b"<p>ciao</p>"), Decoded::Text("<p>ciao</p>".into()));
        assert_eq!(decode(None, &[0, 1, 2, 0xff]), Decoded::Binary { len: 4, note: None });
        assert_eq!(param("multipart/mixed; charset=x; BOUNDARY=abc", "boundary").as_deref(), Some("abc"));
    }
}
