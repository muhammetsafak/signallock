//! Linux has no legacy app, so there is nothing to import.

use signallock_core::Store;

use super::super::{MigrationOutcome, Result};

pub fn migrate_legacy(_store: &Store) -> Result<MigrationOutcome> {
    Ok(MigrationOutcome::NotApplicable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_util::TestDir;

    #[test]
    fn migration_is_not_applicable_and_writes_nothing() {
        let dir = TestDir::new("linux-migrate");
        let store = Store::new(dir.path().to_path_buf());

        assert_eq!(
            migrate_legacy(&store).unwrap(),
            MigrationOutcome::NotApplicable
        );
        assert!(!store.settings_path().exists());
    }
}
