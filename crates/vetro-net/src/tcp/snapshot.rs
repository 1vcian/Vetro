//! Una connessione TCP negli snapshot (M6, ADR 0015): stato, sequenze,
//! finestre, controllo di congestione, dati in transito nei due versi, RTO
//! e timer. Figlio di `tcp` per vedere i campi privati: chi aggiunge un
//! campo a [`TcpConn`] lo aggiunge anche qui.

use vetro_snapshot::{Error, Reader, Result, Writer};

use super::{State, TcpConn};
use crate::stack::snapshot::{get_flow, get_reason, get_time, put_flow, put_reason, put_time};

fn state_code(s: State) -> u8 {
    match s {
        State::Connecting => 0,
        State::SynReceived => 1,
        State::Established => 2,
        State::CloseWait => 3,
        State::LastAck => 4,
        State::FinWait1 => 5,
        State::FinWait2 => 6,
        State::Closing => 7,
        State::TimeWait => 8,
        State::Closed => 9,
        State::SynSent => 10,
    }
}

fn state_from(v: u8) -> Result<State> {
    Ok(match v {
        0 => State::Connecting,
        1 => State::SynReceived,
        2 => State::Established,
        3 => State::CloseWait,
        4 => State::LastAck,
        5 => State::FinWait1,
        6 => State::FinWait2,
        7 => State::Closing,
        8 => State::TimeWait,
        9 => State::Closed,
        10 => State::SynSent,
        _ => return Err(Error::invalid(format!("stato TCP {v}"))),
    })
}

impl TcpConn {
    pub(crate) fn save(&self, w: &mut Writer) {
        w.u64(self.id);
        put_flow(w, &self.flow);
        w.u8(state_code(self.state));
        w.u16(self.local_mss);
        for v in [
            self.mss,
            self.iss,
            self.snd_una,
            self.snd_nxt,
            self.snd_max,
            self.snd_wnd,
            self.snd_wl1,
            self.snd_wl2,
            self.cwnd,
            self.ssthresh,
        ] {
            w.u32(v);
        }
        w.seq(&self.tx, |w, &b| w.u8(b));
        w.bool(self.tx_eof);
        w.opt(self.fin_seq, Writer::u32);
        w.u32(self.irs);
        w.u32(self.rcv_nxt);
        w.seq(&self.rx, |w, &b| w.u8(b));
        w.bool(self.rx_fin);
        w.bool(self.shutdown_done);
        w.u32(self.adv_edge);
        w.bool(self.ack_pending);
        w.u64(self.rto);
        w.u64(self.rto_base);
        w.opt_u64(self.srtt);
        w.u64(self.rttvar);
        w.opt(self.rtt_sample, |w, (seq, t)| {
            w.u32(seq);
            put_time(w, t);
        });
        w.opt(self.rtx_deadline, put_time);
        w.u32(self.retries);
        w.u32(self.dup_acks);
        put_time(w, self.connect_deadline);
        put_time(w, self.time_wait_deadline);
        w.u64(self.bytes_to_remote);
        w.u64(self.bytes_to_guest);
        w.opt(self.close_reason, put_reason);
    }

    pub(crate) fn restore(r: &mut Reader<'_>) -> Result<Self> {
        let id = r.u64()?;
        let flow = get_flow(r)?;
        let state = state_from(r.u8()?)?;
        let local_mss = r.u16()?;
        let mut v = [0u32; 10];
        for x in &mut v {
            *x = r.u32()?;
        }
        let [mss, iss, snd_una, snd_nxt, snd_max, snd_wnd, snd_wl1, snd_wl2, cwnd, ssthresh] = v;
        Ok(TcpConn {
            id,
            flow,
            state,
            local_mss,
            mss,
            iss,
            snd_una,
            snd_nxt,
            snd_max,
            snd_wnd,
            snd_wl1,
            snd_wl2,
            cwnd,
            ssthresh,
            tx: r.seq(1, Reader::u8)?.into(),
            tx_eof: r.bool()?,
            fin_seq: r.opt(Reader::u32)?,
            irs: r.u32()?,
            rcv_nxt: r.u32()?,
            rx: r.seq(1, Reader::u8)?.into(),
            rx_fin: r.bool()?,
            shutdown_done: r.bool()?,
            adv_edge: r.u32()?,
            ack_pending: r.bool()?,
            rto: r.u64()?,
            rto_base: r.u64()?,
            srtt: r.opt_u64()?,
            rttvar: r.u64()?,
            rtt_sample: r.opt(|r| Ok((r.u32()?, get_time(r)?)))?,
            rtx_deadline: r.opt(get_time)?,
            retries: r.u32()?,
            dup_acks: r.u32()?,
            connect_deadline: get_time(r)?,
            time_wait_deadline: get_time(r)?,
            bytes_to_remote: r.u64()?,
            bytes_to_guest: r.u64()?,
            close_reason: r.opt(get_reason)?,
        })
    }
}
