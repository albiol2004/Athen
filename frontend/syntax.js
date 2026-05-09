// Tiny offline syntax highlighter for tool-card bodies.
//
// Trade-offs: not a full grammar engine (no nested context, no
// indentation-aware blocks). Just per-language token regexes for
// comments / strings / keywords / numbers / types — enough to give
// Read/Write/Edit/Grep cards a recognisable structure without bringing
// in highlight.js (~24KB) and a vendored CSS theme that'd fight the
// existing flat-luxe palette. Swap-in candidates: highlight.js,
// shiki, prism.js — all drop into `highlightCode(code, lang)` below.
//
// Each language spec uses non-capturing inner groups so the combined
// regex below can wrap each alternative in a single named group.

(function() {
    'use strict';

    // Regex parts for each token kind, ordered by priority. Comments
    // first so `//` doesn't get parsed as keyword-adjacent slashes,
    // strings second so quoted keywords don't highlight, etc.
    const KW_RUST   = 'fn|let|mut|pub|use|mod|struct|enum|impl|trait|for|in|if|else|match|return|self|Self|crate|super|as|where|async|await|move|ref|const|static|type|dyn|true|false|loop|while|break|continue|extern|unsafe|box';
    const KW_JS     = 'function|let|const|var|if|else|for|while|do|return|class|new|this|super|extends|import|export|from|default|async|await|yield|try|catch|finally|throw|typeof|instanceof|in|of|switch|case|break|continue|delete|void|null|undefined|true|false';
    const KW_PY     = 'def|class|if|elif|else|for|while|return|import|from|as|with|try|except|finally|raise|yield|lambda|pass|break|continue|in|is|not|and|or|None|True|False|self|nonlocal|global|async|await';
    const KW_GO     = 'func|var|const|type|struct|interface|package|import|return|if|else|for|range|switch|case|default|break|continue|defer|go|chan|map|select|fallthrough|true|false|nil|iota';
    const KW_JAVA   = 'class|interface|enum|extends|implements|public|private|protected|static|final|abstract|void|int|long|short|byte|float|double|boolean|char|String|new|this|super|return|if|else|for|while|do|switch|case|break|continue|try|catch|finally|throw|throws|import|package|null|true|false';
    const KW_C      = 'int|long|short|char|float|double|void|signed|unsigned|struct|enum|union|typedef|const|static|extern|return|if|else|for|while|do|switch|case|break|continue|sizeof|true|false|NULL';
    const KW_SH     = 'if|then|else|elif|fi|for|while|do|done|in|case|esac|function|return|local|export|echo|cd|set|unset|read|exit|true|false';

    const SYNTAX = {
        rust: {
            comment: /\/\/[^\n]*|\/\*[\s\S]*?\*\//,
            string:  /"(?:[^"\\]|\\.)*"|r#?"(?:[^"]*?)"#?|'(?:[^'\\]|\\.)'/,
            keyword: new RegExp('\\b(?:' + KW_RUST + ')\\b'),
            type:    /\b[A-Z][A-Za-z0-9_]*\b/,
            number:  /\b\d[\d_]*(?:\.\d[\d_]*)?(?:[fiu](?:8|16|32|64|128|size))?\b/,
        },
        javascript: {
            comment: /\/\/[^\n]*|\/\*[\s\S]*?\*\//,
            string:  /"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`/,
            keyword: new RegExp('\\b(?:' + KW_JS + ')\\b'),
            number:  /\b\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?\b/,
        },
        typescript: {
            comment: /\/\/[^\n]*|\/\*[\s\S]*?\*\//,
            string:  /"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`/,
            keyword: new RegExp('\\b(?:' + KW_JS + '|interface|type|enum|public|private|protected|readonly|abstract|implements|namespace|declare|as)\\b'),
            type:    /\b[A-Z][A-Za-z0-9_]*\b/,
            number:  /\b\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?\b/,
        },
        python: {
            comment: /#[^\n]*/,
            string:  /"""[\s\S]*?"""|'''[\s\S]*?'''|"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/,
            keyword: new RegExp('\\b(?:' + KW_PY + ')\\b'),
            number:  /\b\d[\d_]*(?:\.\d[\d_]*)?\b/,
        },
        go: {
            comment: /\/\/[^\n]*|\/\*[\s\S]*?\*\//,
            string:  /"(?:[^"\\]|\\.)*"|`[^`]*`|'(?:[^'\\]|\\.)'/,
            keyword: new RegExp('\\b(?:' + KW_GO + ')\\b'),
            type:    /\b[A-Z][A-Za-z0-9_]*\b/,
            number:  /\b\d[\d_]*(?:\.\d[\d_]*)?\b/,
        },
        java: {
            comment: /\/\/[^\n]*|\/\*[\s\S]*?\*\//,
            string:  /"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)'/,
            keyword: new RegExp('\\b(?:' + KW_JAVA + ')\\b'),
            type:    /\b[A-Z][A-Za-z0-9_]*\b/,
            number:  /\b\d[\d_]*(?:\.\d[\d_]*)?[lLfFdD]?\b/,
        },
        c: {
            comment: /\/\/[^\n]*|\/\*[\s\S]*?\*\//,
            string:  /"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)'/,
            keyword: new RegExp('\\b(?:' + KW_C + ')\\b'),
            type:    /\b[A-Z][A-Za-z0-9_]*_t\b|\b[A-Z][A-Za-z0-9_]*\b/,
            number:  /\b0[xX][0-9a-fA-F]+|\b\d+(?:\.\d+)?[uUlLfF]?\b/,
        },
        json: {
            string:  /"(?:[^"\\]|\\.)*"/,
            keyword: /\b(?:true|false|null)\b/,
            number:  /-?\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b/,
        },
        yaml: {
            comment: /#[^\n]*/,
            string:  /"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/,
            keyword: /\b(?:true|false|null|yes|no|on|off)\b/,
            type:    /^[ \t]*[A-Za-z_][\w-]*(?=[ \t]*:)/m,
            number:  /\b\d[\d_]*(?:\.\d[\d_]*)?\b/,
        },
        toml: {
            comment: /#[^\n]*/,
            string:  /"""[\s\S]*?"""|'''[\s\S]*?'''|"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/,
            keyword: /\b(?:true|false)\b/,
            type:    /^\s*\[[^\]\n]+\]/m,
            number:  /\b\d[\d_]*(?:\.\d[\d_]*)?\b/,
        },
        sh: {
            comment: /#[^\n]*/,
            string:  /"(?:[^"\\]|\\.)*"|'[^']*'/,
            keyword: new RegExp('\\b(?:' + KW_SH + ')\\b'),
            number:  /\b\d+\b/,
        },
        css: {
            comment: /\/\*[\s\S]*?\*\//,
            string:  /"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/,
            type:    /^[ \t]*[.#]?[A-Za-z_][\w-]*(?=[^{]*\{)/m,
            keyword: /[a-z-]+(?=\s*:)/,
            number:  /-?\b\d+(?:\.\d+)?(?:px|em|rem|vh|vw|%|s|ms|deg)?\b|#[0-9a-fA-F]{3,8}/,
        },
        html: {
            comment: /<!--[\s\S]*?-->/,
            string:  /"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/,
            keyword: /<\/?[a-zA-Z][a-zA-Z0-9-]*|\/?>/,
            type:    /\b[a-zA-Z-]+(?==)/,
        },
        markdown: {
            comment: /<!--[\s\S]*?-->/,
            string:  /"(?:[^"\\]|\\.)*"/,
            keyword: /^#{1,6} .*$|^\s*[-*+] |^\s*\d+\. /m,
            type:    /\*\*[^*]+\*\*|__[^_]+__/,
            number:  /\b\d[\d_]*\b/,
        },
    };

    // File extension → language id. The renderer falls back to plain
    // text when we don't have a mapping; the highlighter degrades to
    // a `pre`/`code` shell with HTML-escaped contents.
    const EXT_TO_LANG = {
        rs: 'rust',
        js: 'javascript', mjs: 'javascript', cjs: 'javascript',
        ts: 'typescript', tsx: 'typescript',
        jsx: 'javascript',
        py: 'python', pyi: 'python',
        go: 'go',
        java: 'java', kt: 'java', kts: 'java',
        c: 'c', h: 'c', cpp: 'c', cc: 'c', cxx: 'c', hpp: 'c', hh: 'c',
        json: 'json',
        yaml: 'yaml', yml: 'yaml',
        toml: 'toml',
        sh: 'sh', bash: 'sh', zsh: 'sh',
        css: 'css', scss: 'css',
        html: 'html', htm: 'html', xml: 'html', svg: 'html',
        md: 'markdown', markdown: 'markdown',
    };

    function detectLanguage(path) {
        if (!path) return null;
        // Strip query/anchor and pull just the extension.
        const clean = path.split(/[?#]/)[0];
        const dot = clean.lastIndexOf('.');
        if (dot < 0) return null;
        const ext = clean.slice(dot + 1).toLowerCase();
        return EXT_TO_LANG[ext] || null;
    }

    function escapeHtmlLocal(s) {
        return String(s)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;');
    }

    // Build one combined regex per language, lazily. Each alternative
    // gets a named group so the matcher can identify the kind in O(1)
    // per match without re-scanning. Inner regex sources MUST use only
    // non-capturing groups — the language specs above are written that
    // way; new entries should follow the same rule.
    const COMBINED = {};
    function buildCombined(lang) {
        if (COMBINED[lang]) return COMBINED[lang];
        const spec = SYNTAX[lang];
        if (!spec) return null;
        const parts = [];
        const order = [];
        for (const k of Object.keys(spec)) {
            parts.push('(?<' + k + '>' + spec[k].source + ')');
            order.push(k);
        }
        // Use 'gm' so per-line patterns (markdown headings, yaml keys,
        // toml sections) anchor as expected.
        const re = new RegExp(parts.join('|'), 'gm');
        COMBINED[lang] = { re, order };
        return COMBINED[lang];
    }

    function highlightCode(code, lang) {
        if (typeof code !== 'string' || !code) return '';
        const built = lang ? buildCombined(lang) : null;
        if (!built) return escapeHtmlLocal(code);
        const { re, order } = built;
        let out = '';
        let last = 0;
        re.lastIndex = 0;
        let m;
        while ((m = re.exec(code)) !== null) {
            // Some pathological regexes can match empty strings — bail
            // forward one char so we don't spin in place.
            if (m.index === re.lastIndex) { re.lastIndex++; continue; }
            out += escapeHtmlLocal(code.slice(last, m.index));
            let kind = null;
            if (m.groups) {
                for (const k of order) {
                    if (m.groups[k] !== undefined) { kind = k; break; }
                }
            }
            const cls = kind ? 'hl-' + kind : '';
            out += '<span class="' + cls + '">' + escapeHtmlLocal(m[0]) + '</span>';
            last = m.index + m[0].length;
        }
        out += escapeHtmlLocal(code.slice(last));
        return out;
    }

    window.AthenSyntax = { highlightCode, detectLanguage };
})();
