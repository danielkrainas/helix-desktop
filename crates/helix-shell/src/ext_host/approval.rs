//! The approval prompt (helixdesktop/spec `contracts/extensions/README.md`,
//! "Approval prompt"): `Enable(id, true)` from another program asks the
//! user before an extension's permissions are approved.

use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

/// What the prompt shows, and where the answer goes.
#[derive(Debug)]
pub struct ApprovalQuestion {
    pub name: String,
    /// What it will be able to do (the permissions' texts).
    pub can: Vec<String>,
    pub sandboxed: bool,
    /// Sandboxed, but with a permission that leaves the sandbox
    /// ([`super::manifest::leaves_sandbox`]).
    pub leaves_sandbox: bool,
    pub reply: oneshot::Sender<bool>,
}

/// An [`ApprovalQuestion`] that can travel in a [`crate::UiEvent`].
#[derive(Debug, Clone)]
pub struct ApprovalSlot(Arc<Mutex<Option<ApprovalQuestion>>>);

impl ApprovalSlot {
    pub fn new(question: ApprovalQuestion) -> Self {
        Self(Arc::new(Mutex::new(Some(question))))
    }

    pub fn take(&self) -> Option<ApprovalQuestion> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl PartialEq for ApprovalSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The prompt's lines below "It will be able to:".
pub fn lines(can: &[String], sandboxed: bool, leaves_sandbox: bool) -> Vec<String> {
    let mut out = can.to_vec();
    if !sandboxed {
        out.push(helix_i18n::tr("It is not sandboxed: it can do anything you can.").to_owned());
    } else if leaves_sandbox {
        out.push(helix_i18n::tr("Its permissions let it out of its sandbox: it can do anything you can.").to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings() {
        let can = vec!["use the network".to_owned()];
        assert_eq!(lines(&can, true, false), can);
        assert_eq!(lines(&can, false, false).len(), 2);
        assert!(lines(&can, true, true)[1].contains("out of its sandbox"));
        assert!(!super::super::manifest::leaves_sandbox(&["network".into(), "session-bus".into()]), "filtered");
        assert!(super::super::manifest::leaves_sandbox(&["session-bus".into(), "session-bus:talk=org.freedesktop.systemd1".into()]));
        assert!(super::super::manifest::leaves_sandbox(&["files:home".into()]));
        assert!(!super::super::manifest::leaves_sandbox(&["files:documents".into(), "network".into(), "system-bus".into()]));
    }
}
