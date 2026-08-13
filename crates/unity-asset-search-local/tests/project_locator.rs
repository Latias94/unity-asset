use std::fs;

use unity_asset_search_local::{ProjectLocatorError, ProjectLocatorV1};

fn create_project(path: &std::path::Path) {
    fs::create_dir_all(path.join("Assets")).unwrap();
    fs::create_dir_all(path.join("ProjectSettings")).unwrap();
}

fn copy_directory(source: &std::path::Path, destination: &std::path::Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

#[test]
fn explicit_unity_project_root_is_required() {
    let temporary = tempfile::tempdir().unwrap();
    assert!(matches!(
        ProjectLocatorV1::open(temporary.path()),
        Err(ProjectLocatorError::MissingMarker {
            marker: "Assets",
            ..
        })
    ));

    fs::create_dir(temporary.path().join("Assets")).unwrap();
    assert!(matches!(
        ProjectLocatorV1::open(temporary.path()),
        Err(ProjectLocatorError::MissingMarker {
            marker: "ProjectSettings",
            ..
        })
    ));
}

#[test]
fn same_filesystem_rename_preserves_project_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let before = temporary.path().join("before");
    let after = temporary.path().join("after");
    create_project(&before);

    let first = ProjectLocatorV1::open(&before).unwrap();
    fs::rename(&before, &after).unwrap();
    let renamed = ProjectLocatorV1::open(&after).unwrap();

    assert_eq!(first.project_id(), renamed.project_id());
    assert_ne!(first.root(), renamed.root());
}

#[test]
fn copied_project_root_receives_a_distinct_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let original = temporary.path().join("original");
    let copied = temporary.path().join("copied");
    create_project(&original);
    fs::write(original.join("Assets/Scene.unity"), b"scene").unwrap();
    fs::write(
        original.join("ProjectSettings/ProjectVersion.txt"),
        b"m_EditorVersion: 6000.0",
    )
    .unwrap();
    copy_directory(&original, &copied);

    let original_identity = ProjectLocatorV1::open(&original).unwrap();
    let copied_identity = ProjectLocatorV1::open(&copied).unwrap();

    assert_eq!(
        fs::read(original.join("Assets/Scene.unity")).unwrap(),
        fs::read(copied.join("Assets/Scene.unity")).unwrap()
    );
    assert_ne!(original_identity.project_id(), copied_identity.project_id());
}

#[test]
fn empty_project_root_is_rejected() {
    assert!(matches!(
        ProjectLocatorV1::open(""),
        Err(ProjectLocatorError::EmptyRoot)
    ));
}

#[test]
fn revalidation_rejects_a_replacement_at_the_original_path() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let moved = temporary.path().join("moved");
    create_project(&project);
    let located = ProjectLocatorV1::open(&project).unwrap();

    fs::rename(&project, &moved).unwrap();
    create_project(&project);

    assert!(matches!(
        located.revalidate(),
        Err(ProjectLocatorError::IdentityChanged { .. })
    ));
}

#[test]
fn revalidation_rejects_a_marker_replaced_by_a_file() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    create_project(&project);
    let located = ProjectLocatorV1::open(&project).unwrap();

    fs::remove_dir(project.join("Assets")).unwrap();
    fs::write(project.join("Assets"), b"not a directory").unwrap();

    assert!(matches!(
        located.revalidate(),
        Err(ProjectLocatorError::InvalidMarker {
            marker: "Assets",
            ..
        })
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn linked_root_and_marker_are_rejected() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let linked_project = temporary.path().join("linked-project");
    create_project(&project);
    symlink(&project, &linked_project).unwrap();
    assert!(matches!(
        ProjectLocatorV1::open(&linked_project),
        Err(ProjectLocatorError::InvalidRoot { .. })
    ));

    let linked_marker_project = temporary.path().join("linked-marker-project");
    fs::create_dir(&linked_marker_project).unwrap();
    fs::create_dir(linked_marker_project.join("ProjectSettings")).unwrap();
    symlink(project.join("Assets"), linked_marker_project.join("Assets")).unwrap();
    assert!(matches!(
        ProjectLocatorV1::open(&linked_marker_project),
        Err(ProjectLocatorError::InvalidMarker {
            marker: "Assets",
            ..
        })
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn ancestor_alias_is_accepted_but_remains_part_of_the_binding() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let physical_parent = temporary.path().join("physical");
    let replacement_parent = temporary.path().join("replacement");
    let physical_project = physical_parent.join("project");
    let replacement_project = replacement_parent.join("project");
    let alias = temporary.path().join("alias");
    create_project(&physical_project);
    create_project(&replacement_project);
    symlink(&physical_parent, &alias).unwrap();

    let located = ProjectLocatorV1::open(alias.join("project")).unwrap();

    assert_eq!(located.root(), physical_project.canonicalize().unwrap());
    located.revalidate().unwrap();

    fs::remove_file(&alias).unwrap();
    symlink(&replacement_parent, &alias).unwrap();

    assert!(matches!(
        located.revalidate(),
        Err(ProjectLocatorError::IdentityChanged { .. })
    ));
}
