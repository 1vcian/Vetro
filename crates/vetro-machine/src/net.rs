//! La rete del guest: virtio-net collegato allo stack di `vetro-net`
//! (gateway virtuale come la rete user di QEMU, sinkhole).
//!
//! Il tempo dello stack è quello della macchina: CNTPCT convertito in
//! microsecondi, mai l'orologio dell'host. La macchina aggiorna l'istante
//! prima di servire i dispositivi e chiama `poll` alla scadenza dello stack
//! (vedi `Machine::sync_irqs`), così le ritrasmissioni e i timer cadono
//! sempre sulla stessa istruzione.
//!
//! Le connessioni dall'host verso i servizi del guest (inoltro di porte,
//! `Stack::host_connect`: `vetro boot --hostfwd`, `vetro_net_*` nel browser)
//! passano da `Machine::net`: sono ingressi dell'host, e il `poll` forzato
//! prima della prossima istruzione li porta al guest.

use vetro_net::{NetConfig, Sinkhole, SinkholeConfig, Stack, VirtualTime};
use vetro_platform::map;
use vetro_platform::virtio::NetBackend;

/// MAC predefinito del guest: quello che QEMU assegna al primo
/// `virtio-net-device` (`52:54:00:12:34:56`).
pub const DEFAULT_GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// La scheda di rete della macchina e ciò che sta dall'altra parte del cavo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetSetup {
    /// MAC del guest (configurazione di virtio-net).
    pub mac: [u8; 6],
    /// Rete virtuale (indirizzi, MTU, lease, seme degli ISN).
    pub config: NetConfig,
    /// Il sinkhole: risposte per porta, DNS finto, ping.
    pub sinkhole: SinkholeConfig,
}

impl Default for NetSetup {
    fn default() -> Self {
        NetSetup { mac: DEFAULT_GUEST_MAC, config: NetConfig::default(), sinkhole: SinkholeConfig::default() }
    }
}

/// Verso di un frame visto al confine di virtio-net.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameDir {
    /// Trasmesso dal guest (va allo stack).
    FromGuest,
    /// Consegnato al guest (viene dallo stack).
    ToGuest,
}

/// Un frame Ethernet osservato dal punto di cattura di [`NetLink`] (M7,
/// ADR 0016): istante in tempo virtuale, verso e byte così come passano
/// per virtio-net (senza intestazione virtio).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TappedFrame {
    pub at: VirtualTime,
    pub dir: FrameDir,
    pub data: Vec<u8>,
}

/// Backend di virtio-net sopra lo stack: i frame del guest vanno a
/// [`Stack::receive`], quelli dello stack al guest con [`Stack::pop_frame`].
pub struct NetLink {
    pub stack: Stack<Sinkhole>,
    /// Istante corrente, fissato dalla macchina prima di ogni servizio.
    pub(crate) now: VirtualTime,
    /// Punto di cattura: se attivo, copia di ogni frame nei due versi.
    /// Solo osservazione: non cambia l'esecuzione e non entra negli
    /// snapshot.
    pub(crate) tap: Option<Vec<TappedFrame>>,
}

impl NetLink {
    pub fn new(setup: &NetSetup) -> Self {
        NetLink {
            stack: Stack::new(setup.config.clone(), Sinkhole::new(setup.sinkhole.clone())),
            now: VirtualTime(0),
            tap: None,
        }
    }

    /// Accende o spegne la cattura dei frame (spegnendola si perdono quelli
    /// non ancora presi).
    pub fn set_tap(&mut self, on: bool) {
        if on != self.tap.is_some() {
            self.tap = on.then(Vec::new);
        }
    }

    /// I frame catturati finora, in ordine; la cattura resta com'è.
    pub fn take_tapped(&mut self) -> Vec<TappedFrame> {
        self.tap.as_mut().map(std::mem::take).unwrap_or_default()
    }
}

impl NetBackend for NetLink {
    fn send(&mut self, frame: &[u8]) {
        if let Some(tap) = &mut self.tap {
            tap.push(TappedFrame { at: self.now, dir: FrameDir::FromGuest, data: frame.to_vec() });
        }
        self.stack.receive(self.now, frame);
    }
    fn recv(&mut self) -> Option<Vec<u8>> {
        let frame = self.stack.pop_frame()?;
        if let Some(tap) = &mut self.tap {
            tap.push(TappedFrame { at: self.now, dir: FrameDir::ToGuest, data: frame.clone() });
        }
        Some(frame)
    }
    /// L'istante corrente e tutto lo stack (connessioni, timer, sinkhole,
    /// registro degli eventi): la rete è dentro la macchina, niente da
    /// ricollegare.
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(self.now.0);
        w.section(b"NETS", |w| w.put(&self.stack));
    }
    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.now = VirtualTime(r.u64()?);
        let mut s = r.section(b"NETS")?;
        s.get(&mut self.stack)?;
        s.finish()
    }
}

/// Microsecondi di tempo virtuale a CNTPCT = `cnt` (per difetto).
pub(crate) fn micros(cnt: u64) -> VirtualTime {
    VirtualTime((u128::from(cnt) * 1_000_000 / u128::from(map::CNTFRQ_HZ)) as u64)
}

/// Primo CNTPCT a cui il tempo virtuale vale almeno `t`.
pub(crate) fn counter_at(t: VirtualTime) -> u64 {
    let c = (u128::from(t.0) * u128::from(map::CNTFRQ_HZ)).div_ceil(1_000_000);
    c.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversione_del_tempo() {
        assert_eq!(map::CNTFRQ_HZ, 62_500_000);
        assert_eq!(micros(62_500_000), VirtualTime::from_secs(1));
        assert_eq!(micros(62), VirtualTime(0));
        assert_eq!(micros(63), VirtualTime(1));
        for us in [0u64, 1, 2, 3, 999, 1_000_001, 75_000_000] {
            let c = counter_at(VirtualTime(us));
            assert!(micros(c) >= VirtualTime(us) && (c == 0 || micros(c - 1) < VirtualTime(us)), "{us}");
        }
    }

    /// Richiesta ARP del guest per il gateway 10.0.2.2.
    fn arp_request() -> Vec<u8> {
        let mut f = vec![0xff; 6];
        f.extend(DEFAULT_GUEST_MAC);
        f.extend([0x08, 0x06, 0, 1, 0x08, 0, 6, 4, 0, 1]);
        f.extend(DEFAULT_GUEST_MAC);
        f.extend([10, 0, 2, 15]);
        f.extend([0; 6]);
        f.extend([10, 0, 2, 2]);
        f
    }

    #[test]
    fn cattura_dei_frame_nei_due_versi() {
        let mut link = NetLink::new(&NetSetup::default());
        link.now = VirtualTime(5);
        link.send(&arp_request());
        assert!(link.take_tapped().is_empty(), "cattura spenta: niente");
        link.recv().expect("risposta ARP");

        link.set_tap(true);
        link.now = VirtualTime(1_000);
        link.send(&arp_request());
        link.now = VirtualTime(1_250);
        let reply = link.recv().expect("risposta ARP");
        assert_eq!(link.recv(), None);
        let t = link.take_tapped();
        assert_eq!(t.len(), 2);
        assert_eq!(
            (t[0].at, t[0].dir, &t[0].data),
            (VirtualTime(1_000), FrameDir::FromGuest, &arp_request())
        );
        assert_eq!((t[1].at, t[1].dir, &t[1].data), (VirtualTime(1_250), FrameDir::ToGuest, &reply));
        assert!(link.take_tapped().is_empty(), "già presi");
        link.set_tap(false);
        link.send(&arp_request());
        assert!(link.take_tapped().is_empty());
    }
}
