//! Limits on what one Wayland client may make the compositor hold (icon
//! pixels, ICC files): a client's own cost to send them is small, as
//! shared memory and files are not copied, so without a limit any client
//! could make the compositor allocate without end.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use smithay::reexports::wayland_server::backend::ClientId;

/// Each client's running total of one kind of thing.
#[derive(Debug)]
pub struct Budgets {
    limit: usize,
    clients: Mutex<Vec<(ClientId, Arc<AtomicUsize>)>>,
}

impl Budgets {
    pub const fn new(limit: usize) -> Self {
        Self { limit, clients: Mutex::new(Vec::new()) }
    }

    /// Take `amount` from `client`'s budget; `None` when that would go past
    /// the limit. It is given back when the [`Charge`] is dropped.
    pub fn charge(&self, client: ClientId, amount: usize) -> Option<Charge> {
        let total = {
            let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
            // Clients holding nothing any more (gone, most likely) are forgotten.
            clients.retain(|(_, t)| Arc::strong_count(t) > 1);
            match clients.iter().find(|(id, _)| *id == client) {
                Some((_, t)) => t.clone(),
                None => {
                    let t = Arc::new(AtomicUsize::new(0));
                    clients.push((client, t.clone()));
                    t
                }
            }
        };
        let limit = self.limit;
        total.try_update(Ordering::AcqRel, Ordering::Acquire, |held| held.checked_add(amount).filter(|n| *n <= limit)).ok()?;
        Some(Charge { total, amount })
    }

    /// What `client` holds now.
    #[cfg(test)]
    fn held(&self, client: &ClientId) -> usize {
        let clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        clients.iter().find(|(id, _)| id == client).map_or(0, |(_, t)| t.load(Ordering::Acquire))
    }
}

/// Part of a client's budget in use, given back when dropped.
#[derive(Debug)]
pub struct Charge {
    total: Arc<AtomicUsize>,
    amount: usize,
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.total.fetch_sub(self.amount, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::wayland_server::Display;

    #[test]
    fn charges_are_limited_per_client_and_given_back() {
        // Two real client IDs, from a display with two connections.
        let display: Display<()> = Display::new().unwrap();
        let mut ends = Vec::new();
        let ids: Vec<ClientId> = (0..2)
            .map(|_| {
                let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
                ends.push(ours);
                display.handle().insert_client(theirs, Arc::new(NoData)).unwrap().id()
            })
            .collect();
        let budgets = Budgets::new(10);
        let a = budgets.charge(ids[0].clone(), 6).unwrap();
        assert!(budgets.charge(ids[0].clone(), 5).is_none(), "past the limit");
        let b = budgets.charge(ids[0].clone(), 4).unwrap();
        assert!(budgets.charge(ids[1].clone(), 10).is_some(), "another client has its own");
        assert_eq!(budgets.held(&ids[0]), 10);
        drop(a);
        assert_eq!(budgets.held(&ids[0]), 4);
        assert!(budgets.charge(ids[0].clone(), 6).is_some());
        drop(b);
        assert_eq!(budgets.held(&ids[0]), 0);
        assert!(budgets.charge(ids[0].clone(), usize::MAX).is_none(), "no overflow");
    }

    struct NoData;
    impl smithay::reexports::wayland_server::backend::ClientData for NoData {}
}
