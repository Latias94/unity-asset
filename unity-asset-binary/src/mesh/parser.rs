//! Mesh parsing implementation
//!
//! This module provides the main parsing logic for Unity Mesh objects.

use super::types::*;
use crate::error::Result;
use crate::object::UnityObject;
use crate::reader::BinaryReader;
use crate::unity_version::UnityVersion;
use indexmap::IndexMap;
use unity_asset_core::UnityValue;

/// Mesh parser
///
/// This struct provides methods for parsing Unity Mesh objects from
/// various data sources including TypeTree and binary data.
pub struct MeshParser {
    version: UnityVersion,
}

impl MeshParser {
    /// Create a new mesh parser
    pub fn new(version: UnityVersion) -> Self {
        Self { version }
    }

    /// Parse Mesh from UnityObject
    pub fn parse_from_unity_object(&self, obj: &UnityObject) -> Result<MeshResult> {
        let mesh = if let Some(type_tree) = &obj.info.type_tree {
            let properties = obj.parse_with_typetree(type_tree)?;
            self.parse_from_typetree(&properties)?
        } else {
            self.parse_from_binary_data(&obj.info.data)?
        };

        Ok(MeshResult::new(mesh))
    }

    /// Parse Mesh from TypeTree properties
    pub fn parse_from_typetree(&self, properties: &IndexMap<String, UnityValue>) -> Result<Mesh> {
        let mut mesh = Mesh::default();

        // Extract name
        if let Some(UnityValue::String(name)) = properties.get("m_Name") {
            mesh.name = name.clone();
        }

        // Extract sub meshes
        if let Some(sub_meshes_value) = properties.get("m_SubMeshes") {
            self.extract_sub_meshes(&mut mesh, sub_meshes_value)?;
        }

        // Extract vertex data
        if let Some(vertex_data_value) = properties.get("m_VertexData") {
            self.extract_vertex_data(&mut mesh, vertex_data_value)?;
        }

        // Extract index buffer
        if let Some(index_buffer_value) = properties.get("m_IndexBuffer") {
            self.extract_index_buffer(&mut mesh, index_buffer_value)?;
        }

        // Extract readable flag
        if let Some(UnityValue::Bool(is_readable)) = properties.get("m_IsReadable") {
            mesh.is_readable = *is_readable;
        }

        // Extract local AABB
        if let Some(local_aabb_value) = properties.get("m_LocalAABB") {
            self.extract_local_aabb(&mut mesh, local_aabb_value)?;
        }

        // Extract mesh compression
        if let Some(UnityValue::Integer(compression)) = properties.get("m_MeshCompression") {
            mesh.mesh_compression = *compression as u8;
        }

        // Extract streaming info if present
        if let Some(stream_data) = properties.get("m_StreamData") {
            mesh.stream_data = self.extract_stream_data(stream_data)?;
        }

        // Extract blend shape data
        if let Some(blend_shapes_value) = properties.get("m_Shapes") {
            mesh.blend_shape_data = self.extract_blend_shapes(blend_shapes_value)?;
        }

        // Extract bind poses
        if let Some(bind_poses_value) = properties.get("m_BindPose") {
            self.extract_bind_poses(&mut mesh, bind_poses_value)?;
        }

        Ok(mesh)
    }

    /// Parse Mesh from raw binary data (fallback method)
    pub fn parse_from_binary_data(&self, data: &[u8]) -> Result<Mesh> {
        let mut reader = BinaryReader::new(data, crate::reader::ByteOrder::Little);
        let mut mesh = Mesh::default();

        // Read name (aligned string)
        mesh.name = reader.read_aligned_string()?;

        // Read basic properties
        mesh.is_readable = reader.read_bool()?;
        mesh.keep_vertices = reader.read_bool()?;
        mesh.keep_indices = reader.read_bool()?;

        // Read index format
        mesh.index_format = reader.read_i32()?;

        // Read index buffer size and data
        let index_buffer_size = reader.read_i32()? as usize;
        if index_buffer_size > 0 {
            mesh.index_buffer = reader.read_bytes(index_buffer_size)?;
        }

        // Read mesh compression
        mesh.mesh_compression = reader.read_u8()?;

        Ok(mesh)
    }

    /// Extract sub meshes from UnityValue
    fn extract_sub_meshes(&self, mesh: &mut Mesh, value: &UnityValue) -> Result<()> {
        if let UnityValue::Array(sub_meshes_array) = value {
            mesh.sub_meshes.clear();
            for sub_mesh_value in sub_meshes_array {
                if let UnityValue::Object(sub_mesh_obj) = sub_mesh_value {
                    let mut sub_mesh = SubMesh::default();

                    if let Some(UnityValue::Integer(first_byte)) = sub_mesh_obj.get("firstByte") {
                        sub_mesh.first_byte = *first_byte as u32;
                    }
                    if let Some(UnityValue::Integer(index_count)) = sub_mesh_obj.get("indexCount") {
                        sub_mesh.index_count = *index_count as u32;
                    }
                    if let Some(UnityValue::Integer(topology)) = sub_mesh_obj.get("topology") {
                        sub_mesh.topology = *topology as i32;
                    }
                    if let Some(UnityValue::Integer(triangle_count)) =
                        sub_mesh_obj.get("triangleCount")
                    {
                        sub_mesh.triangle_count = *triangle_count as u32;
                    }

                    mesh.sub_meshes.push(sub_mesh);
                }
            }
        }
        Ok(())
    }

    /// Extract vertex data from UnityValue
    fn extract_vertex_data(&self, mesh: &mut Mesh, value: &UnityValue) -> Result<()> {
        if let UnityValue::Object(vertex_data_obj) = value {
            if let Some(UnityValue::Integer(vertex_count)) = vertex_data_obj.get("m_VertexCount") {
                mesh.vertex_data.vertex_count = *vertex_count as u32;
            }

            // Extract channels
            if let Some(channels_value) = vertex_data_obj.get("m_Channels") {
                self.extract_vertex_channels(&mut mesh.vertex_data, channels_value)?;
            }

            // Extract data size
            if let Some(data_size_value) = vertex_data_obj.get("m_DataSize") {
                if let UnityValue::Array(data_array) = data_size_value {
                    mesh.vertex_data.data_size.clear();
                    for data_item in data_array {
                        if let UnityValue::Integer(byte_val) = data_item {
                            mesh.vertex_data.data_size.push(*byte_val as u8);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Extract vertex channels from UnityValue
    fn extract_vertex_channels(
        &self,
        vertex_data: &mut VertexData,
        value: &UnityValue,
    ) -> Result<()> {
        if let UnityValue::Array(channels_array) = value {
            vertex_data.channels.clear();
            for channel_value in channels_array {
                if let UnityValue::Object(channel_obj) = channel_value {
                    let mut channel = ChannelInfo::default();

                    if let Some(UnityValue::Integer(stream)) = channel_obj.get("stream") {
                        channel.stream = *stream as u8;
                    }
                    if let Some(UnityValue::Integer(offset)) = channel_obj.get("offset") {
                        channel.offset = *offset as u8;
                    }
                    if let Some(UnityValue::Integer(format)) = channel_obj.get("format") {
                        channel.format = *format as u8;
                    }
                    if let Some(UnityValue::Integer(dimension)) = channel_obj.get("dimension") {
                        channel.dimension = *dimension as u8;
                    }

                    vertex_data.channels.push(channel);
                }
            }
        }
        Ok(())
    }

    /// Extract index buffer from UnityValue
    fn extract_index_buffer(&self, mesh: &mut Mesh, value: &UnityValue) -> Result<()> {
        match value {
            UnityValue::Array(arr) => {
                mesh.index_buffer.clear();
                for item in arr {
                    if let UnityValue::Integer(byte_val) = item {
                        mesh.index_buffer.push(*byte_val as u8);
                    }
                }
            }
            _ => {
                // Handle other formats if needed
            }
        }
        Ok(())
    }

    /// Extract local AABB from UnityValue
    fn extract_local_aabb(&self, mesh: &mut Mesh, value: &UnityValue) -> Result<()> {
        if let UnityValue::Object(aabb_obj) = value {
            // Extract center
            if let Some(center_value) = aabb_obj.get("m_Center") {
                if let UnityValue::Object(center_obj) = center_value {
                    if let Some(UnityValue::Float(x)) = center_obj.get("x") {
                        mesh.local_aabb.center_x = *x as f32;
                    }
                    if let Some(UnityValue::Float(y)) = center_obj.get("y") {
                        mesh.local_aabb.center_y = *y as f32;
                    }
                    if let Some(UnityValue::Float(z)) = center_obj.get("z") {
                        mesh.local_aabb.center_z = *z as f32;
                    }
                }
            }

            // Extract extent
            if let Some(extent_value) = aabb_obj.get("m_Extent") {
                if let UnityValue::Object(extent_obj) = extent_value {
                    if let Some(UnityValue::Float(x)) = extent_obj.get("x") {
                        mesh.local_aabb.extent_x = *x as f32;
                    }
                    if let Some(UnityValue::Float(y)) = extent_obj.get("y") {
                        mesh.local_aabb.extent_y = *y as f32;
                    }
                    if let Some(UnityValue::Float(z)) = extent_obj.get("z") {
                        mesh.local_aabb.extent_z = *z as f32;
                    }
                }
            }
        }
        Ok(())
    }

    /// Extract streaming data from UnityValue
    fn extract_stream_data(&self, value: &UnityValue) -> Result<Option<StreamingInfo>> {
        if let UnityValue::Object(stream_obj) = value {
            let mut stream_info = StreamingInfo::default();

            if let Some(UnityValue::Integer(offset)) = stream_obj.get("offset") {
                stream_info.offset = *offset as u64;
            }
            if let Some(UnityValue::Integer(size)) = stream_obj.get("size") {
                stream_info.size = *size as u32;
            }
            if let Some(UnityValue::String(path)) = stream_obj.get("path") {
                stream_info.path = path.clone();
            }

            // Only return stream info if it has valid data
            if stream_info.size > 0 || !stream_info.path.is_empty() {
                return Ok(Some(stream_info));
            }
        }
        Ok(None)
    }

    /// Extract blend shapes from UnityValue
    fn extract_blend_shapes(&self, _value: &UnityValue) -> Result<Option<BlendShapeData>> {
        // Blend shapes are complex structures
        // This is a placeholder implementation
        Ok(None)
    }

    /// Extract bind poses from UnityValue
    fn extract_bind_poses(&self, mesh: &mut Mesh, value: &UnityValue) -> Result<()> {
        if let UnityValue::Array(bind_poses_array) = value {
            mesh.bind_pose.clear();
            for bind_pose_value in bind_poses_array {
                if let UnityValue::Object(matrix_obj) = bind_pose_value {
                    let mut matrix = [0.0f32; 16];

                    // Extract matrix elements (simplified)
                    for i in 0..16 {
                        let key = format!("e{:02}", i);
                        if let Some(UnityValue::Float(val)) = matrix_obj.get(&key) {
                            matrix[i] = *val as f32;
                        }
                    }

                    mesh.bind_pose.push(matrix);
                }
            }
        }
        Ok(())
    }

    /// Get the Unity version
    pub fn version(&self) -> &UnityVersion {
        &self.version
    }

    /// Set the Unity version
    pub fn set_version(&mut self, version: UnityVersion) {
        self.version = version;
    }
}

impl Default for MeshParser {
    fn default() -> Self {
        Self::new(UnityVersion::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_creation() {
        let parser = MeshParser::new(UnityVersion::default());
        assert_eq!(parser.version(), &UnityVersion::default());
    }

    #[test]
    fn test_extract_local_aabb() {
        let parser = MeshParser::default();
        let mut mesh = Mesh::default();

        let mut center_obj = IndexMap::new();
        center_obj.insert("x".to_string(), UnityValue::Float(1.0));
        center_obj.insert("y".to_string(), UnityValue::Float(2.0));
        center_obj.insert("z".to_string(), UnityValue::Float(3.0));

        let mut extent_obj = IndexMap::new();
        extent_obj.insert("x".to_string(), UnityValue::Float(0.5));
        extent_obj.insert("y".to_string(), UnityValue::Float(1.0));
        extent_obj.insert("z".to_string(), UnityValue::Float(1.5));

        let mut aabb_obj = IndexMap::new();
        aabb_obj.insert("m_Center".to_string(), UnityValue::Object(center_obj));
        aabb_obj.insert("m_Extent".to_string(), UnityValue::Object(extent_obj));

        let aabb_value = UnityValue::Object(aabb_obj);
        parser.extract_local_aabb(&mut mesh, &aabb_value).unwrap();

        assert_eq!(mesh.local_aabb.center_x, 1.0);
        assert_eq!(mesh.local_aabb.center_y, 2.0);
        assert_eq!(mesh.local_aabb.center_z, 3.0);
        assert_eq!(mesh.local_aabb.extent_x, 0.5);
        assert_eq!(mesh.local_aabb.extent_y, 1.0);
        assert_eq!(mesh.local_aabb.extent_z, 1.5);
    }
}
