use slang_inline::{SlangSource, slang_inline};

// Open this file on its own and run Slang Inline: Select Composition.
// The detailed composition adds specular to completion after material.
pub const SURFACE: SlangSource = slang_inline! {
    float shade(Material material, Light light) {
        return material.roughness * light.intensity;
    }
};
