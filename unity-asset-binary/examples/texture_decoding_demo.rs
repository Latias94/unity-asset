//! Texture2D decoding demonstration
//!
//! This example shows how to use the Texture2DConverter to decode texture data.

use unity_asset_binary::{Texture2D, Texture2DConverter, TextureFormat, UnityVersion};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Unity Asset Parser - Texture2D Decoding Demo");
    println!("============================================");

    // Create a simple RGBA32 texture for demonstration
    let mut texture = create_demo_texture();

    // Create converter
    let converter = Texture2DConverter::new(UnityVersion::default());

    println!("\nTexture Information:");
    println!("  Name: {}", texture.name);
    println!("  Dimensions: {}x{}", texture.width, texture.height);
    println!("  Format: {:?}", texture.format);
    println!("  Data size: {} bytes", texture.data_size);

    // Decode the texture
    match converter.decode_to_image(&texture) {
        Ok(image) => {
            println!("\n✅ Successfully decoded texture!");
            println!("  Output image: {}x{} RGBA", image.width(), image.height());

            // Save as PNG for verification
            let output_path = "target/decoded_texture_demo.png";
            std::fs::create_dir_all("target").ok();

            match image.save(output_path) {
                Ok(()) => println!("  Saved to: {}", output_path),
                Err(e) => println!("  Failed to save: {}", e),
            }
        }
        Err(e) => {
            println!("\n❌ Failed to decode texture: {}", e);
        }
    }

    // Test different formats
    println!("\n🧪 Testing different texture formats:");
    test_format_support(&converter);

    Ok(())
}

fn create_demo_texture() -> Texture2D {
    // Create a simple 4x4 RGBA32 texture with a gradient pattern
    let width = 4;
    let height = 4;
    let mut image_data = Vec::new();

    for y in 0..height {
        for x in 0..width {
            // Create a simple gradient pattern
            let r = (x * 255 / (width - 1)) as u8;
            let g = (y * 255 / (height - 1)) as u8;
            let b = 128u8; // Constant blue
            let a = 255u8; // Fully opaque

            image_data.extend_from_slice(&[r, g, b, a]);
        }
    }

    Texture2D {
        name: "DemoTexture".to_string(),
        width: width as i32,
        height: height as i32,
        complete_image_size: image_data.len() as i32,
        format: TextureFormat::RGBA32,
        mip_map: false,
        is_readable: true,
        data_size: image_data.len() as i32,
        image_data,
        ..Default::default()
    }
}

fn test_format_support(converter: &Texture2DConverter) {
    let test_formats = vec![
        TextureFormat::RGBA32,
        TextureFormat::RGB24,
        TextureFormat::ARGB32,
        TextureFormat::BGRA32,
        TextureFormat::Alpha8,
        TextureFormat::RGBA4444,
        TextureFormat::RGB565,
    ];

    for format in test_formats {
        let mut test_texture = create_test_texture_for_format(format);

        match converter.decode_to_image(&test_texture) {
            Ok(_) => println!("  ✅ {:?} - Supported", format),
            Err(_) => println!("  ❌ {:?} - Not supported", format),
        }
    }
}

fn create_test_texture_for_format(format: TextureFormat) -> Texture2D {
    let width = 2;
    let height = 2;

    let image_data = match format {
        TextureFormat::RGBA32 => {
            // 4 bytes per pixel: RGBA
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ]
        }
        TextureFormat::RGB24 => {
            // 3 bytes per pixel: RGB
            vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]
        }
        TextureFormat::ARGB32 => {
            // 4 bytes per pixel: ARGB
            vec![
                255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255,
            ]
        }
        TextureFormat::BGRA32 => {
            // 4 bytes per pixel: BGRA
            vec![
                0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
            ]
        }
        TextureFormat::Alpha8 => {
            // 1 byte per pixel: Alpha only
            vec![255, 128, 64, 0]
        }
        TextureFormat::RGBA4444 => {
            // 2 bytes per pixel: 4-bit channels
            vec![0xFF, 0x0F, 0xF0, 0xFF, 0x0F, 0xF0, 0xFF, 0xFF]
        }
        TextureFormat::RGB565 => {
            // 2 bytes per pixel: 5-6-5 bit channels
            vec![0x1F, 0x00, 0xE0, 0x07, 0x00, 0xF8, 0xFF, 0xFF]
        }
        _ => vec![255, 255, 255, 255], // Default fallback
    };

    Texture2D {
        name: format!("Test_{:?}", format),
        width: width as i32,
        height: height as i32,
        complete_image_size: image_data.len() as i32,
        format,
        mip_map: false,
        is_readable: true,
        data_size: image_data.len() as i32,
        image_data,
        ..Default::default()
    }
}
