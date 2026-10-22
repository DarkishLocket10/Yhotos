//! Link installed libraries only; never fetch or build native dependencies.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let vips = pkg_config::Config::new()
        .atleast_version("8.15")
        .probe("vips")?;
    let jxl = pkg_config::Config::new()
        .atleast_version("0.7")
        .probe("libjxl")?;
    // Ubuntu 24.04 uses libjxl 0.7, whose ICC getters have an extra deprecated
    // nullable argument. Bindgen uses the installed header for both API forms.
    println!("cargo:rustc-check-cfg=cfg(libjxl_legacy_icc)");
    let mut version = jxl.version.split('.');
    let major: u32 = version
        .next()
        .ok_or("missing libjxl major version")?
        .parse()?;
    let minor: u32 = version
        .next()
        .ok_or("missing libjxl minor version")?
        .parse()?;
    if (major, minor) < (0, 9) {
        println!("cargo:rustc-cfg=libjxl_legacy_icc");
    }
    let lcms = pkg_config::Config::new().probe("lcms2")?;
    let mut builder = bindgen::Builder::default()
        .header("src/native.h")
        .allowlist_function("vips_(init|concurrency_set|cache_set_max|cache_set_max_files|error_clear)")
        .allowlist_function("vips_(jpegload_buffer|jpegload|pngload|tiffload|heifload|jxlload|webpload)")
        .allowlist_function("vips_(copy|autorot|cast|linear1|extract_band|extract_area|resize|premultiply|unpremultiply|bandjoin2|bandjoin_const1|flatten|array_double_new|area_unref|scRGB2XYZ|icc_import|icc_export|heifsave|jxlsave|jpegsave)")
        .allowlist_function("vips_image_(get_width|get_height|get_bands|get_orientation|get_format|get_interpretation|hasalpha|get_typeof|get_int|get_string|get_blob|remove|set_int|set_blob_copy|copy_memory|write_to_memory|new_from_memory_copy)")
        .allowlist_function("JxlDecoder(Create|Destroy|SubscribeEvents|SetInput|ReleaseInput|ProcessInput|GetBasicInfo|GetICCProfileSize|GetColorAsICCProfile)")
        .allowlist_type("JxlBasicInfo")
        .allowlist_var("JXL_DEC_.*")
        .allowlist_function("g_(object_unref|free|log_set_handler)")
        .allowlist_function("cms(OpenProfileFromMem|CloseProfile|GetProfileInfoASCII|GetColorSpace|CreateRGBProfile|BuildTabulatedToneCurveFloat|FreeToneCurve|SaveProfileToMem|MLUalloc|MLUsetASCII|MLUfree|WriteTag)")
        .allowlist_type("Vips(Image|BandFormat|Interpretation|PCS|Intent|Kernel|ForeignKeep|ForeignHeifCompression|ForeignSubsample)")
        .allowlist_var("cmsInfoDescription")
        .allowlist_var("cmsSigProfileDescriptionTag")
        .opaque_type("VipsImage")
        .opaque_type("VipsSource")
        .layout_tests(false)
        .generate_comments(false);
    for dir in vips
        .include_paths
        .iter()
        .chain(&lcms.include_paths)
        .chain(&jxl.include_paths)
    {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    builder.generate()?.write_to_file(out.join("native.rs"))?;
    println!("cargo:rerun-if-changed=src/native.h");
    Ok(())
}
