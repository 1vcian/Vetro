//! Timeline input→effetti (M7, ADR 0023): gli ingressi dell'utente
//! (tasti, clic e tocchi, righe della console, comandi del gestore dei
//! file) con il loro numero d'istruzione, e gli effetti che seguono
//! (richieste di rete, scritture di file viste dall'osservazione, uscita
//! della console), tutti nel tempo del guest in microsecondi (10 ns a
//! istruzione: `at_us = istruzioni / 100`, lo stesso tempo dei frame di
//! [`crate::net::capture`]).
//!
//! **Attribuzione (euristica).** Un effetto è attribuito all'ultimo
//! ingresso che lo precede (allo stesso istante vale: l'ingresso arriva al
//! guest prima della prossima istruzione) se è entro `window_us`; altrimenti
//! resta senza causa. Per gli effetti di rete e sui file contano solo gli
//! ingressi *di comando* (Invio, clic, tocco, comando del gestore, tasto di
//! accensione): un carattere battuto a metà riga non "causa" una richiesta.
//! Per l'uscita della console conta qualunque ingresso (l'eco di un tasto è
//! l'effetto di quel tasto). Non è una relazione causale vera (il guest
//! può fare richieste per conto suo durante la finestra, per esempio un
//! rinnovo DHCP): è la stessa approssimazione degli strumenti che mettono
//! in fila azioni e traffico, e la finestra si sceglie.
//!
//! Senza dipendenze e deterministico, come il resto del crate (compila per
//! wasm32).

use std::fmt::Write as _;

use crate::net::dns;
use crate::net::inspector::NetworkAnalysis;
use crate::net::json::quote_into;

/// Finestra di attribuzione predefinita: 3 s di tempo del guest.
pub const DEFAULT_WINDOW_US: u64 = 3_000_000;

/// Ingressi e effetti tenuti al più (i più vecchi si scartano).
pub const MAX_INPUTS: usize = 20_000;
pub const MAX_EFFECTS: usize = 50_000;

/// Caratteri di testo tenuti per un effetto della console.
pub const CONSOLE_TEXT: usize = 160;

/// Microsecondi di tempo del guest a un numero d'istruzione.
pub fn step_us(step: u64) -> u64 {
    step / 100
}

/// Tipo di ingresso dell'utente.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InputKind {
    /// Tasto della tastiera (virtio-input).
    Key,
    /// Pulsante del puntatore (tablet).
    Pointer,
    /// Contatto del touchscreen.
    Touch,
    /// Byte alla console seriale.
    Console,
    /// Comando del gestore dei file (dall'utente, non gli aggiornamenti del
    /// pannello).
    Files,
    /// Tasto di accensione (GPIO).
    Power,
    /// Risoluzione chiesta per lo schermo.
    Display,
    Other,
}

impl InputKind {
    pub const ALL: [InputKind; 8] = [
        InputKind::Key,
        InputKind::Pointer,
        InputKind::Touch,
        InputKind::Console,
        InputKind::Files,
        InputKind::Power,
        InputKind::Display,
        InputKind::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            InputKind::Key => "tasto",
            InputKind::Pointer => "puntatore",
            InputKind::Touch => "tocco",
            InputKind::Console => "console",
            InputKind::Files => "file",
            InputKind::Power => "accensione",
            InputKind::Display => "schermo",
            InputKind::Other => "altro",
        }
    }

    /// Codice numerico (per l'API C di vetro-wasm): la posizione in
    /// [`InputKind::ALL`].
    pub fn code(self) -> u32 {
        Self::ALL.iter().position(|&k| k == self).unwrap_or(7) as u32
    }

    pub fn from_code(c: u32) -> InputKind {
        Self::ALL.get(c as usize).copied().unwrap_or(InputKind::Other)
    }
}

/// Un ingresso dell'utente.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInput {
    /// Numero d'istruzione a cui è arrivato al guest.
    pub step: u64,
    pub at_us: u64,
    pub kind: InputKind,
    pub label: String,
    /// Non è un comando (un carattere a metà riga, un cambio di
    /// risoluzione): non causa effetti di rete o sui file.
    pub weak: bool,
    /// Ordine di arrivo nella timeline (lo mette [`Timeline`]).
    pub seq: u64,
}

impl UserInput {
    pub fn new(step: u64, kind: InputKind, label: impl Into<String>, weak: bool) -> Self {
        UserInput { step, at_us: step_us(step), kind, label: label.into(), weak, seq: 0 }
    }
}

/// Tipo di effetto.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffectKind {
    /// Richiesta HTTP (dall'ispettore di rete).
    Http,
    /// Domanda DNS.
    Dns,
    /// Connessione TLS (nome dal ClientHello).
    Tls,
    /// File creato, scritto, spostato o cancellato (osservazione del
    /// gestore dei file).
    File,
    /// Uscita della console.
    Console,
}

impl EffectKind {
    pub const ALL: [EffectKind; 5] =
        [EffectKind::Http, EffectKind::Dns, EffectKind::Tls, EffectKind::File, EffectKind::Console];

    pub fn name(self) -> &'static str {
        match self {
            EffectKind::Http => "http",
            EffectKind::Dns => "dns",
            EffectKind::Tls => "tls",
            EffectKind::File => "file",
            EffectKind::Console => "console",
        }
    }

    pub fn code(self) -> u32 {
        Self::ALL.iter().position(|&k| k == self).unwrap_or(0) as u32
    }

    pub fn from_code(c: u32) -> Option<EffectKind> {
        Self::ALL.get(c as usize).copied()
    }

    /// Lo causano anche gli ingressi deboli?
    fn any_input(self) -> bool {
        self == EffectKind::Console
    }
}

/// Un effetto osservato.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effect {
    pub at_us: u64,
    pub kind: EffectKind,
    pub label: String,
    /// Riferimento nella sua vista (indice della richiesta nell'ispettore).
    pub detail: Option<usize>,
    /// Byte (uscita della console, corpo della risposta).
    pub bytes: u64,
    /// Ordine di arrivo nella timeline (lo mette [`Timeline`]): allo stesso
    /// istante, un ingresso arrivato dopo l'effetto non ne è la causa
    /// (l'uscita della console letta alla fine di un quanto viene prima
    /// degli ingressi dati a quel confine). `u64::MAX` per gli effetti
    /// calcolati a parte (rete): vengono dopo gli ingressi del loro istante.
    pub seq: u64,
}

impl Effect {
    pub fn new(at_us: u64, kind: EffectKind, label: impl Into<String>) -> Self {
        Effect { at_us, kind, label: label.into(), detail: None, bytes: 0, seq: u64::MAX }
    }
}

/// La causa di un effetto all'istante `at_us` arrivato per `seq`-esimo:
/// l'indice dell'ultimo ingresso che lo precede (istante minore, o uguale e
/// arrivato prima) ed entro `window_us` (solo quelli di comando se
/// `strong_only`). `inputs` in ordine di (istante, arrivo).
pub fn cause(inputs: &[UserInput], at_us: u64, seq: u64, window_us: u64, strong_only: bool) -> Option<usize> {
    let end = inputs.partition_point(|i| (i.at_us, i.seq) < (at_us, seq));
    inputs[..end]
        .iter()
        .enumerate()
        .rev()
        .take_while(|(_, i)| at_us - i.at_us <= window_us)
        .find(|(_, i)| !strong_only || !i.weak)
        .map(|(k, _)| k)
}

/// Il testo stampabile di byte della console (controlli come `⏎`, `⌫`,
/// `^X`; sequenze di escape tolte).
pub fn printable(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                // CSI: ESC [ parametri lettera finale; altrimenti un carattere.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for d in chars.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' | '\n' => out.push('⏎'),
            '\x7f' | '\x08' => out.push('⌫'),
            '\t' => out.push('⇥'),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push(char::from(b'@' + c as u8));
            }
            c => out.push(c),
        }
    }
    out
}

/// Ricostruisce le righe battute alla console (come la vede una shell in
/// modo canonico): caratteri aggiunti, `DEL`/`BS` tolgono l'ultimo, `^U` e
/// `^C` svuotano la riga, CR o LF la chiudono; le sequenze di escape (frecce)
/// si ignorano.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineEditor {
    line: String,
    esc: u8,
}

impl LineEditor {
    /// Aggiunge byte battuti; restituisce le righe chiuse.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut done = Vec::new();
        for c in String::from_utf8_lossy(bytes).chars() {
            match (self.esc, c) {
                (1, '[') => self.esc = 2,
                (1, _) => self.esc = 0,
                (2, c) if ('@'..='~').contains(&c) => self.esc = 0,
                (2, _) => {}
                (_, '\x1b') => self.esc = 1,
                (_, '\r' | '\n') => done.push(core::mem::take(&mut self.line)),
                (_, '\x7f' | '\x08') => {
                    self.line.pop();
                }
                (_, '\x15' | '\x03') => self.line.clear(),
                (_, c) if (c as u32) < 0x20 => {}
                (_, c) => self.line.push(c),
            }
        }
        done
    }

    /// La riga in corso.
    pub fn current(&self) -> &str {
        &self.line
    }
}

/// Nome di un tasto Linux (`KEY_*` e `BTN_*` più comuni).
pub fn key_name(code: u16) -> String {
    const ROW1: &str = "1234567890";
    const QWERTY: [(u16, &str); 3] = [(16, "QWERTYUIOP"), (30, "ASDFGHJKL"), (44, "ZXCVBNM")];
    let named = match code {
        1 => "Esc",
        12 => "-",
        13 => "=",
        14 => "Backspace",
        15 => "Tab",
        26 => "[",
        27 => "]",
        28 => "Invio",
        29 => "Ctrl",
        39 => ";",
        40 => "'",
        41 => "`",
        42 => "Maiusc",
        43 => "\\",
        51 => ",",
        52 => ".",
        53 => "/",
        54 => "Maiusc destro",
        56 => "Alt",
        57 => "Spazio",
        58 => "BlocMaiusc",
        96 => "Invio (tastierino)",
        97 => "Ctrl destro",
        100 => "AltGr",
        102 => "Home",
        103 => "Su",
        104 => "PagSu",
        105 => "Sinistra",
        106 => "Destra",
        107 => "Fine",
        108 => "Giù",
        109 => "PagGiù",
        110 => "Ins",
        111 => "Canc",
        116 => "Accensione",
        125 => "Meta",
        158 => "Indietro",
        172 => "Home page",
        0x110 => "clic sinistro",
        0x111 => "clic destro",
        0x112 => "clic centrale",
        0x14a => "tocco",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_string();
    }
    if (2..=11).contains(&code) {
        return ROW1[(code - 2) as usize..(code - 1) as usize].to_string();
    }
    for (base, row) in QWERTY {
        if code >= base && code < base + row.len() as u16 {
            let i = (code - base) as usize;
            return row[i..=i].to_string();
        }
    }
    if (59..=68).contains(&code) {
        return format!("F{}", code - 58);
    }
    format!("tasto {code}")
}

/// Il tasto Invio (`KEY_ENTER`, `KEY_KPENTER`): chiude un comando.
pub fn is_enter(code: u16) -> bool {
    code == 28 || code == 96
}

/// Gli effetti di un'analisi di rete: una richiesta HTTP all'inizio della
/// richiesta (primo byte, o SYN/DNS della prima della connessione), una
/// domanda DNS, una connessione TLS.
pub fn network_effects(a: &NetworkAnalysis) -> Vec<Effect> {
    let mut v = Vec::new();
    for x in &a.http {
        let status = x.status().map_or_else(|| "senza risposta".to_string(), |s| s.to_string());
        v.push(Effect {
            detail: Some(x.index),
            bytes: x.response.as_ref().map_or(0, |r| r.body.decoded.len() as u64),
            ..Effect::new(
                x.timings.started_us,
                EffectKind::Http,
                format!("{} {} → {status}", x.request.method, x.url),
            )
        });
    }
    for d in &a.dns {
        let addrs: Vec<String> = d.ipv4().map(|a| a.to_string()).collect();
        let answer = if addrs.is_empty() { String::new() } else { format!(" → {}", addrs.join(", ")) };
        v.push(Effect::new(
            d.query_at,
            EffectKind::Dns,
            format!("DNS {} {}{answer}", dns::type_name(d.qtype), d.name),
        ));
    }
    for t in &a.tls {
        v.push(Effect::new(
            t.started_us,
            EffectKind::Tls,
            format!("TLS {} ({})", t.sni.as_deref().unwrap_or("senza SNI"), t.server),
        ));
    }
    v
}

/// Ingressi e effetti non di rete di una sessione (quelli di rete si
/// ricavano dalla cattura quando servono: [`network_effects`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timeline {
    inputs: Vec<UserInput>,
    effects: Vec<Effect>,
    /// Ingressi ed effetti scartati perché oltre i limiti.
    pub dropped: u64,
    /// Cresce a ogni cambiamento (per chi ridisegna solo se serve).
    pub version: u64,
    /// L'ultima cosa aggiunta è l'effetto della console in fondo: l'uscita
    /// che segue ci si unisce.
    console_open: bool,
    /// Contatore degli arrivi (`seq` di ingressi ed effetti).
    seq: u64,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Inserisce `x` tenendo l'ordine per `key` (in fondo fra gli uguali).
fn insert_sorted<T>(v: &mut Vec<T>, x: T, key: impl Fn(&T) -> u64) {
    let k = key(&x);
    let at = v.partition_point(|y| key(y) <= k);
    v.insert(at, x);
}

impl Timeline {
    pub fn new() -> Self {
        Timeline {
            inputs: Vec::new(),
            effects: Vec::new(),
            dropped: 0,
            version: 0,
            console_open: false,
            seq: 0,
        }
    }

    pub fn inputs(&self) -> &[UserInput] {
        &self.inputs
    }

    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }

    pub fn clear(&mut self) {
        self.inputs.clear();
        self.effects.clear();
        self.dropped = 0;
        self.version += 1;
        self.console_open = false;
    }

    /// Aggiunge un ingresso (in ordine di tempo).
    pub fn push_input(&mut self, mut i: UserInput) {
        self.seq += 1;
        i.seq = self.seq;
        insert_sorted(&mut self.inputs, i, |i| i.at_us);
        if self.inputs.len() > MAX_INPUTS {
            self.inputs.remove(0);
            self.dropped += 1;
        }
        self.version += 1;
        self.console_open = false;
    }

    /// Aggiunge un effetto (in ordine di tempo).
    pub fn push_effect(&mut self, mut e: Effect) {
        self.seq += 1;
        e.seq = self.seq;
        insert_sorted(&mut self.effects, e, |e| e.at_us);
        if self.effects.len() > MAX_EFFECTS {
            self.effects.remove(0);
            self.dropped += 1;
        }
        self.version += 1;
        self.console_open = false;
    }

    /// Uscita della console all'istante `at_us`: si unisce all'ultimo
    /// effetto della console se nel frattempo non ci sono stati ingressi né
    /// altri effetti, altrimenti è un effetto nuovo.
    pub fn push_console(&mut self, at_us: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let text = printable(bytes);
        if let Some(e) = self.effects.last_mut()
            && self.console_open
            && e.kind == EffectKind::Console
            && e.at_us <= at_us
        {
            e.bytes += bytes.len() as u64;
            let room = CONSOLE_TEXT.saturating_sub(e.label.chars().count());
            e.label.extend(text.chars().take(room));
            self.version += 1;
            return;
        }
        self.push_effect(Effect {
            bytes: bytes.len() as u64,
            ..Effect::new(at_us, EffectKind::Console, text.chars().take(CONSOLE_TEXT).collect::<String>())
        });
        self.console_open = true;
    }

    /// Tutti gli effetti (questi più `extra`, per esempio quelli di rete) in
    /// ordine di tempo, con la causa di ognuno.
    pub fn attributed(&self, extra: &[Effect], window_us: u64) -> Vec<(Effect, Option<usize>)> {
        let mut all: Vec<Effect> = self.effects.iter().chain(extra).cloned().collect();
        all.sort_by_key(|e| (e.at_us, e.seq, e.kind));
        all.into_iter()
            .map(|e| {
                let c = cause(&self.inputs, e.at_us, e.seq, window_us, !e.kind.any_input());
                (e, c)
            })
            .collect()
    }

    /// La timeline in JSON per l'app web:
    ///
    /// ```json
    /// {"windowUs":3000000,"dropped":0,"version":7,
    ///  "inputs":[{"i":0,"step":123400,"atUs":1234,"kind":"console",
    ///             "label":"Invio: wget ...","weak":false,"effects":3}],
    ///  "effects":[{"atUs":1300,"kind":"http","label":"POST http://... → 200",
    ///              "ref":0,"bytes":0,"cause":0}]}
    /// ```
    pub fn to_json(&self, extra: &[Effect], window_us: u64) -> String {
        let effects = self.attributed(extra, window_us);
        let mut count = vec![0u64; self.inputs.len()];
        for (_, c) in &effects {
            if let Some(c) = c {
                count[*c] += 1;
            }
        }
        let mut out = String::new();
        let _ = write!(
            out,
            "{{\"windowUs\":{window_us},\"dropped\":{},\"version\":{},\"inputs\":[",
            self.dropped, self.version
        );
        for (k, i) in self.inputs.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"i\":{k},\"step\":{},\"atUs\":{},\"kind\":\"{}\",\"label\":",
                i.step,
                i.at_us,
                i.kind.name()
            );
            quote_into(&mut out, &i.label);
            let _ = write!(out, ",\"weak\":{},\"effects\":{}}}", i.weak, count[k]);
        }
        out.push_str("],\"effects\":[");
        for (k, (e, c)) in effects.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            let _ = write!(out, "{{\"atUs\":{},\"kind\":\"{}\",\"label\":", e.at_us, e.kind.name());
            quote_into(&mut out, &e.label);
            out.push_str(",\"ref\":");
            match e.detail {
                Some(d) => {
                    let _ = write!(out, "{d}");
                }
                None => out.push_str("null"),
            }
            let _ = write!(out, ",\"bytes\":{},\"cause\":", e.bytes);
            match c {
                Some(c) => {
                    let _ = write!(out, "{c}");
                }
                None => out.push_str("null"),
            }
            out.push('}');
        }
        out.push_str("]}");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::inspector::tests::session;
    use crate::net::json::{self, Value};

    fn input(step: u64, label: &str, weak: bool) -> UserInput {
        UserInput::new(step, InputKind::Console, label, weak)
    }

    fn effect(at_us: u64, kind: EffectKind) -> Effect {
        Effect::new(at_us, kind, "")
    }

    /// L'ultimo ingresso non oltre l'effetto, entro la finestra; allo
    /// stesso istante l'ingresso viene prima; i deboli solo per la console.
    #[test]
    fn causa_entro_la_finestra() {
        let mut v = [input(100_000, "a", false), input(200_000, "b", true), input(300_000, "c", false)];
        for (k, i) in v.iter_mut().enumerate() {
            i.seq = k as u64 + 1;
        }
        assert_eq!((v[0].at_us, v[2].at_us), (1_000, 3_000));
        let m = u64::MAX;
        assert_eq!(cause(&v, 999, m, 10_000, false), None, "prima di ogni ingresso");
        assert_eq!(cause(&v, 1_000, m, 10_000, false), Some(0), "stesso istante, calcolato a parte");
        assert_eq!(cause(&v, 1_000, 1, 10_000, false), None, "stesso istante, arrivato prima dell'ingresso");
        assert_eq!(cause(&v, 1_000, 2, 10_000, false), Some(0), "stesso istante, arrivato dopo");
        assert_eq!(cause(&v, 2_500, m, 10_000, false), Some(1));
        assert_eq!(cause(&v, 2_500, m, 10_000, true), Some(0), "il debole non causa");
        assert_eq!(cause(&v, 2_500, m, 1_000, true), None, "il comando è fuori finestra");
        assert_eq!(cause(&v, 3_000 + 10_000, m, 10_000, true), Some(2), "sul bordo");
        assert_eq!(cause(&v, 3_000 + 10_001, m, 10_000, true), None);
        assert_eq!(cause(&[], 5, m, 10, false), None);
    }

    /// L'uscita della console si unisce finché non arriva un ingresso o
    /// un altro effetto; gli effetti di rete sono attribuiti al comando.
    #[test]
    fn console_unita_e_attribuzione() {
        let mut t = Timeline::new();
        t.push_console(10, b"login\r\n");
        t.push_console(20, b"# ");
        assert_eq!(t.effects().len(), 1);
        assert_eq!(t.effects()[0].label, "login⏎# ");
        assert_eq!(t.effects()[0].bytes, 9);
        t.push_input(input(3_000, "w", true));
        t.push_console(31, b"w");
        t.push_input(input(4_000, "Invio: wget x", false));
        t.push_console(41, b"\r\n");
        t.push_effect(Effect { label: "file".into(), ..effect(45, EffectKind::File) });
        t.push_console(50, b"fatto");
        // Uscita letta a 60 prima di un ingresso dato a 60: non è sua (si
        // unisce a quella di prima), e l'uscita dopo l'ingresso è nuova.
        t.push_console(60, b"prima");
        t.push_input(input(6_000, "dopo", false));
        t.push_console(60, b"eco");
        assert_eq!(t.effects().len(), 6, "{:?}", t.effects());
        let net = [Effect { detail: Some(0), ..effect(42, EffectKind::Http) }, effect(35, EffectKind::Dns)];
        let a = t.attributed(&net, 1_000);
        let got: Vec<(u64, &str, Option<usize>)> =
            a.iter().map(|(e, c)| (e.at_us, e.kind.name(), *c)).collect();
        assert_eq!(
            got,
            [
                (10, "console", None),
                (31, "console", Some(0)),
                (35, "dns", None),
                (41, "console", Some(1)),
                (42, "http", Some(1)),
                (45, "file", Some(1)),
                (50, "console", Some(1)),
                (60, "console", Some(2)),
            ]
        );
        let j = json::parse(t.to_json(&net, 1_000).as_bytes()).expect("JSON valido");
        let Some(Value::Array(inputs)) = j.get("inputs") else { panic!() };
        assert_eq!(inputs[1].get("effects"), Some(&Value::Number("4".into())));
        assert_eq!(inputs[1].get("label").and_then(Value::as_str), Some("Invio: wget x"));
        assert_eq!(inputs[0].get("weak"), Some(&Value::Bool(true)));
        let Some(Value::Array(effects)) = j.get("effects") else { panic!() };
        assert_eq!(effects[4].get("ref"), Some(&Value::Number("0".into())));
        assert_eq!(effects[4].get("cause"), Some(&Value::Number("1".into())));
        assert_eq!(effects[0].get("cause"), Some(&Value::Null));
        let v = t.version;
        t.clear();
        assert!(t.version > v && t.inputs().is_empty() && t.effects().is_empty());
    }

    /// Ingressi fuori ordine si mettono al loro posto; oltre i limiti si
    /// scartano i più vecchi.
    #[test]
    fn ordine_e_limiti() {
        let mut t = Timeline::new();
        t.push_input(input(500, "b", false));
        t.push_input(input(100, "a", false));
        assert_eq!(t.inputs()[0].label, "a");
        for k in 0..MAX_INPUTS as u64 {
            t.push_input(input(1_000 + k, "x", false));
        }
        assert_eq!(t.inputs().len(), MAX_INPUTS);
        assert_eq!(t.dropped, 2);
        assert_eq!(t.inputs()[0].step, 1_000);
    }

    #[test]
    fn righe_della_console() {
        let mut l = LineEditor::default();
        assert!(l.feed(b"wgex\x7ft").is_empty());
        assert_eq!(l.current(), "wget");
        assert_eq!(l.feed(b" -q\x1b[D\x1b[C http://a\r"), ["wget -q http://a"]);
        assert_eq!(l.feed(b"ls\x15pwd\nuno\rdue"), ["pwd", "uno"]);
        assert_eq!(l.current(), "due");
        assert!(l.feed(b"\x03").is_empty());
        assert_eq!(l.current(), "");
        assert_eq!(l.feed("città\r".as_bytes()), ["città"]);
    }

    #[test]
    fn testo_stampabile_e_tasti() {
        assert_eq!(printable(b"a\x1b[6nb\r\nc\x7f\x01\t"), "ab⏎c⌫^A⇥");
        assert_eq!(key_name(30), "A");
        assert_eq!(key_name(2), "1");
        assert_eq!(key_name(11), "0");
        assert_eq!(key_name(28), "Invio");
        assert_eq!(key_name(50), "M");
        assert_eq!(key_name(60), "F2");
        assert_eq!(key_name(0x110), "clic sinistro");
        assert_eq!(key_name(999), "tasto 999");
        assert!(is_enter(28) && is_enter(96) && !is_enter(30));
        for k in InputKind::ALL {
            assert_eq!(InputKind::from_code(k.code()), k);
        }
        assert_eq!(InputKind::from_code(99), InputKind::Other);
        for k in EffectKind::ALL {
            assert_eq!(EffectKind::from_code(k.code()), Some(k));
        }
    }

    /// Gli effetti di rete dell'analisi di una sessione: la domanda DNS e
    /// le due richieste, con l'indice nell'ispettore.
    #[test]
    fn effetti_dall_analisi_di_rete() {
        let a = NetworkAnalysis::from_frames(&session());
        let e = network_effects(&a);
        let labels: Vec<&str> = e.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "GET http://api.example:8080/v1/items?id=7&q=a+b → 200",
                "POST http://api.example:8080/v1/form → 204",
                "DNS A api.example → 198.18.0.1",
            ]
        );
        assert_eq!((e[0].at_us, e[0].detail, e[0].bytes), (1_000, Some(0), 13));
        assert_eq!((e[1].at_us, e[1].detail), (4_000, Some(1)));
        assert_eq!(e[2].at_us, 1_000);
    }
}
