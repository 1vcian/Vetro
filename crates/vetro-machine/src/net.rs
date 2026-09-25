//! La rete del guest: virtio-net collegato allo stack di `vetro-net`
//! (gateway virtuale come la rete user di QEMU, sinkhole).
//!
//! Il tempo dello stack è quello della macchina: CNTPCT convertito in
//! microsecondi, mai l'orologio dell'host. La macchina aggiorna l'istante
//! prima di servire i dispositivi e chiama `poll` alla scadenza dello stack
//! (vedi `Machine::sync_irqs`), così le ritrasmissioni e i timer cadono
//! sempre sulla stessa istruzione.

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

/// Backend di virtio-net sopra lo stack: i frame del guest vanno a
/// [`Stack::receive`], quelli dello stack al guest con [`Stack::pop_frame`].
pub struct NetLink {
    pub stack: Stack<Sinkhole>,
    /// Istante corrente, fissato dalla macchina prima di ogni servizio.
    pub(crate) now: VirtualTime,
}

impl NetLink {
    pub fn new(setup: &NetSetup) -> Self {
        NetLink {
            stack: Stack::new(setup.config.clone(), Sinkhole::new(setup.sinkhole.clone())),
            now: VirtualTime(0),
        }
    }
}

impl NetBackend for NetLink {
    fn send(&mut self, frame: &[u8]) {
        self.stack.receive(self.now, frame);
    }
    fn recv(&mut self) -> Option<Vec<u8>> {
        self.stack.pop_frame()
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
}
