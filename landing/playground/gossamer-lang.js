// CodeMirror 6 StreamLanguage tokenizer + highlight style for Gossamer.
// Pragmatic regex/state tokenizer (not a full parser): it covers the
// surface tokens the playground needs - comments, keywords, types,
// strings and interpolated strings (whose placeholders highlight as code),
// numbers, the |> pipe, the _ pattern, and #! comments.

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
  "use", "const", "static", "spawn", "defer", "arena", "cohort", "comptime",
  "newtype", "pub", "as", "in", "where", "select", "self", "type", "mod",
]);

const BUILTIN_TYPES = new Set([
  "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "usize", "isize",
  "f32", "f64", "bool", "char", "String", "Option", "Result", "Vec",
  "Map", "Set", "BTreeMap", "BTreeSet", "Deque", "Queue", "Stack", "MinHeap",
  "MaxHeap",
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

/// The innermost open interpolated string, or `undefined` outside one.
function openInterpolation(state) {
  return state.interpolations[state.interpolations.length - 1];
}

/// Tokenize inside an interpolated string whose literal text is next:
/// text up to a placeholder or the closing quote is one string token, a
/// placeholder's `{` opens code, and the closing quote pops the frame.
function interpolatedText(stream, state, frame) {
  const close = frame.triple ? '"""' : '"';
  if (stream.match(close)) {
    state.interpolations.pop();
    return "string";
  }
  if (stream.match("{{") || stream.match("}}")) return "string";
  if (stream.peek() === "{") {
    stream.next();
    frame.inCode = true;
    frame.depth = 0;
    return "meta";
  }
  while (!stream.eol()) {
    if (stream.match(close, false) || stream.match("{{", false) || stream.match("}}", false)) {
      break;
    }
    if (stream.peek() === "{") break;
    if (stream.peek() === "\\") stream.next();
    stream.next();
  }
  return "string";
}

const gossamerStreamParser = {
  name: "gossamer",

  startState() {
    return { inBlockComment: false, inTripleString: false, interpolations: [] };
  },

  copyState(state) {
    return {
      inBlockComment: state.inBlockComment,
      inTripleString: state.inTripleString,
      interpolations: state.interpolations.map((frame) => ({ ...frame })),
    };
  },

  token(stream, state) {
    const frame = openInterpolation(state);
    if (frame && !frame.inCode) {
      return interpolatedText(stream, state, frame);
    }
    if (frame && frame.depth === 0) {
      // A placeholder closes at its own `}`; a single `:` starts its spec.
      if (stream.peek() === "}") {
        stream.next();
        frame.inCode = false;
        return "meta";
      }
      if (stream.peek() === ":" && !stream.match("::", false)) {
        while (!stream.eol() && stream.peek() !== "}") stream.next();
        return "meta";
      }
    }
    const style = tokenCode(stream, state);
    if (frame && frame === openInterpolation(state)) {
      const text = stream.current();
      if (text === "{" || text === "(" || text === "[") frame.depth += 1;
      if (text === "}" || text === ")" || text === "]") frame.depth = Math.max(0, frame.depth - 1);
    }
    return style;
  },

  languageData: {
    commentTokens: { line: "//", block: { open: "/*", close: "*/" } },
    closeBrackets: { brackets: ["(", "[", "{", '"'] },
  },
};

/// Tokenize one token of ordinary code.
function tokenCode(stream, state) {
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

    // Interpolated string: its literal text and placeholders tokenize in
    // turn until its closing quote pops the frame.
    if (ch === "f" && (stream.match('f"""') || stream.match('f"'))) {
      state.interpolations.push({
        triple: stream.current().length === 4,
        inCode: false,
        depth: 0,
      });
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
}

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
