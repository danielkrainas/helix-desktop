//! Agent Access's contract (helixdesktop/spec `contracts/agent-access`)
//! as far as this shell has it: the members of the scoped actions' second
//! batch (helixdesktop/desktop#157) come later, so conformance checks
//! list them as known unimplemented. Batch 2 empties these.

/// `org.helixdesktop.Agent1` members of batch 2.
pub const AGENT1_LATER: &[&str] = &[
    "method SetWallpaper",
    "method SetAccent",
    "method SetPowerMode",
    "method StarFile",
    "method SetFavorite",
    "method DismissNotification",
    "method NotificationAction",
    "method RevealFile",
];

/// `org.helixdesktop.AgentAccess1` members of batch 2.
pub const AGENT_ACCESS1_LATER: &[&str] = &["method ListChanges", "method Undo", "signal ChangesChanged"];
