//! Resolve and read an object's streamed-resource range from an immutable workspace revision.
//!
//! Run:
//! `cargo run -p unity-asset --example workspace_streamed_resource -- <path> [path_id]`
//!
//! Resolution only uses streamed-resource members already present in the loaded source catalog;
//! it never probes the filesystem for an external `.resS` or `.resource` file.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::PathBuf;

use unity_asset::workspace::{
    AssetWorkspace, StreamedResourceRequest, StreamedResourceResolution, WorkspaceInspector,
    WorkspaceObjectFormatInspection,
};
use unity_asset::{AssetLoadBudget, UnityClass, UnityValue};

struct StreamDescriptor {
    path: String,
    offset: u64,
    size: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = arguments()?;
    let mut budget = AssetLoadBudget::default();
    let mut workspace = AssetWorkspace::new()?;
    workspace.load_path(&arguments.path, &mut budget)?;

    let snapshot = workspace.snapshot();
    let inspector = WorkspaceInspector::new(&snapshot);
    let mut objects = inspector.objects(&mut budget)?;
    objects.sort_by(|left, right| left.address().cmp(right.address()));
    let selected = objects.into_iter().find_map(|object| {
        let WorkspaceObjectFormatInspection::Binary { path_id, .. } = object.format() else {
            return None;
        };
        if arguments
            .path_id
            .is_some_and(|expected| path_id != expected)
        {
            return None;
        }
        let descriptor = extract_stream(object.object().class())?;
        Some((object, path_id, descriptor))
    });
    let (object, path_id, stream) = selected.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no matching binary object contains m_Resource or m_StreamData",
        )
    })?;

    let preview_size = stream.size.min(64);
    let request = StreamedResourceRequest::new(
        object.address().source_locator().clone(),
        stream.path.clone(),
        stream.offset,
        preview_size,
    )?;
    let result = inspector.resolve_streamed_resource(&request, &mut budget)?;
    let resource = match result.resolution() {
        StreamedResourceResolution::Resolved { resource } => resource,
        resolution => {
            let resolution = serde_json::to_string(resolution)?;
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("streamed-resource query did not resolve: {resolution}"),
            )
            .into());
        }
    };

    let range = resource.open(&snapshot, &mut budget)?;
    let preview_len = usize::try_from(range.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "preview range length does not fit usize",
        )
    })?;
    let mut preview = [0_u8; 64];
    range.reader().read_exact(&mut preview[..preview_len])?;

    println!(
        "object_address: {}",
        serde_json::to_string(object.address())?
    );
    println!("path_id: {path_id}");
    println!("class_id: {}", object.object().class().class_id());
    println!("class_name: {}", object.object().class().class_name());
    println!(
        "resource_source: {}",
        serde_json::to_string(resource.source().locator())?
    );
    println!("stream_path: {}", stream.path);
    println!("stream_offset: {}", stream.offset);
    println!("stream_size: {}", stream.size);
    print!("preview ({} bytes):", preview_len);
    for byte in &preview[..preview_len] {
        print!(" {byte:02x}");
    }
    println!();

    Ok(())
}

fn extract_stream(class: &UnityClass) -> Option<StreamDescriptor> {
    class
        .get("m_Resource")
        .and_then(|value| descriptor(value, "m_Source", "m_Offset", "m_Size"))
        .or_else(|| {
            class
                .get("m_StreamData")
                .and_then(|value| descriptor(value, "path", "offset", "size"))
        })
}

fn descriptor(
    value: &UnityValue,
    path_field: &str,
    offset_field: &str,
    size_field: &str,
) -> Option<StreamDescriptor> {
    let UnityValue::Object(fields) = value else {
        return None;
    };
    let path = fields.get(path_field)?.as_str()?;
    let offset = fields
        .get(offset_field)
        .and_then(UnityValue::as_u64)
        .unwrap_or(0);
    let size = fields.get(size_field)?.as_u64()?;
    if path.is_empty() || size == 0 {
        return None;
    }
    Some(StreamDescriptor {
        path: path.to_owned(),
        offset,
        size,
    })
}

struct Arguments {
    path: PathBuf,
    path_id: Option<i64>,
}

fn arguments() -> io::Result<Arguments> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments.next().map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: workspace_streamed_resource <path> [path_id]",
        )
    })?;
    let path_id = arguments.next().map(parse_path_id).transpose()?;
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: workspace_streamed_resource <path> [path_id]",
        ));
    }
    Ok(Arguments { path, path_id })
}

fn parse_path_id(value: OsString) -> io::Result<i64> {
    let value = value
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path_id must be UTF-8"))?;
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid path_id {value:?}: {error}"),
        )
    })
}
