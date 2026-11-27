use std::collections::BTreeMap;

use deskbolt_proto::{ControlMsg, SessionMode, ViewerPresence};
use tokio::sync::watch;

#[derive(Default)]
pub(crate) struct Roster {
    viewers: BTreeMap<u64, ViewerPresence>,
    request: Option<u64>,
    exclusive: bool,
}

impl Roster {
    pub(crate) fn new(exclusive: bool) -> Self {
        Self { exclusive, ..Self::default() }
    }

    fn controller(&self) -> Option<u64> {
        self.viewers.values().find(|v| v.controlling).map(|v| v.id)
    }

    pub(crate) fn controlling(&self, id: u64) -> bool {
        self.viewers.get(&id).is_some_and(|v| v.controlling)
    }

    pub(crate) fn snapshot(&self, id: u64) -> ControlMsg {
        ControlMsg::ViewerPresenceList {
            self_id: id,
            exclusive: self.exclusive,
            viewers: self.viewers.values().cloned().collect(),
            request: self.request.filter(|&requester| requester == id || self.controlling(id)),
        }
    }
}

pub(crate) struct ViewerLease {
    pub(crate) id: u64,
    roster: watch::Sender<Roster>,
    desktop: bool,
}

impl ViewerLease {
    pub(crate) fn join(
        roster: watch::Sender<Roster>, id: u64, name: &str, requested: SessionMode,
    ) -> Result<(Self, SessionMode), String> {
        let desktop = requested != SessionMode::FileTransfer;
        let lease = Self { id, roster, desktop };
        if !desktop {
            return Ok((lease, requested));
        }
        let name: String = name.trim().chars()
            .filter(|c| !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
            .take(64).collect();
        let mut mode = requested;
        if !lease.roster.send_if_modified(|r| {
            if r.viewers.len() >= 64 {
                return false;
            }
            if r.exclusive && r.controller().is_some() {
                mode = SessionMode::ViewOnly;
            }
            let name = if name.trim().is_empty() { format!("Viewer {id}") } else { name };
            r.viewers.insert(id, ViewerPresence { id, name, controlling: mode == SessionMode::FullControl });
            true
        }) {
            return Err("The host has reached its desktop session limit.".into());
        }
        Ok((lease, mode))
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Roster> {
        self.roster.subscribe()
    }

    pub(crate) fn acquire_or_request(&self, ask_controller: bool) -> Result<bool, &'static str> {
        let mut result = Ok(false);
        self.roster.send_if_modified(|r| {
            if r.controlling(self.id) {
                result = Ok(true);
                return false;
            }
            if r.exclusive && r.controller().is_some() {
                if !ask_controller {
                    result = Err("Another viewer currently holds remote control.");
                    return false;
                }
                if r.request.is_some_and(|id| id != self.id) {
                    result = Err("Another control request is pending.");
                    return false;
                }
                r.request = Some(self.id);
                return true;
            }
            if let Some(viewer) = r.viewers.get_mut(&self.id) {
                viewer.controlling = true;
                result = Ok(true);
                return true;
            }
            false
        });
        result
    }

    pub(crate) fn can_handoff(&self, to: u64) -> bool {
        let r = self.roster.borrow();
        r.exclusive && r.controlling(self.id) && r.request == Some(to)
    }

    pub(crate) fn handoff(&self, to: u64, approved: bool) -> bool {
        self.roster.send_if_modified(|r| {
            if !r.exclusive || !r.controlling(self.id) || r.request != Some(to) {
                return false;
            }
            r.request = None;
            if approved {
                r.viewers.get_mut(&self.id).unwrap().controlling = false;
                r.viewers.get_mut(&to).unwrap().controlling = true;
            }
            true
        })
    }

    pub(crate) fn release(&self) {
        self.roster.send_if_modified(|r| {
            if !r.controlling(self.id) {
                return false;
            }
            r.viewers.get_mut(&self.id).unwrap().controlling = false;
            if r.exclusive {
                r.request = None;
            }
            true
        });
    }
}

impl Drop for ViewerLease {
    fn drop(&mut self) {
        if self.desktop {
            self.roster.send_if_modified(|r| {
                if r.request == Some(self.id) || (r.exclusive && r.controlling(self.id)) {
                    r.request = None;
                }
                r.viewers.remove(&self.id).is_some()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_control_handoff_is_bound_to_the_current_request_and_owner() {
        let (tx, rx) = watch::channel(Roster::new(true));
        let join = |id| ViewerLease::join(tx.clone(), id, "Operator", SessionMode::FullControl).unwrap();
        let (owner, first) = join(1);
        let (waiting, second) = join(2);
        let (other, _) = join(3);
        assert_eq!(first, SessionMode::FullControl);
        assert_eq!(second, SessionMode::ViewOnly);
        assert!(waiting.acquire_or_request(false).is_err());
        assert_eq!(waiting.acquire_or_request(true), Ok(false));
        assert!(other.acquire_or_request(true).is_err());
        assert!(!other.handoff(2, true));
        assert!(owner.handoff(2, false));
        assert!(rx.borrow().controlling(1));
        assert!(!owner.handoff(2, true));
        assert_eq!(waiting.acquire_or_request(true), Ok(false));
        assert!(owner.handoff(2, true));
        assert!(!rx.borrow().controlling(1));
        assert!(rx.borrow().controlling(2));
        assert!(!owner.handoff(3, true));
        drop(waiting);
        assert_eq!(other.acquire_or_request(true), Ok(true));
    }
}
