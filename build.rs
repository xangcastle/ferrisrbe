use std::fs;
use std::io::Result;
use std::path::PathBuf;

/// Resolves a proto file path.
fn resolve_proto_file(rel_path: &str) -> Option<PathBuf> {
    let current_dir = std::env::current_dir().ok()?;

    eprintln!("Looking for: {}", rel_path);
    eprintln!("Current dir: {}", current_dir.display());

    let try_path = current_dir.join(rel_path);
    eprintln!("  Trying: {}", try_path.display());
    if try_path.exists() {
        eprintln!("  ✓ Found!");
        return Some(try_path);
    }

    let mut search_dir = current_dir.clone();
    for i in 0..8 {
        if let Some(parent) = search_dir.parent() {
            search_dir = parent.to_path_buf();
            let try_path = search_dir.join(rel_path);
            eprintln!("  Trying (up {}): {}", i + 1, try_path.display());
            if try_path.exists() {
                eprintln!("  ✓ Found!");
                return Some(try_path);
            }
        }
    }

    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let try_path = PathBuf::from(&manifest_dir).join(rel_path);
        eprintln!("  Trying (manifest): {}", try_path.display());
        if try_path.exists() {
            eprintln!("  ✓ Found!");
            return Some(try_path);
        }

        let mut search_dir = PathBuf::from(&manifest_dir);
        for i in 0..5 {
            if let Some(parent) = search_dir.parent() {
                search_dir = parent.to_path_buf();
                let try_path = search_dir.join(rel_path);
                eprintln!("  Trying (manifest up {}): {}", i + 1, try_path.display());
                if try_path.exists() {
                    eprintln!("  ✓ Found!");
                    return Some(try_path);
                }
            }
        }
    }

    eprintln!("  Current directory contents:");
    if let Ok(entries) = std::fs::read_dir(&current_dir) {
        for entry in entries.flatten().take(20) {
            let path = entry.path();
            let file_type = if path.is_dir() { "[D]" } else { "[F]" };
            eprintln!("    {} {}", file_type, path.file_name()?.to_str()?);
        }
    }

    None
}

/// Post-processes the generated file to fix semver references and add clippy allow
fn fix_semver_references(out_dir: &PathBuf) -> Result<()> {
    let remote_execution_file = out_dir.join("build.bazel.remote.execution.v2.rs");

    if !remote_execution_file.exists() {
        eprintln!("Archivo remote_execution no encontrado, saltando post-procesamiento");
        return Ok(());
    }

    eprintln!("Post-procesando: {}", remote_execution_file.display());

    let content = fs::read_to_string(&remote_execution_file)?;

    let mut fixed_content = content.replace(
        "super::super::super::semver::SemVer",
        "crate::proto::build::bazel::semver::SemVer",
    );

    // Add clippy allow attribute to suppress doc warnings in generated code
    if !fixed_content.contains("clippy::doc_lazy_continuation") {
        fixed_content = format!("#[allow(clippy::doc_lazy_continuation)]\n{}", fixed_content);
        eprintln!("  ✓ Agregado allow(clippy::doc_lazy_continuation)");
    }

    if content != fixed_content {
        eprintln!("  ✓ Reemplazadas referencias a semver");
        fs::write(&remote_execution_file, fixed_content)?;
    } else {
        eprintln!("  - No se encontraron referencias para reemplazar");
    }

    Ok(())
}

fn main() -> Result<()> {
    let worker_proto = resolve_proto_file("proto/worker.proto").expect("worker.proto not found");
    let remote_execution_proto =
        resolve_proto_file("proto/build/bazel/remote/execution/v2/remote_execution.proto")
            .expect("remote_execution.proto not found");
    let semver_proto = resolve_proto_file("proto/build/bazel/semver/semver.proto")
        .expect("semver.proto not found");
    let operations_proto = resolve_proto_file("proto/google/longrunning/operations.proto")
        .expect("operations.proto not found");
    let status_proto =
        resolve_proto_file("proto/google/rpc/status.proto").expect("status.proto not found");
    let bytestream_proto = resolve_proto_file("proto/google/bytestream/bytestream.proto")
        .expect("bytestream.proto not found");
    let wrappers_proto = resolve_proto_file("proto/google/protobuf/wrappers.proto")
        .expect("wrappers.proto not found");
    let duration_proto = resolve_proto_file("proto/google/protobuf/duration.proto")
        .expect("duration.proto not found");
    let any_proto =
        resolve_proto_file("proto/google/protobuf/any.proto").expect("any.proto not found");
    let annotations_proto = resolve_proto_file("proto/google/api/annotations.proto")
        .expect("annotations.proto not found");
    let http_proto =
        resolve_proto_file("proto/google/api/http.proto").expect("http.proto not found");

    let proto_include = worker_proto
        .parent()
        .expect("worker.proto should have a parent")
        .to_path_buf();

    eprintln!("\nUsing proto include: {}", proto_include.display());
    eprintln!("✓ All proto files found successfully");

    println!("cargo:rerun-if-changed=proto");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_else(|_| ".".to_string()));

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[worker_proto.to_str().unwrap()],
            &[proto_include.to_str().unwrap()],
        )?;

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[
                remote_execution_proto.to_str().unwrap(),
                semver_proto.to_str().unwrap(),
                wrappers_proto.to_str().unwrap(),
                duration_proto.to_str().unwrap(),
                any_proto.to_str().unwrap(),
                annotations_proto.to_str().unwrap(),
                http_proto.to_str().unwrap(),
                operations_proto.to_str().unwrap(),
                status_proto.to_str().unwrap(),
                bytestream_proto.to_str().unwrap(),
            ],
            &[proto_include.to_str().unwrap()],
        )?;

    fix_semver_references(&out_dir)?;
    fix_clippy_warnings(&out_dir)?;

    Ok(())
}

/// Placeholder for future post-processing of generated files
fn fix_clippy_warnings(_out_dir: &PathBuf) -> Result<()> {
    // Clippy warnings for generated code are handled at the module level
    // with #[allow] attributes in src/main.rs and src/lib.rs
    Ok(())
}
