// Minimal ANSI escape-sequence → HTML renderer for shell output.
//
// Covers the parts CLIs actually use: SGR for the 16 base colours
// (30-37, 90-97 fg / 40-47, 100-107 bg) plus bold / dim / italic /
// underline. 256-colour and truecolor parameters are parsed and
// dropped (no neon swatches in our palette anyway). Cursor-move,
// screen-clear, and OSC ("set window title") sequences are stripped.
// Returns a string of HTML-escaped text with <span class="ansi-…">
// wrappers; pair with the rules in styles.css for colour.
//
// Trade-off: not a full terminal emulator. We don't track cursor
// position, redraw lines, or honour `\r`-based progress bars; tools
// that expect those are happier with a real PTY. For tool-card
// retrospective audit ("show me the cargo build output"), this is
// the right shape — small, deterministic, no dependency.

(function() {
    'use strict';

    function escapeHtmlLocal(s) {
        return String(s)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;');
    }

    // SGR codes that close *all* open spans. We could be precise per
    // attribute (22 closes bold/dim, 23 italic, 24 underline, 39 fg,
    // 49 bg) but that requires a span stack keyed by attribute. The
    // lossy "close all on any reset" approach matches what most CLIs
    // actually do (`reset` after each coloured chunk), with no visual
    // artefacts in practice.
    const RESET_CODES = new Set([0, 22, 23, 24, 27, 29, 39, 49]);

    function codesToClasses(codes) {
        const out = [];
        for (let i = 0; i < codes.length; i++) {
            const c = codes[i];
            if (c === 1)      out.push('ansi-bold');
            else if (c === 2) out.push('ansi-dim');
            else if (c === 3) out.push('ansi-italic');
            else if (c === 4) out.push('ansi-underline');
            else if (c >= 30 && c <= 37)   out.push('ansi-fg-' + (c - 30));
            else if (c >= 40 && c <= 47)   out.push('ansi-bg-' + (c - 40));
            else if (c >= 90 && c <= 97)   out.push('ansi-fg-' + (c - 90 + 8));
            else if (c >= 100 && c <= 107) out.push('ansi-bg-' + (c - 100 + 8));
            // Skip past 256-colour (38;5;n / 48;5;n) and truecolor
            // (38;2;r;g;b / 48;2;r;g;b) parameter tails — we don't
            // render them, but we must consume their args so the next
            // base code isn't misread as one of them.
            else if ((c === 38 || c === 48) && codes[i + 1] === 5) i += 2;
            else if ((c === 38 || c === 48) && codes[i + 1] === 2) i += 4;
        }
        return out.join(' ');
    }

    function ansiToHtml(text) {
        if (typeof text !== 'string' || !text) return '';
        // Strip OSC ("\x1b]…\x07" or "\x1b]…\x1b\\"). Common offender:
        // shells / TUIs that re-set the terminal title between commands.
        let s = text.replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, '');
        // Walk every CSI escape. SGR (`m`) drives colour; everything
        // else (cursor moves, screen clears, etc.) gets dropped.
        const CSI = /\x1b\[([0-9;]*)([A-Za-z])/g;

        let out = '';
        let last = 0;
        let openSpans = 0;
        const closeAll = () => {
            let r = '';
            while (openSpans > 0) { r += '</span>'; openSpans--; }
            return r;
        };

        let m;
        while ((m = CSI.exec(s)) !== null) {
            out += escapeHtmlLocal(s.slice(last, m.index));
            if (m[2] === 'm') {
                const raw = m[1] || '0';
                const codes = raw.split(';').map((n) => parseInt(n, 10) || 0);
                // Bare `\x1b[m` (empty params) is treated as `\x1b[0m`.
                const isReset = codes.length === 1 && RESET_CODES.has(codes[0]);
                if (isReset) {
                    out += closeAll();
                } else {
                    // Codes that mix attributes with a reset (rare): we
                    // close first, then open the new attributes — gives
                    // the same on-screen effect as a real terminal.
                    if (codes.some((c) => RESET_CODES.has(c))) {
                        out += closeAll();
                    }
                    const cls = codesToClasses(codes);
                    if (cls) {
                        out += '<span class="' + cls + '">';
                        openSpans++;
                    }
                }
            }
            // else: silently drop non-SGR CSI (cursor moves etc.)
            last = m.index + m[0].length;
        }
        out += escapeHtmlLocal(s.slice(last));
        out += closeAll();
        return out;
    }

    // True if the input contains at least one ANSI escape — the caller
    // can use this to skip the parser when stdout is plain text and
    // hand the raw string to `textContent` instead.
    function hasAnsi(text) {
        return typeof text === 'string' && text.indexOf('\x1b[') !== -1;
    }

    window.AthenAnsi = { toHtml: ansiToHtml, hasAnsi };
})();
