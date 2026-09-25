//! TCP terminato lato host: una connessione per ogni SYN del guest.
//!
//! Lo stack fa la parte del server remoto (apertura passiva, RFC 9293).
//! Scelte, tutte lecite per la RFC e sufficienti su un cavo virtuale:
//! - niente window scaling, SACK né timestamp: il SYN-ACK porta solo l'MSS,
//!   quindi il guest non li usa;
//! - i segmenti fuori ordine si scartano con un ACK duplicato (il guest
//!   ritrasmette); si accettano quelli che si sovrappongono all'atteso;
//! - ACK immediato di ogni segmento con dati, nessun ACK ritardato;
//! - ritrasmissione go-back-N al timeout (RTO della RFC 6298 in tempo
//!   virtuale, algoritmo di Karn, raddoppio), fast retransmit al terzo ACK
//!   duplicato, controllo di congestione Reno (RFC 5681), sonde a finestra
//!   zero con lo stesso timer (segmento vuoto con sequenza già riscontrata);
//! - RST e SYN fuori posto trattati come la RFC 5961 (ACK di sfida per i SYN).

use std::collections::VecDeque;

use crate::events::{CloseReason, Direction, EventKind, EventLog};
use crate::upstream::{TcpRead, TcpStatus, Upstream};
use crate::wire::{TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN, TcpHeader};
use crate::{ConnId, Flow, VirtualTime};

mod snapshot;

/// Buffer di ricezione (dati del guest non ancora presi dall'upstream):
/// coincide con la finestra massima annunciata, senza scaling.
pub(crate) const RX_CAPACITY: usize = 65_535;
/// Buffer di trasmissione (dati dell'upstream non ancora riscontrati).
pub(crate) const TX_CAPACITY: usize = 256 * 1024;
const READ_CHUNK: usize = 16 * 1024;

const RTO_INITIAL: u64 = 1_000_000;
const RTO_MIN: u64 = 200_000;
const RTO_MAX: u64 = 60_000_000;
/// Timeout consecutivi senza alcun ACK dal guest prima di abbandonare
/// (come `tcp_retries2` di Linux: circa 10 minuti con RTO massimo 60 s).
pub(crate) const MAX_RETRIES: u32 = 15;
/// Durata di TIME-WAIT (2 MSL con MSL di 2 s: il cavo è virtuale).
pub(crate) const TIME_WAIT_US: u64 = 4_000_000;
/// MSS predefinito se il SYN non ha l'opzione (RFC 9293 3.7.1).
const DEFAULT_MSS: u16 = 536;

fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn seq_le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// SYN ricevuto, in attesa che l'upstream apra (o rifiuti).
    Connecting,
    SynReceived,
    Established,
    CloseWait,
    LastAck,
    FinWait1,
    FinWait2,
    Closing,
    TimeWait,
    Closed,
}

/// Segmento da mandare al guest (lo stack aggiunge IP ed Ethernet).
#[derive(Clone, Debug)]
pub(crate) struct Segment {
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: Option<u16>,
    pub payload: Vec<u8>,
}

/// Quel che serve a una connessione per agire.
pub(crate) struct Ctx<'a, U: Upstream> {
    pub now: VirtualTime,
    pub up: &'a mut U,
    pub log: &'a mut EventLog,
    pub out: &'a mut Vec<Segment>,
}

pub(crate) struct TcpConn {
    pub id: ConnId,
    pub flow: Flow,
    pub state: State,
    local_mss: u16,
    mss: u32,
    // Trasmissione (verso il guest).
    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    /// Numero di sequenza più alto mai inviato (+1).
    snd_max: u32,
    snd_wnd: u32,
    snd_wl1: u32,
    snd_wl2: u32,
    cwnd: u32,
    ssthresh: u32,
    /// Byte da `snd_una` in poi: già inviati e non riscontrati, poi da inviare.
    tx: VecDeque<u8>,
    /// L'upstream ha finito: dopo `tx` va il FIN.
    tx_eof: bool,
    fin_seq: Option<u32>,
    // Ricezione (dal guest).
    irs: u32,
    rcv_nxt: u32,
    rx: VecDeque<u8>,
    rx_fin: bool,
    shutdown_done: bool,
    /// Bordo destro (`rcv_nxt + finestra`) dell'ultimo segmento inviato.
    adv_edge: u32,
    ack_pending: bool,
    // Timer.
    /// RTO in uso, raddoppiato a ogni timeout.
    rto: u64,
    /// RTO calcolato dai campioni, senza raddoppi.
    rto_base: u64,
    srtt: Option<u64>,
    rttvar: u64,
    rtt_sample: Option<(u32, VirtualTime)>,
    rtx_deadline: Option<VirtualTime>,
    retries: u32,
    dup_acks: u32,
    connect_deadline: VirtualTime,
    time_wait_deadline: VirtualTime,
    // Contatori.
    pub bytes_to_remote: u64,
    pub bytes_to_guest: u64,
}

impl TcpConn {
    pub(crate) fn new(
        id: ConnId,
        flow: Flow,
        syn: &TcpHeader,
        iss: u32,
        local_mss: u16,
        now: VirtualTime,
        connect_timeout_us: u64,
    ) -> Self {
        let mss = u32::from(syn.mss.unwrap_or(DEFAULT_MSS).min(local_mss).max(64));
        let rcv_nxt = syn.seq.wrapping_add(1);
        TcpConn {
            id,
            flow,
            state: State::Connecting,
            local_mss,
            mss,
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_max: iss,
            snd_wnd: u32::from(syn.window),
            snd_wl1: syn.seq,
            snd_wl2: iss,
            cwnd: 10 * mss,
            ssthresh: u32::MAX,
            tx: VecDeque::new(),
            tx_eof: false,
            fin_seq: None,
            irs: syn.seq,
            rcv_nxt,
            rx: VecDeque::new(),
            rx_fin: false,
            shutdown_done: false,
            adv_edge: rcv_nxt.wrapping_add(RX_CAPACITY as u32),
            ack_pending: false,
            rto: RTO_INITIAL,
            rto_base: RTO_INITIAL,
            srtt: None,
            rttvar: 0,
            rtt_sample: None,
            rtx_deadline: None,
            retries: 0,
            dup_acks: 0,
            connect_deadline: now.after(connect_timeout_us),
            time_wait_deadline: VirtualTime(u64::MAX),
            bytes_to_remote: 0,
            bytes_to_guest: 0,
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.state == State::Closed
    }

    fn rcv_wnd(&self) -> u32 {
        (RX_CAPACITY - self.rx.len()) as u32
    }

    fn emit<U: Upstream>(&mut self, ctx: &mut Ctx<'_, U>, seq: u32, flags: u8, payload: Vec<u8>) {
        let window = self.rcv_wnd();
        self.adv_edge = self.rcv_nxt.wrapping_add(window);
        self.ack_pending = false;
        let mss = (flags & TCP_SYN != 0).then_some(self.local_mss);
        ctx.out.push(Segment { seq, ack: self.rcv_nxt, flags, window: window as u16, mss, payload });
    }

    fn send_syn_ack<U: Upstream>(&mut self, ctx: &mut Ctx<'_, U>) {
        self.emit(ctx, self.iss, TCP_SYN | TCP_ACK, Vec::new());
        self.snd_nxt = self.iss.wrapping_add(1);
        self.snd_max = self.snd_nxt;
        self.arm_rtx(ctx.now);
    }

    /// RST verso il guest per una connessione che conosciamo.
    fn send_rst<U: Upstream>(&mut self, ctx: &mut Ctx<'_, U>) {
        let seq = self.snd_max;
        self.emit(ctx, seq, TCP_RST | TCP_ACK, Vec::new());
    }

    fn arm_rtx(&mut self, now: VirtualTime) {
        if self.rtx_deadline.is_none() {
            self.rtx_deadline = Some(now.after(self.rto));
        }
    }

    fn in_flight(&self) -> u32 {
        self.snd_nxt.wrapping_sub(self.snd_una)
    }

    fn finish<U: Upstream>(&mut self, ctx: &mut Ctx<'_, U>, reason: CloseReason, reset: bool) {
        if self.state == State::Closed {
            return;
        }
        self.state = State::Closed;
        self.rtx_deadline = None;
        ctx.log.push(
            ctx.now,
            EventKind::TcpClosed {
                id: self.id,
                reason,
                bytes_to_remote: self.bytes_to_remote,
                bytes_to_guest: self.bytes_to_guest,
            },
        );
        ctx.up.tcp_close(ctx.now, self.id, reset);
    }

    fn enter_time_wait(&mut self, now: VirtualTime) {
        self.state = State::TimeWait;
        self.rtx_deadline = None;
        self.time_wait_deadline = now.after(TIME_WAIT_US);
    }

    /// Aggiorna SRTT/RTTVAR/RTO con un campione (RFC 6298 2.2-2.3).
    fn rtt_update(&mut self, r: u64) {
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2;
            }
            Some(srtt) => {
                self.rttvar = (3 * self.rttvar + srtt.abs_diff(r)) / 4;
                self.srtt = Some((7 * srtt + r) / 8);
            }
        }
        let srtt = self.srtt.unwrap_or(r);
        self.rto_base = (srtt + (4 * self.rttvar).max(1)).clamp(RTO_MIN, RTO_MAX);
        self.rto = self.rto_base;
    }

    /// Segmento dal guest per questa connessione.
    pub(crate) fn input<U: Upstream>(&mut self, ctx: &mut Ctx<'_, U>, h: &TcpHeader, payload: &[u8]) {
        match self.state {
            State::Closed => return,
            State::Connecting => {
                // Il SYN ritrasmesso si ignora: aspettiamo ancora l'upstream.
                if h.has(TCP_RST) && (h.seq == self.rcv_nxt || h.seq == self.irs) {
                    self.finish(ctx, CloseReason::GuestReset, true);
                }
                return;
            }
            State::SynReceived if h.has(TCP_SYN) && !h.has(TCP_ACK) && h.seq == self.irs => {
                // Il guest non ha visto il nostro SYN-ACK.
                self.send_syn_ack(ctx);
                return;
            }
            _ => {}
        }

        // Test di accettabilità (RFC 9293 3.10.7.4).
        let seg_len = payload.len() as u32 + u32::from(h.has(TCP_SYN)) + u32::from(h.has(TCP_FIN));
        let wnd = self.rcv_wnd();
        let in_window = |s: u32| seq_le(self.rcv_nxt, s) && seq_lt(s, self.rcv_nxt.wrapping_add(wnd));
        let acceptable = match (seg_len, wnd) {
            (0, 0) => h.seq == self.rcv_nxt,
            (0, _) => in_window(h.seq),
            (_, 0) => false,
            (l, _) => in_window(h.seq) || in_window(h.seq.wrapping_add(l - 1)),
        };
        if !acceptable {
            if !h.has(TCP_RST) {
                self.ack_pending = true;
                if self.state == State::TimeWait && h.has(TCP_FIN) {
                    self.time_wait_deadline = ctx.now.after(TIME_WAIT_US);
                }
            }
            return;
        }
        if h.has(TCP_RST) {
            self.finish(ctx, CloseReason::GuestReset, true);
            return;
        }
        if h.has(TCP_SYN) {
            // SYN dentro la finestra: ACK di sfida (RFC 5961 4.2).
            self.ack_pending = true;
            return;
        }
        if !h.has(TCP_ACK) {
            return;
        }

        // Elaborazione dell'ACK.
        if self.state == State::SynReceived {
            if seq_lt(self.snd_una, h.ack) && seq_le(h.ack, self.snd_max) {
                self.state = State::Established;
                self.snd_wnd = u32::from(h.window);
                self.snd_wl1 = h.seq;
                self.snd_wl2 = h.ack;
                ctx.log.push(ctx.now, EventKind::TcpEstablished { id: self.id });
            } else {
                ctx.out.push(Segment {
                    seq: h.ack,
                    ack: 0,
                    flags: TCP_RST,
                    window: 0,
                    mss: None,
                    payload: Vec::new(),
                });
                return;
            }
        }
        if seq_lt(self.snd_max, h.ack) {
            // Riscontra qualcosa che non abbiamo mandato.
            self.ack_pending = true;
            return;
        }
        let mut fin_acked = false;
        if seq_le(self.snd_una, h.ack) {
            // Un ACK valido prova che il guest è vivo (anche per le sonde).
            self.retries = 0;
        }
        if seq_lt(self.snd_una, h.ack) {
            let mut acked = h.ack.wrapping_sub(self.snd_una);
            if self.snd_una == self.iss {
                acked -= 1; // il SYN
            }
            if let Some(f) = self.fin_seq
                && seq_lt(f, h.ack)
            {
                acked -= 1; // il FIN
                fin_acked = true;
            }
            let acked = (acked as usize).min(self.tx.len());
            self.tx.drain(..acked);
            self.snd_una = h.ack;
            if seq_lt(self.snd_nxt, h.ack) {
                self.snd_nxt = h.ack;
            }
            if let Some((seq, t)) = self.rtt_sample
                && seq_le(seq, h.ack)
            {
                self.rtt_update(ctx.now.0.saturating_sub(t.0));
                self.rtt_sample = None;
            }
            self.dup_acks = 0;
            // Un ACK di dati nuovi annulla i raddoppi dell'RTO (come BSD e
            // Linux): senza, dopo una raffica di perdite ogni segmento
            // successivo aspetterebbe l'RTO massimo, perché i campioni di
            // RTT sui dati ritrasmessi non valgono (Karn).
            self.rto = self.rto_base;
            let acked = acked as u32;
            if self.cwnd < self.ssthresh {
                self.cwnd = self.cwnd.saturating_add(acked.min(self.mss));
            } else {
                self.cwnd = self.cwnd.saturating_add((self.mss * self.mss / self.cwnd).max(1));
            }
            self.rtx_deadline = (self.snd_una != self.snd_max).then(|| ctx.now.after(self.rto));
        } else if h.ack == self.snd_una
            && payload.is_empty()
            && !h.has(TCP_FIN)
            && u32::from(h.window) == self.snd_wnd
            && self.snd_wnd != 0
            && self.snd_max != self.snd_una
        {
            self.dup_acks += 1;
            if self.dup_acks == 3 {
                self.fast_retransmit(ctx);
            }
        }
        if seq_lt(self.snd_wl1, h.seq) || (self.snd_wl1 == h.seq && seq_le(self.snd_wl2, h.ack)) {
            self.snd_wnd = u32::from(h.window);
            self.snd_wl1 = h.seq;
            self.snd_wl2 = h.ack;
        }
        if fin_acked {
            match self.state {
                State::FinWait1 => self.state = State::FinWait2,
                State::Closing => self.enter_time_wait(ctx.now),
                State::LastAck => {
                    self.finish(ctx, CloseReason::Normal, false);
                    return;
                }
                _ => {}
            }
        }

        // Dati.
        let mut complete = true;
        if !payload.is_empty() && matches!(self.state, State::Established | State::FinWait1 | State::FinWait2)
        {
            if seq_lt(self.rcv_nxt, h.seq) {
                // Fuori ordine: si scarta, l'ACK duplicato fa ritrasmettere.
                complete = false;
            } else {
                let skip = (self.rcv_nxt.wrapping_sub(h.seq) as usize).min(payload.len());
                let data = &payload[skip..];
                let n = data.len().min(self.rcv_wnd() as usize);
                complete = n == data.len();
                if n > 0 {
                    self.rx.extend(&data[..n]);
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(n as u32);
                    self.bytes_to_remote += n as u64;
                    ctx.log
                        .push(ctx.now, EventKind::TcpData { id: self.id, dir: Direction::ToRemote, len: n });
                }
            }
            self.ack_pending = true;
        }

        // FIN.
        if h.has(TCP_FIN) && complete && h.seq.wrapping_add(payload.len() as u32) == self.rcv_nxt {
            let first = match self.state {
                State::Established => {
                    self.state = State::CloseWait;
                    true
                }
                State::FinWait1 => {
                    self.state = State::Closing;
                    true
                }
                State::FinWait2 => {
                    self.enter_time_wait(ctx.now);
                    true
                }
                _ => false,
            };
            if first {
                self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                self.rx_fin = true;
            }
            self.ack_pending = true;
        }
    }

    fn fast_retransmit<U: Upstream>(&mut self, ctx: &mut Ctx<'_, U>) {
        let flight = self.in_flight();
        self.ssthresh = (flight / 2).max(2 * self.mss);
        self.cwnd = self.ssthresh;
        self.rtt_sample = None;
        if self.tx.is_empty() {
            return;
        }
        let n = (self.mss as usize).min(self.tx.len()).min(self.snd_max.wrapping_sub(self.snd_una) as usize);
        if n == 0 {
            return;
        }
        let payload: Vec<u8> = self.tx.range(..n).copied().collect();
        self.emit(ctx, self.snd_una, TCP_ACK, payload);
    }

    /// Chiude subito una connessione in TIME-WAIT (il guest riapre la stessa
    /// quadrupla con un SYN nuovo).
    pub(crate) fn end_time_wait<U: Upstream>(&mut self, ctx: &mut Ctx<'_, U>) {
        if self.state == State::TimeWait {
            self.finish(ctx, CloseReason::Normal, false);
        }
    }

    /// Scadenze dei timer. Da chiamare prima di `process`.
    pub(crate) fn on_timer<U: Upstream>(&mut self, ctx: &mut Ctx<'_, U>) {
        let now = ctx.now;
        match self.state {
            State::Closed => return,
            State::TimeWait if now >= self.time_wait_deadline => {
                self.finish(ctx, CloseReason::Normal, false);
                return;
            }
            _ => {}
        }
        let Some(deadline) = self.rtx_deadline else { return };
        if now < deadline {
            return;
        }
        self.rtx_deadline = None;
        let unsent = self.tx.len() > self.in_flight() as usize;
        if self.snd_una == self.snd_max && !(self.snd_wnd == 0 && unsent) {
            return;
        }
        self.retries += 1;
        if self.retries > MAX_RETRIES {
            self.send_rst(ctx);
            self.finish(ctx, CloseReason::Timeout, true);
            return;
        }
        self.rto = (self.rto * 2).min(RTO_MAX);
        self.rtt_sample = None;
        if self.snd_una != self.snd_max {
            self.ssthresh = (self.snd_max.wrapping_sub(self.snd_una) / 2).max(2 * self.mss);
            self.cwnd = self.mss;
        }
        if self.state == State::SynReceived {
            self.send_syn_ack(ctx);
            return;
        }
        // Go-back-N: si riparte da snd_una.
        self.snd_nxt = self.snd_una;
        if self.snd_wnd == 0 && !self.tx.is_empty() {
            // Sonda a finestra zero: segmento vuoto con un numero di sequenza
            // già riscontrato (come Linux), che obbliga il guest a rispondere
            // con un ACK e la finestra attuale senza mettere dati in volo.
            self.emit(ctx, self.snd_una.wrapping_sub(1), TCP_ACK, Vec::new());
        }
        self.arm_rtx(now);
    }

    /// Scambi con l'upstream e invio: da chiamare dopo ogni `input` e a ogni
    /// giro di poll.
    pub(crate) fn process<U: Upstream>(&mut self, ctx: &mut Ctx<'_, U>) {
        match self.state {
            State::Closed => return,
            State::Connecting => match ctx.up.tcp_status(ctx.now, self.id) {
                TcpStatus::Connected => {
                    self.state = State::SynReceived;
                    self.send_syn_ack(ctx);
                }
                TcpStatus::Refused => {
                    self.emit(ctx, 0, TCP_RST | TCP_ACK, Vec::new());
                    self.finish(ctx, CloseReason::Refused, true);
                    return;
                }
                TcpStatus::Pending => {
                    if ctx.now >= self.connect_deadline {
                        self.emit(ctx, 0, TCP_RST | TCP_ACK, Vec::new());
                        self.finish(ctx, CloseReason::Timeout, true);
                    }
                    return;
                }
            },
            _ => {}
        }

        // Dal guest verso l'upstream.
        while !self.rx.is_empty() {
            let (chunk, _) = self.rx.as_slices();
            let len = chunk.len();
            let n = ctx.up.tcp_write(ctx.now, self.id, chunk).min(len);
            self.rx.drain(..n);
            if n < len {
                break;
            }
        }
        if self.rx.is_empty() && self.rx_fin && !self.shutdown_done {
            self.shutdown_done = true;
            ctx.up.tcp_shutdown(ctx.now, self.id);
        }
        // Aggiornamento di finestra se si è riaperta di almeno un MSS.
        let edge = self.rcv_nxt.wrapping_add(self.rcv_wnd());
        let threshold = self.mss.min(RX_CAPACITY as u32 / 2);
        if !self.rx_fin && seq_lt(self.adv_edge, edge) && edge.wrapping_sub(self.adv_edge) >= threshold {
            self.ack_pending = true;
        }

        // Dall'upstream verso il guest.
        if matches!(self.state, State::SynReceived | State::Established | State::CloseWait) && !self.tx_eof {
            let mut buf = [0u8; READ_CHUNK];
            while self.tx.len() < TX_CAPACITY {
                let room = (TX_CAPACITY - self.tx.len()).min(READ_CHUNK);
                match ctx.up.tcp_read(ctx.now, self.id, &mut buf[..room]) {
                    TcpRead::Data(n) => {
                        let n = n.min(room);
                        if n == 0 {
                            break;
                        }
                        self.tx.extend(&buf[..n]);
                        self.bytes_to_guest += n as u64;
                        ctx.log.push(
                            ctx.now,
                            EventKind::TcpData { id: self.id, dir: Direction::ToGuest, len: n },
                        );
                    }
                    TcpRead::WouldBlock => break,
                    TcpRead::Eof => {
                        self.tx_eof = true;
                        break;
                    }
                    TcpRead::Reset => {
                        self.send_rst(ctx);
                        self.finish(ctx, CloseReason::RemoteReset, true);
                        return;
                    }
                }
            }
        }

        self.transmit(ctx);
        if self.ack_pending {
            self.emit(ctx, self.snd_nxt, TCP_ACK, Vec::new());
        }
    }

    fn transmit<U: Upstream>(&mut self, ctx: &mut Ctx<'_, U>) {
        if !matches!(
            self.state,
            State::Established | State::CloseWait | State::FinWait1 | State::LastAck | State::Closing
        ) {
            return;
        }
        loop {
            let in_flight = self.in_flight();
            let offset = in_flight as usize;
            if offset >= self.tx.len() {
                break;
            }
            let usable = self.snd_wnd.min(self.cwnd).saturating_sub(in_flight);
            if usable == 0 {
                break;
            }
            let n = (self.mss.min(usable) as usize).min(self.tx.len() - offset);
            let payload: Vec<u8> = self.tx.range(offset..offset + n).copied().collect();
            let flags = if offset + n == self.tx.len() { TCP_ACK | TCP_PSH } else { TCP_ACK };
            let seq = self.snd_nxt;
            self.emit(ctx, seq, flags, payload);
            let end = seq.wrapping_add(n as u32);
            if self.rtt_sample.is_none() && seq == self.snd_max {
                self.rtt_sample = Some((end, ctx.now));
            }
            self.snd_nxt = end;
            if seq_lt(self.snd_max, end) {
                self.snd_max = end;
            }
            self.arm_rtx(ctx.now);
        }

        // FIN dopo tutti i dati.
        if self.tx_eof && self.in_flight() as usize == self.tx.len() {
            let send = match self.fin_seq {
                None => match self.state {
                    State::Established => {
                        self.state = State::FinWait1;
                        true
                    }
                    State::CloseWait => {
                        self.state = State::LastAck;
                        true
                    }
                    _ => false,
                },
                Some(f) => f == self.snd_nxt,
            };
            if send {
                let seq = self.snd_nxt;
                self.fin_seq = Some(seq);
                self.emit(ctx, seq, TCP_FIN | TCP_ACK, Vec::new());
                self.snd_nxt = seq.wrapping_add(1);
                if seq_lt(self.snd_max, self.snd_nxt) {
                    self.snd_max = self.snd_nxt;
                }
                self.arm_rtx(ctx.now);
            }
        }

        // Finestra zero con dati in attesa: il timer fa da persist timer.
        if self.snd_wnd == 0 && self.snd_una == self.snd_max && self.tx.len() > self.in_flight() as usize {
            self.arm_rtx(ctx.now);
        }
    }

    /// Prossimo istante in cui `on_timer` ha qualcosa da fare.
    pub(crate) fn next_deadline(&self) -> Option<VirtualTime> {
        match self.state {
            State::Closed => None,
            State::Connecting => Some(self.connect_deadline),
            State::TimeWait => Some(self.time_wait_deadline),
            _ => self.rtx_deadline,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::TcpHeader;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn conn() -> TcpConn {
        let syn = TcpHeader {
            src_port: 1,
            dst_port: 2,
            seq: 0,
            ack: 0,
            flags: TCP_SYN,
            window: 1000,
            mss: None,
            checksum_ok: true,
        };
        let a = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1);
        TcpConn::new(1, Flow { guest: a, remote: a }, &syn, 0, 1460, VirtualTime(0), 1)
    }

    #[test]
    fn rto_follows_rfc6298() {
        let mut c = conn();
        assert_eq!((c.rto, c.mss), (1_000_000, 536), "RTO iniziale 1 s, MSS predefinito 536");
        // Primo campione R: SRTT = R, RTTVAR = R/2, RTO = SRTT + 4·RTTVAR.
        c.rtt_update(100_000);
        assert_eq!((c.srtt, c.rttvar, c.rto), (Some(100_000), 50_000, 300_000));
        // Poi RTTVAR = 3/4·RTTVAR + 1/4·|SRTT - R|, SRTT = 7/8·SRTT + 1/8·R.
        c.rtt_update(200_000);
        assert_eq!((c.srtt, c.rttvar), (Some(112_500), 62_500));
        assert_eq!(c.rto, 362_500);
        // Minimo 200 ms.
        for _ in 0..50 {
            c.rtt_update(10);
        }
        assert_eq!(c.rto, RTO_MIN);
    }

    #[test]
    fn sequence_comparison_wraps() {
        assert!(seq_lt(u32::MAX, 0));
        assert!(seq_lt(0xffff_fff0, 0x10));
        assert!(!seq_lt(0x10, 0xffff_fff0));
        assert!(seq_le(5, 5));
    }
}
