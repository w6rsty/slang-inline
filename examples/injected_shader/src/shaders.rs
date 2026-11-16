use slang_inline::{SlangSource, slang_concat, slang_inline};

const LIBRARY: SlangSource = slang_inline! {
    include {
        crate::material::MATERIAL_DECLS,
        crate::lighting::LIGHT_DECLS,
    }
};

const DETAILED_LIBRARY: SlangSource = slang_inline! {
    include {
        crate::material::DETAILED_MATERIAL_DECLS,
        crate::lighting::LIGHT_DECLS,
    }
};

// Both shaders use the same source fragment with different material declarations.
pub const SHADER: SlangSource = slang_concat!(LIBRARY, crate::surface::SURFACE);
pub const DETAILED_SHADER: SlangSource = slang_concat!(DETAILED_LIBRARY, crate::surface::SURFACE);
