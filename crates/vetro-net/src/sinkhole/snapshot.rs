//! Il sinkhole negli snapshot (M6, ADR 0015): ciò che ha registrato
//! (connessioni, byte del guest, flussi UDP, domande DNS), le risposte in
//! corso e i nomi finti già assegnati. La configurazione non si salva: è
//! quella con cui è stato costruito (la macchina la controlla con l'hash
//! della sua configurazione).

use vetro_snapshot::{Reader, Result, Snapshot, Writer};

use super::{DnsRecord, Sinkhole, TcpRecord, TcpReply, TcpRuntime, UdpRecord};
use crate::stack::snapshot::{
    get_flow, get_ip, get_opt_str, get_time, put_flow, put_ip, put_opt_str, put_time,
};

fn put_reply(w: &mut Writer, r: &TcpReply) {
    w.bytes(&r.on_connect);
    w.bytes(&r.on_data);
    w.bool(r.close_after_reply);
}

fn get_reply(r: &mut Reader<'_>) -> Result<TcpReply> {
    Ok(TcpReply { on_connect: r.vec()?, on_data: r.vec()?, close_after_reply: r.bool()? })
}

impl Snapshot for Sinkhole {
    fn save(&self, w: &mut Writer) {
        w.seq(&self.tcp, |w, (&id, t)| {
            w.u64(id);
            w.u64(t.id);
            put_flow(w, &t.flow);
            put_opt_str(w, t.hostname.as_deref());
            w.bool(t.refused);
            put_time(w, t.opened_at);
            w.opt(t.closed_at, put_time);
            w.bool(t.reset);
            w.bool(t.guest_shutdown);
            w.bytes(&t.from_guest);
            w.u64(t.to_guest);
        });
        w.seq(&self.tcp_runtime, |w, (&id, rt)| {
            w.u64(id);
            put_reply(w, &rt.reply);
            w.seq(&rt.out, |w, &b| w.u8(b));
            w.bool(rt.replied_to_data);
            w.bool(rt.eof);
        });
        w.seq(&self.udp, |w, (&id, u)| {
            w.u64(id);
            w.u64(u.id);
            put_flow(w, &u.flow);
            put_opt_str(w, u.hostname.as_deref());
            w.seq(&u.datagrams, |w, (t, d)| {
                put_time(w, *t);
                w.bytes(d);
            });
            w.opt(u.closed_at, put_time);
        });
        w.seq(&self.udp_out, |w, (id, d)| {
            w.u64(*id);
            w.bytes(d);
        });
        w.seq(&self.dns, |w, d| {
            put_time(w, d.at);
            w.str(&d.name);
            w.u16(d.qtype);
            w.opt(d.answer, put_ip);
        });
        w.seq(&self.names, |w, (n, &a)| {
            w.str(n);
            put_ip(w, a);
        });
        w.seq(&self.addrs, |w, (&a, n)| {
            put_ip(w, a);
            w.str(n);
        });
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> Result<()> {
        self.tcp = r
            .seq(40, |r| {
                let key = r.u64()?;
                let rec = TcpRecord {
                    id: r.u64()?,
                    flow: get_flow(r)?,
                    hostname: get_opt_str(r)?,
                    refused: r.bool()?,
                    opened_at: get_time(r)?,
                    closed_at: r.opt(get_time)?,
                    reset: r.bool()?,
                    guest_shutdown: r.bool()?,
                    from_guest: r.vec()?,
                    to_guest: r.u64()?,
                };
                Ok((key, rec))
            })?
            .into_iter()
            .collect();
        self.tcp_runtime = r
            .seq(26, |r| {
                let key = r.u64()?;
                let rt = TcpRuntime {
                    reply: get_reply(r)?,
                    out: r.seq(1, Reader::u8)?.into(),
                    replied_to_data: r.bool()?,
                    eof: r.bool()?,
                };
                Ok((key, rt))
            })?
            .into_iter()
            .collect();
        self.udp = r
            .seq(30, |r| {
                let key = r.u64()?;
                let rec = UdpRecord {
                    id: r.u64()?,
                    flow: get_flow(r)?,
                    hostname: get_opt_str(r)?,
                    datagrams: r.seq(16, |r| Ok((get_time(r)?, r.vec()?)))?,
                    closed_at: r.opt(get_time)?,
                };
                Ok((key, rec))
            })?
            .into_iter()
            .collect();
        self.udp_out = r.seq(16, |r| Ok((r.u64()?, r.vec()?)))?.into();
        self.dns = r.seq(19, |r| {
            Ok(DnsRecord { at: get_time(r)?, name: r.string()?, qtype: r.u16()?, answer: r.opt(get_ip)? })
        })?;
        self.names = r.seq(12, |r| Ok((r.string()?, get_ip(r)?)))?.into_iter().collect();
        self.addrs = r.seq(12, |r| Ok((get_ip(r)?, r.string()?)))?.into_iter().collect();
        Ok(())
    }
}
