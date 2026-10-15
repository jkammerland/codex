use hmac::Hmac;
use hmac::Mac;
use sha2::Sha256;

use super::mentions;
use super::verify_signature;

#[test]
fn webhook_signature_covers_raw_bytes_and_rejects_changed_payloads() {
    let body = br#"{"action":"created","comment":{"body":"@reviewer review"}}"#;
    let mut mac = Hmac::<Sha256>::new_from_slice(b"test-secret").unwrap();
    mac.update(body);
    let signature = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert!(verify_signature(b"test-secret", &signature, body).is_ok());
    assert!(verify_signature(b"different-secret", &signature, body).is_err());
    assert!(verify_signature(b"test-secret", &signature, b"{}").is_err());
    assert!(verify_signature(b"test-secret", "", body).is_err());
}

#[test]
fn mentions_match_a_whole_alias_and_do_not_wake_similarly_named_agents() {
    assert!(mentions("Please @reviewer take a look", "reviewer"));
    assert!(mentions("(@builder/linux): compile", "builder/linux"));
    assert!(!mentions("@reviewer-two take a look", "reviewer"));
    assert!(!mentions("address@example.org", "example"));
}
