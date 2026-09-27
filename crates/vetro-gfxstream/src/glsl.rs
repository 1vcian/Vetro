//! A GLSL ES declaration scanner: what a program exposes (uniforms, vertex
//! attributes, uniform blocks) computed from the shader text, so that the
//! guest's introspection queries never depend on the host GPU's compiler
//! (ADR 0037, determinism). Every declared uniform counts as active, which
//! the GLSL ES spec allows ("the uniform will be considered active" when the
//! compiler cannot tell), in declaration order, vertex shader first.
//!
//! It handles comments, the preprocessor subset shaders use (object-like
//! `#define`, `#undef`, `#if/#ifdef/#ifndef/#elif/#else/#endif` with
//! `defined`, integer arithmetic and comparisons), `const int` array sizes,
//! `layout(location = N)`, structs (flattened to `s.f`, `a[1].f`), arrays and
//! uniform blocks. Function bodies are skipped.

use std::collections::BTreeMap;

/// GL type enums of GLSL types (glGetActiveUniform/Attrib `type`).
pub fn type_enum(name: &str) -> Option<u32> {
    Some(match name {
        "float" => 0x1406,
        "vec2" => 0x8B50,
        "vec3" => 0x8B51,
        "vec4" => 0x8B52,
        "int" => 0x1404,
        "ivec2" => 0x8B53,
        "ivec3" => 0x8B54,
        "ivec4" => 0x8B55,
        "uint" => 0x1405,
        "uvec2" => 0x8DC6,
        "uvec3" => 0x8DC7,
        "uvec4" => 0x8DC8,
        "bool" => 0x8B56,
        "bvec2" => 0x8B57,
        "bvec3" => 0x8B58,
        "bvec4" => 0x8B59,
        "mat2" | "mat2x2" => 0x8B5A,
        "mat3" | "mat3x3" => 0x8B5B,
        "mat4" | "mat4x4" => 0x8B5C,
        "mat2x3" => 0x8B65,
        "mat2x4" => 0x8B66,
        "mat3x2" => 0x8B67,
        "mat3x4" => 0x8B68,
        "mat4x2" => 0x8B69,
        "mat4x3" => 0x8B6A,
        "sampler2D" => 0x8B5E,
        "sampler3D" => 0x8B5F,
        "samplerCube" => 0x8B60,
        "sampler2DShadow" => 0x8B62,
        "sampler2DArray" => 0x8DC1,
        "sampler2DArrayShadow" => 0x8DC4,
        "samplerCubeShadow" => 0x8DC5,
        "isampler2D" => 0x8DCA,
        "isampler3D" => 0x8DCB,
        "isamplerCube" => 0x8DCC,
        "isampler2DArray" => 0x8DCF,
        "usampler2D" => 0x8DD2,
        "usampler3D" => 0x8DD3,
        "usamplerCube" => 0x8DD4,
        "usampler2DArray" => 0x8DD7,
        "samplerExternalOES" => 0x8D66,
        _ => return None,
    })
}

/// Vertex attribute locations a type takes (matrix columns).
pub fn type_slots(ty: u32) -> u32 {
    match ty {
        0x8B5A | 0x8B65 | 0x8B66 => 2,
        0x8B5B | 0x8B67 | 0x8B68 => 3,
        0x8B5C | 0x8B69 | 0x8B6A => 4,
        _ => 1,
    }
}

pub fn is_sampler(ty: u32) -> bool {
    matches!(ty, 0x8B5E..=0x8B62 | 0x8DC1 | 0x8DC4 | 0x8DC5 | 0x8DCA..=0x8DCF | 0x8DD2..=0x8DD7 | 0x8D66)
}

/// A uniform or attribute: `size` > 1 for arrays (the name has no `[0]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Var {
    pub name: String,
    pub ty: u32,
    pub size: u32,
    pub array: bool,
    /// `layout(location = N)` (attributes).
    pub location: Option<u32>,
}

/// A uniform block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub name: String,
    /// Instances (`uniform B { … } b[2];`).
    pub size: u32,
    pub members: Vec<Var>,
    pub binding: Option<u32>,
}

/// What a shader declares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shader {
    pub version: u32,
    pub uniforms: Vec<Var>,
    /// Vertex inputs (`attribute`, or `in` in a vertex shader).
    pub inputs: Vec<Var>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Id(String),
    Num(i64),
    P(char),
}

/// Removes comments, keeping line breaks.
fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                if b[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i += 2;
            out.push(' ');
        } else {
            let c = src[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn lex(line: &str) -> Vec<Tok> {
    let mut v = Vec::new();
    let c: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch.is_ascii_alphabetic() || ch == '_' {
            let s = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_') {
                i += 1;
            }
            v.push(Tok::Id(c[s..i].iter().collect()));
        } else if ch.is_ascii_digit() {
            let s = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '.') {
                i += 1;
            }
            let t: String = c[s..i].iter().collect();
            let t = t.trim_end_matches(['u', 'U']);
            let n = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                i64::from_str_radix(h, 16).unwrap_or(0)
            } else if t.len() > 1 && t.starts_with('0') && t.chars().all(|d| d.is_ascii_digit()) {
                i64::from_str_radix(t, 8).unwrap_or(0)
            } else {
                t.parse::<i64>().unwrap_or(0) // floats read as 0: never array sizes
            };
            v.push(Tok::Num(n));
        } else {
            // Two-character operators the preprocessor needs.
            let two: String = c[i..(i + 2).min(c.len())].iter().collect();
            let op = match two.as_str() {
                "&&" => Some('&'),
                "||" => Some('|'),
                "==" => Some('='),
                "!=" => Some('≠'),
                "<=" => Some('≤'),
                ">=" => Some('≥'),
                "<<" => Some('«'),
                ">>" => Some('»'),
                _ => None,
            };
            if let Some(o) = op {
                v.push(Tok::P(o));
                i += 2;
            } else {
                v.push(Tok::P(ch));
                i += 1;
            }
        }
    }
    v
}

/// Preprocessor expression evaluator (after `defined` and macro expansion).
struct Expr<'a> {
    t: &'a [Tok],
    i: usize,
}

impl Expr<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn prec(op: char) -> Option<u8> {
        Some(match op {
            '|' => 1,
            '&' => 2,
            '=' | '≠' => 3,
            '<' | '>' | '≤' | '≥' => 4,
            '«' | '»' => 5,
            '+' | '-' => 6,
            '*' | '/' | '%' => 7,
            _ => return None,
        })
    }
    fn unary(&mut self) -> i64 {
        match self.peek().cloned() {
            Some(Tok::Num(n)) => {
                self.i += 1;
                n
            }
            Some(Tok::Id(_)) => {
                self.i += 1;
                0 // undefined identifiers are 0
            }
            Some(Tok::P('(')) => {
                self.i += 1;
                let v = self.binary(0);
                if self.peek() == Some(&Tok::P(')')) {
                    self.i += 1;
                }
                v
            }
            Some(Tok::P('!')) => {
                self.i += 1;
                (self.unary() == 0) as i64
            }
            Some(Tok::P('-')) => {
                self.i += 1;
                self.unary().wrapping_neg()
            }
            Some(Tok::P('+')) => {
                self.i += 1;
                self.unary()
            }
            Some(Tok::P('~')) => {
                self.i += 1;
                !self.unary()
            }
            _ => {
                self.i += 1;
                0
            }
        }
    }
    fn binary(&mut self, min: u8) -> i64 {
        let mut l = self.unary();
        while let Some(Tok::P(op)) = self.peek().cloned() {
            let Some(p) = Self::prec(op) else { break };
            if p <= min {
                break;
            }
            self.i += 1;
            let r = self.binary(p);
            l = match op {
                '|' => ((l != 0) || (r != 0)) as i64,
                '&' => ((l != 0) && (r != 0)) as i64,
                '=' => (l == r) as i64,
                '≠' => (l != r) as i64,
                '<' => (l < r) as i64,
                '>' => (l > r) as i64,
                '≤' => (l <= r) as i64,
                '≥' => (l >= r) as i64,
                '«' => l.wrapping_shl(r as u32),
                '»' => l.wrapping_shr(r as u32),
                '+' => l.wrapping_add(r),
                '-' => l.wrapping_sub(r),
                '*' => l.wrapping_mul(r),
                '/' => l.checked_div(r).unwrap_or(0),
                '%' => l.checked_rem(r).unwrap_or(0),
                _ => l,
            };
        }
        l
    }
}

/// Runs the preprocessor: the tokens of the active lines, object-like macros
/// expanded; returns them and the `#version`.
fn preprocess(src: &str) -> (Vec<Tok>, u32) {
    let text = strip_comments(src).replace("\\\n", "");
    let mut macros: BTreeMap<String, Vec<Tok>> = BTreeMap::new();
    macros.insert("GL_ES".into(), vec![Tok::Num(1)]);
    macros.insert("GL_FRAGMENT_PRECISION_HIGH".into(), vec![Tok::Num(1)]);
    let mut version = 100;
    // Stack of (this branch active, some branch already taken, parent active).
    let mut stack: Vec<(bool, bool, bool)> = Vec::new();
    let active = |s: &Vec<(bool, bool, bool)>| s.last().is_none_or(|t| t.0);
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim_start();
        if let Some(d) = l.strip_prefix('#') {
            let toks = lex(d);
            let (dir, rest) = match toks.split_first() {
                Some((Tok::Id(n), r)) => (n.as_str(), r),
                _ => continue,
            };
            let parent = active(&stack);
            match dir {
                "version" => {
                    if let Some(Tok::Num(n)) = rest.first() {
                        version = *n as u32;
                    }
                    macros.insert("__VERSION__".into(), vec![Tok::Num(i64::from(version))]);
                }
                "define" if parent => {
                    if let Some(Tok::Id(name)) = rest.first() {
                        // Function-like macros (NAME immediately followed by
                        // '(') are recorded as empty: never needed for
                        // declarations.
                        let fnlike =
                            d.trim_start()["define".len()..].trim_start()[name.len()..].starts_with('(');
                        let body = if fnlike { Vec::new() } else { rest[1..].to_vec() };
                        macros.insert(name.clone(), body);
                    }
                }
                "undef" if parent => {
                    if let Some(Tok::Id(name)) = rest.first() {
                        macros.remove(name);
                    }
                }
                "ifdef" | "ifndef" => {
                    let def = matches!(rest.first(), Some(Tok::Id(n)) if macros.contains_key(n));
                    let on = parent && (def == (dir == "ifdef"));
                    stack.push((on, on, parent));
                }
                "if" => {
                    let on = parent && eval(rest, &macros) != 0;
                    stack.push((on, on, parent));
                }
                "elif" => {
                    if let Some(t) = stack.last_mut() {
                        let on = t.2 && !t.1 && eval(rest, &macros) != 0;
                        t.0 = on;
                        t.1 |= on;
                    }
                }
                "else" => {
                    if let Some(t) = stack.last_mut() {
                        t.0 = t.2 && !t.1;
                        t.1 = true;
                    }
                }
                "endif" => {
                    stack.pop();
                }
                _ => {}
            }
            continue;
        }
        if !active(&stack) {
            continue;
        }
        for t in lex(line) {
            expand(t, &macros, &mut out, 0);
        }
    }
    (out, version)
}

fn expand(t: Tok, macros: &BTreeMap<String, Vec<Tok>>, out: &mut Vec<Tok>, depth: u32) {
    if let Tok::Id(n) = &t
        && depth < 16
        && let Some(body) = macros.get(n)
    {
        for b in body.clone() {
            expand(b, macros, out, depth + 1);
        }
        return;
    }
    out.push(t);
}

fn eval(toks: &[Tok], macros: &BTreeMap<String, Vec<Tok>>) -> i64 {
    // `defined X` / `defined(X)` first, then macros.
    let mut t = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == Tok::Id("defined".into()) {
            let (name, skip) = match (toks.get(i + 1), toks.get(i + 2)) {
                (Some(Tok::P('(')), Some(Tok::Id(n))) => (n.clone(), 4),
                (Some(Tok::Id(n)), _) => (n.clone(), 2),
                _ => (String::new(), 1),
            };
            t.push(Tok::Num(macros.contains_key(&name) as i64));
            i += skip;
        } else {
            expand(toks[i].clone(), macros, &mut t, 0);
            i += 1;
        }
    }
    Expr { t: &t, i: 0 }.binary(0)
}

/// Qualifier words skipped before a type.
const QUALIFIERS: &[&str] =
    &["highp", "mediump", "lowp", "invariant", "flat", "smooth", "centroid", "precise"];

struct Parser {
    t: Vec<Tok>,
    i: usize,
    vertex: bool,
    structs: BTreeMap<String, Vec<Var>>,
    consts: BTreeMap<String, i64>,
    out: Shader,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn id(&self) -> Option<&str> {
        match self.peek() {
            Some(Tok::Id(s)) => Some(s),
            _ => None,
        }
    }
    fn is(&self, c: char) -> bool {
        self.peek() == Some(&Tok::P(c))
    }
    fn skip_to_semicolon(&mut self) {
        let mut depth = 0i32;
        while let Some(t) = self.peek() {
            match t {
                Tok::P('{') | Tok::P('(') | Tok::P('[') => depth += 1,
                Tok::P('}') | Tok::P(')') | Tok::P(']') => depth -= 1,
                Tok::P(';') if depth <= 0 => {
                    self.i += 1;
                    return;
                }
                _ => {}
            }
            self.i += 1;
        }
    }
    /// Skips a balanced `{ … }` starting at the current `{`.
    fn skip_braces(&mut self) {
        let mut depth = 0i32;
        while let Some(t) = self.peek() {
            match t {
                Tok::P('{') => depth += 1,
                Tok::P('}') => {
                    depth -= 1;
                    if depth == 0 {
                        self.i += 1;
                        return;
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
    }

    /// An array size expression `[ … ]` at the cursor (the cursor is on `[`).
    fn array_size(&mut self) -> u32 {
        self.i += 1;
        let s = self.i;
        let mut depth = 0;
        while let Some(t) = self.peek() {
            match t {
                Tok::P('[') => depth += 1,
                Tok::P(']') if depth == 0 => break,
                Tok::P(']') => depth -= 1,
                _ => {}
            }
            self.i += 1;
        }
        let toks: Vec<Tok> = self.t[s..self.i]
            .iter()
            .map(|t| match t {
                Tok::Id(n) => Tok::Num(*self.consts.get(n).unwrap_or(&0)),
                o => o.clone(),
            })
            .collect();
        self.i += 1; // ']'
        let v = Expr { t: &toks, i: 0 }.binary(0);
        v.clamp(0, 1 << 16) as u32
    }

    /// `layout( … )`: location and binding.
    fn layout(&mut self) -> (Option<u32>, Option<u32>) {
        let (mut loc, mut bind) = (None, None);
        self.i += 1;
        if !self.is('(') {
            return (loc, bind);
        }
        self.i += 1;
        while let Some(t) = self.peek().cloned() {
            self.i += 1;
            match t {
                Tok::P(')') => break,
                Tok::Id(k) if k == "location" || k == "binding" => {
                    if self.is('=') {
                        self.i += 1;
                        if let Some(Tok::Num(n)) = self.peek().cloned() {
                            self.i += 1;
                            if k == "location" {
                                loc = Some(n as u32);
                            } else {
                                bind = Some(n as u32);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        (loc, bind)
    }

    /// Members of a `{ … }` (struct or block); the cursor is on `{`.
    fn members(&mut self) -> Vec<Var> {
        let mut v = Vec::new();
        self.i += 1;
        while self.peek().is_some() && !self.is('}') {
            while let Some(q) = self.id() {
                if QUALIFIERS.contains(&q) || q == "layout" {
                    if q == "layout" {
                        self.layout();
                    } else {
                        self.i += 1;
                    }
                } else {
                    break;
                }
            }
            let Some(ty) = self.id().map(str::to_owned) else {
                self.skip_to_semicolon();
                continue;
            };
            self.i += 1;
            self.declarators(&ty, None, &mut v);
        }
        self.i += 1; // '}'
        v
    }

    /// `name[N], name2 …;` of type `ty`, flattened into `out`.
    fn declarators(&mut self, ty: &str, location: Option<u32>, out: &mut Vec<Var>) {
        loop {
            let Some(name) = self.id().map(str::to_owned) else {
                self.skip_to_semicolon();
                return;
            };
            self.i += 1;
            let mut size = 1;
            let mut array = false;
            if self.is('[') {
                size = self.array_size().max(1);
                array = true;
            }
            // Initializers (const, or ES 3 uniform initializers): skipped.
            if self.is('=') {
                while self.peek().is_some() && !self.is(',') && !self.is(';') {
                    if self.is('(') || self.is('{') {
                        let open = if self.is('(') { '(' } else { '{' };
                        let close = if open == '(' { ')' } else { '}' };
                        let mut d = 0;
                        while let Some(t) = self.peek() {
                            if *t == Tok::P(open) {
                                d += 1;
                            } else if *t == Tok::P(close) {
                                d -= 1;
                                if d == 0 {
                                    break;
                                }
                            }
                            self.i += 1;
                        }
                    }
                    self.i += 1;
                }
            }
            self.push_var(ty, &name, size, array, location, out);
            if self.is(',') {
                self.i += 1;
                continue;
            }
            if self.is(';') {
                self.i += 1;
            } else {
                self.skip_to_semicolon();
            }
            return;
        }
    }

    fn push_var(
        &self,
        ty: &str,
        name: &str,
        size: u32,
        array: bool,
        location: Option<u32>,
        out: &mut Vec<Var>,
    ) {
        if let Some(e) = type_enum(ty) {
            out.push(Var { name: name.into(), ty: e, size, array, location });
        } else if let Some(fields) = self.structs.get(ty) {
            // Structs flatten: s.f, or a[i].f for arrays of structs.
            for k in 0..size {
                let base = if array { format!("{name}[{k}]") } else { name.to_owned() };
                for f in fields {
                    out.push(Var { name: format!("{base}.{}", f.name), location: None, ..f.clone() });
                }
            }
        }
    }

    fn run(&mut self) {
        while self.peek().is_some() {
            let mut storage = None;
            let mut location = None;
            let mut binding = None;
            let mut is_const = false;
            // Qualifiers.
            loop {
                match self.id() {
                    Some("layout") => {
                        let (l, b) = self.layout();
                        location = l.or(location);
                        binding = b.or(binding);
                    }
                    Some(q @ ("uniform" | "attribute" | "in" | "out" | "varying" | "buffer")) => {
                        storage = Some(q.to_owned());
                        self.i += 1;
                    }
                    Some("const") => {
                        is_const = true;
                        self.i += 1;
                    }
                    Some(q) if QUALIFIERS.contains(&q) => self.i += 1,
                    _ => break,
                }
            }
            match self.id() {
                Some("precision") => {
                    self.skip_to_semicolon();
                    continue;
                }
                Some("struct") => {
                    self.i += 1;
                    let name = self.id().map(str::to_owned).unwrap_or_default();
                    if self.id().is_some() {
                        self.i += 1;
                    }
                    if !self.is('{') {
                        self.skip_to_semicolon();
                        continue;
                    }
                    let fields = self.members();
                    self.structs.insert(name.clone(), fields);
                    if self.is(';') {
                        self.i += 1;
                    } else if storage.as_deref() == Some("uniform") {
                        let mut v = Vec::new();
                        self.declarators(&name, None, &mut v);
                        self.out.uniforms.extend(v);
                    } else {
                        self.skip_to_semicolon();
                    }
                    continue;
                }
                _ => {}
            }
            let Some(ty) = self.id().map(str::to_owned) else {
                // Stray tokens: move on.
                if self.is('{') {
                    self.skip_braces();
                } else {
                    self.i += 1;
                }
                continue;
            };
            self.i += 1;
            // Uniform block: `uniform Name { … } inst;`
            if storage.as_deref() == Some("uniform") && self.is('{') {
                let members = self.members();
                let mut size = 1;
                if let Some(_inst) = self.id() {
                    self.i += 1;
                    if self.is('[') {
                        size = self.array_size().max(1);
                    }
                }
                self.skip_to_semicolon();
                self.out.blocks.push(Block { name: ty, size, members, binding });
                continue;
            }
            // Function definition or prototype: `type name(`…
            if self.id().is_some() && self.t.get(self.i + 1) == Some(&Tok::P('(')) {
                self.i += 1;
                let mut depth = 0;
                while let Some(t) = self.peek() {
                    match t {
                        Tok::P('(') => depth += 1,
                        Tok::P(')') => {
                            depth -= 1;
                            if depth == 0 {
                                self.i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
                if self.is('{') {
                    self.skip_braces();
                } else {
                    self.skip_to_semicolon();
                }
                continue;
            }
            if is_const && ty == "int" || is_const && ty == "uint" {
                // const int N = expr;
                if let Some(name) = self.id().map(str::to_owned) {
                    self.i += 1;
                    if self.is('=') {
                        self.i += 1;
                        let s = self.i;
                        while self.peek().is_some() && !self.is(';') && !self.is(',') {
                            self.i += 1;
                        }
                        let toks: Vec<Tok> = self.t[s..self.i]
                            .iter()
                            .map(|t| match t {
                                Tok::Id(n) => Tok::Num(*self.consts.get(n).unwrap_or(&0)),
                                o => o.clone(),
                            })
                            .collect();
                        let v = Expr { t: &toks, i: 0 }.binary(0);
                        self.consts.insert(name, v);
                    }
                }
                self.skip_to_semicolon();
                continue;
            }
            match storage.as_deref() {
                Some("uniform") => {
                    let mut v = Vec::new();
                    self.declarators(&ty, None, &mut v);
                    self.out.uniforms.extend(v);
                }
                Some("attribute") | Some("in") if self.vertex => {
                    let mut v = Vec::new();
                    self.declarators(&ty, location, &mut v);
                    self.out.inputs.extend(v);
                }
                _ => self.skip_to_semicolon(),
            }
        }
    }
}

/// Scans one shader.
pub fn scan(src: &str, vertex: bool) -> Shader {
    let (t, version) = preprocess(src);
    let mut p = Parser {
        t,
        i: 0,
        vertex,
        structs: BTreeMap::new(),
        consts: BTreeMap::new(),
        out: Shader { version, ..Shader::default() },
    };
    p.run();
    p.out
}

/// Makes guest shader text acceptable to WebGL2: external samplers become
/// `sampler2D` (their ColorBuffers are 2D textures on the host) and the
/// `GL_OES_EGL_image_external` directives go away.
pub fn rewrite_for_webgl(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with('#') && t.contains("extension") && t.contains("GL_OES_EGL_image_external") {
            out.push('\n');
            continue;
        }
        out.push_str(line);
    }
    out.replace("samplerExternalOES", "sampler2D")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skia_style_fragment_shader() {
        let src = "#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision mediump float;
precision mediump sampler2D;
// comment with uniform vec4 fake;
uniform highp vec4 sk_RTAdjust;
uniform vec4 uColor_S0, uRect_S1;
uniform samplerExternalOES uTextureSampler_0_S0;
/* uniform float also_fake; */
uniform highp float uKernel_S1[7];
in highp vec2 vLocalCoord_S0;
out mediump vec4 sk_FragColor;
vec4 helper(vec4 c) { return c * uColor_S0; }
void main() {
    sk_FragColor = helper(texture(uTextureSampler_0_S0, vLocalCoord_S0));
}
";
        let s = scan(src, false);
        assert_eq!(s.version, 300);
        let names: Vec<_> = s.uniforms.iter().map(|u| (u.name.as_str(), u.ty, u.size)).collect();
        assert_eq!(
            names,
            [
                ("sk_RTAdjust", 0x8B52, 1),
                ("uColor_S0", 0x8B52, 1),
                ("uRect_S1", 0x8B52, 1),
                ("uTextureSampler_0_S0", 0x8D66, 1),
                ("uKernel_S1", 0x1406, 7),
            ]
        );
        assert!(s.inputs.is_empty(), "fragment inputs are varyings");
        let w = rewrite_for_webgl(src);
        assert!(w.contains("uniform sampler2D uTextureSampler_0_S0"));
        assert!(!w.contains("GL_OES_EGL_image_external"));
        assert_eq!(w.lines().count(), src.lines().count(), "line numbers kept");
    }

    #[test]
    fn vertex_inputs_layout_structs_blocks_and_preprocessor() {
        let src = "#version 300 es
#define N 3
#define USE_EXTRA
const int M = N * 2;
struct Light { vec3 pos; float power[2]; };
uniform Light uLights[2];
uniform mat4 uMatrix;
uniform vec4 uArr[M + 1];
#ifdef USE_EXTRA
uniform float uExtra;
#else
uniform float uNotThere;
#endif
#if defined(NOPE) || (N > 5)
uniform float uAlsoNot;
#elif N == 3
uniform int uElif;
#endif
layout(std140) uniform Globals { mat4 view; vec4 tint; } globals;
layout(location = 2) in vec4 aColor;
in vec2 aPosition;
in mat3 aTransform;
void main() { gl_Position = uMatrix * vec4(aPosition, 0.0, 1.0); }
";
        let s = scan(src, true);
        let u: Vec<_> = s.uniforms.iter().map(|u| (u.name.as_str(), u.size)).collect();
        assert_eq!(
            u,
            [
                ("uLights[0].pos", 1),
                ("uLights[0].power", 2),
                ("uLights[1].pos", 1),
                ("uLights[1].power", 2),
                ("uMatrix", 1),
                ("uArr", 7),
                ("uExtra", 1),
                ("uElif", 1),
            ]
        );
        assert_eq!(s.blocks.len(), 1);
        assert_eq!(s.blocks[0].name, "Globals");
        assert_eq!(s.blocks[0].members.len(), 2);
        let a: Vec<_> = s.inputs.iter().map(|a| (a.name.as_str(), a.ty, a.location)).collect();
        assert_eq!(
            a,
            [("aColor", 0x8B52, Some(2)), ("aPosition", 0x8B50, None), ("aTransform", 0x8B5B, None)]
        );
    }

    #[test]
    fn essl1_attributes_and_function_prototypes() {
        let src = "attribute vec4 a_position;
attribute vec2 a_texcoord;
uniform mat4 u_mvp;
varying vec2 v_tex;
vec4 f(vec4 x);
void main() { v_tex = a_texcoord; gl_Position = f(u_mvp * a_position); }
vec4 f(vec4 x) { return x; }
";
        let s = scan(src, true);
        assert_eq!(s.version, 100);
        assert_eq!(s.inputs.len(), 2);
        assert_eq!(s.uniforms.len(), 1);
    }
}
