#[cfg(feature = "build-check")]
#[path = "src/lighting.rs"]
mod lighting;
#[cfg(feature = "build-check")]
#[path = "src/material.rs"]
mod material;
#[cfg(feature = "build-check")]
#[path = "src/shaders.rs"]
mod shaders;
#[cfg(feature = "build-check")]
#[path = "src/surface.rs"]
mod surface;

fn main() {
    #[cfg(feature = "build-check")]
    {
        use slang_inline::build::{Shader, check};
        use std::path::Path;

        let directory = Path::new("src");
        let shaders = [
            Shader {
                name: "SHADER",
                source: shaders::SHADER,
                directory,
            },
            Shader {
                name: "DETAILED_SHADER",
                source: shaders::DETAILED_SHADER,
                directory,
            },
        ];
        if let Err(error) = check(&shaders, &[]) {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
