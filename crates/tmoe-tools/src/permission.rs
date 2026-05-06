use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Read,
    Write,
    Run,
    Metrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionProfile {
    grants: Vec<Permission>,
}

impl PermissionProfile {
    pub fn new(grants: impl IntoIterator<Item = Permission>) -> Self {
        Self { grants: grants.into_iter().collect() }
    }

    pub fn allows(&self, p: Permission) -> bool {
        self.grants.iter().any(|g| *g == p)
    }

    pub fn worker() -> Self {
        Self::new([Permission::Read, Permission::Write, Permission::Run])
    }

    pub fn supervisor() -> Self {
        Self::new([Permission::Read, Permission::Metrics])
    }

    pub fn observer() -> Self {
        Self::new([Permission::Read, Permission::Metrics])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_can_write_run() {
        let p = PermissionProfile::worker();
        assert!(p.allows(Permission::Read));
        assert!(p.allows(Permission::Write));
        assert!(p.allows(Permission::Run));
    }

    #[test]
    fn supervisor_cannot_write_or_run() {
        let p = PermissionProfile::supervisor();
        assert!(p.allows(Permission::Read));
        assert!(p.allows(Permission::Metrics));
        assert!(!p.allows(Permission::Write));
        assert!(!p.allows(Permission::Run));
    }

    #[test]
    fn observer_is_read_metrics_only() {
        let p = PermissionProfile::observer();
        assert!(p.allows(Permission::Read));
        assert!(p.allows(Permission::Metrics));
        assert!(!p.allows(Permission::Write));
        assert!(!p.allows(Permission::Run));
    }
}
