use std::collections::HashSet;

#[derive(Default)]
pub(crate) struct CurtainState {
    policy_requests: HashSet<u64>,
    manual_owner: Option<u64>,
    blanked: bool,
}

impl CurtainState {
    pub fn manually(
        &mut self,
        owner: u64,
        enabled: bool,
        set: impl FnOnce(bool) -> Result<(), String>,
    ) -> Result<bool, String> {
        if self.manual_owner.is_some_and(|current| current != owner) {
            return Err("Another viewer controls the privacy curtain.".into());
        }
        let desired = enabled || !self.policy_requests.is_empty();
        self.transition(desired, set)?;
        self.manual_owner = enabled.then_some(owner);
        Ok(self.blanked)
    }

    pub fn policy(
        &mut self,
        viewer: u64,
        enabled: bool,
        set: impl FnOnce(bool) -> Result<(), String>,
    ) -> Result<(), String> {
        if enabled {
            self.policy_requests.insert(viewer);
        } else {
            self.policy_requests.remove(&viewer);
        }
        self.transition(
            self.manual_owner.is_some() || !self.policy_requests.is_empty(),
            set,
        )
    }

    pub fn release(
        &mut self,
        viewer: u64,
        set: impl FnOnce(bool) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.manual_owner == Some(viewer) {
            self.manual_owner = None;
        }
        self.policy(viewer, false, set)
    }

    fn transition(
        &mut self,
        blanked: bool,
        set: impl FnOnce(bool) -> Result<(), String>,
    ) -> Result<(), String> {
        if blanked != self.blanked {
            set(blanked)?;
            self.blanked = blanked;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curtain_waits_for_last_request_and_keeps_manual_ownership() {
        let mut curtain = CurtainState::default();
        let mut changes = Vec::new();
        let mut apply = |enabled| {
            changes.push(enabled);
            Ok(())
        };
        curtain.policy(1, true, &mut apply).unwrap();
        assert!(curtain.manually(2, true, &mut apply).unwrap());
        assert!(curtain.manually(3, false, &mut apply).is_err());
        curtain.release(2, &mut apply).unwrap();
        curtain.release(1, &mut apply).unwrap();
        assert_eq!(changes, [true, false]);
    }
}
