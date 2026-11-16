mod lighting;
mod material;
mod shaders;
mod surface;

fn main() {
    let source = if std::env::args().any(|arg| arg == "--detail") {
        shaders::DETAILED_SHADER
    } else {
        shaders::SHADER
    };
    print!("{}", source.as_str());
}
