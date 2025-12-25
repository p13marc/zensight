fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false) // We only need the client
        .compile_protos(
            &["proto/gnmi_ext.proto", "proto/gnmi.proto"],
            &["proto/", "/usr/include"],
        )?;
    Ok(())
}
