use slang_inline::{SlangSource, slang_concat, slang_inline};

mod lighting;
mod material;
mod surface;

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
const SHADER: SlangSource = slang_concat!(LIBRARY, crate::surface::SURFACE);
const DETAILED_SHADER: SlangSource = slang_concat!(DETAILED_LIBRARY, crate::surface::SURFACE);
const SHADER_TEXT: &str = SHADER.as_str();

fn main() {
    let source = if std::env::args().any(|arg| arg == "--detail") {
        DETAILED_SHADER.as_str()
    } else {
        SHADER_TEXT
    };
    print!("{source}");
}
