use super::*;
use pretty_assertions::assert_eq;

fn fixture(directory: &Path) -> Manifest {
    let mut manifest = crate::tests::manifest(directory);
    manifest.machines.push(Machine {
        id: "machine".into(),
        label: "Machine".into(),
        servers: vec![Server {
            id: "server".into(),
            label: "Server".into(),
            endpoint: Endpoint::Local {
                socket: directory.join("socket"),
            },
            cancellation_adapters: Vec::new(),
            environment_capabilities: Default::default(),
        }],
    });
    manifest
}

#[tokio::test]
async fn declarations_trim_and_deduplicate_tags_without_resolving_environment_ids() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut manifest = fixture(directory.path());
    manifest.machines[0].servers[0]
        .environment_capabilities
        .insert(
            "Unknown-Environment".into(),
            vec![" cuda ".into(), "cuda".into(), "Clang".into()],
        );
    manifest.machines[0].servers[0]
        .environment_capabilities
        .insert("empty-capabilities".into(), vec![]);
    let path = directory.path().join("manifest.toml");
    tokio::fs::write(&path, toml::to_string(&manifest)?).await?;
    let loaded = Manifest::load(&path).await?;
    assert_eq!(
        loaded.machines[0].servers[0].environment_capabilities,
        std::collections::BTreeMap::from([
            (
                "Unknown-Environment".into(),
                vec!["cuda".into(), "Clang".into()]
            ),
            ("empty-capabilities".into(), vec![])
        ])
    );
    Ok(())
}

#[test]
fn blank_declaration_ids_and_tags_are_rejected_by_direct_validation() {
    for (id, tags) in [(" ", vec!["clang".into()]), ("local", vec![" ".into()])] {
        let mut manifest = fixture(Path::new("."));
        manifest.machines[0].servers[0]
            .environment_capabilities
            .insert(id.into(), tags);
        assert!(manifest.validate().is_err());
    }
}
