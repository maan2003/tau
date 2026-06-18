//! Fixtures for Tau's multiprocess VCR end-to-end tests.

use std::path::{Path, PathBuf};

use tau_harness::{EmbeddedOptions, run_embedded_message_with_options};
use tempfile::TempDir;

/// A real headless Tau run with isolated harness config and state.
///
/// The caller owns VCR mode through normal environment variables such as
/// `TAU_VCR` and `TAU_VCR_DIR`. The fixture intentionally does not override XDG
/// state so provider extensions can use the user's real auth store.
#[derive(Debug)]
pub struct VcrFixture {
    _tempdir: TempDir,
    config_dir: PathBuf,
    state_dir: PathBuf,
    harness_state_dir: PathBuf,
    work_dir: PathBuf,
}

impl VcrFixture {
    /// Creates a fixture from the e2e environment.
    ///
    /// Returns `Ok(None)` when the caller did not opt into VCR e2e execution by
    /// setting `TAU_VCR`, `TAU_VCR_DIR`, and `TAU_E2E_MODEL`. This keeps normal
    /// workspace test runs independent of live provider credentials.
    pub fn from_env(name: &str) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        if std::env::var_os("TAU_VCR").is_none()
            || std::env::var_os("TAU_VCR_DIR").is_none()
            || std::env::var_os("TAU_E2E_MODEL").is_none()
        {
            eprintln!(
                "skipping {name}: set TAU_VCR, TAU_VCR_DIR, and TAU_E2E_MODEL to run VCR e2e"
            );
            return Ok(None);
        }

        let tempdir = TempDir::new()?;
        let root = tempdir.path().join(sanitize_name(name));
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        let harness_state_dir = root.join("harness-state");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(&state_dir)?;
        let work_dir = root.join("work");
        std::fs::create_dir_all(&harness_state_dir)?;
        std::fs::create_dir_all(&work_dir)?;

        let fixture = Self {
            _tempdir: tempdir,
            config_dir,
            state_dir,
            harness_state_dir,
            work_dir,
        };
        let tau_bin = std::env::var("TAU_E2E_TAU_BIN").unwrap_or_else(|_| "tau".to_owned());
        fixture.write_harness_config(
            &std::env::var("TAU_E2E_MODEL")?,
            &canonicalize_command_if_path(&tau_bin),
        )?;
        Ok(Some(fixture))
    }

    /// Runs one real embedded Tau turn. VCR mismatch or missing cassette errors
    /// surface as the returned harness error.
    pub fn run_turn(&self, prompt: &str) -> Result<(), tau_harness::HarnessError> {
        run_embedded_message_with_options(
            &self.harness_state_dir,
            prompt,
            EmbeddedOptions::builder()
                .dirs(tau_config::settings::TauDirs {
                    config_dir: Some(self.config_dir.clone()),
                    state_dir: Some(self.state_dir.clone()),
                })
                .build(),
        )?;
        Ok(())
    }

    fn write_harness_config(
        &self,
        model: &str,
        tau_bin: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let tau_bin = serde_json::to_string(tau_bin)?;
        let model = serde_json::to_string(model)?;
        let work_dir = serde_json::to_string(&self.work_dir.display().to_string())?;
        std::fs::write(
            self.config_dir.join("harness.yaml"),
            format!(
                concat!(
                    "default_role: vcr-e2e\n",
                    "agents:\n",
                    "  idTemplate: main\n",
                    "role_groups:\n",
                    "  e2e:\n",
                    "    roles:\n",
                    "      vcr-e2e:\n",
                    "        model: {model}\n",
                    "        tools: [shell, apply_patch]\n",
                    "extensions:\n",
                    "  provider-builtin:\n",
                    "    command: [{tau_bin}]\n",
                    "    suffix: [ext, ext-provider-builtin]\n",
                    "  core-shell:\n",
                    "    command: [{tau_bin}]\n",
                    "    suffix: [ext, ext-shell]\n",
                    "    config:\n",
                    "      working_directory: {work_dir}\n",
                    "  std-notifications:\n",
                    "    enable: false\n",
                    "  std-websearch:\n",
                    "    enable: false\n",
                ),
                model = model,
                tau_bin = tau_bin,
                work_dir = work_dir,
            ),
        )?;
        Ok(())
    }
}

fn canonicalize_command_if_path(command: &str) -> String {
    if !command.contains(std::path::MAIN_SEPARATOR) {
        return command.to_owned();
    }
    let path = Path::new(command);
    if let Ok(canonical) = path.canonicalize() {
        return canonical.display().to_string();
    }
    workspace_root()
        .join(path)
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tau-e2e-tests lives under crates/")
        .to_path_buf()
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect()
}
