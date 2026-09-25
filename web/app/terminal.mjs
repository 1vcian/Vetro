// Terminale semplice per la console seriale (PL011): testo a righe con il
// cursore sull'ultima riga, le sequenze di controllo che usano la shell e
// il kernel (CR, LF, BS, TAB, CSI K/J/C/D/m) e la risposta a ESC[6n (la
// shell di BusyBox chiede la posizione del cursore dopo il prompt). Senza
// DOM: la pagina mostra `text()`.

export class Terminal {
  lines = [''];
  col = 0;
  #state = 'normal';
  #params = '';
  #decoder = new TextDecoder('utf-8');

  /**
   * maxLines: righe tenute (le più vecchie si scartano); onReply(stringa):
   * byte da rimandare al guest (risposte del terminale).
   */
  constructor({ maxLines = 5000, visibleRows = 24, onReply = () => {} } = {}) {
    this.maxLines = maxLines;
    this.visibleRows = visibleRows;
    this.onReply = onReply;
  }

  /** Byte dal guest (Uint8Array), UTF-8 anche spezzato fra due chiamate. */
  feed(bytes) {
    for (const ch of this.#decoder.decode(bytes, { stream: true })) this.#char(ch);
  }

  text() {
    return this.lines.join('\n');
  }

  #put(ch) {
    let line = this.lines[this.lines.length - 1];
    if (this.col > line.length) line += ' '.repeat(this.col - line.length);
    this.lines[this.lines.length - 1] = line.slice(0, this.col) + ch + line.slice(this.col + 1);
    this.col++;
  }

  #newline() {
    this.lines.push('');
    this.col = 0;
    if (this.lines.length > this.maxLines) this.lines.splice(0, this.lines.length - this.maxLines);
  }

  #csi(final) {
    const args = this.#params.replace(/^\?/, '').split(';').map((p) => (p === '' ? 0 : Number(p)));
    const n = args[0] || 1;
    const last = this.lines.length - 1;
    switch (final) {
      case 'n':
        if (args[0] === 6) {
          const row = Math.min(this.lines.length, this.visibleRows);
          this.onReply(`\x1b[${row};${this.col + 1}R`);
        }
        break;
      case 'K':
        if (args[0] === 0) this.lines[last] = this.lines[last].slice(0, this.col);
        else if (args[0] === 1) this.lines[last] = ' '.repeat(this.col) + this.lines[last].slice(this.col);
        else this.lines[last] = '';
        break;
      case 'J':
        if (args[0] === 2 || args[0] === 3) {
          this.lines = [''];
          this.col = 0;
        } else if (args[0] === 0) {
          this.lines[last] = this.lines[last].slice(0, this.col);
        }
        break;
      case 'C':
        this.col += n;
        break;
      case 'D':
        this.col = Math.max(0, this.col - n);
        break;
      case 'G':
        this.col = Math.max(0, n - 1);
        break;
      case 'H':
      case 'f':
        this.col = Math.max(0, (args[1] || 1) - 1);
        break;
      default:
        // Colori (m), modi (h/l) e il resto: ignorati.
        break;
    }
  }

  #char(ch) {
    switch (this.#state) {
      case 'esc':
        if (ch === '[') {
          this.#state = 'csi';
          this.#params = '';
        } else if (ch === ']') {
          this.#state = 'osc';
        } else {
          this.#state = 'normal';
        }
        return;
      case 'csi':
        if (/[0-9;?<=>]/.test(ch)) {
          this.#params += ch;
        } else {
          this.#state = 'normal';
          this.#csi(ch);
        }
        return;
      case 'osc':
        if (ch === '\x07') this.#state = 'normal';
        else if (ch === '\x1b') this.#state = 'esc';
        return;
      default:
        break;
    }
    const c = ch.codePointAt(0);
    if (ch === '\x1b') this.#state = 'esc';
    else if (ch === '\r') this.col = 0;
    else if (ch === '\n') this.#newline();
    else if (ch === '\b') this.col = Math.max(0, this.col - 1);
    else if (ch === '\t') this.col = (Math.floor(this.col / 8) + 1) * 8;
    else if (c >= 0x20 && c !== 0x7f) this.#put(ch);
  }
}

/** I byte che un terminale manda per un tasto premuto, o null. */
export function keyToBytes(e) {
  const special = {
    Enter: '\r', Backspace: '\x7f', Tab: '\t', Escape: '\x1b',
    ArrowUp: '\x1b[A', ArrowDown: '\x1b[B', ArrowRight: '\x1b[C', ArrowLeft: '\x1b[D',
    Home: '\x1b[H', End: '\x1b[F', Delete: '\x1b[3~', PageUp: '\x1b[5~', PageDown: '\x1b[6~', Insert: '\x1b[2~',
  };
  if (e.metaKey) return null;
  if (e.ctrlKey && !e.altKey && e.key.length === 1) {
    const k = e.key.toUpperCase().charCodeAt(0);
    if (k >= 0x40 && k <= 0x5f) return String.fromCharCode(k - 0x40);
    if (e.key === ' ') return '\0';
    return null;
  }
  if (Object.hasOwn(special, e.key)) return special[e.key];
  // Alt+tasto: il carattere che il sistema ha già composto (macOS: Opzione).
  if ([...e.key].length === 1) return e.key;
  return null;
}
