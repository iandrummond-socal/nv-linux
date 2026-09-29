//! Pure three-way reconciliation of one note.
//!
//! The caller detects case collisions and non-UTF-8 names and emits `Skip`
//! actions with `CaseCollision` or `NonUtf8Name` itself.

use crate::keys::syncable_name;
use crate::remote::RemoteFile;
use crate::state::Base;

/// Exclusive size limit for either side of a note.
pub const MAX_SIZE: u64 = 150 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
    pub name: String,
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    TooLarge,
    BadName,
    NotANote,
    CaseCollision,
    NonUtf8Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Upload {
        key: String,
        expect_rev: Option<String>,
    },
    Download {
        key: String,
    },
    DeleteRemote {
        key: String,
        rev: String,
    },
    TrashLocal {
        key: String,
    },
    /// Record or refresh the base from equal local and remote content.
    Adopt {
        key: String,
    },
    Conflict {
        key: String,
    },
    ForgetBase {
        key: String,
    },
    Skip {
        key: String,
        reason: SkipReason,
    },
    /// Both sides and the recorded revision are already in sync.
    Nothing {
        key: String,
    },
}

impl Action {
    pub fn key(&self) -> &str {
        match self {
            Self::Upload { key, .. }
            | Self::Download { key }
            | Self::DeleteRemote { key, .. }
            | Self::TrashLocal { key }
            | Self::Adopt { key }
            | Self::Conflict { key }
            | Self::ForgetBase { key }
            | Self::Skip { key, .. }
            | Self::Nothing { key } => key,
        }
    }

    pub fn is_destructive(&self) -> bool {
        matches!(self, Self::DeleteRemote { .. } | Self::TrashLocal { .. })
    }
}

/// Plan without I/O. Equality uses content hashes, never revisions; conditional
/// writes and deletes always use the current remote revision, not the base's.
pub fn reconcile(
    key: &str,
    base: Option<&Base>,
    local: Option<&LocalFile>,
    remote: Option<&RemoteFile>,
) -> Action {
    let key = key.to_owned();
    if local.is_some_and(|file| file.size >= MAX_SIZE)
        || remote.is_some_and(|file| file.size >= MAX_SIZE)
    {
        return Action::Skip {
            key,
            reason: SkipReason::TooLarge,
        };
    }
    if let Some(local) = local {
        if let Err(reason) = syncable_name(&local.name) {
            return Action::Skip { key, reason };
        }
    }

    match (base, local, remote) {
        (_, None, None) => Action::ForgetBase { key },
        (base, Some(local), None) => {
            if base.is_some_and(|base| local.hash == base.hash) {
                Action::TrashLocal { key }
            } else {
                Action::Upload {
                    key,
                    expect_rev: None,
                }
            }
        }
        (base, None, Some(remote)) => {
            if base.is_some_and(|base| remote.content_hash == base.hash) {
                Action::DeleteRemote {
                    key,
                    rev: remote.rev.clone(),
                }
            } else {
                Action::Download { key }
            }
        }
        (base, Some(local), Some(remote)) => {
            if local.hash == remote.content_hash {
                if base.is_some_and(|base| local.hash == base.hash && remote.rev == base.rev) {
                    Action::Nothing { key }
                } else {
                    Action::Adopt { key }
                }
            } else if base.is_some_and(|base| remote.content_hash == base.hash) {
                Action::Upload {
                    key,
                    expect_rev: Some(remote.rev.clone()),
                }
            } else if base.is_some_and(|base| local.hash == base.hash) {
                Action::Download { key }
            } else {
                Action::Conflict { key }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::time::UNIX_EPOCH;

    const KEY: &str = "/note.txt";

    fn base(hash: &str, rev: &str) -> Base {
        Base {
            id: "id:note".into(),
            rev: rev.into(),
            hash: hash.into(),
            local_name: "Note.txt".into(),
            size: 1,
            mtime: UNIX_EPOCH,
        }
    }

    fn local(hash: &str) -> LocalFile {
        LocalFile {
            name: "Note.txt".into(),
            hash: hash.into(),
            size: 1,
        }
    }

    fn remote(hash: &str, rev: &str) -> RemoteFile {
        RemoteFile {
            id: "id:note".into(),
            name: "Note.txt".into(),
            key: KEY.into(),
            rev: rev.into(),
            content_hash: hash.into(),
            size: 1,
        }
    }

    #[test]
    fn no_base_local_present_remote_absent() {
        let action = reconcile(KEY, None, Some(&local("h1")), None);
        assert_eq!(
            action,
            Action::Upload {
                key: KEY.into(),
                expect_rev: None,
            }
        );
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn no_base_local_absent_remote_present() {
        let action = reconcile(KEY, None, None, Some(&remote("h1", "r1")));
        assert_eq!(action, Action::Download { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn no_base_both_present_same_hash() {
        let action = reconcile(KEY, None, Some(&local("h1")), Some(&remote("h1", "r1")));
        assert_eq!(action, Action::Adopt { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn no_base_both_present_different_hash() {
        let action = reconcile(KEY, None, Some(&local("h1")), Some(&remote("h2", "r1")));
        assert_eq!(action, Action::Conflict { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn no_base_both_absent() {
        let action = reconcile(KEY, None, None, None);
        assert_eq!(action, Action::ForgetBase { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn base_both_same_hash_and_revision() {
        let action = reconcile(
            KEY,
            Some(&base("h1", "r1")),
            Some(&local("h1")),
            Some(&remote("h1", "r1")),
        );
        assert_eq!(action, Action::Nothing { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn base_both_same_hash_remote_touched() {
        let action = reconcile(
            KEY,
            Some(&base("h1", "r1")),
            Some(&local("h1")),
            Some(&remote("h1", "r2")),
        );
        assert_eq!(action, Action::Adopt { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn base_local_changed_remote_same() {
        let action = reconcile(
            KEY,
            Some(&base("h1", "r1")),
            Some(&local("h2")),
            Some(&remote("h1", "r2")),
        );
        assert_eq!(
            action,
            Action::Upload {
                key: KEY.into(),
                expect_rev: Some("r2".into()),
            }
        );
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn base_local_same_remote_changed() {
        let action = reconcile(
            KEY,
            Some(&base("h1", "r1")),
            Some(&local("h1")),
            Some(&remote("h2", "r2")),
        );
        assert_eq!(action, Action::Download { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn base_both_changed_same_hash() {
        let action = reconcile(
            KEY,
            Some(&base("h1", "r1")),
            Some(&local("h2")),
            Some(&remote("h2", "r2")),
        );
        assert_eq!(action, Action::Adopt { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn base_both_changed_different_hash() {
        let action = reconcile(
            KEY,
            Some(&base("h1", "r1")),
            Some(&local("h2")),
            Some(&remote("h3", "r2")),
        );
        assert_eq!(action, Action::Conflict { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn base_local_deleted_remote_same() {
        let action = reconcile(
            KEY,
            Some(&base("h1", "r1")),
            None,
            Some(&remote("h1", "r2")),
        );
        assert_eq!(
            action,
            Action::DeleteRemote {
                key: KEY.into(),
                rev: "r2".into(),
            }
        );
        assert_eq!(action.key(), KEY);
        assert!(action.is_destructive());
    }

    #[test]
    fn base_local_deleted_remote_changed() {
        let action = reconcile(
            KEY,
            Some(&base("h1", "r1")),
            None,
            Some(&remote("h2", "r2")),
        );
        assert_eq!(action, Action::Download { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn base_local_same_remote_deleted() {
        let action = reconcile(KEY, Some(&base("h1", "r1")), Some(&local("h1")), None);
        assert_eq!(action, Action::TrashLocal { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(action.is_destructive());
    }

    #[test]
    fn base_local_changed_remote_deleted() {
        let action = reconcile(KEY, Some(&base("h1", "r1")), Some(&local("h2")), None);
        assert_eq!(
            action,
            Action::Upload {
                key: KEY.into(),
                expect_rev: None,
            }
        );
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn base_both_deleted() {
        let action = reconcile(KEY, Some(&base("h1", "r1")), None, None);
        assert_eq!(action, Action::ForgetBase { key: KEY.into() });
        assert_eq!(action.key(), KEY);
        assert!(!action.is_destructive());
    }

    #[test]
    fn size_limit_is_exclusive_and_checked_before_names() {
        for size in [MAX_SIZE, MAX_SIZE + 1] {
            let mut local = local("h1");
            local.size = size;
            local.name = "bad\\name.jpg".into();
            let mut remote = remote("h1", "r1");
            remote.size = size;
            for (local, remote) in [(Some(&local), None), (None, Some(&remote))] {
                let action = reconcile(KEY, Some(&base("h1", "r1")), local, remote);
                assert_eq!(
                    action,
                    Action::Skip {
                        key: KEY.into(),
                        reason: SkipReason::TooLarge,
                    }
                );
                assert_eq!(action.key(), KEY);
                assert!(!action.is_destructive());
            }
        }
        let mut local = local("h1");
        local.size = MAX_SIZE - 1;
        let mut remote = remote("h1", "r1");
        remote.size = MAX_SIZE - 1;
        assert_eq!(
            reconcile(KEY, None, Some(&local), Some(&remote)),
            Action::Adopt { key: KEY.into() }
        );
    }

    #[test]
    fn invalid_local_names_are_skipped_before_reconciliation() {
        for (name, reason) in [
            ("bad\\name.txt", SkipReason::BadName),
            ("image.jpg", SkipReason::NotANote),
        ] {
            let mut local = local("h1");
            local.name = name.into();
            assert_eq!(
                reconcile(KEY, Some(&base("h1", "r1")), Some(&local), None),
                Action::Skip {
                    key: KEY.into(),
                    reason,
                }
            );
        }
    }

    proptest! {
        #[test]
        fn reconciliation_preserves_content_and_uses_current_revision(
            base_data in prop::option::of((
                prop::sample::select(vec!["h1", "h2", "h3"]),
                prop::sample::select(vec!["r1", "r2"]),
            )),
            local_hash in prop::option::of(prop::sample::select(vec!["h1", "h2", "h3"])),
            remote_data in prop::option::of((
                prop::sample::select(vec!["h1", "h2", "h3"]),
                prop::sample::select(vec!["r1", "r2"]),
            )),
        ) {
            let base = base_data.map(|(hash, rev)| base(hash, rev));
            let local = local_hash.map(local);
            let remote = remote_data.map(|(hash, rev)| remote(hash, rev));
            let action = reconcile(KEY, base.as_ref(), local.as_ref(), remote.as_ref());
            prop_assert_eq!(action.key(), KEY);
            if base.is_none() {
                prop_assert!(!action.is_destructive());
            }
            match action {
                Action::DeleteRemote { rev, .. } => {
                    prop_assert!(local.is_none());
                    prop_assert!(base.is_some() && remote.is_some());
                    let base = base.as_ref().unwrap();
                    let remote = remote.as_ref().unwrap();
                    prop_assert_eq!(&remote.content_hash, &base.hash);
                    prop_assert_eq!(rev, remote.rev.as_str());
                }
                Action::TrashLocal { .. } => {
                    prop_assert!(remote.is_none());
                    prop_assert!(base.is_some() && local.is_some());
                    prop_assert_eq!(&local.as_ref().unwrap().hash, &base.as_ref().unwrap().hash);
                }
                Action::Upload { expect_rev, .. } => {
                    prop_assert_eq!(
                        expect_rev.as_deref(),
                        remote.as_ref().map(|remote| remote.rev.as_str())
                    );
                }
                _ => {}
            }
        }
    }
}
