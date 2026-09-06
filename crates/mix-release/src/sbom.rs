use base64::Engine;
use clap::Subcommand;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const CARGO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

struct Inventory {
    components: Vec<Value>,
    dependencies: BTreeMap<String, Vec<String>>,
    direct: Vec<String>,
}

fn cargo_package_field<'a>(
    manifest: &'a toml::Value,
    workspace: &'a toml::Value,
    field: &str,
) -> Option<&'a str> {
    let value = manifest.get("package")?.get(field)?;
    if let Some(value) = value.as_str() {
        return Some(value);
    }
    (value.get("workspace")?.as_bool() == Some(true))
        .then(|| {
            workspace
                .get("workspace")?
                .get("package")?
                .get(field)?
                .as_str()
        })
        .flatten()
}

fn enforce_source_language_policy(root: &Path) -> Result<(), String> {
    const SOURCE_ROOTS: &[&str] = &[
        "crates",
        "apps/macos/src",
        "apps/macos/src-tauri",
        "scripts",
    ];
    const FORBIDDEN_EXTENSIONS: &[&str] = &["py", "pyc", "swift", "whl"];
    let mut pending = SOURCE_ROOTS
        .iter()
        .map(|path| root.join(path))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("cannot inspect {}: {error}", directory.display()))?
        {
            let entry = entry
                .map_err(|error| format!("cannot inspect {}: {error}", directory.display()))?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
            let path = entry.path();
            let forbidden = path
                .extension()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|extension| FORBIDDEN_EXTENSIONS.contains(&extension));
            if forbidden {
                return Err(format!(
                    "unsupported implementation file is forbidden: {}",
                    path.display()
                ));
            }
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir()
                && !matches!(entry.file_name().to_str(), Some("target" | "node_modules"))
            {
                pending.push(path);
            }
        }
    }
    Ok(())
}

#[derive(Subcommand)]
pub enum Command {
    SourceCheck,
    Generate {
        #[arg(long)]
        target: String,
        #[arg(long)]
        artifact: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        arch: String,
        #[arg(long)]
        cargo_metadata: PathBuf,
        #[arg(long)]
        cargo_tree: PathBuf,
    },
    Verify {
        sbom: PathBuf,
        #[arg(long)]
        artifact: Option<PathBuf>,
    },
    Notices {
        #[arg(required = true)]
        sbom: Vec<PathBuf>,
        #[arg(long)]
        output: PathBuf,
    },
}

pub fn run(command: Command, root: &Path) -> Result<(), String> {
    match command {
        Command::SourceCheck => {
            enforce_source_language_policy(root)?;
            let npm = npm_components(root)?;
            let lock = super::read_toml(&root.join("Cargo.lock"))?;
            let cargo = lock
                .get("package")
                .and_then(toml::Value::as_array)
                .ok_or("Cargo.lock has no packages")?;
            for package in cargo {
                let source = package
                    .get("source")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("");
                if !source.is_empty() && source != CARGO_SOURCE {
                    return Err(format!("Cargo.lock contains a non-public source: {source}"));
                }
            }
            let package = super::read_json(&root.join("apps/macos/package.json"))?;
            if package.get("license").and_then(Value::as_str) != Some("MIT") {
                return Err("apps/macos/package.json must declare MIT".into());
            }
            let tauri = super::read_toml(&root.join("apps/macos/src-tauri/Cargo.toml"))?;
            let workspace = super::read_toml(&root.join("Cargo.toml"))?;
            if cargo_package_field(&tauri, &workspace, "license") != Some("MIT") {
                return Err("apps/macos/src-tauri/Cargo.toml must declare MIT".into());
            }
            println!(
                "public dependency sources: npm_production={}, cargo_locked={}",
                npm.components.len(),
                cargo.len()
            );
        }
        Command::Generate {
            target,
            artifact,
            output,
            arch,
            cargo_metadata,
            cargo_tree,
        } => {
            let document = generate(
                root,
                &target,
                &artifact,
                &arch,
                &cargo_metadata,
                &cargo_tree,
            )?;
            super::atomic_json(&output, &document, 0o644)?;
            verify(root, &output, Some(&artifact))?;
            println!(
                "generated {} with {} components",
                output.display(),
                document["components"].as_array().map_or(0, Vec::len)
            );
        }
        Command::Verify { sbom, artifact } => {
            let count = verify(root, &sbom, artifact.as_deref())?;
            println!("verified {}: {count} components", sbom.display());
        }
        Command::Notices { sbom, output } => {
            notices(root, &sbom, &output)?;
            println!("generated {}", output.display());
        }
    }
    Ok(())
}

fn encode(value: &str, preserve_slash: bool) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || (preserve_slash && byte == b'/')
        {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn purl(ecosystem: &str, name: &str, version: &str) -> String {
    format!(
        "pkg:{}/{}@{}",
        ecosystem,
        encode(name, true),
        encode(version, false)
    )
}

fn licenses(expression: &str) -> Result<Value, String> {
    let expression = expression.trim();
    if expression.is_empty() {
        return Err("a dependency has no declared license".into());
    }
    Ok(json!([{"expression":expression}]))
}

fn properties(ecosystem: &str, values: &[(&str, &str)]) -> Value {
    let mut rows = vec![json!({"name":"mix:ecosystem","value":ecosystem})];
    let mut values = values.to_vec();
    values.sort_unstable_by_key(|item| item.0);
    rows.extend(
        values
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, value)| json!({"name":format!("mix:{name}"),"value":value})),
    );
    Value::Array(rows)
}

fn validate_npm_distribution(value: &str) -> Result<(), String> {
    let url = url::Url::parse(value).map_err(|_| "npm dependency has no valid distribution URL")?;
    if !value.starts_with("https://registry.npmjs.org/")
        || url.scheme() != "https"
        || url.host_str() != Some("registry.npmjs.org")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path().is_empty()
        || url.path() == "/"
    {
        return Err(format!("npm dependency is not public: {value}"));
    }
    Ok(())
}

fn npm_components(root: &Path) -> Result<Inventory, String> {
    let lock = super::read_json(&root.join("apps/macos/package-lock.json"))?;
    if lock.get("lockfileVersion").and_then(Value::as_u64) != Some(3) {
        return Err("package-lock.json must use lockfileVersion 3".into());
    }
    let packages = lock
        .get("packages")
        .and_then(Value::as_object)
        .ok_or("invalid npm packages")?;
    let mut components = Vec::new();
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut entries = BTreeMap::new();
    for (path, item) in packages {
        if path.is_empty() {
            continue;
        }
        let resolved = item.get("resolved").and_then(Value::as_str).unwrap_or("");
        validate_npm_distribution(resolved)?;
        if item.get("dev").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let name = path.rsplit("node_modules/").next().unwrap_or(path);
        let version = item.get("version").and_then(Value::as_str).unwrap_or("");
        if version.is_empty() {
            return Err(format!("npm production package has no version: {path}"));
        }
        let reference = purl("npm", name, version);
        if !entries.contains_key(&reference) {
            let mut component = Map::from_iter([
                ("bom-ref".into(), Value::String(reference.clone())),
                ("type".into(), Value::String("library".into())),
                ("name".into(), Value::String(name.into())),
                ("version".into(), Value::String(version.into())),
                ("purl".into(), Value::String(reference.clone())),
                (
                    "licenses".into(),
                    licenses(item.get("license").and_then(Value::as_str).unwrap_or(""))?,
                ),
                ("properties".into(), properties("npm", &[])),
            ]);
            if let Some(integrity) = item.get("integrity").and_then(Value::as_str) {
                let (algorithm, encoded) =
                    integrity.split_once('-').ok_or("invalid npm integrity")?;
                let algorithm = match algorithm {
                    "sha256" => "SHA-256",
                    "sha384" => "SHA-384",
                    "sha512" => "SHA-512",
                    _ => return Err("unsupported npm integrity algorithm".into()),
                };
                let hash = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| "invalid npm integrity base64")?;
                component.insert(
                    "hashes".into(),
                    json!([{"alg":algorithm,"content":hex(&hash)}]),
                );
            }
            if !resolved.is_empty() {
                component.insert(
                    "externalReferences".into(),
                    json!([{"type":"distribution","url":resolved}]),
                );
            }
            components.push(Value::Object(component));
            entries.insert(reference.clone(), item);
        }
        by_name.entry(name.into()).or_default().push(reference);
    }
    let mut dependencies = BTreeMap::new();
    for (reference, item) in entries {
        let mut refs = BTreeSet::new();
        if let Some(rows) = item.get("dependencies").and_then(Value::as_object) {
            for name in rows.keys() {
                let candidates = by_name
                    .get(name)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                if candidates.len() == 1 {
                    refs.extend(candidates);
                }
            }
        }
        dependencies.insert(reference, refs.into_iter().collect());
    }
    let mut direct = Vec::new();
    if let Some(rows) = packages
        .get("")
        .and_then(|item| item.get("dependencies"))
        .and_then(Value::as_object)
    {
        for name in rows.keys() {
            let candidates = by_name
                .get(name)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect::<BTreeSet<_>>();
            if candidates.len() != 1 {
                return Err(format!("npm direct dependency is ambiguous: {name}"));
            }
            direct.extend(candidates);
        }
    }
    direct.sort();
    Ok(Inventory {
        components,
        dependencies,
        direct,
    })
}

fn cargo_components(
    root: &Path,
    metadata_path: &Path,
    tree_path: &Path,
) -> Result<Inventory, String> {
    let metadata = super::read_json(metadata_path)?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("Cargo metadata has no packages")?;
    let packages_by_id = packages
        .iter()
        .map(|package| {
            (
                package
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                package,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let root_id = metadata
        .pointer("/resolve/root")
        .and_then(Value::as_str)
        .ok_or("Cargo metadata has no root")?;
    let mut identities: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (id, package) in &packages_by_id {
        identities
            .entry((
                package
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                package
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
            ))
            .or_default()
            .push(id.clone());
    }
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut included = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    for (index, line) in std::fs::read_to_string(tree_path)
        .map_err(|error| error.to_string())?
        .lines()
        .enumerate()
    {
        let digit_count = line.bytes().take_while(u8::is_ascii_digit).count();
        if digit_count == 0 {
            return Err(format!("invalid Cargo tree row {}", index + 1));
        }
        let depth: usize = line[..digit_count]
            .parse()
            .map_err(|_| "invalid Cargo tree depth")?;
        let rest = &line[digit_count..];
        let (name, version_and_more) = rest.split_once(" v").ok_or("invalid Cargo tree package")?;
        let version = version_and_more.split_whitespace().next().unwrap_or("");
        let candidates = identities
            .get(&(name.into(), version.into()))
            .cloned()
            .unwrap_or_default();
        if candidates.len() != 1 {
            return Err(format!("Cargo package is ambiguous: {name} {version}"));
        }
        let id = candidates[0].clone();
        if index == 0 && (depth != 0 || id != root_id) {
            return Err("Cargo tree must begin with its resolved root".into());
        }
        if index > 0 && (depth == 0 || depth > stack.len()) {
            return Err("invalid Cargo tree depth".into());
        }
        if depth > 0 {
            edges
                .entry(stack[depth - 1].clone())
                .or_default()
                .insert(id.clone());
        }
        stack.truncate(depth);
        stack.push(id.clone());
        included.insert(id);
    }
    let cargo_lock = super::read_toml(&root.join("Cargo.lock"))?;
    let mut checksums = BTreeMap::new();
    for item in cargo_lock
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or("Cargo.lock has no packages")?
    {
        checksums.insert(
            (
                item.get("name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                item.get("version")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                item.get("source")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            item.get("checksum")
                .and_then(toml::Value::as_str)
                .unwrap_or("")
                .to_string(),
        );
    }
    let mut components = Vec::new();
    let mut id_to_ref = BTreeMap::new();
    for id in &included {
        if id == root_id {
            continue;
        }
        let package = packages_by_id[id];
        let name = package["name"].as_str().unwrap_or("");
        let version = package["version"].as_str().unwrap_or("");
        let source = package.get("source").and_then(Value::as_str).unwrap_or("");
        if !source.is_empty() && source != CARGO_SOURCE {
            return Err(format!("non-public Cargo dependency: {source}"));
        }
        let reference = purl("cargo", name, version);
        if id_to_ref.values().any(|value| value == &reference) {
            return Err(format!("duplicate Cargo package: {reference}"));
        }
        id_to_ref.insert(id.clone(), reference.clone());
        let mut component = Map::from_iter([
            ("bom-ref".into(), Value::String(reference.clone())),
            ("type".into(), Value::String("library".into())),
            ("name".into(), Value::String(name.into())),
            ("version".into(), Value::String(version.into())),
            ("purl".into(), Value::String(reference)),
            (
                "licenses".into(),
                licenses(package.get("license").and_then(Value::as_str).unwrap_or(""))?,
            ),
            ("properties".into(), properties("cargo", &[])),
        ]);
        let checksum = checksums
            .get(&(name.into(), version.into(), source.into()))
            .cloned()
            .unwrap_or_default();
        if !source.is_empty() && checksum.is_empty() {
            return Err(format!("missing Cargo checksum: {name} {version}"));
        }
        if !checksum.is_empty() {
            component.insert(
                "hashes".into(),
                json!([{"alg":"SHA-256","content":checksum}]),
            );
        }
        if let Some(repository) = package
            .get("repository")
            .and_then(Value::as_str)
            .filter(|value| value.starts_with("https://"))
        {
            component.insert(
                "externalReferences".into(),
                json!([{"type":"vcs","url":repository}]),
            );
        }
        components.push(Value::Object(component));
    }
    let mut dependencies = BTreeMap::new();
    for (id, reference) in &id_to_ref {
        let refs = edges
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|child| id_to_ref.get(child))
            .cloned()
            .collect::<BTreeSet<_>>();
        dependencies.insert(reference.clone(), refs.into_iter().collect());
    }
    let direct = edges
        .get(root_id)
        .into_iter()
        .flatten()
        .filter_map(|id| id_to_ref.get(id))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Inventory {
        components,
        dependencies,
        direct,
    })
}

fn generate(
    root: &Path,
    target: &str,
    artifact: &Path,
    arch: &str,
    metadata: &Path,
    tree: &Path,
) -> Result<Value, String> {
    if !artifact.is_file() || !matches!(target, "macos" | "cli") {
        return Err("invalid SBOM target or artifact".into());
    }
    let version = super::release_version(root)?;
    let artifact_hash = super::sha256(artifact)?;
    let cargo = cargo_components(root, metadata, tree)?;
    let mut components = cargo.components;
    let mut dependencies = cargo.dependencies;
    let mut direct = cargo.direct;
    let root_name = if target == "macos" {
        let npm = npm_components(root)?;
        components.extend(npm.components);
        dependencies.extend(npm.dependencies);
        direct.extend(npm.direct);
        "mix-macos"
    } else {
        "mix-cli"
    };
    components.sort_by(|left, right| left["bom-ref"].as_str().cmp(&right["bom-ref"].as_str()));
    let references = components
        .iter()
        .map(|item| item["bom-ref"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    if references.iter().collect::<BTreeSet<_>>().len() != references.len() {
        return Err("duplicate SBOM component".into());
    }
    direct.sort();
    direct.dedup();
    let root_ref = format!(
        "{}?arch={}",
        purl("generic", root_name, &version),
        encode(arch, false)
    );
    let mut dependency_rows = vec![json!({"ref":root_ref,"dependsOn":direct})];
    dependency_rows.extend(references.iter().map(|reference| json!({"ref":reference,"dependsOn":dependencies.remove(reference).unwrap_or_default()})));
    dependency_rows.sort_by(|left, right| left["ref"].as_str().cmp(&right["ref"].as_str()));
    let seed = [target, arch, &version, &artifact_hash]
        .into_iter()
        .chain(references.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let serial = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, seed.as_bytes());
    Ok(json!({
        "$schema":"https://cyclonedx.org/schema/bom-1.6.schema.json",
        "bomFormat":"CycloneDX","specVersion":"1.6","serialNumber":format!("urn:uuid:{serial}"),"version":1,
        "metadata":{"component":{
            "bom-ref":root_ref,"type":"application","name":format!("mix-{target}"),"version":version,
            "licenses":licenses("MIT")?,"hashes":[{"alg":"SHA-256","content":artifact_hash}],
            "properties":properties("mix",&[("artifact",artifact.file_name().and_then(|v|v.to_str()).unwrap_or("")),("architecture",arch),("target",target)])
        }},
        "components":components,"dependencies":dependency_rows
    }))
}

fn verify(root: &Path, path: &Path, artifact: Option<&Path>) -> Result<usize, String> {
    let document = super::read_json(path)?;
    if document["bomFormat"] != "CycloneDX"
        || document["specVersion"] != "1.6"
        || document["version"] != 1
        || document["metadata"].get("timestamp").is_some()
    {
        return Err("SBOM must be deterministic CycloneDX 1.6".into());
    }
    let serial = document["serialNumber"]
        .as_str()
        .and_then(|value| value.strip_prefix("urn:uuid:"))
        .and_then(|value| uuid::Uuid::parse_str(value).ok());
    if serial.is_none() {
        return Err("SBOM serialNumber must be a UUID URN".into());
    }
    let root_component = &document["metadata"]["component"];
    if root_component["version"] != super::release_version(root)?
        || root_component["licenses"] != licenses("MIT")?
    {
        return Err("SBOM root metadata mismatch".into());
    }
    if let Some(artifact) = artifact {
        let expected = super::sha256(artifact)?;
        if hash_value(root_component, "SHA-256") != Some(expected.as_str()) {
            return Err("SBOM artifact hash mismatch".into());
        }
    }
    let components = document["components"]
        .as_array()
        .ok_or("SBOM has no components")?;
    let references = components
        .iter()
        .map(|item| item["bom-ref"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    if references.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("SBOM components must be uniquely sorted".into());
    }
    let mut known = references.iter().copied().collect::<BTreeSet<_>>();
    known.insert(root_component["bom-ref"].as_str().unwrap_or(""));
    for component in components {
        if component["version"].as_str().unwrap_or("").is_empty()
            || component["purl"] != component["bom-ref"]
            || component["licenses"].as_array().is_none_or(Vec::is_empty)
        {
            return Err("incomplete SBOM component".into());
        }
        for hash in component["hashes"].as_array().into_iter().flatten() {
            let length = match hash["alg"].as_str() {
                Some("SHA-256") => 64,
                Some("SHA-384") => 96,
                Some("SHA-512") => 128,
                _ => return Err("invalid SBOM hash algorithm".into()),
            };
            let value = hash["content"].as_str().unwrap_or("");
            if value.len() != length
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err("invalid SBOM hash".into());
            }
        }
        for reference in component["externalReferences"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if reference["type"] == "distribution"
                && validate_npm_distribution(reference["url"].as_str().unwrap_or("")).is_err()
            {
                return Err("SBOM contains a non-public npm distribution URL".into());
            }
        }
    }
    let rows = document["dependencies"]
        .as_array()
        .ok_or("SBOM has no dependency graph")?;
    let row_refs = rows
        .iter()
        .map(|item| item["ref"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    if row_refs.windows(2).any(|pair| pair[0] >= pair[1])
        || row_refs.iter().copied().collect::<BTreeSet<_>>() != known
    {
        return Err("invalid SBOM dependency graph".into());
    }
    for row in rows {
        if row["dependsOn"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| !known.contains(item.as_str().unwrap_or("")))
        {
            return Err("SBOM dependency graph has unknown reference".into());
        }
    }
    Ok(components.len())
}

fn notices(root: &Path, paths: &[PathBuf], output: &Path) -> Result<(), String> {
    let mut subjects = BTreeSet::new();
    let mut inventory = BTreeSet::new();
    for path in paths {
        verify(root, path, None)?;
        let document = super::read_json(path)?;
        let root_component = &document["metadata"]["component"];
        let root_properties = property_map(root_component);
        subjects.insert((
            root_properties
                .get("mix:artifact")
                .cloned()
                .unwrap_or_default(),
            root_properties
                .get("mix:target")
                .cloned()
                .unwrap_or_default(),
            root_properties
                .get("mix:architecture")
                .cloned()
                .unwrap_or_default(),
        ));
        for component in document["components"].as_array().into_iter().flatten() {
            let values = property_map(component);
            inventory.insert((
                values.get("mix:ecosystem").cloned().unwrap_or_default(),
                component["name"].as_str().unwrap_or("").to_string(),
                component["version"].as_str().unwrap_or("").to_string(),
                component["licenses"][0]["expression"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            ));
        }
    }
    let mut rows=vec!["# Mix third-party notices".into(),"".into(),"This inventory is generated from artifact-bound release SBOMs. Consult each upstream project for complete license text and notices.".into(),"".into(),"## Covered release artifacts".into(),"".into()];
    for (artifact, target, architecture) in subjects {
        if artifact.is_empty() || target.is_empty() || architecture.is_empty() {
            return Err("SBOM root is missing artifact identity".into());
        }
        rows.push(format!(
            "- `{artifact}` — target `{target}`, architecture `{architecture}`"
        ));
    }
    rows.extend([
        "".into(),
        "## Dependency license inventory".into(),
        "".into(),
        "| Ecosystem | Package | Version | License |".into(),
        "| --- | --- | --- | --- |".into(),
    ]);
    for (ecosystem, name, version, license) in inventory {
        rows.push(format!("| {ecosystem} | {name} | {version} | {license} |"));
    }
    rows.extend([
        "".into(),
        "Mix itself is licensed under the MIT License. See `LICENSE` in the source repository."
            .into(),
        "".into(),
    ]);
    super::atomic_write(output, rows.join("\n").as_bytes(), 0o644)
}

fn property_map(value: &Value) -> BTreeMap<String, String> {
    value["properties"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some((
                item["name"].as_str()?.into(),
                item["value"].as_str()?.into(),
            ))
        })
        .collect()
}
fn hash_value<'a>(value: &'a Value, algorithm: &str) -> Option<&'a str> {
    value["hashes"]
        .as_array()?
        .iter()
        .find(|item| item["alg"] == algorithm)?
        .get("content")?
        .as_str()
}
fn hex(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_package_fields_follow_explicit_workspace_inheritance() {
        let workspace: toml::Value = toml::from_str(
            r#"
            [workspace.package]
            license = "MIT"
            "#,
        )
        .unwrap();
        let inherited: toml::Value = toml::from_str(
            r#"
            [package]
            license.workspace = true
            "#,
        )
        .unwrap();
        let explicit: toml::Value = toml::from_str(
            r#"
            [package]
            license = "Apache-2.0"
            "#,
        )
        .unwrap();

        assert_eq!(
            cargo_package_field(&inherited, &workspace, "license"),
            Some("MIT")
        );
        assert_eq!(
            cargo_package_field(&explicit, &workspace, "license"),
            Some("Apache-2.0")
        );
    }

    #[test]
    fn source_policy_rejects_unsupported_implementation_languages() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("crates/core/src")).unwrap();
        fs::write(root.path().join("crates/core/src/lib.rs"), "").unwrap();
        assert!(enforce_source_language_policy(root.path()).is_ok());

        fs::create_dir_all(root.path().join("apps/macos/src-tauri/scripts")).unwrap();
        fs::write(
            root.path().join("apps/macos/src-tauri/scripts/build.swift"),
            "",
        )
        .unwrap();
        assert!(enforce_source_language_policy(root.path()).is_err());
    }

    #[test]
    fn purls_encode_scopes_and_versions_deterministically() {
        assert_eq!(
            purl("npm", "@scope/package", "1.0.0+build"),
            "pkg:npm/%40scope/package@1.0.0%2Bbuild"
        );
    }

    #[test]
    fn dependency_licenses_are_mandatory() {
        assert!(licenses("MIT").is_ok());
        assert!(licenses("  ").is_err());
    }

    #[test]
    fn generated_hashes_are_lowercase_hex() {
        assert_eq!(hex(&[0, 15, 16, 255]), "000f10ff");
    }

    #[test]
    fn source_policy_checks_development_dependencies_too() {
        let root = tempfile::tempdir().expect("temporary root");
        let app = root.path().join("apps/macos");
        std::fs::create_dir_all(&app).expect("application directory");
        std::fs::write(
            app.join("package-lock.json"),
            serde_json::to_vec(&json!({
                "lockfileVersion":3,
                "packages":{
                    "":{},
                    "node_modules/build-tool":{
                        "version":"1.0.0",
                        "dev":true,
                        "resolved":"https://packages.internal.invalid/build-tool.tgz"
                    }
                }
            }))
            .expect("lock JSON"),
        )
        .expect("lock file");

        assert!(npm_components(root.path()).is_err());
    }

    #[test]
    fn npm_distribution_requires_an_exact_immutable_public_url() {
        assert!(validate_npm_distribution(
            "https://registry.npmjs.org/package/-/package-1.0.0.tgz"
        )
        .is_ok());
        for invalid in [
            "",
            "http://registry.npmjs.org/package/-/package-1.0.0.tgz",
            "https://user@registry.npmjs.org/package/-/package-1.0.0.tgz",
            "https://registry.npmjs.org:443/package/-/package-1.0.0.tgz",
            "https://registry.npmjs.org/package/-/package-1.0.0.tgz?token=value",
            "https://registry.npmjs.org/package/-/package-1.0.0.tgz#fragment",
            "https://packages.invalid/package-1.0.0.tgz",
        ] {
            assert!(validate_npm_distribution(invalid).is_err(), "{invalid}");
        }
    }
}
