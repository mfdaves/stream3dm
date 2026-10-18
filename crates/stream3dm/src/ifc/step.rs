//! STEP physical files (ISO 10303-21), the text encoding of IFC: records split from a stream
//! of slices, and the argument lists of the records worth reading.
//!
//! A record is `#id=TYPE(arguments);` in the data section, `TYPE(arguments);` in the header.
//! Strings are quoted with `'` (a doubled `''` inside), may hold `;` and `)`, and encode
//! characters beyond ASCII with `\X\hh`, `\X2\hhhh…\X0\`, `\X4\hhhhhhhh…\X0\` and `\S\c`
//! directives; comments are `/* … */`.

/// Splits a byte stream into records, slice by slice; only the record being read is buffered.
#[derive(Default)]
pub(crate) struct Splitter {
    record: Vec<u8>,
    state: State,
    /// Bytes consumed so far.
    pub(crate) offset: u64,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Code,
    /// Inside a string.
    Quoted,
    /// Inside a string, just after a `'`: either the closing quote or the first of `''`.
    QuoteSeen,
    /// After a `/` outside strings: maybe a comment.
    Slash,
    Comment,
    /// Inside a comment, just after a `*`.
    CommentStar,
}

impl Splitter {
    /// Feeds `data`, calling `record` with each complete record (without its `;`).
    pub(crate) fn push(&mut self, data: &[u8], mut record: impl FnMut(&[u8])) {
        let mut start = 0;
        for (i, &b) in data.iter().enumerate() {
            match self.state {
                State::Code => match b {
                    b'\'' => self.state = State::Quoted,
                    b'/' => self.state = State::Slash,
                    b';' => {
                        if self.record.is_empty() {
                            record(&data[start..i]);
                        } else {
                            self.record.extend_from_slice(&data[start..i]);
                            record(&self.record);
                            self.record.clear();
                        }
                        start = i + 1;
                    }
                    _ => {}
                },
                State::Quoted => {
                    if b == b'\'' {
                        self.state = State::QuoteSeen;
                    }
                }
                State::QuoteSeen => {
                    // `''` stays in the string; anything else follows the closing quote.
                    self.state = if b == b'\'' {
                        State::Quoted
                    } else {
                        State::Code
                    };
                    if self.state == State::Code {
                        self.code_byte(b, data, i, &mut start, &mut record);
                    }
                }
                State::Slash => {
                    self.state = if b == b'*' {
                        State::Comment
                    } else {
                        State::Code
                    };
                    if self.state == State::Code {
                        self.code_byte(b, data, i, &mut start, &mut record);
                    }
                }
                State::Comment => {
                    if b == b'*' {
                        self.state = State::CommentStar;
                    }
                }
                State::CommentStar => {
                    self.state = match b {
                        b'/' => State::Code,
                        b'*' => State::CommentStar,
                        _ => State::Comment,
                    };
                }
            }
        }
        self.record.extend_from_slice(&data[start..]);
        self.offset += data.len() as u64;
    }

    /// A byte in code state reached from another state (a quote or slash just closed).
    fn code_byte(
        &mut self,
        b: u8,
        data: &[u8],
        i: usize,
        start: &mut usize,
        record: &mut impl FnMut(&[u8]),
    ) {
        match b {
            b'\'' => self.state = State::Quoted,
            b'/' => self.state = State::Slash,
            b';' => {
                if self.record.is_empty() {
                    record(&data[*start..i]);
                } else {
                    self.record.extend_from_slice(&data[*start..i]);
                    record(&self.record);
                    self.record.clear();
                }
                *start = i + 1;
            }
            _ => {}
        }
    }

    /// Bytes of an unfinished record at the end of the input.
    pub(crate) fn pending(&self) -> usize {
        self.record
            .iter()
            .filter(|b| !b.is_ascii_whitespace())
            .count()
    }
}

/// The parts of a record: its id (none in the header) and its upper-case type name, with the
/// argument text still unparsed.
pub(crate) struct Record<'a> {
    pub id: Option<u64>,
    pub name: &'a str,
    pub args: &'a [u8],
}

/// Splits `#id = TYPE ( … )` into its parts; None for anything else (section markers such as
/// `DATA`, `ENDSEC`, comments only).
pub(crate) fn record(text: &[u8]) -> Option<Record<'_>> {
    let mut i = skip_space(text, 0);
    let mut id = None;
    if text.get(i) == Some(&b'#') {
        let digits_start = i + 1;
        let mut j = digits_start;
        while j < text.len() && text[j].is_ascii_digit() {
            j += 1;
        }
        id = std::str::from_utf8(&text[digits_start..j])
            .ok()?
            .parse()
            .ok();
        i = skip_space(text, j);
        if text.get(i) != Some(&b'=') {
            return None;
        }
        i = skip_space(text, i + 1);
    }
    let name_start = i;
    while i < text.len() && (text[i].is_ascii_alphanumeric() || text[i] == b'_') {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = std::str::from_utf8(&text[name_start..i]).ok()?;
    let open = skip_space(text, i);
    if text.get(open) != Some(&b'(') {
        return None;
    }
    // The argument list runs to the last `)`.
    let close = text.iter().rposition(|&b| b == b')')?;
    if close < open {
        return None;
    }
    Some(Record {
        id,
        name,
        args: &text[open + 1..close],
    })
}

fn skip_space(text: &[u8], mut i: usize) -> usize {
    while i < text.len() && text[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// An argument value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    /// `$`: unset.
    Null,
    /// `*`: derived.
    Derived,
    Int(i64),
    Real(f64),
    Str(String),
    /// `.NAME.`: an enumeration or boolean (`.T.`, `.F.`, `.U.`).
    Enum(String),
    Ref(u64),
    List(Vec<Value>),
    /// `TYPE(value)`: a typed value such as `IFCLABEL('x')` or `IFCLENGTHMEASURE(2.5)`.
    Typed(String, Box<Value>),
    /// Hexadecimal binary (`"0A1B"`), kept as written.
    Binary(String),
}

impl Value {
    pub(crate) fn as_ref(&self) -> Option<u64> {
        match self {
            Self::Ref(r) => Some(*r),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            Self::Typed(_, v) => v.as_str(),
            _ => None,
        }
    }

    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Real(x) => Some(*x),
            Self::Int(i) => Some(*i as f64),
            Self::Typed(_, v) => v.as_f64(),
            _ => None,
        }
    }

    pub(crate) fn refs(&self) -> Vec<u64> {
        match self {
            Self::List(items) => items.iter().filter_map(Value::as_ref).collect(),
            Self::Ref(r) => vec![*r],
            _ => Vec::new(),
        }
    }

    pub(crate) fn list(&self) -> &[Value] {
        match self {
            Self::List(items) => items,
            _ => &[],
        }
    }

    /// The value as text for a property panel: strings as they are, numbers in the shortest
    /// form, booleans as true/false, lists joined with commas.
    pub(crate) fn display(&self) -> Option<String> {
        match self {
            Self::Null | Self::Derived => None,
            Self::Int(i) => Some(i.to_string()),
            Self::Real(x) => Some(format_number(*x)),
            Self::Str(s) => Some(s.clone()),
            Self::Enum(e) => Some(match e.as_str() {
                "T" => "true".to_string(),
                "F" => "false".to_string(),
                "U" => "unknown".to_string(),
                other => other.to_string(),
            }),
            Self::Ref(r) => Some(format!("#{r}")),
            Self::List(items) => {
                let parts: Vec<String> = items.iter().filter_map(Value::display).collect();
                (!parts.is_empty()).then(|| parts.join(", "))
            }
            Self::Typed(_, v) => v.display(),
            Self::Binary(b) => Some(b.clone()),
        }
    }
}

fn format_number(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        let s = format!("{:.6}", x);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Parses an argument list (the text between the record's parentheses).
pub(crate) fn args(text: &[u8]) -> Vec<Value> {
    let mut p = Parser {
        text,
        at: 0,
        depth: 0,
    };
    let mut out = Vec::new();
    loop {
        p.space();
        if p.at >= text.len() {
            break;
        }
        out.push(p.value());
        p.space();
        if p.peek() == Some(b',') {
            p.at += 1;
        } else {
            break;
        }
    }
    out
}

/// Lists nested deeper than this are skipped whole (IFC nests a few levels; a crafted file
/// could nest until the stack overflows).
const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
    /// Lists and typed values open around the current value.
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn space(&mut self) {
        self.at = skip_space(self.text, self.at);
    }

    /// Skips a parenthesised group that starts at the cursor, strings included, without
    /// recursion.
    fn skip_group(&mut self) {
        let mut open = 0usize;
        while let Some(b) = self.peek() {
            match b {
                b'\'' => {
                    self.string();
                    continue;
                }
                b'(' => open += 1,
                b')' => {
                    open = open.saturating_sub(1);
                    if open == 0 {
                        self.at += 1;
                        return;
                    }
                }
                _ => {}
            }
            self.at += 1;
        }
    }

    fn value(&mut self) -> Value {
        self.space();
        if self.depth >= MAX_DEPTH && self.peek() == Some(b'(') {
            self.skip_group();
            return Value::Null;
        }
        match self.peek() {
            Some(b'$') => {
                self.at += 1;
                Value::Null
            }
            Some(b'*') => {
                self.at += 1;
                Value::Derived
            }
            Some(b'#') => {
                self.at += 1;
                let start = self.at;
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.at += 1;
                }
                std::str::from_utf8(&self.text[start..self.at])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .map_or(Value::Null, Value::Ref)
            }
            Some(b'\'') => Value::Str(self.string()),
            Some(b'"') => {
                self.at += 1;
                let start = self.at;
                while self.peek().is_some_and(|b| b != b'"') {
                    self.at += 1;
                }
                let s = String::from_utf8_lossy(&self.text[start..self.at]).into_owned();
                self.at += 1;
                Value::Binary(s)
            }
            Some(b'.') => {
                self.at += 1;
                let start = self.at;
                while self.peek().is_some_and(|b| b != b'.') {
                    self.at += 1;
                }
                let s = String::from_utf8_lossy(&self.text[start..self.at]).into_owned();
                self.at += 1;
                Value::Enum(s)
            }
            Some(b'(') => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.space();
                    match self.peek() {
                        Some(b')') => {
                            self.at += 1;
                            break;
                        }
                        None => break,
                        _ => {}
                    }
                    self.depth += 1;
                    items.push(self.value());
                    self.depth -= 1;
                    self.space();
                    if self.peek() == Some(b',') {
                        self.at += 1;
                    }
                }
                Value::List(items)
            }
            Some(b) if b.is_ascii_alphabetic() => {
                let start = self.at;
                while self
                    .peek()
                    .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    self.at += 1;
                }
                let name = String::from_utf8_lossy(&self.text[start..self.at]).into_owned();
                self.space();
                if self.peek() == Some(b'(') {
                    if self.depth >= MAX_DEPTH {
                        self.skip_group();
                        return Value::Null;
                    }
                    self.at += 1;
                    self.depth += 1;
                    let inner = self.value();
                    self.depth -= 1;
                    self.space();
                    // Skip to the closing parenthesis of the typed value.
                    while self.peek().is_some_and(|b| b != b')') {
                        self.at += 1;
                    }
                    self.at += 1;
                    Value::Typed(name, Box::new(inner))
                } else {
                    Value::Enum(name)
                }
            }
            Some(_) => {
                let start = self.at;
                while self.peek().is_some_and(|b| {
                    b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'E' | b'e')
                }) {
                    self.at += 1;
                }
                if self.at == start {
                    // Unknown byte: skip it so parsing always advances.
                    self.at += 1;
                    return Value::Null;
                }
                let s = std::str::from_utf8(&self.text[start..self.at]).unwrap_or("");
                if s.contains(['.', 'E', 'e']) {
                    s.parse().map_or(Value::Null, Value::Real)
                } else {
                    s.parse()
                        .map(Value::Int)
                        .or_else(|_| s.parse().map(Value::Real))
                        .unwrap_or(Value::Null)
                }
            }
            None => Value::Null,
        }
    }

    /// A quoted string, `''` and the encoding directives decoded.
    fn string(&mut self) -> String {
        self.at += 1;
        let mut raw = Vec::new();
        while let Some(b) = self.peek() {
            self.at += 1;
            if b == b'\'' {
                if self.peek() == Some(b'\'') {
                    raw.push(b'\'');
                    self.at += 1;
                } else {
                    break;
                }
            } else {
                raw.push(b);
            }
        }
        decode_string(&raw)
    }
}

/// Decodes the `\X\`, `\X2\`, `\X4\` and `\S\` directives of a STEP string.
pub(crate) fn decode_string(raw: &[u8]) -> String {
    let hex =
        |s: &[u8]| -> Option<u32> { u32::from_str_radix(std::str::from_utf8(s).ok()?, 16).ok() };
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\\' {
            let rest = &raw[i..];
            if rest.starts_with(b"\\X2\\") || rest.starts_with(b"\\X4\\") {
                let width = if rest[2] == b'2' { 4 } else { 8 };
                let mut j = i + 4;
                while j + width <= raw.len() && !raw[j..].starts_with(b"\\X0\\") {
                    if let Some(c) = hex(&raw[j..j + width]).and_then(char::from_u32) {
                        out.push(c);
                    }
                    j += width;
                }
                i = (j + 4).min(raw.len());
                continue;
            }
            if rest.starts_with(b"\\X\\") && rest.len() >= 5 {
                if let Some(c) = hex(&rest[3..5]).and_then(char::from_u32) {
                    out.push(c);
                }
                i += 5;
                continue;
            }
            if rest.starts_with(b"\\S\\") && rest.len() >= 4 {
                // ISO 8859-1 upper half.
                if let Some(c) = char::from_u32(u32::from(rest[3]) + 128) {
                    out.push(c);
                }
                i += 4;
                continue;
            }
            if rest.starts_with(b"\\\\") {
                out.push('\\');
                i += 2;
                continue;
            }
        }
        // Plain bytes: UTF-8 when valid (many writers emit it), else Latin-1.
        let end = raw[i..]
            .iter()
            .position(|&b| b == b'\\')
            .map_or(raw.len(), |p| i + p.max(1));
        match std::str::from_utf8(&raw[i..end]) {
            Ok(s) => out.push_str(s),
            Err(_) => out.extend(raw[i..end].iter().map(|&b| char::from(b))),
        }
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(parts: &[&[u8]]) -> Vec<String> {
        let mut s = Splitter::default();
        let mut out = Vec::new();
        for p in parts {
            s.push(p, |r| {
                out.push(String::from_utf8_lossy(r).trim().to_string())
            });
        }
        out
    }

    #[test]
    fn records_split_across_slices_strings_and_comments() {
        let text = b"#1=IFCWALL('a;b','it''s');/* c; */#2=IFCSLAB($);";
        let whole = split(&[text]);
        assert_eq!(
            whole,
            ["#1=IFCWALL('a;b','it''s')", "/* c; */#2=IFCSLAB($)"]
        );
        // Every split point gives the same records.
        for cut in 1..text.len() {
            assert_eq!(split(&[&text[..cut], &text[cut..]]), whole, "cut at {cut}");
        }
    }

    #[test]
    fn record_parts_and_arguments() {
        let r =
            record(b" #12 = IFCPROPERTYSINGLEVALUE('Width',$,IFCLENGTHMEASURE(0.25),$)").unwrap();
        assert_eq!((r.id, r.name), (Some(12), "IFCPROPERTYSINGLEVALUE"));
        let a = args(r.args);
        assert_eq!(a[0], Value::Str("Width".into()));
        assert_eq!(a[1], Value::Null);
        assert_eq!(
            a[2],
            Value::Typed("IFCLENGTHMEASURE".into(), Box::new(Value::Real(0.25)))
        );
        assert_eq!(a[2].display().as_deref(), Some("0.25"));
        let r = record(b"#5=IFCRELAGGREGATES('g',#2,$,$,#3,(#4,#6,#7))").unwrap();
        let a = args(r.args);
        assert_eq!(a[4], Value::Ref(3));
        assert_eq!(a[5].refs(), [4, 6, 7]);
        let a = args(b".T.,-1.5E-3,42,*,\"0F\"");
        assert_eq!(
            a,
            [
                Value::Enum("T".into()),
                Value::Real(-0.0015),
                Value::Int(42),
                Value::Derived,
                Value::Binary("0F".into())
            ]
        );
        assert!(record(b"DATA").is_none());
        let header = record(b"FILE_SCHEMA(('IFC4'))").unwrap();
        assert_eq!((header.id, header.name), (None, "FILE_SCHEMA"));
    }

    #[test]
    fn deep_nesting_is_skipped_not_followed() {
        // 200,000 nested lists would overflow the stack if followed; past MAX_DEPTH the
        // group is skipped whole and the arguments after it still read.
        let deep = format!("{}{},'after'", "(".repeat(200_000), ")".repeat(200_000));
        let values = args(deep.as_bytes());
        assert_eq!(values.len(), 2);
        assert_eq!(values[1], Value::Str("after".into()));
        // Ordinary nesting is read in full.
        let mut v = &args(b"((((((1.5))))))")[0];
        for _ in 0..6 {
            let Value::List(items) = v else {
                panic!("{v:?}")
            };
            v = &items[0];
        }
        assert!(matches!(v, Value::Real(_) | Value::Int(_)), "{v:?}");
        // Unbalanced input ends without a panic.
        let _ = args("(".repeat(100_000).as_bytes());
        let _ = args(format!("IFCX{}", "(".repeat(100_000)).as_bytes());
    }

    #[test]
    fn string_directives() {
        assert_eq!(decode_string(b"K\\X2\\00FC\\X0\\che"), "Küche");
        assert_eq!(decode_string(b"\\X\\E4"), "ä");
        assert_eq!(decode_string(b"\\X4\\0001F600\\X0\\"), "😀");
        assert_eq!(decode_string(b"a\\\\b"), "a\\b");
        assert_eq!(decode_string("Straße".as_bytes()), "Straße");
    }
}
