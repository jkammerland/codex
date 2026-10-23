use super::*;
use pretty_assertions::assert_eq;

#[test]
fn enrollment_authorization_is_stable_and_revoked_with_membership() {
    let root = ThreadId::new();
    let enrolled = authorization_section(Some(root), None);
    let (snapshot, first) = enrolled.render_diff(PreviousWorldStateSection::Absent);
    let first = first.expect("new enrollment must authorize collaboration");
    assert_eq!(first.role(), "developer");
    assert!(
        first
            .body()
            .clone()
            .into_text()
            .unwrap()
            .contains(AUTHORIZED)
    );
    assert!(
        enrolled
            .render_diff(PreviousWorldStateSection::Known(snapshot.as_ref().unwrap()))
            .1
            .is_none()
    );

    let revoked = authorization_section(None, snapshot.clone());
    assert!(
        revoked.matches_retained_fragment("developer", &format!("{START}\n{AUTHORIZED}\n{END}"))
    );
    let (snapshot, changed) =
        revoked.render_diff(PreviousWorldStateSection::Known(snapshot.as_ref().unwrap()));
    assert!(
        changed
            .unwrap()
            .body()
            .clone()
            .into_text()
            .unwrap()
            .contains(REVOKED)
    );
    assert!(
        revoked
            .render_diff(PreviousWorldStateSection::Known(snapshot.as_ref().unwrap()))
            .1
            .is_none()
    );
    assert!(
        authorization_section(None, None)
            .render_diff(PreviousWorldStateSection::Absent)
            .1
            .is_none()
    );
}

#[test]
fn authorization_restores_after_history_compaction() {
    let section = authorization_section(Some(ThreadId::new()), None);
    let (_, restored) = section.render_diff(PreviousWorldStateSection::Unknown);
    let restored = restored.unwrap();
    let text = format!(
        "{START}\n{}\n{END}",
        restored.body().clone().into_text().unwrap()
    );
    assert!(section.has_retained_fragment_matcher());
    assert!(section.matches_retained_fragment("developer", &text));
    assert!(!section.matches_retained_fragment("assistant", &text));
    assert!(!section.matches_retained_fragment("developer", "unrelated context"));
}

#[test]
fn revocation_restores_after_compaction_without_reviving_an_older_grant() {
    let section = authorization_section(None, Some(Value::Bool(false)));
    assert!(
        !section.matches_retained_fragment("developer", &format!("{START}\n{AUTHORIZED}\n{END}"))
    );
    let (snapshot, restored) = section.render_diff(PreviousWorldStateSection::Absent);
    assert_eq!(snapshot, Some(Value::Bool(false)));
    assert!(
        restored
            .unwrap()
            .body()
            .clone()
            .into_text()
            .unwrap()
            .contains(REVOKED)
    );
    assert!(section.matches_retained_fragment("developer", &format!("{START}\n{REVOKED}\n{END}")));
    assert!(
        section
            .render_diff(PreviousWorldStateSection::Known(&Value::Bool(false)))
            .1
            .is_none()
    );
}
