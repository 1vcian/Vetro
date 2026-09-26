// L'immagine AOSP di Vetro nel browser e in Node (M5, ADR 0028): le fasi
// dell'avvio lette dalla console del guest. Non usa API di Node.
//
// Il kernel GKI scrive su ttyAMA0 e init (con `printk.devkmsg=on`) scrive in
// kmsg: le righe qui sotto sono stabili fra un avvio e l'altro e bastano a
// dire a che punto è la macchina. L'ordine è quello dell'avvio; una fase
// vista implica le precedenti.

/** Fasi dell'avvio: [nome, etichetta, riconoscitore della riga]. */
export const PHASES = [
  ['kernel', 'kernel', /Booting Linux on physical CPU/],
  ['init', 'init, prima fase', /Run \/init as init process/],
  ['init2', 'init, seconda fase', /init: init second stage started/],
  ['zygote', 'zygote', /init: starting service 'zygote'/],
  ['surfaceflinger', 'grafica (surfaceflinger)', /init: starting service 'surfaceflinger'/],
  ['system_server', 'system_server', /\(system_server\)/],
  // `sys.boot_completed=1`: init mette in coda l'evento sys-boot-completed-set
  // (azione di init.cutf_cvm.rc nell'immagine di Vetro).
  ['booted', 'avvio finito', /\(sys\.boot_completed=1\)|sys-boot-completed-set/],
  // Non dalla console: l'attività in primo piano diventa il launcher (prima
  // c'è FallbackHome, "Phone is starting"). Chi ha adb chiama `mark`.
  ['home', 'home (launcher)', null],
];

/**
 * Parametri del bootloader per l'immagine AOSP di Vetro (ADR 0028): `nokaslr`
 * come tools/aosp/vetro.sh.
 */
export const ANDROID_PARAMS = 'nokaslr';

/**
 * Un colore dell'app come lo mostra lo scanout. L'immagine di oggi scambia
 * rosso e blu (l'app blu 0x1565c0 arriva come (192, 101, 21): il composer
 * scrive RGBA in buffer che virtio-gpu presenta come XRGB8888; con
 * `display_framebuffer_format=bgra` non cambia, ADR 0028): i test accettano
 * i due ordini e dicono quale hanno visto.
 */
export function colorSeen(px, rgb, tol = 8) {
  if (!px) return null;
  const near = (c) => c.every((v, i) => Math.abs(v - px[i]) <= tol);
  if (near(rgb)) return 'rgb';
  if (near([rgb[2], rgb[1], rgb[0]])) return 'bgr';
  return null;
}

/** Comando adb per l'attività in primo piano; la home c'è se contiene "launcher". */
export const HOME_QUERY = 'dumpsys window | grep -m1 mCurrentFocus';
export const isHome = (out) => /launcher/i.test(out);

/** Segue la console e dice quando si entra in una fase nuova. */
export class BootProgress {
  /** Indice dell'ultima fase vista (-1 = nessuna). */
  index = -1;
  /** [{ phase, label, guestSecs }] in ordine. */
  events = [];
  #line = '';

  get phase() {
    return this.index < 0 ? null : PHASES[this.index][0];
  }

  get label() {
    return this.index < 0 ? 'in attesa del kernel' : PHASES[this.index][1];
  }

  /** Nuovo testo della console (stringa); restituisce le fasi nuove. */
  feed(text, guestSecs) {
    const out = [];
    const lines = (this.#line + text).split('\n');
    this.#line = lines.pop();
    // Una riga lunghissima senza a capo non deve crescere per sempre.
    if (this.#line.length > 4096) this.#line = this.#line.slice(-4096);
    for (const l of lines) {
      for (let k = this.index + 1; k < PHASES.length; k++) {
        if (PHASES[k][2]?.test(l)) {
          // Le fasi saltate (righe perse) contano come viste adesso.
          for (let j = this.index + 1; j <= k; j++) {
            const ev = { phase: PHASES[j][0], label: PHASES[j][1], guestSecs };
            this.events.push(ev);
            out.push(ev);
          }
          this.index = k;
          break;
        }
      }
    }
    return out;
  }

  /** Segna una fase vista da fuori della console (la home); restituisce le fasi nuove. */
  mark(phase, guestSecs) {
    const k = PHASES.findIndex((p) => p[0] === phase);
    if (k <= this.index) return [];
    const out = [];
    for (let j = this.index + 1; j <= k; j++) {
      const ev = { phase: PHASES[j][0], label: PHASES[j][1], guestSecs };
      this.events.push(ev);
      out.push(ev);
    }
    this.index = k;
    return out;
  }
}
