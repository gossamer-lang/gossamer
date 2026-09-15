// CodeMirror 6 StreamLanguage tokenizer + highlight style for Gossamer.
// Pragmatic regex/state tokenizer (not a full parser): it covers the
// surface tokens the playground needs - comments, keywords, types,
// strings, numbers, the |> pipe, _ placeholder, format-macros, and
// #! comments.

import {
  StreamLanguage,
  LanguageSupport,
  HighlightStyle,
  syntaxHighlighting,
} from "https://esm.sh/@codemirror/language@6";
import { tags as t } from "https://esm.sh/@lezer/highlight@1";

const KEYWORDS = new Set([
  "let", "mut", "fn", "if", "else", "match", "for", "while", "loop",
  "return", "break", "continue", "struct", "enum", "trait", "impl",
  "use", "const", "static", "go", "spawn", "defer", "arena", "pub",
  "move", "as", "in", "where", "select", "self",
]);

const BUILTIN_TYPES = new Set([
  "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "usize", "isize",
  "f32", "f64", "bool", "char", "String", "Option", "Result", "Vec",
  "HashMap",
]);

const MACROS = new Set([
  "println", "print", "eprintln", "eprint", "format", "panic",
]);

const NUM_SUFFIX = /^(?:i8|i16|i32|i64|u8|u16|u32|u64|usize|isize|f32|f64)/;

const MULTI_OP =
  /^(?:\|>|<<=|>>=|\.\.=|->|=>|::|==|!=|<=|>=|&&|\|\||\.\.|\+%=|-%=|\*%=|\+%|-%|\*%|\+=|-=|\*=|\/=|%=|&=|\|=|\^=|<<|>>)/;
const SINGLE_OP = /^[-+*/%=<>!&|^~?@]/;
const IDENT = /^[A-Za-z_¡-￿][A-Za-z0-9_¡-￿]*/;

/// Consume the remainder of an open block comment, clearing the
/// flag when its `*/` terminator is reached on this line.
function consumeBlockComment(stream, state) {
  while (!stream.eol()) {
    if (stream.match("*/")) {
      state.inBlockComment = false;
      return;
    }
    stream.next();
  }
}

/// Consume the remainder of an open triple-quoted string, clearing the
/// flag when its closing `"""` is reached on this line. An escape
/// consumes the character after it, so `\"""` does not close.
function consumeTripleString(stream, state) {
  while (!stream.eol()) {
    if (stream.match('"""')) {
      state.inTripleString = false;
      return;
    }
    if (stream.peek() === "\\") {
      stream.next();
    }
    stream.next();
  }
}

const gossamerStreamParser = {
  name: "gossamer",

  startState() {
    return { inBlockComment: false, inTripleString: false };
  },

  token(stream, state) {
    if (state.inBlockComment) {
      consumeBlockComment(stream, state);
      return "comment";
    }

    if (state.inTripleString) {
      consumeTripleString(stream, state);
      return "string";
    }

    if (stream.eatSpace()) return null;

    // Line comment.
    if (stream.match("//")) {
      stream.skipToEnd();
      return "comment";
    }

    // Hash-bang comment. A bare # is collection syntax, and #[...]
    // should stay punctuation-coloured like ordinary brackets.
    if (stream.match("#!")) {
      stream.skipToEnd();
      return "comment";
    }

    // Block comment (non-nesting).
    if (stream.match("/*")) {
      state.inBlockComment = true;
      consumeBlockComment(stream, state);
      return "comment";
    }

    const ch = stream.peek();

    // Char literal: 'c' or '\n'. Gossamer has no lifetimes, so a lone
    // quote is always a character literal.
    if (ch === "'") {
      if (stream.match(/^'(?:\\.|[^'\\])'/)) return "string";
      stream.next();
      return "string";
    }

    // Triple-quoted string: spans lines, so its open state carries
    // across token calls the way a block comment's does.
    if (ch === '"' && stream.match('\"\"\"')) {
      state.inTripleString = true;
      consumeTripleString(stream, state);
      return "string";
    }

    // Double-quoted string with escapes.
    if (ch === '"') {
      stream.next();
      let escaped = false;
      let c;
      while ((c = stream.next()) != null) {
        if (c === '"' && !escaped) break;
        escaped = !escaped && c === "\\";
      }
      return "string";
    }

    // Numbers: hex / binary / octal / decimal-float, optional type suffix.
    if (ch >= "0" && ch <= "9") {
      if (
        stream.match(/^0x[0-9a-fA-F_]+/) ||
        stream.match(/^0b[01_]+/) ||
        stream.match(/^0o[0-7_]+/) ||
        stream.match(/^\d[\d_]*(?:\.[\d_]+)?(?:[eE][+-]?\d[\d_]*)?/)
      ) {
        stream.match(NUM_SUFFIX);
        return "number";
      }
    }

    // Identifiers / keywords / types / macros / booleans.
    if (stream.match(IDENT)) {
      const word = stream.current();
      if (stream.peek() === "!" && MACROS.has(word)) {
        stream.next();
        return "macroName";
      }
      if (word === "true" || word === "false") return "bool";
      if (KEYWORDS.has(word)) return "keyword";
      if (word === "_") return "keyword";
      if (BUILTIN_TYPES.has(word)) return "typeName";
      if (/^[A-Z]/.test(word)) return "typeName";
      return "variableName";
    }

    // Operators (|> and the rest), longest first.
    if (stream.match(MULTI_OP)) return "operator";
    if (stream.match(SINGLE_OP)) return "operator";

    // Punctuation and anything else - consume one char, leave unstyled.
    stream.next();
    return null;
  },

  languageData: {
    commentTokens: { line: "//", block: { open: "/*", close: "*/" } },
    closeBrackets: { brackets: ["(", "[", "{", '"'] },
  },
};

/// The Gossamer stream language (no highlighting attached).
export const gossamerLanguage = StreamLanguage.define(gossamerStreamParser);

/// Refined-dark highlight style matching the Gossamer landing palette.
export const gossamerHighlightStyle = HighlightStyle.define([
  { tag: t.comment, color: "#6b7280", fontStyle: "italic" },
  { tag: t.lineComment, color: "#6b7280", fontStyle: "italic" },
  { tag: t.blockComment, color: "#6b7280", fontStyle: "italic" },
  { tag: t.keyword, color: "#38bdf8" },
  { tag: t.operator, color: "#7dd3fc" },
  { tag: t.typeName, color: "#7dd3fc" },
  { tag: t.string, color: "#86efac" },
  { tag: t.number, color: "#e5a663" },
  { tag: t.bool, color: "#fbbf24" },
  { tag: t.macroName, color: "#c4b5fd" },
  { tag: t.meta, color: "#9ca3af" },
  { tag: t.variableName, color: "#f3f4f6" },
]);

/// CodeMirror `LanguageSupport` for Gossamer with the refined-dark
/// highlight style bundled as support extension.
export function gossamer() {
  return new LanguageSupport(gossamerLanguage, [
    syntaxHighlighting(gossamerHighlightStyle),
  ]);
}

export default gossamer;
