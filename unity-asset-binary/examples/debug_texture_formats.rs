//! Debug texture format decoding
//!
//! This example helps debug specific texture format issues.

use unity_asset_binary::{
    Texture2D, Texture2DConverter, TextureFormat, UnityVersion
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Debug Texture Format Decoding");
    println!("=============================");

    let converter = Texture2DConverter::new(UnityVersion::default());
    
    // Test RGBA4444 specifically
    println!("\n🔍 Testing RGBA4444:");
    test_rgba4444(&converter);
    
    // Test RGB565 specifically  
    println!("\n🔍 Testing RGB565:");
    test_rgb565(&converter);

    Ok(())
}

fn test_rgba4444(converter: &Texture2DConverter) {
    // Create a simple 2x2 RGBA4444 texture
    // Each pixel is 2 bytes, so 8 bytes total
    let image_data = vec![
        0xFF, 0x0F, // Pixel 1: R=15, G=15, B=15, A=0 (white, transparent)
        0x0F, 0xF0, // Pixel 2: R=0, G=15, B=15, A=15 (cyan, opaque)
        0xF0, 0x0F, // Pixel 3: R=15, G=0, B=0, A=15 (red, opaque)
        0x00, 0xFF, // Pixel 4: R=0, G=0, B=15, A=15 (blue, opaque)
    ];
    
    let texture = Texture2D {
        name: "RGBA4444_Test".to_string(),
        width: 2,
        height: 2,
        complete_image_size: image_data.len() as i32,
        format: TextureFormat::RGBA4444,
        mip_map: false,
        is_readable: true,
        data_size: image_data.len() as i32,
        image_data,
        ..Default::default()
    };
    
    println!("  Input data: {:?}", texture.image_data);
    println!("  Data size: {} bytes", texture.data_size);
    
    match converter.decode_to_image(&texture) {
        Ok(image) => {
            println!("  ✅ Successfully decoded RGBA4444!");
            println!("  Output: {}x{} RGBA", image.width(), image.height());
            
            // Print first few pixels for debugging
            let pixels = image.pixels().take(4).collect::<Vec<_>>();
            for (i, pixel) in pixels.iter().enumerate() {
                println!("  Pixel {}: R={}, G={}, B={}, A={}", 
                    i, pixel[0], pixel[1], pixel[2], pixel[3]);
            }
        }
        Err(e) => {
            println!("  ❌ Failed to decode RGBA4444: {}", e);
        }
    }
}

fn test_rgb565(converter: &Texture2DConverter) {
    // Create a simple 2x2 RGB565 texture
    // Each pixel is 2 bytes, so 8 bytes total
    let image_data = vec![
        0x1F, 0x00, // Pixel 1: Red (R=31, G=0, B=0)
        0xE0, 0x07, // Pixel 2: Green (R=0, G=63, B=0)
        0x00, 0xF8, // Pixel 3: Blue (R=0, G=0, B=31)
        0xFF, 0xFF, // Pixel 4: White (R=31, G=63, B=31)
    ];
    
    let texture = Texture2D {
        name: "RGB565_Test".to_string(),
        width: 2,
        height: 2,
        complete_image_size: image_data.len() as i32,
        format: TextureFormat::RGB565,
        mip_map: false,
        is_readable: true,
        data_size: image_data.len() as i32,
        image_data,
        ..Default::default()
    };
    
    println!("  Input data: {:?}", texture.image_data);
    println!("  Data size: {} bytes", texture.data_size);
    
    match converter.decode_to_image(&texture) {
        Ok(image) => {
            println!("  ✅ Successfully decoded RGB565!");
            println!("  Output: {}x{} RGBA", image.width(), image.height());
            
            // Print first few pixels for debugging
            let pixels = image.pixels().take(4).collect::<Vec<_>>();
            for (i, pixel) in pixels.iter().enumerate() {
                println!("  Pixel {}: R={}, G={}, B={}, A={}", 
                    i, pixel[0], pixel[1], pixel[2], pixel[3]);
            }
        }
        Err(e) => {
            println!("  ❌ Failed to decode RGB565: {}", e);
        }
    }
}
