// "Try Gossamer" interactive code playground - a self-contained,
// dependency-light web component. Renders a CodeMirror 6 editor with
// Run / Reset controls and an output pane, and talks to a runtime that
// is lazy-loaded via dynamic import (the stub by default, the real wasm
// module when one is dropped in alongside this file).
//
// Public API:
//   mountPlayground(el, {
//     source,        // initial Gossamer source (required)
//     autorun,       // run once on mount (default false)
//     height,        // editor height, e.g. "320px" (default: auto)
//     editable,      // allow editing (default true)
//     runtimeUrl,    // runtime module URL (default "./runtime-stub.js")
//   }) -> { run, reset, getSource, destroy, view }

import { EditorView, basicSetup } from "https://esm.sh/codemirror@6.0.1";
import { EditorState, Prec } from "https://esm.sh/@codemirror/state@6";
import { keymap } from "https://esm.sh/@codemirror/view@6";
import { indentWithTab } from "https://esm.sh/@codemirror/commands@6";
import { gossamer } from "./gossamer-lang.js";

// Prefer the real wasm VM; fall back to the JS stub when it has not been
// deployed yet (the wasm module is built and dropped in by CI).
const DEFAULT_RUNTIME_URL = "./gossamer_playground.js";
const STUB_RUNTIME_URL = new URL("./runtime-stub.js", import.meta.url).href;

// One cached load per runtime URL for the whole page session. A failed
// load is evicted so a later Run can re-attempt.
const runtimeCache = new Map();

async function importRuntime(url) {
  const mod = await import(url);
  if (typeof mod.default === "function") {
    await mod.default();
  }
  if (typeof mod.run !== "function") {
    throw new Error("runtime module has no run(source) export");
  }
  return mod;
}

/// Lazy-load and initialize a runtime module, caching the instance. When
/// the requested module is absent (the wasm build is not deployed yet),
/// fall back to the JS stub so the playground UI still works.
function loadRuntime(url) {
  let pending = runtimeCache.get(url);
  if (!pending) {
    pending = importRuntime(url)
      .catch((err) => {
        if (url !== STUB_RUNTIME_URL) return importRuntime(STUB_RUNTIME_URL);
        throw err;
      })
      .catch((err) => {
        runtimeCache.delete(url);
        throw err;
      });
    runtimeCache.set(url, pending);
  }
  return pending;
}

const editorTheme = EditorView.theme(
  {
    "&": {
      color: "#f3f4f6",
      backgroundColor: "#0f1115",
      fontSize: "0.9rem",
    },
    ".cm-content": {
      fontFamily:
        '"JetBrains Mono", "Fira Code", Menlo, Consolas, monospace',
      caretColor: "#38bdf8",
      padding: "0.7rem 0",
    },
    ".cm-scroller": {
      fontFamily:
        '"JetBrains Mono", "Fira Code", Menlo, Consolas, monospace',
      lineHeight: "1.6",
    },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "#38bdf8" },
    "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection":
      { backgroundColor: "rgba(56, 189, 248, 0.25)" },
    ".cm-gutters": {
      backgroundColor: "#0b0d11",
      color: "#4b5563",
      border: "none",
      borderRight: "1px solid #1f2937",
    },
    ".cm-activeLineGutter": {
      backgroundColor: "rgba(56, 189, 248, 0.07)",
      color: "#9ca3af",
    },
    ".cm-activeLine": { backgroundColor: "rgba(56, 189, 248, 0.04)" },
    ".cm-lineNumbers .cm-gutterElement": {
      padding: "0 0.5rem 0 0.85rem",
    },
    ".cm-matchingBracket": {
      backgroundColor: "rgba(56, 189, 248, 0.18)",
      outline: "1px solid rgba(56, 189, 248, 0.4)",
    },
    ".cm-selectionMatch": { backgroundColor: "rgba(125, 211, 252, 0.12)" },
  },
  { dark: true },
);

/// Append a coloured text segment to the output pane.
function appendSegment(parent, text, className) {
  const span = document.createElement("span");
  span.className = className;
  span.textContent = text;
  parent.appendChild(span);
}

/// Render a run result into the output pane.
function renderOutput(out, result) {
  out.replaceChildren();
  let any = false;
  if (result.stdout) {
    appendSegment(out, result.stdout, "gp-stdout");
    any = true;
  }
  if (result.stderr) {
    appendSegment(out, result.stderr, "gp-stderr");
    any = true;
  }
  if (result.error) {
    const text = String(result.error);
    appendSegment(out, text.endsWith("\n") ? text : text + "\n", "gp-error");
    any = true;
  }
  if (!any) {
    appendSegment(out, "(no output)\n", "gp-empty");
  }
}

/// Convert a JavaScript exception from the wasm module into an actionable
/// playground message. wasm32 has no unwinder, so a Rust panic ends the
/// module with a bare `RuntimeError: unreachable`; the runtime records the
/// panic's own message first, and `last_panic()` is what makes the report
/// name a cause rather than the trap.
function runtimeThrowMessage(err, rt) {
  const raw = err?.message ?? String(err);
  if (!/unreachable(?: executed)?/i.test(raw)) {
    return "runtime error: " + raw;
  }
  let panic = "";
  try {
    if (rt && typeof rt.last_panic === "function") {
      panic = String(rt.last_panic() || "");
    }
  } catch {
    panic = "";
  }
  return [
    panic
      ? "runtime error: the playground runtime stopped: " + panic
      : "runtime error: the playground runtime trapped while executing this program.",
    "This is an internal bug in the browser build, not a mistake in the program.",
    "If the same source works with `gos run`, please report it with the program text.",
  ].join("\n");
}

/// Render check() diagnostics below the output; hides when empty.
function renderDiagnostics(list, diagnostics) {
  list.replaceChildren();
  if (!Array.isArray(diagnostics) || diagnostics.length === 0) {
    list.hidden = true;
    return;
  }
  for (const d of diagnostics) {
    const li = document.createElement("li");
    const severity = d.severity === "warning" ? "warning" : "error";
    li.className = "gp-diag gp-diag-" + severity;
    const loc =
      Number.isFinite(d.line) && Number.isFinite(d.col)
        ? `${d.line}:${d.col}: `
        : "";
    const code = d.code ? `[${d.code}] ` : "";
    li.textContent = `${severity}: ${code}${loc}${d.message ?? ""}`;
    list.appendChild(li);
  }
  list.hidden = false;
}

/// Mount a playground into `el`. Returns a small control handle.
export function mountPlayground(el, opts = {}) {
  if (!el) throw new Error("mountPlayground: target element is required");

  const originalSource = typeof opts.source === "string" ? opts.source : "";
  const editable = opts.editable !== false;
  const runtimeUrl = new URL(
    opts.runtimeUrl || DEFAULT_RUNTIME_URL,
    import.meta.url,
  ).href;

  // ---- DOM scaffold -------------------------------------------------
  el.replaceChildren();
  const root = document.createElement("div");
  root.className = "gp";

  const toolbar = document.createElement("div");
  toolbar.className = "gp-toolbar";

  const title = document.createElement("span");
  title.className = "gp-title";
  title.textContent = "Gossamer";

  const actions = document.createElement("div");
  actions.className = "gp-actions";

  const runBtn = document.createElement("button");
  runBtn.type = "button";
  runBtn.className = "gp-btn gp-run";
  runBtn.textContent = "Run";
  runBtn.setAttribute("aria-keyshortcuts", "Control+Enter Meta+Enter");

  const resetBtn = document.createElement("button");
  resetBtn.type = "button";
  resetBtn.className = "gp-btn gp-reset";
  resetBtn.textContent = "Reset";

  actions.append(runBtn, resetBtn);
  toolbar.append(title, actions);

  const editorHost = document.createElement("div");
  editorHost.className = "gp-editor";

  const outputWrap = document.createElement("div");
  outputWrap.className = "gp-output-wrap";

  const outputHeader = document.createElement("div");
  outputHeader.className = "gp-output-header";

  const outputLabel = document.createElement("span");
  outputLabel.className = "gp-output-label";
  outputLabel.textContent = "Output:";

  const status = document.createElement("span");
  status.className = "gp-status";
  status.setAttribute("role", "status");
  status.setAttribute("aria-live", "polite");

  outputHeader.append(outputLabel, status);

  const out = document.createElement("pre");
  out.className = "gp-output";
  out.tabIndex = 0;
  out.setAttribute("aria-label", "Program output");

  const diags = document.createElement("ul");
  diags.className = "gp-diags";
  diags.hidden = true;

  outputWrap.append(outputHeader, out, diags);
  root.append(toolbar, editorHost, outputWrap);
  el.appendChild(root);

  // ---- Editor -------------------------------------------------------
  const heightTheme = EditorView.theme({
    "&": opts.height ? { height: opts.height } : {},
    ".cm-scroller": {
      overflow: "auto",
      maxHeight: opts.height ? "none" : "440px",
    },
  });

  const extensions = [
    basicSetup,
    // `basicSetup` carries the default keymap, which binds Mod-Enter to
    // "insert a blank line". The run shortcut has to outrank it, so it is
    // registered at the highest precedence rather than merely earlier in the
    // extension list.
    Prec.highest(
      keymap.of([
        {
          key: "Mod-Enter",
          preventDefault: true,
          run: () => {
            doRun();
            return true;
          },
        },
      ]),
    ),
    keymap.of([indentWithTab]),
    gossamer(),
    editorTheme,
    heightTheme,
  ];
  if (!editable) {
    extensions.push(EditorView.editable.of(false));
    extensions.push(EditorState.readOnly.of(true));
  }

  const view = new EditorView({
    state: EditorState.create({ doc: originalSource, extensions }),
    parent: editorHost,
  });

  // ---- Behaviour ----------------------------------------------------
  let running = false;
  let destroyed = false;

  function setBusy(busy) {
    runBtn.disabled = busy;
    resetBtn.disabled = busy;
    root.setAttribute("aria-busy", busy ? "true" : "false");
  }

  async function doRun() {
    if (running || destroyed) return;
    running = true;
    setBusy(true);
    status.textContent = runtimeCache.has(runtimeUrl)
      ? "Running..."
      : "Loading runtime...";

    try {
      const rt = await loadRuntime(runtimeUrl);
      if (destroyed) return;
      status.textContent = "Running...";

      const source = view.state.doc.toString();

      let result;
      try {
        result = rt.run(source);
      } catch (err) {
        result = {
          stdout: "",
          stderr: "",
          error: runtimeThrowMessage(err, rt),
          fuel_used: 0,
        };
      }
      renderOutput(out, result);

      // Diagnostics are best-effort and never fatal to a run.
      let diagnostics = [];
      try {
        if (typeof rt.check === "function") {
          const checked = rt.check(source);
          if (checked && Array.isArray(checked.diagnostics)) {
            diagnostics = checked.diagnostics;
          }
        }
      } catch {
        diagnostics = [];
      }
      renderDiagnostics(diags, diagnostics);

      // The rendered output below is the completion signal; a successful run
      // leaves the header reading just "Output:" with no redundant status.
      status.textContent = "";
    } catch (err) {
      out.replaceChildren();
      appendSegment(
        out,
        "Failed to load runtime: " + (err?.message ?? String(err)) + "\n",
        "gp-error",
      );
      renderDiagnostics(diags, []);
      status.textContent = "Runtime error";
    } finally {
      running = false;
      if (!destroyed) setBusy(false);
    }
  }

  function doReset() {
    if (destroyed) return;
    view.dispatch({
      changes: {
        from: 0,
        to: view.state.doc.length,
        insert: originalSource,
      },
    });
    out.replaceChildren();
    renderDiagnostics(diags, []);
    status.textContent = "";
    if (editable) view.focus();
  }

  function destroy() {
    destroyed = true;
    view.destroy();
    el.replaceChildren();
  }

  runBtn.addEventListener("click", doRun);
  resetBtn.addEventListener("click", doReset);

  if (opts.autorun) {
    doRun();
  }

  return {
    run: doRun,
    reset: doReset,
    getSource: () => view.state.doc.toString(),
    destroy,
    view,
  };
}

export default mountPlayground;
