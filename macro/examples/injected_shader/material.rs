pub const MATERIAL_DECLS: &str = r#"
struct Material {
    float roughness;
    float metallic;
};
"#;

pub const DETAILED_MATERIAL_DECLS: &str = r#"
struct Material {
    float roughness;
    float metallic;
    float specular;
};
"#;
