//! `gos init` and `gos new` - project scaffolding plus the inline
//! source / manifest / README templates each `--template` choice
//! emits.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

/// `gos init ID` - drops a `project.toml` (and a starter
/// `src/main.gos` when neither it nor `src/lib.gos` exists) into
/// the current directory.
pub(crate) fn init(id: &str) -> Result<()> {
    let project =
        gossamer_pkg::ProjectId::parse(id).with_context(|| format!("invalid id `{id}`"))?;
    let manifest_path = PathBuf::from("project.toml");
    if manifest_path.exists() {
        return Err(anyhow!("`project.toml` already exists"));
    }
    let manifest =
        gossamer_pkg::render_initial_manifest(&project, gossamer_pkg::Version::new(0, 1, 0));
    fs::write(&manifest_path, &manifest)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    let src_dir = PathBuf::from("src");
    let main_gos = src_dir.join("main.gos");
    let lib_gos = src_dir.join("lib.gos");
    let scaffolded = if !main_gos.exists() && !lib_gos.exists() {
        fs::create_dir_all(&src_dir).with_context(|| format!("creating {}", src_dir.display()))?;
        let body = gossamer_pkg::render_main_source(&project);
        fs::write(&main_gos, body).with_context(|| format!("writing {}", main_gos.display()))?;
        true
    } else {
        false
    };
    if scaffolded {
        outln!("init: created project.toml + src/main.gos for {project}");
        outln!("hint: try `gos` or `gos test`");
    } else {
        outln!("init: created project.toml for {project}");
    }
    Ok(())
}

/// `gos new ID --path P --template T` - scaffolds a fresh project
/// directory according to the chosen template (`bin`, `lib`,
/// `service`, `workspace`, or `binding`).
pub(crate) fn new(id: &str, path: Option<PathBuf>, template: &str) -> Result<()> {
    // The binding template writes a Rust crate rather than a project, so
    // it is reached from a project's manifest rather than run on its own.
    let mut binding_hint: Option<(String, PathBuf)> = None;
    let project =
        gossamer_pkg::ProjectId::parse(id).with_context(|| format!("invalid id `{id}`"))?;
    let dir = path.unwrap_or_else(|| PathBuf::from(project.tail()));
    if dir.exists() {
        return Err(anyhow!("{} already exists", dir.display()));
    }
    let manifest =
        gossamer_pkg::render_initial_manifest(&project, gossamer_pkg::Version::new(0, 1, 0));
    match template {
        "bin" => {
            fs::create_dir_all(dir.join("src"))
                .with_context(|| format!("creating {}", dir.display()))?;
            fs::write(dir.join("project.toml"), &manifest)?;
            fs::write(
                dir.join("src/main.gos"),
                gossamer_pkg::render_main_source(&project),
            )?;
        }
        "lib" => {
            fs::create_dir_all(dir.join("src"))
                .with_context(|| format!("creating {}", dir.display()))?;
            fs::write(dir.join("project.toml"), &manifest)?;
            fs::write(dir.join("src/lib.gos"), lib_template_source(&project))?;
            fs::write(dir.join("src/lib_test.gos"), lib_template_test_source())?;
        }
        "service" => {
            fs::create_dir_all(dir.join("src"))
                .with_context(|| format!("creating {}", dir.display()))?;
            fs::write(dir.join("project.toml"), &manifest)?;
            fs::write(dir.join("src/main.gos"), service_template_source(&project))?;
        }
        "workspace" => {
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            fs::write(
                dir.join("project.toml"),
                workspace_template_manifest(&project),
            )?;
            fs::write(dir.join("README.md"), workspace_template_readme(&project))?;
        }
        "binding" => {
            // Phase-0 `gos new --template binding`: scaffolds a
            // ready-to-edit Rust binding crate under the project
            // directory. The user fills in fn signatures inside
            // the `#[gos_module]`-annotated module body; no
            // `Cargo.toml`/`__bindings_force_link()` boilerplate.
            fs::create_dir_all(dir.join("src"))
                .with_context(|| format!("creating {}", dir.display()))?;
            let cargo_toml = binding_template_cargo_toml(project.tail());
            let lib_rs = binding_template_lib_rs(project.tail());
            fs::write(dir.join("Cargo.toml"), cargo_toml)?;
            fs::write(dir.join("src/lib.rs"), lib_rs)?;
            binding_hint = Some((
                format!("{}-binding", project.tail().replace('/', "-")),
                dir.clone(),
            ));
        }
        other => {
            return Err(anyhow!(
                "unknown template `{other}` - expected bin, lib, service, workspace, or binding"
            ));
        }
    }
    outln!(
        "new: scaffolded {} ({} template) at {}",
        project,
        template,
        dir.display()
    );
    if let Some((crate_name, crate_dir)) = binding_hint {
        outln!(
            "add it to the calling project's `project.toml`:\n\n\
             [rust-bindings]\n\
             {crate_name} = {{ path = \"{}\" }}",
            crate_dir.display()
        );
    }
    Ok(())
}

/// Returns the seed `src/lib.gos` for `--template lib`.
fn lib_template_source(project: &gossamer_pkg::ProjectId) -> String {
    format!(
        "//! {project} - library crate.\n\
         //!\n\
         //! Replace this scaffolding with the real API before\n\
         //! publishing.\n\
         \n\
         /// Returns a greeting addressed to `name`.\n\
         pub fn greet(name: str) -> String {{\n\
         \x20\x20\x20\x20\"hello, \" + name\n\
         }}\n",
    )
}

/// Returns the seed `src/main.gos` for `--template service`.
fn service_template_source(project: &gossamer_pkg::ProjectId) -> String {
    format!(
        "//! {project} - HTTP service entry point.\n\
         //!\n\
         //! Listens on 0.0.0.0:8080 and answers `/health` with a 200.\n\
         //! Replace the match arms with your real routes before shipping.\n\
         \n\
         use std::http\n\
         \n\
         struct App {{ }}\n\
         \n\
         impl http::Handler for App {{\n\
         \x20\x20\x20\x20fn serve(self, request: http::Request) -> Result<http::Response, http::Error> {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20match request.path() {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\"/health\" => Ok(http::Response::text(200, \"ok\")),\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20_ => Ok(http::Response::text(404, \"not found\")),\n\
         \x20\x20\x20\x20\x20\x20\x20\x20}}\n\
         \x20\x20\x20\x20}}\n\
         }}\n\
         \n\
         fn main() -> Result<(), http::Error> {{\n\
         \x20\x20\x20\x20let app = App {{ }}\n\
         \x20\x20\x20\x20println(\"listening on 0.0.0.0:8080\")\n\
         \x20\x20\x20\x20http::serve(\"0.0.0.0:8080\", app)\n\
         }}\n",
    )
}

/// Returns the seed test fixture for `--template lib`.
fn lib_template_test_source() -> String {
    "//! Smoke tests for the library crate.\n\
     \n\
     use std::testing\n\
     \n\
     #[test]\n\
     fn greet_includes_name() {\n\
     \x20\x20\x20\x20testing::check_eq(greet(\"gossamer\"), \"hello, gossamer\", \"greet round-trips\").expect(\"mismatch\")\n\
     }\n"
        .to_string()
}

/// Returns the `project.toml` contents for `--template workspace`.
fn workspace_template_manifest(project: &gossamer_pkg::ProjectId) -> String {
    format!(
        "[package]\n\
         id = \"{project}\"\n\
         version = \"0.1.0\"\n\
         \n\
         [workspace]\n\
         members = []\n",
    )
}

/// Returns a README.md stub for `--template workspace`.
fn workspace_template_readme(project: &gossamer_pkg::ProjectId) -> String {
    format!(
        "# {project}\n\
         \n\
         A Gossamer workspace. Add members under `members/` and list\n\
         their ids under `[workspace.members]` in `project.toml`.\n",
    )
}

/// Returns the seed `Cargo.toml` for `--template binding`.
fn binding_template_cargo_toml(crate_name: &str) -> String {
    let safe = crate_name.replace('/', "-");
    format!(
        "[package]\n\
         name = \"{safe}-binding\"\n\
         version = \"0.0.1\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [workspace]\n\
         \n\
         [lib]\n\
         crate-type = [\"rlib\"]\n\
         \n\
         [dependencies]\n\
         # The binding ABI this crate is built against. The toolchain\n\
         # resolves it to the `gos` that builds the project, so it\n\
         # tracks the version stated in `project.toml`.\n\
         gossamer-binding = \"{binding_version}\"\n",
        binding_version = gossamer_pkg::toolchain_version(),
    )
}

/// Returns the seed `src/lib.rs` for `--template binding`.
fn binding_template_lib_rs(crate_name: &str) -> String {
    let mod_name = crate_name.replace('-', "_");
    format!(
        "//! `{crate_name}` - Rust bindings exposed to Gossamer.\n\
         //!\n\
         //! Drop fn definitions inside the `#[gos_module]` block.\n\
         //! `///` doc-comments above each fn flow through to\n\
         //! `gos doc {mod_name}::<fn>`.\n\
         \n\
         use gossamer_binding::{{GosError, gos_module}};\n\
         \n\
         #[gos_module(\"{mod_name}\")]\n\
         mod bindings {{\n\
         \x20\x20\x20\x20use super::*;\n\
         \n\
         \x20\x20\x20\x20/// Greet the supplied name.\n\
         \x20\x20\x20\x20pub fn greet(name: String) -> String {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20format!(\"hello, {{name}}\")\n\
         \x20\x20\x20\x20}}\n\
         \n\
         \x20\x20\x20\x20/// Fallible example: parse an integer.\n\
         \x20\x20\x20\x20pub fn parse_int(s: String) -> Result<i64, GosError> {{\n\
         \x20\x20\x20\x20\x20\x20\x20\x20Ok(s.parse::<i64>()?)\n\
         \x20\x20\x20\x20}}\n\
         }}\n",
    )
}
