use slang_inline::{SlangSource, slang_inline};

const _: SlangSource = slang_inline! {
    float linear(float x) { return x; }
};

// Fix the missing Slang semicolon in the editor to clear the diagnostic.
const _: SlangSource = slang_inline! {
    float broken(float x) { return x }
};

const _: SlangSource = slang_inline! {
    Texture2D<float4> image;
    float4 sample(int3 location) {
        // Try completion after the dot, or hover over image.
        return image.Load(location);
    }
};

// Use a raw string when preprocessor branches have unbalanced Rust delimiters.
const _: SlangSource = slang_inline! { r#"
#if 1
float conditional() {
#else
int conditional() {
#endif
    return 3;
}
"# };

fn main() {}
