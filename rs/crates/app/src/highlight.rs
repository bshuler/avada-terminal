//! Source colouring: a line of code in, markdown-with-`<font>` markup out.
//!
//! Slint can draw coloured runs inside one paragraph — that is what `StyledText` is for —
//! but it offers no way to *build* one span by span: `StyledText::paragraphs` is
//! `pub(crate)`, and the only public constructors are `from_plain_text` and
//! `from_markdown`. So the colours have to arrive as markup, and `from_markdown` happens to
//! accept `<font color="#rrggbb">…</font>` and turn it into a real per-span colour. This
//! module is the Rust side of that trade: it tokenises a line and re-emits it as markup,
//! keeping the promise the viewpane module's header makes — *all of the parsing happens in
//! Rust, never in `.slint`*.
//!
//! Three things about that markup channel cost an experiment each, and all three are
//! load-bearing:
//!
//! * **Every metacharacter must be backslash-escaped.** Not for tidiness: an unsupported
//!   HTML tag is a hard `Err` from `from_markdown`, and `viewpane::markdown_text` falls back
//!   to `from_plain_text` on error — so one unescaped `<T>` in a generic would not mangle a
//!   word, it would silently drop *every colour on the row*. Escaping is what makes the
//!   round trip byte-exact.
//! * **Leading whitespace does not survive.** Four spaces or a tab is a markdown code block
//!   (another hard `Err`); two spaces are silently eaten. Indentation is therefore re-emitted
//!   as NBSP (U+00A0), which passes through verbatim. Internal and trailing runs are left
//!   alone — internal spaces survive as-is, and trailing ones do not matter in code.
//! * **Colour comes from the palette, never from a literal.** `ui/theme.slint` states the
//!   house rule — no colour is written anywhere but there — and there are five palettes,
//!   one of them light. Hard-coding a syntax theme would be the one thing on screen that
//!   stops changing when the user changes theirs, so each class maps onto an existing
//!   token instead: this is a re-use of the palette, not a second one.
//!
//! The tokeniser is hand-rolled for the same reason the markdown parser and the mermaid
//! layout engine next door are: it is the house style, it is the only way to keep the
//! output on the palette, and a highlighting crate would drag a regex engine and a syntax
//! dump in for a file that is never more than [`crate::viewpane::MAX_LINES`] lines long.

use crate::theme::UiPalette;

/// What a run of characters *is*, which is all the tokeniser commits to. Deliberately
/// coarse — six classes is what a palette with one accent can actually distinguish, and a
/// finer split (say, function names apart from other identifiers) would have to invent a
/// colour that no theme token owns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// Identifiers, whitespace, and anything unclaimed: drawn in the row's default ink.
    Plain,
    Keyword,
    /// A type name — an entry in the language's built-in list, or a `CamelCase` word in the
    /// languages that reserve that shape for types.
    Type,
    Str,
    Number,
    Comment,
    Punct,
}

impl Class {
    /// The palette token this class borrows, or `None` for [`Class::Plain`], which is left
    /// unmarked so it inherits the row's `default-color` — that also keeps the markup small,
    /// since most of a line is plain.
    fn ink(self, p: &UiPalette) -> Option<u32> {
        match self {
            Class::Plain => None,
            Class::Keyword => Some(p.accent),
            Class::Type => Some(p.link),
            Class::Str => Some(p.ok),
            Class::Number => Some(p.warn),
            Class::Comment => Some(p.faint),
            Class::Punct => Some(p.subtext),
        }
    }
}

/// One language's surface syntax. A table rather than a trait because every language here
/// differs only in these six answers; the scanner below is the same for all of them.
struct Syntax {
    /// Every prefix that starts a comment running to end of line.
    line_comment: &'static [&'static str],
    /// The delimiters of a comment that can cross lines, if the language has one.
    block_comment: Option<(&'static str, &'static str)>,
    /// Whether `/* /* */ */` closes once or twice — Rust nests, C does not.
    nest_block: bool,
    /// The quote characters that open a string.
    quotes: &'static [char],
    /// Python's `"""` / `'''`.
    triple: bool,
    /// JavaScript's backtick, which is the one quote allowed to cross a line.
    template: bool,
    /// `'a` is a lifetime, not an unterminated char literal.
    lifetimes: bool,
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    /// Whether a `CamelCase` identifier is a type by convention.
    camel_is_type: bool,
    /// Tags and attributes instead of keywords and identifiers.
    markup: bool,
}

const DEFAULT: Syntax = Syntax {
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    nest_block: false,
    quotes: &['"', '\''],
    triple: false,
    template: false,
    lifetimes: false,
    keywords: &[],
    types: &[],
    camel_is_type: true,
    markup: false,
};

const RUST: Syntax = Syntax {
    nest_block: true,
    lifetimes: true,
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "union", "unsafe", "use", "where", "while", "yield",
    ],
    types: &[
        "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8",
        "u16", "u32", "u64", "u128", "usize",
    ],
    ..DEFAULT
};

const JS: Syntax = Syntax {
    template: true,
    keywords: &[
        "abstract",
        "any",
        "as",
        "async",
        "await",
        "boolean",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "constructor",
        "continue",
        "debugger",
        "declare",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "from",
        "function",
        "get",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "is",
        "keyof",
        "let",
        "namespace",
        "new",
        "null",
        "of",
        "private",
        "protected",
        "public",
        "readonly",
        "require",
        "return",
        "satisfies",
        "set",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "type",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "yield",
    ],
    types: &[
        "number", "string", "symbol", "bigint", "never", "unknown", "object",
    ],
    ..DEFAULT
};

const SWIFT: Syntax = Syntax {
    keywords: &[
        "as",
        "associatedtype",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "continue",
        "default",
        "defer",
        "deinit",
        "do",
        "else",
        "enum",
        "extension",
        "fallthrough",
        "false",
        "fileprivate",
        "for",
        "func",
        "guard",
        "if",
        "import",
        "in",
        "init",
        "inout",
        "internal",
        "is",
        "let",
        "mutating",
        "nil",
        "open",
        "operator",
        "private",
        "protocol",
        "public",
        "repeat",
        "required",
        "return",
        "self",
        "Self",
        "static",
        "struct",
        "subscript",
        "super",
        "switch",
        "throw",
        "throws",
        "true",
        "try",
        "typealias",
        "var",
        "weak",
        "where",
        "while",
    ],
    types: &[
        "Bool",
        "Int",
        "Double",
        "Float",
        "String",
        "Character",
        "Void",
        "Any",
    ],
    ..DEFAULT
};

const KOTLIN: Syntax = Syntax {
    keywords: &[
        "abstract",
        "actual",
        "annotation",
        "as",
        "break",
        "by",
        "catch",
        "class",
        "companion",
        "const",
        "constructor",
        "continue",
        "crossinline",
        "data",
        "do",
        "else",
        "enum",
        "expect",
        "external",
        "false",
        "final",
        "finally",
        "for",
        "fun",
        "get",
        "if",
        "import",
        "in",
        "infix",
        "init",
        "inline",
        "inner",
        "interface",
        "internal",
        "is",
        "lateinit",
        "null",
        "object",
        "open",
        "operator",
        "out",
        "override",
        "package",
        "private",
        "protected",
        "public",
        "reified",
        "return",
        "sealed",
        "set",
        "super",
        "suspend",
        "this",
        "throw",
        "true",
        "try",
        "typealias",
        "val",
        "var",
        "vararg",
        "when",
        "where",
        "while",
    ],
    types: &[
        "Boolean", "Byte", "Short", "Int", "Long", "Float", "Double", "Char", "String", "Unit",
        "Any",
    ],
    ..DEFAULT
};

const JAVA: Syntax = Syntax {
    keywords: &[
        "abstract",
        "assert",
        "break",
        "case",
        "catch",
        "class",
        "continue",
        "default",
        "do",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "for",
        "if",
        "implements",
        "import",
        "instanceof",
        "interface",
        "native",
        "new",
        "null",
        "package",
        "private",
        "protected",
        "public",
        "record",
        "return",
        "sealed",
        "static",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "try",
        "var",
        "void",
        "volatile",
        "while",
        "true",
        "false",
    ],
    types: &[
        "boolean", "byte", "char", "double", "float", "int", "long", "short", "String",
    ],
    ..DEFAULT
};

const C: Syntax = Syntax {
    keywords: &[
        "auto",
        "break",
        "case",
        "class",
        "const",
        "constexpr",
        "continue",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "extern",
        "false",
        "for",
        "goto",
        "if",
        "inline",
        "namespace",
        "new",
        "nullptr",
        "operator",
        "private",
        "protected",
        "public",
        "register",
        "return",
        "sizeof",
        "static",
        "struct",
        "switch",
        "template",
        "this",
        "throw",
        "true",
        "try",
        "typedef",
        "typename",
        "union",
        "using",
        "virtual",
        "volatile",
        "while",
    ],
    types: &[
        "bool", "char", "double", "float", "int", "long", "short", "signed", "size_t", "unsigned",
        "void",
    ],
    ..DEFAULT
};

const GO: Syntax = Syntax {
    quotes: &['"', '\'', '`'],
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
        "nil",
        "true",
        "false",
    ],
    types: &[
        "bool",
        "byte",
        "complex64",
        "complex128",
        "error",
        "float32",
        "float64",
        "int",
        "int8",
        "int16",
        "int32",
        "int64",
        "rune",
        "string",
        "uint",
        "uint8",
        "uint16",
        "uint32",
        "uint64",
        "uintptr",
    ],
    ..DEFAULT
};

const DART: Syntax = Syntax {
    keywords: &[
        "abstract",
        "as",
        "assert",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "covariant",
        "default",
        "deferred",
        "do",
        "dynamic",
        "else",
        "enum",
        "export",
        "extends",
        "extension",
        "external",
        "factory",
        "false",
        "final",
        "finally",
        "for",
        "get",
        "if",
        "implements",
        "import",
        "in",
        "is",
        "late",
        "library",
        "mixin",
        "new",
        "null",
        "on",
        "operator",
        "part",
        "required",
        "rethrow",
        "return",
        "set",
        "show",
        "static",
        "super",
        "switch",
        "sync",
        "this",
        "throw",
        "true",
        "try",
        "typedef",
        "var",
        "void",
        "while",
        "with",
        "yield",
    ],
    types: &[
        "bool", "double", "int", "num", "String", "List", "Map", "Set", "Future", "Stream",
    ],
    ..DEFAULT
};

const PYTHON: Syntax = Syntax {
    line_comment: &["#"],
    block_comment: None,
    triple: true,
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
        "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True",
        "try", "while", "with", "yield",
    ],
    types: &[
        "bool",
        "bytes",
        "dict",
        "float",
        "frozenset",
        "int",
        "list",
        "object",
        "set",
        "str",
        "tuple",
    ],
    ..DEFAULT
};

const SHELL: Syntax = Syntax {
    line_comment: &["#"],
    block_comment: None,
    keywords: &[
        "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if",
        "in", "local", "readonly", "return", "select", "then", "until", "while",
    ],
    types: &["echo", "cd", "set", "source", "trap", "unset"],
    camel_is_type: false,
    ..DEFAULT
};

const SQL: Syntax = Syntax {
    line_comment: &["--"],
    keywords: &[
        "ALTER", "AND", "AS", "ASC", "BY", "CASE", "CREATE", "DELETE", "DESC", "DISTINCT", "DROP",
        "ELSE", "END", "EXISTS", "FROM", "GROUP", "HAVING", "IN", "INDEX", "INNER", "INSERT",
        "INTO", "JOIN", "LEFT", "LIKE", "LIMIT", "NOT", "NULL", "ON", "OR", "ORDER", "OUTER",
        "SELECT", "SET", "TABLE", "THEN", "UNION", "UPDATE", "VALUES", "VIEW", "WHEN", "WHERE",
        "WITH",
    ],
    camel_is_type: false,
    ..DEFAULT
};

const CSS: Syntax = Syntax {
    keywords: &[
        "@import",
        "@media",
        "@mixin",
        "@include",
        "@use",
        "@extend",
        "@keyframes",
        "@function",
        "@return",
        "!important",
    ],
    camel_is_type: false,
    ..DEFAULT
};

const SLINT: Syntax = Syntax {
    quotes: &['"'],
    keywords: &[
        "animate",
        "callback",
        "component",
        "export",
        "for",
        "function",
        "global",
        "if",
        "import",
        "in",
        "in-out",
        "inherits",
        "out",
        "property",
        "private",
        "public",
        "pure",
        "return",
        "states",
        "struct",
        "enum",
        "transitions",
        "true",
        "false",
    ],
    types: &[
        "angle",
        "bool",
        "brush",
        "color",
        "duration",
        "float",
        "image",
        "int",
        "length",
        "percent",
        "physical-length",
        "string",
    ],
    ..DEFAULT
};

const HTML: Syntax = Syntax {
    line_comment: &[],
    block_comment: Some(("<!--", "-->")),
    markup: true,
    camel_is_type: false,
    ..DEFAULT
};

/// The extensions this pane kind claims. Anything not here stays a plain
/// [`crate::viewpane`] file viewer, which is the conservative half of the trade: an
/// unrecognised extension renders exactly as it always did rather than being guessed at.
///
/// `json`, `yaml` and `toml` are deliberately absent — they are data, not code, and get a
/// structured tree of their own rather than a second-best colouring here.
fn syntax_for(ext: &str) -> Option<&'static Syntax> {
    Some(match ext {
        "rs" => &RUST,
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => &JS,
        "swift" => &SWIFT,
        "kt" | "kts" => &KOTLIN,
        "java" => &JAVA,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "m" | "mm" => &C,
        "go" => &GO,
        "dart" => &DART,
        "py" | "pyi" => &PYTHON,
        "sh" | "bash" | "zsh" | "fish" => &SHELL,
        "sql" => &SQL,
        "css" | "scss" | "sass" | "less" => &CSS,
        "slint" => &SLINT,
        "html" | "htm" | "xml" | "svg" | "vue" | "svelte" => &HTML,
        _ => return None,
    })
}

/// Whether a file gets the coloured viewer. The one question the rest of the app asks this
/// module before it decides a pane's kind.
pub fn is_source(ext: &str) -> bool {
    syntax_for(ext).is_some()
}

/// What a line left open for the next one. A string that ends without its closing quote is
/// a broken line in most languages, but a block comment, a Python triple-quote and a
/// JavaScript template literal are all *meant* to span lines — so colouring a file one line
/// at a time only works if that much travels forwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Carry {
    #[default]
    None,
    /// Inside a block comment, at this nesting depth (always 1 where the language does not
    /// nest, so one `*/` closes it).
    Block(u16),
    /// Inside a `"""` or `'''`, remembering which.
    Triple(char),
    /// Inside a backtick template literal.
    Template,
}

/// A half-open byte range of the line and what it is. Contiguous and in order, so the whole
/// line is covered exactly once.
type Span = (usize, usize, Class);

/// Colours one file's worth of lines, returning the markup for each in order.
///
/// Takes the whole slice rather than one line because [`Carry`] has to thread through them;
/// a per-line entry point would quietly mis-colour every multi-line string in the file.
pub fn markup_lines(lines: &[&str], ext: &str, palette: &UiPalette) -> Option<Vec<String>> {
    let syn = syntax_for(ext)?;
    let mut carry = Carry::None;
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let (spans, next) = scan(line, syn, carry);
        carry = next;
        out.push(emit(line, &spans, palette));
    }
    Some(out)
}

/// One line escaped for the markdown channel but left uncoloured.
///
/// What a source pane draws when the file's extension has no syntax — reachable when a file
/// is renamed under a live pane, since [`is_source`] is what routed it there in the first
/// place. `emit` builds the body out of the spans, so the span has to cover the line: with
/// an empty slice it would emit the indent and drop the code. `Class::Plain` inks to `None`,
/// so what comes out is the same escaping with no `<font>` around it. Colour is the
/// decoration here; losing the line itself would be the defect.
pub fn plain_markup(line: &str, palette: &UiPalette) -> String {
    emit(line, &[(0, line.len(), Class::Plain)], palette)
}

/// Tokenises one line, given what the line before it left open.
fn scan(line: &str, syn: &Syntax, carry: Carry) -> (Vec<Span>, Carry) {
    let ch: Vec<(usize, char)> = line.char_indices().collect();
    let n = ch.len();
    let end = line.len();
    // Byte offset of character `k`, or the end of the line — every span boundary goes
    // through this so an offset is never a character index by accident. The colour spans
    // Slint receives are byte offsets, and a line with one non-ASCII character in it would
    // otherwise slide every colour after it.
    let at = |k: usize| if k < n { ch[k].0 } else { end };
    let mut spans: Vec<Span> = Vec::new();
    let mut i = 0usize;
    let mut carry = carry;

    // Whatever the previous line left open is closed first, and it owns the start of this
    // line whether or not it is code.
    match carry {
        Carry::Block(depth) => {
            let (stop, left) = close_block(&ch, 0, syn, depth);
            spans.push((0, at(stop), Class::Comment));
            i = stop;
            carry = left;
        }
        Carry::Triple(q) => {
            let (stop, open) = close_triple(&ch, 0, q);
            spans.push((0, at(stop), Class::Str));
            i = stop;
            carry = if open { Carry::Triple(q) } else { Carry::None };
        }
        Carry::Template => {
            let (stop, open) = close_template(&ch, 0);
            spans.push((0, at(stop), Class::Str));
            i = stop;
            carry = if open { Carry::Template } else { Carry::None };
        }
        Carry::None => {}
    }

    while i < n {
        let c = ch[i].1;

        if c.is_whitespace() {
            let s = i;
            while i < n && ch[i].1.is_whitespace() {
                i += 1;
            }
            spans.push((at(s), at(i), Class::Plain));
            continue;
        }

        if let Some(len) = match_any(&ch, i, syn.line_comment) {
            let _ = len;
            spans.push((at(i), end, Class::Comment));
            i = n;
            continue;
        }

        if let Some((open, _)) = syn.block_comment {
            if match_at(&ch, i, open).is_some() {
                let s = i;
                let (stop, left) = close_block(&ch, i + open.chars().count(), syn, 1);
                spans.push((at(s), at(stop), Class::Comment));
                i = stop;
                carry = left;
                continue;
            }
        }

        if syn.markup {
            // Markup has no keywords; it has tags. `<name` and `</name` colour as one, the
            // attribute names inside the tag take the type ink, and everything between tags
            // is text — which is the whole reason this branch exists rather than a keyword
            // list that would light up nothing in an average HTML file.
            if c == '<' {
                let s = i;
                i += 1;
                if i < n && ch[i].1 == '/' {
                    i += 1;
                }
                while i < n && (ch[i].1.is_alphanumeric() || "-_:.".contains(ch[i].1)) {
                    i += 1;
                }
                spans.push((at(s), at(i), Class::Keyword));
                continue;
            }
            if c == '>' || c == '/' || c == '=' {
                spans.push((at(i), at(i + 1), Class::Punct));
                i += 1;
                continue;
            }
        }

        if syn.quotes.contains(&c) {
            if syn.triple && is_triple(&ch, i, c) {
                let s = i;
                let (stop, open) = close_triple(&ch, i + 3, c);
                spans.push((at(s), at(stop), Class::Str));
                i = stop;
                carry = if open { Carry::Triple(c) } else { Carry::None };
                continue;
            }
            if syn.template && c == '`' {
                let s = i;
                let (stop, open) = close_template(&ch, i + 1);
                spans.push((at(s), at(stop), Class::Str));
                i = stop;
                carry = if open { Carry::Template } else { Carry::None };
                continue;
            }
            // A Rust lifetime looks exactly like a char literal that forgot to close. Tell
            // them apart the way the eye does: a char literal is one character (or one
            // escape) and then another quote.
            if syn.lifetimes && c == '\'' && !is_char_literal(&ch, i) {
                let s = i;
                i += 1;
                while i < n && (ch[i].1.is_alphanumeric() || ch[i].1 == '_') {
                    i += 1;
                }
                spans.push((at(s), at(i), Class::Type));
                continue;
            }
            let s = i;
            i = close_quote(&ch, i + 1, c);
            spans.push((at(s), at(i), Class::Str));
            continue;
        }

        if c.is_ascii_digit() {
            let s = i;
            while i < n && (ch[i].1.is_alphanumeric() || ch[i].1 == '_' || ch[i].1 == '.') {
                i += 1;
            }
            spans.push((at(s), at(i), Class::Number));
            continue;
        }

        // `@media` and `!important` are one token each in CSS, so a word is allowed to
        // start with a sigil — but only where the language actually has a keyword shaped
        // that way, or every `@` and `!` in C-like code would swallow the word behind it.
        let sigil = (c == '@' || c == '!') && match_any(&ch, i, syn.keywords).is_some();
        if sigil || c.is_alphabetic() || c == '_' {
            let s = i;
            if sigil {
                i += 1;
            }
            while i < n
                && (ch[i].1.is_alphanumeric() || ch[i].1 == '_' || (ch[i].1 == '-' && syn.markup))
            {
                i += 1;
            }
            let word: String = ch[s..i].iter().map(|(_, c)| *c).collect();
            let class = if syn.keywords.iter().any(|k| *k == word) {
                Class::Keyword
            } else if syn.types.iter().any(|t| *t == word) {
                Class::Type
            } else if syn.markup {
                // Inside a tag every bare word is an attribute name; outside one it is
                // prose. Distinguishing the two would need the scanner to track tag depth,
                // and mis-tinting prose is the more visible of the two mistakes.
                Class::Plain
            } else if syn.camel_is_type && starts_upper(&word) {
                Class::Type
            } else {
                Class::Plain
            };
            spans.push((at(s), at(i), class));
            continue;
        }

        spans.push((at(i), at(i + 1), Class::Punct));
        i += 1;
    }

    (spans, carry)
}

/// True for `Word`; false for `WORD` and for a bare `T`. An all-caps identifier is a
/// constant in every language here, and a lone capital is as often a loop variable as a
/// generic parameter — both are cases where guessing "type" is the more visible mistake.
fn starts_upper(w: &str) -> bool {
    let mut cs = w.chars();
    match cs.next() {
        Some(c) if c.is_uppercase() => cs.any(|c| c.is_lowercase()),
        _ => false,
    }
}

fn match_at(ch: &[(usize, char)], i: usize, pat: &str) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    if i + p.len() > ch.len() {
        return None;
    }
    (0..p.len()).all(|k| ch[i + k].1 == p[k]).then_some(p.len())
}

fn match_any(ch: &[(usize, char)], i: usize, pats: &[&str]) -> Option<usize> {
    pats.iter().find_map(|p| match_at(ch, i, p))
}

/// Scans to the end of a block comment from `i`, returning where it stopped and what is
/// still open. `depth` is what is already open when the scan starts.
fn close_block(ch: &[(usize, char)], mut i: usize, syn: &Syntax, mut depth: u16) -> (usize, Carry) {
    let (open, close) = match syn.block_comment {
        Some(p) => p,
        None => return (ch.len(), Carry::None),
    };
    while i < ch.len() {
        if let Some(l) = match_at(ch, i, close) {
            i += l;
            depth -= 1;
            if depth == 0 {
                return (i, Carry::None);
            }
            continue;
        }
        if syn.nest_block {
            if let Some(l) = match_at(ch, i, open) {
                i += l;
                depth += 1;
                continue;
            }
        }
        i += 1;
    }
    (ch.len(), Carry::Block(depth))
}

fn is_triple(ch: &[(usize, char)], i: usize, q: char) -> bool {
    ch.len() >= i + 3 && ch[i + 1].1 == q && ch[i + 2].1 == q
}

/// Scans to the closing `qqq`; the bool says whether it is still open at end of line.
fn close_triple(ch: &[(usize, char)], mut i: usize, q: char) -> (usize, bool) {
    while i < ch.len() {
        if ch[i].1 == '\\' {
            i += 2;
            continue;
        }
        if ch[i].1 == q && is_triple(ch, i, q) {
            return (i + 3, false);
        }
        i += 1;
    }
    (ch.len(), true)
}

fn close_template(ch: &[(usize, char)], mut i: usize) -> (usize, bool) {
    while i < ch.len() {
        if ch[i].1 == '\\' {
            i += 2;
            continue;
        }
        if ch[i].1 == '`' {
            return (i + 1, false);
        }
        i += 1;
    }
    (ch.len(), true)
}

/// Scans to the matching quote. An unterminated one simply runs to end of line and does not
/// carry: outside the three multi-line forms above, a quote that reaches the newline is a
/// syntax error in the file, and letting it carry would paint the rest of the file green.
fn close_quote(ch: &[(usize, char)], mut i: usize, q: char) -> usize {
    while i < ch.len() {
        if ch[i].1 == '\\' {
            i += 2;
            continue;
        }
        if ch[i].1 == q {
            return i + 1;
        }
        i += 1;
    }
    ch.len()
}

/// `'a'` and `'\n'` are char literals; `'a` on its own is a lifetime.
fn is_char_literal(ch: &[(usize, char)], i: usize) -> bool {
    match ch.get(i + 1).map(|(_, c)| *c) {
        Some('\\') => true,
        Some(_) => ch.get(i + 2).map(|(_, c)| *c) == Some('\''),
        None => false,
    }
}

/// Every character `from_markdown` would read as syntax. Escaped with a backslash, which it
/// accepts for all of them and strips back off, so the text round-trips byte for byte.
const META: &str = "\\`*_{}[]()#+-.!<>&\"'~|";

/// One tab, as the four non-breaking spaces that survive the markdown round trip.
const TAB: &str = "\u{a0}\u{a0}\u{a0}\u{a0}";

/// Renders one line as markdown markup: indentation as NBSP, everything escaped, and a
/// `<font>` around each run that is not plain.
fn emit(line: &str, spans: &[Span], palette: &UiPalette) -> String {
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    let mut out = String::with_capacity(line.len() * 2);
    // U+00A0 rather than a space: markdown eats a leading run of spaces (and reads four of
    // them as a code block, which is a hard parse error), and indentation is not decoration
    // in a source file — losing it loses the structure the reader came for.
    for c in line[..indent].chars() {
        out.push_str(if c == '\t' { TAB } else { "\u{a0}" });
    }

    let mut prev: Option<Class> = None;
    for (s, e, class) in spans.iter().copied() {
        let s = s.max(indent);
        if s >= e {
            continue;
        }
        // Runs of the same class are merged rather than re-opened, because a `<font>` tag
        // per token would triple the size of a dense line for no visible difference.
        if prev != Some(class) {
            if prev.map(|p| p.ink(palette).is_some()).unwrap_or(false) {
                out.push_str("</font>");
            }
            if let Some(argb) = class.ink(palette) {
                out.push_str(&format!("<font color=\"#{:06x}\">", argb & 0x00ff_ffff));
            }
            prev = Some(class);
        }
        for c in line[s..e].chars() {
            // A tab anywhere, not just in the indent: markdown's own handling of one is not
            // something this module should be betting a Go file's alignment on.
            if c == '\t' {
                out.push_str(TAB);
                continue;
            }
            if META.contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
    }
    if prev.map(|p| p.ink(palette).is_some()).unwrap_or(false) {
        out.push_str("</font>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ui_palette;
    use i_slint_core::styled_text::get_raw_text;

    /// What the reader would have to be able to type to break this. Each line carries one
    /// hazard the markdown channel is known to mangle.
    const CORPUS: &[(&str, &str)] = &[
        ("rs", "    let x: Vec<String> = vec![]; // a *starred* note"),
        ("rs", "\tlet s = \"a #hash and a [bracket](x)\";"),
        (
            "rs",
            "fn f<'a>(v: &'a str) -> Option<&'a str> { v.get(0..1) }",
        ),
        ("rs", "let n = 0xff_u32 + 1.5e3 - 2;"),
        ("ts", "const t = `a ${x} b`; // 100% _fine_"),
        ("py", "def f(a=1, *args, **kw):  # trailing"),
        ("py", "    return {'k': \"v\"}"),
        ("sh", "if [ -n \"$1\" ]; then echo \"hi ~ | & <>\"; fi"),
        (
            "html",
            "<div class=\"a-b\" data-x='1'>text &amp; more</div>",
        ),
        ("sql", "SELECT a.b FROM t WHERE x LIKE '%y%' -- note"),
        (
            "css",
            "@media (min-width: 40px) { .a > .b { color: #fff !important; } }",
        ),
        (
            "go",
            "\t\tif err != nil { return fmt.Errorf(\"x: %w\", err) }",
        ),
        ("rs", ""),
        ("rs", "        "),
    ];

    /// Put the leading NBSP back to spaces. It cannot put a tab back: `emit` renders one as
    /// four columns and four spaces as four columns, and by design they are the same thing on
    /// screen — so the comparison is against [`detab`] of the source, not the source itself.
    fn unpad(s: &str) -> String {
        s.replace('\u{a0}', " ")
    }

    /// A tab as the reader sees it. Four, matching [`TAB`]; a source viewer that renders a tab
    /// as one column would collapse every Go file's alignment.
    fn detab(s: &str) -> String {
        s.replace('\t', "    ")
    }

    /// The claim the whole module rests on: what Slint parses back out of the markup is the
    /// source line, character for character. Nothing else here matters if this is false —
    /// a viewer that quietly drops a `*` or eats an indent is worse than an uncoloured one.
    ///
    /// `from_markdown` returning `Err` is the sharper half of the assertion:
    /// `viewpane::markdown_text` falls back to plain text on error, so one unescaped
    /// character would not fail loudly, it would silently strip every colour on that row.
    #[test]
    fn markup_reproduces_the_source_line_exactly() {
        let p = ui_palette(0);
        for (ext, line) in CORPUS {
            let m = markup_lines(&[line], ext, &p).expect("a known extension");
            let st = slint::StyledText::from_markdown(&m[0])
                .unwrap_or_else(|e| panic!("{ext}: {line:?} -> {:?}\n{e:?}", m[0]));
            let got = unpad(&get_raw_text(&st));
            let want = detab(line);
            assert_eq!(
                got.trim_end(),
                want.trim_end(),
                "{ext}: {line:?} became {:?}",
                m[0]
            );
        }
    }

    /// Indentation is structure, not decoration. Four leading spaces are a markdown code
    /// block (a hard parse error) and two are silently eaten, so the leading run has to
    /// leave as NBSP — and come back as the same number of columns.
    #[test]
    fn indentation_survives() {
        let p = ui_palette(0);
        for (src, want) in [("    x", "    x"), ("\tx", "    x"), ("  x", "  x")] {
            let m = markup_lines(&[src], "rs", &p).unwrap();
            let st = slint::StyledText::from_markdown(&m[0]).unwrap();
            assert_eq!(unpad(&get_raw_text(&st)), want, "from {src:?}");
        }
    }

    /// The reason [`markup_lines`] takes the whole file rather than one line: a block
    /// comment, a Python triple-quote and a JS template all outlive the line they open on,
    /// and a per-line entry point would re-colour their contents as code.
    #[test]
    fn multi_line_forms_carry() {
        let p = ui_palette(0);
        let comment = format!("{:06x}", p.faint & 0x00ff_ffff);
        let m = markup_lines(&["/* one", "two", "three */ let x = 1;"], "rs", &p).unwrap();
        for row in &m[..2] {
            assert!(row.contains(&comment), "{row:?} should be comment ink");
        }
        assert!(
            m[2].contains(&format!("{:06x}", p.accent & 0x00ff_ffff)),
            "the keyword after the close is a keyword again: {:?}",
            m[2]
        );

        let py = markup_lines(&["s = '''one", "two'''", "x = 1"], "py", &p).unwrap();
        let strink = format!("{:06x}", p.ok & 0x00ff_ffff);
        assert!(
            py[1].contains(&strink),
            "still inside the triple quote: {:?}",
            py[1]
        );
        assert!(
            !py[2].contains(&strink),
            "it closed on the line before: {:?}",
            py[2]
        );
    }

    /// `'a` is a lifetime; `'a'` is a char. Reading the first as an unterminated string
    /// would paint the rest of every generic Rust signature green.
    #[test]
    fn a_lifetime_is_not_a_string() {
        let p = ui_palette(0);
        let strink = format!("{:06x}", p.ok & 0x00ff_ffff);
        let m = markup_lines(&["fn f<'a>(x: &'a str) {}"], "rs", &p).unwrap();
        assert!(
            !m[0].contains(&strink),
            "no string ink on a lifetime: {:?}",
            m[0]
        );
        let m = markup_lines(&["let c = 'x';"], "rs", &p).unwrap();
        assert!(
            m[0].contains(&strink),
            "a char literal is a string: {:?}",
            m[0]
        );
    }

    /// The palette rule from `ui/theme.slint` — no colour is written anywhere but there —
    /// binds syntax colouring too, or the one thing on screen that stops following the
    /// user's theme is the thing they spend the most time reading.
    #[test]
    fn every_colour_comes_from_the_palette() {
        for idx in 0..5 {
            let p = ui_palette(idx);
            let known: Vec<String> = [p.text, p.subtext, p.faint, p.accent, p.ok, p.warn, p.link]
                .iter()
                .map(|c| format!("#{:06x}", c & 0x00ff_ffff))
                .collect();
            for (ext, line) in CORPUS {
                for row in markup_lines(&[line], ext, &p).unwrap() {
                    for hit in row.match_indices("color=\"").map(|(i, _)| i + 7) {
                        let hex = &row[hit..hit + 7];
                        assert!(known.iter().any(|k| k == hex), "{hex} is not in {}", p.name);
                    }
                }
            }
        }
    }

    /// And the consequence: switching palette really does change the ink. This is what the
    /// view cache's fingerprint has to account for — the markup is baked, so a palette
    /// change that did not re-project would leave the old theme's colours on screen.
    #[test]
    fn a_palette_switch_changes_the_ink() {
        let line = ["fn main() {}"];
        let mocha = markup_lines(&line, "rs", &ui_palette(0)).unwrap();
        let latte = markup_lines(&line, "rs", &ui_palette(3)).unwrap();
        assert_ne!(mocha, latte, "palette 3 must not paint like palette 0");
    }

    /// An unknown extension is not guessed at: it stays a plain viewer, rendering exactly
    /// as it did before this module existed.
    #[test]
    fn only_known_extensions_are_source() {
        for ext in [
            "rs", "ts", "tsx", "py", "html", "sh", "swift", "kt", "scss", "slint",
        ] {
            assert!(is_source(ext), "{ext} should be highlighted");
        }
        // json/yaml/toml are data, and get a structured view of their own rather than a
        // second-best colouring here.
        for ext in ["json", "yaml", "toml", "png", "csv", "", "md"] {
            assert!(!is_source(ext), "{ext} should stay a plain viewer");
        }
    }
    /// The uncoloured path has to be as faithful as the coloured one — same escaping, same
    /// indentation, just no ink. A blank result here is the bug it exists to prevent.
    #[test]
    fn an_uncoloured_line_still_carries_the_line() {
        let pal = crate::theme::ui_palette(0);
        for src in [
            "let x = *p; // a [note]",
            "\tif (a && b) { return 1; }",
            "    plain.indented(\"text\")",
            "",
        ] {
            let out = plain_markup(src, &pal);
            assert!(!out.contains("<font"), "no syntax means no ink: {out:?}");
            assert_eq!(
                detab(&unpad(&out.replace('\\', ""))),
                detab(src),
                "the line did not survive escaping"
            );
        }
    }
}
