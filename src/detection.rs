use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{config::Config, environment};

const COMPOSE_FILES: [&str; 4] = [
    "compose.yml",
    "compose.yaml",
    "docker-compose.yml",
    "docker-compose.yaml",
];

#[derive(Default)]
pub struct Report {
    pub config: Config,
    pub development: Vec<String>,
    pub notices: Vec<String>,
    pub warnings: Vec<String>,
    pub env_template: Option<EnvTemplate>,
}

#[derive(Clone)]
pub struct EnvTemplate {
    pub source: PathBuf,
    pub target: PathBuf,
}

#[derive(Default, Deserialize)]
struct Package {
    #[serde(rename = "packageManager")]
    package_manager: Option<String>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
    #[serde(default)]
    dependencies: BTreeMap<String, serde_json::Value>,
    #[serde(rename = "devDependencies", default)]
    dev_dependencies: BTreeMap<String, serde_json::Value>,
}

struct PackageAt {
    root: PathBuf,
    package: Package,
}

pub fn detect(repo: &Path, mut config: Config) -> Result<Report> {
    let env_candidates = ignored_env_files(repo)?;
    let tracked = tracked_files(repo)?;
    let devbox = read_optional(&repo.join("devbox.json"))?;
    let env_template = select_env_template(repo, config.env.as_deref(), &env_candidates, &tracked)?;
    if config.env.is_none() {
        config.env = env_candidates.first().cloned();
        if config.env.is_none() {
            config.env = env_template.as_ref().map(|plan| plan.target.clone());
        }
    }
    config.copies = detected_copies(config.copies, env_candidates, config.env.as_ref());
    config.compose |= has_root_compose(repo);
    let packages = packages(repo, &tracked)?;
    let mut report = Report {
        config,
        env_template,
        ..Report::default()
    };
    detect_development(repo, &tracked, &packages, &mut report)?;
    finish_config(repo, &tracked, devbox.as_deref(), &packages, &mut report)?;
    Ok(report)
}

fn finish_config(
    repo: &Path,
    tracked: &[PathBuf],
    devbox: Option<&str>,
    packages: &[PackageAt],
    report: &mut Report,
) -> Result<()> {
    if report.config.bootstrap.is_none() {
        report.config.bootstrap = detect_bootstrap(repo, devbox, packages, &mut report.notices)?;
    }
    if report.config.teardown.is_none() && report.config.compose {
        report.config.teardown = Some("docker compose down --remove-orphans".to_owned());
    }
    if report.config.disposable.is_empty() {
        report.config.disposable = detect_disposables(repo, tracked, devbox.is_some(), packages)?;
    }
    add_compose_notices(tracked, &mut report.notices);
    add_repository_warnings(repo, tracked, devbox, &report.config, &mut report.warnings)?;
    report.warnings.sort();
    report.warnings.dedup();
    Ok(())
}

fn detect_development(
    repo: &Path,
    tracked: &[PathBuf],
    packages: &[PackageAt],
    report: &mut Report,
) -> Result<()> {
    if !report.config.ports.is_empty() {
        report.development = report
            .config
            .ports
            .iter()
            .map(|port| format!("Configured port {port}"))
            .collect();
        return Ok(());
    }
    detect_package_servers(repo, packages, report);
    detect_vite_configs(repo, tracked, report)?;
    detect_wrangler_configs(repo, tracked, report)?;
    detect_compose_ports(repo, report)?;
    report.config.ports.sort();
    report.config.ports.dedup();
    report.development.sort();
    report.development.dedup();
    Ok(())
}

fn detect_package_servers(repo: &Path, packages: &[PackageAt], report: &mut Report) {
    for package in packages {
        let mut vite_config = None;
        for (name, script) in &package.package.scripts {
            if let Some(command) = server_command(script, "next") {
                detect_next(name, command, &package.root, report);
            }
            if let Some(command) = server_command(script, "vite") {
                let has_config =
                    *vite_config.get_or_insert_with(|| has_vite_config(repo, &package.root));
                if !has_config {
                    detect_vite(name, command, &package.root, report);
                }
            }
            if let Some(command) = server_command(script, "wrangler") {
                detect_wrangler(name, command, &package.root, report);
            }
        }
    }
}

fn has_vite_config(repo: &Path, root: &Path) -> bool {
    ["js", "mjs", "ts", "mts"].iter().any(|extension| {
        repo.join(root)
            .join(format!("vite.config.{extension}"))
            .is_file()
    })
}

fn server_command<'a>(script: &'a str, executable: &str) -> Option<&'a str> {
    let offset = script.find(executable)?;
    let before = script[..offset].chars().last();
    let after = script[offset + executable.len()..].chars().next();
    let boundary = |value: Option<char>| value.is_none_or(|char| !is_word(char));
    (boundary(before) && boundary(after)).then_some(&script[offset + executable.len()..])
}

fn is_word(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
}

fn detect_next(_name: &str, command: &str, root: &Path, report: &mut Report) {
    if !command
        .split_whitespace()
        .any(|word| matches!(word, "dev" | "start"))
    {
        return;
    }
    let label = source_label(root, "Next.js");
    match port_argument(command) {
        Some(PortArgument::Number(port)) => {
            report.development.push(format!("{label}: fixed {port}"));
            report.warnings.push(format!(
                "{label} fixes port {port}; replace the flag with -p $PORT so wt can isolate it."
            ));
        }
        Some(PortArgument::Variable(key)) => manage_process_port(report, label, key, 3000),
        None => manage_process_port(report, label, "PORT".to_owned(), 3000),
    }
}

fn detect_vite(_name: &str, command: &str, root: &Path, report: &mut Report) {
    let label = source_label(root, "Vite");
    match port_argument(command) {
        Some(PortArgument::Variable(key)) => manage_process_port(report, label, key, 5173),
        Some(PortArgument::Number(port)) if command.contains("--strictPort") => {
            report.development.push(format!("{label}: fixed {port}"));
            report
                .warnings
                .push(format!("{label} uses strict port {port}; it can collide."));
        }
        Some(PortArgument::Number(port)) => report
            .development
            .push(format!("{label}: starts at {port}, automatic fallback")),
        None => report
            .development
            .push(format!("{label}: starts at 5173, automatic fallback")),
    }
}

fn detect_wrangler(_name: &str, command: &str, root: &Path, report: &mut Report) {
    if !command.split_whitespace().any(|word| word == "dev") {
        return;
    }
    let label = source_label(root, "Wrangler");
    match port_argument(command) {
        Some(PortArgument::Variable(key)) => manage_process_port(report, label, key, 8787),
        Some(PortArgument::Number(port)) => {
            report.development.push(format!("{label}: fixed {port}"));
            report
                .warnings
                .push(format!("{label} uses fixed port {port}; it can collide."));
        }
        None => report
            .development
            .push(format!("{label}: default port 8787, not reserved")),
    }
}

fn source_label(root: &Path, server: &str) -> String {
    if root.as_os_str().is_empty() {
        server.to_owned()
    } else {
        format!("{server} ({})", root.display())
    }
}

fn manage_process_port(report: &mut Report, label: String, key: String, default: u16) {
    report.config.ports.push(format!("{key}:{default}"));
    report
        .development
        .push(format!("{label}: {default} → isolated via {key}"));
}

enum PortArgument {
    Number(u16),
    Variable(String),
}

fn port_argument(command: &str) -> Option<PortArgument> {
    let mut words = command.split_whitespace();
    while let Some(word) = words.next() {
        if let Some(value) = word.strip_prefix("--port=") {
            return parse_port_argument(value);
        }
        if matches!(word, "-p" | "--port") {
            return words.next().and_then(parse_port_argument);
        }
    }
    None
}

fn parse_port_argument(value: &str) -> Option<PortArgument> {
    let value = value.trim_matches(['\'', '"']);
    if let Ok(port) = value.parse() {
        return Some(PortArgument::Number(port));
    }
    let key = value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .or_else(|| value.strip_prefix('$'))?;
    (!key.is_empty()).then(|| PortArgument::Variable(key.to_owned()))
}

fn detect_vite_configs(repo: &Path, tracked: &[PathBuf], report: &mut Report) -> Result<()> {
    for path in tracked {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("vite.config.") {
            continue;
        }
        let contents = fs::read_to_string(repo.join(path))?;
        let ports = numeric_port_properties(&contents);
        if ports.is_empty() {
            continue;
        }
        let ports = ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let label = format!("Vite ({})", path.display());
        if contains_true_property(&contents, "strictPort") {
            report.development.push(format!("{label}: fixed {ports}"));
            report.warnings.push(format!(
                "{label} uses strict ports {ports}; they can collide."
            ));
        } else {
            report
                .development
                .push(format!("{label}: starts at {ports}, automatic fallback"));
        }
    }
    Ok(())
}

fn numeric_port_properties(contents: &str) -> Vec<u16> {
    let mut ports = contents
        .lines()
        .filter_map(numeric_port_property)
        .collect::<Vec<_>>();
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn detect_wrangler_configs(repo: &Path, tracked: &[PathBuf], report: &mut Report) -> Result<()> {
    for path in tracked.iter().filter(|path| is_wrangler_config(path)) {
        let contents = fs::read_to_string(repo.join(path))?;
        let label = format!("Wrangler ({})", path.display());
        if let Some(port) = contents.lines().find_map(wrangler_port) {
            report.development.push(format!("{label}: fixed {port}"));
            report
                .warnings
                .push(format!("{label} uses fixed port {port}; it can collide."));
        } else {
            report
                .development
                .push(format!("{label}: default port 8787, not reserved"));
        }
    }
    Ok(())
}

fn is_wrangler_config(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "wrangler.toml" | "wrangler.json" | "wrangler.jsonc"))
}

fn wrangler_port(line: &str) -> Option<u16> {
    let line = line.trim();
    let value = line
        .strip_prefix("port")?
        .trim_start()
        .strip_prefix(['=', ':'])?
        .trim();
    value.trim_end_matches(',').parse().ok()
}

fn numeric_port_property(line: &str) -> Option<u16> {
    let (_, value) = line.trim().split_once(':')?;
    line.trim()
        .starts_with("port:")
        .then(|| value.trim().trim_end_matches(','))?
        .parse()
        .ok()
}

fn contains_true_property(contents: &str, key: &str) -> bool {
    let prefix = format!("{key}:");
    contents.lines().any(|line| {
        line.trim()
            .strip_prefix(&prefix)
            .is_some_and(|value| value.trim().trim_end_matches(',') == "true")
    })
}

fn detect_compose_ports(repo: &Path, report: &mut Report) -> Result<()> {
    if !report.config.compose {
        return Ok(());
    }
    let Some(env) = &report.config.env else {
        return Ok(());
    };
    let source = if repo.join(env).is_file() {
        env
    } else if let Some(template) = &report.env_template {
        &template.source
    } else {
        return Ok(());
    };
    let available = environment::discover_ports(&repo.join(source))?;
    let host = compose_host_port_variables(repo)?;
    for port in available.into_iter().filter(|port| host.contains(port)) {
        report
            .development
            .push(format!("Docker Compose: isolated via {port}"));
        report.config.ports.push(port);
    }
    Ok(())
}

fn compose_host_port_variables(repo: &Path) -> Result<HashSet<String>> {
    let mut variables = HashSet::new();
    for file in COMPOSE_FILES {
        let path = repo.join(file);
        if path.is_file() {
            for line in fs::read_to_string(path)?.lines() {
                variables.extend(compose_variables_in_line(line));
            }
        }
    }
    Ok(variables)
}

fn compose_variables_in_line(line: &str) -> Vec<String> {
    let line = line.trim();
    if !line.starts_with('-') && !line.starts_with("published:") {
        return Vec::new();
    }
    let published = line.starts_with("published:");
    variable_expressions(line)
        .into_iter()
        .filter(|(_, after)| published || after.trim_start_matches(['\'', '"']).starts_with(':'))
        .map(|(variable, _)| variable)
        .collect()
}

fn variable_expressions(mut value: &str) -> Vec<(String, &str)> {
    let mut variables = Vec::new();
    while let Some(start) = value.find("${") {
        let expression = &value[start + 2..];
        let Some(end) = expression.find('}') else {
            break;
        };
        let after = &expression[end + 1..];
        let variable = expression[..end]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect();
        variables.push((variable, after));
        value = after;
    }
    variables
}

fn packages(repo: &Path, tracked: &[PathBuf]) -> Result<Vec<PackageAt>> {
    tracked
        .iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("package.json"))
        .map(|path| {
            let package = serde_json::from_slice(&fs::read(repo.join(path))?)
                .with_context(|| format!("parse {}", path.display()))?;
            Ok(PackageAt {
                root: path.parent().unwrap_or(Path::new("")).to_owned(),
                package,
            })
        })
        .collect()
}

fn detect_bootstrap(
    repo: &Path,
    devbox: Option<&str>,
    packages: &[PackageAt],
    notices: &mut Vec<String>,
) -> Result<Option<String>> {
    let devbox = devbox
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .context("parse devbox.json")?;
    if devbox.as_ref().is_some_and(|value| {
        value
            .pointer("/shell/scripts/bootstrap")
            .is_some_and(|script| script.is_string() || script.is_array())
    }) {
        return Ok(Some("devbox run bootstrap".to_owned()));
    }
    let Some(root) = packages
        .iter()
        .find(|package| package.root.as_os_str().is_empty())
    else {
        return Ok(None);
    };
    let Some(manager) = package_manager(repo, &root.package) else {
        return Ok(None);
    };
    if devbox_installs_dependencies(devbox.as_ref(), &manager) {
        notices.push(format!(
            "Dependencies: handled by the Devbox init hook ({manager} install)"
        ));
        return Ok(None);
    }
    let command = install_command(&manager);
    Ok(Some(if devbox.is_some() {
        format!("devbox run -- {command}")
    } else {
        command
    }))
}

fn install_command(manager: &str) -> String {
    match manager {
        "npm" => "npm ci".to_owned(),
        "yarn" => "yarn install --immutable".to_owned(),
        _ => format!("{manager} install --frozen-lockfile"),
    }
}

fn devbox_installs_dependencies(devbox: Option<&serde_json::Value>, manager: &str) -> bool {
    let install = format!("{manager} install");
    devbox
        .and_then(|value| value.pointer("/shell/init_hook"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|hooks| {
            hooks
                .iter()
                .any(|hook| hook.as_str().is_some_and(|line| line.contains(&install)))
        })
}

fn package_manager(repo: &Path, package: &Package) -> Option<String> {
    if let Some(manager) = package.package_manager.as_deref() {
        let manager = manager.split('@').next()?;
        return matches!(manager, "bun" | "npm" | "pnpm" | "yarn").then(|| manager.to_owned());
    }
    [
        ("bun.lock", "bun"),
        ("bun.lockb", "bun"),
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("package-lock.json", "npm"),
    ]
    .into_iter()
    .find(|(lock, _)| repo.join(lock).is_file())
    .map(|(_, manager)| manager.to_owned())
}

fn detect_disposables(
    repo: &Path,
    tracked: &[PathBuf],
    has_devbox: bool,
    packages: &[PackageAt],
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if has_devbox {
        paths.push(PathBuf::from(".devbox"));
    }
    let mut candidates = Vec::new();
    for package in packages {
        candidates.extend(node_disposable_candidates(repo, package)?);
    }
    for root in tracked_manifest_roots(tracked, "Cargo.toml") {
        candidates.push(root.join("target"));
    }
    let ignored = ignored_paths(repo, &candidates)?;
    paths.extend(candidates.into_iter().filter(|path| ignored.contains(path)));
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn node_disposable_candidates(repo: &Path, package: &PackageAt) -> Result<Vec<PathBuf>> {
    let mut paths = vec![package.root.join("node_modules")];
    let next = package_has(package, "next") || scripts_contain(package, "next ");
    if next {
        paths.extend([
            package.root.join(".next"),
            package.root.join("next-env.d.ts"),
        ]);
        paths.push(package.root.join("tsconfig.tsbuildinfo"));
    }
    if scripts_contain(package, "next export") || next_exports(repo, &package.root)? {
        paths.push(package.root.join("out"));
    }
    if coverage_evidence(repo, package) {
        paths.push(package.root.join("coverage"));
    }
    Ok(paths)
}

fn package_has(package: &PackageAt, name: &str) -> bool {
    package.package.dependencies.contains_key(name)
        || package.package.dev_dependencies.contains_key(name)
}

fn scripts_contain(package: &PackageAt, needle: &str) -> bool {
    package
        .package
        .scripts
        .values()
        .any(|script| script.contains(needle))
}

fn next_exports(repo: &Path, root: &Path) -> Result<bool> {
    for name in [
        "next.config.js",
        "next.config.mjs",
        "next.config.ts",
        "next.config.mts",
    ] {
        if read_optional(&repo.join(root).join(name))?.is_some_and(|text| {
            text.contains("output: \"export\"") || text.contains("output: 'export'")
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn coverage_evidence(repo: &Path, package: &PackageAt) -> bool {
    let tools = ["jest", "vitest", "c8", "nyc"];
    repo.join(&package.root).join("coverage").exists()
        || tools.iter().any(|tool| package_has(package, tool))
        || package
            .package
            .scripts
            .iter()
            .any(|(name, script)| name.contains("coverage") || script.contains("--coverage"))
}

fn add_compose_notices(tracked: &[PathBuf], notices: &mut Vec<String>) {
    let extra = tracked
        .iter()
        .filter(|path| is_compose_path(path) && !is_root_compose_path(path))
        .cloned()
        .collect::<Vec<_>>();
    if !extra.is_empty() {
        notices.push(format!(
            "Additional Compose files (not enabled): {}",
            display_paths(&extra)
        ));
    }
}

fn is_compose_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let yaml = name.ends_with(".yml") || name.ends_with(".yaml");
            yaml && (name.starts_with("compose") || name.starts_with("docker-compose"))
        })
}

fn is_root_compose_path(path: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| parent.as_os_str().is_empty())
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| COMPOSE_FILES.contains(&name))
}

fn add_repository_warnings(
    repo: &Path,
    tracked: &[PathBuf],
    devbox: Option<&str>,
    config: &Config,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if devbox_uses_worktree_gopath(devbox) {
        warnings.push(
            "devbox.json stores GOPATH inside .devbox; each worktree duplicates the Go cache."
                .to_owned(),
        );
    }
    if config.env.is_none() && config.compose {
        warnings.push("Docker Compose isolation needs a primary env file.".to_owned());
    }
    if has_process_ports(config) && !command_exists("direnv") {
        warnings
            .push("Process-port isolation needs direnv, but direnv is not installed.".to_owned());
    }
    add_localhost_warnings(repo, tracked, config, warnings)
}

fn command_exists(command: &str) -> bool {
    Command::new(command)
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn add_localhost_warnings(
    repo: &Path,
    tracked: &[PathBuf],
    config: &Config,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let mut ports = process_defaults(config)?;
    ports.sort_unstable();
    ports.dedup();
    for (port, paths) in hardcoded_localhost_paths(repo, tracked, &ports)? {
        if !paths.is_empty() {
            warnings.push(format!(
                "Tracked runtime files hard-code localhost:{port}: {}. They may not follow the assigned port.",
                display_paths(&paths)
            ));
        }
    }
    Ok(())
}

fn process_defaults(config: &Config) -> Result<Vec<u16>> {
    config
        .ports
        .iter()
        .map(|port| Ok(crate::config::parse_port_spec(port)?.default))
        .collect::<Result<Vec<_>>>()
        .map(|ports| ports.into_iter().flatten().collect())
}

fn hardcoded_localhost_paths(
    repo: &Path,
    tracked: &[PathBuf],
    ports: &[u16],
) -> Result<BTreeMap<u16, Vec<PathBuf>>> {
    let mut paths_by_port = BTreeMap::new();
    let service_roots = isolated_service_roots(tracked);
    if service_roots.is_empty() || ports.is_empty() {
        return Ok(paths_by_port);
    }
    let needles = ports
        .iter()
        .map(|port| (*port, format!("localhost:{port}")))
        .collect::<Vec<_>>();
    let mut command = Command::new("git");
    command.current_dir(repo).args(["grep", "-l", "-z", "-F"]);
    for (_, needle) in &needles {
        command.args(["-e", needle]);
    }
    let output = command.arg("--").output()?;
    if output.status.code() == Some(1) {
        return Ok(paths_by_port);
    }
    if !output.status.success() {
        bail!("cannot search for hard-coded localhost ports");
    }
    for path in nul_paths(&output.stdout)?.into_iter().filter(|path| {
        is_runtime_file(path) && service_roots.iter().any(|root| path.starts_with(root))
    }) {
        if let [(port, _)] = needles.as_slice() {
            paths_by_port.entry(*port).or_default().push(path);
            continue;
        }
        let contents = fs::read(repo.join(&path))?;
        for (port, needle) in &needles {
            if contains_bytes(&contents, needle.as_bytes()) {
                paths_by_port.entry(*port).or_default().push(path.clone());
            }
        }
    }
    Ok(paths_by_port)
}

fn isolated_service_roots(tracked: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = tracked
        .iter()
        .filter(|path| is_wrangler_config(path))
        .filter_map(|path| path.parent().map(Path::to_owned))
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    roots
}

fn contains_bytes(contents: &[u8], needle: &[u8]) -> bool {
    contents
        .windows(needle.len())
        .any(|window| window == needle)
}

fn is_runtime_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if path.components().any(|part| part.as_os_str() == "tests")
        || name.contains(".test.")
        || name.contains(".spec.")
    {
        return false;
    }
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            matches!(
                extension,
                "ts" | "tsx" | "js" | "mjs" | "cjs" | "toml" | "json" | "jsonc" | "yaml" | "yml"
            )
        })
}

pub fn planned_changes(
    repo: &Path,
    config: &Config,
    template: Option<&EnvTemplate>,
) -> Result<Vec<String>> {
    let mut changes = Vec::new();
    if let Some(template) = template.filter(|plan| !repo.join(&plan.target).exists()) {
        changes.push(format!(
            "Create {} from {}",
            template.target.display(),
            template.source.display()
        ));
    }
    if has_process_ports(config) && !file_contains(repo.join(".envrc"), "dotenv_if_exists .wt.env")?
    {
        changes.push("Add `dotenv_if_exists .wt.env` to .envrc".to_owned());
    }
    if has_process_ports(config) && !file_has_line(repo.join(".gitignore"), "/.wt.env")? {
        changes.push("Ignore /.wt.env".to_owned());
    }
    Ok(changes)
}

pub fn apply_changes(repo: &Path, config: &Config, template: Option<&EnvTemplate>) -> Result<()> {
    if let Some(template) = template.filter(|plan| !repo.join(&plan.target).exists()) {
        fs::copy(repo.join(&template.source), repo.join(&template.target)).with_context(|| {
            format!(
                "create {} from {}",
                template.target.display(),
                template.source.display()
            )
        })?;
    }
    if has_process_ports(config) {
        append_line(repo.join(".envrc"), "dotenv_if_exists .wt.env")?;
        append_line(repo.join(".gitignore"), "/.wt.env")?;
    }
    Ok(())
}

fn has_process_ports(config: &Config) -> bool {
    config.ports.iter().any(|port| port.contains(':'))
}

fn append_line(path: PathBuf, line: &str) -> Result<()> {
    if file_has_line(&path, line)? {
        return Ok(());
    }
    let mut contents = read_optional(&path)?.unwrap_or_default();
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(line);
    contents.push('\n');
    fs::write(&path, contents).with_context(|| format!("update {}", path.display()))
}

fn file_has_line(path: impl AsRef<Path>, expected: &str) -> Result<bool> {
    Ok(read_optional(path.as_ref())?
        .is_some_and(|text| text.lines().any(|line| line.trim() == expected)))
}

fn file_contains(path: impl AsRef<Path>, expected: &str) -> Result<bool> {
    Ok(read_optional(path.as_ref())?.is_some_and(|text| text.contains(expected)))
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn select_env_template(
    repo: &Path,
    supplied: Option<&Path>,
    existing: &[PathBuf],
    tracked: &[PathBuf],
) -> Result<Option<EnvTemplate>> {
    if supplied.is_some() || !existing.is_empty() {
        return Ok(None);
    }
    for source in tracked.iter().filter(|path| {
        path.parent()
            .is_some_and(|parent| parent.as_os_str().is_empty())
            && is_env_template(path)
    }) {
        if let Some(target) = documented_env_target(repo, source, tracked)? {
            return Ok(Some(EnvTemplate {
                source: source.clone(),
                target,
            }));
        }
    }
    Ok(None)
}

fn is_env_template(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == ".env.example"
                || name == ".env.sample"
                || name == ".dev.vars.example"
                || name.ends_with(".env.example")
        })
}

fn documented_env_target(
    repo: &Path,
    source: &Path,
    tracked: &[PathBuf],
) -> Result<Option<PathBuf>> {
    let root = source.parent().unwrap_or(Path::new(""));
    for readme in tracked.iter().filter(|path| {
        path.parent() == Some(root)
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("README"))
    }) {
        let text = fs::read_to_string(repo.join(readme))?;
        let Some(source_name) = source.file_name() else {
            return Ok(None);
        };
        let source_name = source_name.to_string_lossy();
        for target in [".env.local", ".dev.vars", ".env"] {
            if text.lines().any(|line| {
                line.find(source_name.as_ref())
                    .is_some_and(|position| line[position + source_name.len()..].contains(target))
            }) {
                return Ok(Some(root.join(target)));
            }
        }
    }
    Ok(None)
}

fn detected_copies(
    supplied: Vec<PathBuf>,
    candidates: Vec<PathBuf>,
    env: Option<&PathBuf>,
) -> Vec<PathBuf> {
    if !supplied.is_empty() {
        return supplied;
    }
    candidates
        .into_iter()
        .filter(|path| Some(path) != env)
        .collect()
}

fn ignored_env_files(repo: &Path) -> Result<Vec<PathBuf>> {
    let output = git(
        repo,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
            "--",
            ":(glob)**/.env",
            ":(glob)**/.env.*",
            ":(glob)**/.dev.vars",
        ],
    )?;
    let mut paths = Vec::new();
    for path in nul_paths(&output.stdout)? {
        if is_env_candidate(&path) && regular_file(repo.join(&path))? {
            paths.push(path);
        }
    }
    paths.sort_by_key(|path| path != Path::new(".env"));
    Ok(paths)
}

fn is_env_candidate(path: &Path) -> bool {
    let generated = [
        ".cache",
        ".devbox",
        ".direnv",
        ".next",
        ".nuxt",
        ".venv",
        "build",
        "coverage",
        "dist",
        "node_modules",
        "target",
        "vendor",
        "venv",
    ];
    let valid = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(valid_env_name);
    valid
        && !path
            .components()
            .any(|part| generated.iter().any(|name| part.as_os_str() == *name))
}

fn valid_env_name(name: &str) -> bool {
    if name == ".dev.vars" {
        return true;
    }
    name == ".env"
        || name.strip_prefix(".env.").is_some_and(|suffix| {
            !suffix.split('.').any(|part| {
                matches!(
                    part,
                    "bak" | "backup" | "example" | "invalid" | "sample" | "template"
                )
            })
        })
}

fn regular_file(path: PathBuf) -> Result<bool> {
    match fs::symlink_metadata(&path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn has_root_compose(repo: &Path) -> bool {
    COMPOSE_FILES.iter().any(|file| repo.join(file).is_file())
}

fn tracked_manifest_roots(tracked: &[PathBuf], manifest: &str) -> Vec<PathBuf> {
    tracked
        .iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some(manifest))
        .map(|path| path.parent().unwrap_or(Path::new("")).to_owned())
        .collect()
}

fn tracked_files(repo: &Path) -> Result<Vec<PathBuf>> {
    nul_paths(&git(repo, ["ls-files", "-z"])?.stdout)
}

fn nul_paths(bytes: &[u8]) -> Result<Vec<PathBuf>> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            String::from_utf8(entry.to_vec())
                .map(PathBuf::from)
                .map_err(Into::into)
        })
        .collect()
}

fn ignored_paths(repo: &Path, paths: &[PathBuf]) -> Result<HashSet<PathBuf>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["check-ignore", "--stdin", "-z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run git check-ignore")?;
    let write = child.stdin.take().map_or_else(
        || Err(std::io::Error::other("missing git check-ignore stdin")),
        |mut stdin| {
            for path in paths {
                stdin.write_all(path.as_os_str().as_encoded_bytes())?;
                stdin.write_all(&[0])?;
            }
            Ok(())
        },
    );
    let output = child
        .wait_with_output()
        .context("wait for git check-ignore")?;
    match output.status.code() {
        Some(0 | 1) => {
            write.context("send paths to git check-ignore")?;
            Ok(nul_paths(&output.stdout)?.into_iter().collect())
        }
        _ => bail!(
            "cannot check ignored paths: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) -> Result<std::process::Output> {
    let output = Command::new("git").current_dir(repo).args(args).output()?;
    if output.status.success() {
        Ok(output)
    } else {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn devbox_uses_worktree_gopath(devbox: Option<&str>) -> bool {
    devbox.is_some_and(|contents| {
        contents.contains("\"GOPATH\"")
            && (contents.contains("$PWD/.devbox") || contents.contains("${PWD}/.devbox"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parses_supported_port_arguments() {
        assert!(matches!(
            port_argument("dev -p 3001"),
            Some(PortArgument::Number(3001))
        ));
        assert!(
            matches!(port_argument("dev --port=$PORT"), Some(PortArgument::Variable(key)) if key == "PORT")
        );
        assert!(
            matches!(port_argument("dev --port ${WEB_PORT}"), Some(PortArgument::Variable(key)) if key == "WEB_PORT")
        );
    }

    #[test]
    fn package_manager_field_cannot_inject_a_bootstrap_command() {
        let package = Package {
            package_manager: Some("pnpm; touch owned@1".to_owned()),
            ..Package::default()
        };

        assert_eq!(package_manager(Path::new("."), &package), None);
    }

    #[test]
    fn env_candidates_reject_templates_and_generated_files() {
        assert!(is_env_candidate(Path::new(".env")));
        assert!(is_env_candidate(Path::new("frontend/.env.local")));
        assert!(is_env_candidate(Path::new(".dev.vars")));
        assert!(!is_env_candidate(Path::new(".env.example")));
        assert!(!is_env_candidate(Path::new("node_modules/pkg/.env")));
    }

    #[test]
    fn ignored_env_discovery_limits_git_output_without_missing_nested_files() {
        let fixture = tempfile::tempdir().unwrap();
        command(fixture.path(), ["init", "-q"]);
        write(
            fixture.path(),
            ".gitignore",
            ".env\n.dev.vars\napp/.env.local\napp/.env.example\nnode_modules\n",
        );
        for path in [
            ".env",
            ".dev.vars",
            "app/.env.local",
            "app/.env.example",
            "node_modules/pkg/.env",
        ] {
            write(fixture.path(), path, "VALUE=1\n");
        }

        assert_eq!(
            ignored_env_files(fixture.path()).unwrap(),
            [".env", ".dev.vars", "app/.env.local"]
                .map(PathBuf::from)
                .to_vec()
        );
    }

    #[test]
    fn compose_parser_only_returns_host_variables() {
        assert_eq!(
            compose_variables_in_line("- \"${API_PORT}:8080\""),
            ["API_PORT"]
        );
        assert_eq!(
            compose_variables_in_line("published: \"${WEB_PORT}\""),
            ["WEB_PORT"]
        );
        assert!(compose_variables_in_line("PORT: ${PORT}").is_empty());
    }

    #[test]
    fn next_project_gets_process_port_bootstrap_and_precise_disposables() {
        let fixture = node_fixture("next dev", true);
        let report = detect(fixture.path(), Config::default()).unwrap();

        assert_eq!(report.config.env.as_deref(), Some(Path::new(".env.local")));
        assert_eq!(report.config.ports, ["PORT:3000"]);
        assert_eq!(
            report.config.bootstrap.as_deref(),
            Some("devbox run -- bun install --frozen-lockfile")
        );
        for path in [
            ".devbox",
            ".next",
            "node_modules",
            "out",
            "next-env.d.ts",
            "tsconfig.tsbuildinfo",
        ] {
            assert!(
                report.config.disposable.contains(&PathBuf::from(path)),
                "missing {path}"
            );
        }
        assert!(
            !report
                .config
                .disposable
                .contains(&PathBuf::from("coverage"))
        );
        let changes =
            planned_changes(fixture.path(), &report.config, report.env_template.as_ref()).unwrap();
        assert_eq!(changes.len(), 3);
    }

    #[test]
    fn fixed_next_port_is_reported_but_not_falsely_managed() {
        let fixture = node_fixture("next dev -p 3001", false);
        let report = detect(fixture.path(), Config::default()).unwrap();

        assert!(report.config.ports.is_empty());
        assert!(
            report
                .development
                .iter()
                .any(|line| line.contains("fixed 3001"))
        );
        assert!(report.warnings.iter().any(|line| line.contains("-p $PORT")));
    }

    #[test]
    fn nested_wrangler_and_localhost_coupling_are_visible() {
        let fixture = node_fixture("next dev", false);
        write(
            fixture.path(),
            "worker/wrangler.toml",
            "name = \"worker\"\n",
        );
        write(
            fixture.path(),
            "worker/src/index.ts",
            "const origin = 'http://localhost:3000';\n",
        );
        command(
            fixture.path(),
            ["add", "worker/wrangler.toml", "worker/src/index.ts"],
        );

        let report = detect(fixture.path(), Config::default()).unwrap();

        assert!(report.development.iter().any(|line| {
            line.contains("Wrangler (worker/wrangler.toml): default port 8787, not reserved")
        }));
        assert!(
            report
                .warnings
                .iter()
                .any(|line| { line.contains("hard-code localhost:3000: worker/src/index.ts") })
        );
    }

    #[test]
    fn one_localhost_search_maps_multiple_ports_to_their_runtime_files() {
        let fixture = tempfile::tempdir().unwrap();
        command(fixture.path(), ["init", "-q"]);
        write(
            fixture.path(),
            "worker/wrangler.toml",
            "name = \"worker\"\n",
        );
        write(
            fixture.path(),
            "worker/src/web.ts",
            "const web = 'http://localhost:3000';\n",
        );
        write(
            fixture.path(),
            "worker/src/api.ts",
            "const api = 'http://localhost:8787';\n",
        );
        write(
            fixture.path(),
            "worker/src/web.test.ts",
            "const test = 'http://localhost:3000';\n",
        );
        track(
            fixture.path(),
            vec![
                "worker/wrangler.toml",
                "worker/src/web.ts",
                "worker/src/api.ts",
                "worker/src/web.test.ts",
            ],
        );
        let config = Config {
            ports: vec!["WEB_PORT:3000".to_owned(), "API_PORT:8787".to_owned()],
            ..Config::default()
        };

        let report = detect(fixture.path(), config).unwrap();

        assert!(report.warnings.iter().any(|warning| {
            warning.contains("localhost:3000: worker/src/web.ts")
                && !warning.contains("web.test.ts")
        }));
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("localhost:8787: worker/src/api.ts"))
        );
    }

    #[test]
    fn vite_config_overrides_the_script_default() {
        let fixture = tempfile::tempdir().unwrap();
        command(fixture.path(), ["init", "-q"]);
        write(
            fixture.path(),
            "package.json",
            r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"1"}}"#,
        );
        write(
            fixture.path(),
            "vite.config.ts",
            "export default {\n server: {\n  port: 1420,\n  strictPort: true,\n },\n};\n",
        );
        track(fixture.path(), vec!["package.json", "vite.config.ts"]);

        let report = detect(fixture.path(), Config::default()).unwrap();

        assert_eq!(report.development, ["Vite (vite.config.ts): fixed 1420"]);
    }

    fn node_fixture(dev: &str, devbox: bool) -> TempDir {
        let fixture = tempfile::tempdir().unwrap();
        command(fixture.path(), ["init", "-q"]);
        write_node_fixture(fixture.path(), dev);
        let mut tracked = vec![
            ".gitignore",
            "README.md",
            ".env.example",
            "bun.lock",
            "next.config.ts",
            "package.json",
        ];
        if devbox {
            write(
                fixture.path(),
                "devbox.json",
                r#"{"packages":["bun@latest"]}"#,
            );
            tracked.push("devbox.json");
        }
        track(fixture.path(), tracked);
        fixture
    }

    fn write_node_fixture(root: &Path, dev: &str) {
        write(
            root,
            ".gitignore",
            ".env.local\nnode_modules\n.next\nout\nnext-env.d.ts\n*.tsbuildinfo\ncoverage\n",
        );
        write(root, "README.md", "cp .env.example .env.local\n");
        write(root, ".env.example", "TOKEN=replace-me\n");
        write(root, "bun.lock", "");
        write(
            root,
            "next.config.ts",
            "export default { output: 'export' };\n",
        );
        write(
            root,
            "package.json",
            &format!(r#"{{"scripts":{{"dev":"{dev}"}},"dependencies":{{"next":"1"}}}}"#),
        );
    }

    fn track(root: &Path, tracked: Vec<&str>) {
        let mut args = vec!["add"];
        args.extend(tracked);
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    fn write(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn command<const N: usize>(root: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
    }
}
