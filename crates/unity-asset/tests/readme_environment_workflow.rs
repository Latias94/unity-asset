use unity_asset::AssetLoadBudget;
use unity_asset::environment::{Environment, EnvironmentObjectRef};
use unity_asset::{UnityDocument, YamlDocument};

fn readme_yaml_workflow(path: &std::path::Path) -> unity_asset::Result<()> {
    let (doc, warnings) = YamlDocument::load_yaml_with_warnings(path, false)?;
    for warning in warnings {
        eprintln!("warning: {}", warning);
    }
    let _ = doc.entries().len();
    let _ = doc.get(Some("PlayerSettings"), None);
    Ok(())
}

fn script_typetree_workflow(
    registry_path: std::path::PathBuf,
    input: std::path::PathBuf,
) -> unity_asset::Result<Environment> {
    let mut env = Environment::new();
    let mut budget = AssetLoadBudget::default();
    env.set_type_tree_registry_from_paths(&[registry_path], &mut budget)?;
    env.load(input, &mut budget)?;
    Ok(env)
}

fn readme_environment_workflow() -> unity_asset::Result<()> {
    let mut env = Environment::new();

    let mut budget = AssetLoadBudget::default();
    env.load("tests/samples", &mut budget)?;

    let sources = env.binary_sources();
    if let Some((_kind, source)) = sources.first()
        && let Some(object_ref) = env.find_binary_object_in_source_id(source, 1)
    {
        let key = object_ref.key();
        let _parsed = env.read_binary_object_key(&key, &mut budget)?;
    }

    if let Some(object_ref) = env.find_binary_object(1) {
        let _pptr_object = env.read_binary_pptr(&object_ref, 0, 1, &mut budget)?;
    }

    let container = env.find_binary_object_keys_in_bundle_container("Assets/", &mut budget)?;
    for (asset_path, key) in container.into_iter().take(10) {
        let _object = env.read_binary_object_key(&key, &mut budget)?;
        println!("{} -> path_id={}", asset_path, key.path_id);
    }

    for object in env.objects() {
        match object {
            EnvironmentObjectRef::Yaml(class) => {
                let _ = &class.class_name;
            }
            EnvironmentObjectRef::Binary(object_ref) => {
                let _parsed = object_ref.read(&mut budget)?;
                let _key = object_ref.key();
            }
        }
    }

    Ok(())
}

#[test]
fn readme_environment_workflow_is_type_checked() {
    let _: fn() -> unity_asset::Result<()> = readme_environment_workflow;
    let _: fn(&std::path::Path) -> unity_asset::Result<()> = readme_yaml_workflow;
    let _: fn(std::path::PathBuf, std::path::PathBuf) -> unity_asset::Result<Environment> =
        script_typetree_workflow;
}
